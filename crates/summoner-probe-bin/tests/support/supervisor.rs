//! The lab harness's scan supervisor (TESTS ONLY, never shipped: the product's is the host application's). It tests the
//! scan child against the scan semantics:
//!
//! - **one process per bundle**: each scan spawns one probe in a process group of its own (`process_group(0)`),
//!   never reused, with SUMMONER_LAB_DEATH=kill and the test's scratch HOME; the pgid is recorded at spawn;
//! - **the budget** (30 s in the product, a parameter here): at the budget the supervisor SIGKILLs the recorded GROUP
//!   (`/bin/kill -KILL -- -<pgid>`: no FFI, and only the group it made, checked: its leader is still our unreaped
//!   child, so the id cannot have been reused); after EVERY scan no process of that group may be left (the leftovers
//!   rule): any survivor is killed and named in the outcome; the leader is reaped last;
//! - **the mapping** to the host application's `codec::ScanResult`, strict: exit 0 with one line that decodes as
//!   `codec::Scanned`
//!   → `Ok` / `Failed`; the probe's own step deadline (exit 75) → `Timeout {step}`; the supervisor's budget kill →
//!   `Timeout {budget}`, and ONLY that (the `we_sent` flag), so a SIGKILL from SUMMONER_LAB_DEATH reads as `Crashed`
//!   and a timeout never does; a signal we did not send → `Crashed {signal}`; anything else (another exit, no line, a
//!   line that does not decode) → `Crashed {exit_code}`, never parsed loosely;
//! - **the pedal**: `<home>/plugin-scan.json` (the test's scratch HOME only), a map from the absolute bundle path to
//!   `{state, mtimeMs, result}`, written atomically (temp + rename): `pending` BEFORE the probe is spawned, the
//!   result after. At load, a `pending` entry (a supervisor that died mid-scan) becomes `Crashed {}`. One strike:
//!   an entry is skipped on later passes unless the pass NAMES its path, which clears and re-probes it.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use summoner_proto::codec::{Scan, ScanResult, Scanned, ScannedOutcome, TimeoutKind};

/// The probe's own step-deadline exit (summoner-scan-probe's DEADLINE_EXIT).
const DEADLINE_EXIT: i32 = 75;

/// The test's own outer guard past the budget: if the budget kill never fired, the group is killed here and the
/// test fails "GUARD" (so a broken budget can never hang a test run).
const GUARD: Duration = Duration::from_secs(8);

/// What one scan process did.
#[derive(Debug)]
pub struct Outcome {
    pub result: ScanResult,
    /// Processes of the scan's group still alive after it (each killed, then named here): must be empty.
    pub leftovers: Vec<String>,
    pub elapsed: Duration,
    pub pgid: u32,
    /// The scan's lookup log (lookwatch), for the tests' allowlist checks.
    pub lookups: String,
}

/// One pedal file entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub state: String,
    pub mtime_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ScanResult>,
}

pub struct Supervisor {
    /// The probe (or, for the leftovers test, a stand-in), and the arguments before the bundle.
    pub program: PathBuf,
    pub pre_args: Vec<String>,
    /// The test's scratch HOME (the pedal file lives here, and nowhere else).
    pub home: PathBuf,
    pub budget: Duration,
    /// The probe's `--step-ms` (None: the stand-in takes no such flag).
    pub step_ms: Option<u64>,
    /// The recorders (fswatch, lookwatch) injected into each probe.
    pub recorders: Option<(PathBuf, PathBuf)>,
    /// Scan processes spawned so far (one strike: a skipped path spawns none).
    pub spawned: u32,
    /// The backend: `false` the probe (a CLI, one JSON line on stdout); `true` `summoner-host --scan` (the scan
    /// child: hello / scan → receipt + `scanned` event / shutdown, over a socket on its descriptor 3).
    pub child_mode: bool,
}

/// The pids of every live process in group `pgid` (a read-only `ps` listing by group id, never by name).
pub fn group_members(pgid: u32) -> Vec<(u32, String)> {
    let out = Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,pgid=,stat=,comm="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, g, stat) = (
                f.next()?.parse::<u32>().ok()?,
                f.next()?.parse::<u32>().ok()?,
                f.next()?,
            );
            let comm = f.collect::<Vec<_>>().join(" ");
            (g == pgid && !stat.starts_with('Z')).then_some((pid, comm))
        })
        .collect()
}

