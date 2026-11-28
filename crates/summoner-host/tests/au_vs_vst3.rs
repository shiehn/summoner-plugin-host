//! The AU v2 path at partial blocks, other rates and bigger blocks. The lab's unregistered AUs have no second
//! implementation to compare against; the stand-in: the lab AU gain and the lab VST3 gain run the SAME DSP core, and
//! the VST3 path is held identical by the host application's VST3 null test (at these same shapes). So through
//! this child, with the null harness, the AU gain must render the VST3 gain's exact bits for every shape: if
//! `AudioUnitRender` with fewer frames than the maximum, another rate, or a 256-frame block were wrong, they would
//! differ.
#![forbid(unsafe_code)]

mod common;

use common::null_harness::*;
use common::*;

fn au_gain_xml() -> String {
    au_ref(
        &lab_plugins::dir()
            .join("au/summoner-lab-gain.component")
            .display()
            .to_string(),
        b"aufx",
        b"SLgn",
        b"SaSL",
        "Summoner Lab Gain",
    )
}

fn au_synth_xml() -> String {
    au_ref(
        &lab_plugins::dir()
            .join("au/summoner-lab-synth.component")
            .display()
            .to_string(),
        b"aumu",
        b"SLsy",
        b"SaSL",
        "Summoner Lab Synth",
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

/// A block shape: (name, rate, block, frames pattern).
type Shape = (&'static str, Option<f64>, Option<usize>, Vec<usize>);

fn shapes() -> Vec<Shape> {
    vec![
        ("full-64", None, None, vec![]),
        ("partial-64-37-1-50", None, None, vec![64, 37, 1, 50]),
        ("block-256", None, Some(256), vec![]),
        ("partial-256-100-3", None, Some(256), vec![256, 100, 3]),
        ("rate-44100", Some(44_100.0), None, vec![]),
        ("rate-96000-partial", Some(96_000.0), None, vec![64, 17]),
    ]
}

#[test]
fn the_au_gain_renders_the_vst3_gains_bits_at_every_block_shape() {
    if lab_plugins::absent("the_au_gain_renders_the_vst3_gains_bits_at_every_block_shape") {
        return;
    }
    let (au, v3) = (au_gain_xml(), gain_xml());
    let mut checked = 0;
    for (name, rate, block, frames) in shapes() {
        let mk = |xml: &str, which: &str| {
            let mut s = fx(&format!("au-vs-vst3-{name}-{which}"), &[xml]);
            s.rate = rate;
            s.block = block;
            s.frames = frames.clone();
            s.params = vec![0];
            s.values = vec![0.5];
            s.changes = vec![(7, 0, 0.25), (15, 0, 1.25)];
            s
        };
        let (a, b) = (
            run(&child_bin(), &mk(&au, "au")),
            run(&child_bin(), &mk(&v3, "vst3")),
        );
        assert!(a.out.iter().any(|&x| x != 0), "{name}: the AU plays");
        assert_eq!(
            a.out.len(),
            b.out.len(),
            "{name}: the same number of frames"
        );
        let first = a.out.iter().zip(&b.out).position(|(x, y)| x != y);
        assert!(
            first.is_none(),
            "{name}: the AU gain's bits differ from the VST3 gain's at frame {first:?}"
        );
        checked += 1;
    }
    assert_eq!(checked, shapes().len());
}

/// The synth too: MIDI notes whose offsets land inside partial blocks (a note at a frame a short block must carry),
/// sustain and all-notes-off, at every shape: the AU synth (MusicDeviceMIDIEvent) and the VST3 synth (its event list)
/// must render the same bits.
#[test]
fn the_au_synth_renders_the_vst3_synths_bits_at_every_block_shape() {
    if lab_plugins::absent("the_au_synth_renders_the_vst3_synths_bits_at_every_block_shape") {
        return;
    }
    let (au, v3) = (au_synth_xml(), vst3_synth_xml());
    let note = |frame: u64, b: [u8; 3]| summoner_rt::rt::MidiEvent::new(frame, b);
    for (name, rate, block, frames) in shapes() {
        let mk = |xml: &str, which: &str| {
            let mut s = fx(&format!("au-vs-vst3-synth-{name}-{which}"), &[xml]);
            s.fx = false;
            s.rate = rate;
            s.block = block;
            s.frames = frames.clone();
            s.events = vec![
                note(70, [0x90, 60, 100]),
                note(101, [0x90, 67, 80]),
                note(300, [0xb0, 64, 127]),
                note(420, [0x80, 60, 0]),
                note(700, [0xb0, 64, 0]),
                note(900, [0xb0, 123, 0]),
            ];
            s
        };
        let (a, b) = (
            run(&child_bin(), &mk(&au, "au")),
            run(&child_bin(), &mk(&v3, "vst3")),
        );
        assert!(a.out.iter().any(|&x| x != 0), "{name}: the AU synth sounds");
        let first = a.out.iter().zip(&b.out).position(|(x, y)| x != y);
        assert!(
            first.is_none() && a.out.len() == b.out.len(),
            "{name}: the AU synth's bits differ from the VST3 synth's at frame {first:?}"
        );
    }
}
