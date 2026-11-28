//! An AU slot's [`summoner_proc::Dsp::io`] is its prepared processor's channels: the effective input count (0 for a
//! unit without an input bus, a music device's included; never the requested `ch_in`) and the output count. A music
//! device WITH an input bus reports and hears its inputs (the unit's input elements are counted whatever its type).
//! The lab's own bundles only (through `Bundle::check`), as `au_load.rs`.

#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
use summoner_format_au::desc::Description;
use summoner_proc::{Io, Plugin};
use summoner_sys::bundle::{Format, allowed_root};

/// A lab bundle's one entry by its codes (protocol 4.0: an AU slot names its entry; `testkit/plugins/CMakeLists.txt`).
/// The tests of this file run one at a time: each loads AUs, and two loads at once in one process trip the image
/// check (one test's newly loaded images are foreign to the other's post-load diff), and the first load's cold start
/// of the system's AU registration races a second one. Serialised here rather than one process per test: the gate
/// stays exact, and the race is never a skip.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn lab(component: &str) -> Description {
    let c = |b: &[u8; 4]| u32::from_be_bytes(*b);
    let codes = match component {
        "summoner-lab-gain.component" => [c(b"aufx"), c(b"SLgn"), c(b"SaSL")],
        "summoner-lab-synth.component" => [c(b"aumu"), c(b"SLsy"), c(b"SaSL")],
        "summoner-lab-synthin.component" => [c(b"aumu"), c(b"SLsi"), c(b"SaSL")],
        "summoner-lab-synthinstrict.component" => [c(b"aumu"), c(b"SLsx"), c(b"SaSL")],
        "summoner-lab-synthinchange.component" => [c(b"aumu"), c(b"SLsc"), c(b"SaSL")],
        other => panic!("no codes for {other}"),
    };
    Description::new(
        String::new(),
        allowed_root(Format::Au)
            .unwrap()
            .join(component)
            .display()
            .to_string(),
        codes,
    )
}

fn io_of(component: &str, ch_in: u32, ch_out: u32) -> Io {
    io_and_render(component, ch_in, ch_out, &[]).0
}

/// The prepared io, one 64-frame block of `input` (one line per block channel) rendered in place, and whether the
/// prepare was an `inputsRefused` retry.
fn io_and_render(
    component: &str,
    ch_in: u32,
    ch_out: u32,
    input: &[Vec<f32>],
) -> (Io, Vec<Vec<f32>>, bool) {
    let p = summoner_format_au::load(&lab(component), ch_in, ch_out).expect("the lab bundle loads");
    let mut dsp = p.prepare(48_000.0, 512);
    let io = dsp.io();
    let mut ch = input.to_vec();
    if !ch.is_empty() {
        let t = summoner_proc::Transport::default();
        let ok = summoner_proc::with_channels(&mut ch, |channels| {
            dsp.process(&mut summoner_proc::Block {
                frames: 64,
                channels,
                midi: &[],
                transport: &t,
            })
        });
        assert!(ok, "{component}: the block failed");
    }
    if let Err(bad) = summoner_sys::bundle::process_images_clean() {
        panic!("non-lab, non-system images in the process: {bad:?}");
    }
    assert_eq!(
        p.input_lines(),
        io.inputs,
        "{component}: the wired input lines are the prepared inputs"
    );
    // The bridge's carrier names the retry (`Plugin::notices`, the slot's `slotNotices` after a prepare).
    let want: Vec<String> = if p.inputs_refused() {
        vec!["inputsRefused".into()]
    } else {
        Vec::new()
    };
    assert_eq!(p.notices(), want, "{component}: the slot's notices");
    (io, ch, p.inputs_refused())
}

/// One test, so the two bundles load one after the other (two loads at once in one process trip the image check).
#[test]
fn each_lab_au_reports_its_prepared_channels() {
    if lab_plugins::absent("each_lab_au_reports_its_prepared_channels") {
        return;
    }
    let _serial = serial();
    assert_eq!(io_of("summoner-lab-gain.component", 2, 2), Io::STEREO);
    assert_eq!(
        io_of("summoner-lab-gain.component", 1, 1),
        Io::new(1, 1),
        "a mono effect (the lab gain takes any N to N)"
    );
    assert_eq!(
        io_of("summoner-lab-synth.component", 2, 2),
        Io::new(0, 2),
        "a music device without an input bus reports no inputs even when offered stereo"
    );
    // The music device with an input element takes the chain's stereo, and hears it (its output is the
    // synth's, silent without a note, plus what it pulled).
    let l: Vec<f32> = (0..64).map(|i| i as f32 / 64.0 - 0.5).collect();
    let r: Vec<f32> = l.iter().map(|x| -0.5 * x).collect();
    let (io, out, refused) = io_and_render(
        "summoner-lab-synthin.component",
        2,
        2,
        &[l.clone(), r.clone()],
    );
    assert_eq!(
        io,
        Io::STEREO,
        "an aumu with an input bus reports its inputs"
    );
    assert!(!refused, "it took the inputs: no inputsRefused");
    assert!(
        out[0] == l && out[1] == r,
        "an aumu with an input bus hears the block: {:?} {:?}",
        &out[0][..4],
        &out[1][..4]
    );
    // An instrument AU with an input element that refuses a stereo input format is prepared
    // again with no inputs and plays (its block succeeds: never a silent THREW), 0 in / 2 out, named inputsRefused.
    let (io, out, refused) = io_and_render(
        "summoner-lab-synthinstrict.component",
        2,
        2,
        &[l.clone(), r.clone()],
    );
    assert_eq!(io, Io::new(0, 2), "the strict aumu falls back to no inputs");
    assert!(refused, "the fallback is named inputsRefused");
    assert!(
        out[0].iter().chain(&out[1]).all(|x| *x == 0.0),
        "deaf to the block (no note: silent): {:?}",
        &out[0][..4]
    );
}

#[test]
fn inputs_and_notices_follow_each_prepared_processor() {
    if lab_plugins::absent("inputs_and_notices_follow_each_prepared_processor") {
        return;
    }
    let _serial = serial();
    let p = summoner_format_au::load(&lab("summoner-lab-synthinchange.component"), 2, 2).unwrap();
    for refuses in [false, true, false, true] {
        p.host_set(0, if refuses { 1.0 } else { 0.0 }).unwrap();
        let mut dsp = p.prepare(48_000.0, 512);
        assert_eq!(
            dsp.io().inputs,
            if refuses { 0 } else { 2 },
            "actual prepared inputs"
        );
        assert_eq!(p.inputs_refused(), refuses);
        assert_eq!(
            p.notices(),
            if refuses {
                vec!["inputsRefused".to_string()]
            } else {
                vec![]
            }
        );
        let mut audio = vec![vec![0.25; 64], vec![0.5; 64]];
        let transport = summoner_proc::Transport::default();
        assert!(summoner_proc::with_channels(&mut audio, |channels| dsp
            .process(&mut summoner_proc::Block {
                frames: 64,
                channels,
                midi: &[],
                transport: &transport,
            })));
        assert_eq!(
            audio[0][0],
            if refuses { 0.0 } else { 0.25 },
            "processor hears only accepted inputs"
        );
        drop(dsp);
    }
}
