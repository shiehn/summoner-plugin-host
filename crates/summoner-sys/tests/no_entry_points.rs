//! A plugin module's entry and exit points (`bundleEntry` / `bundleExit` on macOS, `InitDll` / `ExitDll`
//! on Windows, `ModuleEntry` / `ModuleExit` on Linux) stay inside the module's body (the shim's loader): neither the
//! shim's C header nor any `pub` item of this repo's crates names one. The public surface takes a bundle
//! (`bundle::Bundle`) and resolves the binary inside.

use std::path::{Path, PathBuf};

const ENTRY_POINTS: [&str; 6] = [
    "bundleEntry",
    "bundleExit",
    "InitDll",
    "ExitDll",
    "ModuleEntry",
    "ModuleExit",
];

/// The lines of `text` that are a public declaration or a C prototype naming an entry point.
fn offending(text: &str, header: bool) -> Vec<String> {
    text.lines()
        .filter(|l| {
            let code = l.split("//").next().unwrap_or("");
            let public = header || code.trim_start().starts_with("pub ");
            public && ENTRY_POINTS.iter().any(|e| code.contains(e))
        })
        .map(|l| l.trim().to_string())
        .collect()
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn the_public_surface_names_no_module_entry_point() {
    // This crate's own directory, whatever its name (the export renames the crate directories).
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates = here.join("..");
    let header = std::fs::read_to_string(here.join("vst3/summoner_vst3_shim.h")).unwrap();
    assert_eq!(
        offending(&header, true),
        Vec::<String>::new(),
        "the shim header"
    );
    let mut files = Vec::new();
    for e in std::fs::read_dir(&crates).unwrap() {
        let src = e.unwrap().path().join("src");
        if src.is_dir() {
            rs_files(&src, &mut files);
        }
    }
    assert!(
        files.len() > 10,
        "the crates' sources were read: {}",
        files.len()
    );
    let found: Vec<String> = files
        .iter()
        .flat_map(|f| {
            let t = std::fs::read_to_string(f).unwrap();
            offending(&t, false)
                .into_iter()
                .map(move |l| format!("{}: {l}", f.display()))
        })
        .collect();
    assert!(found.is_empty(), "{found:#?}");
}

/// The guard's planted controls: a public Rust item and a C prototype naming one are found; a private one and a comment
/// are not.
#[test]
fn the_guard_finds_a_planted_entry_point() {
    assert_eq!(
        offending("pub fn bundle_entry_point() {} // bundleEntry", false).len(),
        0
    );
    assert_eq!(
        offending("pub unsafe extern \"C\" fn bundleEntry() {}", false).len(),
        1
    );
    assert_eq!(offending("fn bundleEntry() {}", false).len(), 0);
    assert_eq!(offending("int InitDll(void);", true).len(), 1);
}
