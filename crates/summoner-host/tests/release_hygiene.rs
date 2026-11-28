//! The release build's hygiene, in the generic checks any release of this host must pass:
//! - **no lab strings:** a child built WITHOUT `test-hooks` (the release feature set; its own target dir under
//!   `target.noindex`, so this build never touches the others) holds neither the test double (`summoner_testproc`)
//!   nor its knobs (`BAREGET`), nor the lab's death switch or test hooks; the positive control is the test build,
//!   which holds them all;
//! - **no unexpected linked library:** neither child, release or test build, links AppKit, CoreGraphics or libobjc,
//!   and `summoner-sys-ui` is not in its dependency graph (the scan probe's graph has it: the control);
//! - the child resolves an editor dylib that exists.
//!
//! The no-home-paths check of a release binary is `no_home_paths.rs`; the fence's absent case is `dev_roots_absent.rs`.
use std::path::{Path, PathBuf};
use std::process::Command;

fn contains(bin: &Path, needle: &[u8]) -> bool {
    let bytes = std::fs::read(bin).unwrap_or_else(|e| panic!("{}: {e}", bin.display()));
    bytes.windows(needle.len()).any(|w| w == needle)
}

fn release_child() -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = repo.join("target.noindex/release-hygiene");
    let out = Command::new(env!("CARGO"))
        .current_dir(&repo)
        .args([
            "build",
            "-q",
            "--release",
            "--no-default-features",
            "-p",
            "summoner-host",
        ])
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "the release build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target.join("release/summoner-host")
}

#[test]
fn a_release_child_has_no_bareget_and_no_test_double() {
    let test_build = PathBuf::from(env!("CARGO_BIN_EXE_summoner-host"));
    assert!(
        contains(&test_build, b"BAREGET"),
        "the control: the test build has the knob"
    );
    assert!(
        contains(&test_build, b"summoner_testproc"),
        "the control: the test build has the double"
    );
    let release = release_child();
    assert!(
        !contains(&release, b"BAREGET"),
        "BAREGET in the release child"
    );
    assert!(
        !contains(&release, b"summoner_testproc"),
        "the test double in the release child"
    );
    // The lab's death switch is lab code only; the release child never reads it.
    assert!(
        contains(&test_build, b"SUMMONER_LAB_DEATH"),
        "the control: the test build has the death switch"
    );
    assert!(
        !contains(&release, b"SUMMONER_LAB_DEATH"),
        "SUMMONER_LAB_DEATH in the release child"
    );
    // The oversized-scan test hook is test-hooks only; the release child never reads it.
    assert!(
        contains(&test_build, b"SUMMONER_TEST_SCAN_REPEAT"),
        "the control: the test build has the scan-repeat hook"
    );
    assert!(
        !contains(&release, b"SUMMONER_TEST_SCAN_REPEAT"),
        "SUMMONER_TEST_SCAN_REPEAT in the release child"
    );
}

/// Builds `package` in the release dir of its own (as `release_child`, so the outer test build's lock is not needed),
/// with `args`; returns the binary `bin`.
fn release_bin(package: &str, bin: &str, args: &[&str]) -> PathBuf {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = repo.join("target.noindex/release-hygiene");
    let out = Command::new(env!("CARGO"))
        .current_dir(&repo)
        .args(["build", "-q", "--release", "-p", package])
        .args(args)
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "{package}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    target.join("release").join(bin)
}

/// `otool -L`'s load commands of `bin`.
fn links(bin: &Path) -> String {
    let out = Command::new("otool")
        .arg("-L")
        .arg(bin)
        .output()
        .expect("otool");
    assert!(out.status.success(), "otool -L {}", bin.display());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `cargo tree -e normal -p <package>` (the dependency graph a build of it links, dev-dependencies excluded).
fn tree(package: &str, args: &[&str]) -> String {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new(env!("CARGO"))
        .current_dir(&repo)
        .args(["tree", "--offline", "-e", "normal", "-p", package])
        .args(args)
        .output()
        .expect("cargo tree");
    assert!(
        out.status.success(),
        "cargo tree {package}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

const UI_LINKS: [&str; 3] = ["AppKit.framework", "CoreGraphics.framework", "libobjc"];

/// The link guard: the host child, release AND test build, links no AppKit, no
/// CoreGraphics, no libobjc (a summoner-sys feature for the probe's UI surfaces once unified all three into it); and the
/// reason holds: summoner-sys-ui is not in the host child's dependency graph (the probe's graph has it, the control).
/// The probe links neither AppKit nor CoreGraphics either (sys-ui's surfaces are all off). Nothing is executed:
/// builds, `otool -L` and `cargo tree` only.
#[test]
fn the_host_child_never_links_the_ui_surfaces() {
    let test_build = PathBuf::from(env!("CARGO_BIN_EXE_summoner-host"));
    let release = release_child();
    let probe = release_bin("summoner-probe-bin", "summoner-scan-probe", &[]);
    let (t, r, p) = (links(&test_build), links(&release), links(&probe));
    for l in UI_LINKS {
        assert!(!t.contains(l), "{l} in the test-build child:\n{t}");
        assert!(!r.contains(l), "{l} in the release child:\n{r}");
    }
    // The probe links neither (no WindowServer contact from a scan); the control that otool's list is read at all: every one of these binaries links CoreFoundation.
    for l in ["AppKit.framework", "CoreGraphics.framework"] {
        assert!(
            !p.contains(l),
            "{l} in the probe (no NSApplication, no window listing):\n{p}"
        );
    }
    for (name, list) in [
        ("test-build child", &t),
        ("release child", &r),
        ("probe", &p),
    ] {
        assert!(
            list.contains("CoreFoundation.framework"),
            "the control: {name} links CoreFoundation:\n{list}"
        );
    }
    let (child, child_release, probe_tree) = (
        tree("summoner-host", &["--features", "test-hooks"]),
        tree("summoner-host", &["--no-default-features"]),
        tree("summoner-probe-bin", &[]),
    );
    assert!(
        !child.contains("summoner-sys-ui"),
        "summoner-sys-ui in the child's graph:\n{child}"
    );
    assert!(
        !child_release.contains("summoner-sys-ui"),
        "summoner-sys-ui in the release child's graph:\n{child_release}"
    );
    assert!(
        probe_tree.contains("summoner-sys-ui"),
        "the control: the probe's graph has it:\n{probe_tree}"
    );
}

/// `--editor-dylib-path` prints the editor dylib the child would load; in the lab build it exists (the
/// compiled-in build path, or a copy beside the executable). The shipped side is tools/build-shipped.sh's check (the
/// installed child resolves the copy beside it).
#[test]
fn the_child_resolves_an_editor_dylib_that_exists() {
    let out = Command::new(env!("CARGO_BIN_EXE_summoner-host"))
        .arg("--editor-dylib-path")
        .output()
        .expect("the child");
    assert!(out.status.success());
    let path = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(path.ends_with("/libsummoner_editor.dylib"), "{path}");
    assert!(Path::new(&path).is_file(), "{path} is not a file");
}
