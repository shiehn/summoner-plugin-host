//! AU hosting through the host application: the lab's AU v2 slots in this child, driven by the pinned
//! supervisor and the tests' realtime driver (`common::drive`) in lockstep, every child under fswatch + lookwatch with
//! a scratch HOME.
//!
//! - **the AU exit check:** the gain and the synth (notes, CC 64 / 120 / 123) as AU render the same bits as the same
//!   cores as VST3; the gain also equals the raw core (`x * g`) at the pipeline's delay;
//! - the hostile set: a render error status and a rewritten output list are silent THREW blocks and the
//!   chain lives; an abort is attributed and quarantined;
//! - `latency.changed` through the AU's own notice (the listener) and, for an AU that refused the listener, through
//!   the periodic re-read;
//! - the recorders: the registrar lookup is present, nothing else new, nothing under a plugin folder.
//!
//! **Gated to the clean first AU load:** the lab's own bundles only (by their target.noindex
//! paths, through the child's `Bundle::check`), every Rig's recorders asserted (`au_recorder_clean_for`: lab
//! bundles plus the registrar), and no wall-clock assertion (waits are bounded, never measured).
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive, Input};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;
use summoner_rt::rt::MidiEvent;

const B: usize = 64;
const KEY: u64 = 51;

/// The hostile AU's parameter indices (testkit/plugins/au/lab_au.cpp): the host application's, then the AU-only
/// switches.
const H_ABORT: u32 = 1;
const H_LATENCY: u32 = 10;
const H_RENDER_ERROR: u32 = 11;
const H_SWAP_OUT: u32 = 15;

/// A lab AU's slot, by its bundle's absolute path and the entry's codes (`<kind> <sub> SaSL`).
fn au_xml(bundle: &str, name: &str, kind: &[u8; 4], sub: &[u8; 4]) -> String {
    au_ref(
        &lab_plugins::dir()
            .join(format!("au/{bundle}.component"))
            .display()
            .to_string(),
        kind,
        sub,
        b"SaSL",
        name,
    )
}

/// The lab synth VST3's description (its processor class id: "SASL", "LAB" + 2, 'P', 0).
fn vst3_synth_xml() -> String {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414202, 0x50, 0].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-synth.vst3"),
        &cid,
        "Summoner Lab Synth",
    )
}

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

/// One run: a one-slot chain of `xml` (an effect with noise in, or an instrument), its region parameters `params`
/// at `values`, `changes` (block, region index, value), `events` (the bridge's MIDI); the output's bits per block.
struct Run<'a> {
    name: &'a str,
    xml: String,
    fx: bool,
    params: Vec<u32>,
    values: Vec<f64>,
    changes: Vec<(usize, u16, f64)>,
    events: Vec<MidiEvent>,
    blocks: usize,
}

struct Ran {
    rig: Rig,
    out: Vec<u32>,
    /// The right channel's bits (the parameter-list and host-callback fixtures report on it).
    out_r: Vec<u32>,
    link: Option<Arc<summoner_rt::rt::ChainLink>>,
}

fn run(r: Run<'_>) -> Ran {
    let mut rig = Rig::start(&format!("au-{}", r.name));
    let mut sp = spec(&[&r.xml]);
    sp.geometry = Geometry {
        ch_in: if r.fx { 2 } else { 0 },
        ..Geometry::stereo(B as u32)
    };
    sp.param_ids = Arc::from(r.params.clone());
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let Event::ChainReady { link, slots, .. } = rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) else {
        return Ran {
            rig,
            out: Vec::new(),
            out_r: Vec::new(),
            link: None,
        };
    };
    assert!(slots[0].ok, "{}: the slot loaded", r.name);
    let mut eng = Drive::new(
        if r.fx { noise(48_000) } else { Input::None },
        vec![Bridged {
            link: link.clone(),
            events: Arc::from(r.events.clone()),
            fx: r.fx,
            params: r.values.clone(),
        }],
        B,
    );
    let mut bits = Vec::new();
    let mut bits_r = Vec::new();
    for k in 0..r.blocks {
        for (_, p, v) in r.changes.iter().filter(|(b, _, _)| *b == k) {
            eng.set_param(0, usize::from(*p), *v);
        }
        bits.extend(eng.block().iter().map(|x| x.to_bits()));
        bits_r.extend(eng.out.iter().skip(1).step_by(2).map(|x| x.to_bits()));
        if !settle(&link, Duration::from_millis(600)) {
            break;
        }
    }
    Ran {
        rig,
        out: bits,
        out_r: bits_r,
        link: Some(link),
    }
}

