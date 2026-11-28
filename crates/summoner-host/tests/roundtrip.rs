//! The round trip: a class from OUR scan, as the `PluginRef` a slot takes (protocol 4.0: a VST3
//! class by id, an AU entry by path and codes), loads the same class in this child through the summoner supervisor, and
//! plays. Each scan runs in a process of its own, as the scan protocol requires (one bundle per scan process: two scans
//! in one process see each other's images and STOP): this test binary re-executed as `scan_child`, which scans one
//! bundle with the probe's library (`summoner_probe::scan`) and prints its one ref. Lab gain and synth only.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive, Input};
use common::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;
use summoner_rt::rt::MidiEvent;

const B: usize = 64;
const KEY: u64 = 51;

fn plugins() -> PathBuf {
    lab_plugins::dir()
}

/// The scan process: with `SUMMONER_RT_SCAN=<bundle>`, scan it and print its one class's ref; without
/// it (an ordinary test run), nothing.
#[test]
fn scan_child() {
    let Some(bundle) = std::env::var_os("SUMMONER_RT_SCAN") else {
        return;
    };
    let path = Path::new(&bundle);
    let format = if path.extension().is_some_and(|e| e == "vst3") {
        summoner_probe::ScanFormat::Vst3
    } else {
        summoner_probe::ScanFormat::Au
    };
    let scan =
        summoner_probe::Scan::new(format, path.display().to_string()).expect("a bundle path");
    let r = summoner_probe::scan(&scan, &summoner_probe::NoSteps);
    match r.scanned.outcome {
        summoner_probe::ScannedOutcome::Ok if r.scanned.classes.len() == 1 => {
            println!(
                "RT-REF {}",
                serde_json::to_string(&r.probe.refs[0]).unwrap()
            )
        }
        o => panic!("{}: {o:?}", path.display()),
    }
}

/// Our scan of `bundle`, in a process of its own: its one class's ref (JSON).
fn scanned_xml(bundle: &Path) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "scan_child", "--nocapture", "--test-threads=1"])
        .env("SUMMONER_RT_SCAN", bundle)
        .env("SUMMONER_LAB_DEATH", "kill")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}: the scan process: {text}{}",
        bundle.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    text.lines()
        // libtest writes "test scan_child ... " on the same line before the test's own output.
        .find_map(|l| l.split_once("RT-REF ").map(|(_, x)| x))
        .and_then(|x| x.rfind('}').map(|end| &x[..end + 1]))
        .unwrap_or_else(|| panic!("{}: no ref printed: {text}", bundle.display()))
        .to_string()
}

/// Load `xml` as slot 0 of a chain (`fx`: a stereo input), and render 40 blocks: the output's left channel.
fn load_and_play(rig: &mut Rig, xml: &str, fx: bool) -> Vec<f32> {
    let mut sp = spec(&[xml]);
    sp.geometry = Geometry {
        ch_in: if fx { 2 } else { 0 },
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let (link, slots) = match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) {
        Event::ChainReady { link, slots, .. } => (link, slots),
        e => panic!("the scanned ref did not load: {e:?}"),
    };
    assert!(slots[0].ok, "slot 0 hosted: {slots:?}");
    let events: Vec<MidiEvent> = if fx {
        vec![]
    } else {
        vec![MidiEvent::new(4 * B as u64, [0x90, 69, 100])]
    };
    let mut eng = Drive::new(
        if fx { noise(48_000) } else { Input::None },
        vec![Bridged {
            link: link.clone(),
            events: Arc::from(events),
            fx,
            params: Vec::new(),
        }],
        B,
    );
    let mut y = Vec::new();
    for _ in 0..40 {
        y.extend(eng.block());
        let until = Instant::now() + Duration::from_secs(5);
        let r = &link.region;
        while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
            assert!(Instant::now() < until, "the chain never acked");
            std::thread::yield_now();
        }
    }
    y
}

#[test]
fn a_scanned_vst3_gain_ref_loads_and_plays_the_input() {
    if lab_plugins::absent("a_scanned_vst3_gain_ref_loads_and_plays_the_input") {
        return;
    }
    let xml = scanned_xml(&plugins().join("vst3/summoner-lab-gain.vst3"));
    let mut rig = Rig::start("roundtrip-vst3-gain");
    let y = load_and_play(&mut rig, &xml, true);
    // The gain at its default (1.0) plays the noise: not silent, and every sample finite.
    assert!(
        y[16 * B..].iter().any(|&x| x != 0.0),
        "the input plays through"
    );
    assert!(y.iter().all(|x| x.is_finite()));
    rig.recorder_clean_for("summoner-lab-gain.vst3");
}

#[test]
fn a_scanned_vst3_synth_ref_loads_and_sounds_its_note() {
    if lab_plugins::absent("a_scanned_vst3_synth_ref_loads_and_sounds_its_note") {
        return;
    }
    let xml = scanned_xml(&plugins().join("vst3/summoner-lab-synth.vst3"));
    let mut rig = Rig::start("roundtrip-vst3-synth");
    let y = load_and_play(&mut rig, &xml, false);
    assert!(
        y[..4 * B].iter().all(|&x| x == 0.0),
        "silent before the note"
    );
    assert!(y[8 * B..].iter().any(|&x| x != 0.0), "the note sounds");
    rig.recorder_clean_for("summoner-lab-synth.vst3");
}

#[test]
fn a_scanned_au_gain_ref_loads_and_plays_the_input() {
    if lab_plugins::absent("a_scanned_au_gain_ref_loads_and_plays_the_input") {
        return;
    }
    let bundle = plugins().join("au/summoner-lab-gain.component");
    let xml = scanned_xml(&bundle);
    let mut rig = Rig::start("roundtrip-au-gain");
    let y = load_and_play(&mut rig, &xml, true);
    assert!(
        y[16 * B..].iter().any(|&x| x != 0.0),
        "the input plays through"
    );
    assert!(y.iter().all(|x| x.is_finite()));
    rig.au_recorder_clean_for("summoner-lab-gain.component");
}

#[test]
fn a_scanned_au_synth_ref_loads_and_sounds_its_note() {
    if lab_plugins::absent("a_scanned_au_synth_ref_loads_and_sounds_its_note") {
        return;
    }
    let bundle = plugins().join("au/summoner-lab-synth.component");
    let xml = scanned_xml(&bundle);
    let mut rig = Rig::start("roundtrip-au-synth");
    let y = load_and_play(&mut rig, &xml, false);
    assert!(
        y[..4 * B].iter().all(|&x| x == 0.0),
        "silent before the note"
    );
    assert!(y[8 * B..].iter().any(|&x| x != 0.0), "the note sounds");
    rig.au_recorder_clean_for("summoner-lab-synth.component");
}
