//! The dev build roots' base is the workspace root, found from the crate's manifest directory by the nearest
//! `[workspace]` manifest above it: moving or renaming the crate's folder (a copy of the workspace under other
//! directory names) does not change where the roots are, relative to the workspace.
//! Mutant: `--cfg mutant_dev_repo_by_suffix` (the root by the folder name `crates/sys`) turns it red.
#[path = "../build_support/workspace_root.rs"]
mod workspace_root;

use std::path::Path;
use workspace_root::workspace_root;

fn tree(base: &Path, crate_dir: &str) {
    std::fs::create_dir_all(base.join(crate_dir)).unwrap();
    std::fs::write(
        base.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/*\"]\n",
    )
    .unwrap();
    std::fs::write(
        base.join(crate_dir).join("Cargo.toml"),
        "[package]\nname = \"x\"\n",
    )
    .unwrap();
}

#[test]
fn the_root_does_not_depend_on_the_crates_folder_name() {
    // Outside any workspace: CARGO_TARGET_TMPDIR sits inside this repository when the target folder does (a CI
    // checkout), and the "lone" case below would then find this repository's manifest.
    let d = std::env::temp_dir().join(format!("summoner-ws-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    for (ws, krate) in [
        ("a", "crates/sys"),
        ("b", "crates/summoner-sys"),
        ("c", "crates/nested/deeper/sys"),
        ("d", "elsewhere"),
    ] {
        let base = d.join(ws);
        tree(&base, krate);
        assert_eq!(
            workspace_root(&base.join(krate)),
            Some(base.clone()),
            "{ws}: {krate}"
        );
    }
    // No workspace manifest above: no root (so no build roots, fail closed).
    std::fs::create_dir_all(d.join("lone/crates/sys")).unwrap();
    std::fs::write(
        d.join("lone/crates/sys/Cargo.toml"),
        "[package]\nname = \"x\"\n",
    )
    .unwrap();
    assert_eq!(workspace_root(&d.join("lone/crates/sys")), None);
    std::fs::remove_dir_all(&d).unwrap();
}

/// This build's own: the root `build.rs` compiled in is this workspace's.
#[test]
fn the_compiled_in_root_is_this_workspace() {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let want = here.join("../..").canonicalize().unwrap();
    let got = summoner_sys::bundle::dev_repo().expect("a dev build compiles its workspace root in");
    assert_eq!(Path::new(got).canonicalize().unwrap(), want);
}