fn note(frame: u64, b: [u8; 3]) -> MidiEvent {
    MidiEvent::new(frame, b)
}

#[test]
fn the_au_gain_renders_the_vst3_gains_bits_and_the_cores() {
    if lab_plugins::absent("the_au_gain_renders_the_vst3_gains_bits_and_the_cores") {
        return;
    }
    let mk = |name, xml| Run {
        name,
        xml,
        fx: true,
        params: vec![0],
        values: vec![0.5],
        changes: vec![(20, 0, 0.25)],
        events: vec![],
        blocks: 40,
    };
    let au = run(mk(
        "gain-au",
        au_xml("summoner-lab-gain", "Summoner Lab Gain", b"aufx", b"SLgn"),
    ));
    let v3 = run(mk("gain-vst3", gain_xml()));
    assert!(
        !au.out.is_empty() && au.out == v3.out,
        "AU and VST3: the same bits"
    );
    // The raw core: y = x * g at the pipeline's delay (0.5, then 0.25 from the change).
    let mut reference = Drive::dry(noise(48_000), B);
    let x: Vec<f32> = (0..40).flat_map(|_| reference.block()).collect();
    let y: Vec<f32> = au.out.iter().map(|b| f32::from_bits(*b)).collect();
    let p = (0..4 * B)
        .find(|p| (8 * B..16 * B).all(|t| y[t] == x[t - p] * 0.5))
        .expect("x * 0.5 at a fixed delay");
    assert!(
        (30 * B..40 * B).all(|t| y[t] == x[t - p] * 0.25),
        "x * 0.25 after the change"
    );
    au.rig.au_recorder_clean_for("summoner-lab-gain.component");
    v3.rig.recorder_clean_for("summoner-lab-gain.vst3");
}

#[test]
fn the_au_synth_renders_the_vst3_synths_bits() {
    if lab_plugins::absent("the_au_synth_renders_the_vst3_synths_bits") {
        return;
    }
    let events = vec![
        note(10, [0x90, 69, 100]),
        note(700, [0x90, 72, 90]),
        note(900, [0xb0, 64, 127]),
        note(1100, [0x80, 69, 0]),
        note(1500, [0xb0, 64, 0]),
        note(1800, [0x90, 76, 80]),
        note(2000, [0xb0, 123, 0]),
        note(2300, [0x90, 60, 64]),
        note(2500, [0xb0, 120, 0]),
    ];
    let mk = |name, xml| Run {
        name,
        xml,
        fx: false,
        params: vec![],
        values: vec![],
        changes: vec![],
        events: events.clone(),
        blocks: 48,
    };
    let au = run(mk(
        "synth-au",
        au_xml("summoner-lab-synth", "Summoner Lab Synth", b"aumu", b"SLsy"),
    ));
    let v3 = run(mk("synth-vst3", vst3_synth_xml()));
    assert!(au.out.iter().any(|b| *b != 0), "it sounds");
    assert_eq!(
        au.out, v3.out,
        "AU and VST3: the same bits (notes, CC 64 / 123 / 120)"
    );
    au.rig.au_recorder_clean_for("summoner-lab-synth.component");
}

