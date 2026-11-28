//! `summoner-format-au`'s resolution against the lab's hostile AU bundles (8 and 2 AudioComponents entries): a slot is
//! a bundle path and the entry's three codes (protocol 4.0's `PluginRef`); a repeated parameter id's values are
//! dropped.
//!
//! **Not gated** (the first AU load was proven clean): the lab's own bundles only (through `Bundle::check`),
//! the image list asserted after each test, no wall-clock bound.

#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
use summoner_format_au::desc::Description;
use summoner_proc::Plugin;
use summoner_sys::bundle::{Format, allowed_root};

/// The tests of this file run one at a time: each loads AUs, and two loads at once in one process trip the image
/// check (one test's newly loaded images are foreign to the other's post-load diff), and the first load's cold start
/// of the system's AU registration races a second one. Serialised here rather than one process per test: the gate
/// stays exact, and the race is never a skip.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn hostile_path() -> String {
    allowed_root(Format::Au)
        .unwrap()
        .join("summoner-lab-hostile.component")
        .display()
        .to_string()
}

fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

/// The codes of a hostile entry (`aufx <sub> SaSL`).
fn hostile(sub: &[u8; 4]) -> [u32; 3] {
    [fourcc(b"aufx"), fourcc(sub), fourcc(b"SaSL")]
}

/// A slot by path and the entry's codes, as the host application's ref gives it.
fn by_codes(file: String, codes: [u32; 3]) -> Description {
    Description::new(String::new(), file, codes)
}

/// Every image in the process is the test binary, a lab bundle, the system's own, or an allowed recorder.
fn images_clean() {
    if let Err(bad) = summoner_sys::bundle::process_images_clean() {
        panic!("non-lab, non-system images in the process: {bad:?}");
    }
}

#[test]
fn codes_name_their_entry_or_nothing() {
    if lab_plugins::absent("codes_name_their_entry_or_nothing") {
        return;
    }
    let _serial = serial();
    // Codes the bundle has: that entry (the liar, not entry [0]).
    let liar = summoner_format_au::load(&by_codes(hostile_path(), hostile(b"SLh7")), 2, 2)
        .expect("the liar by its codes");
    assert_eq!(liar.name(), "Summoner Lab Hostile (77 parameters)");
    // Codes it does not have: a not-ok slot, never [0].
    let missing = summoner_format_au::load(&by_codes(hostile_path(), hostile(b"Zzzz")), 2, 2)
        .err()
        .expect("refused");
    assert!(
        missing.as_str().contains("no AudioComponents entry"),
        "{missing}"
    );
    images_clean();
}

#[test]
fn a_liars_repeated_ids_are_reported_but_never_drive_the_real_parameter() {
    if lab_plugins::absent("a_liars_repeated_ids_are_reported_but_never_drive_the_real_parameter") {
        return;
    }
    let _serial = serial();
    let liar = summoner_format_au::load(&by_codes(hostile_path(), hostile(b"SLh7")), 2, 2)
        .expect("the liar");
    // The ids whose info succeeds, repeats included (5 real + 5 repeats of id 0).
    assert_eq!(liar.param_count(), 10);
    assert_eq!(liar.param_name(5), "Gain", "a repeat of id 0 is named");
    let _dsp = liar.prepare(48000.0, 512);
    liar.set_param(0, 0.5);
    assert_eq!(liar.dropped_params(), 0, "the real index drives");
    liar.set_param(5, 0.0);
    assert_eq!(
        liar.dropped_params(),
        1,
        "a repeat's value is dropped and counted"
    );
    images_clean();
}

fn multi_path() -> String {
    allowed_root(Format::Au)
        .unwrap()
        .join("summoner-lab-multi.component")
        .display()
        .to_string()
}

/// Each loadable variant of the hostile multi-entry bundle is the one its codes name (the crash variants are never
/// instantiated in a test process).
#[test]
fn each_hostile_variant_loads_exactly_by_its_codes() {
    if lab_plugins::absent("each_hostile_variant_loads_exactly_by_its_codes") {
        return;
    }
    let _serial = serial();
    for (sub, name, params) in [
        (b"SLho", "Summoner Lab Hostile", None),
        (b"SLh7", "Summoner Lab Hostile (77 parameters)", Some(10)),
        (b"SLh5", "Summoner Lab Hostile (refuses listeners)", None),
    ] {
        let p = summoner_format_au::load(&by_codes(hostile_path(), hostile(sub)), 2, 2)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(p.name(), name);
        if let Some(n) = params {
            assert_eq!(p.param_count(), n);
        }
    }
    images_clean();
}

