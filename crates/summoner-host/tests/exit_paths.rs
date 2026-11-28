//! This child ends with `_exit` (`die`, the reader's EOF, `shutdown`, the orphan watch), so no hosted plugin's C++
//! static destructor and no `atexit` handler runs while chain threads may still be inside plugin code.
//!
//! The fixture is the lab's ATEXIT bundle (testkit/plugins/vst3/lab_vst3.cpp): the gain with a static destructor that,
//! armed by `$HOME/summoner-lab-atexit.arm`, writes `$HOME/summoner-lab-atexit.ran` and then dies (labAbort: a SIGKILL under the
//! rig's `SUMMONER_LAB_DEATH=kill`, so no crash report). The marker file is the signal: it must never appear. Each case
//! loads a chain of it, processes blocks, and ends the child while the blocks keep coming:
//! - (a) the host application's side of the control socket shut (the child reads EOF) → exit 0;
//! - (b) the parent killed (the orphan watch) → the child gone within 2 s (its exit code is not observable here: an
//!   orphan is reaped by launchd, never by us; exit 3 itself is pinned by scan_mode.rs
//!   `an_orphaned_scan_child_exits_3`);
//! - (c) `shutdown` → exit 0.
//!
//! Every process a test starts is ended by its recorded pid (never by name).
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::collections::BTreeSet;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;
use summoner_sys::exit_watch::ExitWatch;

const B: usize = 64;
const KEY: u64 = 31;

/// The tests of this file one at a time: (a) finds its rig's socket by what is new among this process's descriptors.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The ATEXIT bundle's description (class id FUID(0x5341534C, 0x4C414201, 0x50, 0x46), as the bundle defines it).
fn atexit_xml() -> String {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414201, 0x50, 0x46].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-atexit.vst3"),
        &cid,
        "Summoner Lab Atexit",
    )
}

/// Arm the fixture in the rig's scratch home; the marker it would write.
fn arm(rig: &Rig) -> PathBuf {
    let home = rig.dir.join("home");
    std::fs::write(home.join("summoner-lab-atexit.arm"), b"armed\n").unwrap();
    home.join("summoner-lab-atexit.ran")
}

/// A one-slot ATEXIT chain, ready, and an engine driving it (noise → the chain → out), some blocks already through.
fn chain(rig: &mut Rig) -> Drive {
    let mut sp = spec(&[&atexit_xml()]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let link = match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) {
        Event::ChainReady { link, .. } => link,
        e => panic!("the ATEXIT chain did not load: {e:?}"),
    };
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], B);
    let mut heard = false;
    for _ in 0..64 {
        heard |= eng.block().iter().any(|x| *x != 0.0);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(heard, "the chain processes (the gain passes the noise)");
    eng
}

/// Blocks for `d` (the chain keeps being driven while the child ends).
fn keep_processing(eng: &mut Drive, d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        eng.block();
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// This process's unix sockets, by descriptor number (`/dev/fd`, each stat'ed through its own entry).
fn sockets() -> BTreeSet<u32> {
    let mut s = BTreeSet::new();
    for e in std::fs::read_dir("/dev/fd").unwrap().flatten() {
        let Some(n) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if std::fs::metadata(format!("/dev/fd/{n}")).is_ok_and(|m| m.file_type().is_socket()) {
            s.insert(n);
        }
    }
    s
}

/// The child's exit status, as the supervisor's reaper reports it.
fn exited(rig: &mut Rig) -> String {
    match rig.wait(|e| matches!(e, Event::ChildExited { .. })) {
        Event::ChildExited { status, .. } => status,
        _ => unreachable!(),
    }
}

/// (a) The host application's side of the control socket shut for writing, the supervisor otherwise untouched: the
/// child reads EOF (`n <= 0`) and `_exit(0)`s.
#[test]
fn eof_with_a_live_chain_exits_0_without_static_destructors() {
    if lab_plugins::absent("eof_with_a_live_chain_exits_0_without_static_destructors") {
        return;
    }
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let before = sockets();
    let mut rig = Rig::start("o1-eof");
    let ours: Vec<u32> = sockets().difference(&before).copied().collect();
    assert!(!ours.is_empty(), "the rig's control socket");
    let marker = arm(&rig);
    let mut eng = chain(&mut rig);
    // Each new socket is the host application's end (or a dup of it): a dup of each, shut for writing. The supervisor
    // keeps its own descriptors; only the child's reads end.
    let ends: Vec<UnixStream> = ours
        .iter()
        .filter_map(|n| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/fd/{n}"))
                .ok()
        })
        .map(|f| UnixStream::from(std::os::fd::OwnedFd::from(f)))
        .collect();
    let t = Instant::now();
    for s in &ends {
        let _ = s.shutdown(std::net::Shutdown::Write);
    }
    keep_processing(&mut eng, Duration::from_millis(300));
    let status = exited(&mut rig);
    println!("o1 eof: {status} after {:?}", t.elapsed());
    assert_eq!(status, "Exited(0)", "EOF exits 0");
    assert!(
        !marker.exists(),
        "a plugin's static destructor ran at the child's exit (exit, not _exit)"
    );
}