/// A hostile AU chain with the AU-only switch `param` raised at block 4: the blocks after it are silent THREW and the
/// chain is not downed.
fn silent_and_alive(name: &str, param: u32) {
    let mut ran = run(Run {
        name,
        xml: au_xml(
            "summoner-lab-hostile",
            "Summoner Lab Hostile",
            b"aufx",
            b"SLho",
        ),
        fx: true,
        params: vec![param],
        values: vec![0.0],
        changes: vec![(4, 0, 1.0)],
        events: vec![],
        blocks: 24,
    });
    let silent_from = 8 * B * 2;
    assert!(
        ran.out[..4 * B * 2].iter().any(|b| *b != 0),
        "{name}: it plays before"
    );
    assert!(
        ran.out[silent_from..].iter().all(|b| *b == 0),
        "{name}: silent after"
    );
    std::thread::sleep(Duration::from_millis(100));
    while let Ok(e) = ran.rig.rx.try_recv() {
        ran.rig.seen.push(e);
    }
    assert!(
        !ran.rig
            .seen
            .iter()
            .any(|e| matches!(e, Event::ChainDown { .. })),
        "{name}: the chain lives"
    );
    ran.rig
        .au_recorder_clean_for("summoner-lab-hostile.component");
}

#[test]
fn a_render_error_status_is_a_silent_threw_block_d7() {
    if lab_plugins::absent("a_render_error_status_is_a_silent_threw_block_d7") {
        return;
    }
    silent_and_alive("render-error", H_RENDER_ERROR);
}

#[test]
fn a_rewritten_output_list_is_a_silent_threw_block_d8() {
    if lab_plugins::absent("a_rewritten_output_list_is_a_silent_threw_block_d8") {
        return;
    }
    silent_and_alive("swap-out", H_SWAP_OUT);
}

#[test]
fn an_abort_in_render_is_attributed_and_quarantined() {
    if lab_plugins::absent("an_abort_in_render_is_attributed_and_quarantined") {
        return;
    }
    let mut ran = run(Run {
        name: "abort",
        xml: au_xml(
            "summoner-lab-hostile",
            "Summoner Lab Hostile",
            b"aufx",
            b"SLho",
        ),
        fx: true,
        params: vec![H_ABORT],
        values: vec![0.0],
        changes: vec![(2, 0, 1.0)],
        events: vec![],
        blocks: 8,
    });
    assert!(matches!(
        ran.rig.wait(|e| matches!(e, Event::ChainDown { .. })),
        Event::ChainDown { .. }
    ));
    // The respawn reloads the chain with its parameters (the abort switch still at 1.0): drive it again, so the slot
    // dies a second time in process and is quarantined (as the VST3 hostile test does).
    ran.rig.advance_to_respawn(1);
    let Event::ChainReady { link, .. } = ran
        .rig
        .wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    let mut eng = Drive::new(
        noise(48_000),
        vec![Bridged {
            params: vec![1.0],
            ..Bridged::fx(&link)
        }],
        B,
    );
    for _ in 0..200 {
        eng.block();
        if !settle(&link, Duration::from_millis(500)) {
            break;
        }
    }
    match ran.rig.wait(|e| matches!(e, Event::Quarantined { .. })) {
        Event::Quarantined {
            slot, plugin, op, ..
        } => {
            assert_eq!(
                (slot, plugin.as_str(), op.as_str()),
                (0, "Summoner Lab Hostile", "process")
            )
        }
        e => panic!("{e:?}"),
    }
    ran.rig
        .au_recorder_clean_for("summoner-lab-hostile.component");
}

/// The hostile AU's latency switch moved at block 4 (`H_LATENCY` 0.5 = 4096 frames): `latency.changed` arrives with the
/// chain's new sum, from the AU's own notice (`listening`) or the periodic re-read (the deaf variant).
fn latency_follows(name: &str, sub: &[u8; 4], entry: &str) {
    let mut ran = run(Run {
        name,
        xml: au_xml("summoner-lab-hostile", entry, b"aufx", sub),
        fx: true,
        params: vec![H_LATENCY],
        values: vec![0.0],
        changes: vec![(4, 0, 0.5)],
        events: vec![],
        blocks: 12,
    });
    assert!(ran.link.is_some(), "{name}: loaded");
    match ran.rig.wait(|e| matches!(e, Event::LatencyChanged { .. })) {
        Event::LatencyChanged { link, .. } => assert_eq!(link.plugin_latency, 4096, "{name}"),
        e => panic!("{e:?}"),
    }
    // The re-activation handover is VST3 only: an AU's latency move is latency.changed alone: no handover, no
    // slot.reactivated (and so no ST_BYPASSED).
    std::thread::sleep(Duration::from_millis(200));
    while let Ok(e) = ran.rig.rx.try_recv() {
        ran.rig.seen.push(e);
    }
    assert!(
        !ran.rig
            .seen
            .iter()
            .any(|e| matches!(e, Event::SlotReactivated { .. })),
        "{name}: an AU latency change sends no slot.reactivated: {:?}",
        ran.rig.seen
    );
    ran.rig
        .au_recorder_clean_for("summoner-lab-hostile.component");
}

