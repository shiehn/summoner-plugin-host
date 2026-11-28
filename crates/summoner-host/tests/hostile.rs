//! The lab's hostile VST3 through the summoner supervisor (after the gate): every misbehaviour the protocol says
//! the host must survive, end to end, from a real VST3 module loaded by the child. Each child runs under the
//! file-access recorder and its supervisor on a manual clock (the respawn backoff and the
//! wedge detector advance deterministically, as the host application's own recovery tests do).
//!
//! The hostile bundle's parameters (testkit/plugins/dsp `hostile::Param`): 0 gain, 1 abort, 2 segv, 3 throw, 4 wedge, 5
//! burn, 6 NaN, 7 inf, 8 subnormal, 9 huge, 10 latency. A chain's region parameter `slot << 24 | index` is driven from
//! the graph's Bridge node parameter (the spec's `param_ids`).
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, DownReason, Event};
use summoner_proto::layout::Geometry;
use summoner_proto::layout::{R_STATUS, ST_THREW};

const B: usize = 64;
const KEY: u64 = 21;

/// A one-slot hostile chain of variant `variant`, its region parameters `params` (slot 0's indices).
fn load(rig: &mut Rig, variant: u32, params: &[u32]) -> Option<Arc<summoner_rt::rt::ChainLink>> {
    let xml = hostile_xml(variant);
    let mut sp = spec(&[&xml]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    sp.param_ids = Arc::from(params.to_vec());
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) {
        Event::ChainReady { link, .. } => Some(link),
        _ => None,
    }
}

/// noise → the bridged chain (its node parameters `values`, the spec's `param_ids` in order) → out.
fn engine(link: &Arc<summoner_rt::rt::ChainLink>, values: Vec<f64>) -> Drive {
    Drive::new(
        noise(48_000),
        vec![Bridged {
            params: values,
            ..Bridged::fx(link)
        }],
        B,
    )
}

