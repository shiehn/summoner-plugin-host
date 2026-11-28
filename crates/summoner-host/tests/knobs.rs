//! The hostile VST3's getState knobs and its TRANSPORT reporter (the knobs are
//! the host application's component-state words, `testkit/plugins/dsp/include/summoner_lab_dsp.h`), through this child
//! and the host application's supervisor: what a `state.get` answers for each, and the transport this child hands the
//! plugin, read back from the AUDIO (TRANSPORT_ENCODING) and checked against the host application's own `Transport::at`
//! arithmetic.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event, SlotState};
use summoner_proto::layout::Geometry;
use summoner_proto::state_blob::{self as sasp, Blob};

const B: usize = 64;
const KEY: u64 = 41;

fn load(rig: &mut Rig) -> Arc<summoner_rt::rt::ChainLink> {
    let xml = hostile_xml(HOSTILE);
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
        Event::ChainReady { link, .. } => link,
        e => panic!("{e:?}"),
    }
}

/// `state.set` of a component chunk alone: its `ok`.
fn set(rig: &mut Rig, generation: u64, component: &[u8]) -> bool {
    rig.send(Command::SetState {
        key: KEY,
        state: SlotState::new(0, generation, Arc::from(vst3_blob(Some(component), None))),
    });
    match rig.wait(|e| matches!(e, Event::StateApplied { generation: g, .. } if *g == generation)) {
        Event::StateApplied { ok, .. } => ok,
        e => panic!("{e:?}"),
    }
}

/// `state.get`: (the component chunk, the controller chunk) as the state blob carries them, and how long it took.
fn get(rig: &mut Rig) -> (Option<Vec<u8>>, Option<Vec<u8>>, Duration) {
    let t0 = Instant::now();
    rig.send(Command::GetState { key: KEY, slot: 0 });
    match rig.wait(|e| matches!(e, Event::StateRead { key: KEY, .. })) {
        Event::StateRead { blob, .. } => match sasp::decode(&blob) {
            Ok(Blob::Vst3 {
                component,
                controller,
            }) => (
                component.map(<[u8]>::to_vec),
                controller.map(<[u8]>::to_vec),
                t0.elapsed(),
            ),
            other => panic!("not a vst3 state blob: {other:?}"),
        },
        e => panic!("{e:?}"),
    }
}

#[test]
fn slow_big_and_failing_gets_answer_as_the_knobs_say() {
    if lab_plugins::absent("slow_big_and_failing_gets_answer_as_the_knobs_say") {
        return;
    }
    let mut rig = Rig::start("knobs-get");
    let _link = load(&mut rig);
    // SLOW_GETSTATE: the get waits, then answers the knob's own bytes.
    assert!(set(&mut rig, 1, b"SLOW_GETSTATE=300"));
    let (c, e, took) = get(&mut rig);
    assert_eq!(c.as_deref(), Some(&b"SLOW_GETSTATE=300"[..]));
    assert_eq!(&e.expect("the controller's chunk")[..4], b"SASC");
    assert!(took >= Duration::from_millis(300), "{took:?}");
    // BIG_GETSTATE: exactly 2 MiB, and it round-trips through a set.
    assert!(set(&mut rig, 2, b"BIG_GETSTATE"));
    let (big, _, _) = get(&mut rig);
    let big = big.expect("a component chunk");
    assert_eq!(big.len(), 2 * 1024 * 1024);
    assert_eq!(&big[..8], b"SASBIG2M");
    assert!(
        big.iter()
            .enumerate()
            .skip(8)
            .all(|(i, &b)| b == ((i as u64 * 2_654_435_761) >> 24) as u8),
        "the documented pattern"
    );
    assert!(set(&mut rig, 3, &big), "the 2 MiB chunk is taken back");
    assert_eq!(get(&mut rig).0.as_deref(), Some(&big[..]));
    // FAIL_GETSTATE: a partial get, the component absent, the controller present.
    assert!(set(&mut rig, 4, b"FAIL_GETSTATE"));
    let (c, e, _) = get(&mut rig);
    assert_eq!(c, None, "the component's getState failed");
    assert_eq!(&e.expect("the controller's chunk still")[..4], b"SASC");
    // A normal state ends the mode.
    assert!(set(&mut rig, 5, &hostile_chunk(1.0)));
    assert_eq!(get(&mut rig).0, Some(hostile_chunk(1.0)));
    rig.recorder_clean_for("summoner-lab-hostile.vst3");
}

/// The hostile core's normal state ("SASL", kind 3, version 1, its 11 parameters as raw f32: the gain, then 0s).
fn hostile_chunk(gain: f32) -> Vec<u8> {
    let mut v = b"SASL".to_vec();
    for x in [3u32, 1, 11] {
        v.extend(x.to_le_bytes());
    }
    v.extend(gain.to_bits().to_le_bytes());
    for _ in 1..11 {
        v.extend(0u32.to_le_bytes());
    }
    v
}

#[test]
fn the_transport_reporter_plays_what_this_child_passed() {
    if lab_plugins::absent("the_transport_reporter_plays_what_this_child_passed") {
        return;
    }
    let mut rig = Rig::start("knobs-transport");
    let link = load(&mut rig);
    assert!(set(&mut rig, 1, b"TRANSPORT"));
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], B);
    let mut blocks = Vec::new();
    for _ in 0..40 {
        eng.block();
        blocks.push(eng.out.clone());
        let until = Instant::now() + Duration::from_secs(5);
        let r = &link.region;
        while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
            assert!(Instant::now() < until, "the chain never acked");
            std::thread::yield_now();
        }
    }
    let mut last = None;
    for (n, b) in blocks.iter().enumerate().skip(8) {
        let (l, r): (Vec<f32>, Vec<f32>) = (
            b.iter().step_by(2).copied().collect(),
            b.iter().skip(1).step_by(2).copied().collect(),
        );
        let flags = r[1] as u32;
        assert_eq!(
            r[1], flags as f32,
            "block {n}: the flags are an exact integer"
        );
        assert_eq!(
            flags & 0b11_1000,
            0b11_1000,
            "block {n}: given, music and tempo valid: {flags:#b}"
        );
        assert_eq!(flags & 0b110, 0, "block {n}: not recording, no cycle");
        assert_eq!(r[0], 120.0, "block {n}: the tempo");
        assert!(
            r[3..].iter().all(|&x| x == 0.0),
            "block {n}: the rest of ch1 is 0"
        );
        let samples = r[2] as u64;
        let ppq = (samples as f64 / 48_000.0 * 120.0 / 60.0) as f32;
        assert!(
            l.iter().all(|&x| x == ppq),
            "block {n}: ch0 = the block-start ppq of {samples} samples"
        );
        if let Some(prev) = last {
            assert_eq!(samples, prev + B as u64, "block {n}: one block later");
        }
        last = Some(samples);
    }
    rig.recorder_clean_for("summoner-lab-hostile.vst3");
}