#[test]
fn latency_changed_follows_the_aus_notice_and_the_reread_for_a_deaf_au() {
    if lab_plugins::absent("latency_changed_follows_the_aus_notice_and_the_reread_for_a_deaf_au") {
        return;
    }
    latency_follows("latency-listening", b"SLho", "Summoner Lab Hostile");
    latency_follows(
        "latency-deaf",
        b"SLh5",
        "Summoner Lab Hostile (refuses listeners)",
    );
}

/// State for an AU slot: `state.get` answers a state blob {au} (the ClassInfo binary plist, `bplist00`); setting it
/// back is `ok: true`; a state blob {au} whose plist does not parse is `ok: false` (a declared divergence: it is not
/// accepted as ok), and no `applied`.
#[test]
fn an_au_state_is_sasp_au_round_trips_and_a_bad_plist_is_ok_false() {
    if lab_plugins::absent("an_au_state_is_sasp_au_round_trips_and_a_bad_plist_is_ok_false") {
        return;
    }
    let mut ran = run(Run {
        name: "au-state",
        xml: au_xml("summoner-lab-gain", "Summoner Lab Gain", b"aufx", b"SLgn"),
        fx: true,
        params: vec![0],
        values: vec![0.25],
        changes: vec![],
        events: vec![],
        blocks: 4,
    });
    ran.rig.send(Command::GetState { key: KEY, slot: 0 });
    let blob = match ran
        .rig
        .wait(|e| matches!(e, Event::StateRead { key: KEY, .. }))
    {
        Event::StateRead { blob, .. } => blob.to_vec(),
        e => panic!("{e:?}"),
    };
    let plist = match summoner_proto::state_blob::decode(&blob) {
        Ok(summoner_proto::state_blob::Blob::Au { plist }) => plist.to_vec(),
        other => panic!("not an au state blob: {other:?}"),
    };
    assert!(plist.starts_with(b"bplist00"), "the ClassInfo binary plist");
    let set = |rig: &mut Rig, generation: u64, blob: Vec<u8>| {
        rig.send(Command::SetState {
            key: KEY,
            state: summoner::supervisor::SlotState::new(0, generation, Arc::from(blob)),
        });
        match rig
            .wait(|e| matches!(e, Event::StateApplied { generation: g, .. } if *g == generation))
        {
            Event::StateApplied { ok, applied, .. } => (ok, applied),
            e => panic!("{e:?}"),
        }
    };
    assert_eq!(
        set(&mut ran.rig, 1, blob.clone()),
        (true, None),
        "its own state back"
    );
    let bad = summoner_proto::state_blob::encode(&summoner_proto::state_blob::Blob::Au {
        plist: b"bplist00 but no plist after the magic",
    })
    .unwrap();
    assert_eq!(
        set(&mut ran.rig, 2, bad),
        (false, None),
        "a plist that does not parse is ok: false"
    );
    ran.rig.au_recorder_clean_for("summoner-lab-gain.component");
}

