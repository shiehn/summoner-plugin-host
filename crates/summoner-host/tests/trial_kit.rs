//! The real-plugin trial kit's stage 2: every description the scan (stage 1,
//! the gated scan's driver) answered `ok`, one at a time, each in a fresh shipped child through the summoner supervisor
//! and graph engine: load, render (a test tone into an effect, MIDI into an instrument), a state round trip, the editor
//! opened and closed, unload. One JSON line per plugin into the run's `trial.jsonl`, names hashed unless asked.
//!
//! `#[ignore]`d: run only by the trial kit's driver script, which sets:
//! - `SUMMONER_TRIAL_CHILD`: the shipped child;
//! - `SUMMONER_TRIAL_RESULTS`: stage 1's results file (`results-*.jsonl`, one line per bundle);
//! - `SUMMONER_TRIAL_OUT`: the run's directory (the report and the crash logs go there);
//! - `SUMMONER_TRIAL_SALT`: 32 hex digits, the name hash's key (kept in the run directory, never in the report);
//! - optional `SUMMONER_TRIAL_NAMES=1` (names in the report) and `SUMMONER_TRIAL_EDITORS=0` (no windows).
//!
//! No lab session runs it on real plugins: its dry runs use the lab's own test plugins.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive, Input};
use common::*;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event, SlotState};
use summoner_proto::layout::Geometry;

const B: usize = 64;
const RENDER_BLOCKS: usize = 1125; // 1.5 s at 48 kHz
const KEY: u64 = 1;

/// A mono 440 Hz sine at -12 dBFS, `frames` long, from frame 0 (the effect's test tone).
fn tone(frames: usize) -> Input {
    let amp = 10f32.powf(-12.0 / 20.0);
    Input::Once(
        (0..frames)
            .map(|i| amp * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SR as f32).sin())
            .collect(),
    )
}

/// The instrument's MIDI: C4 at block 0, off at 0.75 s, CC 64 on at 0.25 s and off at 1 s, all-notes-off at the end.
fn midi() -> Vec<summoner_rt::rt::MidiEvent> {
    let at = |s: f64| (s * SR) as u64;
    vec![
        summoner_rt::rt::MidiEvent::new(0, [0x90, 60, 100]),
        summoner_rt::rt::MidiEvent::new(at(0.25), [0xb0, 64, 127]),
        summoner_rt::rt::MidiEvent::new(at(0.75), [0x80, 60, 0]),
        summoner_rt::rt::MidiEvent::new(at(1.0), [0xb0, 64, 0]),
        summoner_rt::rt::MidiEvent::new(at(1.45), [0xb0, 123, 0]),
    ]
}

/// One step's outcome: `"pass"`, `"fail"` or `"skipped"`, its time, and why.
fn step(ok: bool, t: Instant, why: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
        "result": if ok { "pass" } else { "fail" },
        "ms": t.elapsed().as_millis() as u64,
        "why": why.into(),
    })
}

