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
    // The handover's take bound at this geometry: a take is forced only when no block began on the chain thread for
    // that long after the drain was announced. The longest span this driver saw from one block's send to the NEXT
    // block's completion, up to the slot.reactivated, is the evidence for that (a block's begin lies between its
    // send and its completion, so a window of the bound with no begin needs such a span at least as long).
    let bound = summoner_plugin::handover::take_bound(B as u32, SR);
    let mut prev_send: Option<Instant> = None;
    let mut longest_span = Duration::ZERO;
    while changed.is_none_or(|c| k < c + 200) {
        let sent = Instant::now();
        y.extend(eng.block());
        x.extend(reference.block());
        settle();
        if let (None, Some(p)) = (reactivated, prev_send) {
            longest_span = longest_span.max(p.elapsed());
        }
        prev_send = Some(sent);
        // The pace stays a fixed sleep after each block, the cadence the shape below was made at: a deadline pace at
        // real time was measured to put one block at delay 4096 inside the transition, in about 1 run in 10.
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

    let (latency, bypassed_blocks, forced, abandoned) = reactivated.expect("slot.reactivated");
    assert_eq!(
        latency, 4096,
        "slot.reactivated: the latency after the cycle"
    );
    assert!(
        !abandoned,
        "the handover is never abandoned: {reactivated:?}"
    );
    // The clean handover is the normal case: no forced take, and the slot passed its bypass signal. A forced take
    // (the handover's deadline path: no block began on the chain thread within the take bound, ~5.3 ms at B = 64)
    // is legitimate only when this driver itself left such a gap, as a loaded machine's scheduler can; it is
    // accepted then and only then, and a forced take has no bypassed block. Without the gap a forced take is a
    // fault and fails here. Either way the latency and the event order (above) and the output's shape (below) hold.
    let deadline_path = longest_span >= bound;
    println!(
        "handover: forced {forced}, longest driver span {longest_span:?} (take bound {bound:?}), bypassed {bypassed_blocks}"
    );
    if forced {
        assert!(
            deadline_path,
            "a forced take with no driver gap past the take bound: {reactivated:?} (longest span {longest_span:?}, bound {bound:?})"
        );
    } else {
        assert!(
            bypassed_blocks > 0,
            "slot.reactivated: the slot passed its bypass signal (ST_BYPASSED): {reactivated:?} (latency, bypassed, forced, abandoned)"
        );
    }

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

/// The plugin's own latency change through the re-activation handover, on a sine (where a jump in the bypass
/// signal's delay is a step in the output, unlike on noise): every sample-to-sample step of the transition, from the
/// drain to the plugin's own fresh delay line starting at delay 4096, stays within the handover's bound
/// `max|Δb| + (π / (2F))·max|y − b|` (`summoner_plugin::handover`). The bypass signal keeps the latency the processor
/// ran with for the whole handover (drain, out and return): before, the chain thread read the slot's latency every
/// block, so the cycle's new latency (stored before the give-back) moved the bypass signal by 4096 frames at the
/// return fade's first frame, where its gain is 1: a step of nearly the sine's whole swing, every cycle.
///
/// Mutation: the bypass signal read at the slot's current latency again: the step is past the bound.
#[test]
fn a_latency_change_handover_has_no_step_past_the_fade_bound() {
    if lab_plugins::absent("a_latency_change_handover_has_no_step_past_the_fade_bound") {
        return;
    }
    handover_steps(&[], false);
}

/// The test above on a slot disabled (`--test-slot-enabled 0:off@10`, the fader's fade over long before the cycle):
/// the slot's output is its bypass signal before and after the cycle, at delay 0 and then 4096, and the handover in
/// between must stay within the same bound, up to and including the first frame at delay 4096 (here the bypass
/// signal's, not a plugin's fresh delay line). Before the fix the return faded to the plugin's fresh (silent) output
/// and the fader's bypass then cut in at once: a step of up to the signal's whole swing.
#[test]
fn a_latency_change_handover_on_a_disabled_slot_has_no_step_past_the_fade_bound() {
    if lab_plugins::absent(
        "a_latency_change_handover_on_a_disabled_slot_has_no_step_past_the_fade_bound",
    ) {
        return;
    }
    handover_steps(&["--test-slot-enabled", "0:off@10"], true);
}

/// One run of the two tests above with `args` for the child; asserts the bound. `disabled`: the first frame at delay
/// 4096 is the bypass signal's, so its step is the handover's too.
fn handover_steps(args: &[&str], disabled: bool) {
    use crate::common::drive::{Bridged, Drive, Input};
    const B: usize = 64;
    const A: f32 = 0.5;
    // 100 Hz: 4096 frames are 8.53 periods, so the two delays differ by about half a period.
    let w = 2.0 * std::f64::consts::PI * 100.0 / SR;
    let frames = 48_000 * 3;
    let sine: std::sync::Arc<[f32]> = (0..frames)
        .map(|t| A * (w * t as f64).sin() as f32)
        .collect();
    let bundle = lab_plugins::dir().join("vst3/summoner-lab-hostile.vst3");
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    cfg.args = args.iter().map(Into::into).collect();
    let (bridge, rx) = start(cfg);
    let mut seen = Vec::new();
    let xml = vst3_ref(&bundle, &hostile_cid(), "Summoner Lab Hostile");
    let key = 12;
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
    let mut eng = Drive::new(
        Input::Once(sine.clone()),
        vec![Bridged {
            params: vec![1.0],
            ..Bridged::fx(&link)
        }],
        B,
    );
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
    let mut y = Vec::new();
    let mut reactivated: Option<(u32, bool)> = None;
    let mut done: Option<usize> = None;
    let mut k = 0;
    while done.is_none_or(|d| k < d + 200) {
        y.extend(eng.block());
        settle();
        std::thread::sleep(Duration::from_micros(1333));
        while let Ok(e) = rx.try_recv() {
            if let Event::SlotReactivated {
                key: kk,
                slot: 0,
                latency,
                forced_take,
                ..
            } = e
                && kk == key
            {
                reactivated = Some((latency, forced_take));
                done = Some(k);
            }
        }
        k += 1;
        assert!(k < frames / B - 1, "no slot.reactivated in 3 s");
    }
    drop(bridge);
    let (latency, forced) = reactivated.expect("slot.reactivated");
    assert_eq!(latency, 4096);
    let x = |t: usize, d: usize| if t >= d { sine[t - d] } else { 0.0 };
    // The pipeline delay P on the early blocks (delay 0 at the plugin).
    let p = (0..4 * B)
        .find(|p| (4 * B..8 * B).all(|t| y[t] == x(t, *p)))
        .expect("the early blocks are the input at a fixed pipeline delay");
    // The transition: from the first sample that is not delay 0 to the plugin's own delay line, delay 4096 from there
    // to the end.
    // (The fader's fade between two equal signals rounds: a deviation below 1e-6 is no transition.)
    let first = (0..y.len())
        .find(|&t| (y[t] - x(t, p)).abs() > 1e-6)
        .expect("the output leaves delay 0");
    let onset = (first..y.len())
        .find(|&t| (t..y.len()).all(|u| y[u] == x(u, p + 4096)))
        .expect("the output ends at delay 4096");
    // The steps inside it (the onset's own step, silence to the plugin's fresh output, is the plugin's, not the
    // handover's), and the bound: the bypass signal's own largest step plus the crossfade's term on the largest
    // |y − b| (y and b each within ±A).
    let end = if disabled { onset + 1 } else { onset };
    let (at, step) = (first.max(1)..end)
        .map(|t| (t, (y[t] - y[t - 1]).abs()))
        .fold((0, 0.0f32), |m, s| if s.1 > m.1 { s } else { m });
    let db = A * w as f32;
    let bound =
        db + std::f32::consts::PI / (2.0 * summoner_plugin::handover::FADE as f32) * (2.0 * A);
    println!(
        "handover steps: largest {step} at frame {at} ({} after the transition's start, onset at {}), bound {bound}, \
         forced {forced}",
        at - first,
        onset - first
    );
    assert!(
        step <= bound,
        "a step of {step} at frame {at} inside the handover (bound {bound}): the bypass signal jumped"
    );
}

/// Two latency changes back to back: the plugin's latency goes 0 -> 4096 (the first handover) and, while that handover's
/// processor is out (`--test-cycle-hold-ms 150`: a known window), 4096 -> 2048; the message thread re-announces at the
/// first handover's end, and `--test-hold-after-return` makes the chain's next block see that announce, so the second
/// handover starts with no plugin-only block between the two. The second handover's bypass signal must play at the
/// latency the first one left (4096, what the host application's compensation already is), never at the first
/// handover's stale 0: every block is the input at delay 0 until the first handover leaves it, then never again; the
/// second handover's time out is at delay 4096; the plugin's own delay line ends it at 2048.
///
/// Mutation: the latch not cleared at the first handover's end: the second handover's time out plays at delay 0.
#[test]
fn back_to_back_latency_handovers_each_play_their_own_starting_latency() {
    if lab_plugins::absent("back_to_back_latency_handovers_each_play_their_own_starting_latency") {
        return;
    }
    use crate::common::drive::{Bridged, Drive, Input};
    const B: usize = 64;
    let w = 2.0 * std::f64::consts::PI * 100.0 / SR;
    let frames = 48_000 * 3;
    let sine: std::sync::Arc<[f32]> = (0..frames)
        .map(|t| 0.5 * (w * t as f64).sin() as f32)
        .collect();
    let bundle = lab_plugins::dir().join("vst3/summoner-lab-hostile.vst3");
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    cfg.args = vec![
        "--test-cycle-hold-ms".into(),
        "150".into(),
        "--test-hold-after-return".into(),
    ];
    let (bridge, rx) = start(cfg);
    let mut seen = Vec::new();
    let xml = vst3_ref(&bundle, &hostile_cid(), "Summoner Lab Hostile");
    let key = 13;
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
    let mut eng = Drive::new(
        Input::Once(sine.clone()),
        vec![Bridged {
            params: vec![1.0],
            ..Bridged::fx(&link)
        }],
        B,
    );
    // 0 -> 4096 at 150 ms (its restart at 170 ms, the cycle then, held out until about 320 ms); 4096 -> 2048 at
    // 250 ms (its restart at 270 ms, inside that hold).
    for (generation, text) in [(1, &b"LATER=150:4096"[..]), (2, &b"LATER=250:2048"[..])] {
        bridge
            .send(Command::SetState {
                key,
                state: vst3_state(0, generation, text),
            })
            .map_err(|_| "busy")
            .unwrap();
    }
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
    let mut y = Vec::new();
    let mut reactivated: Vec<u32> = Vec::new();
    let mut done: Option<usize> = None;
    let mut k = 0;
    while done.is_none_or(|d| k < d + 200) {
        y.extend(eng.block());
        settle();
        std::thread::sleep(Duration::from_micros(1333));
        while let Ok(e) = rx.try_recv() {
            if let Event::SlotReactivated {
                key: kk,
                slot: 0,
                latency,
                ..
            } = e
                && kk == key
            {
                reactivated.push(latency);
                if reactivated.len() == 2 {
                    done = Some(k);
                }
            }
        }
        k += 1;
        assert!(
            k < frames / B - 1,
            "two slot.reactivated not seen in 3 s: {reactivated:?}"
        );
    }
    drop(bridge);
    assert_eq!(reactivated, [4096, 2048], "the two cycles' latencies");
    let x = |t: usize, d: usize| if t >= d { sine[t - d] } else { 0.0 };
    let p = (0..4 * B)
        .find(|p| (4 * B..8 * B).all(|t| y[t] == x(t, *p)))
        .expect("the early blocks are the input at a fixed pipeline delay");
    // Each block: 'a' delay 0, 'b' delay 4096, 'c' delay 2048, '-' anything else (within 1e-6: a crossfade between
    // two equal signals rounds).
    let class: String = (8..y.len() / B)
        .map(|blk| {
            let ts = blk * B..(blk + 1) * B;
            let at = |d: usize| ts.clone().all(|t| (y[t] - x(t, p + d)).abs() <= 1e-6);
            if at(0) {
                'a'
            } else if at(4096) {
                'b'
            } else if at(2048) {
                'c'
            } else {
                '-'
            }
        })
        .collect();
    println!("back to back: {class}");
    // The second handover's time out (its pure bypass blocks, after the first handover's return) at delay 4096, and
    // nothing at delay 0 once it began.
    assert!(
        class.matches('b').count() >= 20,
        "the second handover played its bypass at the first one's stale latency, not at 4096: {class}"
    );
    let first_b = class.find('b').unwrap_or(class.len());
    assert!(
        !class[first_b..].contains('a'),
        "delay 0 again after the second handover began: {class}"
    );
    let tail = class.len() - class.trim_end_matches('c').len();
    assert!(tail >= 100, "the plugin at delay 2048 to the end: {class}");
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