/// An AU that changes its ParameterList after load (`summoner-lab-paramlist`: at its 8th
/// block, id N inserted at index 0, announced). This child takes the notice and does not act on it: the indices the
/// slot loaded with keep driving the same ids. Region index 0 drives the gain before and after the change (the left
/// channel is the plain AU gain's, bit for bit, through a change at block 20), and id N, which the fixture writes on
/// its right channel, is never driven (0 throughout). On the `kAudioUnitProperty_ParameterList` notice this child
/// re-reads the NAMES and labels of the ids it has (`AudioUnitGetProperty (kAudioUnitProperty_ParameterInfo)`) and
/// never re-indexes: id 0's, which the fixture renames at the change, is asserted through `ParamList`.
#[test]
fn an_aus_parameter_list_change_is_taken_and_the_loaded_indices_keep_driving_m5_au_1() {
    if lab_plugins::absent(
        "an_aus_parameter_list_change_is_taken_and_the_loaded_indices_keep_driving_m5_au_1",
    ) {
        return;
    }
    let mk = |name, xml| Run {
        name,
        xml,
        fx: true,
        params: vec![0],
        values: vec![0.5],
        changes: vec![(20, 0, 0.25)],
        events: vec![],
        blocks: 40,
    };
    let pl = run(mk(
        "paramlist",
        au_xml(
            "summoner-lab-paramlist",
            "Summoner Lab Param List (changes at block 8)",
            b"aufx",
            b"SLpl",
        ),
    ));
    let gain = run(mk(
        "paramlist-gain",
        au_xml("summoner-lab-gain", "Summoner Lab Gain", b"aufx", b"SLgn"),
    ));
    assert_eq!(pl.out.len(), 40 * B, "every block rendered");
    assert!(
        pl.out == gain.out,
        "index 0 drives the gain, before and after the change"
    );
    assert!(
        pl.out_r.iter().all(|b| f32::from_bits(*b) == 0.0),
        "id N is never driven"
    );
    // The names, re-read on the ParameterList notice: the same total and
    // ids as at load, id 0's name re-read ("... (renamed)" from the change on), every other name as loaded.
    let (mut pl, mut gain) = (pl, gain);
    let before = param_list(&mut gain.rig);
    let until = Instant::now() + Duration::from_secs(3);
    let after = loop {
        let l = param_list(&mut pl.rig);
        if l.1.first().is_some_and(|p| p.1.ends_with(" (renamed)")) || Instant::now() > until {
            break l;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    eprintln!("m5-au-1 names: gain {before:?}; paramlist after the change {after:?}");
    assert_eq!(
        after.0, before.0,
        "the total is the loaded one (no re-index)"
    );
    assert_eq!(
        after.1.iter().map(|p| p.0).collect::<Vec<_>>(),
        before.1.iter().map(|p| p.0).collect::<Vec<_>>(),
        "the ids are the loaded ones"
    );
    assert_eq!(
        after.1[0].1,
        format!("{} (renamed)", before.1[0].1),
        "id 0's name is read again after the notice"
    );
    assert_eq!(after.1[1..], before.1[1..], "every other name as loaded");
    pl.rig
        .au_recorder_clean_for("summoner-lab-paramlist.component");
    gain.rig
        .au_recorder_clean_for("summoner-lab-gain.component");
}

/// Slot 0's `ParamList` page (offset 0, 256): (total, [(id, name)]).
fn param_list(rig: &mut Rig) -> (u32, Vec<(u32, String)>) {
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
        } => (total, params.into_iter().map(|p| (p.id, p.name)).collect()),
        e => panic!("param list: {e:?}"),
    }
}

