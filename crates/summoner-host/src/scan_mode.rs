//! `summoner-host --scan`: the scan child. One process, one bundle:
//!
//! - start-up as the live child (the crash handler, descriptor 3, the orphan watch), but NO chains, NO mach channel
//!   and no slot-0 port; the main thread, which runs the probe, at QoS utility (a probe never competes with the
//!   live child or the host application), set through the pinned FFI crate's `set_thread_qos_utility` and read
//!   back (`summoner_sys::qos`), its class said on stderr;
//! - the wire, over the existing framing: `hello` → its reply (`host: "native"`, the formats; a page descriptor, if
//!   one is sent, is taken and dropped: a scan child has no chains); `scan` (64, a request kind), decoded only
//!   through `codec::Scan`'s own check → its receipt REPLY at once (its id), then `scanned` (65, an EVENT kind, id
//!   0: the codec's decoder accepts it only so), built by the CORE's encoder (`codec::encode_scanned`):
//!   form B (the classes in the blob, discovery's on a validation `failed`) when the `scan` offered
//!   `descriptionsBlob: 1`, form A otherwise; past a bound, form B's `overLimit` frame; a meta past MAX_META, its
//!   header alone, then the host application's EOF awaited for at most the host application's `scan::SEND_TIMEOUT`,
//!   then exit 0;
//!   `shutdown` → its reply, exit 0. ONE scan per process: a second is refused (an error reply).
//!   Every live-child kind is refused ("not in scan mode");
//! - the probe itself: `summoner_probe::scan`, unchanged, on the main thread, with the crash breadcrumb
//!   `plugin-op: plugin.scan <path>` around it, so a crash log names the bundle;
//! - the per-step deadline (`--step-ms`, default [`DEFAULT_STEP_MS`], 25 000: real VST3s have taken ~21 s in
//!   discovery; inside the scan's 30 s budget): a step past it exits **75**, "my own step deadline": the supervisor
//!   maps it to `Timeout {step}`; the child never SENDS `crashed` or `timeout`;
//! - no AppKit, no CoreGraphics, no window listing: these no-UI rules are deferred, not yet enforced (release_build's
//!   link guard holds for this mode too: it is the same binary).
//!
//! Exit codes: 0 after `shutdown` (or the host application's EOF), 3 orphaned, 4 a protocol error, 75 a step's
//! deadline; a signal is a crash (the supervisor's verdict).
use crate::wire::{self, die, empty};
use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use summoner_proto::codec::{self, Direction, Frame, kind};

/// The child's own step deadline exit (the supervisor maps it to Timeout).
pub const DEADLINE_EXIT: i32 = 75;

struct Steps(Arc<Mutex<(String, Instant)>>);

impl summoner_probe::Step for Steps {
    fn step(&self, name: &str) {
        *self.0.lock().unwrap() = (name.to_string(), Instant::now());
    }
}

/// The deadline thread: a step that runs past `ms` ends the process with [`DEADLINE_EXIT`].
fn deadline(ms: u64) -> Arc<Mutex<(String, Instant)>> {
    let now = Arc::new(Mutex::new(("start".to_string(), Instant::now())));
    let watched = now.clone();
    std::thread::Builder::new()
        .name("hc-scan-deadline".into())
        .spawn(move || {
            let limit = Duration::from_millis(ms);
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let (step, since) = watched.lock().unwrap().clone();
                if step != "done" && since.elapsed() > limit {
                    eprintln!("[summoner-host] scan: step {step} passed its {ms} ms deadline (exit {DEADLINE_EXIT})");
                    summoner_sys::proc_exit::exit_now(DEADLINE_EXIT);
                }
            }
        })
        .expect("the scan deadline thread");
    now
}

/// The encoder's input (the codec's `Scanned` docs): the classes in discovery order, the validated
/// ones on `ok`, discovery's (`report.discovered`) on `failed`. Form B carries them in its blob; form A writes
/// them only on `ok` (`[]` on `failed`, as 3.1 does), so the same input serves both forms.
fn encoder_input(report: &summoner_probe::Report) -> codec::Scanned {
    let mut s = report.scanned.clone();
    if matches!(s.outcome, codec::ScannedOutcome::Failed { .. }) {
        s.classes = report.discovered.clone();
    }
    s
}