/// Wait (in the test) until the chain has acked everything published, or `d` passes.
fn settle(link: &Arc<summoner_rt::rt::ChainLink>, d: Duration) -> bool {
    let until = Instant::now() + d;
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        if Instant::now() > until {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

fn set(eng: &mut Drive, param: u16, value: f64) {
    eng.set_param(0, usize::from(param), value);
}

/// The shim: a C++ exception from the plugin's process is caught at the boundary; the block is THREW and
/// silent, the child lives, and the chain comes back when the plugin stops throwing.
#[test]
fn a_throw_in_process_is_a_silent_threw_block_and_the_chain_lives() {
    if lab_plugins::absent("a_throw_in_process_is_a_silent_threw_block_and_the_chain_lives") {
        return;
    }
    let mut rig = Rig::start("hostile-throw");
    let pid = rig.bridge.counters().child_pid();
    let link = load(&mut rig, HOSTILE, &[3]).expect("loads");
    let mut eng = engine(&link, vec![1.0]); // throw from the first block
    let mut out = Vec::new();
    for _ in 0..20 {
        out.extend(eng.block());
        assert!(
            settle(&link, Duration::from_secs(5)),
            "the chain keeps acking"
        );
    }
    let seq = link.region.seq().load(Ordering::Acquire);
    let status = link.region.slot(seq).u32(R_STATUS).load(Ordering::Relaxed);
    assert_ne!(
        status & ST_THREW,
        0,
        "the response line says THREW: {status:#x}"
    );
    assert!(
        out[4 * B..].iter().all(|x| *x == 0.0),
        "a THREW block is silent"
    );
    set(&mut eng, 0, 0.0);
    let mut after = Vec::new();
    for _ in 0..20 {
        after.extend(eng.block());
        assert!(settle(&link, Duration::from_secs(5)));
    }
    assert!(
        after[8 * B..].iter().any(|x| *x != 0.0),
        "it plays again once the plugin stops throwing"
    );
    assert_eq!(rig.bridge.counters().child_pid(), pid, "the same child");
    assert!(
        !rig.seen
            .iter()
            .any(|e| matches!(e, Event::ChainDown { .. })),
        "{:?}",
        rig.seen
    );
    rig.recorder_clean();
}

/// Run blocks until the chain stops acking (the child died or the chain wedged).
fn run_until_stuck(eng: &mut Drive, link: &Arc<summoner_rt::rt::ChainLink>) {
    for _ in 0..200 {
        eng.block();
        if !settle(link, Duration::from_millis(500)) {
            return;
        }
    }
    panic!("the chain never stopped");
}

/// a crash inside the plugin's process (abort) downs the chain, is attributed to the slot as op `process`
/// (the region's in_slot), and a second death of the same slot after the respawn quarantines it.
#[test]
fn an_abort_in_process_is_attributed_and_quarantined_at_the_second_death() {
    if lab_plugins::absent("an_abort_in_process_is_attributed_and_quarantined_at_the_second_death")
    {
        return;
    }
    // The one real-SIGABRT case: two .ips per run, on purpose; every other death is a SIGKILL.
    let mut rig = Rig::start_with_real_deaths("hostile-abort");
    let link = load(&mut rig, HOSTILE, &[1]).expect("loads");
    let mut eng = engine(&link, vec![1.0]);
    run_until_stuck(&mut eng, &link);
    rig.wait(|e| {
        matches!(
            e,
            Event::ChainDown {
                key: KEY,
                reason: DownReason::Crash
            }
        )
    });
    rig.recorder_clean();
    // The respawn (2 s) reloads the chain; its parameters are sent again, so it dies again in process.
    rig.advance_to_respawn(1);
    let Event::ChainReady { link, .. } =
        rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    let mut eng = engine(&link, vec![1.0]);
    run_until_stuck(&mut eng, &link);
    match rig.wait(|e| matches!(e, Event::Quarantined { .. })) {
        Event::Quarantined {
            key,
            slot,
            plugin,
            op,
        } => {
            assert_eq!(
                (key, slot, plugin.as_str(), op.as_str()),
                (KEY, 0, "Summoner Lab Hostile", "process")
            );
        }
        e => panic!("{e:?}"),
    }
}

/// A bad-pointer write inside process downs the chain (a crash). It
/// dies quietly (SIGKILL); the handler's log is child.rs's one real-SIGSEGV test.
#[test]
fn a_segv_in_process_downs_the_chain_as_a_quiet_death() {
    if lab_plugins::absent("a_segv_in_process_downs_the_chain_as_a_quiet_death") {
        return;
    }
    // Under SUMMONER_LAB_DEATH=kill (the Rig's default) the hostile core's null write is a SIGKILL: no crash report in
    // the user's DiagnosticReports. The SIGSEGV handler's log and breadcrumb are child.rs's one real-SIGSEGV test.
    let mut rig = Rig::start("hostile-segv");
    let link = load(&mut rig, HOSTILE, &[2]).expect("loads");
    let mut eng = engine(&link, vec![1.0]);
    run_until_stuck(&mut eng, &link);
    rig.wait(|e| {
        matches!(
            e,
            Event::ChainDown {
                key: KEY,
                reason: DownReason::Crash
            }
        )
    });
    match rig.wait(|e| matches!(e, Event::ChildExited { .. })) {
        Event::ChildExited { status, .. } => {
            assert!(
                status.contains("Signaled(9)"),
                "a quiet death (SIGKILL), no .ips: {status}"
            )
        }
        e => panic!("{e:?}"),
    }
    rig.recorder_clean();
}

/// A plugin that never returns from process wedges its chain; the detector (2 s suspect, 2 s verdict, on
/// the manual clock) downs it as a wedge and kills the child.
#[test]
fn a_wedge_in_process_is_downed_by_the_wedge_detector() {
    if lab_plugins::absent("a_wedge_in_process_is_downed_by_the_wedge_detector") {
        return;
    }
    let mut rig = Rig::start("hostile-wedge");
    let link = load(&mut rig, HOSTILE, &[4]).expect("loads");
    let mut eng = engine(&link, vec![1.0]);
    eng.block();
    let t = Instant::now();
    while link.region.in_slot().load(Ordering::Acquire) == 0 {
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the chain enters the plugin"
        );
        eng.block();
        std::thread::yield_now();
    }
    rig.advance(Duration::from_millis(2100)); // suspect: ack has not moved for 2 s; the ping goes out
    rig.advance(Duration::from_millis(2100)); // verdict: no pong
    rig.wait(|e| {
        matches!(
            e,
            Event::ChainDown {
                key: KEY,
                reason: DownReason::Wedge
            }
        )
    });
    assert_eq!(rig.bridge.counters().wedges(), 1);
    rig.recorder_clean();
}

/// A crash inside the plugin's creation (chain.load) or its prepare is attributed by the status record to that
/// op, and the second death of the slot quarantines it. The hostile VST3 prepares inside `setupProcessing`, which this
/// child first calls at load (with a placeholder setup), so its "crash in
/// prepare" dies in chain.load.
#[test]
fn a_crash_in_construction_or_prepare_is_attributed_to_that_op() {
    if lab_plugins::absent("a_crash_in_construction_or_prepare_is_attributed_to_that_op") {
        return;
    }
    for (variant, want_op) in [
        (HOSTILE_CRASH_IN_CTOR, "load"),
        (HOSTILE_CRASH_IN_PREPARE, "load"),
    ] {
        let mut rig = Rig::start("hostile-construct");
        assert!(
            load(&mut rig, variant, &[]).is_none(),
            "variant {variant} dies while loading"
        );
        rig.advance_to_respawn(1); // the respawn reloads it, and it dies again
        match rig.wait_timing("no quarantine after respawn #1 was due", |e| {
            matches!(e, Event::Quarantined { .. })
        }) {
            Event::Quarantined { key, slot, op, .. } => {
                assert_eq!(
                    (key, slot, op.as_str()),
                    (KEY, 0, want_op),
                    "ATTRIBUTION: variant {variant}"
                );
            }
            e => panic!("{e:?}"),
        }
        rig.recorder_clean();
    }
}

/// A plugin slow to prepare (3 s) still comes up; and a plugin that reports 77 parameters is believed (only a
/// quarantined slot's report is overridden).
#[test]
fn a_slow_prepare_comes_up_and_a_large_parameter_count_is_reported() {
    if lab_plugins::absent("a_slow_prepare_comes_up_and_a_large_parameter_count_is_reported") {
        return;
    }
    let mut rig = Rig::start("hostile-slow");
    let t0 = Instant::now();
    assert!(
        load(&mut rig, HOSTILE_SLOW_PREPARE, &[]).is_some(),
        "it comes up"
    );
    assert!(
        t0.elapsed() >= Duration::from_secs(3),
        "after its 3 s prepare: {:?}",
        t0.elapsed()
    );
    rig.recorder_clean();
    drop(rig);
    let mut rig = Rig::start("hostile-77");
    let xml = hostile_xml(HOSTILE_77_PARAMS);
    let mut sp = spec(&[&xml]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    match rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. })) {
        Event::ChainReady { slots, .. } => {
            assert_eq!((slots[0].ok, slots[0].param_count), (true, 77), "{slots:?}");
        }
        e => panic!("{e:?}"),
    }
    rig.recorder_clean();
}

