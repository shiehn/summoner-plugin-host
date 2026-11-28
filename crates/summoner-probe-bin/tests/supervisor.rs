//! The lab harness supervisor (tests/support/supervisor.rs) against the scan semantics, on the lab's own
//! bundles and two shell stand-ins (for the process-group rule). See the support module for what it does.

#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
#[path = "support/supervisor.rs"]
mod supervisor;

use std::path::{Path, PathBuf};
use std::time::Duration;
use summoner_proto::codec::{ScanFailure, ScanResult, TimeoutKind};
use supervisor::{Supervisor, group_members, kill_ours, lab_scan};

const START_UP: &[&str] = &[
    "com.apple.logd",
    "com.apple.system.notification_center",
    "com.apple.system.opendirectoryd.libinfo",
];
const AU: &[&str] = &[
    "com.apple.audio.AudioComponentRegistrar",
    "com.apple.cfprefsd.agent",
    "com.apple.cfprefsd.daemon",
];

fn target() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target.noindex")
        .canonicalize()
        .unwrap()
}

fn plugins() -> PathBuf {
    lab_plugins::dir()
}

/// A fresh scratch HOME for one test (removed first, so a crashed run leaves nothing that is read).
fn home(name: &str) -> PathBuf {
    let h = target().join(format!("sup-{name}"));
    let _ = std::fs::remove_dir_all(&h);
    std::fs::create_dir_all(&h).unwrap();
    h
}

/// The release host child (no test hooks: the product surface), built as the release-build link guard builds it, in its
/// own target dir (this test's build lock is not needed). `summoner-host --scan` is the child backend.
fn host_child() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let repo = target().parent().unwrap().to_path_buf();
            let dir = repo.join("target.noindex/no-hooks");
            let out = std::process::Command::new(env!("CARGO"))
                .current_dir(&repo)
                .args([
                    "build",
                    "-q",
                    "--release",
                    "--no-default-features",
                    "-p",
                    "summoner-host",
                ])
                .env("CARGO_TARGET_DIR", &dir)
                .output()
                .expect("cargo");
            assert!(
                out.status.success(),
                "the child's build: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            dir.join("release/summoner-host")
        })
        .clone()
}

/// The supervisor over the probe (`child: false`) or over `summoner-host --scan` (`child: true`).
fn backend(home: PathBuf, budget: Duration, step_ms: u64, child: bool) -> Supervisor {
    Supervisor {
        program: if child {
            host_child()
        } else {
            PathBuf::from(env!("CARGO_BIN_EXE_summoner-scan-probe"))
        },
        pre_args: if child {
            vec!["--scan".into()]
        } else {
            Vec::new()
        },
        child_mode: child,
        home,
        budget,
        step_ms: Some(step_ms),
        recorders: Some((
            plugins().join("libsummoner_lab_fswatch.dylib"),
            plugins().join("libsummoner_lab_lookwatch.dylib"),
        )),
        spawned: 0,
    }
}

fn probe(home: PathBuf, budget: Duration, step_ms: u64) -> Supervisor {
    backend(home, budget, step_ms, false)
}

/// The two backends, each case run against both (the probe stays as a CLI test tool beside the child's mode).
const BACKENDS: [(bool, &str); 2] = [(false, "probe"), (true, "child")];

fn lookups_clean(what: &str, look: &str, allowed: &[&str]) {
    assert!(
        !look.contains("windowserver"),
        "{what}: a WindowServer lookup:\n{look}"
    );
    for l in look.lines() {
        let s = l.rsplit(' ').next().unwrap_or("");
        assert!(
            START_UP.contains(&s) || allowed.contains(&s),
            "{what}: a new lookup (a STOP): {l}"
        );
    }
}

