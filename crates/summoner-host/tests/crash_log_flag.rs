//! `--crash-log <dir>`, the crash
//! log's folder as the host application gives it. With the flag the log lands in that folder and not under `HOME`;
//! every bad folder is refused with exit 2 and a `--crash-log:` message, before fd 3 and before any plugin code (a
//! relative path, a missing folder, a symlink to a folder, a non-canonical `..` path, a file, a folder longer than the
//! handler's 512 bytes). Without the flag, `$HOME/Library/Logs` as before (child.rs's crash test).
mod common;

use common::*;
use std::path::Path;
use std::process::Command as Proc;
use summoner::supervisor::{Command, Config, Event};

/// The child alone (no fd 3) with `args`: its exit code and stderr. A refusal says `--crash-log:`; a good folder gets
/// past the check and then stops for want of fd 3 (exit 2 too, with another message).
fn bare(args: &[&str]) -> (Option<i32>, String) {
    let out = Proc::new(child_bin()).args(args).output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn every_bad_crash_log_folder_is_refused_with_exit_2() {
    let s = scratch("crash-log-refusals").canonicalize().unwrap();
    let good = s.join("good");
    std::fs::create_dir_all(&good).unwrap();
    std::os::unix::fs::symlink(&good, s.join("link")).unwrap();
    std::fs::write(s.join("a-file"), b"x").unwrap();
    // A canonical, existing folder longer than 512 bytes.
    let mut long = s.clone();
    while long.as_os_str().len() <= 512 {
        long = long.join("d".repeat(60));
    }
    std::fs::create_dir_all(&long).unwrap();
    let dotdot = format!("{}/good/../good", s.display());
    let cases: Vec<(&str, String)> = vec![
        ("relative", "relative/logs".into()),
        ("missing", s.join("missing").display().to_string()),
        ("symlink", s.join("link").display().to_string()),
        ("dotdot", dotdot),
        ("file", s.join("a-file").display().to_string()),
        ("too long", long.display().to_string()),
    ];
    for (name, dir) in &cases {
        let (code, err) = bare(&["--crash-log", dir]);
        assert_eq!(code, Some(2), "{name}: {err}");
        assert!(
            err.contains("--crash-log:"),
            "{name}: refused by the flag's own check: {err}"
        );
    }
    // The control: a good folder passes the check (the bare child then stops for want of fd 3).
    let (code, err) = bare(&["--crash-log", &good.display().to_string()]);
    assert_eq!(code, Some(2), "{err}");
    assert!(
        !err.contains("--crash-log:"),
        "a good folder is not refused: {err}"
    );
    let _ = std::fs::remove_dir_all(&s);
}

/// The flag's folder gets the log, `HOME` does not (child.rs's SIGSEGV in a plugin's set_state, through the host
/// application's supervisor, with `--crash-log`).
#[test]
fn a_crash_is_logged_in_the_crash_log_folder_not_under_home() {
    let home = scratch("crash-flag-home");
    let home_logs = home.join("Library/Logs");
    std::fs::create_dir_all(&home_logs).unwrap();
    let folder = scratch("crash-flag-folder").canonicalize().unwrap();
    let mut cfg = Config::new(child_bin(), SR, crate::common::state_hasher());
    for (k, v) in cfg.env.iter_mut() {
        if k == "HOME" || k == "CFFIXED_USER_HOME" {
            *v = home.clone().into();
        }
    }
    cfg.args = vec!["--crash-log".into(), folder.clone().into()];
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
    let name = summoner_proto::host_names::crash_log_name(pid);
    let text = std::fs::read_to_string(folder.join(&name))
        .unwrap_or_else(|e| panic!("{}: {e}", folder.join(&name).display()));
    assert!(
        text.contains(&format!(
            "\n{}\nSignal: SIGSEGV (11)\n",
            summoner_proto::host_names::CRASH_BANNER
        )),
        "{text}"
    );
    assert!(text.ends_with("plugin-op: setState segv\n"), "{text}");
    assert!(
        !Path::new(&home_logs.join(&name)).exists(),
        "nothing under HOME"
    );
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&folder);
}
