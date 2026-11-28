//! `build_support/source_hash.rs` (the hash Windows builds embed) equals `tools/source-hash.sh` (the hash every other
//! build embeds, and the host application's stale-child guard reads), on this tree, with and without a lab root. Unix
//! only: the script is bash. The listing's format is the subtle part (two spaces, byte order, which files), so a drift
//! in either one fails here.
#![allow(dead_code)]

/// The hasher `build_support/source_hash.rs` names as `crate::sha256` (summoner-sys's, by the crate, not a path).
mod sha256 {
    pub use summoner_sys::sha256::*;
}
#[path = "../build_support/source_hash.rs"]
mod source_hash;

#[cfg(unix)]
fn script(root: &std::path::Path, lab: Option<&str>) -> String {
    let mut c = std::process::Command::new(root.join("tools/source-hash.sh"));
    match lab {
        Some(v) => c.env("SUMMONER_DEV_PLUGIN_ROOTS", v),
        None => c.env_remove("SUMMONER_DEV_PLUGIN_ROOTS"),
    };
    let out = c.output().expect("tools/source-hash.sh runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[cfg(unix)]
#[test]
fn the_rust_hash_is_the_scripts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for lab in [None, Some(""), Some("/a/lab/root")] {
        let want = script(&root, lab);
        let got = source_hash::source_hash(&root, lab.unwrap_or("")).unwrap();
        assert_eq!(got, want, "SUMMONER_DEV_PLUGIN_ROOTS={lab:?}");
        assert_eq!(got.len(), 64);
    }
}

#[test]
fn a_name_shasum_would_escape_is_refused() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("source-hash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["crates/x", "tools", "testkit/plugins"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    for f in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "tools/source-hash.sh",
        "testkit/plugins/vst3-sdk.pins",
    ] {
        std::fs::write(dir.join(f), b"x\n").unwrap();
    }
    std::fs::write(dir.join("crates/x/plain.rs"), b"fn main() {}\n").unwrap();
    let ok = source_hash::source_hash(&dir, "").unwrap();
    assert_eq!(ok.len(), 64);
    // Editor droppings are not hashed: adding one leaves the hash unchanged.
    std::fs::write(dir.join("crates/x/plain.rs~"), b"old\n").unwrap();
    std::fs::write(dir.join("crates/x/.plain.rs.swp"), b"swap\n").unwrap();
    assert_eq!(source_hash::source_hash(&dir, "").unwrap(), ok);
    // The lab root is part of the hash.
    assert_ne!(source_hash::source_hash(&dir, "/lab").unwrap(), ok);
    #[cfg(unix)]
    {
        std::fs::write(dir.join("crates/x/a\\b.rs"), b"\n").unwrap();
        let e = source_hash::source_hash(&dir, "").unwrap_err();
        assert!(e.contains("escape"), "{e}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