/// One plugin's trial (a fresh child; a panic, a wait past 10 s, a crash: the caller records it).
fn trial(child: &Path, out: &Path, i: usize, d: &ClassSlot, editors: bool) -> serde_json::Value {
    let mut r = serde_json::Map::new();
    let counters = out.join(format!("counters-{i}.txt"));
    let crash = out.join("crash");
    std::fs::create_dir_all(&crash).unwrap();
    let mut rig = Rig::start_child(
        &format!("trial-{i}"),
        child,
        vec![
            "--counters".into(),
            counters.into(),
            "--crash-log".into(),
            crash.canonicalize().unwrap().into(),
        ],
    );
    // The scan's own classification. Not "no inputs": a scan through a moduleinfo.json leaves a VST3's channel
    // counts at 0, which would make every effect that ships a moduleinfo.json an instrument fed no tone (the dry run's
    // lab modinfo gain).
    let instrument = d.instrument;
    // 1. load
    let t = Instant::now();
    let mut sp = spec(&[&d.slot]);
    sp.geometry = Geometry {
        ch_in: if instrument { 0 } else { 2 },
        ..Geometry::stereo(B as u32)
    };
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
        Event::ChainReady { link, slots, .. } if slots[0].ok => {
            r.insert("load".into(), step(true, t, ""));
            r.insert("latency".into(), slots[0].latency.into());
            r.insert("params".into(), slots[0].param_count.into());
            link
        }
        Event::ChainReady { slots, .. } => {
            r.insert(
                "load".into(),
                step(false, t, format!("not hosted: {slots:?}")),
            );
            return serde_json::Value::Object(r);
        }
        e => {
            r.insert("load".into(), step(false, t, format!("{e:?}")));
            return serde_json::Value::Object(r);
        }
    };
    // 2. render
    let t = Instant::now();
    let mut eng = Drive::new(
        if instrument {
            Input::None
        } else {
            tone(RENDER_BLOCKS * B)
        },
        vec![Bridged {
            link: link.clone(),
            events: Arc::from(if instrument { midi() } else { Vec::new() }),
            fx: !instrument,
            params: Vec::new(),
        }],
        B,
    );
    let (mut peak, mut sum, mut n, mut nonfinite, mut late) = (0f32, 0f64, 0usize, 0usize, 0usize);
    for _ in 0..RENDER_BLOCKS {
        let _ = eng.block();
        // Both channels (a plugin may write one only: the dry run's lab FPCR fixture reports on the right).
        for &x in &eng.out {
            if x.is_finite() {
                peak = peak.max(x.abs());
                sum += f64::from(x) * f64::from(x);
            } else {
                nonfinite += 1;
            }
            n += 1;
        }
        if !null_harness::settle(&link, Duration::from_millis(600)) {
            late += 1;
        }
    }
    let rms = (sum / n.max(1) as f64).sqrt();
    let silent = peak < 1e-6;
    let ok = nonfinite == 0 && !silent && late == 0;
    let why = format!("peak {peak:.4} rms {rms:.5} non-finite {nonfinite} unacked {late}");
    r.insert("render".into(), step(ok, t, why));
    // 3. state: get, set it back, get again
    let t = Instant::now();
    rig.send(Command::GetState { key: KEY, slot: 0 });
    let a = match rig.wait(|e| {
        matches!(
            e,
            Event::StateRead {
                key: KEY,
                slot: 0,
                ..
            }
        )
    }) {
        Event::StateRead { blob, .. } => blob.to_vec(),
        _ => unreachable!(),
    };
    rig.send(Command::SetState {
        key: KEY,
        state: SlotState::new(0, 1, Arc::from(a.clone())),
    });
    let applied = match rig.wait(|e| {
        matches!(
            e,
            Event::StateApplied {
                key: KEY,
                generation: 1,
                ..
            }
        )
    }) {
        Event::StateApplied { ok, .. } => ok,
        _ => unreachable!(),
    };
    rig.send(Command::GetState { key: KEY, slot: 0 });
    let b = match rig.wait(|e| {
        matches!(
            e,
            Event::StateRead {
                key: KEY,
                slot: 0,
                ..
            }
        )
    }) {
        Event::StateRead { blob, .. } => blob.to_vec(),
        _ => unreachable!(),
    };
    let why = format!(
        "{} bytes; set {}; {}",
        a.len(),
        if applied { "applied" } else { "refused" },
        if a == b {
            "round-trips".to_string()
        } else {
            format!("re-serialises differently ({} bytes after)", b.len())
        }
    );
    r.insert("state".into(), step(applied && a == b, t, why));
    // 4. editor
    if editors {
        let t = Instant::now();
        rig.send(Command::EditorOpen {
            key: KEY,
            slot: 0,
            guard_reset: false,
        });
        std::thread::sleep(Duration::from_millis(1500));
        rig.send(Command::EditorClose {
            key: KEY,
            slot: Some(0),
        });
        let e = rig.wait(|e| {
            matches!(
                e,
                Event::EditorClosed { key: KEY, .. } | Event::EditorUnavailable { key: KEY, .. }
            )
        });
        let (ok, why) = match e {
            Event::EditorClosed { .. } => (true, "opened and closed"),
            _ => (false, "no editor view"),
        };
        r.insert("editor".into(), step(ok, t, why));
    } else {
        r.insert("editor".into(), serde_json::json!({"result": "skipped"}));
    }
    // 5. unload
    rig.send(Command::Unload { key: KEY });
    drop(rig);
    serde_json::Value::Object(r)
}

