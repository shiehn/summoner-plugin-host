//! A real VST3 slot's state through the host application, as the protocol frames it (protocol 3.1): every `state.set`
//! carries a state blob {vst3} (the component chunk, the controller chunk, each present or absent), every `state.get`
//! answers one. The component chunk round-trips byte for byte and the audio follows it; a component the plugin refuses
//! is `ok: false` with nothing applied; and a partial set (the component applied, then the controller refused) is
//! `ok: false` with `applied {component: true, controller: false}`: the record that the plugin changed.
//! (A declared divergence: a refused chunk is never reported `ok: true`.)
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event, SlotState};
use summoner_proto::codec::Applied;
use summoner_proto::layout::Geometry;
use summoner_proto::state_blob::{self as sasp, Blob};

const B: usize = 64;
const KEY: u64 = 31;

fn settle(link: &Arc<summoner_rt::rt::ChainLink>) {
    let until = Instant::now() + Duration::from_secs(5);
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        assert!(Instant::now() < until, "the chain never acked");
        std::thread::yield_now();
    }
}

fn load_gain(rig: &mut Rig) -> Arc<summoner_rt::rt::ChainLink> {
    let xml = gain_xml();
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
    let Event::ChainReady { link, .. } =
        rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    link
}

/// `state.set` of `blob`, its receipt: (ok, applied).
fn set(rig: &mut Rig, generation: u64, blob: Vec<u8>) -> (bool, Option<Applied>) {
    rig.send(Command::SetState {
        key: KEY,
        state: SlotState::new(0, generation, Arc::from(blob)),
    });
    match rig.wait(|e| matches!(e, Event::StateApplied { generation: g, .. } if *g == generation)) {
        Event::StateApplied { ok, applied, .. } => (ok, applied),
        e => panic!("{e:?}"),
    }
}

/// The bridged output against the input over 40 blocks: the gain it plays (the input × `g` at a fixed delay).
fn plays_at(link: &Arc<summoner_rt::rt::ChainLink>, g: f32) {
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(link)], B);
    let mut reference = Drive::dry(noise(48_000), B);
    let (mut y, mut x) = (Vec::new(), Vec::new());
    for _ in 0..40 {
        y.extend(eng.block());
        x.extend(reference.block());
        settle(link);
    }
    let p = (0..4 * B)
        .find(|p| (8 * B..16 * B).all(|t| y[t] == x[t - p] * g))
        .unwrap_or_else(|| panic!("the output is the input at {g}, at a fixed pipeline delay"));
    assert!(
        (16 * B..40 * B).all(|t| y[t] == x[t - p] * g),
        "throughout, at {g}"
    );
}

#[test]
fn a_vst3_component_chunk_round_trips_as_sasp_through_the_core_and_plays() {
    if lab_plugins::absent("a_vst3_component_chunk_round_trips_as_sasp_through_the_core_and_plays")
    {
        return;
    }
    let mut rig = Rig::start("state-gain");
    let link = load_gain(&mut rig);
    let chunk = gain_chunk(0.25);
    assert_eq!(
        set(&mut rig, 1, vst3_blob(Some(&chunk), None)),
        (true, None),
        "the gain takes its own chunk"
    );
    rig.send(Command::GetState { key: KEY, slot: 0 });
    match rig.wait(|e| matches!(e, Event::StateRead { key: KEY, .. })) {
        Event::StateRead { blob, .. } => match sasp::decode(&blob) {
            Ok(Blob::Vst3 {
                component: Some(c),
                controller: Some(e),
            }) => {
                assert_eq!(
                    c,
                    &chunk[..],
                    "state.get: the component chunk byte for byte"
                );
                assert_eq!(&e[..4], b"SASC", "and the controller's own chunk, present");
            }
            other => panic!("not a vst3 state blob with both chunks: {other:?}"),
        },
        e => panic!("{e:?}"),
    }
    plays_at(&link, 0.25);
    // A component chunk the plugin refuses: ok false, nothing applied (no `applied`).
    assert_eq!(
        set(&mut rig, 2, vst3_blob(Some(b"not a SASL state"), None)),
        (false, None),
        "a refused component is ok: false, nothing applied"
    );
    // Both chunks, both accepted: ok true, no `applied`.
    let ctl = [b"SASC".as_slice(), &1u32.to_le_bytes(), &1u32.to_le_bytes()].concat();
    assert_eq!(
        set(&mut rig, 3, vst3_blob(Some(&gain_chunk(0.25)), Some(&ctl))),
        (true, None),
        "both chunks applied"
    );
    rig.recorder_clean_for("summoner-lab-gain.vst3");
}

/// The partial-set record: the component applies (the audio moves to 0.5, the proof the plugin
/// changed), then the controller refuses its chunk (every lab VST3 controller refuses one that is not its own 12-byte
/// "SASC" form, lab_vst3.cpp). The receipt is ok: false WITH `applied {component: true, controller: false}`, never a
/// bare ok: false that would say nothing changed.
#[test]
fn a_partial_vst3_set_component_applied_controller_refused() {
    if lab_plugins::absent("a_partial_vst3_set_component_applied_controller_refused") {
        return;
    }
    let mut rig = Rig::start("state-partial");
    let link = load_gain(&mut rig);
    let (ok, applied) = set(
        &mut rig,
        1,
        vst3_blob(Some(&gain_chunk(0.5)), Some(b"nope")),
    );
    assert!(!ok, "a partial set is ok: false");
    assert_eq!(
        applied,
        Some(Applied::new(true, false)),
        "which part applied: exactly one true"
    );
    plays_at(&link, 0.5);
    rig.recorder_clean_for("summoner-lab-gain.vst3");
}
