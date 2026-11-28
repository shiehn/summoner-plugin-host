//! The child's own behaviour, beyond what the host application's bridge tests cover: the orphan watch,
//! `latency.changed`, the crash log. The child runs under the summoner supervisor (`summoner::supervisor`, the
//! pinned crate) where a supervisor is the right parent; the orphan test needs a parent it can kill, so it re-runs this
//! test binary as that parent.
//!
//! Every process a test starts is ended by its recorded pid (never by name).
#![forbid(unsafe_code)]

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Config, Event};
use summoner_proto::layout::Geometry;
use summoner_sys::exit_watch::ExitWatch;

mod common;
use common::*;

/// One block for the orphan watch's bound: 512 frames at 48 kHz (10.7 ms).
const BLOCK: Duration = Duration::from_micros(512 * 1_000_000 / 48_000);

/// The parent role of the orphan test: a no-op in a normal run. Re-run by the test with `HC_ORPHAN_PARENT_OUT` set
/// and the socket's child end on fd 3, it spawns the child on that end, writes the child's pid, and waits to be
/// killed.
#[test]
fn orphan_parent_role() {
    let Some(out) = std::env::var_os("HC_ORPHAN_PARENT_OUT") else {
        return;
    };
    let fd = runecraft_ipc::process::unix::take_fd3().expect("fd 3");
    let env: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| k != "HC_ORPHAN_PARENT_OUT")
        .collect();
    let pid =
        runecraft_ipc::process::unix::spawn_stdio_only(&child_bin(), &[], &env, fd.as_fd(), None)
            .expect("spawn the child");
    std::fs::write(out, pid.to_string()).unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// The orphan watch, measured: a parent killed with SIGKILL, then how long the child outlived it (None: more than
/// 2 s). The child must go by the kernel's exit note, not by the control socket's EOF: the test holds the socket's
/// other end open throughout, so the child never sees EOF.
fn child_outlives_a_killed_parent_by() -> Option<Duration> {
    let dir = scratch("orphan");
    let out = dir.join("child.pid");
    let (core_end, child_end) = UnixStream::pair().unwrap();
    let mut env: Vec<_> = std::env::vars_os().collect();
    env.push(("HC_ORPHAN_PARENT_OUT".into(), out.clone().into()));
    let args = [
        "--exact",
        "orphan_parent_role",
        "--nocapture",
        "--test-threads=1",
    ]
    .map(Into::into);
    let me = std::env::current_exe().unwrap();
    let parent =
        runecraft_ipc::process::unix::spawn_stdio_only(&me, &args, &env, child_end.as_fd(), None)
            .unwrap();
    drop(child_end);
    let until = Instant::now() + Duration::from_secs(10);
    let child: u32 = loop {
        if let Some(p) = std::fs::read_to_string(&out)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break p;
        }
        assert!(Instant::now() < until, "the parent never spawned the child");
        std::thread::sleep(Duration::from_millis(5));
    };
    let cw = ExitWatch::new(child).unwrap();
    let pw = ExitWatch::new(parent).unwrap();
    assert!(
        !cw.wait(Some(Duration::from_millis(300))).unwrap(),
        "the child is up"
    );
    assert!(common::sigkill(parent), "SIGKILL the parent (recorded pid)");
    assert!(pw.wait(Some(Duration::from_secs(5))).unwrap());
    let t0 = Instant::now();
    let gone = cw.wait(Some(Duration::from_secs(2))).unwrap();
    let dt = t0.elapsed();
    let _ = runecraft_ipc::process::unix::wait_child(parent);
    if !gone {
        common::sigkill(child); // recorded pid
    }
    drop(core_end);
    let _ = std::fs::remove_dir_all(&dir);
    gone.then_some(dt)
}