#[test]
#[ignore = "the trial kit's stage 2: run only by its driver script"]
fn trial_kit_stage_2() {
    let env = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| panic!("{k}: set by the trial kit's driver script"))
    };
    let child = PathBuf::from(env("SUMMONER_TRIAL_CHILD"));
    let out = PathBuf::from(env("SUMMONER_TRIAL_OUT"));
    let salt = runecraft_log::parse_salt(&env("SUMMONER_TRIAL_SALT")).expect("32 hex digits");
    let names = std::env::var("SUMMONER_TRIAL_NAMES").is_ok_and(|v| v == "1");
    let editors = !std::env::var("SUMMONER_TRIAL_EDITORS").is_ok_and(|v| v == "0");
    let shown = |s: &str| {
        if names {
            s.to_string()
        } else {
            runecraft_log::hash_keyed(&salt, s)
        }
    };
    let results =
        std::fs::read_to_string(env("SUMMONER_TRIAL_RESULTS")).expect("stage 1's results");
    let mut report = std::fs::File::create(out.join("trial.jsonl")).unwrap();
    let mut i = 0usize;
    for line in results.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let scan = &v["end"];
        let outcome = scan["outcome"].as_str().unwrap_or("none");
        let base = serde_json::json!({
            "bundle": shown(v["path"].as_str().unwrap_or("")),
            // The gated scan's driver writes the format's Debug form (`Some(Vst3)`): the report says `Vst3`.
            "format": v["format"]
                .as_str()
                .unwrap_or("")
                .trim_start_matches("Some(")
                .trim_end_matches(')'),
            "arch": v["arch"],
            "scanMs": v["ms"],
            "scan": if v.get("skipped").is_some() { "skipped".into() } else { outcome.to_string() },
        });
        if outcome != "ok" {
            let mut b = base.clone();
            b["scanDetail"] = if v.get("skipped").is_some() {
                v["skipped"].clone()
            } else {
                serde_json::json!(scan)
            };
            writeln!(report, "{b}").unwrap();
            continue;
        }
        // The gated scan's driver writes the scan's end as the supervisor's verdict, whose list keeps the record's key
        // `descriptions` (the 4.0 wire's `scanned` says `classes`); each entry is a raw `ScanClass`. Either key.
        let classes = scan
            .get("descriptions")
            .or_else(|| scan.get("classes"))
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        for dv in classes {
            let Some(d) = class_slot(&dv) else { continue };
            i += 1;
            let mut row = base.clone();
            row["plugin"] = shown(&d.name).into();
            row["vendor"] = shown(&d.vendor).into();
            row["version"] = shown(&d.version).into();
            row["instrument"] = d.instrument.into();
            let t = Instant::now();
            let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                trial(&child, &out, i, &d, editors)
            }));
            match got {
                Ok(steps) => row["steps"] = steps,
                Err(e) => {
                    let why = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    // A wait past 10 s, a crash that downed the chain, or a failed assertion in the driver.
                    row["steps"] =
                        serde_json::json!({"aborted": why.chars().take(500).collect::<String>()});
                }
            }
            row["ms"] = (t.elapsed().as_millis() as u64).into();
            writeln!(report, "{row}").unwrap();
            report.flush().unwrap();
            eprintln!("trial {i}: {}", row["steps"]);
        }
    }
}
