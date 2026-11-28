//! The diagnostic log (the child's side is a thin layer over runecraft-log): the child's diagnostic log through the
//! summoner supervisor. With `--log-dir`, `SUMMONER_LOG=host=trace,plugin=info` and `SUMMONER_LOG_SALT`:
//! `session.start`, `log.salt {source: "env"}`, `chain.load` (`corr` null, `chain` in fields), the slot's name hashed
//! under that salt (the exact value; never in plain text), one `chain.block` record per block from the chain thread's
//! RT ring (the host application's row for the child, with the chain key), `chain.unload`, and `session.end` on the
//! orderly exit; every line the schema's JSON. Without `SUMMONER_LOG` and the salt: only the log crate's own
//! `session.start`, `log.salt {source: "file"}` and `session.end` (every area at warn: no info, debug or trace record).
//! A crash with `--log-dir`: the crash log names the diagnostic file (`diag <file name> seq <n>`, the host
//! application's token). Logs only under the lab's scratch folders.
mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Config, Event};

const SECRET: &str = "secret-plugin-name-xyz";

fn lines(dir: &Path) -> Vec<serde_json::Value> {
    let cur = std::fs::read_link(dir.join("current")).expect("a current link");
    std::fs::read_to_string(dir.join(cur))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

/// A child with `--log-dir <dir>` and `log_spec` (None: unset) running a one-slot test-double chain for `blocks`
/// blocks through the host application's engine, then shut down in order (the Bridge's drop sends `shutdown`).
fn run(name: &str, log_spec: Option<&str>, salt: Option<&str>, blocks: usize) -> PathBuf {
    use crate::common::drive::{Bridged, Drive};
    let home = scratch(&format!("{name}-home"));
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    let dir = scratch(&format!("{name}-logs")).canonicalize().unwrap();
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    cfg.env
        .retain(|(k, _)| k != "SUMMONER_LOG" && k != "SUMMONER_LOG_SALT");
    if let Some(l) = log_spec {
        cfg.env.push(("SUMMONER_LOG".into(), l.into()));
    }
    if let Some(s) = salt {
        cfg.env.push(("SUMMONER_LOG_SALT".into(), s.into()));
    }
    cfg.args = vec!["--log-dir".into(), dir.clone().into()];
    let (bridge, rx) = start(cfg);
    let mut seen = Vec::new();
    let mut sp = spec(&[&format!(
        r#"{{"format":"fake","fake":{{"name":"{SECRET}"}}}}"#
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
    drop(bridge); // shutdown: the child's orderly exit, through the pre-exit flush
    // The child has left once its file ends with session.end (or 3 s).
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        if lines(&dir)
            .last()
            .is_some_and(|l| l["msg"] == "session.end")
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_dir_all(&home);
    dir
}

#[test]
fn with_a_level_spec_the_child_writes_its_records_hashed_with_rt_blocks_and_a_clean_end() {
    let dir = run(
        "diag-on",
        Some("host=trace,plugin=info"),
        Some("000102030405060708090a0b0c0d0e0f"),
        40,
    );
    let ls = lines(&dir);
    let msgs: Vec<&str> = ls.iter().map(|l| l["msg"].as_str().unwrap()).collect();
    eprintln!(
        "{} records: first {:?} last {:?}",
        ls.len(),
        &msgs[..msgs.len().min(6)],
        msgs.last()
    );
    assert_eq!(msgs[0], "session.start");
    let salt = ls
        .iter()
        .find(|l| l["msg"] == "log.salt")
        .expect("log.salt");
    assert_eq!(
        salt["fields"]["source"], "env",
        "SUMMONER_LOG_SALT taken: {salt}"
    );
    assert_eq!(
        *msgs.last().unwrap(),
        "session.end",
        "the orderly exit flushed and ended the session"
    );
    let load = ls
        .iter()
        .find(|l| l["msg"] == "chain.load")
        .expect("chain.load");
    assert!(load["corr"].is_null(), "corr null in phase 1");
    assert_eq!(load["fields"]["chain"], 41);
    assert_eq!(load["comp"], "host");
    assert_eq!(load["fields"]["ok"], true);
    let slot = ls
        .iter()
        .find(|l| l["msg"] == "slot.load")
        .expect("slot.load");
    assert_eq!(
        (
            slot["fields"]["chain"].as_u64(),
            slot["fields"]["slot"].as_u64()
        ),
        (Some(41), Some(0))
    );
    // the name under the host application's salt, as the host application would hash it (host application and child
    // join a plugin's records).
    let key: [u8; 16] = std::array::from_fn(|i| i as u8);
    assert_eq!(
        slot["fields"]["name"].as_str().unwrap(),
        runecraft_log::hash_keyed(&key, SECRET),
        "{slot}"
    );
    let blocks = ls.iter().filter(|l| l["msg"] == "chain.block").count();
    assert!(blocks >= 40, "one RT block record per block: {blocks}");
    let b = ls.iter().find(|l| l["msg"] == "chain.block").unwrap();
    assert_eq!(b["thread"], "hc-chain");
    assert_eq!(b["area"], "host");
    assert_eq!(b["level"], "trace");
    assert!(b["corr"].is_null());
    assert_eq!(b["fields"]["chain"], 41);
    assert_eq!(b["fields"]["frames"], 64);
    assert!(b["fields"]["ticks"].as_u64().is_some(), "{b}");
    assert!(b["fields"]["seq"].as_u64().is_some(), "{b}");
    assert!(
        msgs.contains(&"chain.prepare") && msgs.contains(&"chain.unload"),
        "{msgs:?}"
    );
    // the plugin's name never in plain text.
    let text = std::fs::read_to_string(dir.join(std::fs::read_link(dir.join("current")).unwrap()))
        .unwrap();
    assert!(!text.contains(SECRET), "a plain name in the log");
    // Every seq from 1, no gap.
    let seqs: Vec<u64> = ls.iter().map(|l| l["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=ls.len() as u64).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_a_level_spec_nothing_new_is_written() {
    let dir = run("diag-default", None, None, 20);
    let msgs: Vec<String> = lines(&dir)
        .iter()
        .map(|l| l["msg"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        msgs,
        ["session.start", "log.salt", "session.end"],
        "every area at warn: only summoner-log's own records"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The crash log names the diagnostic file and its last flushed seq (the design doc's 2.4).
#[test]
fn a_crash_log_names_the_diagnostic_file() {
    let home = scratch("diag-crash-home");
    let logs = home.join("Library/Logs");
    std::fs::create_dir_all(&logs).unwrap();
    let dir = scratch("diag-crash-logs").canonicalize().unwrap();
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    cfg.args = vec!["--log-dir".into(), dir.clone().into()];
    let (bridge, rx) = start(cfg);
    let pid = bridge.counters().child_pid();
    let mut seen = Vec::new();
    bridge
        .send(Command::Load {
            key: 9,
            spec: spec(&[r#"{"format":"fake","fake":{"name":"segv"}}"#]),
            states: vec![state(0, 1, "SEGV")],
        })
        .map_err(|_| "busy")
        .unwrap();
    wait_for(&rx, &mut seen, |e| {
        matches!(e, Event::ChainDown { key: 9, .. })
    });
    drop(bridge);
    let text = std::fs::read_to_string(logs.join(summoner_proto::host_names::crash_log_name(pid)))
        .unwrap();
    let line = text
        .lines()
        .find(|l| l.starts_with("diag "))
        .unwrap_or_else(|| panic!("{text}"));
    // The host application's token with the FILE NAME only: never the folder (it would put the account name into
    // bundles).
    let name = std::fs::read_link(dir.join("current")).unwrap();
    assert_eq!(
        line.rsplit_once(" seq ").map(|(a, _)| a.to_string()),
        Some(format!("diag {}", name.display())),
        "{line}"
    );
    assert!(!line.contains('/'), "no path in the crash line: {line}");
    // The seq is the last line that reached the file (`runecraft_log::flushed_seq`); the child died at the crash, so it
    // is the file's last line.
    let seq: u64 = line
        .rsplit_once(" seq ")
        .and_then(|(_, n)| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("{line}"));
    let last = lines(&dir).last().map(|l| l["seq"].as_u64().unwrap());
    assert_eq!(Some(seq), last, "{line}");
    assert!(seq > 0, "{line}");
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&dir);
}
