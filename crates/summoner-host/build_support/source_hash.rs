//! `tools/source-hash.sh` in Rust, byte for byte, for build hosts that cannot run the script (Windows: a `.sh` is not
//! a Win32 executable). `build.rs` uses it on Windows only; elsewhere it still runs the script, which stays the one
//! definition (the host application's stale-child guard runs the script as its configured source-hash command).
//! `tests/source_hash_rs.rs` holds the two equal on unix.
//!
//! The script, step by step, and what mirrors it here:
//! - the names: `Cargo.toml Cargo.lock rust-toolchain.toml tools/source-hash.sh testkit/plugins/vst3-sdk.pins`, then
//!   `find crates -type f ! -name '.DS_Store' ! -name '*.swp' ! -name '*~'` (regular files only, symlinks neither
//!   listed nor followed), relative to the repo root, `/`-separated;
//! - `LC_ALL=C sort`: byte order, duplicates kept;
//! - `xargs -0 shasum -a 256`: one line per name, `<64 hex>  <name>\n` (two spaces, text mode). shasum escapes a name
//!   holding `\` or a newline; such a name is refused here rather than escaped, so the two can never disagree silently;
//! - `printf 'SUMMONER_DEV_PLUGIN_ROOTS=%s\n'` with the variable's value (empty when unset);
//! - the sha256 of all of it, as 64 lowercase hex digits.

use std::path::Path;

/// The names the script lists before `crates/`.
const FIXED: [&str; 5] = [
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "tools/source-hash.sh",
    "testkit/plugins/vst3-sdk.pins",
];

/// The hash of the tree at `root`, with `lab_root` as the value of `SUMMONER_DEV_PLUGIN_ROOTS` (`""` when
/// unset).
pub fn source_hash(root: &Path, lab_root: &str) -> Result<String, String> {
    let mut names: Vec<String> = FIXED.iter().map(|s| s.to_string()).collect();
    walk(root, "crates", &mut names)?;
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let mut listing = Vec::new();
    for name in &names {
        if name.contains('\\') || name.contains('\n') {
            return Err(format!("source-hash: a name shasum would escape: {name:?}"));
        }
        let bytes =
            std::fs::read(root.join(name)).map_err(|e| format!("source-hash: {name}: {e}"))?;
        listing.extend_from_slice(crate::sha256::hex(&bytes).as_bytes());
        listing.extend_from_slice(b"  ");
        listing.extend_from_slice(name.as_bytes());
        listing.push(b'\n');
    }
    listing.extend_from_slice(format!("SUMMONER_DEV_PLUGIN_ROOTS={lab_root}\n").as_bytes());
    Ok(crate::sha256::hex(&listing))
}

/// `find <rel> -type f` with the script's three exclusions: every regular file below `root/rel`, as `/`-joined names.
fn walk(root: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), String> {
    let dir = root.join(rel);
    for entry in std::fs::read_dir(&dir).map_err(|e| format!("source-hash: {rel}: {e}"))? {
        let entry = entry.map_err(|e| format!("source-hash: {rel}: {e}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("source-hash: a name that is not UTF-8 in {rel}: {n:?}"))?;
        let child = format!("{rel}/{name}");
        // `find` without -L: the entry's own type, never its target's (std's file_type does not follow links).
        let t = entry
            .file_type()
            .map_err(|e| format!("source-hash: {child}: {e}"))?;
        if t.is_dir() {
            walk(root, &child, out)?;
        } else if t.is_file()
            && name != ".DS_Store"
            && !name.ends_with(".swp")
            && !name.ends_with('~')
        {
            out.push(child);
        }
    }
    Ok(())
}
