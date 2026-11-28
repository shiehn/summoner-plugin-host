//! A disabled slot stays loaded and outputs
//! its bypass signal (an effect's input delayed by its latency, an instrument's silence) after a raised-cosine fade of
//! `handover::FADE` frames; enabling fades the plugin back in. The chain's latency does not move. A disabled
//! plugin: with a latency above 0 the plugin is still called every
//! block (its output discarded), at latency 0 it is not called once the fade is over.
//!
//! The toggle: the wire (protocol 3.2: `chain.load`'s `SlotDesc.enabled` through `ChainSpec.enabled`, and
//! `slot.setEnabled` through the host application's `Command::SetEnabled`), or the test-hooks flag `--test-slot-enabled
//! <slot>:<on|off>@<block>` (the chain thread sets the flag at the top of that processed block; `@0` at load), which
//! places a toggle at a known block. Each scenario runs in a fresh child, all with the same input, and the runs are
//! compared block by block, bit for bit.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive, Input};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_plugin::handover::{FADE, Fade};
use summoner_proto::layout::Geometry;

const B: usize = 64;
const KEY: u64 = 53;
const BLOCKS: usize = 120;
/// The blocks a fade spans (FADE = 144 frames: 2.25 blocks of 64).
const FADE_BLOCKS: usize = (FADE as usize).div_ceil(B);

fn settle(link: &Arc<summoner_rt::rt::ChainLink>) {
    let until = Instant::now() + Duration::from_secs(5);
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        assert!(Instant::now() < until, "the chain never acked");
        std::thread::yield_now();
    }
}

struct Run {
    /// The left channel, block by block.
    blocks: Vec<Vec<f32>>,
    /// Each slot's process calls (`--counters`).
    calls: Vec<u64>,
    /// `chain.loaded`'s latency (the slots' sum).
    latency: u32,
}

/// One fresh child with `slots` (an effect chain with noise in, or an instrument with `events`), `toggles` as
/// `--test-slot-enabled` (empty: none), `BLOCKS` blocks; `extra` more child arguments.
fn run(
    name: &str,
    slots: &[&str],
    fx: bool,
    events: Vec<summoner_rt::rt::MidiEvent>,
    toggles: &str,
    extra: &[&str],
    inserted: &[std::path::PathBuf],
) -> (Run, String) {
    run_wire(name, slots, fx, events, toggles, extra, inserted, &[], &[])
}

