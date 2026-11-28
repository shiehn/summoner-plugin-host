//! The lab's VST3 bundles (`tools/build-plugins.sh` → `target.noindex/plugins/vst3/`), each exercised by
//! `summoner-vst3-probe` in its own process under the file-access recorder: every run asserts that nothing under the
//! system's plugin folders was touched, and that the recorder saw the bundle (it works).
//!
//! The recorder's positive control (a stat, no load) runs too.
#![forbid(unsafe_code)]
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;

use std::path::{Path, PathBuf};
use std::process::Command;

fn bundle(kind: &str) -> PathBuf {
    let b = lab_plugins::dir().join(format!("vst3/summoner-lab-{kind}.vst3"));
    assert!(
        b.is_dir(),
        "{} is not built (tools/build-plugins.sh)",
        b.display()
    );
    b
}

/// A number per probe run: the tests run in parallel in one process, and each run's scratch directory (removed when
/// it ends) must be its own, or one test's cleanup deletes another's recorder log under it.
fn run_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The real home: the parent of the dev fence the lab env exports (`SUMMONER_DEV_PLUGIN_ROOTS` = `<real home>/<lab>`,
/// the development environment's), so this test needs no engine-side crate for it.
fn real_home() -> Result<std::path::PathBuf, String> {
    std::env::var_os("SUMMONER_DEV_PLUGIN_ROOTS")
        .and_then(|r| Path::new(&r).parent().map(Path::to_path_buf))
        .ok_or_else(|| {
            "SUMMONER_DEV_PLUGIN_ROOTS is not set (the development plugin root)".to_string()
        })
}

/// The system plugin folders a lab process must never touch, with the real home's and the lab home's.
fn forbidden() -> Vec<String> {
    let mut v = vec![
        "/Library/Audio/Plug-Ins".to_string(),
        "/System/Library/Components".to_string(),
    ];
    if let Ok(h) = real_home() {
        v.push(format!("{}/Library/Audio/Plug-Ins", h.display()));
    }
    if let Some(h) = std::env::var_os("HOME") {
        v.push(format!(
            "{}/Library/Audio/Plug-Ins",
            Path::new(&h).display()
        ));
    }
    v
}

#[path = "../../../testkit/plugins/fswatch/fswatch_v2.rs"]
mod fswatch_v2;

/// The records of a recorder log (fswatch v2, parsed strictly by the shared reader: an old-format log panics) that
/// name a forbidden folder in ANY of their locations (a rename's or a link's two), or, relative, look like one; and
/// every WRITE whose location cannot be resolved (a STOP, never a pass).
pub fn violations(log: &str) -> Vec<String> {
    let bad = forbidden();
    let named = |path: &str| {
        bad.iter().any(|b| path.starts_with(b.as_str()))
            || (!path.starts_with('/')
                && (path.contains("Audio/Plug-Ins") || path.contains("Library/Components")))
    };
    let recs = fswatch_v2::parse(log);
    let mut v: Vec<String> = recs
        .iter()
        .filter(|r| {
            let paths = r.locations().unwrap_or_else(|| r.paths.clone());
            paths.iter().any(|p| named(p))
        })
        .map(|r| r.raw.clone())
        .collect();
    // And every STOP of the shared rules (v3: an unplaceable attempt that never returned or reached a file, a
    // truncated attempt, a write under /System or /Library, …; v2: an unplaceable write), once per record.
    for s in fswatch_v2::stops(&recs) {
        if !v.iter().any(|r| s.ends_with(r.as_str())) {
            v.push(s);
        }
    }
    v
}

/// Runs the probe under the recorder; returns (stdout, the recorder's log).
fn probe(args: &[&str]) -> (String, String, bool) {
    let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
    assert!(
        fswatch.is_file(),
        "{} is not built (tools/build-plugins.sh)",
        fswatch.display()
    );
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "fswatch-{}-{}",
        std::process::id(),
        run_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join(format!("{}.log", args.join("_").replace('/', "_")));
    let _ = std::fs::remove_file(&log);
    let out = Command::new(env!("CARGO_BIN_EXE_summoner-vst3-probe"))
        .args(args)
        .env("DYLD_INSERT_LIBRARIES", &fswatch)
        .env("SUMMONER_FSWATCH_LOG", &log)
        .output()
        .expect("run the probe");
    let text = fswatch_v2::read_log(std::path::Path::new(&log));
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        text,
        out.status.success(),
    )
}

#[test]
fn the_recorder_sees_a_stat_of_the_bundle_and_nothing_forbidden() {
    if lab_plugins::absent("the_recorder_sees_a_stat_of_the_bundle_and_nothing_forbidden") {
        return;
    }
    let b = bundle("gain");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--dry"]);
    assert!(ok && out.contains("dry: stat ok"), "{out}");
    assert!(
        fswatch_v2::parse(&log).iter().any(|r| r.call == "stat"
            && r.paths == [b.display().to_string()]
            && (r.version == 2 || r.post.as_ref().is_some_and(|p| p.ret == 0))),
        "the recorder saw the stat:\n{log}"
    );
    assert_eq!(violations(&log), Vec::<String>::new());
}

#[test]
fn the_violation_check_flags_each_forbidden_folder() {
    let home = real_home().unwrap();
    let log = format!(
        "fswatch\tv2\tpid=1\nstat\t-\t/Library/Audio/Plug-Ins/VST3\nopendir\t-\t{}/Library/Audio/Plug-Ins/Components\nopen\tflags=0x0\t/System/Library/Components/x.component\nopen\tflags=0x0\tAudio/Plug-Ins/rel\nrename\t-\t/scratch/x/a\t/Library/Audio/Plug-Ins/Components/x\nstat\t-\t/scratch/x/ok.vst3\nrename\t-\t/scratch/x/a\t/scratch/x/b\n",
        home.display()
    );
    assert_eq!(violations(&log).len(), 5, "{:?}", violations(&log));
    // A v1 log (no version line) is refused, never read as clean.
    let v1 = std::panic::catch_unwind(|| violations("stat /Library/Audio/Plug-Ins/VST3\n"));
    assert!(v1.is_err(), "an old-format log must fail loudly");
}

