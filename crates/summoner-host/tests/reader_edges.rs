//! The reader's edges. The child is spawned as the host application spawns it
//! (`runecraft_ipc::process::unix::spawn_stdio_only`, a socketpair on descriptor 3) and driven with summoner-proto's
//! codec, by hand:
//! - a control message the kernel truncates (MSG_CTRUNC: more descriptors than the child's control buffer holds)
//!   exits 4, "truncated descriptors"; EOF exits 0;
//! - a `chain.load` whose `instanceId` is present is checked, 0 included; one without it is not;
//! - protocol 3.3, read off the frames themselves (the summoner supervisor surfaces neither yet):
//!   `state.applied.childGeneration` is the slot's generation once the set took effect (what `state.generations`
//!   then says), and a state over `MAX_BLOB` is answered `state.get {ok: false, reason: "tooLarge", bytes}` with no blob.
//!
//! Every process a test starts is ended by its recorded pid (never by name).
#![forbid(unsafe_code)]

mod common;

use common::*;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use summoner_proto::codec::{self, Direction, Frame, kind};
use summoner_proto::layout::Geometry;
use summoner_rt::region::{ChildPage, Region};

struct Child {
    pid: u32,
    sock: UnixStream,
    dec: codec::Decoder,
    pending: Vec<Frame>,
    home: PathBuf,
    _page: ChildPage,
    /// The `hello` reply (with its page pair).
    hello: Option<codec::HelloReply>,
}

impl Child {
    /// A live child, its `hello` answered (the page sent with it).
    fn up(name: &str) -> Child {
        Child::up_with(name, &[], &[])
    }

    /// [`Child::up`] with more arguments and environment.
    fn up_with(name: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Child {
        let home = scratch(name);
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("CFFIXED_USER_HOME".into(), home.clone().into()),
            ("SUMMONER_LAB_DEATH".into(), "kill".into()),
        ];
        env.extend(extra_env.iter().map(|(k, v)| (k.into(), v.into())));
        let args: Vec<std::ffi::OsString> = args.iter().map(Into::into).collect();
        let pid = runecraft_ipc::process::unix::spawn_stdio_only(
            &child_bin(),
            &args,
            &env,
            theirs.as_fd(),
            None,
        )
        .expect("spawn");
        drop(theirs);
        ours.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let (page, pfd) = ChildPage::create().expect("the child page");
        let mut c = Child {
            pid,
            sock: ours,
            dec: codec::Decoder::new(Direction::ToCore),
            pending: Vec::new(),
            home,
            _page: page,
            hello: None,
        };
        let hello = codec::Hello::new(name.into(), 1);
        c.send_fd(kind::HELLO, 1, &hello, &pfd);
        let f = c
            .frame(|f| f.header.id == 1, Duration::from_secs(10))
            .expect("hello's reply");
        c.hello = f.meta_as().ok();
        c
    }

    /// A request with one descriptor (its first byte carries it, as the supervisor sends one).
    fn send_fd<T: serde::Serialize>(&mut self, k: u16, id: u64, meta: &T, fd: &impl AsFd) {
        let frame = codec::encode_msg(k, codec::FF_FDS, id, meta, &[]);
        let n = runecraft_ipc::uds::send_with_fd(&self.sock, &frame, fd.as_fd()).unwrap();
        self.sock.write_all(&frame[n..]).unwrap();
    }

    /// A request without descriptors.
    fn send<T: serde::Serialize>(&mut self, k: u16, id: u64, meta: &T, blob: &[u8]) {
        self.sock
            .write_all(&codec::encode_msg(k, 0, id, meta, blob))
            .unwrap();
    }

