//! The slot channel rule through the host application, around every called slot, on the slot's buses (`Dsp::io`,
//! cached at prepare). The pinned supervisor and the tests' driver (`common::drive`) drive a fresh child per
//! run, an effect chain fed stereo noise whose channels differ (so L != R and the average is visible); each case's two
//! output channels are compared, bit for bit, with the expected samples for the same input (an external-plugin node,
//! the child's staging and the proxy's pop, as `chain.rs`'s `stage_inputs` and `finish_outputs` model them). The input
//! itself is read back through an identity fake slot (2 in, 2 out: the rule moves nothing).
//!
//! The slots are the test double's `inputs` / `outputs` / `write` knobs, and the lab's VST3 and AU synths (0 inputs):
//! fed the chain's input at the head of an effect chain, they render the same bits as with no input at all.
#![forbid(unsafe_code)]

mod common;

use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;

const B: usize = 64;
const KEY: u64 = 57;
const BLOCKS: usize = 24;

fn settle(link: &Arc<summoner_rt::rt::ChainLink>) {
    let until = Instant::now() + Duration::from_secs(5);
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        assert!(Instant::now() < until, "the chain never acked");
        std::thread::yield_now();
    }
}

/// [`noise_repeating`] with its right channel at half the left's: the two channels differ.
fn panned_noise() -> drive::Input {
    drive::Input::Stereo(Box::new(noise_repeating(4_800, BLOCKS * B)), 0.5)
}

/// The two output channels of a run, whole.
#[derive(Debug, Clone, PartialEq)]
struct Out {
    l: Vec<f32>,
    r: Vec<f32>,
}

impl Out {
    fn bits(&self) -> (Vec<u32>, Vec<u32>) {
        let b = |v: &[f32]| v.iter().map(|x| x.to_bits()).collect::<Vec<_>>();
        (b(&self.l), b(&self.r))
    }
    fn zeros() -> Out {
        Out {
            l: vec![0.0; BLOCKS * B],
            r: vec![0.0; BLOCKS * B],
        }
    }
}

