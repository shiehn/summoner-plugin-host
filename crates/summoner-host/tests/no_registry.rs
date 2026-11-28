//! No code of ours walks the system's AU registry. Every binary we build that hosts
//! plugins is checked twice: no undefined symbol `AudioComponentFindNext`, `AudioComponentCount` or
//! `AudioComponentCopyName` (`nm -u`), and none of those three names anywhere in its bytes, so a `dlsym` by name cannot
//! slip past the symbol check either.
use std::path::{Path, PathBuf};
use std::process::Command;

const FORBIDDEN: [&str; 3] = [
    "AudioComponentFindNext",
    "AudioComponentCount",
    "AudioComponentCopyName",
];

/// The binaries: this package's child, and its siblings in the same target directory (the VST3 and AU probes).
fn binaries() -> Vec<PathBuf> {
    let child = PathBuf::from(env!("CARGO_BIN_EXE_summoner-host"));
    let dir = child.parent().unwrap().to_path_buf();
    let mut out = vec![child];
    for sibling in ["summoner-vst3-probe", "summoner-au-probe"] {
        let p = dir.join(sibling);
        assert!(
            p.is_file(),
            "{} not built (cargo build --workspace first)",
            p.display()
        );
        out.push(p);
    }
    out
}

fn undefined(bin: &Path) -> String {
    let out = Command::new("nm").arg("-uj").arg(bin).output().expect("nm");
    assert!(out.status.success(), "nm {}", bin.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn no_binary_names_the_registry_walk() {
    for bin in binaries() {
        let syms = undefined(&bin);
        // The control: nm read the binary's imports (every one of these links libSystem).
        assert!(
            syms.lines().any(|l| l == "_write" || l == "_malloc"),
            "nm saw no imports in {}",
            bin.display()
        );
        let bytes = std::fs::read(&bin).unwrap();
        for name in FORBIDDEN {
            assert!(
                !syms.lines().any(|l| l.contains(name)),
                "{} imports {name}",
                bin.display()
            );
            assert!(
                !bytes.windows(name.len()).any(|w| w == name.as_bytes()),
                "{} contains the string {name}",
                bin.display()
            );
        }
    }
}

#[test]
fn the_au_binaries_do_reach_audiotoolbox() {
    // The positive control for the check above: the AU-hosting binaries import AudioToolbox's registration call.
    let syms = undefined(Path::new(env!("CARGO_BIN_EXE_summoner-host")));
    assert!(
        syms.lines().any(|l| l == "_AudioComponentRegister"),
        "the child does not import AudioComponentRegister"
    );
}

/// A child built with `release-loading` (a release build in its own target dir under target.noindex, so it never
/// touches the others) imports none of the three, like the default binaries above. There is no exact registry
/// lookup (`AudioComponentFindNext`) and no `registry-test` feature.
#[test]
fn a_release_loading_child_imports_none_of_the_registry_walk() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = repo.join("target.noindex/release-loading");
    let out = Command::new(env!("CARGO"))
        .current_dir(&repo)
        .args([
            "build",
            "-q",
            "--release",
            "--no-default-features",
            "--features",
            "release-loading",
            "-p",
            "summoner-host",
        ])
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "the release-loading build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bin = target.join("release/summoner-host");
    let syms = undefined(&bin);
    let bytes = std::fs::read(&bin).unwrap();
    for name in FORBIDDEN {
        assert!(
            !syms.lines().any(|l| l.contains(name)),
            "{} imports {name}",
            bin.display()
        );
        assert!(
            !bytes.windows(name.len()).any(|w| w == name.as_bytes()),
            "{} contains the string {name}",
            bin.display()
        );
    }
}
