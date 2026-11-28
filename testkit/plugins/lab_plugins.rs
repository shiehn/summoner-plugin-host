//! The lab plugins a test needs: where `tools/build-plugins.sh` puts them (`target.noindex/plugins`, or
//! `SUMMONER_LAB_PLUGINS_DIR` for a test run that proves the skip), and whether a test that needs them runs.
//!
//! A test that needs them starts with `if lab_plugins::absent("<test>") { return; }`. Without the plugins it is
//! skipped loudly: one `SKIPPED (no lab plugins)` line on stderr naming the test and the missing bundle. With
//! `SUMMONER_REQUIRE_LAB_PLUGINS=1` (the lab and the full CI, which build the plugins first) a missing plugin fails
//! the test instead, so it never hides. Included by `#[path]` into each test crate that needs it.
#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};

/// The folder the lab plugins are in.
pub fn dir() -> PathBuf {
    match std::env::var_os("SUMMONER_LAB_PLUGINS_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        // Canonical: the lab fence and the scan request take plain absolute paths only (no `..`). The repository
        // root always exists, so the path is plain even when the plugins are not built.
        _ => {
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            root.canonicalize()
                .unwrap_or(root)
                .join("target.noindex/plugins")
        }
    }
}

/// Whether the lab plugins are absent (the test is then skipped, said on stderr); panics instead when
/// `SUMMONER_REQUIRE_LAB_PLUGINS=1`.
pub fn absent(test: &str) -> bool {
    // The marker every `tools/build-plugins.sh` run builds.
    let marker = dir().join("vst3/summoner-lab-gain.vst3");
    if marker.exists() {
        return false;
    }
    let why = format!(
        "{test}: {} is absent; run tools/build-plugins.sh",
        marker.display()
    );
    if std::env::var_os("SUMMONER_REQUIRE_LAB_PLUGINS").is_some_and(|v| v == "1") {
        panic!("the lab plugins are required (SUMMONER_REQUIRE_LAB_PLUGINS=1): {why}");
    }
    // Straight to the stderr handle, past libtest's capture (which holds `eprintln!` of a passing test), so the skip
    // shows in every run.
    let _ = writeln!(std::io::stderr(), "SKIPPED (no lab plugins): {why}");
    true
}
