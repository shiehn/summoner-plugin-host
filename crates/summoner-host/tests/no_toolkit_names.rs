//! The plugin host is a standalone open-source repo with no reference to a C++ plugin toolkit or the engine it came
//! from: no file of this repo names them, in any case. The tree is every file
//! git tracks plus every untracked file it does not ignore, so a new file is checked before it is added. The words are
//! kept hex-encoded here so that this file does not name them either; a link-map check of the
//! built binaries covers their symbols.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::Command;

/// The names, hex-encoded (lowercase ASCII; matched case-insensitively).
const NAMES_HEX: [&str; 3] = ["6a756365", "747261636b74696f6e", "70726f6a75636572"];

fn names() -> Vec<Vec<u8>> {
    NAMES_HEX
        .iter()
        .map(|h| {
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect()
        })
        .collect()
}

/// The repo's files (tracked, and untracked but not ignored), relative to the repo root.
fn files(repo: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    out.stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect()
}

/// Each `(file, line number)` that names one of [`names`], its path included.
fn hits(repo: &Path) -> Vec<(String, usize)> {
    let names = names();
    let named = |s: &[u8]| {
        let s = s.to_ascii_lowercase();
        names
            .iter()
            .any(|n| s.windows(n.len()).any(|w| w == n.as_slice()))
    };
    let mut out = Vec::new();
    for f in files(repo) {
        if named(f.as_bytes()) {
            out.push((f.clone(), 0));
        }
        // A tracked file deleted in the working tree is not there to read.
        let Ok(body) = std::fs::read(repo.join(&f)) else {
            continue;
        };
        for (i, line) in body.split(|b| *b == b'\n').enumerate() {
            if named(line) {
                out.push((f.clone(), i + 1));
            }
        }
    }
    out
}

#[test]
fn no_file_names_the_oracle_toolkit_or_the_reference_engine() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    assert!(files(&repo).len() > 100, "the tree is listed");
    let h = hits(&repo);
    assert!(
        h.is_empty(),
        "{} line(s) name them (file, line; 0 = the path): {h:?}",
        h.len()
    );
}

#[test]
fn the_gate_sees_a_name_in_any_case() {
    let names = names();
    assert_eq!(names.len(), 3);
    for n in &names {
        let upper = n.to_ascii_uppercase();
        let line = [b"a ".as_slice(), &upper, b" b"].concat();
        assert!(
            names.iter().any(|m| line
                .to_ascii_lowercase()
                .windows(m.len())
                .any(|w| w == m.as_slice())),
            "an upper-case name is seen"
        );
    }
}