/// Each lab bundle once, through its own scan process: ok, crashed (a signal the supervisor did not send: the hostile
/// bundles' constructor deaths, SIGKILL under SUMMONER_LAB_DEATH), failed; no process left behind; lookups as allowed.
#[test]
fn each_lab_bundle_maps_to_its_scan_result() {
    if lab_plugins::absent("each_lab_bundle_maps_to_its_scan_result") {
        return;
    }
    for (child, which) in BACKENDS {
        let mut sup = backend(
            home(&format!("map-{which}")),
            Duration::from_secs(30),
            10_000,
            child,
        );
        for (rel, allowed) in [
            ("vst3/summoner-lab-gain.vst3", &[][..]),
            ("vst3/summoner-lab-synth.vst3", &[][..]),
            ("au/summoner-lab-gain.component", AU),
            ("au/summoner-lab-synth.component", AU),
        ] {
            let o = sup.scan_once(&lab_scan(&plugins(), rel));
            match &o.result {
                ScanResult::Ok { descriptions } => assert_eq!(descriptions.len(), 1, "{rel}"),
                r => panic!("{rel}: {r:?}"),
            }
            assert!(o.leftovers.is_empty(), "{rel}: {:?}", o.leftovers);
            // The positive control that the recorders reached the scan process (through /bin/sh in child mode).
            assert!(
                !o.lookups.is_empty(),
                "{which} {rel}: the lookup recorder saw nothing (not injected?)"
            );
            lookups_clean(rel, &o.lookups, allowed);
        }
        for (rel, allowed) in [
            ("vst3/summoner-lab-hostile.vst3", &[][..]),
            ("au/summoner-lab-hostile.component", AU),
        ] {
            let o = sup.scan_once(&lab_scan(&plugins(), rel));
            assert_eq!(
                o.result,
                ScanResult::crashed_by(summoner_proto::codec::OsCode::Signal(9)),
                "{rel}: a SIGKILL the supervisor did not send is a crash, never a timeout"
            );
            assert!(o.leftovers.is_empty(), "{rel}: {:?}", o.leftovers);
            lookups_clean(rel, &o.lookups, allowed);
        }
        let o = sup.scan_once(&lab_scan(&plugins(), "au/summoner-lab-drag.component"));
        assert!(
            matches!(&o.result, ScanResult::Failed { failure: ScanFailure::Instantiate, detail } if detail.contains("STOP")),
            "{:?}",
            o.result
        );
        assert_eq!(sup.spawned, 7, "{which}: one process per scan");
    }
}

/// The two timeouts: the probe's own step deadline (exit 75) and the supervisor's budget, which SIGKILLs the
/// recorded GROUP and reaps it: Timeout {the budget}, not Crashed, although the probe died of SIGKILL.
#[test]
fn a_hang_times_out_by_the_probes_deadline_or_by_the_budget() {
    if lab_plugins::absent("a_hang_times_out_by_the_probes_deadline_or_by_the_budget") {
        return;
    }
    for (child, which) in BACKENDS {
        let mut sup = backend(
            home(&format!("hang-{which}")),
            Duration::from_secs(30),
            800,
            child,
        );
        let o = sup.scan_once(&lab_scan(&plugins(), "vst3/summoner-lab-hang.vst3"));
        assert_eq!(
            o.result,
            ScanResult::Timeout {
                kind: TimeoutKind::Step
            },
            "the probe's deadline (exit 75)"
        );
        let mut sup = backend(
            home(&format!("budget-{which}")),
            Duration::from_secs(2),
            60_000,
            child,
        );
        let o = sup.scan_once(&lab_scan(&plugins(), "vst3/summoner-lab-hang.vst3"));
        assert_eq!(
            o.result,
            ScanResult::Timeout {
                kind: TimeoutKind::Budget
            },
            "the budget, by the supervisor's own kill"
        );
        assert!(
            o.elapsed >= Duration::from_secs(2) && o.elapsed < Duration::from_secs(5),
            "{:?}",
            o.elapsed
        );
        assert!(
            o.leftovers.is_empty() && group_members(o.pgid).is_empty(),
            "{which}: the group is gone"
        );
    }
}

fn stand_in(name: &str, script: &str, budget: Duration) -> Supervisor {
    Supervisor {
        program: PathBuf::from("/bin/sh"),
        pre_args: vec!["-c".into(), script.into(), "sh".into()],
        home: home(name),
        budget,
        step_ms: None,
        recorders: None,
        spawned: 0,
        child_mode: false,
    }
}

