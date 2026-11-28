//! The cold launch (the host application's live_hello.rs `a_cold_launch_stays_within_500_ms`, to be re-measured with
//! the signed product child): how long the SIGNED, HARDENED shipped child
//! (tools/build-shipped.sh's dist/) takes from the start of `posix_spawn` to the verified hello, as the host
//! application's own supervisor measures it (`hello_launch_ns`). `#[ignore]`d (timing; run in an idle window):
//!
//! `cargo test -p summoner-host --test cold_launch -- --ignored --nocapture`
//!
//! Each trial: a copy of the dist child re-signed with a unique identifier (a new code directory hash, so the system
//! has never validated it: a cold first exec), launched once in a fresh process of this test binary (cold), then the
//! same copy again in another fresh process (the system warm). The host application's budgets: 500 ms cold, the hard
//! hello deadline 2 s (`HELLO_DEADLINE`).
mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use summoner::supervisor::Config;

#[test]
fn cold_child() {
    let Ok(bin) = std::env::var("SUMMONER_COLD_BIN") else {
        return; // only as the parent's child
    };
    let mut cfg = Config::new(PathBuf::from(bin), 48_000.0, crate::common::state_hasher());
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cold-launch-home");
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    let (b, _rx) = start(cfg);
    let ns = b.counters().hello_launch_ns();
    println!("COLD-LAUNCH-NS {ns}");
}

fn launch(bin: &Path) -> Duration {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "cold_child", "--nocapture", "--test-threads=1"])
        .env("SUMMONER_COLD_BIN", bin)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let ns: u64 = stdout
        .lines()
        .find_map(|l| l.split("COLD-LAUNCH-NS ").nth(1))
        .unwrap_or_else(|| {
            panic!(
                "no measurement:\n{stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .trim()
        .parse()
        .unwrap();
    Duration::from_nanos(ns)
}

#[test]
#[ignore = "timing: run in an idle window"]
fn the_signed_shipped_child_launches_within_the_cores_budget() {
    let dist = repo().join("target.noindex/shipped/dist");
    assert!(
        dist.join("summoner-host").is_file(),
        "tools/build-shipped.sh first"
    );
    let tmp =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("cold-launch-{}", std::process::id()));
    let (mut cold, mut warm) = (Vec::new(), Vec::new());
    // SUMMONER_COLD_TRIALS (default 5); SUMMONER_COLD_PLAIN=1: the control, the same copy signed ad hoc WITHOUT the hardened
    // runtime and the entitlements (as a lab build), to see what hardening costs.
    let trials: usize = std::env::var("SUMMONER_COLD_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let plain = std::env::var("SUMMONER_COLD_PLAIN").is_ok_and(|v| v == "1");
    for i in 0..trials {
        let d = tmp.join(format!("t{i}"));
        std::fs::create_dir_all(&d).unwrap();
        for f in ["summoner-host", "libsummoner_editor.dylib"] {
            std::fs::copy(dist.join(f), d.join(f)).unwrap();
        }
        let bin = d.join("summoner-host");
        let mut sign = Command::new("codesign");
        sign.args(["--force", "--sign", "-", "--identifier"])
            .arg(format!("summoner-host.cold.{}.{i}", std::process::id()));
        if !plain {
            sign.args(["--options", "runtime", "--entitlements"])
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("summoner-host.entitlements"));
        }
        assert!(sign.arg(&bin).status().unwrap().success(), "codesign");
        cold.push(launch(&bin));
        warm.push(launch(&bin));
    }
    let mut sorted = cold.clone();
    sorted.sort();
    println!(
        "{}: cold median {:?}, max {:?}",
        if plain { "plain (control)" } else { "hardened" },
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1]
    );
    println!("cold (a fresh signature, first exec): {cold:?}");
    println!("warm (the same copy again): {warm:?}");
    let _ = std::fs::remove_dir_all(&tmp);
    let max = |v: &[Duration]| v.iter().max().copied().unwrap();
    assert!(
        max(&cold) <= Duration::from_millis(500),
        "over H-1 (b)'s 500 ms: {cold:?}"
    );
    assert!(max(&warm) <= Duration::from_millis(500), "{warm:?}");
}
