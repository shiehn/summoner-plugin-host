//! Embeds this repo's source hash (`tools/source-hash.sh`) as `SUMMONER_HOST_SOURCE_HASH`, for `--source-hash`.
//! On Windows, where a `.sh` cannot be executed, the same hash comes from `build_support/source_hash.rs` (the script
//! in Rust, byte for byte; `tests/source_hash_rs.rs` holds the two equal on unix).
use std::path::Path;
#[cfg(not(windows))]
use std::process::Command;

/// The hasher `build_support/source_hash.rs` names as `crate::sha256` on Windows: the `sha2` crate (already in the
/// lockfile), so the build script reads no other crate's source by path. The digest is the same
/// FIPS 180-4 SHA-256 as summoner-sys's, which `tests/source_hash_rs.rs` uses on unix.
#[cfg(windows)]
mod sha256 {
    /// The lowercase hex SHA-256 of `data`.
    pub fn hex(data: &[u8]) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}
#[cfg(windows)]
#[path = "build_support/source_hash.rs"]
mod source_hash;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let hash = hash(&root);
    println!("cargo:rustc-env=SUMMONER_HOST_SOURCE_HASH={hash}");
    for p in [
        "crates",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "tools/source-hash.sh",
        "testkit/plugins/vst3-sdk.pins",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(p).display());
    }
    // The hash covers the compiled-in dev fence (summoner-sys's `SUMMONER_DEV_PLUGIN_ROOTS`).
    println!("cargo:rerun-if-env-changed=SUMMONER_DEV_PLUGIN_ROOTS");
}

#[cfg(not(windows))]
fn hash(root: &Path) -> String {
    let script = root.join("tools/source-hash.sh");
    let out = Command::new(&script)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", script.display()));
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success() && hash.len() == 64,
        "{} failed: {}",
        script.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    hash
}

#[cfg(windows)]
fn hash(root: &Path) -> String {
    let lab = std::env::var_os("SUMMONER_DEV_PLUGIN_ROOTS").unwrap_or_default();
    let lab = lab
        .to_str()
        .unwrap_or_else(|| panic!("SUMMONER_DEV_PLUGIN_ROOTS is not UTF-8: {lab:?}"));
    source_hash::source_hash(root, lab).unwrap_or_else(|e| panic!("{e}"))
}
