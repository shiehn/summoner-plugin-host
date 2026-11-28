//! Protocol 3.7 in this child, through the pinned supervisor: `param.list`'s
//! 3.7 fields and the parameter-tree order on the lab's own plugins (`summoner-lab-params`, VST3 and AU,
//! built by `tools/build-plugins.sh`; never a real plugin), and `param.set` (kind 35) with its read-back, its refusals
//! and the slot's generation.
//!
//! Mutations (this repo's `--cfg`), each failing the test named:
//! - `mutant_param_gen_before_apply` (host): `a_vst3_set_moves_the_generation_again_once_the_processor_took_it`;
//! - `mutant_param_automatable_always` (vst3):
//!   `the_lab_vst3_reports_its_tree_order_flags_steps_and_units`;
//! - `mutant_param_set_any_id` (vst3): `an_unknown_id_a_non_finite_value_and_a_missing_slot_are_refused`.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::codec::ParamEntry;
use summoner_proto::layout::Geometry;

const B: usize = 64;
const KEY: u64 = 631;

/// The lab VST3 PARAMS fixture (testkit/plugins/vst3/lab_vst3.cpp: "SASL", "LAB" + 1, 'P', variant 0x49).
fn vst3_params_xml() -> String {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414201, 0x50, 0x49].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-params.vst3"),
        &cid,
        "Summoner Lab Params",
    )
}

/// The lab AU PARAMS fixture (`aufx SLpm SaSL`), by its bundle path and codes.
fn au_params_xml() -> String {
    au_ref(
        &lab_plugins::dir()
            .join("au/summoner-lab-params.component")
            .display()
            .to_string(),
        b"aufx",
        b"SLpm",
        b"SaSL",
        "Summoner Lab Params",
    )
}

fn load(rig: &mut Rig, xml: &str) -> Arc<summoner_rt::rt::ChainLink> {
    let mut sp = spec(&[xml]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
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
        Event::ChainReady { link, slots, .. } => {
            assert!(slots[0].ok, "the slot loaded");
            link
        }
        e => panic!("the chain did not load: {e:?}"),
    }
}

fn page(rig: &mut Rig) -> (u32, Vec<ParamEntry>) {
    rig.send(Command::ParamList {
        key: KEY,
        slot: 0,
        offset: 0,
        count: 256,
    });
    match rig.wait(|e| {
        matches!(
            e,
            Event::ParamList {
                key: KEY,
                slot: 0,
                ..
            }
        )
    }) {
        Event::ParamList {
            ok: true,
            total,
            params,
            ..
        } => (total, params),
        e => panic!("param list: {e:?}"),
    }
}

fn set(rig: &mut Rig, slot: u32, plugin_id: u32, value: f32) -> (bool, f32) {
    rig.send(Command::ParamSet {
        key: KEY,
        slot,
        plugin_id,
        value,
    });
    match rig.wait(|e| matches!(e, Event::ParamSet { key: KEY, slot: s, plugin_id: p, .. } if *s == slot && *p == plugin_id)) {
        Event::ParamSet { ok, value, .. } => (ok, value),
        e => panic!("param set: {e:?}"),
    }
}

/// The claim is tied to kind 35: a 3.7-or-later hello (3.8 and later keep every 3.7 behaviour), and the test
/// double's set answered with its read-back.
#[test]
fn the_hello_claims_3_7_and_param_set_is_answered() {
    if lab_plugins::absent("the_hello_claims_3_7_and_param_set_is_answered") {
        return;
    }
    let mut rig = Rig::start("param-set-claim");
    let claimed = rig.bridge.counters().child_protocol();
    assert!(
        claimed >> 16 == 4,
        "the child's hello says 4.x, which carries 3.7's param.set: {claimed:#x}"
    );
    load(
        &mut rig,
        r#"{"format":"fake","fake":{"name":"p","gain":"0.5","xparams":"Cutoff|1|5|0|Hz|0.25;Mode|0|0|3||0;Res|1|5|0||0.5"}}"#,
    );
    let (total, p) = page(&mut rig);
    assert_eq!(total, 7);
    assert_eq!(
        p.iter().map(|e| e.plugin_id).collect::<Vec<_>>(),
        [10, 11, 12, 13, 100, 101, 102]
    );
    assert_eq!(
        p.iter().map(|e| e.order).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 6, 5]
    );
    assert_eq!(set(&mut rig, 0, 10, 0.25), (true, 0.25));
    assert_eq!(set(&mut rig, 0, 102, 0.75), (true, 0.75));
    let (_, p) = page(&mut rig);
    assert_eq!((p[0].value, p[6].value), (0.25, 0.75));
}