    /// The first frame matching `pred`, within `within`.
    fn frame(&mut self, pred: impl Fn(&Frame) -> bool, within: Duration) -> Option<Frame> {
        let until = Instant::now() + within;
        loop {
            if let Some(i) = self.pending.iter().position(&pred) {
                return Some(self.pending.remove(i));
            }
            if Instant::now() > until {
                return None;
            }
            let mut buf = [0u8; 65536];
            match self.sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    let p = &mut self.pending;
                    self.dec
                        .feed(&buf[..n], &mut |f| p.push(f))
                        .expect("a frame");
                }
                Err(_) => {}
            }
        }
    }

    /// [`Child::load_slots`] with `extra` keys merged into the meta (a `corr`, say): the `chain.loaded`
    /// frame (with its echo).
    fn load_meta_frame(&mut self, key: u64, slots: &[&str], extra: serde_json::Value) -> Frame {
        let geo = Geometry {
            ch_in: 2,
            ..Geometry::stereo(64)
        };
        let (region, fd) = Region::create(geo, key, SR, 0).expect("a region");
        let mut m = serde_json::to_value({
            let mut msg1 = codec::ChainLoad::default();
            msg1.chain_key = key;
            msg1.instance_id = region.instance_id();
            msg1.rate = SR;
            msg1.max_frames = geo.max_frames;
            msg1.ch_in = geo.ch_in;
            msg1.ch_out = geo.ch_out;
            msg1.max_midi = geo.max_midi;
            msg1.max_params = geo.max_params;
            msg1.tap_slot = geo.tap_slot;
            msg1.fds = 1;
            msg1.slots = slots
                .iter()
                .map(|x| {
                    let mut msg = codec::SlotDesc::default();
                    msg.plugin = slot(x);
                    msg
                })
                .collect();
            msg1
        })
        .unwrap();
        if let (Some(o), Some(e)) = (m.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        self.send_fd(kind::CHAIN_LOAD, 200 + key, &m, &fd);
        self.frame(
            |f| f.header.kind == kind::CHAIN_LOADED && f.header.id == 0,
            Duration::from_secs(10),
        )
        .expect("chain.loaded")
    }

    /// `chain.load` of an empty chain over a fresh region, its meta's `instanceId` as `id` says (`None`: absent);
    /// `chain.loaded`'s `ok`.
    fn load(&mut self, key: u64, id: impl Fn(u64) -> Option<u64>) -> bool {
        self.load_slots(key, id, &[])
    }

    /// [`Child::load`] with `slots` (descriptions).
    fn load_slots(&mut self, key: u64, id: impl Fn(u64) -> Option<u64>, slots: &[&str]) -> bool {
        let geo = Geometry {
            ch_in: 2,
            ..Geometry::stereo(64)
        };
        let (region, fd) = Region::create(geo, key, SR, 0).expect("a region");
        let mut m = serde_json::to_value({
            let mut msg1 = codec::ChainLoad::default();
            msg1.chain_key = key;
            msg1.instance_id = 0;
            msg1.rate = SR;
            msg1.max_frames = geo.max_frames;
            msg1.ch_in = geo.ch_in;
            msg1.ch_out = geo.ch_out;
            msg1.max_midi = geo.max_midi;
            msg1.max_params = geo.max_params;
            msg1.tap_slot = geo.tap_slot;
            msg1.fds = 1;
            msg1.slots = slots
                .iter()
                .map(|x| {
                    let mut msg = codec::SlotDesc::default();
                    msg.plugin = slot(x);
                    msg
                })
                .collect();
            msg1
        })
        .unwrap();
        let o = m.as_object_mut().unwrap();
        o.remove("instanceId");
        if let Some(v) = id(region.instance_id()) {
            o.insert("instanceId".into(), v.into());
        }
        self.send_fd(kind::CHAIN_LOAD, 100 + key, &m, &fd);
        let f = self
            .frame(
                |f| f.header.kind == kind::CHAIN_LOADED && f.header.id == 0,
                Duration::from_secs(10),
            )
            .expect("chain.loaded");
        let r: codec::ChainLoaded = f.meta_as().unwrap();
        assert_eq!(r.chain_key, key);
        r.ok
    }

    fn exit(&self) -> runecraft_ipc::process::ChildExit {
        runecraft_ipc::process::unix::wait_child(self.pid).expect("reap our child")
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // Our own child, by its recorded pid; harmless if it is gone already.
        common::sigkill(self.pid);
        let _ = runecraft_ipc::process::unix::wait_child(self.pid);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Five descriptors in one `sendmsg` (the child's control buffer holds four: the FFI crate's `MAX_FDS`), sent by a
/// Python process that holds a dup of our end as its stdin (the standard library's `sendmsg`; no unsafe code here). The
/// kernel truncates the control message; the child dies 4 on MSG_CTRUNC.
#[test]
fn a_truncated_control_message_exits_4() {
    let c = Child::up("o4-ctrunc");
    let dup = OwnedFd::from(c.sock.try_clone().unwrap());
    let py = r#"
import array, os, socket
s = socket.socket(fileno=0)
fds = [os.open("/dev/null", os.O_RDONLY) for _ in range(5)]
s.sendmsg([b"\0"], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array("i", fds).tobytes())])
"#;
    let ok = std::process::Command::new("/usr/bin/python3")
        .args(["-I", "-c", py])
        .stdin(std::process::Stdio::from(dup))
        .env_clear()
        .status()
        .unwrap()
        .success();
    assert!(ok, "the sender ran");
    assert_eq!(
        c.exit(),
        runecraft_ipc::process::ChildExit::Exited(4),
        "MSG_CTRUNC: truncated descriptors"
    );
}