/// An AU that asks its host for the beat, tempo and
/// transport (`summoner-lab-hostcb`) gets, every block, the host application's own transport for that block: tempo 120
/// (the host application's default), all three callbacks answering (flags 7), the sample in the timeline advancing by
/// one block each block, the beat equal to the host application's arithmetic for that sample (ppq = samples / rate ×
/// bpm / 60, as `Transport::at`, the VST3 TRANSPORT reporter's check in the host application's VST3 null test), the
/// time signature 4/4 and the offset to the next beat 0. Read in the steady state, past the pipeline's delay.
#[test]
fn an_au_asking_for_host_callbacks_gets_the_cores_transport_m5_au_2() {
    if lab_plugins::absent("an_au_asking_for_host_callbacks_gets_the_cores_transport_m5_au_2") {
        return;
    }
    let r = run(Run {
        name: "hostcb",
        xml: au_xml(
            "summoner-lab-hostcb",
            "Summoner Lab Host Callbacks (reports them)",
            b"aufx",
            b"SLhc",
        ),
        fx: true,
        params: vec![],
        values: vec![],
        changes: vec![],
        events: vec![],
        blocks: 24,
    });
    let l: Vec<f32> = r.out.iter().map(|b| f32::from_bits(*b)).collect();
    let rr: Vec<f32> = r.out_r.iter().map(|b| f32::from_bits(*b)).collect();
    assert_eq!(l.len(), 24 * B);
    // The fixture's blocks: where its right channel carries tempo 120 and flags 7 at a block's first two frames.
    let starts: Vec<usize> = (0..rr.len().saturating_sub(1))
        .filter(|&t| rr[t] == 120.0 && rr[t + 1] == 7.0)
        .collect();
    assert!(
        starts.len() >= 16,
        "the fixture reported in most blocks: {starts:?}"
    );
    let mut last: Option<f32> = None;
    for &t in &starts {
        let block = &rr[t..t + 8];
        let samples = block[3];
        let ppq = (f64::from(samples) / 48_000.0 * 120.0 / 60.0) as f32;
        assert!(
            l[t..t + B.min(l.len() - t)].iter().all(|x| *x == ppq),
            "frame {t}: the beat is the block's ppq"
        );
        assert_eq!(
            (block[4], block[5], block[7]),
            (4.0, 4.0, 0.0),
            "frame {t}: 4/4, offset 0"
        );
        if let Some(p) = last {
            assert_eq!(samples, p + B as f32, "frame {t}: one block later");
        }
        last = Some(samples);
    }
    r.rig.au_recorder_clean_for("summoner-lab-hostcb.component");
}

/// Crash attribution for the prepare op (since the hostile VST3's prepare crash now dies at load, the placeholder
/// setupProcessing): the hostile AU `SLh2` aborts in `Initialize`, which this child calls at the chain's prepare, so
/// the status record attributes the death to `prepare`, and the second death of the slot quarantines it.
#[test]
fn an_au_crash_in_prepare_is_attributed_to_prepare() {
    if lab_plugins::absent("an_au_crash_in_prepare_is_attributed_to_prepare") {
        return;
    }
    let mut rig = Rig::start("au-hostile-prepare");
    let mut sp = spec(&[&au_xml(
        "summoner-lab-hostile",
        "Summoner Lab Hostile (crash in prepare)",
        b"aufx",
        b"SLh2",
    )]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    assert!(
        matches!(
            rig.wait(|e| matches!(
                e,
                Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
            )),
            Event::ChainDown { .. }
        ),
        "the AU dies while preparing"
    );
    rig.advance_to_respawn(1); // the respawn reloads it, and it dies again
    match rig.wait_timing("no quarantine after respawn #1 was due", |e| {
        matches!(e, Event::Quarantined { .. })
    }) {
        Event::Quarantined { key, slot, op, .. } => {
            assert_eq!((key, slot, op.as_str()), (KEY, 0, "prepare"), "ATTRIBUTION")
        }
        e => panic!("{e:?}"),
    }
    rig.au_recorder_clean_for("summoner-lab-hostile.component");
}

/// The protocol claim tied to the AU entry pick: the child claims protocol 3.5 at `hello` (the host application sends
/// AU slots only then) and, with that claim, a slot described by path plus triple is the triple's entry. The multi
/// fixture's two entries share one uniqueId (the XOR): a child that picked by it would load A for B (the liar, more
/// parameters).
#[test]
fn the_hello_claims_3_6_and_a_triple_picks_the_colliding_entry() {
    if lab_plugins::absent("the_hello_claims_3_6_and_a_triple_picks_the_colliding_entry") {
        return;
    }
    let path = lab_plugins::dir().join("au/summoner-lab-multi.component");
    let xml = |name: &str, sub: &[u8; 4], manu: &[u8; 4]| {
        au_ref(&path.display().to_string(), b"aufx", sub, manu, name)
    };
    let mut rig = Rig::start("au-multi-triple");
    let mut sp = spec(&[
        &xml("Summoner Lab Multi A", b"SLxa", b"SaSL"),
        &xml("Summoner Lab Multi B", b"SLx`", b"SaSM"),
        &xml("Summoner Lab Multi C", b"SLxc", b"SaSN"),
    ]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let Event::ChainReady { slots, .. } = rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) else {
        panic!("the chain loaded");
    };
    let claimed = rig.bridge.counters().child_protocol();
    // 4.x carries 3.5's entry pick; the host application sends an AU slot to any verified 4.x child (it has no 3.5
    // gate).
    assert!(
        claimed >> 16 == 4,
        "the child's hello says 4.x, which carries 3.5's AU entry pick: {claimed:#x}"
    );
    assert!(slots[0].ok && slots[1].ok, "A and B loaded");
    // B is the liar (the gain core's ids plus its repeats of id 0); A is the plain gain. A pick by uniqueId would
    // give both slots A's count.
    assert!(
        slots[1].param_count > slots[0].param_count,
        "each slot is its triple's entry, not the first entry with its uniqueId: A {} B {}",
        slots[0].param_count,
        slots[1].param_count
    );
    assert!(
        !slots[2].ok,
        "a triple the bundle does not have: not ok, never A"
    );
    rig.au_recorder_clean_for("summoner-lab-multi.component");
}

/// Bridge protocol 3.9: a music device whose input element refuses the chain's stereo (the retry with no
/// inputs) is named `inputsRefused` in `chain.prepared`'s per-slot notices, which the pinned supervisor hands control
/// on `ChainReady`. Mutation `--cfg mutant_child_no_notice` (the AU names none) fails it.
#[test]
fn an_instrument_that_refused_its_inputs_is_named_on_chain_ready() {
    if lab_plugins::absent("an_instrument_that_refused_its_inputs_is_named_on_chain_ready") {
        return;
    }
    let mut rig = Rig::start("au-inputs-refused");
    assert_eq!(
        rig.bridge.counters().child_protocol(),
        4 << 16,
        "the hello claims 4.0, which carries 3.9/3.10's slot notices"
    );
    let mut sp = spec(&[&au_xml(
        "summoner-lab-synthinstrict",
        "Summoner Lab Synth In Strict",
        b"aumu",
        b"SLsx",
    )]);
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
        Event::ChainReady { slots, .. } => {
            assert!(slots[0].ok, "the strict synth loads");
            assert_eq!(slots[0].notices, vec!["inputsRefused".to_string()]);
        }
        e => panic!("{e:?}"),
    }
    rig.au_recorder_clean_for("summoner-lab-synthinstrict.component");
}