/// On the lab VST3: the flattened unit tree (units 1, 2 and 3 under 2),
/// `kCanAutomate` alone (a hidden one is automatable, a read-only meter is not), the list's steps, the units label, the
/// empty title.
#[test]
fn the_lab_vst3_reports_its_tree_order_flags_steps_and_units() {
    if lab_plugins::absent("the_lab_vst3_reports_its_tree_order_flags_steps_and_units") {
        return;
    }
    let mut rig = Rig::start("param-set-vst3-tree");
    load(&mut rig, &vst3_params_xml());
    let (total, p) = page(&mut rig);
    assert_eq!(total, 8, "{p:?}");
    assert_eq!(
        p.iter().map(|e| e.plugin_id).collect::<Vec<_>>(),
        [0, 4000, 4001, 4002, 4003, 4004, 4005, 4006]
    );
    assert_eq!(
        p.iter().map(|e| e.automatable).collect::<Vec<_>>(),
        [true, true, true, true, false, true, true, true]
    );
    // Root: [gain, B, Mode, A, Meter, ""]; B: [Cutoff, Cutoff, C]; C: [Res]; A: [Hidden].
    assert_eq!(
        p.iter().map(|e| e.order).collect::<Vec<_>>(),
        [0, 1, 4, 5, 6, 2, 7, 3]
    );
    assert_eq!(p[2].step_count, 3);
    assert_eq!(p[1].label, "Hz");
    assert_eq!(p[4].label, "dB");
    assert_eq!(p[6].name, "");
    assert_eq!(
        (p[1].name.as_str(), p[5].name.as_str()),
        ("Cutoff", "Cutoff")
    );
    assert_eq!(p[1].default, 0.25);
    assert_eq!(p[1].value, 0.25, "the controller's value");
    rig.recorder_clean_for("summoner-lab-params.vst3");
}

/// On the lab AU: the clumps gathered at their first member, NonRealTime not automatable, the
/// indexed unit's steps, the unit label table and the custom unit's name, the defaults normalized.
#[test]
fn the_lab_au_reports_its_clump_order_flags_steps_and_labels() {
    if lab_plugins::absent("the_lab_au_reports_its_clump_order_flags_steps_and_labels") {
        return;
    }
    let mut rig = Rig::start("param-set-au-tree");
    load(&mut rig, &au_params_xml());
    let (total, p) = page(&mut rig);
    assert_eq!(total, 8, "{p:?}");
    assert_eq!(
        p.iter().map(|e| e.plugin_id).collect::<Vec<_>>(),
        (0..8).collect::<Vec<u32>>()
    );
    assert_eq!(
        p.iter().map(|e| e.automatable).collect::<Vec<_>>(),
        [true, true, true, false, true, true, true, true]
    );
    // Root: [gain, clump 5 (Cutoff, Cutoff), Mode, Offline, "", clump 9 (Drive, Res)].
    assert_eq!(
        p.iter().map(|e| e.order).collect::<Vec<_>>(),
        [0, 1, 3, 4, 2, 5, 6, 7]
    );
    assert_eq!(p[2].step_count, 3);
    assert_eq!(
        (
            p[1].label.as_str(),
            p[6].label.as_str(),
            p[7].label.as_str()
        ),
        ("Hz", "drv", "%")
    );
    assert_eq!(p[5].name, "");
    assert!((p[1].default - (1000.0 - 20.0) / 19980.0).abs() < 1e-6);
    assert!((p[2].default - 1.0 / 3.0).abs() < 1e-6);
    // An AU set is the unit's at once (`AudioUnitSetParameter`, `kAudioUnitScope_Global`): the read-back and the list
    // agree.
    let (ok, v) = set(&mut rig, 0, 1, 0.5);
    assert!(ok && (v - 0.5).abs() < 1e-6, "{v}");
    let (_, p) = page(&mut rig);
    assert!((p[1].value - 0.5).abs() < 1e-6);
    rig.au_recorder_clean_for("summoner-lab-params.component");
}

