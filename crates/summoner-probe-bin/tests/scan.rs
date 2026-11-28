//! `summoner-scan-probe` over the lab's built bundles, as the summoner supervisor sees a scan process. Each scan is its
//! own process (one bundle per process), run with `SUMMONER_LAB_DEATH=kill` (a hostile death leaves no crash report in the
//! user's DiagnosticReports) and a scratch HOME.
//! - The lab's gain and synth, VST3 and AU, scan `ok`, with the expected description fields and a passing
//!   validation;
//! - the hostile bundles die in discovery (their constructor-crash class), which the supervisor reports `crashed`:
//!   the probe prints nothing and is killed by a signal;
//! - the drag fixture is a STOP (`failed instantiate`, the post-load image check);
//! - a path that is not a lab bundle is `failed notFound`, refused by the lab gate before anything is opened (a
//!   lab-home path: no real plugin folder is named, even lexically).

#[path = "../../../testkit/plugins/fswatch/fswatch_v2.rs"]
mod fswatch_v2;
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn plugins() -> PathBuf {
    lab_plugins::dir()
        .canonicalize()
        .expect("tools/build-plugins.sh")
}

/// The lookups every lab process makes at start-up (the VST3 baseline, transcripts/vst3-lookwatch).
const START_UP: &[&str] = &[
    "com.apple.logd",
    "com.apple.system.notification_center",
    "com.apple.system.opendirectoryd.libinfo",
];
/// An AU scan adds AudioToolbox's registrar (expected) and its CFPreferences read (accepted).
const AU: &[&str] = &[
    "com.apple.audio.AudioComponentRegistrar",
    "com.apple.cfprefsd.agent",
    "com.apple.cfprefsd.daemon",
];
/// A failed CFBundle load builds a LOCALIZED error (CoreFoundation's loctable + ICU data) and reads the language
/// preference through cfprefsd: read-only, accepted (seen in the architecture check's mutant run). Anything else new is
/// a STOP. No test here takes that path yet (the architecture refusal comes before any load): the first one that does
/// uses this.
#[allow(dead_code)]
const LOAD_FAILURE: &[&str] = &["com.apple.cfprefsd.agent", "com.apple.cfprefsd.daemon"];
/// No scan touches a plugin folder (the fswatch recorder's lines; the probe's argument is not a syscall).
const FOLDERS: &[&str] = &["Library/Audio/Plug-Ins", "/System/Library/Components"];

