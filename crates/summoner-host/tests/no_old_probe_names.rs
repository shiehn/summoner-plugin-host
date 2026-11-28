//! The probe binaries are `summoner-scan-probe`, `summoner-vst3-probe` and `summoner-au-probe`: no file of the public
//! tree names their old names (the crates, the testkit, the manifests and the exported tools; the repo's private
//! notes are not exported and are not read). The old names are assembled at run time, so this file names none.
//! A planted control proves the matcher sees a name before the tree is judged.
//! Mutant: `--cfg mutant_probe_names_blind` (the matcher sees nothing) turns both tests red.
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::Command;

/// The public tree: these directories, and these files at the root and in `tools/`.
const DIRS: [&str; 2] = ["crates/", "testkit/"];
const FILES: [&str; 11] = [
    "Cargo.toml",
    "Cargo.lock",
    "VENDORED.md",
    "rust-toolchain.toml",
    "tools/au-sdk.sh",
    "tools/build-plugins.sh",
    "tools/build-shipped.sh",
    "tools/fetch-au-sdk.sh",
    "tools/fetch-vst3-subs.sh",
    "tools/source-hash.sh",
    "tools/vst3-sdk.sh",
];

/// The three old names (the old three-letter prefix, the format, `-probe`).
fn old_names() -> Vec<Vec<u8>> {
    ["scan", "vst3", "au"]
        .iter()
        .map(|f| format!("{}-{f}-probe", ["s", "a", "s"].concat()).into_bytes())
        .collect()
}

fn named(line: &[u8], names: &[Vec<u8>]) -> bool {
    !cfg!(mutant_probe_names_blind)
        && names
            .iter()
            .any(|n| line.windows(n.len()).any(|w| w == n.as_slice()))
}

/// The public tree's files (tracked, and untracked but not ignored).
fn public_files(repo: &Path) -> Vec<String> {
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
        .filter(|f| DIRS.iter().any(|d| f.starts_with(d)) || FILES.contains(&f.as_str()))
        .collect()
}

#[test]
fn the_matcher_sees_each_old_name() {
    let names = old_names();
    for n in &names {
        let line = [b"run ".as_slice(), n, b" now"].concat();
        assert!(
            named(&line, &names),
            "blind to {}",
            String::from_utf8_lossy(n)
        );
    }
    assert!(!named(b"run summoner-scan-probe now", &names));
}

#[test]
fn no_public_file_names_an_old_probe_binary() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let names = old_names();
    // The planted control first: a scratch file holding one old name is a hit.
    let plant =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("probe-plant-{}", std::process::id()));
    std::fs::write(&plant, [b"x ".as_slice(), &names[0], b"\n"].concat()).unwrap();
    let planted = std::fs::read(&plant).unwrap();
    std::fs::remove_file(&plant).unwrap();
    assert!(
        planted.split(|b| *b == b'\n').any(|l| named(l, &names)),
        "the planted control is not seen"
    );
    let files = public_files(&repo);
    assert!(files.len() > 100, "the tree is listed: {}", files.len());
    let mut hits = Vec::new();
    for f in &files {
        if named(f.as_bytes(), &names) {
            hits.push((f.clone(), 0));
        }
        let Ok(body) = std::fs::read(repo.join(f)) else {
            continue;
        };
        for (i, line) in body.split(|b| *b == b'\n').enumerate() {
            if named(line, &names) {
                hits.push((f.clone(), i + 1));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "old probe names (file, line; 0 = the path): {hits:?}"
    );
}