#[test]
fn an_unknown_id_a_non_finite_value_and_a_missing_slot_are_refused() {
    if lab_plugins::absent("an_unknown_id_a_non_finite_value_and_a_missing_slot_are_refused") {
        return;
    }
    let mut rig = Rig::start("param-set-refused");
    load(&mut rig, &vst3_params_xml());
    assert!(!set(&mut rig, 0, 99_999, 0.5).0, "no such parameter");
    assert!(!set(&mut rig, 4, 4000, 0.5).0, "no such slot");
    assert!(!set(&mut rig, 0, 4000, f32::INFINITY).0, "never sent");
    let (ok, v) = set(&mut rig, 0, 4000, 0.75);
    assert!(ok && (v - 0.75).abs() < 1e-6, "{v}");
    let (_, p) = page(&mut rig);
    assert!(
        (p[1].value - 0.75).abs() < 1e-6,
        "the controller holds the set"
    );
    assert!((p[0].value - 1.0).abs() < 1e-6, "the gain is untouched");
}

/// A VST3 set moves the slot's generation at once (the controller holds it), and once more after the
/// processor took it at its next block, so a capture after the block reads the applied state.
#[test]
fn a_vst3_set_moves_the_generation_again_once_the_processor_took_it() {
    if lab_plugins::absent("a_vst3_set_moves_the_generation_again_once_the_processor_took_it") {
        return;
    }
    let mut rig = Rig::start("param-set-gen");
    let link = load(&mut rig, &vst3_params_xml());
    // The baseline generation, then the set's first move.
    let _ = moved_gen(&mut rig);
    assert!(set(&mut rig, 0, 4000, 0.6).0);
    let first = moved_gen(&mut rig);
    assert!(first > 0, "the set moved the slot at once");
    // Blocks: the processor takes the queued value.
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], B);
    for _ in 0..8 {
        eng.block();
        let until = Instant::now() + Duration::from_millis(600);
        let r = &link.region;
        while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire)
            && Instant::now() < until
        {
            std::thread::yield_now();
        }
    }
    // The child's poll (50 ms) sees the processor took it.
    std::thread::sleep(Duration::from_millis(300));
    let second = moved_gen(&mut rig);
    assert!(
        second > first,
        "the generation moved again once the processor took the value ({first} then {second})"
    );
}

/// The slot's generation as a save burst's generations read reports it moved (the supervisor polls generations at a
/// burst, or while an editor is open), or 0 when it did not move since the last one.
fn moved_gen(rig: &mut Rig) -> u64 {
    static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let token = TOKEN.fetch_add(1, Ordering::Relaxed);
    rig.seen.clear();
    rig.send(Command::save_burst(token, Vec::new()));
    let mut moved = 0;
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        match rig.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::Moved { slots }) => {
                if let Some((_, _, g)) = slots.iter().find(|(k, s, _)| *k == KEY && *s == 0) {
                    moved = *g;
                }
            }
            Ok(Event::Burst { token: t, .. }) if t == token => return moved,
            Ok(e) => rig.seen.push(e),
            Err(_) => rig.advance(Duration::from_millis(100)),
        }
    }
    moved
}