/// The orphan watch's correctness half (the default run): the child ends after its parent's SIGKILL, within a hang
/// bound of 1 s. Mutation "no watch at all" fails it; a poll instead of the exit note (250 ms) passes it, and fails
/// the timing half below.
#[test]
fn a_killed_parent_ends_the_child() {
    let dt = child_outlives_a_killed_parent_by();
    println!("orphan watch: the child ended {dt:?} after its parent (hang bound 1 s)");
    let dt = dt.expect("the child outlived its parent by 2 s");
    assert!(
        dt < Duration::from_secs(1),
        "the child ended {dt:?} after its parent"
    );
}

/// The orphan watch's timing half (out of the default run): the child ends within one block of its
/// parent's SIGKILL. A wall-clock bound, so it is judged only on an idle machine, with `--ignored` (load at
/// most 4).
#[test]
#[ignore = "timing: run on an idle machine with --ignored"]
fn a_killed_parent_ends_the_child_within_one_block() {
    let dt = child_outlives_a_killed_parent_by();
    println!("orphan watch: the child ended {dt:?} after its parent (bound {BLOCK:?})");
    let dt = dt.expect("the child outlived its parent by 2 s");
    assert!(
        dt < BLOCK,
        "the child ended {dt:?} after its parent, more than one block"
    );
}

/// `latency.changed`: a hosted slot whose reported latency moves at run time changes the chain's PDC; the host
/// application hears the new sum before the state's receipt, and a state that leaves it alone sends nothing.
#[test]
fn a_slot_latency_change_reaches_the_core_as_the_chains_new_sum() {
    let (bridge, rx) = start(Config::new(child_bin(), SR, crate::common::state_hasher()));
    let mut seen = Vec::new();
    let key = 7;
    bridge
        .send(Command::Load {
            key,
            spec: spec(&[
                r#"{"format":"fake","fake":{"name":"lat","delay":"3","latency":"5"}}"#,
                r#"{"format":"fake","fake":{"name":"b","delay":"2"}}"#,
            ]),
            states: vec![],
        })
        .map_err(|_| "busy")
        .unwrap();
    let Event::ChainReady { link, .. } = wait_for(
        &rx,
        &mut seen,
        |e| matches!(e, Event::ChainReady { key: k, .. } if *k == key),
    ) else {
        unreachable!()
    };
    assert_eq!(link.plugin_latency, 7, "chain.loaded: 5 (reported) + 2");

    let set = |generation, text: &str| {
        bridge
            .send(Command::SetState {
                key,
                state: state(0, generation, text),
            })
            .map_err(|_| "busy")
            .unwrap();
    };
    set(1, "latency=9");
    let Event::LatencyChanged { link, .. } = wait_for(
        &rx,
        &mut seen,
        |e| matches!(e, Event::LatencyChanged { key: k, .. } if *k == key),
    ) else {
        unreachable!()
    };
    assert_eq!(link.plugin_latency, 11, "latency.changed: 9 + 2");
    wait_for(&rx, &mut seen, |e| {
        matches!(e, Event::StateApplied { generation: 1, .. })
    });

    seen.clear();
    set(2, "gain=0.5");
    wait_for(&rx, &mut seen, |e| {
        matches!(e, Event::StateApplied { generation: 2, .. })
    });
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::LatencyChanged { .. })),
        "a state that keeps the latency sends no latency.changed: {seen:?}"
    );
    drop(bridge);
}