/// The group rule, fixture-free: a stand-in "probe" that forks a sleeping child. Killed at the budget, the group
/// goes, the child with it. And a stand-in that exits leaving a child behind: the leftovers rule names it (and then
/// kills it): the positive control that the check sees a survivor.
#[test]
fn the_group_is_killed_whole_and_a_leftover_is_named() {
    let scan = lab_scan(&plugins(), "vst3/summoner-lab-gain.vst3"); // only its path is passed (and ignored)
    let mut sup = stand_in("group", "sleep 30 & sleep 30", Duration::from_secs(1));
    let o = sup.scan_once(&scan);
    assert_eq!(
        o.result,
        ScanResult::Timeout {
            kind: TimeoutKind::Budget
        }
    );
    assert!(o.leftovers.is_empty(), "{:?}", o.leftovers);
    assert!(
        group_members(o.pgid).is_empty(),
        "the forked child went with the group"
    );
    let mut sup = stand_in("leftover", "sleep 30 & exit 0", Duration::from_secs(10));
    let o = sup.scan_once(&scan);
    // The survivor is the forked `sleep`; caught before its exec it is still the forked shell (`/bin/sh`): the same
    // process either way (a timing flake once showed `["1406 /bin/sh"]`).
    assert!(
        o.leftovers
            .iter()
            .any(|l| l.contains("sleep") || l.ends_with("/bin/sh")),
        "the survivor is named: {:?}",
        o.leftovers
    );
    assert!(group_members(o.pgid).is_empty(), "and then killed");
    assert!(
        matches!(
            o.result,
            ScanResult::Crashed {
                os_code: None,
                exit_code: Some(0),
                ..
            }
        ),
        "no Scanned line: a crash, never parsed loosely: {:?}",
        o.result
    );
}

/// The pedal: `pending` is written BEFORE a probe exists; a pending entry at load (a supervisor that died) is a
/// crash, and not re-probed. One strike: a second pass spawns nothing; a rescan that NAMES a path re-probes it.
/// The pedal file is only in the scratch HOME: no file this test writes is left anywhere else (a before/after diff,
/// so a stray left by some other run before it starts is not this test's).
#[test]
fn the_pedal_and_one_strike() {
    if lab_plugins::absent("the_pedal_and_one_strike") {
        return;
    }
    let before = scan_files_outside_scratch(&target().join("sup-pedal"));
    let h = home("pedal");
    let gain = lab_scan(&plugins(), "vst3/summoner-lab-gain.vst3");
    let hostile = lab_scan(&plugins(), "vst3/summoner-lab-hostile.vst3");
    // A supervisor that died mid-scan of `gain` left this behind.
    std::fs::write(
        h.join("plugin-scan.json"),
        format!(
            r#"{{"{}": {{"state": "pending", "mtimeMs": 1}}}}"#,
            gain.path()
        ),
    )
    .unwrap();
    let mut sup = probe(h.clone(), Duration::from_secs(30), 10_000);
    let m = sup.pass(&[gain.clone(), hostile.clone()], &[]);
    assert_eq!(
        m[gain.path()].result,
        Some(ScanResult::crashed_unknown()),
        "the dead-man's pedal"
    );
    assert_eq!(sup.spawned, 1, "gain was not re-probed; hostile was");
    assert!(matches!(
        m[hostile.path()].result,
        Some(ScanResult::Crashed {
            os_code: Some(summoner_proto::codec::OsCode::Signal(9)),
            ..
        })
    ));
    let m2 = sup.pass(&[gain.clone(), hostile.clone()], &[]);
    assert_eq!(sup.spawned, 1, "one strike: nothing re-probed");
    assert_eq!(m2, sup.load());
    let m3 = sup.pass(&[gain.clone(), hostile.clone()], &[gain.path()]);
    assert_eq!(sup.spawned, 2, "named: re-probed");
    assert!(matches!(
        m3[gain.path()].result,
        Some(ScanResult::Ok { .. })
    ));
    // The pending mark is ON DISK before the scan process exists: a stand-in "probe" looks for it while it runs
    // (exit 3: seen; exit 4: not), through the same pass.
    let mut seer = Supervisor {
        program: PathBuf::from("/bin/sh"),
        pre_args: vec![
            "-c".into(),
            r#"grep -q '"pending"' "$HOME/plugin-scan.json" && exit 3; exit 4"#.into(),
            "sh".into(),
        ],
        home: h.clone(),
        budget: Duration::from_secs(10),
        step_ms: None,
        recorders: None,
        spawned: 0,
        child_mode: false,
    };
    let synth = lab_scan(&plugins(), "vst3/summoner-lab-synth.vst3");
    let m4 = seer.pass(std::slice::from_ref(&synth), &[]);
    assert_eq!(
        m4[synth.path()].result,
        Some(ScanResult::exited_early(3)),
        "the scan process saw its own pending entry"
    );
    // And done afterwards: nothing pending on disk.
    let text = std::fs::read_to_string(h.join("plugin-scan.json")).unwrap();
    assert!(!text.contains("pending"), "{text}");
    // The pedal file only under the scratch HOMEs of these tests. One exclusion, anchored at its root: the host
    // application's test scratch root (its scan record tests); nothing in this repo writes there. Its concurrent runs
    // leave dirs there mid-run, so the exclusion stays even once the host application cleans up after itself;
    // everywhere else in the lab home, a stray pedal file still fails. A second, likewise anchored: the host
    // application's plugin-scan tests' scratch root, whose tests are to remove their scratch.
    // The lab HOME (the test runs with it), read at run time.
    // Mutant: this test leaves a pedal file outside its scratch HOME; the diff must fail it. Removed on drop
    // (the failing assert unwinds), so the mutant run leaves nothing either.
    #[cfg(mutant_pedal_leaves_stray)]
    let _stray = Stray::leave();
    let after = scan_files_outside_scratch(&h);
    let new: Vec<&String> = after.iter().filter(|p| !before.contains(p)).collect();
    assert!(
        new.is_empty(),
        "plugin-scan.json outside a scratch HOME, written during this test: {new:?}"
    );
}