/// The control: the same child, the socket closed (EOF), exits 0.
#[test]
fn eof_exits_0() {
    let c = Child::up("o4-eof");
    c.sock.shutdown(std::net::Shutdown::Both).unwrap();
    assert_eq!(c.exit(), runecraft_ipc::process::ChildExit::Exited(0));
}

/// `instanceId` present is checked, 0 included; absent, there is nothing to check.
#[test]
fn a_present_instance_id_is_checked_zero_included() {
    let mut c = Child::up("o4-instance");
    assert!(c.load(1, Some), "the region's own instanceId: loads");
    assert!(!c.load(2, |_| Some(0)), "instanceId 0 is checked: refused");
    assert!(!c.load(3, |id| Some(id ^ 1)), "another instanceId: refused");
    assert!(c.load(4, |_| None), "no instanceId: nothing to check");
}

/// The slot's generation in `state.generations` (the child's own count).
fn generation(c: &mut Child, id: u64, key: u64) -> u64 {
    c.send(kind::STATE_GENERATIONS, id, &serde_json::json!({}), &[]);
    let f = c
        .frame(|f| f.header.id == id, Duration::from_secs(10))
        .expect("state.generations' reply");
    let g: codec::Generations = f.meta_as().unwrap();
    g.slots
        .iter()
        .find(|e| e.chain_key == key && e.slot == 0)
        .expect("the slot")
        .child_gen
}

/// `state.set` of `text` (a state blob {builtin}) to slot 0: its `state.applied`.
fn set(c: &mut Child, id: u64, key: u64, generation: u64, text: &[u8]) -> codec::StateApplied {
    let m = codec::StateSet::new(key, 0, generation, codec::StateMode::V1);
    c.send(
        kind::STATE_SET,
        id,
        &m,
        &summoner_proto::state_blob::builtin(text),
    );
    let f = c
        .frame(
            |f| f.header.kind == kind::STATE_APPLIED,
            Duration::from_secs(10),
        )
        .expect("state.applied");
    f.meta_as().unwrap()
}