/// The ONE point that sends `scanned`: the CORE's encoder (`codec::encode_scanned`, never host's), form B when the
/// `scan` offered `descriptionsBlob: 1`, form A (frozen) otherwise. It never panics and never calls `encode_msg` on an
/// unchecked size: past a bound it gives form B's `overLimit` frame; a meta past MAX_META gives only the 24-byte header
/// with its true lengths. Every variant is bytes to send as they are. An engine limit is said on stderr BEFORE its
/// frame goes: the host application's scan supervisor kills the group as soon as it has judged the frame a fault, and
/// its stderr capture must already hold why.
fn send_scanned(
    w: &wire::Writer,
    report: &summoner_probe::Report,
    offered: bool,
) -> codec::ScannedFrame {
    let input = encoder_input(report);
    let f = codec::encode_scanned(&input, offered);
    match &f {
        codec::ScannedFrame::Whole(_) => {}
        codec::ScannedFrame::OverLimit { bound, .. } => eprintln!(
            "[summoner-host] scan: {} classes pass form B's {} bound: sending the form B meta with overLimit \
             {:?} and an empty blob (an engine limit)",
            input.classes.len(),
            bound.word(),
            bound.word()
        ),
        codec::ScannedFrame::HeaderOnly {
            meta_len, blob_len, ..
        } => eprintln!(
            "[summoner-host] scan: the scanned line's meta is {meta_len} bytes (blob {blob_len}), over MAX_META \
             ({}): sending its header only (an engine limit), then waiting for the host application's EOF",
            codec::MAX_META
        ),
    }
    wire::send(w, f.bytes().to_vec());
    f
}

/// After a header-only `scanned` ("then wait for the host application's EOF"): nothing
/// more can follow on this connection (summoner-proto's decoder has refused the header), so the child reads and drops
/// what arrives until the host application closes, for at most `bound`, then ends as after a shutdown. The bound is the
/// host application's own send stall, `summoner_scan::scan::SEND_TIMEOUT` (2 s, the scan supervisor's socket write
/// timeout): the host application's constant, never a copy. True when the host application's EOF was seen within it.
fn wait_for_eof(sock: &UnixStream, bound: Duration) -> bool {
    use std::io::Read;
    let until = Instant::now() + bound;
    let mut buf = [0u8; 4096];
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || sock.set_read_timeout(Some(left)).is_err() {
            return false;
        }
        match (&*sock).read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return false;
            }
            Err(_) => return true, // a reset: the host application is gone, as an EOF
        }
    }
}

/// The scan child's whole life: frames in on `sock`, answered on `w`. Never returns.
/// The scan step deadline when the host application passes no `--step-ms` (it never does: the scan child's argv). In
/// the gated scan of installed plugins, 10 000 ms timed out on 4 of 23 real VST3s that finish in
/// 5-21 s; 25 000 ms leaves the whole-scan budget (`summoner_scan::scan::BUDGET`, 30 s) room for the spawn, the hello
/// and the reply, so a hung step is still the child's own exit 75 (`timeout {step}`), not the supervisor's kill.
pub const DEFAULT_STEP_MS: u64 = 25_000;