/// Every pedal file under this repo and the lab home, except the scratch HOMEs of these tests and the host
/// application's two anchored test scratch roots, sorted. `h` is this test's scratch HOME (it must not
/// be under a host application's root, or the exclusions would hide its writes).
fn scan_files_outside_scratch(h: &Path) -> Vec<String> {
    let lab_home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let core_scratch = [
        lab_home.join("tmp/scan-record-tests"),
        lab_home.join("tmp/plugin-scan-tests"),
    ];
    // The exclusions must never hide our own writes: no scratch path of this repo is under a host application's root.
    for ours in [h, &target()] {
        for root in &core_scratch {
            assert!(
                !ours.starts_with(root),
                "{} is under the host application's test scratch root {}",
                ours.display(),
                root.display()
            );
        }
    }
    let out = std::process::Command::new("/usr/bin/find")
        .args([
            target().parent().unwrap().to_str().unwrap(),
            lab_home.to_str().unwrap(),
            "-name",
            "plugin-scan.json",
            "-not",
            "-path",
            "*/target.noindex/sup-*",
            "-not",
            "-path",
            &format!("{}/*", core_scratch[0].display()),
            "-not",
            "-path",
            &format!("{}/*", core_scratch[1].display()),
        ])
        .output()
        .unwrap();
    let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    files.sort();
    files
}

/// The mutant's stray: a pedal file under the lab home's tmp, outside every scratch root, removed on drop.
#[cfg(mutant_pedal_leaves_stray)]
struct Stray(PathBuf);

#[cfg(mutant_pedal_leaves_stray)]
impl Stray {
    fn leave() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
        let d = home
            .join("tmp")
            .join(format!("pedal-stray-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin-scan.json"), "{}").unwrap();
        Stray(d)
    }
}

#[cfg(mutant_pedal_leaves_stray)]
impl Drop for Stray {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The kill guard: a group kill is sent only to a group that is provably ours, its leader still our unreaped
/// child. A process of ours that is NOT our child (a grandchild, so not a leader we recorded) is refused, and so are
/// 0 and 1 (`kill -- -1` would mean every process). Never tried on any process but our own grandchild.
#[test]
fn a_group_kill_is_refused_unless_the_group_is_ours() {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let mut sh = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 30 & echo $!; wait"])
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = sh.id();
    let mut line = String::new();
    std::io::BufReader::new(sh.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let grandchild: u32 = line.trim().parse().unwrap();
    assert!(
        !kill_ours(0) && !kill_ours(1),
        "0 and 1 are never signalled"
    );
    assert!(
        !kill_ours(grandchild),
        "not our child, not a group we recorded: refused"
    );
    assert!(
        group_members(pgid).iter().any(|(p, _)| *p == grandchild),
        "and it is still alive"
    );
    assert!(
        kill_ours(pgid),
        "the group we made, its leader our child: killed"
    );
    sh.wait().unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(2);
    while !group_members(pgid).is_empty() && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        group_members(pgid).is_empty(),
        "the grandchild went with it"
    );
}