/// Protocol 3.3: `state.applied.childGeneration` is the slot's own generation once the applied state took effect
/// (the host application's capture baseline): one above the generation before the set, and what `state.generations`
/// says after.
#[test]
fn state_applied_carries_the_childs_generation() {
    let mut c = Child::up("p33-gen");
    assert!(c.load_slots(5, Some, &[r#"{"format":"fake","fake":{"name":"g"}}"#]));
    let before = generation(&mut c, 10, 5);
    let a = set(&mut c, 11, 5, 7, b"gain=0.5");
    assert!(a.ok, "{a:?}");
    assert_eq!(a.generation, 7, "the host application's generation echoed");
    assert_eq!(a.child_generation, Some(before + 1), "{a:?}");
    assert_eq!(generation(&mut c, 12, 5), before + 1);
    // A refused set (the double's FAIL is applied, then refused: it ran) moves it on too, and says so.
    let b = set(&mut c, 13, 5, 8, b"FAIL");
    assert!(!b.ok);
    assert_eq!(b.child_generation, Some(before + 2), "{b:?}");
}

/// Protocol 3.3: a state over `MAX_BLOB` is answered `{ok: false, reason: "tooLarge", bytes}` (the
/// framed size) keeping chainKey / slot / childGen, with no blob; one just under the cap is the ordinary reply.
#[test]
fn a_state_over_the_cap_is_answered_too_large_with_no_blob() {
    let mut c = Child::up("p33-big");
    assert!(c.load_slots(6, Some, &[r#"{"format":"fake","fake":{"name":"big"}}"#]));
    let header = summoner_proto::state_blob::builtin(b"").len();
    let fits = codec::MAX_BLOB - header;
    let a = set(&mut c, 20, 6, 1, format!("BIGGET={fits}").as_bytes());
    assert!(a.ok);
    let get = |c: &mut Child, id: u64| {
        c.send(kind::STATE_GET, id, &codec::StateGet::new(6, 0, false), &[]);
        c.frame(|f| f.header.id == id, Duration::from_secs(30))
            .expect("state.get's reply")
    };
    let f = get(&mut c, 21);
    assert!(!f.header.is_error());
    let r: codec::StateGetReply = f.meta_as().unwrap();
    assert!(r.ok, "at the cap: the ordinary reply {r:?}");
    assert_eq!(f.blob.len(), codec::MAX_BLOB, "the whole state, framed");
    let a = set(&mut c, 22, 6, 2, format!("BIGGET={}", fits + 1).as_bytes());
    assert!(a.ok);
    let f = get(&mut c, 23);
    assert!(!f.header.is_error(), "a reply, not an error frame");
    assert!(f.blob.is_empty(), "nothing of the state is sent");
    let r: codec::StateGetReply = f.meta_as().unwrap();
    assert_eq!(
        (r.chain_key, r.slot, r.child_gen),
        (6, 0, a.child_generation.unwrap()),
        "{r:?}"
    );
    assert!(!r.ok);
    assert_eq!(r.reason.as_deref(), Some(codec::StateGetReply::TOO_LARGE));
    assert_eq!(r.bytes, Some(codec::MAX_BLOB as u64 + 1), "the framed size");
}

/// A `chain.load` on a key that is live is refused (`chain.loaded {ok: false}`) and
/// the live chain stays as it was (its slot still answers); after an unload the key loads again.
#[test]
fn a_chain_load_on_a_live_key_is_refused() {
    let mut c = Child::up("s4-live-key");
    assert!(c.load_slots(7, Some, &[r#"{"format":"fake","fake":{"name":"g"}}"#]));
    let before = generation(&mut c, 30, 7);
    assert!(!c.load(7, Some), "the second load of a live key is refused");
    assert_eq!(
        generation(&mut c, 31, 7),
        before,
        "the live chain is untouched"
    );
    c.send(kind::CHAIN_UNLOAD, 32, &codec::ChainRef::new(7, true), &[]);
    c.frame(
        |f| f.header.kind == kind::CHAIN_UNLOADED,
        Duration::from_secs(10),
    )
    .expect("chain.unloaded");
    assert!(c.load(7, Some), "after the unload, the key loads again");
}

/// Protocol 3.4: the child's own `chain.loaded` says `hasEditor` per slot: absent (true) for
/// a hosted slot, `false` for one this build does not host. Read off the frame: the summoner supervisor forces false
/// for a slot that is not loaded on its own, so only the frame shows what the child sent.
#[test]
fn chain_loaded_carries_has_editor() {
    let mut c = Child::up("p34-has-editor");
    let geo = Geometry {
        ch_in: 2,
        ..Geometry::stereo(64)
    };
    let (region, fd) = Region::create(geo, 8, SR, 0).expect("a region");
    let m = {
        let mut msg1 = codec::ChainLoad::default();
        msg1.chain_key = 8;
        msg1.instance_id = region.instance_id();
        msg1.rate = SR;
        msg1.max_frames = geo.max_frames;
        msg1.ch_in = geo.ch_in;
        msg1.ch_out = geo.ch_out;
        msg1.max_midi = geo.max_midi;
        msg1.max_params = geo.max_params;
        msg1.tap_slot = geo.tap_slot;
        msg1.fds = 1;
        msg1.slots = [
            r#"{"format":"fake","fake":{"name":"g"}}"#,
            r#"{"format":"vst3","path":"/nowhere/none.vst3","classId":"00000000000000000000000000000000","name":"none"}"#,
        ]
        .iter()
        .map(|x| { let mut msg = codec::SlotDesc::default(); msg.plugin = slot(x); msg })
        .collect();
        msg1
    };
    c.send_fd(kind::CHAIN_LOAD, 108, &m, &fd);
    let f = c
        .frame(
            |f| f.header.kind == kind::CHAIN_LOADED && f.header.id == 0,
            Duration::from_secs(10),
        )
        .expect("chain.loaded");
    let r: codec::ChainLoaded = f.meta_as().unwrap();
    let has: Vec<bool> = r.slots.iter().map(|s| s.has_editor).collect();
    assert_eq!(has, [true, false], "{r:?}");
    let raw: serde_json::Value = f.meta_as().unwrap();
    assert!(
        raw["slots"][0].get("hasEditor").is_none(),
        "true is omitted (a 3.3 slot's bytes): {raw}"
    );
    assert_eq!(raw["slots"][1]["hasEditor"], false, "{raw}");
}

/// The records of a child's log folder (each line one JSON record), once its drainer flushed (250 ms).
fn records(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::thread::sleep(Duration::from_millis(600));
    let cur = std::fs::read_link(dir.join("current")).expect("a current link");
    std::fs::read_to_string(dir.join(cur))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// A child with a log folder (its drainer installed) at `levels`.
fn logged_child(name: &str, levels: &str) -> (Child, PathBuf) {
    let dir = scratch(&format!("{name}-logs"));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let d = dir.to_str().unwrap().to_string();
    let c = Child::up_with(name, &["--log-dir", &d], &[("SUMMONER_LOG", levels)]);
    (c, dir)
}

/// A request's `corr`, a top-level meta string of the log's corr grammar (1..=64 bytes of `[A-Za-z0-9._:-]`, the host
/// application's `runecraft_log::corr_ok`), is on every record the op writes and on the answer (`chain.loaded`); a
/// request without one writes `corr: null`; an invalid one (too long, a space, a byte outside the grammar, not a
/// string) is ignored: the op runs, `corr` null, no echo. Mutation `--cfg mutant_corr_unchecked` fails the space row.
#[test]
fn a_requests_corr_reaches_the_records_its_op_writes() {
    let (mut c, dir) = logged_child("p36-corr", "host=info,plugin=info");
    let echo = |f: &Frame| codec::corr_of_meta(&f.meta);
    let f = c.load_meta_frame(
        51,
        &[r#"{"format":"fake","fake":{"name":"g"}}"#],
        serde_json::json!({"corr": "r:7"}),
    );
    assert_eq!(
        echo(&f),
        codec::CorrRead::Valid("r:7".into()),
        "the answer echoes it"
    );
    let f = c.load_meta_frame(
        52,
        &[r#"{"format":"fake","fake":{"name":"g"}}"#],
        serde_json::json!({}),
    );
    assert_eq!(echo(&f), codec::CorrRead::Absent);
    let bad = [
        serde_json::json!("x".repeat(65)),
        serde_json::json!("a b"),
        serde_json::json!("r/7"),
        serde_json::json!("é"),
        serde_json::json!(7),
    ];
    for (i, b) in bad.iter().enumerate() {
        let f = c.load_meta_frame(
            53 + i as u64,
            &[r#"{"format":"fake","fake":{"name":"g"}}"#],
            serde_json::json!({ "corr": b }),
        );
        let r: codec::ChainLoaded = f.meta_as().unwrap();
        assert!(r.ok, "an invalid corr is ignored, the op runs: {b}");
        assert_eq!(
            echo(&f),
            codec::CorrRead::Absent,
            "no echo of an ignored corr: {b}"
        );
    }
    let lines = records(&dir);
    let corr_of = |chain: u64, msg: &str| -> Vec<serde_json::Value> {
        lines
            .iter()
            .filter(|l| l["msg"] == msg && l["fields"]["chain"] == chain)
            .map(|l| l["corr"].clone())
            .collect()
    };
    assert_eq!(corr_of(51, "chain.load"), [serde_json::json!("r:7")]);
    assert_eq!(corr_of(51, "slot.load"), [serde_json::json!("r:7")]);
    assert_eq!(corr_of(52, "chain.load"), [serde_json::Value::Null]);
    for i in 0..bad.len() as u64 {
        assert_eq!(
            corr_of(53 + i, "chain.load"),
            [serde_json::Value::Null],
            "{}",
            bad[i as usize]
        );
    }
    assert_eq!(
        lines.iter().filter(|l| l["msg"] == "corr.ignored").count(),
        bad.len(),
        "one warning per ignored corr"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn log_set(c: &mut Child, id: u64, meta: &serde_json::Value) -> Frame {
    c.send(kind::LOG_SET, id, meta, &[]);
    c.frame(
        |f| f.header.kind == kind::LOG_SET && f.header.id == id,
        Duration::from_secs(5),
    )
    .expect("log.set's answer")
}

/// `log.set` applies the word (the reply names it, and echoes the corr), and its expiry returns the child
/// to its launch levels on its own clock, with one `log.levels {reason: "expired"}` record.
#[test]
fn log_set_applies_the_word_and_its_expiry_restores_the_launch_levels() {
    let (mut c, dir) = logged_child("p36-logset", "all=info");
    let info = 0x3333_3333_3333_3333u64;
    let word = runecraft_log::with_level(
        info,
        summoner_proto::log_vocab::HOST,
        runecraft_log::Level::Debug,
    );
    let f = log_set(
        &mut c,
        300,
        &serde_json::json!({"levels": word, "expiresInMs": 300, "corr": "r:9"}),
    );
    assert!(!f.header.is_error(), "{}", String::from_utf8_lossy(&f.meta));
    let r: codec::LogSetReply = f.meta_as().unwrap();
    assert_eq!(r.levels, word);
    assert_eq!(
        codec::corr_of_meta(&f.meta),
        codec::CorrRead::Valid("r:9".into())
    );
    std::thread::sleep(Duration::from_millis(700));
    let lines = records(&dir);
    let levels: Vec<&serde_json::Value> =
        lines.iter().filter(|l| l["msg"] == "log.levels").collect();
    assert_eq!(levels.len(), 2, "applied, then expired: {levels:?}");
    assert_eq!(levels[1]["fields"]["reason"], "expired");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every refusal is `-32602` and applies nothing (no `log.levels` record): a nibble that is no level, an
/// expiry outside 1..=86,400,000 ms, debug or trace without an expiry, no `levels`. Mutation `--cfg
/// mutant_log_set_unchecked` fails it.
#[test]
fn a_bad_log_set_is_refused_and_applies_nothing() {
    let (mut c, dir) = logged_child("p36-logset-bad", "all=info");
    let info = 0x3333_3333_3333_3333u64;
    let trace = runecraft_log::with_level(
        info,
        summoner_proto::log_vocab::HOST,
        runecraft_log::Level::Trace,
    );
    for (i, m) in [
        serde_json::json!({"levels": info | 6}),
        serde_json::json!({"levels": info, "expiresInMs": 0}),
        serde_json::json!({"levels": info, "expiresInMs": 86_400_001u64}),
        serde_json::json!({"levels": trace}),
        serde_json::json!({}),
        serde_json::json!({"levels": "info"}),
    ]
    .iter()
    .enumerate()
    {
        let f = log_set(&mut c, 400 + i as u64, m);
        assert!(f.header.is_error(), "refused: {m}");
        let e: codec::ErrorReply = f.meta_as().unwrap();
        assert_eq!(e.code, -32602, "{m}: {}", e.message);
    }
    let lines = records(&dir);
    assert_eq!(
        lines.iter().filter(|l| l["msg"] == "log.levels").count(),
        0,
        "nothing applied"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Check 5 (a drainer installed): this child installs its drainer at start with or without `--log-dir`
/// (without one it echoes warnings to stderr), so a child given no folder still expires a level, and takes `log.set`.
/// (The host application sends none to such a child: its supervisor forwards only with a child folder. Check 5 refuses
/// only after a failed install, which no wire input can cause: a guard by construction, with no mutant.)
#[test]
fn a_child_without_a_log_folder_still_has_its_drainer_and_takes_log_set() {
    let mut c = Child::up("p36-logset-nodir");
    let word = runecraft_log::with_level(
        0x3333_3333_3333_3333,
        summoner_proto::log_vocab::HOST,
        runecraft_log::Level::Debug,
    );
    let f = log_set(
        &mut c,
        500,
        &serde_json::json!({"levels": word, "expiresInMs": 200}),
    );
    assert!(!f.header.is_error(), "{}", String::from_utf8_lossy(&f.meta));
    let r: codec::LogSetReply = f.meta_as().unwrap();
    assert_eq!(r.levels, word);
}

/// `log.set` is Inline. While the message thread sleeps inside a plugin (the double's `SLOWGET`,
/// in `state.get`), a `log.set` sent after the `state.get` is answered first, at once. The hello claims 3.6, the
/// minor that has `log.set` (the claim is tied to the behaviour).
#[test]
fn the_hello_claims_3_6_and_log_set_is_answered_inline() {
    let (mut c, dir) = logged_child("p36-inline", "all=info");
    let h = c.hello.clone().expect("a hello reply");
    // 4.0 carries 3.6's log.set; the host application sends it to any 4.x child (no per-minor gate).
    assert!(
        h.protocol >> 16 == 4,
        "the child claims 4.x, which carries 3.6's log.set: {:#x}",
        h.protocol
    );
    assert!(c.load_slots(61, Some, &[r#"{"format":"fake","fake":{"name":"g"}}"#]));
    assert!(set(&mut c, 610, 61, 1, b"SLOWGET=1500").ok);
    c.send(
        kind::STATE_GET,
        611,
        &{
            let mut msg = codec::StateGet::default();
            msg.chain_key = 61;
            msg.slot = 0;
            msg
        },
        &[],
    );
    std::thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let f = log_set(
        &mut c,
        612,
        &serde_json::json!({"levels": 0x3333_3333_3333_3333u64}),
    );
    let took = t0.elapsed();
    assert!(!f.header.is_error());
    assert!(
        took < Duration::from_millis(500),
        "answered on the reader, not behind the 1.5 s plugin call: {took:?}"
    );
    assert!(
        !c.pending.iter().any(|f| f.header.kind == kind::STATE_GET),
        "state.get still in its plugin call"
    );
    c.frame(
        |f| f.header.kind == kind::STATE_GET && f.header.id == 611,
        Duration::from_secs(5),
    )
    .expect("state.get's reply, after");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The hello reports the page (wired whole pages, nothing refused in a test), and `chain.prepared`
/// the chain's pair after the prepare (present, so it replaces the host application's; absent only from a 3.5 child).
#[test]
fn hello_and_chain_prepared_report_the_wired_memory() {
    let mut c = Child::up("p36-mem");
    let page = runecraft_rt::os::page_size() as u64;
    let h = c.hello.clone().expect("a hello reply");
    assert!(
        h.wired_bytes >= page && h.wired_bytes.is_multiple_of(page),
        "{h:?}"
    );
    assert_eq!(h.unwired_bytes, 0, "{h:?}");
    let f = c.load_meta_frame(
        71,
        &[r#"{"format":"fake","fake":{"name":"g"}}"#],
        serde_json::json!({}),
    );
    let loaded: codec::ChainLoaded = f.meta_as().unwrap();
    assert!(loaded.wired_bytes > 0);
    c.send(
        kind::CHAIN_PREPARE,
        710,
        &codec::ChainRef::new(71, true),
        &[],
    );
    let f = c
        .frame(
            |f| f.header.kind == kind::CHAIN_PREPARED,
            Duration::from_secs(10),
        )
        .expect("chain.prepared");
    let p: codec::ChainPrepared = f.meta_as().unwrap();
    assert!(p.ok);
    let w = p.wired_bytes.expect("a 3.6 child reports the pair");
    assert!(
        w >= loaded.wired_bytes / 2 && w > 0,
        "{p:?} after {loaded:?}"
    );
    assert_eq!(p.unwired_bytes, Some(0), "{p:?}");
}