static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// One scan process under the file-access and lookup recorders. Every lookup must be a start-up one or in `allowed`,
/// and no file access may name a plugin folder.
fn run(bundle: &Path, allowed: &[&str], args: &[&str]) -> Output {
    // Canonical (no `..`): the probe writes under this HOME, and a write through `..` is a STOP.
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target.noindex")
        .canonicalize()
        .unwrap();
    let home = base.join("scan-test-home");
    std::fs::create_dir_all(&home).unwrap();
    let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (fs, look) = (
        home.join(format!("fs-{}-{n}.log", std::process::id())),
        home.join(format!("look-{}-{n}.log", std::process::id())),
    );
    let recorders = format!(
        "{}:{}",
        plugins().join("libsummoner_lab_fswatch.dylib").display(),
        plugins().join("libsummoner_lab_lookwatch.dylib").display()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_summoner-scan-probe"))
        .arg(bundle)
        .args(args)
        .env("HOME", &home)
        .env("CFFIXED_USER_HOME", &home)
        .env("SUMMONER_LAB_DEATH", "kill")
        .env("DYLD_INSERT_LIBRARIES", recorders)
        .env("SUMMONER_FSWATCH_LOG", &fs)
        .env("SUMMONER_LOOKWATCH_LOG", &look)
        .output()
        .unwrap();
    let (fs_text, look_text) = (
        fswatch_v2::read_log(&fs),
        // Written at the first lookup: a scan refused before any has none (the ok scans are the positive control).
        std::fs::read_to_string(&look).unwrap_or_default(),
    );
    let _ = (std::fs::remove_file(&fs), std::fs::remove_file(&look));
    if String::from_utf8_lossy(&out.stdout).contains(r#""outcome":"ok""#) {
        assert!(
            !look_text.is_empty() && !fs_text.is_empty(),
            "{}: the recorders saw nothing of an ok scan (not injected?)",
            bundle.display()
        );
    }
    // No probe run ever reaches the WindowServer.
    assert!(
        !look_text.contains("windowserver"),
        "{}: a WindowServer lookup (a STOP):\n{look_text}",
        bundle.display()
    );
    for l in look_text.lines() {
        let service = l.rsplit(' ').next().unwrap_or("");
        assert!(
            START_UP.contains(&service) || allowed.contains(&service),
            "{}: a new lookup (a STOP): {l}",
            bundle.display()
        );
    }
    let recs = fswatch_v2::parse(&fs_text); // strict: an old-format log panics
    let u = fswatch_v2::stops(&recs);
    assert!(
        u.is_empty(),
        "{}: the recorder's STOPs (the shared rules): {u:?}",
        bundle.display()
    );
    for l in fs_text.lines() {
        assert!(
            !FOLDERS.iter().any(|f| l.contains(f)),
            "{}: a plugin folder touched: {l}",
            bundle.display()
        );
    }
    out
}

fn report(bundle: &Path, allowed: &[&str]) -> Value {
    let out = run(bundle, allowed, &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}: {}",
        bundle.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("a JSON report")
}

fn fourcc(s: &str) -> u32 {
    u32::from_be_bytes(s.as_bytes().try_into().unwrap())
}

/// The one class of an `ok` report whose validation passed (protocol 4.0's raw `ScanClass`).
fn one_ok(r: &Value) -> &Value {
    assert_eq!(r["outcome"], "ok", "{r}");
    let d = r["classes"].as_array().expect("classes");
    assert_eq!(d.len(), 1, "{r}");
    let v = r["probe"]["validation"].as_array().unwrap();
    assert!(v.len() == 1 && v[0]["ok"] == true, "{r}");
    assert_eq!(r["probe"]["refs"].as_array().unwrap().len(), 1);
    &d[0]
}

#[test]
fn the_lab_vst3s_scan_ok_with_their_classes() {
    if lab_plugins::absent("the_lab_vst3s_scan_ok_with_their_classes") {
        return;
    }
    for (k, name, categories, cid, ins) in [
        (
            "gain",
            "Summoner Lab Gain",
            serde_json::json!(["Fx"]),
            "5341534c4c4142010000005000000000",
            2,
        ),
        (
            "synth",
            "Summoner Lab Synth",
            serde_json::json!(["Instrument", "Synth"]),
            "5341534c4c4142020000005000000000",
            0,
        ),
    ] {
        let path = plugins().join(format!("vst3/summoner-lab-{k}.vst3"));
        let r = report(&path, &[]);
        assert_eq!(r["format"], "vst3");
        assert_eq!(r["path"], path.display().to_string());
        let d = one_ok(&r);
        assert_eq!(d["format"], "vst3");
        assert_eq!(d["path"], path.display().to_string());
        assert_eq!(d["source"], "factory");
        assert_eq!(d["classId"], cid);
        assert_eq!(d["category"], "Audio Module Class");
        assert_eq!(d["name"], name);
        assert_eq!(d["categories"], categories);
        assert_eq!(d["vendor"], "Summoner Lab");
        assert_eq!(d["version"], "1.0.0");
        assert_eq!(d["instantiated"], true);
        assert_eq!((&d["inputs"], &d["outputs"]), (&ins.into(), &2.into()));
        assert!(d["bundleMtimeMs"].as_i64().unwrap() > 0, "{d}");
        assert_eq!(
            r["probe"]["refs"][0],
            serde_json::json!({"format": "vst3", "path": path.display().to_string(), "classId": cid, "name": name})
        );
    }
}

#[test]
fn the_lab_aus_scan_ok_with_their_classes() {
    if lab_plugins::absent("the_lab_aus_scan_ok_with_their_classes") {
        return;
    }
    for (k, ty, sub, name, ins) in [
        ("gain", "aufx", "SLgn", "Summoner Lab Gain", 2),
        ("synth", "aumu", "SLsy", "Summoner Lab Synth", 0),
    ] {
        let path = plugins().join(format!("au/summoner-lab-{k}.component"));
        let r = report(&path, AU);
        assert_eq!(r["format"], "au");
        let d = one_ok(&r);
        assert_eq!(d["format"], "au");
        assert_eq!(d["path"], path.display().to_string());
        assert_eq!(d["entry"], 0);
        let codes = format!(
            "{:08x}-{:08x}-{:08x}",
            fourcc(ty),
            fourcc(sub),
            fourcc("SaSL")
        );
        assert_eq!(d["auCodes"], codes.as_str());
        assert_eq!(
            d["componentName"],
            format!("Summoner Lab: {name}"),
            "the entry's name, raw"
        );
        assert_eq!((&d["inputs"], &d["outputs"]), (&ins.into(), &2.into()));
        assert!(d.get("refused").is_none(), "{d}");
        assert_eq!(
            r["probe"]["refs"][0],
            serde_json::json!({"format": "au", "path": path.display().to_string(), "auCodes": codes, "name": name})
        );
    }
}

/// An x86_64-only bundle is refused by its Mach-O header, `failed arch`, with ZERO images
/// loaded by the scan (Rosetta is never asked). The fixture is built, never executed. A lab bundle that does load
/// is the control: its scan adds its own executable.
#[test]
fn an_x86_64_only_bundle_is_refused_by_its_header_with_nothing_loaded() {
    if lab_plugins::absent("an_x86_64_only_bundle_is_refused_by_its_header_with_nothing_loaded") {
        return;
    }
    let path = plugins().join("vst3/summoner-lab-x86.vst3");
    let r = report(&path, &[]);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert_eq!(r["reason"], "arch", "{r}");
    assert!(
        r["detail"]
            .as_str()
            .unwrap()
            .contains("x86_64 only, no arm64 slice"),
        "{r}"
    );
    assert_eq!(
        r["discovered"],
        serde_json::json!([]),
        "a failure before discovery has no classes : {r}"
    );
    assert_eq!(
        r["probe"]["loadedImages"],
        serde_json::json!([]),
        "nothing loaded: {r}"
    );
    let gain = plugins().join("vst3/summoner-lab-gain.vst3");
    let c = report(&gain, &[]);
    let exe = gain
        .join("Contents/MacOS/summoner-lab-gain")
        .display()
        .to_string();
    assert!(
        c["probe"]["loadedImages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i == &serde_json::json!(exe)),
        "the control loads its executable: {c}"
    );
}

#[test]
fn a_hostile_bundle_dies_in_its_scan_process_and_prints_nothing() {
    if lab_plugins::absent("a_hostile_bundle_dies_in_its_scan_process_and_prints_nothing") {
        return;
    }
    use std::os::unix::process::ExitStatusExt;
    for b in [
        "vst3/summoner-lab-hostile.vst3",
        "au/summoner-lab-hostile.component",
    ] {
        let out = run(
            &plugins().join(b),
            if b.starts_with("au/") { AU } else { &[] },
            &[],
        );
        assert_eq!(
            out.status.signal(),
            Some(9),
            "{b}: a quiet death (SIGKILL): {:?}",
            out.status
        );
        assert!(out.stdout.is_empty(), "{b}: no report from a dead scan");
    }
}

#[test]
fn the_drag_fixture_is_a_stop_and_a_foreign_path_is_refused_before_it_is_opened() {
    if lab_plugins::absent(
        "the_drag_fixture_is_a_stop_and_a_foreign_path_is_refused_before_it_is_opened",
    ) {
        return;
    }
    let r = report(&plugins().join("au/summoner-lab-drag.component"), AU);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert_eq!(r["reason"], "instantiate", "{r}");
    assert!(r["detail"].as_str().unwrap().contains("STOP"), "{r}");
    // Not a lab bundle: a plugin-folder-shaped path in the LAB's home (never a real plugin folder), outside both of
    // the lab gate's roots, refused lexically (lab_gate's recorder proves the gate makes no syscall for such a path).
    let r = report(
        &Path::new(env!("SUMMONER_DEV_PLUGIN_ROOTS"))
            .join("home/Library/Audio/Plug-Ins/VST3/Nope.vst3"),
        &[],
    );
    assert_eq!(r["outcome"], "failed", "{r}");
    assert_eq!(r["reason"], "notFound", "{r}");
    assert!(
        r["detail"]
            .as_str()
            .unwrap()
            .contains("not a development plugin bundle"),
        "{r}"
    );
    // Not a `scan` the host application could send (codec::Scan::new refuses it): exit 64, nothing scanned, no report.
    let out = run(&plugins().join("vst3/summoner-lab-gain.dll"), &[], &[]);
    assert_eq!(out.status.code(), Some(64), "{out:?}");
    assert!(out.stdout.is_empty());
}

/// The constructor-hang fixture (a bundle of its own, VST3 and AU): the scan never gets past instantiating it, and
/// the probe's own step deadline ends it (exit 75, the step named on stderr, no report): the supervisor's
/// `timeout`, well before its 30 s budget.
#[test]
fn a_constructor_hang_is_ended_by_the_probes_step_deadline() {
    if lab_plugins::absent("a_constructor_hang_is_ended_by_the_probes_step_deadline") {
        return;
    }
    for (b, allowed) in [
        ("vst3/summoner-lab-hang.vst3", &[][..]),
        ("au/summoner-lab-hang.component", AU),
    ] {
        let t0 = std::time::Instant::now();
        let out = run(&plugins().join(b), allowed, &["--step-ms", "800"]);
        let took = t0.elapsed();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(75),
            "{b}: the deadline's exit: {err}"
        );
        assert!(out.stdout.is_empty(), "{b}: no report");
        assert!(err.contains("passed its 800 ms deadline"), "{b}: {err}");
        assert!(
            err.contains("step discover") || err.contains("step instantiate"),
            "{b}: the hang is in creating the class: {err}"
        );
        assert!(
            took >= std::time::Duration::from_millis(800)
                && took < std::time::Duration::from_secs(10),
            "{b}: {took:?}"
        );
    }
}

/// The moduleinfo.json fixtures (copies of the gain, a moduleinfo.json put in by the build): discovery takes a
/// valid one (Contents/Resources/, then Contents/), refuses one the SDK's parser would refuse and falls back to the
/// factory. A moduleinfo.json whose class NAME its factory does not have, with the factory class's id, validates: a
/// slot picks its class by id (the name is for display; before protocol 4.0 a name match failed it as
/// `failed instantiate`).
#[test]
fn moduleinfo_json_is_taken_or_refused_and_the_class_is_picked_by_id() {
    if lab_plugins::absent("moduleinfo_json_is_taken_or_refused_and_the_class_is_picked_by_id") {
        return;
    }
    // Valid: its own version and sub categories; the moduleinfo path reports no channels.
    let r = report(&plugins().join("vst3/summoner-lab-modinfo.vst3"), &[]);
    let d = one_ok(&r);
    assert_eq!(
        (&d["source"], &d["name"], &d["version"], &d["categories"]),
        (
            &"moduleinfo".into(),
            &"Summoner Lab Gain".into(),
            &"1.0.1".into(),
            &serde_json::json!(["Fx", "Tools"])
        ),
        "from moduleinfo.json: {r}"
    );
    assert!(
        d.get("inputs").is_none() && d.get("instantiated").is_none(),
        "{d}"
    );
    // The class id from the moduleinfo.json path: its CID string, lowercased.
    assert_eq!(d["classId"], "5341534c4c4142010000005000000000", "{r}");
    // Refused in both places (a required key missing; not JSON): the factory's class, never the files' names.
    let r = report(&plugins().join("vst3/summoner-lab-badmodinfo.vst3"), &[]);
    let d = one_ok(&r);
    assert!(!r.to_string().contains("MUST NOT APPEAR"), "{r}");
    assert_eq!(
        (&d["source"], &d["name"], &d["version"], &d["categories"]),
        (
            &"factory".into(),
            &"Summoner Lab Gain".into(),
            &"1.0.0".into(),
            &serde_json::json!(["Fx"])
        ),
        "from the factory: {r}"
    );
    assert_eq!((&d["inputs"], &d["outputs"]), (&2.into(), &2.into()));
    // Valid, naming a class its factory does not have by that name, with the factory class's id: validated by id.
    let r = report(
        &plugins().join("vst3/summoner-lab-modinfo-mismatch.vst3"),
        &[],
    );
    let d = one_ok(&r);
    assert_eq!(
        (&d["source"], &d["name"]),
        (
            &"moduleinfo".into(),
            &"Summoner Lab Gain (moduleinfo)".into()
        ),
        "{r}"
    );
}

/// The round-trip fixture (a bundle of its own, VST3 and AU): a class whose getState returns bytes its own setState
/// refuses (its version field flipped). Validation's state step, framed as a state blob and set back whole, catches it:
/// `failed state`, with the format's own refusal in the detail.
#[test]
fn a_state_that_does_not_round_trip_fails_validation() {
    if lab_plugins::absent("a_state_that_does_not_round_trip_fails_validation") {
        return;
    }
    for (b, allowed, refusal) in [
        (
            "vst3/summoner-lab-badstate.vst3",
            &[][..],
            "component: Refused",
        ),
        ("au/summoner-lab-badstate.component", AU, "Status(-10851)"),
    ] {
        let r = report(&plugins().join(b), allowed);
        assert_eq!(
            (&r["outcome"], &r["reason"]),
            (&"failed".into(), &"state".into()),
            "{b}: {r}"
        );
        let detail = r["detail"].as_str().unwrap();
        assert!(
            detail.contains("its own state was refused") && detail.contains(refusal),
            "{b}: {detail}"
        );
        let v = &r["probe"]["validation"][0];
        assert_eq!(
            (&v["ok"], &v["reason"]),
            (&false.into(), &"state".into()),
            "{b}: {r}"
        );
        // A VALIDATION failure keeps discovery's classes in the host's own report (`discovered`); form A's `classes`
        // stays `[]` on a `failed`.
        assert_eq!(r["classes"], serde_json::json!([]), "{b}: form A is frozen");
        let d = r["discovered"].as_array().expect("discovered");
        assert_eq!(d.len(), 1, "{b}: {r}");
        assert_eq!(d[0]["format"], r["format"], "{b}");
        if b.starts_with("vst3/") {
            assert_eq!(d[0]["name"], v["name"], "{b}: the class validation tried");
            assert!(
                d[0]["name"]
                    .as_str()
                    .unwrap()
                    .starts_with("Summoner Lab Bad State"),
                "{b}: {r}"
            );
            // kind 3, its fixture variant 0x41: FUID(0x5341534C, 0x4C414203, 0x50, 0x41), non-COM order.
            assert_eq!(d[0]["classId"], "5341534c4c4142030000005000000041", "{b}");
        } else {
            assert!(
                d[0]["componentName"]
                    .as_str()
                    .unwrap()
                    .ends_with(v["name"].as_str().unwrap()),
                "{b}: the entry validation tried: {r}"
            );
            assert!(
                d[0]["auCodes"].is_string() && d[0].get("classId").is_none(),
                "{b}: an AU entry"
            );
        }
    }
}

/// The needs-the-note fixture: an instrument whose blocks fail from the 8th
/// after prepare unless a note-on arrived (VST3 kResultFalse, AU an error status, no exception). Validation sends one
/// note in its first block to an instrument and runs 8, so it passes, and only because the note is sent (the
/// no-note mutant of validation gave `failed process` at the 8th block, run once per format).
fn needs_the_note(b: &str, allowed: &[&str]) {
    let r = report(&plugins().join(b), allowed);
    let d = one_ok(&r);
    assert_eq!(d["inputs"], 0, "{b}: an instrument, no inputs: {r}");
}

// One case per format: a VST3 failure must never hide the AU half.
#[test]
fn a_vst3_instrument_that_needs_the_note_passes_because_validation_sends_it() {
    if lab_plugins::absent(
        "a_vst3_instrument_that_needs_the_note_passes_because_validation_sends_it",
    ) {
        return;
    }
    needs_the_note("vst3/summoner-lab-needsnote.vst3", &[]);
}

#[test]
fn an_au_instrument_that_needs_the_note_passes_because_validation_sends_it() {
    if lab_plugins::absent(
        "an_au_instrument_that_needs_the_note_passes_because_validation_sends_it",
    ) {
        return;
    }
    needs_the_note("au/summoner-lab-needsnote.component", AU);
}

/// The knobs bundle: the hostile bundle's knob class ALONE, so unlike the hostile bundle
/// it SCANS (no constructor- or prepare-crash class to end discovery): one description, validated.
#[test]
fn the_knobs_bundle_scans_ok_where_the_hostile_bundle_cannot() {
    if lab_plugins::absent("the_knobs_bundle_scans_ok_where_the_hostile_bundle_cannot") {
        return;
    }
    let r = report(&plugins().join("vst3/summoner-lab-knobs.vst3"), &[]);
    let d = one_ok(&r);
    assert_eq!(
        (&d["name"], &d["categories"]),
        (&"Summoner Lab Knobs".into(), &serde_json::json!(["Fx"])),
        "{r}"
    );
    assert_eq!((&d["inputs"], &d["outputs"]), (&2.into(), &2.into()));
}

/// The default channel count sums the default-active audio
/// buses only. The side-chain fixture has a stereo main input plus a mono auxiliary input that is not default-active
/// (2 in), and a stereo main output plus a second default-active stereo output (4 out).
#[test]
fn the_default_channels_sum_the_default_active_buses_only() {
    if lab_plugins::absent("the_default_channels_sum_the_default_active_buses_only") {
        return;
    }
    let r = report(&plugins().join("vst3/summoner-lab-sidechain.vst3"), &[]);
    let d = &r["classes"].as_array().expect("classes")[0];
    assert_eq!(d["instantiated"], true, "{r}");
    assert_eq!((&d["inputs"], &d["outputs"]), (&2.into(), &4.into()), "{r}");
}

// ---- auCodes and auVersion ------------------------------------------------------------------------------------------

/// A bundle's `AudioComponents` entries as plutil reads its Info.plist (a parser that is not the child's):
/// `(name, codes in their wire form, version)`, each code the string's first four bytes, padded with spaces.
fn plist_entries(bundle: &Path) -> Vec<(String, String, u64)> {
    let out = Command::new("/usr/bin/plutil")
        .args(["-extract", "AudioComponents", "json", "-o", "-"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let code = |s: &Value| {
        let mut b = [b' '; 4];
        for (d, c) in b.iter_mut().zip(s.as_str().unwrap().bytes()) {
            *d = c;
        }
        format!("{:08x}", u32::from_be_bytes(b))
    };
    v.as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["name"].as_str().unwrap().to_string(),
                format!(
                    "{}-{}-{}",
                    code(&e["type"]),
                    code(&e["subtype"]),
                    code(&e["manufacturer"])
                ),
                e["version"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// An AU class's codes are in their wire form (three groups of 8 lowercase hex digits), and a validated entry has its
/// version.
fn agree(d: &Value) {
    let codes = d["auCodes"].as_str().expect("auCodes");
    let c: Vec<u32> = codes
        .split('-')
        .map(|g| u32::from_str_radix(g, 16).unwrap())
        .collect();
    assert_eq!(c.len(), 3, "{codes}");
    assert_eq!(format!("{:08x}-{:08x}-{:08x}", c[0], c[1], c[2]), codes);
    assert!(d["auVersion"].as_u64().is_some(), "{d}");
}

/// The lab gain and synth report the codes and version of their plists, and the four fields agree; a VST3 reports
/// neither field.
#[test]
fn the_lab_aus_report_their_plists_codes_and_version() {
    if lab_plugins::absent("the_lab_aus_report_their_plists_codes_and_version") {
        return;
    }
    for k in ["gain", "synth"] {
        let path = plugins().join(format!("au/summoner-lab-{k}.component"));
        let want = plist_entries(&path);
        let r = report(&path, AU);
        let d = one_ok(&r);
        assert_eq!(d["auCodes"], want[0].1.as_str(), "{k}");
        assert_eq!(d["auVersion"], want[0].2, "{k}");
        assert_eq!(d["auVersion"], 65536);
        agree(d);
        assert_eq!(
            d["componentName"],
            want[0].0.as_str(),
            "{k}: the plist's name, raw"
        );
    }
    let r = report(&plugins().join("vst3/summoner-lab-gain.vst3"), &[]);
    let d = one_ok(&r);
    assert!(
        d.get("auCodes").is_none() && d.get("auVersion").is_none(),
        "{d}"
    );
}

/// A multi-entry bundle: one class per entry, each with its own entry's codes (never the first's), even where the two
/// codes' XORs collide (the multi fixture).
#[test]
fn a_multi_entry_bundle_reports_each_entrys_own_codes() {
    if lab_plugins::absent("a_multi_entry_bundle_reports_each_entrys_own_codes") {
        return;
    }
    let path = plugins().join("au/summoner-lab-multi.component");
    let want = plist_entries(&path);
    assert_eq!(want.len(), 2);
    let r = report(&path, AU);
    assert_eq!(r["outcome"], "ok", "{r}");
    let d = r["classes"].as_array().unwrap();
    assert_eq!(d.len(), 2, "{r}");
    for (i, (d, (name, codes, version))) in d.iter().zip(&want).enumerate() {
        assert_eq!(d["auCodes"], codes.as_str(), "{name}");
        assert_eq!(d["auVersion"], *version, "{name}");
        assert_eq!(d["entry"], i, "{name}");
        agree(d);
    }
    assert_ne!(d[0]["auCodes"], d[1]["auCodes"]);
    assert_eq!(
        d[1]["auCodes"], "61756678-534c7860-5361534d",
        "entry 1's own codes"
    );
}

/// The code check, child-side: an entry with a code byte outside printable ASCII or one of `,` `:` `/` is a failed
/// verdict (its own word `auCodes`), named by its entry index and code, never instantiated; its good sibling is still
/// validated and reported (`discovered`, with its codes). The bundle's plist is the fixture
/// (testkit/plugins/CMakeLists.txt, badcodes).
#[test]
fn an_entry_with_a_bad_code_byte_is_a_failed_verdict_and_its_sibling_is_reported() {
    if lab_plugins::absent(
        "an_entry_with_a_bad_code_byte_is_a_failed_verdict_and_its_sibling_is_reported",
    ) {
        return;
    }
    let path = plugins().join("au/summoner-lab-badcodes.component");
    let want = plist_entries(&path);
    assert_eq!(want.len(), 5);
    let r = report(&path, AU);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert_eq!(
        r["reason"], "auCodes",
        "its own word (N1), not instantiate: {r}"
    );
    let detail = r["detail"].as_str().unwrap();
    assert!(
        detail.contains("AudioComponents entry 1: the subtype code's byte 1"),
        "the first refusal, by entry and code: {detail}"
    );
    assert!(detail.contains("not instantiated"), "{detail}");
    assert!(r["classes"].as_array().unwrap().is_empty());
    // The raw facts carry every entry, a refused one flagged `refused` (its code-check reason, the value never
    // echoed); only the good sibling has a version. The host application describes none for a refused entry.
    let found = r["discovered"].as_array().unwrap();
    assert_eq!(found.len(), 5, "every entry: {r}");
    let good: Vec<&Value> = found
        .iter()
        .filter(|d| d.get("refused").is_none())
        .collect();
    assert_eq!(good.len(), 1, "the good sibling: {r}");
    assert_eq!(good[0]["componentName"], want[0].0.as_str());
    assert_eq!(good[0]["auCodes"], want[0].1.as_str());
    agree(good[0]);
    for d in found.iter().filter(|d| d.get("refused").is_some()) {
        assert!(
            d.get("auVersion").is_none() && d.get("inputs").is_none(),
            "never instantiated: {d}"
        );
    }
    let v = r["probe"]["validation"].as_array().unwrap();
    assert_eq!(v.len(), 5, "{r}");
    assert_eq!(v[0]["ok"], true, "the sibling validated: {r}");
    for (i, (entry, code, byte)) in [
        (1, "subtype", 1),
        (2, "manufacturer", 2),
        (3, "subtype", 2),
        (4, "type", 2),
    ]
    .into_iter()
    .enumerate()
    {
        let x = &v[i + 1];
        assert_eq!(x["ok"], false);
        assert_eq!(x["reason"], "auCodes");
        let d = x["detail"].as_str().unwrap();
        assert!(
            d.contains(&format!(
                "AudioComponents entry {entry}: the {code} code's byte {byte}"
            )),
            "{d}"
        );
    }
    assert_eq!(r["probe"]["refs"].as_array().unwrap().len(), 1);
    // The `auCodes` word is wire-safe: summoner-proto's codec reads it as ScanFailure::Other, keeping the word.
    let back: summoner_proto::codec::Scanned =
        serde_json::from_value(r.clone()).expect("summoner-proto's Scanned");
    match back.outcome {
        summoner_proto::codec::ScannedOutcome::Failed { failure, .. } => assert_eq!(
            failure,
            summoner_proto::codec::ScanFailure::Other {
                reason: "auCodes".into()
            }
        ),
        o => panic!("{o:?}"),
    }
}

/// Any VALIDATION failure outranks `auCodes` as the bundle's verdict, wherever the refused entry stands. The host
/// application trusts that every `discovered` entry beside `failed {reason: auCodes}` passed validation and indexes
/// them as good (the host application's scan index, and its
/// test of that rule), so a refusal FIRST must not hide a later sibling's
/// failure. The fixture (testkit/plugins/CMakeLists.txt, badorder): entry 0 refused for its codes, entry 1 refuses its
/// own state, entry 2 good. The verdict is entry 1's `state`; both validated entries ride in `discovered` (the host
/// application keeps them `validationFailed`, never listed); the refusal stays in the probe's validation, by its own
/// word.
#[test]
fn a_validation_failure_outranks_an_earlier_au_codes_refusal() {
    if lab_plugins::absent("a_validation_failure_outranks_an_earlier_au_codes_refusal") {
        return;
    }
    let path = plugins().join("au/summoner-lab-badorder.component");
    let want = plist_entries(&path);
    assert_eq!(want.len(), 3);
    let r = report(&path, AU);
    assert_eq!(r["outcome"], "failed", "{r}");
    assert_eq!(
        r["reason"], "state",
        "the validation failure, not the earlier auCodes refusal: {r}"
    );
    let detail = r["detail"].as_str().unwrap();
    assert!(
        detail.contains("Summoner Lab Order Bad State")
            && detail.contains("its own state was refused"),
        "the failing entry, by name: {detail}"
    );
    assert!(r["classes"].as_array().unwrap().is_empty());
    let found = r["discovered"].as_array().unwrap();
    let validated: Vec<&Value> = found
        .iter()
        .filter(|d| d.get("refused").is_none())
        .collect();
    let names: Vec<&str> = validated
        .iter()
        .map(|d| d["componentName"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [want[1].0.as_str(), want[2].0.as_str()],
        "the validated entries, in order; the refused one flagged apart: {r}"
    );
    assert!(
        names[0].ends_with("Summoner Lab Order Bad State")
            && names[1].ends_with("Summoner Lab Order OK")
    );
    assert!(found[0].get("refused").is_some(), "entry 0, refused: {r}");
    for (d, (_, codes, _)) in validated.iter().zip(&want[1..]) {
        assert_eq!(d["auCodes"], codes.as_str());
        agree(d);
    }
    let v = r["probe"]["validation"].as_array().unwrap();
    let got: Vec<(bool, &str)> = v
        .iter()
        .map(|x| (x["ok"] == true, x["reason"].as_str().unwrap_or("")))
        .collect();
    assert_eq!(
        got,
        [(false, "auCodes"), (false, "state"), (true, "")],
        "{r}"
    );
    assert!(
        v[0]["detail"]
            .as_str()
            .unwrap()
            .contains("AudioComponents entry 0: the subtype code's byte 1"),
        "{r}"
    );
    assert_eq!(r["probe"]["refs"].as_array().unwrap().len(), 2);
    // As the host application reads it: its own ScanFailure::State, a validation failure, not ScanFailure::Other.
    let back: summoner_proto::codec::Scanned =
        serde_json::from_value(r.clone()).expect("summoner-proto's Scanned");
    match back.outcome {
        summoner_proto::codec::ScannedOutcome::Failed { failure, .. } => {
            assert_eq!(failure, summoner_proto::codec::ScanFailure::State)
        }
        o => panic!("{o:?}"),
    }
}