/// [`run`], with the wire: `enabled` as `chain.load`'s (`ChainSpec.enabled`) and `wire` as `slot.setEnabled`s, each
/// `(block, slot, enabled)` sent before that block.
#[allow(clippy::too_many_arguments)]
fn run_wire(
    name: &str,
    slots: &[&str],
    fx: bool,
    events: Vec<summoner_rt::rt::MidiEvent>,
    toggles: &str,
    extra: &[&str],
    inserted: &[std::path::PathBuf],
    enabled: &[bool],
    wire: &[(usize, u32, bool)],
) -> (Run, String) {
    let dir = scratch(&format!("{name}-counters"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("counters.txt");
    let mut args: Vec<std::ffi::OsString> = vec!["--counters".into(), path.clone().into()];
    if !toggles.is_empty() {
        args.push("--test-slot-enabled".into());
        args.push(toggles.into());
    }
    args.extend(extra.iter().map(Into::into));
    let mut rig = if inserted.is_empty() {
        Rig::start_with(name, args)
    } else {
        Rig::start_inserting(name, &child_bin(), args, inserted)
    };
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
    let latency = got.iter().map(|s| s.latency).sum();
    let mut eng = Drive::new(
        if fx {
            noise_repeating(4_800, BLOCKS * B)
        } else {
            Input::None
        },
        vec![Bridged {
            link: link.clone(),
            events: Arc::from(events),
            fx,
            params: Vec::new(),
        }],
        B,
    );
    let mut blocks = Vec::with_capacity(BLOCKS);
    for k in 0..BLOCKS {
        for &(at, slot, on) in wire {
            if at == k {
                rig.send(Command::SetEnabled {
                    key: KEY,
                    slot,
                    enabled: on,
                });
            }
        }
        blocks.push(eng.block());
        settle(&link);
    }
    drop(rig); // shutdown: the child writes its counters
    let text = std::fs::read_to_string(&path).expect("the counters");
    let calls = (0..slots.len())
        .map(|i| {
            let line = text
                .lines()
                .find(|l| l.starts_with(&format!("chain {KEY} slot {i} ")))
                .unwrap_or_else(|| panic!("{name}: no slot {i} line:\n{text}"));
            line.rsplit_once(" calls ")
                .and_then(|(_, n)| n.trim().parse().ok())
                .unwrap_or_else(|| panic!("{name}: no calls in {line:?}"))
        })
        .collect();
    (
        Run {
            blocks,
            calls,
            latency,
        },
        text,
    )
}

/// The first block where `a` and `b` differ (bit for bit).
fn first_diff(a: &[Vec<f32>], b: &[Vec<f32>]) -> Option<usize> {
    (0..a.len()).find(|&k| bits(&a[k]) != bits(&b[k]))
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

/// The fade's output at each frame of blocks `from..from + FADE_BLOCKS`: `Mix::apply`'s, from `y` (the plugin's
/// output) to `b` (the bypass signal), `dir` +1 (disabling) or -1 (enabling).
fn faded(y: &[Vec<f32>], b: &[Vec<f32>], from: usize, dir: i32) -> Vec<Vec<f32>> {
    let fade = Fade::default();
    (from..from + FADE_BLOCKS)
        .map(|k| {
            (0..B)
                .map(|i| {
                    let n = ((k - from) * B + i).min(FADE as usize) as u32;
                    let xf = if dir > 0 { n } else { FADE - n };
                    let (x, bv) = (y[k][i], b[k][i]);
                    if xf == 0 {
                        x
                    } else if xf == FADE {
                        bv
                    } else {
                        let g = fade.g(xf);
                        x * (1.0 - g) + bv * g
                    }
                })
                .collect()
        })
        .collect()
}

fn assert_blocks(what: &str, got: &[Vec<f32>], want: &[Vec<f32>]) {
    assert_eq!(got.len(), want.len(), "{what}: block count");
    for (k, (g, w)) in got.iter().zip(want).enumerate() {
        assert_eq!(bits(g), bits(w), "{what}: block {k} of the range");
    }
}

const SOURCE: &str = r#"{"format":"fake","fake":{"name":"src","gain":"0.5"}}"#;

/// An effect with latency (a delay of 100 frames, gain 0.25): disabled, its output is slot 0's delayed by 100 (the
/// chain's latency unchanged), the plugin still called every block; toggled mid-run, a fade of FADE frames each way,
/// and once enabled again the output is the always-enabled run's, bit for bit (its delay line ran on meanwhile).
#[test]
fn a_disabled_effect_with_latency_passes_its_delayed_input_and_is_still_called() {
    if lab_plugins::absent(
        "a_disabled_effect_with_latency_passes_its_delayed_input_and_is_still_called",
    ) {
        return;
    }
    let fx = r#"{"format":"fake","fake":{"name":"fx","gain":"0.25","delay":"100"}}"#;
    let (on, _) = run("en-lat-on", &[SOURCE, fx], true, vec![], "", &[], &[]);
    let (off, _) = run(
        "en-lat-off",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@0",
        &[],
        &[],
    );
    let (tog, _) = run(
        "en-lat-toggle",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@30,1:on@70",
        &[],
        &[],
    );
    assert_eq!(on.latency, 100, "the chain's latency");
    assert_eq!(
        off.latency, 100,
        "a disabled slot keeps the chain's latency"
    );
    assert_eq!(tog.latency, 100);
    // Disabled from load: the bypass signal (slot 0's output, delayed by the slot's 100 frames) is 4x the enabled
    // output (gain 0.25, a power of two: exact), from block 0, no fade.
    let n = on.calls[1];
    assert!(n >= BLOCKS as u64 - 2, "the plugin ran ({n} calls)");
    for k in 0..BLOCKS {
        let want: Vec<u32> = on.blocks[k].iter().map(|x| (x * 4.0).to_bits()).collect();
        assert_eq!(bits(&off.blocks[k]), want, "disabled from load: block {k}");
    }
    assert!(
        off.blocks.iter().flatten().any(|x| *x != 0.0),
        "the input reached the output"
    );
    // Latency above 0: still called every block, disabled or not.
    assert_eq!(off.calls, on.calls, "disabled with latency: still called");
    assert_eq!(tog.calls, on.calls, "toggled with latency: still called");
    // The toggle: the always-enabled run up to it, the fade, the bypass signal, the fade back, the enabled run again.
    let t1 = first_diff(&tog.blocks, &on.blocks).expect("the disable changed the output");
    let t2 = t1 + 40;
    assert!(t2 + FADE_BLOCKS < BLOCKS, "the toggle at block {t1}");
    assert_blocks(
        "the fade out",
        &tog.blocks[t1..t1 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t1, 1),
    );
    assert_blocks(
        "disabled",
        &tog.blocks[t1 + FADE_BLOCKS..t2],
        &off.blocks[t1 + FADE_BLOCKS..t2],
    );
    assert_blocks(
        "the fade in",
        &tog.blocks[t2..t2 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t2, -1),
    );
    assert_blocks(
        "enabled again",
        &tog.blocks[t2 + FADE_BLOCKS..],
        &on.blocks[t2 + FADE_BLOCKS..],
    );
}

/// An effect at latency 0 (a feedback echo: it has state): disabled from load it is never called; toggled, it is not
/// called from the first block after the fade out until the enabling block, and the output there is its input.
#[test]
fn a_disabled_effect_at_latency_zero_is_not_called_once_faded_out() {
    if lab_plugins::absent("a_disabled_effect_at_latency_zero_is_not_called_once_faded_out") {
        return;
    }
    let fx = r#"{"format":"fake","fake":{"name":"echo","echo":"50","feedback":"0.5"}}"#;
    let (on, _) = run("en-zero-on", &[SOURCE, fx], true, vec![], "", &[], &[]);
    let (off, _) = run(
        "en-zero-off",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@0",
        &[],
        &[],
    );
    let (tog, _) = run(
        "en-zero-toggle",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@30,1:on@70",
        &[],
        &[],
    );
    assert_eq!((on.latency, off.latency), (0, 0));
    assert_eq!(
        off.calls[1], 0,
        "disabled from load at latency 0: never called"
    );
    assert_eq!(off.calls[0], on.calls[0], "slot 0 runs throughout");
    // Called in every block but the 40 - FADE_BLOCKS between the fade out's end and the enabling block.
    assert_eq!(
        tog.calls[1],
        on.calls[1] - (40 - FADE_BLOCKS as u64),
        "toggled at latency 0"
    );
    let t1 = first_diff(&tog.blocks, &on.blocks).expect("the disable changed the output");
    let t2 = t1 + 40;
    assert_blocks(
        "the fade out",
        &tog.blocks[t1..t1 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t1, 1),
    );
    assert_blocks(
        "disabled: the input",
        &tog.blocks[t1 + FADE_BLOCKS..t2],
        &off.blocks[t1 + FADE_BLOCKS..t2],
    );
}

/// An instrument disabled: silence (its bypass signal), and at latency 0 not called; enabled again it sounds.
#[test]
fn a_disabled_instrument_is_silent() {
    if lab_plugins::absent("a_disabled_instrument_is_silent") {
        return;
    }
    let synth = r#"{"format":"fake","fake":{"name":"synth","synth":"1"}}"#;
    let notes = || vec![summoner_rt::rt::MidiEvent::new(0, [0x90, 60, 100])];
    let (on, _) = run("en-inst-on", &[synth], false, notes(), "", &[], &[]);
    let (off, _) = run("en-inst-off", &[synth], false, notes(), "0:off@0", &[], &[]);
    let (tog, _) = run(
        "en-inst-toggle",
        &[synth],
        false,
        notes(),
        "0:off@30,0:on@70",
        &[],
        &[],
    );
    assert!(
        on.blocks[BLOCKS - 1].iter().all(|x| *x == 0.25),
        "it sounds"
    );
    assert!(
        off.blocks.iter().flatten().all(|x| *x == 0.0),
        "disabled: silence"
    );
    assert_eq!(off.calls[0], 0, "never called");
    let t1 = first_diff(&tog.blocks, &on.blocks).expect("the disable changed the output");
    let t2 = t1 + 40;
    assert_blocks(
        "the fade out",
        &tog.blocks[t1..t1 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t1, 1),
    );
    assert!(
        tog.blocks[t1 + FADE_BLOCKS..t2]
            .iter()
            .flatten()
            .all(|x| *x == 0.0),
        "silence while disabled"
    );
    assert_blocks(
        "enabled again (the note is still held: the plugin keeps its state)",
        &tog.blocks[t2 + FADE_BLOCKS..],
        &on.blocks[t2 + FADE_BLOCKS..],
    );
}

/// A toggle inside the counted first blocks allocates nothing on the chain thread (the no-allocation discipline).
#[test]
fn a_toggle_allocates_nothing_on_the_chain_thread() {
    if lab_plugins::absent("a_toggle_allocates_nothing_on_the_chain_thread") {
        return;
    }
    let counter = lab_plugins::dir().join("libsummoner_lab_malloccount.dylib");
    assert!(counter.is_file(), "tools/build-plugins.sh");
    let fx = r#"{"format":"fake","fake":{"name":"fx","gain":"0.25","delay":"100"}}"#;
    let (_, text) = run(
        "en-noalloc",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@1,1:on@5,0:off@6",
        &["--noalloc-blocks", "10"],
        &[counter],
    );
    assert!(
        text.contains(&format!("chain {KEY} noalloc_blocks 10 allocations 0\n")),
        "{text}"
    );
}

/// A malformed `--test-slot-enabled` is a usage error before fd 3 (exit 2).
#[test]
fn a_malformed_toggle_is_a_usage_error() {
    for bad in ["1", "1:off", "x:off@1", "1:maybe@1", "1:off@-1", "1:off@1,"] {
        let out = std::process::Command::new(child_bin())
            .args(["--test-slot-enabled", bad])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{bad:?}");
    }
}

/// The wire (protocol 3.2): `chain.load` with slot 1 disabled is the test flag's `1:off@0` run, bit for bit, and still
/// called (latency above 0); `slot.setEnabled` toggles it: the fade from the block it lands at, the bypass signal, the
/// fade back and the enabled run again, all bit for bit. The host application sends either to any 4.x child (no
/// per-minor gate).
#[test]
fn the_wire_disables_at_load_and_toggles_a_live_slot() {
    if lab_plugins::absent("the_wire_disables_at_load_and_toggles_a_live_slot") {
        return;
    }
    let fx = r#"{"format":"fake","fake":{"name":"fx","gain":"0.25","delay":"100"}}"#;
    let (on, _) = run("en-wire-on", &[SOURCE, fx], true, vec![], "", &[], &[]);
    let (off, _) = run(
        "en-wire-off",
        &[SOURCE, fx],
        true,
        vec![],
        "1:off@0",
        &[],
        &[],
    );
    let (load, _) = run_wire(
        "en-wire-load",
        &[SOURCE, fx],
        true,
        vec![],
        "",
        &[],
        &[],
        &[true, false],
        &[],
    );
    for k in 0..BLOCKS {
        assert_eq!(
            bits(&load.blocks[k]),
            bits(&off.blocks[k]),
            "chain.load enabled false: block {k}"
        );
    }
    assert_eq!(load.calls, on.calls, "still called (latency 100)");
    let (tog, _) = run_wire(
        "en-wire-toggle",
        &[SOURCE, fx],
        true,
        vec![],
        "",
        &[],
        &[],
        &[],
        &[(30, 1, false), (70, 1, true)],
    );
    let t1 = first_diff(&tog.blocks, &on.blocks).expect("slot.setEnabled false changed the output");
    // A wire toggle lands at whatever block the message thread stores it before (not the test's to place): within
    // the run, before the enable is sent.
    assert!(
        (30..60).contains(&t1),
        "the disable (sent before block 30) landed at block {t1}"
    );
    assert_blocks(
        "the fade out",
        &tog.blocks[t1..t1 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t1, 1),
    );
    // The enable lands somewhere after block 70: the first block from there that is not the bypass signal.
    let t2 = (t1 + FADE_BLOCKS..BLOCKS)
        .find(|&k| bits(&tog.blocks[k]) != bits(&off.blocks[k]))
        .expect("slot.setEnabled true changed the output");
    assert!(
        (70..BLOCKS - FADE_BLOCKS).contains(&t2),
        "the enable (sent before block 70) landed at block {t2}"
    );
    assert_blocks(
        "the fade in",
        &tog.blocks[t2..t2 + FADE_BLOCKS],
        &faded(&on.blocks, &off.blocks, t2, -1),
    );
    assert_blocks(
        "enabled again",
        &tog.blocks[t2 + FADE_BLOCKS..],
        &on.blocks[t2 + FADE_BLOCKS..],
    );
}