/// A lying parameter count through the host application: the 77-parameter variant has 5 real parameters.
/// Region parameters at a ghost index (7, a colliding id) and a failed one (20), driven to 0.0 from the Bridge node,
/// are dropped and counted, never sent: the output keeps playing at gain 1. (Sending them as id 0,
/// the gain, would silence it.) Mutation: driving invalid indices with their (zeroed) id silences it, and this fails.
#[test]
fn a_lying_parameter_count_never_drives_a_bogus_id() {
    if lab_plugins::absent("a_lying_parameter_count_never_drives_a_bogus_id") {
        return;
    }
    let cdir = scratch("hostile-liar-counters");
    let counters = cdir.join("counters.txt");
    let mut rig = Rig::start_with(
        "hostile-liar-params",
        vec!["--counters".into(), counters.clone().into()],
    );
    let link = load(&mut rig, HOSTILE_77_PARAMS, &[7, 20]).expect("loads");
    let mut eng = engine(&link, vec![0.0, 0.0]);
    let mut out = Vec::new();
    for _ in 0..30 {
        out.extend(eng.block());
        assert!(settle(&link, Duration::from_secs(5)));
    }
    assert!(
        out[8 * B..].iter().any(|x| *x != 0.0),
        "the bogus indices did not silence the gain"
    );
    rig.recorder_clean();
    drop(eng);
    drop(rig); // shutdown: the child writes its counters
    let until = Instant::now() + Duration::from_secs(5);
    let text = loop {
        match std::fs::read_to_string(&counters) {
            Ok(t) if !t.is_empty() => break t,
            _ if Instant::now() > until => panic!("no counters at {}", counters.display()),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let _ = std::fs::remove_dir_all(&cdir);
    let dropped: u64 = text
        .split_whitespace()
        .skip_while(|w| *w != "dropped_params")
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);
    assert!(
        dropped > 0,
        "the values for indices 7 and 20 were dropped and counted: {text}"
    );
}