#[test]
fn the_gain_bundle_loads_by_its_class_id_and_processes_exactly() {
    if lab_plugins::absent("the_gain_bundle_loads_by_its_class_id_and_processes_exactly") {
        return;
    }
    let b = bundle("gain");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "gain"]);
    println!("{out}");
    assert!(
        ok && out.contains("done ok") && !out.contains("FAIL"),
        "{out}"
    );
    assert!(
        log.contains(&format!("{}/Contents/MacOS/summoner-lab-gain", b.display())),
        "the load was seen:\n{log}"
    );
    assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
}

/// The load-time setupProcessing's result is ignored. The
/// fixture refuses its first setupProcessing (the load's, at 44.1 kHz / 1024); the load still succeeds and the real
/// prepare's call plays the gain exactly.
#[test]
fn a_refused_load_time_setup_is_not_a_load_error() {
    if lab_plugins::absent("a_refused_load_time_setup_is_not_a_load_error") {
        return;
    }
    let b = bundle("refusesetup");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "gain"]);
    println!("{out}");
    assert!(
        ok && out.contains("done ok") && !out.contains("FAIL"),
        "{out}"
    );
    assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
}

/// A plugin that refuses the stereo arrangement keeps its own main buses and trusts them (every channel
/// pointer dereferenced): 4/4 and the asymmetric 1/4, 4/1 and 2/6. Each loads and plays with a real buffer per
/// channel (zeros in past the block, its extra outputs dropped), never a null pointer (the probe would die of
/// SIGSEGV), on 2- and 4-wide blocks, a stereo block after the wide one included.
#[test]
fn a_main_bus_wider_than_stereo_gets_a_real_buffer_per_channel() {
    if lab_plugins::absent("a_main_bus_wider_than_stereo_gets_a_real_buffer_per_channel") {
        return;
    }
    for (kind, io) in [
        ("quad", "4/4"),
        ("wide14", "1/4"),
        ("wide41", "4/1"),
        ("wide26", "2/6"),
    ] {
        let b = bundle(kind);
        let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "quad", "--io", io]);
        println!("{kind}: {out}");
        assert!(
            ok && out.contains("done ok") && !out.contains("FAIL"),
            "{kind}: {out}"
        );
        assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
    }
}

/// The probe under both recorders: the file-access one and the allocation counter.
fn probe_counted(args: &[&str]) -> (String, String, bool) {
    let counter = lab_plugins::dir().join("libsummoner_lab_malloccount.dylib");
    assert!(
        counter.is_file(),
        "{} is not built (tools/build-plugins.sh)",
        counter.display()
    );
    let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "counted-{}-{}",
        std::process::id(),
        run_id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join(format!("{}.log", args.join("_").replace('/', "_")));
    let _ = std::fs::remove_file(&log);
    let out = Command::new(env!("CARGO_BIN_EXE_summoner-vst3-probe"))
        .args(args)
        .env(
            "DYLD_INSERT_LIBRARIES",
            format!("{}:{}", fswatch.display(), counter.display()),
        )
        .env("SUMMONER_FSWATCH_LOG", &log)
        .output()
        .expect("run the probe");
    let text = fswatch_v2::read_log(std::path::Path::new(&log));
    let _ = std::fs::remove_dir_all(&dir);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        text,
        out.status.success(),
    )
}

#[test]
fn the_synth_bundle_is_deterministic_and_takes_notes() {
    if lab_plugins::absent("the_synth_bundle_is_deterministic_and_takes_notes") {
        return;
    }
    let b = bundle("synth");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "synth"]);
    println!("{out}");
    assert!(
        ok && out.contains("done ok") && !out.contains("FAIL"),
        "{out}"
    );
    assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
}

#[test]
fn processing_allocates_nothing_gain_and_synth() {
    if lab_plugins::absent("processing_allocates_nothing_gain_and_synth") {
        return;
    }
    for kind in ["gain", "synth"] {
        let b = bundle(kind);
        let (out, log, ok) = probe_counted(&[b.to_str().unwrap(), "--noalloc", kind]);
        println!("{kind}: {out}");
        assert!(
            ok && out.contains("check no_allocation_in_64_blocks ok"),
            "{kind}: {out}"
        );
        assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
    }
}

#[test]
fn a_lying_parameter_count_is_reported_but_never_indexed_past() {
    if lab_plugins::absent("a_lying_parameter_count_is_reported_but_never_indexed_past") {
        return;
    }
    let b = bundle("hostile");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "liar"]);
    println!("{out}");
    assert!(
        ok && out.contains("done ok") && !out.contains("FAIL"),
        "{out}"
    );
    assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
}

#[test]
fn state_raw_chunks_round_trip_and_play() {
    if lab_plugins::absent("state_raw_chunks_round_trip_and_play") {
        return;
    }
    let b = bundle("gain");
    let (out, log, ok) = probe(&[b.to_str().unwrap(), "--check", "state"]);
    println!("{out}");
    assert!(
        ok && out.contains("done ok") && !out.contains("FAIL"),
        "{out}"
    );
    assert_eq!(violations(&log), Vec::<String>::new(), "{log}");
}
