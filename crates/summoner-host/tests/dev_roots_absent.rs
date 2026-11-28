//! A hooks child built WITHOUT `SUMMONER_DEV_PLUGIN_ROOTS` has no dev fence: it refuses every `--plugin-root`, and no
//! fence path is compiled into it. Its own target dir under `target.noindex`, so this build never touches the others.
use std::path::Path;
use std::process::Command;

fn contains(bin: &Path, needle: &[u8]) -> bool {
    let bytes = std::fs::read(bin).unwrap_or_else(|e| panic!("{}: {e}", bin.display()));
    bytes.windows(needle.len()).any(|w| w == needle)
}

/// The fence's absent case: a hooks child built WITHOUT `SUMMONER_DEV_PLUGIN_ROOTS` (its own target dir)
/// refuses every `--plugin-root` with exit 2 before fd 3, naming the variable, and no fence path is compiled in.
#[test]
fn a_build_without_dev_roots_refuses_every_plugin_root() {
    let fence = summoner_sys::bundle::dev_fence()
        .expect("the tests are built with SUMMONER_DEV_PLUGIN_ROOTS");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = repo.join("target.noindex/no-dev-roots");
    let out = Command::new(env!("CARGO"))
        .current_dir(&repo)
        .args(["build", "-q", "--release", "-p", "summoner-host"])
        // The hooks child (the test hooks are opt-in): `--plugin-root` is one of its flags.
        .args(["--features", "test-hooks"])
        .env("CARGO_TARGET_DIR", &target)
        .env_remove("SUMMONER_DEV_PLUGIN_ROOTS")
        .output()
        .expect("cargo");
    assert!(
        out.status.success(),
        "the build without dev roots: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let child = target.join("release/summoner-host");
    let run = Command::new(&child)
        .args([
            "--plugin-root",
            &format!("{fence}/bin/test-plugins/0123abcd"),
        ])
        .output()
        .expect("the child");
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(run.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains(
            "this child was built without SUMMONER_DEV_PLUGIN_ROOTS: every plugin root is refused"
        ),
        "{stderr}"
    );
    assert!(
        !contains(&child, fence.as_bytes()),
        "the fence path {fence} is in a build made without it"
    );
    assert!(
        contains(&child, b"SUMMONER_DEV_PLUGIN_ROOTS"),
        "the control: the refusal text names the variable"
    );
}