/// The crash log: a SIGSEGV inside a plugin's set_state is logged under the crash log's file name, with its lines,
/// and the breadcrumb names the op and the plugin. The child's HOME is a scratch directory, so the log lands there.
#[test]
fn a_segv_in_a_plugin_is_logged_with_the_op_in_flight() {
    let home = scratch("crash-home");
    let logs = home.join("Library/Logs");
    std::fs::create_dir_all(&logs).unwrap();
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    let (bridge, rx) = start(cfg);
    let pid = bridge.counters().child_pid();
    let mut seen = Vec::new();
    bridge
        .send(Command::Load {
            key: 9,
            spec: spec(&[r#"{"format":"fake","fake":{"name":"segv"}}"#]),
            states: vec![state(0, 1, "SEGV")],
        })
        .map_err(|_| "busy")
        .unwrap();
    wait_for(&rx, &mut seen, |e| {
        matches!(e, Event::ChainDown { key: 9, .. })
    });
    drop(bridge); // before its backoff respawns the child into the same state
    let log = logs.join(summoner_proto::host_names::crash_log_name(pid));
    let text = std::fs::read_to_string(&log).unwrap_or_else(|e| panic!("{}: {e}", log.display()));
    let _ = std::fs::remove_dir_all(&home);
    println!("{text}");
    assert!(
        text.contains(&format!(
            "\n{}\nSignal: SIGSEGV (11)\n",
            summoner_proto::host_names::CRASH_BANNER
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("\nPID: {pid}\nBacktrace:\n")),
        "{text}"
    );
    assert!(text.ends_with("plugin-op: setState segv\n"), "{text}");
}

/// `latency.changed` from the plugin itself, and the re-activation that makes it true: the hostile VST3's
/// `LATER=<ms>:<frames>` changes its reported latency later, on a thread of its own, and its controller raises
/// `restartComponent (kLatencyChanged)`; the hostile DSP applies a latency (as a delay) only when re-activated. So:
/// - the state's receipt carries no change; the poll sends `latency.changed` with the new sum (4096);
/// - the output proves the re-activation, through the re-activation handover (protocol 3.1,
///   `summoner_plugin::handover`): every block is the input at delay 0, then one bounded transition (the drain fade,
///   the bypass signal while the processor is out, the return fade, then the plugin's own fresh 4096-frame delay line:
///   4096 / 64 blocks plus at most 16), then the input at delay 4096, and nothing else (it never goes back);
/// - `slot.reactivated` (latency 4096, bypassed blocks > 0, no forced take) comes before `latency.changed`;
/// - the slot's counters show one re-activation (`--counters`); the child runs under the file-access recorder.
///
/// Mutation: without the re-activation, no block is ever at delay 4096, and the timing assertion fails.
#[test]
fn a_latency_change_the_plugin_raises_on_its_own_is_applied_and_reaches_the_core() {
    if lab_plugins::absent(
        "a_latency_change_the_plugin_raises_on_its_own_is_applied_and_reaches_the_core",
    ) {
        return;
    }
    // With the file-access recorder and without it: the interposer's cost must not change the
    // shape. Each run asserts the bounded transition itself; the lengths are printed (a cycle may fall a block apart).
    let recorded = latency_later_run(true);
    let bare = latency_later_run(false);
    println!("transition: {recorded} blocks recorded, {bare} bare");
}

/// One run of the test above; returns the transition's length in blocks (after its own assertions).
fn latency_later_run(recorded: bool) -> usize {
    use crate::common::drive::{Bridged, Drive};
    const B: usize = 64;
    let bundle = lab_plugins::dir().join("vst3/summoner-lab-hostile.vst3");
    let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
    assert!(
        bundle.is_dir() && fswatch.is_file(),
        "tools/build-plugins.sh"
    );
    let dir = scratch("latency-later");
    let log = dir.join("fswatch.log");
    let counters = dir.join("counters.txt");
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    cfg.args = vec!["--counters".into(), counters.clone().into()];
    if recorded {
        cfg.env
            .push(("DYLD_INSERT_LIBRARIES".into(), fswatch.clone().into()));
        cfg.env
            .push(("SUMMONER_FSWATCH_LOG".into(), log.clone().into()));
    }
    let (bridge, rx) = start(cfg);
    let mut seen = Vec::new();
    let xml = vst3_ref(&bundle, &hostile_cid(), "Summoner Lab Hostile");
    let key = 11;
    let mut sp = spec(&[&xml]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    bridge
        .send(Command::Load {
            key,
            spec: sp,
            states: vec![],
        })
        .map_err(|_| "busy")
        .unwrap();
    let Event::ChainReady { link, .. } = wait_for(
        &rx,
        &mut seen,
        |e| matches!(e, Event::ChainReady { key: k, .. } if *k == key),
    ) else {
        unreachable!()
    };
    assert_eq!(link.plugin_latency, 0);

    // The engine (noise → the bridged slot → out) and a reference without the bridge (noise → out).
    let frames = 48_000 * 3;
    let mut eng = Drive::new(
        noise(frames),
        vec![Bridged {
            params: vec![1.0],
            ..Bridged::fx(&link)
        }],
        B,
    );
    let mut reference = Drive::dry(noise(frames), B);

    bridge
        .send(Command::SetState {
            key,
            state: vst3_state(0, 1, b"LATER=150:4096"),
        })
        .map_err(|_| "busy")
        .unwrap();
    let settle = || {
        let until = Instant::now() + Duration::from_secs(5);
        let reg = &link.region;
        while reg.ack().load(std::sync::atomic::Ordering::Acquire)
            < reg.seq().load(std::sync::atomic::Ordering::Acquire)
        {
            assert!(Instant::now() < until, "the chain never acked");
            std::thread::yield_now();
        }
    };
    // Blocks at about real time, until the latency change has arrived and 200 blocks more have run.
    let (mut y, mut x) = (Vec::new(), Vec::new());
    let mut changed: Option<usize> = None;
    let mut applied = false;
    // the slot.reactivated that must come before the latency.changed.
    let mut reactivated: Option<(u32, u32, bool, bool)> = None;
    let mut k = 0;
    while changed.is_none_or(|c| k < c + 200) {
        y.extend(eng.block());
        x.extend(reference.block());
        settle();
        std::thread::sleep(Duration::from_micros(1333));
        while let Ok(e) = rx.try_recv() {
            match e {
                Event::StateApplied { generation: 1, .. } => applied = true,
                Event::SlotReactivated {
                    key: kk,
                    slot: 0,
                    latency,
                    bypassed_blocks,
                    forced_take,
                    abandoned,
                    ..
                } if kk == key => {
                    reactivated = Some((latency, bypassed_blocks, forced_take, abandoned));
                }
                Event::LatencyChanged { key: kk, link: l } if kk == key => {
                    assert!(
                        applied,
                        "the change came after the state's receipt, not with it"
                    );
                    assert!(
                        reactivated.is_some(),
                        "slot.reactivated comes before latency.changed"
                    );
                    assert_eq!(
                        l.plugin_latency, 4096,
                        "the plugin's own change, through the poll"
                    );
                    changed = Some(k);
                }
                _ => {}
            }
        }
        k += 1;
        assert!(k < 48_000 * 3 / B - 1, "no latency.changed in 3 s");
    }
    drop(bridge);

    // The pipeline delay P: the bridge's depth, found on the first blocks (delay 0 at the plugin).
    let p = (0..4 * B)
        .find(|p| (4 * B..8 * B).all(|t| y[t] == x[t - p]))
        .expect("the early blocks are the input at a fixed pipeline delay");
    // Each block: 'a' at delay 0, 'b' at delay 4096, '-' anything else (the reactivation handover: the drain fade, the
    // bypass signal while the processor is out, the return fade, then the plugin's own fresh 4096-frame delay line).
    let class: String = (8..y.len() / B)
        .map(|blk| {
            let ts = blk * B..(blk + 1) * B;
            if ts.clone().all(|t| y[t] == x[t - p]) {
                'a'
            } else if ts.clone().all(|t| t >= p + 4096 && y[t] == x[t - p - 4096]) {
                'b'
            } else {
                '-'
            }
        })
        .collect();
    // The handover's shape: delay 0, one bounded transition, then delay 4096 to the end and nothing else. Inside the
    // transition the bypass signal at the old latency (0) IS delay 0, so 'a' blocks may appear there; a 'b' may not.
    let head = class.len() - class.trim_start_matches('a').len();
    let tail = class.len() - class.trim_end_matches('b').len();
    let middle = &class[head..class.len() - tail];
    let transition = middle.len();
    assert!(
        head > 0 && tail >= 100 && !middle.contains('b'),
        "the output must be delay 0, a transition, then delay 4096 and nothing else: {class}"
    );
    // The transition: the plugin's own fresh delay (4096 / B blocks) plus the fades and the cycle, a few blocks.
    assert!(
        (4096 / B..=4096 / B + 16).contains(&transition),
        "a transition of {transition} blocks: {class}"
    );
    let (latency, bypassed_blocks, forced, abandoned) = reactivated.expect("slot.reactivated");
    assert_eq!(
        latency, 4096,
        "slot.reactivated: the latency after the cycle"
    );
    // The whole event in each message: a forced take (blocks spaced past the handover's take bound, ~5.3 ms at B = 64,
    // as a loaded machine's sleep can space them) has no bypassed block, so both lines name what happened.
    assert!(
        bypassed_blocks > 0,
        "slot.reactivated: the slot passed its bypass signal (ST_BYPASSED): {reactivated:?} (latency, bypassed, forced, abandoned)"
    );
    assert!(
        !forced && !abandoned,
        "a clean handover: no forced take, not abandoned: {reactivated:?}"
    );

    let until = Instant::now() + Duration::from_secs(5);
    let text = loop {
        match std::fs::read_to_string(&counters) {
            Ok(t) if !t.is_empty() => break t,
            _ if Instant::now() > until => panic!("no counters"),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    println!("counters: {text}");
    assert!(
        text.contains(&format!("chain {key} slot 0 reactivations 1 ")),
        "{text}"
    );
    let watched = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    if recorded {
        assert!(
            watched.contains("summoner-lab-hostile.vst3"),
            "the recorder saw the load"
        );
        for bad in [
            "/Library/Audio/Plug-Ins",
            "/System/Library/Components",
            "Library/Audio/Plug-Ins",
        ] {
            assert!(
                !watched.lines().any(|l| l.contains(bad)),
                "{bad} touched:\n{watched}"
            );
        }
    }
    transition
}

/// The re-activation cycle, through the test double's `REACTIVATE[=<ms>]` (a plugin
/// asking to be re-activated, as a VST3 `kLatencyChanged` does; no VST3 is loaded): the message thread takes the slot's
/// processor through the re-activation handover (`summoner_plugin::handover`), re-activates it, gives it back, and
/// counts it; a cycle longer than `handover::cycle_budget` (4 block budgets, here 4 × 64 frames at 48 kHz = 5.3 ms) is
/// a stall. The counters come from `--counters` at shutdown.
#[test]
fn a_reactivation_is_run_counted_and_a_slow_one_is_a_stall() {
    for (state_text, want_stalls) in [("REACTIVATE", 0), ("REACTIVATE=100", 1)] {
        let dir = scratch("reactivate");
        let counters = dir.join("counters.txt");
        let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
        cfg.args = vec!["--counters".into(), counters.clone().into()];
        let (bridge, rx) = start(cfg);
        let mut seen = Vec::new();
        bridge
            .send(Command::Load {
                key: 5,
                spec: spec(&[r#"{"format":"fake","fake":{"name":"r","gain":"1"}}"#]),
                states: vec![],
            })
            .map_err(|_| "busy")
            .unwrap();
        wait_for(&rx, &mut seen, |e| {
            matches!(e, Event::ChainReady { key: 5, .. })
        });
        bridge
            .send(Command::SetState {
                key: 5,
                state: state(0, 1, state_text),
            })
            .map_err(|_| "busy")
            .unwrap();
        wait_for(&rx, &mut seen, |e| {
            matches!(e, Event::StateApplied { generation: 1, .. })
        });
        std::thread::sleep(Duration::from_millis(400)); // the 50 ms poll, then the cycle
        drop(bridge); // shutdown: the child writes its counters
        let until = Instant::now() + Duration::from_secs(5);
        let text = loop {
            match std::fs::read_to_string(&counters) {
                Ok(t) if !t.is_empty() => break t,
                _ if Instant::now() > until => panic!("no counters at {}", counters.display()),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            text.contains(&format!(
                "chain 5 slot 0 reactivations 1 bypassed 0 stalls {want_stalls}"
            )),
            "{state_text}: {text}"
        );
    }
}