/// The multi fixture's two entries share their XOR of the codes; the codes tell them apart.
#[test]
fn colliding_xors_are_told_apart_by_the_codes() {
    if lab_plugins::absent("colliding_xors_are_told_apart_by_the_codes") {
        return;
    }
    let _serial = serial();
    let a = [fourcc(b"aufx"), fourcc(b"SLxa"), fourcc(b"SaSL")];
    let b = [fourcc(b"aufx"), fourcc(b"SLx`"), fourcc(b"SaSM")];
    assert_eq!(
        a[0] ^ a[1] ^ a[2],
        b[0] ^ b[1] ^ b[2],
        "the fixture's point: one XOR"
    );
    let pa = summoner_format_au::load(&by_codes(multi_path(), a), 2, 2).expect("A");
    let pb = summoner_format_au::load(&by_codes(multi_path(), b), 2, 2).expect("B");
    assert_eq!(pa.name(), "Summoner Lab Multi A");
    assert_eq!(pb.name(), "Summoner Lab Multi B (77 parameters)");
    assert_ne!(pa.param_count(), pb.param_count());
    images_clean();
}

/// Codes the bundle does not have are refused (never the first entry, never an entry with the same XOR); codes that
/// fail the code check are refused before the bundle is opened.
#[test]
fn wrong_or_f4_failing_codes_are_refused() {
    if lab_plugins::absent("wrong_or_f4_failing_codes_are_refused") {
        return;
    }
    let _serial = serial();
    let a = [fourcc(b"aufx"), fourcc(b"SLxa"), fourcc(b"SaSL")];
    // The XOR of A (and B), but neither entry's codes (SLxc / SaSN).
    let wrong = [fourcc(b"aufx"), fourcc(b"SLxa") ^ 2, fourcc(b"SaSL") ^ 2];
    assert_eq!(wrong[0] ^ wrong[1] ^ wrong[2], a[0] ^ a[1] ^ a[2]);
    let e = summoner_format_au::load(&by_codes(multi_path(), wrong), 2, 2)
        .err()
        .expect("refused");
    assert!(
        e.as_str()
            .contains("no AudioComponents entry 61756678-534c7863-5361534e"),
        "{e}"
    );
    // Checked before anything is opened: a path that is no lab bundle still gets the code check's refusal.
    let e = summoner_format_au::load(
        &by_codes(
            "/nonexistent/x.component".into(),
            [fourcc(b"aufx"), 0, fourcc(b"SaSL")],
        ),
        2,
        2,
    )
    .err()
    .expect("refused");
    assert!(e.as_str().contains("subtype code is zero"), "{e}");
    images_clean();
}

/// A triple that more than one entry of a bundle carries is refused (never the first of them); one match
/// is its index; none is "no AudioComponents entry".
#[test]
fn a_triple_two_entries_share_is_refused_as_ambiguous() {
    if lab_plugins::absent("a_triple_two_entries_share_is_refused_as_ambiguous") {
        return;
    }
    let _serial = serial();
    let a = [fourcc(b"aufx"), fourcc(b"SLxa"), fourcc(b"SaSL")];
    let b = [fourcc(b"aufx"), fourcc(b"SLx`"), fourcc(b"SaSM")];
    assert_eq!(summoner_format_au::pick_by_triple(&[a, b], b), Ok(1));
    let e = summoner_format_au::pick_by_triple(&[b, a, a], a).expect_err("ambiguous");
    assert!(
        e.as_str().contains("entries 1 and 2") && e.as_str().contains("ambiguous"),
        "{e}"
    );
    let e = summoner_format_au::pick_by_triple(&[a], b).expect_err("none");
    assert!(
        e.as_str()
            .starts_with("no AudioComponents entry 61756678-534c7860-5361534d"),
        "{e}"
    );
}

/// A lab AU parameter flagged
/// `kAudioUnitParameterFlag_NonRealTime` is reported not automatable, and only it. The lab params AU's "Offline"
/// (index 3) carries the flag (the fixture sets `kAudioUnitParameterFlag_NonRealTime`, `1 << 24` in AudioToolbox's
/// header); the description is read through the format crate's public `Plugin::param_meta`.
#[test]
fn a_non_realtime_parameter_is_not_automatable() {
    if lab_plugins::absent("a_non_realtime_parameter_is_not_automatable") {
        return;
    }
    let _serial = serial();
    let path = allowed_root(Format::Au)
        .unwrap()
        .join("summoner-lab-params.component")
        .display()
        .to_string();
    let p = summoner_format_au::load(
        &by_codes(path, [fourcc(b"aufx"), fourcc(b"SLpm"), fourcc(b"SaSL")]),
        2,
        2,
    )
    .expect("the lab params AU");
    assert_eq!(p.param_count(), 8);
    assert_eq!(p.param_name(3), "Offline");
    let automatable: Vec<bool> = (0..8).map(|i| p.param_meta(i).automatable).collect();
    assert_eq!(
        automatable,
        [true, true, true, false, true, true, true, true]
    );
    images_clean();
}