/// `ps -o ppid=,stat= -p <pid>` (zombies included): the process's parent and state, if it exists.
fn ps_of(pid: u32) -> Option<(u32, String)> {
    let out = Command::new("/bin/ps")
        .args(["-o", "ppid=,stat=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut f = text.split_whitespace();
    Some((f.next()?.parse().ok()?, f.next()?.to_string()))
}

/// The recorded leader has exited (a zombie, not yet reaped by us) or is gone.
fn leader_exited(pgid: u32) -> bool {
    ps_of(pgid).is_none_or(|(_, stat)| stat.starts_with('Z'))
}

/// SIGKILL the group, only if it is provably OURS: its leader (pid = pgid) is still our child
/// (unreaped, so the id cannot have been reused), and the group has a live member. Otherwise nothing is sent.
pub fn kill_ours(pgid: u32) -> bool {
    // `kill -- -1` would signal EVERY process this user may signal, and `-0` the caller's own group: never.
    if pgid <= 1 {
        return false;
    }
    let ours = ps_of(pgid).is_some_and(|(ppid, _)| ppid == std::process::id());
    if !ours || group_members(pgid).is_empty() {
        return false;
    }
    kill_group(pgid);
    true
}

/// SIGKILL the group (`kill -KILL -- -pgid`): no FFI, never by name; called only through [`kill_ours`].
fn kill_group(pgid: u32) {
    assert!(pgid > 1, "never kill -- -{pgid}");
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
}

impl Supervisor {
    fn pedal_path(&self) -> PathBuf {
        self.home.join("plugin-scan.json")
    }

    pub fn load(&self) -> BTreeMap<String, Entry> {
        let mut m: BTreeMap<String, Entry> = std::fs::read(self.pedal_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        // The dead-man's pedal: a scan that never finished (the supervisor died during it) is a crash.
        for e in m.values_mut() {
            if e.state == "pending" {
                e.state = "done".into();
                e.result = Some(ScanResult::crashed_unknown());
            }
        }
        m
    }

    fn save(&self, m: &BTreeMap<String, Entry>) {
        let p = self.pedal_path();
        assert!(
            p.starts_with(&self.home),
            "plugin-scan.json outside the scratch HOME"
        );
        let tmp = p.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(m).unwrap()).unwrap();
        std::fs::rename(&tmp, &p).unwrap();
    }

    /// One pass over `scans`: a path with an entry is skipped (one strike, and ok entries too) unless `named`
    /// names it; every other path is marked pending, probed, and recorded.
    pub fn pass(&mut self, scans: &[Scan], named: &[&str]) -> BTreeMap<String, Entry> {
        let mut m = self.load();
        self.save(&m);
        for s in scans {
            if m.contains_key(s.path()) && !named.contains(&s.path()) {
                continue;
            }
            let mtime_ms = std::fs::metadata(s.path())
                .and_then(|md| md.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_millis() as i64);
            m.insert(
                s.path().to_string().clone(),
                Entry {
                    state: "pending".into(),
                    mtime_ms,
                    result: None,
                },
            );
            self.save(&m); // BEFORE the probe exists
            let o = self.scan_once(s);
            assert!(
                o.leftovers.is_empty(),
                "{}: processes left behind: {:?}",
                s.path(),
                o.leftovers
            );
            m.insert(
                s.path().to_string().clone(),
                Entry {
                    state: "done".into(),
                    mtime_ms,
                    result: Some(o.result),
                },
            );
            self.save(&m);
        }
        m
    }

    /// One scan process for `scan`, mapped strictly to a ScanResult.
    pub fn scan_once(&mut self, scan: &Scan) -> Outcome {
        let n = self.spawned;
        self.spawned += 1;
        let look = self
            .home
            .join(format!("look-{}-{n}.log", std::process::id()));
        let fs = self.home.join(format!("fs-{}-{n}.log", std::process::id()));
        let inserted = self
            .recorders
            .as_ref()
            .map(|(fsw, lw)| format!("{}:{}", fsw.display(), lw.display()));
        let mut sock_ours: Option<std::os::unix::net::UnixStream> = None;
        let mut cmd = if self.child_mode {
            // The child takes its socket on descriptor 3. std cannot place fd 3 (and spawn_stdio_only makes no process
            // group), so /bin/sh moves it there from stdin and execs the child (same pid: the group's leader). sh is
            // SIP-protected, so dyld strips DYLD_* given to IT: the recorders travel as SUMMONER_SUP_INSERT and become
            // DYLD_INSERT_LIBRARIES only at the exec of our (unprotected) child.
            let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("a socketpair");
            sock_ours = Some(ours);
            let mut c = Command::new("/bin/sh");
            c.args([
                "-c",
                r#"exec 3<&0 0</dev/null; if [ -n "$SUMMONER_SUP_INSERT" ]; then DYLD_INSERT_LIBRARIES="$SUMMONER_SUP_INSERT" exec "$0" "$@"; fi; exec "$0" "$@""#,
            ])
            .arg(&self.program)
            .args(&self.pre_args)
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
            .stdout(Stdio::null());
            if let Some(ms) = self.step_ms {
                c.args(["--step-ms", &ms.to_string()]);
            }
            if let Some(ins) = &inserted {
                c.env("SUMMONER_SUP_INSERT", ins);
            }
            c
        } else {
            let mut c = Command::new(&self.program);
            c.args(&self.pre_args)
                .arg(scan.path())
                .stdout(Stdio::piped());
            if let Some(ms) = self.step_ms {
                c.args(["--step-ms", &ms.to_string()]);
            }
            if let Some(ins) = &inserted {
                c.env("DYLD_INSERT_LIBRARIES", ins);
            }
            c
        };
        cmd.env("HOME", &self.home)
            .env("CFFIXED_USER_HOME", &self.home)
            .env("SUMMONER_LAB_DEATH", "kill")
            .stderr(Stdio::null())
            .process_group(0);
        if self.recorders.is_some() {
            cmd.env("SUMMONER_FSWATCH_LOG", &fs)
                .env("SUMMONER_LOOKWATCH_LOG", &look);
        }
        let t0 = Instant::now();
        let mut child = cmd.spawn().expect("the scan process");
        // The Command holds the parent's copy of the child's socket end: dropped now, so the child's exit reaches the
        // conversation as EOF (kept, it hung the first child-mode run).
        drop(cmd);
        let pgid = child.id(); // process_group(0): the leader's pid is the group's id
        let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        // What the scan said: the probe's stdout (one line), or the child's `scanned` event, re-encoded as that
        // same one JSON line, so the strict mapping below is the same for both backends.
        let reader = match sock_ours {
            None => {
                let mut out = child.stdout.take().unwrap();
                std::thread::spawn(move || {
                    let mut v = Vec::new();
                    let _ = out.read_to_end(&mut v);
                    v
                })
            }
            Some(sock) => {
                let s = scan.clone();
                let ended = ended.clone();
                std::thread::spawn(move || child_conversation(sock, &s, &ended))
            }
        };
        // The leader is NOT reaped until its group has been dealt with: while our exited but
        // unreaped leader exists, its pid, and so the pgid, cannot be reused, so every group kill below is of OUR
        // group. Its exit is seen with ps (state Z), never with try_wait (which would reap it).
        let mut we_sent = false;
        loop {
            if leader_exited(pgid) {
                break;
            }
            if !we_sent && t0.elapsed() >= self.budget {
                we_sent = true;
                kill_ours(pgid);
            }
            if t0.elapsed() >= self.budget + GUARD {
                kill_ours(pgid);
                let _ = child.wait();
                panic!(
                    "GUARD: the budget kill never fired ({:?} past a {:?} budget)",
                    t0.elapsed(),
                    self.budget
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let elapsed = t0.elapsed();
        // The leftovers rule (nothing of the group may outlive the scan: a plugin's helper, say), BEFORE the output
        // is read: a survivor may hold the probe's stdout pipe open.
        let mut leftovers = Vec::new();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            let m = group_members(pgid);
            if m.is_empty() {
                break;
            }
            if !we_sent || Instant::now() > until {
                // Survivors (of a scan that ended by itself, or of our kill after 2 s): named (a failure), killed.
                leftovers = m.iter().map(|(p, c)| format!("{p} {c}")).collect();
                kill_ours(pgid);
                let gone = Instant::now() + Duration::from_secs(2);
                while !group_members(pgid).is_empty() && Instant::now() < gone {
                    std::thread::sleep(Duration::from_millis(20));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        ended.store(true, std::sync::atomic::Ordering::Release); // the conversation's backstop
        let status = child.wait().expect("reap the leader"); // last: the group id is released only now
        let stdout = reader.join().unwrap_or_default();
        // The host application's codec: a timeout carries its KIND only. Our own budget kill (and ONLY that,
        // `we_sent`) is `budget`; the probe's own step deadline (exit 75) is `step`.
        let result = if we_sent {
            ScanResult::Timeout {
                kind: TimeoutKind::Budget,
            }
        } else if let Some(sig) = status.signal() {
            ScanResult::crashed_by(summoner_proto::codec::OsCode::Signal(sig))
        } else if status.code() == Some(DEADLINE_EXIT) {
            ScanResult::Timeout {
                kind: TimeoutKind::Step,
            }
        } else if status.code() == Some(0) {
            // Strict: exactly one line, which decodes as the host application's Scanned.
            let text = String::from_utf8_lossy(&stdout);
            let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            match (
                lines.as_slice(),
                lines.first().map(|l| serde_json::from_str::<Scanned>(l)),
            ) {
                ([_], Some(Ok(s))) => match s.outcome {
                    ScannedOutcome::Ok => ScanResult::Ok {
                        descriptions: s.classes,
                    },
                    ScannedOutcome::Failed { failure, detail } => {
                        ScanResult::Failed { failure, detail }
                    }
                    // An outcome a later codec names: not one this strict reader takes.
                    _ => ScanResult::exited_early(0),
                },
                _ => ScanResult::exited_early(0),
            }
        } else {
            match status.code() {
                Some(c) => ScanResult::exited_early(c),
                None => ScanResult::crashed_unknown(),
            }
        };
        let lookups = std::fs::read_to_string(&look).unwrap_or_default();
        let _ = (std::fs::remove_file(&look), std::fs::remove_file(&fs));
        Outcome {
            result,
            leftovers,
            elapsed,
            pgid,
            lookups,
        }
    }
}

/// The child backend's side of the wire: `hello`, the one `scan`, its receipt, the `scanned` event (every frame
/// checked with the host application's own validation), then `shutdown`. Returns the `scanned` meta as one JSON line
/// (empty if the child ended first), so the caller maps both backends the same way.
fn child_conversation(
    sock: std::os::unix::net::UnixStream,
    scan: &Scan,
    ended: &std::sync::atomic::AtomicBool,
) -> Vec<u8> {
    use std::io::Write;
    use summoner_proto::codec::{self, Direction, kind};
    let mut w = sock.try_clone().expect("clone the socket");
    let send =
        |w: &mut std::os::unix::net::UnixStream, k: u16, id: u64, meta: &serde_json::Value| {
            let _ = w.write_all(&codec::encode_msg(k, 0, id, meta, &[]));
        };
    send(
        &mut w,
        kind::HELLO,
        1,
        &serde_json::json!({"childId": "scan", "fds": 0}),
    );
    send(&mut w, kind::SCAN, 2, &serde_json::to_value(scan).unwrap());
    let mut dec = codec::Decoder::new(Direction::ToHost);
    let mut r = sock;
    // A backstop beside EOF: once the supervisor has seen the leader exit (and dealt with its group), stop reading.
    let _ = r.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 65536];
    let mut line = Vec::new();
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => return line,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if ended.load(std::sync::atomic::Ordering::Acquire) {
                    return line;
                }
                continue;
            }
            Err(_) => return line,
        };
        let mut frames = Vec::new();
        if dec.feed(&buf[..n], &mut |f| frames.push(f)).is_err() {
            return Vec::new(); // a frame the host application would refuse: no line (a crash, never parsed loosely)
        }
        for f in frames {
            if codec::check_to_host(&f).is_err() {
                return Vec::new();
            }
            if f.header.kind == kind::SCANNED && f.header.id == 0 {
                line = f.meta.clone();
                line.push(b'\n');
                send(&mut w, kind::SHUTDOWN, 3, &serde_json::json!({}));
            }
        }
    }
}

/// A scan of the lab's built bundle `rel` (`vst3/…` or `au/…`).
pub fn lab_scan(plugins: &Path, rel: &str) -> Scan {
    let path = plugins.join(rel).display().to_string();
    let format = if rel.ends_with(".vst3") {
        summoner_proto::codec::ScanFormat::Vst3
    } else {
        summoner_proto::codec::ScanFormat::Au
    };
    Scan::new(format, path).expect("a lab bundle path")
}
