//! `tools/vst3-sdk.sh` fails closed: without a pins file, or with one that does not pin the four trees once each, it
//! exits 1 and prints no root (before it, a missing or empty pins file verified nothing and printed a root, exit 0).
//! Each case runs the repo's script, copied into a scratch repo of its own with an empty fetch cache.
//! Mutant: `--cfg mutant_vst3_sdk_fail_open` runs the copy with its `# guard:pins` lines removed; this test turns red.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The script under test: the repo's, or (the mutant) the repo's without its pins guard.
fn script_text() -> String {
    let s = std::fs::read_to_string(repo().join("tools/vst3-sdk.sh")).expect("tools/vst3-sdk.sh");
    if cfg!(mutant_vst3_sdk_fail_open) {
        s.lines()
            .filter(|l| !l.contains("# guard:pins"))
            .map(|l| format!("{l}\n"))
            .collect()
    } else {
        s
    }
}

/// Runs the script in a fresh scratch repo whose pins file is `pins` (`None`: no file): (exit code, stdout, stderr).
fn run(case: &str, pins: Option<&str>) -> (Option<i32>, String, String) {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vst3-sdk-closed-{case}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("repo/tools")).unwrap();
    std::fs::create_dir_all(d.join("repo/testkit/plugins")).unwrap();
    std::fs::create_dir_all(d.join("cache")).unwrap();
    let script = d.join("repo/tools/vst3-sdk.sh");
    std::fs::write(&script, script_text()).unwrap();
    if let Some(p) = pins {
        std::fs::write(d.join("repo/testkit/plugins/vst3-sdk.pins"), p).unwrap();
    }
    let out = Command::new("/bin/bash")
        .arg(&script)
        .env("SUMMONER_VST3_SDK_CACHE", d.join("cache"))
        .output()
        .expect("bash");
    let r = (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    std::fs::remove_dir_all(&d).unwrap();
    r
}

const LIC: &str = "e86d79e19e4a33ebc442d2e0d083887dfc75a2a8de9fe17eeddedb84810ad74e";

fn line(name: &str) -> String {
    format!("{name} repo 0000000000000000000000000000000000000000 {LIC}\n")
}

#[test]
fn vst3_sdk_sh_refuses_a_missing_or_incomplete_pins_file() {
    let four: String = ["vst3sdk", "pluginterfaces", "base", "public.sdk"]
        .map(line)
        .concat();
    let three: String = ["vst3sdk", "pluginterfaces", "base"].map(line).concat();
    let twice = format!("{four}{}", line("base"));
    let cases: [(&str, Option<&str>); 5] = [
        ("missing", None),
        ("empty", Some("")),
        ("comments", Some("# only a comment\n\n")),
        ("three", Some(&three)),
        ("twice", Some(&twice)),
    ];
    for (case, pins) in cases {
        let (code, stdout, stderr) = run(case, pins);
        assert_eq!(
            code,
            Some(1),
            "{case}: exit {code:?}, stdout {stdout:?}, stderr {stderr:?}"
        );
        assert!(stdout.is_empty(), "{case}: printed a root: {stdout:?}");
        assert!(stderr.contains("vst3-sdk: "), "{case}: {stderr:?}");
    }
    // The control: four well-formed lines get past the pins guard and are refused for the missing fetch instead.
    let (code, stdout, stderr) = run("four", Some(&four));
    assert_eq!(code, Some(1), "{stdout:?} {stderr:?}");
    assert!(stderr.contains("no fetched vst3sdk"), "{stderr:?}");
}
