//! The child's own log lines, frozen. The host application's log tests check what the child writes; this pins how it
//! writes it, byte for byte, so moving the child's logging (onto runecraft-log, its areas adopted from the parent's
//! `SUMMONER_LOG`) changes none of it:
//! `session.start` (with its `levels`, the host application's `describe` of all 16 areas), `log.salt`, the `host`
//! records (a slot's name hashed under the salt), the first `chain.block` of the chain thread's realtime ring,
//! `session.end`.
//!
//! The child is launched with the PRODUCTION spec: `SUMMONER_LOG` is what the host application's child
//! environment passes, the host application's `describe` of its launch levels (here `host=trace,plugin=info` over the
//! default), never a partial spec. It is a literal ([`PRODUCTION_SPEC`]), so the input outlives the child's logging
//! library.
//!
//! Stderr: [`the_childs_stderr_is_the_golden_one`] pins the child's own `SUMMONER_LOG` warning line (the child launched with
//! a refused spec and no descriptor 3). Not pinned here, a named gap: the drainer's echo of a warning record,
//! `summoner-host: {json}`, which the child writes only without `--log-dir` and only for a warning-level record of a running
//! chain (no such record is reachable from a test-double chain). Its machinery is the host application's
//! `summoner-<comp>: ` echo, pinned by runecraft-log's own tests; the child's comp word `host` is pinned in
//! every line of [`every_child_record_is_the_golden_one`].
//!
//! Normalised: `ts`, `mono`, `pid`, `seq` (dense `seq` is the host application's log tests' check), `session.end`'s
//! `records`, and the realtime fields `ticks`, `seq`, `rtSeq`. Only the first `chain.block` is kept, on a line of its
//! own: the ring's lines interleave with the message thread's by the 50 ms pull.
//!
//! **Re-recorded once, at the neutral-names decision:** the thread of the log's own
//! records (the golden's 3 control lines, now `summoner-log`), and in the stderr golden the levels variable (now
//! `SUMMONER_LOG`), the host's prefixes (now `[summoner-host]` / `summoner-host:`) and the no-descriptor line's "run me
//! through the bridge". Nothing else in either golden changed.
mod common;

use common::*;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Config, Event};

const GOLDEN: &str = include_str!("diag_log_bytes.txt");
const NAME: &str = "golden-plugin-name";
const SALT: &str = "000102030405060708090a0b0c0d0e0f";

fn lines(dir: &Path) -> Vec<String> {
    let cur = std::fs::read_link(dir.join("current")).expect("a current link");
    std::fs::read_to_string(dir.join(cur))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The host application's child log spec, as a literal: `describe` of the launch levels `host=trace,plugin=info` over
/// the default (every area named, in nibble order), exactly what the host application's child environment passes today.
/// A literal, not a call: the child's logging library may change, and the golden's input must not move with it.
const PRODUCTION_SPEC: &str = "wire=warn,control=warn,compile=warn,graph=warn,audio=warn,store=warn,bridge=warn,\
package=warn,writer=warn,capture=warn,arr=warn,host=trace,plugin=info,app=warn,rpc=warn,intent=warn";

/// The spec the child is given. `--cfg mutant_child_area_words_differ` (never in a normal build): the parent's area
/// words differ from the child's at one nibble (10, `arr` -> `arx`). A child that parses the words refuses the whole
/// spec (unknown area) and logs at the default; one that adopts the parent's words takes them. Either way the
/// golden's `session.start` `levels` line must turn red: the mutant reaches the area-words path (parse or adoption).
fn production_spec() -> String {
    let spec = PRODUCTION_SPEC.to_string();
    #[cfg(mutant_child_area_words_differ)]
    let spec = spec.replace(",arr=", ",arx=");
    spec
}

fn run(blocks: usize) -> PathBuf {
    use crate::common::drive::{Bridged, Drive};
    let home = scratch("diag-bytes-home");
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    let dir = scratch("diag-bytes-logs").canonicalize().unwrap();
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    cfg.env
        .retain(|(k, _)| k != "SUMMONER_LOG" && k != "SUMMONER_LOG_SALT");
    cfg.env
        .push(("SUMMONER_LOG".into(), production_spec().into()));
    cfg.env.push(("SUMMONER_LOG_SALT".into(), SALT.into()));
    cfg.args = vec!["--log-dir".into(), dir.clone().into()];
    let (bridge, rx) = start(cfg);
    let mut seen = Vec::new();
    let mut sp = spec(&[&format!(
        r#"{{"format":"fake","fake":{{"name":"{NAME}"}}}}"#
    )]);
    sp.geometry = summoner_proto::layout::Geometry {
        ch_in: 2,
        ..summoner_proto::layout::Geometry::stereo(64)
    };
    bridge
        .send(Command::Load {
            key: 41,
            spec: sp,
            states: vec![],
        })
        .map_err(|_| "busy")
        .unwrap();
    let Event::ChainReady { link, .. } = wait_for(&rx, &mut seen, |e| {
        matches!(
            e,
            Event::ChainReady { key: 41, .. } | Event::ChainDown { key: 41, .. }
        )
    }) else {
        panic!("the chain: {seen:?}")
    };
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], 64);
    for k in 0..blocks {
        eng.block();
        assert!(
            null_harness::settle(&link, Duration::from_millis(600)),
            "block {k}"
        );
    }
    bridge
        .send(Command::Unload { key: 41 })
        .map_err(|_| "busy")
        .unwrap();
    std::thread::sleep(Duration::from_millis(200));
    drop(bridge);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if lines(&dir)
            .last()
            .is_some_and(|l| l.contains("\"msg\":\"session.end\""))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&home);
    dir
}

