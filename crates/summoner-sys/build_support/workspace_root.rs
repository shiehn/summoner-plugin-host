//! The workspace root this crate is built in: the nearest ancestor of the crate's manifest directory whose
//! `Cargo.toml` declares `[workspace]`. `build.rs` compiles it in (`SUMMONER_DEV_REPO`, beside the fence) as the base
//! of the dev build roots, so the roots never depend on the crate's own folder name or depth: a copy of the
//! workspace under other directory names has the same roots below its own root. `tests/workspace_root.rs` checks it.

use std::path::{Path, PathBuf};

/// The workspace root above `manifest_dir` (the crate's `CARGO_MANIFEST_DIR`), or `None` when no ancestor's
/// `Cargo.toml` holds a `[workspace]` table.
#[cfg(not(mutant_dev_repo_by_suffix))]
pub fn workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir.ancestors().skip(1).find_map(|d| {
        let text = std::fs::read_to_string(d.join("Cargo.toml")).ok()?;
        text.lines()
            .any(|l| l.trim() == "[workspace]")
            .then(|| d.to_path_buf())
    })
}

/// The mutant (`--cfg mutant_dev_repo_by_suffix`): the root by the crate's folder name, as before.
/// `tests/workspace_root.rs`'s `the_root_does_not_depend_on_the_crates_folder_name` must turn red.
#[cfg(mutant_dev_repo_by_suffix)]
pub fn workspace_root(manifest_dir: &Path) -> Option<PathBuf> {
    manifest_dir
        .to_str()?
        .strip_suffix("/crates/sys")
        .map(PathBuf::from)
}
