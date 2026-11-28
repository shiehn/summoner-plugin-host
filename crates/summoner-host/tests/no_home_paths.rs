//! A release build names no home directory: `tools/release-remap.sh`'s path remapping, checked by
//! `tools/no-home-paths.sh` (which `tools/build-shipped.sh` runs on the shipped child and its editor dylib).
//! The checker is proven on planted controls first; then a release child built here with the remapping passes it.
//! Mutant: `--cfg mutant_home_paths_blind` runs the checker without its `# guard:home` lines; both tests turn red.
#![forbid(unsafe_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The checker under test: the repo's, or (the mutant) the repo's without its guard lines, copied into `dir`.
fn checker(dir: &Path) -> PathBuf {
    let s = std::fs::read_to_string(repo().join("tools/no-home-paths.sh"))
        .expect("tools/no-home-paths.sh");
    let s: String = if cfg!(mutant_home_paths_blind) {
        s.lines()
            .filter(|l| !l.contains("# guard:home"))
            .map(|l| format!("{l}\n"))
            .collect()
    } else {
        s
    };
    let p = dir.join("no-home-paths.sh");
    std::fs::write(&p, s).unwrap();
    p
}

fn check(script: &Path, file: &Path) -> Option<i32> {
    Command::new("/bin/bash")
        .arg(script)
        .arg(file)
        .output()
        .expect("bash")
        .status
        .code()
}

fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_checker_refuses_a_home_path_and_passes_a_clean_file() {
    let d = scratch("home-paths-controls");
    let script = checker(&d);
    // The home prefixes are assembled here, so this file names none itself.
    let users = format!("\x00abc /{}/someone/src/x.rs:1:2 \x00", "Users");
    let home = format!("\x00 /{}/someone/.cargo/x \x00", "home");
    for (name, body, want) in [
        ("users", users.as_bytes(), 1),
        ("home", home.as_bytes(), 1),
        (
            "clean",
            &b"\x00 /summoner/crates/sys/src/lib.rs /home-dir /rustc/abc \x00"[..],
            0,
        ),
    ] {
        let f = d.join(name);
        std::fs::write(&f, body).unwrap();
        assert_eq!(check(&script, &f), Some(want), "{name}");
    }
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_remapped_release_child_names_no_home_directory() {
    let d = scratch("home-paths-release");
    let script = checker(&d);
    // The control: a planted home path is seen by the same checker, so a pass below is not a blind one.
    let plant = d.join("plant");
    std::fs::write(&plant, format!("x /{}/someone/y\n", "Users")).unwrap();
    assert_eq!(
        check(&script, &plant),
        Some(1),
        "the planted control is not seen"
    );
    let out = Command::new(repo().join("tools/release-remap.sh"))
        .output()
        .expect("release-remap.sh");
    assert!(out.status.success());
    let flags: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert!(
        flags.len() >= 4 && flags.iter().all(|f| f.starts_with("--remap-path-prefix=")),
        "{flags:?}"
    );
    let target = repo().join("target.noindex/remapped");
    let build = Command::new(env!("CARGO"))
        .current_dir(repo())
        .args([
            "build",
            "-q",
            "--release",
            "-p",
            "summoner-host",
            "--no-default-features",
        ])
        .args(["--features", "release-loading"])
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_ENCODED_RUSTFLAGS", flags.join("\x1f"))
        .env_remove("RUSTFLAGS")
        .env_remove("SUMMONER_DEV_PLUGIN_ROOTS")
        .output()
        .expect("cargo");
    assert!(
        build.status.success(),
        "the remapped release build: {}",
        String::from_utf8_lossy(&build.stderr)
    );
    let child = target.join("release/summoner-host");
    assert_eq!(
        check(&script, &child),
        Some(0),
        "{} names a home directory",
        child.display()
    );
    std::fs::remove_dir_all(&d).unwrap();
}