pub fn run(sock: UnixStream, w: wire::Writer, step_ms: u64, repeat: usize) -> ! {
    // the probe (this thread) at QoS utility, and what it actually got, on stderr (the supervisor keeps it).
    match (
        runecraft_rt::os::set_thread_qos_utility(),
        summoner_sys::qos::current(),
    ) {
        (Ok(()), Ok((q, _))) => {
            eprintln!("[summoner-host] scan: the probe thread runs at QoS {q:?}")
        }
        (set, read) => eprintln!(
            "[summoner-host] scan: QoS utility not confirmed (set {set:?}, read {read:?})"
        ),
    }
    let mut dec = codec::Decoder::new(Direction::ToChild);
    let mut buf = vec![0u8; 64 * 1024];
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();
    let mut scanned = false;
    let mut steps: Option<Arc<Mutex<(String, Instant)>>> = None;
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
            match k {
                kind::HELLO => {
                    let m: codec::Hello = f
                        .meta_as()
                        .unwrap_or_else(|e| die(4, &format!("protocol error: {e}")));
                    // A page descriptor, if the host application sends one, is not needed here: taken and dropped.
                    for _ in 0..m.fds {
                        drop(fds.pop_front());
                    }
                    let r = {
                        let mut msg = codec::HelloReply::default();
                        msg.pid = std::process::id();
                        msg.protocol = wire::PROTOCOL;
                        msg.child_id = m.child_id;
                        msg.host = "native".to_string();
                        msg.formats = vec!["VST3".to_string(), "AudioUnit".to_string()];
                        msg.mach_slot0 = false;
                        // A scan child maps no page and hosts no chain: nothing wired.
                        msg.wired_bytes = 0;
                        msg.unwired_bytes = 0;
                        msg
                    };
                    wire::reply(&w, k, id, &r, &[]);
                }
                kind::PING => wire::reply(&w, k, id, &empty(), &[]),
                kind::SCAN if scanned => wire::error(&w, k, id, -32600, "one scan per process"),
                kind::SCAN => {
                    let s: codec::Scan = match f.meta_as() {
                        Ok(s) => s,
                        Err(e) => {
                            wire::error(&w, k, id, -32602, &format!("not a scan: {e}"));
                            continue;
                        }
                    };
                    scanned = true;
                    wire::reply(&w, k, id, &empty(), &[]); // the receipt, before the probe runs
                    let now = steps.get_or_insert_with(|| deadline(step_ms)).clone();
                    summoner_sys::crash::set_breadcrumb("plugin.scan", s.path());
                    let mut report = summoner_probe::scan(&s, &Steps(now.clone()));
                    summoner_sys::crash::clear_breadcrumb();
                    // Done: no step is running any more (the deadline must not fire while the host application reads).
                    *now.lock().unwrap() = ("done".to_string(), Instant::now());
                    // test-hooks only (`SUMMONER_TEST_SCAN_REPEAT=<n>`): each class n times, as a shell bundle's many
                    // classes would make the line; 1 (no repeat) in every other build.
                    if repeat > 1 {
                        let one = std::mem::take(&mut report.scanned.classes);
                        for _ in 0..repeat {
                            report.scanned.classes.extend(one.iter().cloned());
                        }
                    }
                    if let codec::ScannedFrame::HeaderOnly { .. } =
                        send_scanned(&w, &report, s.offers_descriptions_blob())
                    {
                        // Over MAX_META: only the header went (the host application records an EngineFault, no strike).
                        // Nothing more can follow on this connection: wait for the host application's EOF, bounded by
                        // the host application's own send stall, then close and end as after a shutdown.
                        let bound = summoner_scan::scan::SEND_TIMEOUT;
                        let eof = wait_for_eof(&sock, bound);
                        eprintln!(
                            "[summoner-host] scan: {}; exiting 0",
                            if eof {
                                "the host application closed".to_string()
                            } else {
                                format!("no EOF within {} ms", bound.as_millis())
                            }
                        );
                        let _ = sock.shutdown(std::net::Shutdown::Both);
                        summoner_sys::proc_exit::exit_now(0);
                    }
                }
                kind::SHUTDOWN => {
                    wire::reply(&w, k, id, &empty(), &[]);
                    summoner_sys::proc_exit::exit_now(0);
                }
                _ => wire::error(&w, k, id, -32601, "not in scan mode"),
            }
        }
    }
}

#[cfg(test)]
mod default_step_tests {
    /// The default step deadline is inside the host application's whole-scan budget, with at least 3 s to spare.
    #[test]
    fn the_default_step_deadline_fits_the_cores_budget() {
        let budget = summoner_scan::scan::BUDGET.as_millis() as u64;
        assert_eq!(super::DEFAULT_STEP_MS, 25_000);
        assert!(super::DEFAULT_STEP_MS + 3_000 <= budget, "{budget}");
    }
}
