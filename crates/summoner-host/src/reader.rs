//! The reader thread: frames in. Inline ops (hello, ping, generations, park / unpark) are answered here; every
//! other op is receipt-acked here (before it runs) and posted to the message thread, which runs them in
//! arrival order. A protocol error exits 4, as does a truncated control message (MSG_CTRUNC); a frame without its
//! descriptor exits 5; EOF or a recv error (the host application is gone) exits 0.
use crate::host::{Op, Posted, Shared};
use crate::wire::{self, die, empty};
use runecraft_ipc::bridge::{MachReceive, MachSend};
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::time::Duration;
use summoner_proto::codec::{self, Direction, Frame, kind};
use summoner_rt::region::ChildPage;

/// The typed meta; a meta that is not a JSON object is a protocol error (fields of the wrong type read as
/// defaults).
fn meta<T: for<'de> serde::Deserialize<'de> + Default>(f: &Frame) -> T {
    if let Err(e) = f.meta_as::<serde_json::Value>() {
        die(4, &format!("protocol error: {e}"));
    }
    f.meta_as::<T>().unwrap_or_default()
}

/// The descriptors a frame carries come off the queue in order: `fds` of them (default 1); the first is kept.
/// A descriptor received with a frame, as the object the region views map.
pub(crate) fn received(fd: OwnedFd) -> runecraft_ipc::channel::Transferable {
    runecraft_ipc::channel::unix::TransferableExt::from_fd(
        fd,
        runecraft_ipc::channel::Access::ReadWrite,
    )
}

fn take_fd(fds: &mut VecDeque<OwnedFd>, want: u32, what: &str) -> OwnedFd {
    let want = want.max(1) as usize;
    if fds.len() < want {
        die(5, &format!("{what} without its descriptor"));
    }
    let fd = fds.pop_front().expect("checked");
    for _ in 1..want {
        drop(fds.pop_front());
    }
    fd
}