/// One fresh child with `slots` (`enabled` as `chain.load`'s; empty: all), an effect chain fed [`panned_noise`]
/// (`fx`) or an instrument with no input; a note on at frame 0 either way (only a `synth` hears it).
fn run(name: &str, slots: &[&str], fx: bool, enabled: &[bool]) -> Out {
    let mut rig = Rig::start(&format!("io-{name}"));
    let mut sp = spec(slots);
    sp.enabled = enabled.to_vec();
    sp.geometry = Geometry {
        ch_in: if fx { 2 } else { 0 },
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let Event::ChainReady {
        link, slots: got, ..
    } = rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    assert!(got.iter().all(|s| s.ok), "{name}: every slot hosted");
    let chain = drive::Bridged {
        events: Arc::from(vec![summoner_rt::rt::MidiEvent::new(0, [0x90, 60, 100])]),
        fx,
        ..drive::Bridged::fx(&link)
    };
    let input = if fx {
        panned_noise()
    } else {
        drive::Input::None
    };
    let mut eng = drive::Drive::new(input, vec![chain], B);
    let mut o = Out {
        l: Vec::with_capacity(BLOCKS * B),
        r: Vec::with_capacity(BLOCKS * B),
    };
    for _ in 0..BLOCKS {
        let _ = eng.block();
        o.l.extend(eng.out.iter().step_by(2));
        o.r.extend(eng.out.iter().skip(1).step_by(2));
        settle(&link);
    }
    o
}

/// The chain's input as an identity slot passes it (2 in, 2 out).
fn input() -> Out {
    let i = run("input", &[r#"{"format":"fake","fake":{}}"#], true, &[]);
    assert!(
        i.l.iter().zip(&i.r).any(|(l, r)| l != r) && i.l.iter().any(|x| *x != 0.0),
        "the input is stereo and sounds"
    );
    i
}

fn assert_out(what: &str, got: &Out, want: &Out) {
    let (gl, gr) = got.bits();
    let (wl, wr) = want.bits();
    let first = |a: &[u32], b: &[u32]| a.iter().zip(b).position(|(x, y)| x != y);
    if let Some(f) = first(&gl, &wl) {
        panic!(
            "{what}: channel 0 differs first at frame {f}: {:?} vs {:?}",
            &got.l[f..(f + 4).min(got.l.len())],
            &want.l[f..(f + 4).min(want.l.len())]
        );
    }
    if let Some(f) = first(&gr, &wr) {
        panic!(
            "{what}: channel 1 differs first at frame {f}: {:?} vs {:?}",
            &got.r[f..(f + 4).min(got.r.len())],
            &want.r[f..(f + 4).min(want.r.len())]
        );
    }
}

/// The five-shape table: in0/out1, in1/out2, in2/out1, in1/out1 and out0, against the expected samples of the same
/// input.
#[test]
fn the_five_bus_shapes_give_cpps_samples() {
    if lab_plugins::absent("the_five_bus_shapes_give_cpps_samples") {
        return;
    }
    let i = input();
    let z = vec![0.0f32; BLOCKS * B];
    let avg: Vec<f32> = i.l.iter().zip(&i.r).map(|(l, r)| (l + r) * 0.5).collect();
    let cases = [
        (
            "in0-out1",
            r#"{"format":"fake","fake":{"inputs":"0","outputs":"1"}}"#,
            Out::zeros(),
        ),
        (
            "in1-out2",
            r#"{"format":"fake","fake":{"inputs":"1","outputs":"2"}}"#,
            Out {
                l: i.l.clone(),
                r: z.clone(),
            },
        ),
        (
            "in2-out1",
            r#"{"format":"fake","fake":{"inputs":"2","outputs":"1"}}"#,
            Out {
                l: i.l.clone(),
                r: z.clone(),
            },
        ),
        (
            "in1-out1",
            r#"{"format":"fake","fake":{"inputs":"1","outputs":"1"}}"#,
            Out {
                l: avg.clone(),
                r: avg.clone(),
            },
        ),
        (
            "out0",
            r#"{"format":"fake","fake":{"outputs":"0"}}"#,
            Out::zeros(),
        ),
    ];
    for (name, xml, want) in cases {
        let got = run(name, &[xml], true, &[]);
        assert_out(name, &got, &want);
    }
}

/// The rule is per slot: each slot sees what the one before it left, through its own buses.
#[test]
fn each_slot_applies_its_own_buses() {
    if lab_plugins::absent("each_slot_applies_its_own_buses") {
        return;
    }
    let i = input();
    let avg: Vec<f32> = i.l.iter().zip(&i.r).map(|(l, r)| (l + r) * 0.5).collect();
    let got = run(
        "two-slots",
        &[
            r#"{"format":"fake","fake":{"inputs":"1","outputs":"1"}}"#,
            r#"{"format":"fake","fake":{"inputs":"2","outputs":"1"}}"#,
        ],
        true,
        &[],
    );
    assert_out(
        "in1/out1 then in2/out1",
        &got,
        &Out {
            l: avg,
            r: vec![0.0; BLOCKS * B],
        },
    );
}

/// A 0-input instrument at the head of an effect chain, fed the input: whatever it writes (everything, channel 0
/// only, accumulating, nothing), the output is its own signal or silence, never the input.
#[test]
fn a_zero_input_instrument_fed_input_outputs_only_its_own_signal() {
    if lab_plugins::absent("a_zero_input_instrument_fed_input_outputs_only_its_own_signal") {
        return;
    }
    // Its own signal: the gate as the same synth renders it in an instrument chain (no input; the bridge's pipeline
    // delay included).
    let alone = run(
        "synth-alone",
        &[r#"{"format":"fake","fake":{"synth":"1"}}"#],
        false,
        &[],
    );
    assert!(alone.l.contains(&0.25), "it sounds");
    assert_eq!(alone.l, alone.r);
    let gate = alone.l;
    let z = vec![0.0f32; BLOCKS * B];
    let cases = [
        (
            "all",
            r#"{"format":"fake","fake":{"synth":"1","inputs":"0"}}"#,
            Out {
                l: gate.clone(),
                r: gate.clone(),
            },
        ),
        (
            "ch0",
            r#"{"format":"fake","fake":{"synth":"1","inputs":"0","write":"ch0"}}"#,
            Out {
                l: gate.clone(),
                r: z.clone(),
            },
        ),
        (
            "ch0-mono-out",
            r#"{"format":"fake","fake":{"synth":"1","inputs":"0","outputs":"1","write":"ch0"}}"#,
            Out {
                l: gate.clone(),
                r: gate.clone(),
            },
        ),
        (
            "add",
            r#"{"format":"fake","fake":{"synth":"1","inputs":"0","write":"add"}}"#,
            Out {
                l: gate.clone(),
                r: gate.clone(),
            },
        ),
        (
            "none",
            r#"{"format":"fake","fake":{"synth":"1","inputs":"0","write":"none"}}"#,
            Out::zeros(),
        ),
    ];
    for (name, xml, want) in cases {
        let got = run(&format!("synth-{name}"), &[xml], true, &[]);
        assert_out(name, &got, &want);
    }
}

/// A disabled slot with latency is still called (its output discarded) and outputs its bypass signal: the input
/// delayed by its latency, written to the bypass line before the rule zeroes anything, so the full stereo input.
#[test]
fn the_bypass_line_keeps_the_full_input() {
    if lab_plugins::absent("the_bypass_line_keeps_the_full_input") {
        return;
    }
    let want = run(
        "delayed",
        &[r#"{"format":"fake","fake":{"delay":"16"}}"#],
        true,
        &[],
    );
    assert!(want.l.iter().any(|x| *x != 0.0), "it sounds");
    let got = run(
        "bypassed-in0",
        &[r#"{"format":"fake","fake":{"inputs":"0","outputs":"1","delay":"16"}}"#],
        true,
        &[false],
    );
    assert_out("a disabled in0/out1 slot at latency 16", &got, &want);
}

/// The lab's 0-input synths, VST3 and AU, at the head of an effect chain fed the input: the same bits as the same
/// synth with no input (an instrument chain).
#[test]
fn the_lab_vst3_and_au_synths_fed_input_render_as_without_it() {
    if lab_plugins::absent("the_lab_vst3_and_au_synths_fed_input_render_as_without_it") {
        return;
    }
    let au = au_ref(
        &lab_plugins::dir()
            .join("au/summoner-lab-synth.component")
            .display()
            .to_string(),
        b"aumu",
        b"SLsy",
        b"SaSL",
        "Summoner Lab Synth",
    );
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414202, 0x50, 0].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    let vst3 = vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-synth.vst3"),
        &cid,
        "Summoner Lab Synth",
    );
    for (name, xml) in [("vst3", vst3), ("au", au)] {
        let alone = run(&format!("{name}-alone"), &[&xml], false, &[]);
        assert!(alone.l.iter().any(|x| *x != 0.0), "{name}: it sounds");
        let fed = run(&format!("{name}-fed"), &[&xml], true, &[]);
        assert_out(&format!("{name} fed input"), &fed, &alone);
    }
}