/// (c) `shutdown` with a live chain: the reply, then `_exit(0)`.
#[test]
fn shutdown_with_a_live_chain_exits_0_without_static_destructors() {
    if lab_plugins::absent("shutdown_with_a_live_chain_exits_0_without_static_destructors") {
        return;
    }
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let mut rig = Rig::start("o1-shutdown");
    let marker = arm(&rig);
    let mut eng = chain(&mut rig);
    rig.send(Command::Shutdown);
    keep_processing(&mut eng, Duration::from_millis(300));
    let status = exited(&mut rig);
    assert_eq!(status, "Exited(0)", "shutdown exits 0");
    assert!(
        !marker.exists(),
        "a plugin's static destructor ran at the child's exit (exit, not _exit)"
    );
}

/// The parent role of (b): a no-op in a normal run. Re-run by the test with `HC_O1_PARENT_OUT` set, it is the child's
/// supervisor (so its parent): an armed ATEXIT chain, processing. The host application's end of the control socket is
/// also held by a keeper (`/bin/sleep`, the socket as its stdin), so the parent's death closes nothing and the child
/// sees no EOF: only the orphan watch ends it (without the keeper, EOF and the watch race, two exits at once). It
/// writes the child's pid, the marker's path and the keepers' pids, then keeps processing until it is killed.
#[test]
fn o1_parent_role() {
    let Some(out) = std::env::var_os("HC_O1_PARENT_OUT") else {
        return;
    };
    let before = sockets();
    let mut rig = Rig::start("o1-orphan");
    let keepers: Vec<String> = sockets()
        .difference(&before)
        .filter_map(|n| {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!("/dev/fd/{n}"))
                .ok()?;
            let k = std::process::Command::new("/bin/sleep")
                .arg("120")
                .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(f)))
                .spawn()
                .ok()?;
            Some(k.id().to_string())
        })
        .collect();
    assert!(!keepers.is_empty(), "a keeper of the control socket");
    let marker = arm(&rig);
    let mut eng = chain(&mut rig);
    std::fs::write(
        &out,
        format!(
            "{}\n{}\n{}\n",
            rig.child_pid,
            marker.display(),
            keepers.join(" ")
        ),
    )
    .unwrap();
    loop {
        keep_processing(&mut eng, Duration::from_secs(3600));
    }
}

/// (b) The parent (the supervisor's process) SIGKILLed while the chain processes, the control socket still open (the
/// role's keeper): the orphan watch `_exit(3)`s; the static destructor never runs.
#[test]
fn a_killed_parent_with_a_live_chain_ends_the_child_without_static_destructors() {
    if lab_plugins::absent(
        "a_killed_parent_with_a_live_chain_ends_the_child_without_static_destructors",
    ) {
        return;
    }
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|p| p.into_inner());
    let dir = scratch("o1-parent");
    let out = dir.join("child");
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "o1_parent_role",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("HC_O1_PARENT_OUT", &out)
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    let (child, marker, keepers): (u32, PathBuf, Vec<u32>) = loop {
        if let Some(got) = std::fs::read_to_string(&out).ok().and_then(|s| {
            let mut l = s.lines();
            let p = l.next()?.trim().parse().ok()?;
            let m = PathBuf::from(l.next()?);
            let k: Vec<u32> = l
                .next()?
                .split(' ')
                .filter_map(|x| x.parse().ok())
                .collect();
            Some((p, m, k))
        }) {
            break got;
        }
        if Instant::now() > until {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("the parent role never loaded its chain");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let cw = ExitWatch::new(child).unwrap();
    assert!(
        !cw.wait(Some(Duration::from_millis(200))).unwrap(),
        "the child is up"
    );
    // Our own child, by its recorded pid.
    parent.kill().unwrap();
    parent.wait().unwrap();
    let t = Instant::now();
    let gone = cw.wait(Some(Duration::from_secs(2))).unwrap();
    if !gone {
        common::sigkill(child);
    }
    let ran = marker.exists();
    // The keepers: the role's children, by the pids it recorded (reparented to launchd when it died; never reaped
    // by us, so a pid is re-checked by its exit note before the signal).
    for k in keepers {
        if ExitWatch::new(k).is_ok_and(|w| !w.wait(Some(Duration::ZERO)).unwrap_or(true)) {
            common::sigkill(k);
        }
    }
    // The role's scratch directory: the role was killed, so it is ours to remove.
    if let Some(d) = marker.parent().and_then(Path::parent) {
        let _ = std::fs::remove_dir_all(d);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(gone, "the child outlived its killed parent by 2 s");
    println!(
        "o1 orphan: the child ended {:?} after its parent",
        t.elapsed()
    );
    assert!(
        !ran,
        "a plugin's static destructor ran at the child's exit (exit, not _exit)"
    );
}