#[test]
fn fresh_prepare_reports_input_acceptance_changes() {
    if lab_plugins::absent("fresh_prepare_reports_input_acceptance_changes") {
        return;
    }
    let mut rig = Rig::start("au-changing-inputs");
    let mut sp = spec(&[&au_xml(
        "summoner-lab-synthinchange",
        "Summoner Lab Synth In Change",
        b"aumu",
        b"SLsc",
    )]);
    sp.role = summoner_proto::codec::Role::Render;
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
            assert_eq!(slots[0].notices, vec!["inputsRefused".to_string()])
        }
        e => panic!("{e:?}"),
    }
    for refuses in [false, true, false] {
        rig.send(Command::ParamSet {
            key: KEY,
            slot: 0,
            plugin_id: 0,
            value: if refuses { 1.0 } else { 0.0 },
        });
        match rig.wait(|e| {
            matches!(
                e,
                Event::ParamSet {
                    key: KEY,
                    slot: 0,
                    ..
                }
            )
        }) {
            Event::ParamSet { ok, .. } => assert!(ok, "fixture acceptance parameter applied"),
            e => panic!("{e:?}"),
        }
        rig.send(Command::PrepareFresh { key: KEY });
        match rig.wait(|e| matches!(e, Event::PreparedFresh { key: KEY, .. })) {
            Event::PreparedFresh {
                ok,
                notices_known,
                slots,
                error,
                ..
            } => {
                assert!(ok, "{error:?}");
                assert!(notices_known, "3.10 child must report a fresh snapshot");
                assert_eq!(
                    slots[0].notices,
                    if refuses {
                        vec!["inputsRefused".to_string()]
                    } else {
                        vec![]
                    },
                    "actual fresh processor state"
                );
            }
            e => panic!("{e:?}"),
        }
    }
    rig.au_recorder_clean_for("summoner-lab-synthinchange.component");
}