/// `"key":<value>` (a number or a string) replaced by `"key":<with>`, every time.
fn zero(l: &str, key: &str, with: &str) -> String {
    let pat = format!("\"{key}\":");
    let mut out = String::new();
    let mut rest = l;
    while let Some(at) = rest.find(&pat) {
        let start = at + pat.len();
        let tail = &rest[start..];
        let end = if let Some(r) = tail.strip_prefix('"') {
            1 + r.find('"').unwrap() + 1
        } else {
            tail.find([',', '}']).unwrap()
        };
        out.push_str(&rest[..start]);
        out.push_str(with);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn every_child_record_is_the_golden_one() {
    let dir = run(3);
    let mut out = String::new();
    let mut block = None;
    for l in lines(&dir) {
        let v: serde_json::Value =
            serde_json::from_str(&l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}"));
        let msg = v["msg"].as_str().unwrap();
        let mut n = zero(&l, "ts", "\"TS\"");
        n = zero(&n, "mono", "0");
        n = zero(&n, "pid", "0");
        n = zero(&n, "seq", "\"S\"");
        n = zero(&n, "records", "\"N\"");
        if msg == "chain.block" {
            // The ring is pulled every 50 ms: where its lines fall among the message thread's varies. The first one
            // is pinned on its own line, after the rest.
            n = zero(&n, "ticks", "\"T\"");
            n = zero(&n, "rtSeq", "\"R\"");
            block.get_or_insert(n);
            continue;
        }
        writeln!(out, "line {n}").unwrap();
    }
    // A missing block line is a golden mismatch (reported after the lines before it), never an early panic.
    writeln!(out, "block {}", block.unwrap_or_else(|| "<none>".into())).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    if std::env::var_os("SUMMONER_GOLDEN_PRINT").is_some() {
        std::fs::write(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/diag_log_bytes.txt"),
            &out,
        )
        .unwrap();
        return;
    }
    let (got, want): (Vec<&str>, Vec<&str>) = (out.lines().collect(), GOLDEN.lines().collect());
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "child golden line {i}");
    }
    assert_eq!(got.len(), want.len(), "child golden line count");
}

/// The child's own stderr at start: a refused `SUMMONER_LOG` is a warning and the default (never an exit), then, run outside
/// a bridge (no descriptor 3), its usage exit. Both lines byte for byte.
#[test]
fn the_childs_stderr_is_the_golden_one() {
    let home = scratch("diag-bytes-stderr-home");
    let out = std::process::Command::new(child_bin())
        .env("HOME", &home)
        .env("CFFIXED_USER_HOME", &home)
        .env("SUMMONER_LOG", "all=info,nope=debug")
        .env_remove("SUMMONER_LOG_SALT")
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8(out.stderr).unwrap();
    assert_eq!(
        err,
        "[summoner-host] SUMMONER_LOG: unknown area \"nope\" (the default applies)\n\
         summoner-host: no descriptor 3 (run me through the bridge)\n",
        "{err}"
    );
}