pub fn reader_thread(
    sock: UnixStream,
    sh: Arc<Shared>,
    ops: Sender<Posted>,
    slot0: Option<MachSend>,
    own: Option<Arc<MachReceive>>,
) {
    let w = sh.w.clone();
    let mut dec = codec::Decoder::new(Direction::ToChild);
    let mut buf = vec![0u8; 64 * 1024];
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();
    let mut slot0 = slot0;
    // The `corr` of the frame being read, given to every op it posts.
    let corr_now: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    let post = |op: Op| {
        // The message thread gone means we are exiting anyway.
        let _ = ops.send(Posted {
            corr: corr_now.borrow().clone(),
            op,
        });
    };
    loop {
        let mut got = Vec::new();
        let n = wire::recv_or_exit(&sock, &mut buf, &mut got);
        fds.extend(got);
        let mut frames: Vec<Frame> = Vec::new();
        if let Err(e) = dec.feed(&buf[..n], &mut |f| frames.push(f)) {
            die(4, &format!("protocol error: {e}"));
        }
        for f in frames {
            let (k, id) = (f.header.kind, f.header.id);
            // The request's `corr` (any kind; a child struct ignores the key), read before the typed decode with
            // summoner-proto's lenient reader and the log's corr grammar (`runecraft_log::corr_ok`, the one check every
            // hop makes). An invalid one is ignored and counted, with one line and one warning record; the op runs.
            *corr_now.borrow_mut() = corr_of_request(&sh, k, id, &f);
            match k {
                kind::HELLO => {
                    if sh.opts.hang_at_hello {
                        // Wedged before its first answer (the orphan watch still ends it with the parent).
                        loop {
                            std::thread::sleep(Duration::from_secs(3600));
                        }
                    }
                    let m: codec::Hello = meta(&f);
                    let pfd = take_fd(&mut fds, m.fds, "hello");
                    // The page, and its hub doorbells' objects (none on unix: `take_fd` keeps the one descriptor).
                    let page = ChildPage::open(received(pfd), &mut Vec::new())
                        .unwrap_or_else(|e| die(5, &format!("the child page: {e}")));
                    // The child page wired in place at hello, counted.
                    if let Err(e) = page.wire() {
                        eprintln!(
                            "[summoner-host] the child page was not wired ({e:?}); it runs unwired"
                        );
                        sh.page_unwired.store(e.bytes as u64, Ordering::Relaxed);
                    }
                    sh.set_page(Arc::new(page));
                    // The channel hello goes to slot 0's port before the reply.
                    let mut b = false;
                    if let (Some(s0), Some(own)) = (slot0.take(), own.as_ref()) {
                        b = s0.send_making(std::process::id() as u64, own, 1000).is_ok();
                    }
                    // The formats this child hosts, by the names the host application expects (the test double first,
                    // listed only under the test hooks). The supervisor does not read them.
                    let mut formats = Vec::new();
                    if cfg!(feature = "test-hooks") {
                        formats.push("summoner-test".to_string());
                    }
                    formats.extend(["VST3".to_string(), "AudioUnit".to_string()]);
                    // The child-wide memory wired at hello (the page), and what a refusal left unwired.
                    let (wired_bytes, unwired_bytes) = (
                        sh.page().map_or(0, |p| p.wired_bytes()),
                        sh.page_unwired.load(Ordering::Relaxed),
                    );
                    let r = {
                        let mut msg = codec::HelloReply::default();
                        msg.pid = std::process::id();
                        msg.protocol = wire::PROTOCOL;
                        msg.child_id = m.child_id;
                        msg.host = "native".to_string();
                        msg.formats = formats;
                        msg.mach_slot0 = b;
                        msg.wired_bytes = wired_bytes;
                        msg.unwired_bytes = unwired_bytes;
                        msg
                    };
                    wire::reply(&w, k, id, &r, &[]);
                    if std::env::var_os("SUMMONER_PLUGIN_HOST_VERBOSE").is_some_and(|v| v == "1") {
                        eprintln!(
                            "[summoner-host] up: pid {}, route {}",
                            std::process::id(),
                            if b { "B" } else { "A (or none yet)" }
                        );
                    }
                }
                kind::PING => wire::reply(&w, k, id, &empty(), &[]),
                // Inline, on this thread, so it lands even while the message thread is inside a
                // plugin.
                kind::LOG_SET => log_set(&sh, id, &f, corr_now.borrow().as_deref()),
                kind::STATE_GENERATIONS => wire::reply(&w, k, id, &sh.generations(), &[]),
                kind::SHUTDOWN => post(Op::Shutdown(id)),
                kind::CHAIN_LOAD => {
                    let m: codec::ChainLoad = meta(&f);
                    // Whether `instanceId` is in the meta at all (any present value is checked, 0 included).
                    let given = f
                        .meta_as::<serde_json::Value>()
                        .is_ok_and(|v| v.get("instanceId").is_some());
                    let fd = take_fd(&mut fds, m.fds, "chain.load");
                    wire::reply(&w, k, id, &empty(), &[]);
                    post(Op::Load(m, fd, given));
                }
                kind::CHAIN_PARK | kind::CHAIN_UNPARK => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    let m: codec::ChainUnpark = meta(&f);
                    park(&sh, m, k == kind::CHAIN_PARK, corr_now.borrow().as_deref());
                }
                kind::CHAIN_PREPARE_FRESH => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    let m: codec::ChainRef = meta(&f);
                    post(Op::PrepareFresh(m.chain_key));
                }
                kind::CHAIN_UNLOAD | kind::CHAIN_PREPARE => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    let m: codec::ChainRef = meta(&f);
                    post(if k == kind::CHAIN_UNLOAD {
                        Op::Unload(m.chain_key)
                    } else {
                        Op::Prepare(m.chain_key)
                    });
                }
                kind::STATE_SET => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    let m: codec::StateSet = meta(&f);
                    post(Op::SetState(m, f.blob));
                }
                kind::EDITOR_OPEN | kind::EDITOR_CLOSE => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    let m: codec::EditorRef = meta(&f);
                    post(if k == kind::EDITOR_OPEN {
                        Op::EditorOpen(m)
                    } else {
                        Op::EditorClose(m)
                    });
                }
                kind::WORKGROUP_ATTACH => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    post(Op::Workgroup(meta(&f)));
                }
                kind::STATE_GET => post(Op::GetState(id, meta(&f))),
                kind::PARAM_LIST => post(Op::ParamList(id, meta(&f))),
                // Protocol 3.7: every field of `param.set` is required, so a meta missing one is answered
                // invalid-params, never applied as a default.
                kind::PARAM_SET => match f.meta_as::<codec::ParamSet>() {
                    Ok(m) => post(Op::ParamSet(id, m)),
                    Err(e) => wire::error(&w, k, id, -32602, &format!("param.set: {e}")),
                },
                // Protocol 3.2: every field of `slot.setEnabled` is required (the host application's type has no
                // defaults), so a meta missing one is answered invalid-params, never applied as a default.
                kind::SLOT_SET_ENABLED => {
                    let _: serde_json::Value = meta(&f);
                    match f.meta_as::<codec::SlotEnabled>() {
                        Ok(m) => post(Op::SlotEnabled(id, m)),
                        Err(e) => wire::error(&w, k, id, -32602, &format!("slot.setEnabled: {e}")),
                    }
                }
                kind::SCAN => wire::error(
                    &w,
                    k,
                    id,
                    -32601,
                    "not in the live child (scan is --scan's)",
                ),
                _ => wire::error(&w, k, id, -32601, "not in summoner-host"),
            }
        }
    }
}

