//! The child's library crates' surfaces equal their committed snapshots (`api/<crate>.txt`):
//! `tools/public-api.py check`, run when the pinned nightly (`api/NIGHTLY`, whose rustdoc JSON the snapshots were
//! made from) is installed. Without it the test says SKIPPED on stderr and passes: rustdoc JSON's schema moves between nightlies, so another nightly
//! cannot judge the snapshots. The snapshots are the aarch64-apple-darwin surface (the host this runs on); another
//! target's cfg surface gets its own snapshots when its port lands.
use std::path::Path;
use std::process::Command;

#[test]
fn the_public_crates_match_their_snapshots() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let pinned = std::fs::read_to_string(repo.join("api/NIGHTLY")).expect("api/NIGHTLY");
    let have = Command::new("rustc")
        .args(["+nightly", "-V"])
        .current_dir(&repo)
        .output();
    let have = match have {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => {
            eprintln!(
                "SKIPPED the_public_crates_match_their_snapshots: no nightly toolchain installed"
            );
            return;
        }
    };
    if have.trim() != pinned.trim() {
        eprintln!(
            "SKIPPED the_public_crates_match_their_snapshots: the installed nightly is {:?}, the snapshots' is {:?}",
            have.trim(),
            pinned.trim()
        );
        return;
    }
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        eprintln!(
            "SKIPPED the_public_crates_match_their_snapshots: the snapshots are aarch64-apple-darwin's"
        );
        return;
    }
    let out = Command::new("python3")
        .arg(repo.join("tools/public-api.py"))
        .arg("check")
        .current_dir(&repo)
        .env(
            "CARGO_TARGET_DIR",
            Path::new(env!("CARGO_TARGET_TMPDIR")).join("public-api"),
        )
        .output()
        .expect("python3 tools/public-api.py");
    assert!(
        out.status.success(),
        "the public API drifted from api/ (review, then tools/public-api.py gen <crate>):\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
