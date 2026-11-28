//! Protocol 3.8 in this child, through the pinned supervisor: the hello claims
//! 3.8, and `chain.prepareFresh` (kind 25) on a render set releases its slots' processors and makes them again on the
//! kept plugins, answered `chain.preparedFresh`
//! (kind 61) with `ok`, the chain's latency and the child's `ms`. The slot is the test double's echo
//! (`{"format":"fake","fake":{"echo":"64","feedback":"0.5"}}`): after the input has gone silent its tail rings on;
//! after the fresh prepare the render set's output is exactly silent.
//!
//! Mutation (this repo's `--cfg`): `mutant_prepare_fresh_keeps_processor` (host: answered ok, nothing rebuilt)
//! fails `a_fresh_prepare_clears_a_render_sets_tail_and_answers_ok`.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use runecraft_ipc::bridge::sync::Waited;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};

const B: usize = 64;
const KEY: u64 = 648;

/// One render-set block as the render worker runs it (publish, wake `seq`, wait on `ack`); the left channel.
fn render_block(eng: &mut Drive, link: &summoner_rt::rt::ChainLink) -> Vec<f32> {
    let r = &link.region;
    let out = eng.block();
    let _ = r.seq_waker().wake();
    let until = Instant::now() + Duration::from_secs(2);
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        let w = r.ack_waker().wait(
            r.ack().load(Ordering::Acquire),
            Some(Instant::now() + Duration::from_secs(2)),
        );
        assert!(
            w != Waited::TimedOut && Instant::now() < until,
            "the render set never acked"
        );
    }
    out
}

#[test]
fn a_fresh_prepare_clears_a_render_sets_tail_and_answers_ok() {
    if lab_plugins::absent("a_fresh_prepare_clears_a_render_sets_tail_and_answers_ok") {
        return;
    }
    let mut rig = Rig::start("prepare-fresh");
    assert_eq!(
        rig.bridge.counters().child_protocol(),
        4 << 16,
        "the hello claims 4.0, which carries 3.8's kind 25"
    );
    let mut sp =
        spec(&[r#"{"format":"fake","fake":{"name":"echo","echo":"64","feedback":"0.5"}}"#]);
    sp.geometry = summoner_proto::layout::Geometry {
        ch_in: 2,
        ..summoner_proto::layout::Geometry::stereo(B as u32)
    };
    sp.role = summoner_proto::codec::Role::Render;
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let link = match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) {
        Event::ChainReady { link, slots, .. } => {
            assert!(slots[0].ok, "{slots:?}");
            link
        }
        e => panic!("{e:?}"),
    };
    // The noise plays its 4 blocks once, then the input is silent.
    let mut eng = Drive::new(noise(4 * B), vec![Bridged::fx(&link)], B);
    let mut last = Vec::new();
    for _ in 0..12 {
        last = render_block(&mut eng, &link);
    }
    assert!(
        last.iter().any(|v| *v != 0.0),
        "the echo's tail rings on after the input went silent"
    );
    rig.send(Command::PrepareFresh { key: KEY });
    match rig.wait(|e| matches!(e, Event::PreparedFresh { key: KEY, .. })) {
        Event::PreparedFresh {
            ok,
            ms,
            latency,
            error,
            ..
        } => {
            assert!(ok, "{error:?}");
            assert_eq!(latency, 0);
            assert!(ms.is_finite() && ms >= 0.0, "{ms}");
            eprintln!("chain.preparedFresh ms {ms:.3}");
        }
        e => panic!("{e:?}"),
    }
    // One block may still carry the ack before the prepare; from the second the output is the fresh processor's.
    let _ = render_block(&mut eng, &link);
    for k in 0..3 {
        let x = render_block(&mut eng, &link);
        assert!(
            x.iter().all(|v| *v == 0.0),
            "block {k} after the fresh prepare: the warm pass's tail leaked: {x:?}"
        );
    }
}