/// `chain.park` / `chain.unpark`, inline: park wakes the chain off its hub so it re-checks and waits on its
/// own seq; unpark is the hub switch: store the new group, wake the OLD hub once, wake a parked thread's seq, then
/// `chain.unparked`.
fn park(sh: &Shared, m: codec::ChainUnpark, park: bool, corr: Option<&str>) {
    let Some(c) = sh.chain(m.chain_key) else {
        return;
    };
    let page = sh.page();
    if park {
        c.parked.store(true, Ordering::SeqCst);
        if let Some(p) = &page {
            let g = c.region.group().load(Ordering::Acquire) as usize;
            let _ = p.hub_waker(g).wake();
        }
        let r = codec::ChainRef::new(m.chain_key, true);
        wire::answer_event(&sh.w, kind::CHAIN_PARKED, &r, &[], corr);
        return;
    }
    let old = c.region.group().swap(m.group.min(3), Ordering::AcqRel) as usize;
    c.parked.store(false, Ordering::SeqCst);
    if let Some(p) = &page {
        let _ = p.hub_waker(old).wake();
    }
    let _ = c.region.seq_waker().wake_all();
    let r = codec::ChainUnpark::new(m.chain_key, m.group);
    wire::answer_event(&sh.w, kind::CHAIN_UNPARKED, &r, &[], corr);
}

/// A request's `corr`, if it meets the log's corr grammar; an invalid one is ignored and counted (never a refusal: the
/// key is advisory). `--cfg mutant_corr_unchecked` (never in a normal build) takes any string; reader_edges's corr test
/// fails.
fn corr_of_request(sh: &Shared, k: u16, id: u64, f: &Frame) -> Option<String> {
    let read = codec::corr_of_meta(&f.meta);
    #[cfg(mutant_corr_unchecked)]
    let read = match read {
        codec::CorrRead::Invalid => f
            .meta_as::<serde_json::Value>()
            .ok()
            .and_then(|v| v.get("corr").and_then(|c| c.as_str()).map(str::to_string))
            .map_or(codec::CorrRead::Invalid, codec::CorrRead::Valid),
        r => r,
    };
    match read {
        codec::CorrRead::Absent => None,
        codec::CorrRead::Valid(c) => Some(c),
        codec::CorrRead::Invalid => {
            sh.corr_ignored.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "[summoner-host] kind {k} id {id}: corr is not 1..=64 bytes of [A-Za-z0-9._:-] (ignored)"
            );
            use crate::diag::{Level, Value, area, log};
            log(
                Level::Warn,
                area::HOST,
                None,
                "corr.ignored",
                vec![("kind", Value::U64(k as u64)), ("id", Value::U64(id))],
            );
            None
        }
    }
}

/// `log.set {levels, expiresInMs?}`, Inline. Check 1, the decode; checks 2-4, the host application's
/// `LogSet::check` (every nibble a known level, the expiry in 1..=86,400,000 ms); check 5, this child's drainer is
/// installed (it was given `--log-dir`; without one nothing could ever expire a level). Any refusal is `-32602` and
/// applies nothing. Then `runecraft_log::set_levels`: the child's own drainer expires it on the child's clock, back to
/// its launch levels (`SUMMONER_LOG`). The answer echoes the request's corr. Check 5 holds by construction here (`main`
/// installs the drainer with or without a folder; it refuses only after a failed install). `--cfg
/// mutant_log_set_unchecked` (never in a normal build) skips checks 2-4; reader_edges's refusal test fails.
fn log_set(sh: &Shared, id: u64, f: &Frame, corr: Option<&str>) {
    let refuse = |why: &str| {
        sh.log_set_refused.fetch_add(1, Ordering::Relaxed);
        eprintln!("[summoner-host] {why}");
        wire::answer_error(&sh.w, kind::LOG_SET, id, -32602, why, corr);
    };
    let m: codec::LogSet = match f.meta_as() {
        Ok(m) => m,
        Err(e) => return refuse(&format!("log.set: {e}")),
    };
    let checked = if cfg!(mutant_log_set_unchecked) {
        Ok((m.levels, m.expires_in_ms.map(Duration::from_millis)))
    } else {
        m.check()
    };
    let (word, expires) = match checked {
        Ok(v) => v,
        Err(why) => return refuse(why),
    };
    if !crate::diag::installed() {
        return refuse("log.set: no log folder (--log-dir): nothing could expire a level");
    }
    crate::diag::set_levels(word, expires);
    sh.log_set_applied.fetch_add(1, Ordering::Relaxed);
    wire::answer_reply(
        &sh.w,
        kind::LOG_SET,
        id,
        &codec::LogSetReply::new(crate::diag::levels()),
        &[],
        corr,
    );
}
