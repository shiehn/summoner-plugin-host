//! Every exit 2 happens before any byte on fd 3 (so before `hello`) and
//! before any plugin code, so the host application can treat it as engine-side (no verdict, no strike, no respawn
//! loop). The child is given a socket on fd 3 and each usage error (an unknown flag, a missing value, a bad
//! `--crash-log`, a bad `--step-ms`, in both modes): exit 2, and NOTHING is read from the socket. The control: a good
//! argv on the same harness gets past the usage checks (it then waits for the host application's `hello`, and exits 0
//! on the socket's EOF).
mod common;

use common::*;
use std::io::Read;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Reaps our child `pid` (the one this test spawned), SIGKILLing that pid if it has not exited within 5 s: a child that
/// is not refused waits for `hello` forever, and must fail the assertion, never hang the test.
fn reap_within(pid: u32) -> runecraft_ipc::process::ChildExit {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = done.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        if !d.load(std::sync::atomic::Ordering::SeqCst) {
            common::sigkill(pid);
        }
    });
    let exit = runecraft_ipc::process::unix::wait_child(pid).expect("reap");
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    exit
}

/// The child with `args`, fd 3 a socket: (its exit, the bytes it wrote on fd 3 before exiting).
fn run(args: &[&str]) -> (runecraft_ipc::process::ChildExit, Vec<u8>) {
    let home = scratch("pre-hello");
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    // Before the spawn: macOS refuses SO_RCVTIMEO (EINVAL) on a socket whose peer has already closed.
    ours.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
        ("HOME".into(), home.clone().into()),
        ("CFFIXED_USER_HOME".into(), home.clone().into()),
        ("SUMMONER_LAB_DEATH".into(), "kill".into()),
    ];
    let a: Vec<std::ffi::OsString> = args.iter().map(|x| (*x).into()).collect();
    let pid = runecraft_ipc::process::unix::spawn_stdio_only(
        &child_bin(),
        &a,
        &env,
        theirs.as_fd(),
        None,
    )
    .expect("spawn");
    drop(theirs);
    let exit = reap_within(pid);
    let mut got = Vec::new();
    let _ = ours.read_to_end(&mut got);
    let _ = std::fs::remove_dir_all(&home);
    (exit, got)
}

/// As [`run`], but the harness closes its end first, so a child past its usage checks reads EOF and leaves.
fn run_eof(args: &[&str]) -> (runecraft_ipc::process::ChildExit, ()) {
    let home = scratch("pre-hello-eof");
    let (ours, theirs) = UnixStream::pair().unwrap();
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
        ("HOME".into(), home.clone().into()),
        ("CFFIXED_USER_HOME".into(), home.clone().into()),
        ("SUMMONER_LAB_DEATH".into(), "kill".into()),
    ];
    let a: Vec<std::ffi::OsString> = args.iter().map(|x| (*x).into()).collect();
    let pid = runecraft_ipc::process::unix::spawn_stdio_only(
        &child_bin(),
        &a,
        &env,
        theirs.as_fd(),
        None,
    )
    .expect("spawn");
    drop(theirs);
    drop(ours);
    let exit = reap_within(pid);
    let _ = std::fs::remove_dir_all(&home);
    (exit, ())
}

#[test]
fn every_usage_error_exits_2_before_any_byte_on_fd_3() {
    let s = scratch("pre-hello-dirs").canonicalize().unwrap();
    let file = s.join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let file = file.display().to_string();
    let cases: Vec<Vec<&str>> = vec![
        vec!["--no-such-flag"],
        vec!["--crash-log"],
        vec!["--crash-log", "relative"],
        vec!["--crash-log", &file],
        vec!["--step-ms", "0"],
        vec!["--step-ms", "soon"],
        vec!["--step-ms", "999999999"],
        vec!["--scan", "--step-ms", "-5"],
        vec!["--scan", "--crash-log", "relative"],
        vec!["--scan", "--no-such-flag"],
        vec!["--log-dir", "relative"],
        vec!["--log-dir", &file],
        vec!["--scan", "--log-dir", "relative"],
    ];
    for args in &cases {
        let (exit, got) = run(args);
        assert!(
            matches!(exit, runecraft_ipc::process::ChildExit::Exited(2)),
            "{args:?}: {exit:?}"
        );
        assert!(
            got.is_empty(),
            "{args:?}: wrote {} bytes on fd 3 before exiting",
            got.len()
        );
    }
    // The control: good argvs (both modes) are not usage errors: with the harness's end closed, the child reads EOF
    // on fd 3 and leaves with something other than 2.
    for good in [
        vec!["--step-ms", "5000"],
        vec!["--crash-log", s.to_str().unwrap()],
        vec!["--scan", "--step-ms", "120000"],
    ] {
        let (exit, _) = run_eof(&good);
        assert!(
            !matches!(exit, runecraft_ipc::process::ChildExit::Exited(2)),
            "{good:?} is not a usage error: {exit:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&s);
}
