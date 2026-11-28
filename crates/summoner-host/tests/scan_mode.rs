//! `summoner-host --scan` (the scan child): spawned as the host application will spawn it
//! (`runecraft_ipc::process::unix::spawn_stdio_only`, a socketpair on descriptor 3), driven over the host application's
//! framing with summoner-proto's codec, every frame back checked with `codec::check_to_host`. The lab's own bundles
//! only; the recorders injected (no plugin folder, no WindowServer lookup); SUMMONER_LAB_DEATH=kill except in the one
//! real-signal breadcrumb case.
#![forbid(unsafe_code)]

mod common;

use common::*;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use summoner_proto::codec::{self, Direction, Frame, ScanFormat, ScannedOutcome, kind};

struct ScanChild {
    pid: u32,
    sock: UnixStream,
    dec: codec::Decoder,
    pending: Vec<Frame>,
    home: PathBuf,
    look: PathBuf,
    fs: PathBuf,
}

fn plugin(rel: &str) -> String {
    lab_plugins::dir().join(rel).display().to_string()
}

impl ScanChild {
    fn spawn(name: &str, args: &[&str], real_deaths: bool) -> ScanChild {
        ScanChild::spawn_env(name, args, real_deaths, &[])
    }

    /// As [`ScanChild::spawn`], with `extra` added to the child's environment (the test hooks).
    fn spawn_env(
        name: &str,
        args: &[&str],
        real_deaths: bool,
        extra: &[(&str, &str)],
    ) -> ScanChild {
        let home = repo().join(format!("target.noindex/scan-mode-{name}"));
        let _ = std::fs::remove_dir_all(&home);
        // The crash handler writes into $HOME/Library/Logs (made beforehand, as the Rig does: a handler cannot).
        std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let plugins = lab_plugins::dir();
        let (look, fs) = (home.join("look.log"), home.join("fs.log"));
        let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
            ("HOME".into(), home.clone().into()),
            ("CFFIXED_USER_HOME".into(), home.clone().into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "DYLD_INSERT_LIBRARIES".into(),
                format!(
                    "{}:{}",
                    plugins.join("libsummoner_lab_fswatch.dylib").display(),
                    plugins.join("libsummoner_lab_lookwatch.dylib").display()
                )
                .into(),
            ),
            ("SUMMONER_FSWATCH_LOG".into(), fs.clone().into()),
            ("SUMMONER_LOOKWATCH_LOG".into(), look.clone().into()),
        ];
        if !real_deaths {
            env.push(("SUMMONER_LAB_DEATH".into(), "kill".into()));
        }
        env.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        let mut argv: Vec<std::ffi::OsString> = vec!["--scan".into()];
        argv.extend(args.iter().map(|a| a.into()));
        let pid = runecraft_ipc::process::unix::spawn_stdio_only(
            &child_bin(),
            &argv,
            &env,
            theirs.as_fd(),
            None,
        )
        .expect("spawn");
        drop(theirs);
        ours.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        ScanChild {
            pid,
            sock: ours,
            dec: codec::Decoder::new(Direction::ToHost),
            pending: Vec::new(),
            home,
            look,
            fs,
        }
    }

    fn send<T: serde::Serialize>(&mut self, k: u16, id: u64, meta: &T) {
        use std::io::Write;
        let _ = self.sock.write_all(&codec::encode_msg(k, 0, id, meta, &[]));
    }

    /// The next frame for request `id`, within `within`, checked by the host application's own validation.
    fn reply(&mut self, id: u64, within: Duration) -> Option<Frame> {
        use std::io::Read;
        let until = Instant::now() + within;
        loop {
            if let Some(i) = self.pending.iter().position(|f| f.header.id == id) {
                let f = self.pending.remove(i);
                codec::check_to_host(&f)
                    .unwrap_or_else(|e| panic!("the host application would refuse it: {e}"));
                return Some(f);
            }
            if Instant::now() > until {
                return None;
            }
            let mut buf = [0u8; 65536];
            match self.sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    let p = &mut self.pending;
                    self.dec
                        .feed(&buf[..n], &mut |f| p.push(f))
                        .expect("a frame");
                }
                Err(_) => {}
            }
        }
    }

    fn hello(&mut self) -> codec::HelloReply {
        self.send(kind::HELLO, 1, &codec::Hello::new("scan".into(), 0));
        let f = self
            .reply(1, Duration::from_secs(10))
            .expect("hello's reply");
        f.meta_as().unwrap()
    }

    /// The next EVENT of kind `k` (id 0), within `within`, checked by the host application's validation.
    fn event(&mut self, k: u16, within: Duration) -> Option<Frame> {
        use std::io::Read;
        let until = Instant::now() + within;
        loop {
            if let Some(i) = self
                .pending
                .iter()
                .position(|f| f.header.id == 0 && f.header.kind == k)
            {
                let f = self.pending.remove(i);
                codec::check_to_host(&f)
                    .unwrap_or_else(|e| panic!("the host application would refuse it: {e}"));
                return Some(f);
            }
            if Instant::now() > until {
                return None;
            }
            let mut buf = [0u8; 65536];
            match self.sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    let p = &mut self.pending;
                    self.dec
                        .feed(&buf[..n], &mut |f| p.push(f))
                        .expect("a frame");
                }
                Err(_) => {}
            }
        }
    }

    /// `scan`: its receipt (or its error reply, returned as is), then the `scanned` event.
    fn scan(&mut self, id: u64, format: ScanFormat, path: &str) -> Option<Frame> {
        self.scan_offered(id, format, path, false)
    }

    /// As [`ScanChild::scan`]; `offered` sends the `scan` with form B's offer (`descriptionsBlob: 1`, the codec's
    /// `Scan::offer_descriptions_blob`).
    fn scan_offered(
        &mut self,
        id: u64,
        format: ScanFormat,
        path: &str,
        offered: bool,
    ) -> Option<Frame> {
        let scan = codec::Scan::new(format, path).unwrap();
        let scan = if offered {
            scan.offer_descriptions_blob()
        } else {
            scan
        };
        assert_eq!(scan.offers_descriptions_blob(), offered);
        self.send(kind::SCAN, id, &scan);
        let receipt = self.reply(id, Duration::from_secs(5))?;
        if receipt.header.is_error() {
            return Some(receipt);
        }
        assert_eq!(receipt.header.kind, kind::SCAN, "the receipt");
        self.event(kind::SCANNED, Duration::from_secs(30))
    }

    /// `shutdown`, its reply, and the exit: 0 (never 101, the panic's).
    fn shutdown_exits_0(&mut self, id: u64) {
        self.send(kind::SHUTDOWN, id, &serde_json::json!({}));
        assert!(
            self.reply(id, Duration::from_secs(5)).is_some(),
            "shutdown's reply"
        );
        assert_eq!(self.exit(), runecraft_ipc::process::ChildExit::Exited(0));
    }

    fn exit(&self) -> runecraft_ipc::process::ChildExit {
        runecraft_ipc::process::unix::wait_child(self.pid).expect("reap")
    }

    /// No plugin folder in the file log, no WindowServer in the lookups (the recorders' logs).
    fn recorders_clean(&self) {
        let fs = common::fswatch_v2::read_log(std::path::Path::new(&self.fs));
        assert_fswatch_v2(&fs);
        assert!(
            !fs.contains("Library/Audio/Plug-Ins") && !fs.contains("/System/Library/Components"),
            "{fs}"
        );
        let look = std::fs::read_to_string(&self.look).unwrap_or_default();
        assert!(
            !look.contains("windowserver"),
            "a WindowServer lookup:\n{look}"
        );
    }
}

impl Drop for ScanChild {
    fn drop(&mut self) {
        // Our own child, by its recorded pid (never by name); harmless if it is gone already.
        common::sigkill(self.pid);
        let _ = runecraft_ipc::process::unix::wait_child(self.pid);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A scanned class's display name (its ref's: a VST3 class's trimmed name, an AU entry's name part).
fn shown(c: &summoner_proto::plugin::ScanClass) -> String {
    summoner_probe::plugin_ref(c)
        .map(|r| r.name().to_string())
        .unwrap_or_default()
}

#[test]
fn hello_scan_scanned_shutdown_for_the_lab_gain_vst3_and_au() {
    if lab_plugins::absent("hello_scan_scanned_shutdown_for_the_lab_gain_vst3_and_au") {
        return;
    }
    for (name, format, rel) in [
        ("vst3", ScanFormat::Vst3, "vst3/summoner-lab-gain.vst3"),
        ("au", ScanFormat::Au, "au/summoner-lab-gain.component"),
    ] {
        let mut c = ScanChild::spawn(name, &[], false);
        let h = c.hello();
        assert_eq!(h.host.as_str(), "native");
        assert_eq!(h.formats, ["VST3", "AudioUnit"]);
        assert!(!h.mach_slot0, "no mach channel in scan mode");
        let f = c.scan(7, format, &plugin(rel)).expect("scanned");
        assert_eq!(
            (f.header.kind, f.header.id),
            (kind::SCANNED, 0),
            "scanned is an event (after scan 7's receipt)"
        );
        let s: codec::Scanned = f.meta_as().unwrap();
        assert_eq!(s.outcome, ScannedOutcome::Ok, "{name}: {s:?}");
        assert_eq!(s.classes.len(), 1);
        assert_eq!(shown(&s.classes[0]), "Summoner Lab Gain");
        c.send(kind::SHUTDOWN, 8, &serde_json::json!({}));
        assert!(
            c.reply(8, Duration::from_secs(5)).is_some(),
            "shutdown's reply"
        );
        assert_eq!(c.exit(), runecraft_ipc::process::ChildExit::Exited(0));
        c.recorders_clean();
    }
}

#[test]
fn one_scan_per_process_and_no_live_kinds() {
    if lab_plugins::absent("one_scan_per_process_and_no_live_kinds") {
        return;
    }
    let mut c = ScanChild::spawn("refusals", &[], false);
    c.hello();
    // A meta that is not a scan the host application could send: refused, and it does not use up the one scan.
    c.send(
        kind::SCAN,
        2,
        &serde_json::json!({"format": "vst3", "path": "relative.vst3"}),
    );
    let f = c.reply(2, Duration::from_secs(5)).expect("an error reply");
    assert!(f.header.is_error());
    // A live-child kind.
    c.send(
        kind::STATE_GET,
        3,
        &serde_json::json!({"chainKey": 1, "slot": 0}),
    );
    let f = c.reply(3, Duration::from_secs(5)).expect("an error reply");
    assert!(f.header.is_error());
    let e: codec::ErrorReply = f.meta_as().unwrap();
    assert_eq!(e.message, "not in scan mode");
    // The one scan, then a second: refused.
    let f = c
        .scan(4, ScanFormat::Vst3, &plugin("vst3/summoner-lab-synth.vst3"))
        .expect("scanned");
    assert_eq!(f.header.kind, kind::SCANNED);
    let f = c
        .scan(5, ScanFormat::Vst3, &plugin("vst3/summoner-lab-gain.vst3"))
        .expect("an error reply");
    assert!(f.header.is_error());
    let e: codec::ErrorReply = f.meta_as().unwrap();
    assert!(e.message.contains("one scan per process"), "{}", e.message);
}

#[test]
fn a_step_past_its_deadline_exits_75_and_a_hostile_constructor_is_a_crash() {
    if lab_plugins::absent("a_step_past_its_deadline_exits_75_and_a_hostile_constructor_is_a_crash")
    {
        return;
    }
    let mut c = ScanChild::spawn("deadline", &["--step-ms", "800"], false);
    c.hello();
    assert!(
        c.scan(2, ScanFormat::Vst3, &plugin("vst3/summoner-lab-hang.vst3"))
            .is_none(),
        "no scanned"
    );
    assert_eq!(
        c.exit(),
        runecraft_ipc::process::ChildExit::Exited(75),
        "the child's own step deadline"
    );
    let mut c = ScanChild::spawn("hostile", &[], false);
    c.hello();
    assert!(
        c.scan(
            2,
            ScanFormat::Vst3,
            &plugin("vst3/summoner-lab-hostile.vst3")
        )
        .is_none()
    );
    assert_eq!(
        c.exit(),
        runecraft_ipc::process::ChildExit::Signaled(9),
        "a quiet death (SUMMONER_LAB_DEATH)"
    );
}

/// The breadcrumb (the ONE real-signal case, with its crash log and its one .ips, ledgered): the hostile VST3's
/// constructor-crash class aborts (SIGABRT) during discovery; the crash log names the scan in flight.
#[test]
fn a_crash_during_a_scan_is_logged_with_its_bundle() {
    if lab_plugins::absent("a_crash_during_a_scan_is_logged_with_its_bundle") {
        return;
    }
    let mut c = ScanChild::spawn("breadcrumb", &[], true);
    c.hello();
    let path = plugin("vst3/summoner-lab-hostile.vst3");
    assert!(c.scan(2, ScanFormat::Vst3, &path).is_none());
    assert_eq!(
        c.exit(),
        runecraft_ipc::process::ChildExit::Signaled(6),
        "a real SIGABRT"
    );
    let log = c.home.join(format!(
        "Library/Logs/{}",
        summoner_proto::host_names::crash_log_name(c.pid)
    ));
    let until = Instant::now() + Duration::from_secs(5);
    let text = loop {
        match std::fs::read_to_string(&log) {
            Ok(t) if t.contains("Backtrace:") => break t,
            _ if Instant::now() > until => panic!("no crash log at {}", log.display()),
            _ => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    assert!(text.contains("Signal: SIGABRT"), "{text}");
    assert!(
        text.contains(&format!("plugin-op: plugin.scan {path}")),
        "the breadcrumb names the bundle:\n{text}"
    );
}

#[test]
fn an_orphaned_scan_child_exits_3() {
    if lab_plugins::absent("an_orphaned_scan_child_exits_3") {
        return;
    }
    // A stand-in parent of ours, which we then end by its own pid.
    let mut parent = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pp = parent.id().to_string();
    let mut c = ScanChild::spawn("orphan", &["--parent-pid", &pp], false);
    c.hello();
    parent.kill().unwrap();
    parent.wait().unwrap();
    assert_eq!(
        c.exit(),
        runecraft_ipc::process::ChildExit::Exited(3),
        "the orphan watch"
    );
}

/// The live child refuses `scan` (the live child never scans).
#[test]
fn the_live_child_refuses_a_scan() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let home = repo().join("target.noindex/scan-mode-live");
    std::fs::create_dir_all(&home).unwrap();
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = vec![
        ("HOME".into(), home.clone().into()),
        ("CFFIXED_USER_HOME".into(), home.clone().into()),
    ];
    let pid = runecraft_ipc::process::unix::spawn_stdio_only(
        &child_bin(),
        &[],
        &env,
        theirs.as_fd(),
        None,
    )
    .unwrap();
    drop(theirs);
    ours.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut c = ScanChild {
        pid,
        sock: ours,
        dec: codec::Decoder::new(Direction::ToHost),
        pending: Vec::new(),
        home,
        look: PathBuf::new(),
        fs: PathBuf::new(),
    };
    c.send(
        kind::SCAN,
        9,
        &codec::Scan::new(ScanFormat::Vst3, plugin("vst3/summoner-lab-gain.vst3")).unwrap(),
    );
    let f = c.reply(9, Duration::from_secs(5)).expect("an error reply");
    let e: codec::ErrorReply = f.meta_as().unwrap();
    assert!(e.message.contains("not in the live child"), "{}", e.message);
}

/// The child makes no directory in its HOME, and a crash log that cannot be opened is never lost:
/// the whole report goes to stderr. A HOME WITHOUT Library/Logs, a real SIGABRT (the hostile bundle's constructor
/// during a scan; one .ips, ledgered), and the report (signal, backtrace, breadcrumb) found on the child's stderr,
/// with the notice saying the log was NOT written. stderr is captured by starting the child through /bin/sh, which
/// moves the socket from stdin to descriptor 3 and execs it (the supervisor's child backend does the same).
#[test]
fn a_crash_log_that_cannot_be_opened_goes_to_stderr_whole() {
    if lab_plugins::absent("a_crash_log_that_cannot_be_opened_goes_to_stderr_whole") {
        return;
    }
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let home = repo().join("target.noindex/scan-mode-nologs");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap(); // and NOT Library/Logs
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", r#"exec 3<&0 0</dev/null; exec "$0" "$@""#])
        .arg(child_bin())
        .arg("--scan")
        .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            theirs,
        )))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .env("HOME", &home)
        .env("CFFIXED_USER_HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .process_group(0)
        .spawn()
        .unwrap();
    let path = plugin("vst3/summoner-lab-hostile.vst3");
    let _ = ours.write_all(&codec::encode_msg(
        kind::HELLO,
        0,
        1,
        &codec::Hello::new("scan".into(), 0),
        &[],
    ));
    let _ = ours.write_all(&codec::encode_msg(
        kind::SCAN,
        0,
        2,
        &codec::Scan::new(ScanFormat::Vst3, &path).unwrap(),
        &[],
    ));
    // Bounded: a scan that does not crash (a refused bundle: no dev roots, say) leaves the child waiting for the next
    // message while this test holds the socket, and stderr never ends. That is a named failure here, never a hang.
    let err = read_to_end_within(
        child.stderr.take().unwrap(),
        SCAN_CRASH_BOUND,
        child.id(),
        "waiting for the scan child's crash report on stderr (the hostile constructor's SIGABRT)",
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let st = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(st.signal(), Some(6), "a real SIGABRT: {err}");
    assert!(
        !home.join("Library/Logs").exists(),
        "the child made no directory"
    );
    for want in [
        summoner_proto::host_names::CRASH_BANNER,
        "Signal: SIGABRT (6)",
        "Backtrace:",
        &format!("plugin-op: plugin.scan {path}"),
        "crash log NOT written (cannot open",
    ] {
        assert!(err.contains(want), "{want:?} not on stderr:\n{err}");
    }
    let _ = std::fs::remove_dir_all(&home);
}

/// The env every oversized-scan test gives the child: a scratch HOME and the repeat hook (test-hooks:
/// `SUMMONER_TEST_SCAN_REPEAT`), the gain's one class repeated `repeat` times.
fn oversize_env(
    home: &std::path::Path,
    repeat: usize,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let _ = std::fs::remove_dir_all(home);
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    vec![
        ("HOME".into(), home.into()),
        ("CFFIXED_USER_HOME".into(), home.into()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("SUMMONER_LAB_DEATH".into(), "kill".into()),
        (
            "SUMMONER_TEST_SCAN_REPEAT".into(),
            repeat.to_string().into(),
        ),
    ]
}

/// The bytes a frame was sent as (header, meta, blob): what summoner-proto's encoder must have produced, byte for byte.
fn frame_bytes(f: &Frame) -> Vec<u8> {
    codec::encode(f.header.kind, f.header.flags, f.header.id, &f.meta, &f.blob)
}

/// What [`raw_oversized_scan`] saw.
struct Raw {
    bytes: Vec<u8>,
    exit: runecraft_ipc::process::ChildExit,
    /// From the header-only `scanned`'s arrival to the child's EOF.
    header_to_eof: Duration,
}

/// A raw conversation with a child whose one class is repeated `repeat` times, NOT offered form B (the header-only case):
/// hello, scan, then read to the child's EOF. When the oversized header has arrived, `close` shuts our write side (the
/// host application's EOF, as the child sees it); otherwise we hold the connection open and the child's own bound ends
/// it.
fn raw_oversized_scan(name: &str, repeat: usize, close: bool) -> Raw {
    use std::io::{Read, Write};
    let home = repo().join(format!("target.noindex/scan-mode-{name}"));
    let env = oversize_env(&home, repeat);
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    let argv: Vec<std::ffi::OsString> = vec!["--scan".into()];
    let pid = runecraft_ipc::process::unix::spawn_stdio_only(
        &child_bin(),
        &argv,
        &env,
        theirs.as_fd(),
        None,
    )
    .expect("spawn");
    drop(theirs);
    let path = plugin("vst3/summoner-lab-gain.vst3");
    ours.write_all(&codec::encode_msg(
        kind::HELLO,
        0,
        1,
        &codec::Hello::new("scan".into(), 0),
        &[],
    ))
    .unwrap();
    ours.write_all(&codec::encode_msg(
        kind::SCAN,
        0,
        2,
        &codec::Scan::new(ScanFormat::Vst3, &path).unwrap(),
        &[],
    ))
    .unwrap();
    ours.set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut header_at = None;
    let mut buf = [0u8; 65536];
    let eof_at = loop {
        match ours.read(&mut buf) {
            Ok(0) => break Instant::now(),
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e) => panic!("{name}: no EOF from the child: {e}"),
        }
        if header_at.is_none() {
            let mut d = codec::Decoder::new(Direction::ToHost);
            if d.feed(&bytes, &mut |_| {}).is_err() && d.oversized_meta_header().is_some() {
                header_at = Some(Instant::now());
                if close {
                    ours.shutdown(std::net::Shutdown::Write).unwrap();
                }
            }
        }
    };
    let exit = runecraft_ipc::process::unix::wait_child(pid).expect("reap our child");
    let _ = std::fs::remove_dir_all(&home);
    Raw {
        bytes,
        exit,
        header_to_eof: eof_at - header_at.expect("the oversized header arrived"),
    }
}

/// The checks every raw oversized scan passes: every frame before it decodes, the decoder refuses the header for its
/// size alone (`oversized_meta_header` names it: `scanned`, an event, id 0, blob 0), and nothing follows it.
fn assert_header_alone(r: &Raw) {
    assert_eq!(
        r.exit,
        runecraft_ipc::process::ChildExit::Exited(0),
        "exit 0, never 101"
    );
    let mut dec = codec::Decoder::new(Direction::ToHost);
    let mut frames: Vec<Frame> = Vec::new();
    let res = dec.feed(&r.bytes, &mut |f| frames.push(f));
    let ids: Vec<(u16, u64)> = frames
        .iter()
        .map(|f| (f.header.kind, f.header.id))
        .collect();
    assert_eq!(
        ids,
        [(kind::HELLO, 1), (kind::SCAN, 2)],
        "the hello reply and the scan's receipt, then nothing whole"
    );
    for f in &frames {
        codec::check_to_host(f).unwrap();
    }
    assert!(
        matches!(res, Err(codec::ProtoError::MetaTooLong(n)) if n as usize > codec::MAX_META),
        "{res:?}"
    );
    let h = dec
        .oversized_meta_header()
        .expect("refused for its size alone");
    assert_eq!(
        (h.kind, h.flags, h.id, h.blob_len),
        (kind::SCANNED, codec::FF_EVENT, 0, 0)
    );
    let whole: usize = frames
        .iter()
        .map(|f| 24 + f.meta.len() + f.blob.len())
        .sum();
    assert_eq!(
        r.bytes.len(),
        whole + 24,
        "the header alone: no meta byte after it"
    );
}

/// A bundle of many classes makes form A's meta pass MAX_META, and `codec::encode_msg` ASSERTS the cap: a child that
/// called it unchecked would panic (exit 101: the host application's `crashed`, a strike on an innocent plugin). A
/// 2,000-class report, NOT offered form B, gives the header-only answer (summoner-proto's encoder's header alone, its
/// true lengths, no body) and no panic. Then the child waits for the
/// host application's EOF for at most the host application's `scan::SEND_TIMEOUT`: here the host application never
/// closes, so the child's own bound ends the conversation, no sooner and not much later, and it exits 0.
#[test]
fn a_2000_class_report_not_offered_is_its_header_alone_then_the_childs_bound_and_exit_0() {
    if lab_plugins::absent(
        "a_2000_class_report_not_offered_is_its_header_alone_then_the_childs_bound_and_exit_0",
    ) {
        return;
    }
    let bound = summoner_scan::scan::SEND_TIMEOUT;
    let r = raw_oversized_scan("oversize-raw-held", 2000, false);
    assert_header_alone(&r);
    assert!(
        r.header_to_eof >= bound - Duration::from_millis(50),
        "the child waited for the host application's EOF up to SEND_TIMEOUT ({bound:?}): closed after {:?}",
        r.header_to_eof
    );
    assert!(
        r.header_to_eof <= bound + Duration::from_secs(2),
        "and then closed: {:?}",
        r.header_to_eof
    );
}

/// The same, but the host application closes once the header has arrived (as the summoner supervisor does): the child
/// takes the EOF and ends at once, never waiting out its bound.
#[test]
fn after_a_header_alone_the_cores_eof_ends_the_child_at_once() {
    if lab_plugins::absent("after_a_header_alone_the_cores_eof_ends_the_child_at_once") {
        return;
    }
    let bound = summoner_scan::scan::SEND_TIMEOUT;
    let r = raw_oversized_scan("oversize-raw-closed", 2000, true);
    assert_header_alone(&r);
    assert!(
        r.header_to_eof < bound / 2,
        "the host application's EOF, not the bound ({bound:?}), ended it: {:?}",
        r.header_to_eof
    );
}

/// Form B (the `scan` offered `descriptionsBlob: 1`) with 0, 1 and many classes, each `scanned` byte-equal
/// to the host's encoder (`codec::encode_scanned_form_b`) on what it carries, and decoded by the host application's
/// strict decoder (`codec::decode_scanned`'s checks) as form B. 0: a path that is no lab bundle (`failed notFound`, no
/// class discovered); 1: the lab gain, VST3 and AU; many: the gain's class 50 times (the repeat hook).
#[test]
fn form_b_with_0_1_and_many_classes_is_byte_equal_to_the_cores_encoder() {
    if lab_plugins::absent("form_b_with_0_1_and_many_classes_is_byte_equal_to_the_cores_encoder") {
        return;
    }
    for (name, format, path, repeat, want) in [
        (
            "b0",
            ScanFormat::Vst3,
            "/nonexistent/summoner-lab-none.vst3".to_string(),
            "1",
            0,
        ),
        (
            "b1-vst3",
            ScanFormat::Vst3,
            plugin("vst3/summoner-lab-gain.vst3"),
            "1",
            1,
        ),
        (
            "b1-au",
            ScanFormat::Au,
            plugin("au/summoner-lab-gain.component"),
            "1",
            1,
        ),
        (
            "bmany",
            ScanFormat::Vst3,
            plugin("vst3/summoner-lab-gain.vst3"),
            "50",
            50,
        ),
    ] {
        let mut c =
            ScanChild::spawn_env(name, &[], false, &[("SUMMONER_TEST_SCAN_REPEAT", repeat)]);
        c.hello();
        let f = c.scan_offered(7, format, &path, true).expect("scanned");
        match codec::parse_scanned_meta(&f.meta) {
            Ok(codec::ScannedMeta::B(m)) => {
                assert_eq!(m.description_count, want as u64, "{name}");
                assert_eq!(m.over_limit, None, "{name}");
            }
            other => panic!("{name}: not form B: {other:?}"),
        }
        let b = match codec::decode_scanned(&f, true) {
            Ok(codec::ScannedLine::B(b)) => b,
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(b.descriptions.len(), want, "{name}");
        if want == 0 {
            assert!(
                matches!(
                    b.outcome,
                    ScannedOutcome::Failed {
                        failure: codec::ScanFailure::NotFound,
                        ..
                    }
                ),
                "{name}: {:?}",
                b.outcome
            );
            assert!(f.blob.is_empty(), "{name}: no line, no byte");
        } else {
            assert_eq!(b.outcome, ScannedOutcome::Ok, "{name}");
            assert!(
                b.descriptions
                    .iter()
                    .all(|d| shown(d) == "Summoner Lab Gain"),
                "{name}"
            );
            assert!(
                b.descriptions.iter().all(|d| *d == b.descriptions[0]),
                "{name}: the one class, repeated"
            );
        }
        let expected = codec::encode_scanned_form_b(&codec::Scanned::new(
            b.format,
            b.path.clone(),
            b.outcome.clone(),
            b.descriptions.clone(),
        ));
        assert_eq!(
            expected,
            codec::ScannedFrame::Whole(frame_bytes(&f)),
            "{name}: byte-equal to summoner-proto's encoder"
        );
        c.shutdown_exits_0(8);
        c.recorders_clean();
    }
}

/// Not offered: form A, byte-equal to the host application's frozen form A encoder (`codec::encode_scanned_form_a`, as
/// at 1488c63), no `descriptionsBlob` key, the descriptions in the meta and no blob.
#[test]
fn not_offered_is_form_a_byte_equal_to_the_cores_encoder() {
    if lab_plugins::absent("not_offered_is_form_a_byte_equal_to_the_cores_encoder") {
        return;
    }
    let mut c = ScanChild::spawn("form-a", &[], false);
    c.hello();
    let f = c
        .scan_offered(
            7,
            ScanFormat::Vst3,
            &plugin("vst3/summoner-lab-gain.vst3"),
            false,
        )
        .expect("scanned");
    assert!(f.blob.is_empty(), "form A has no blob");
    let v: serde_json::Value = serde_json::from_slice(&f.meta).unwrap();
    assert!(v.get("descriptionsBlob").is_none(), "{v}");
    let s = match codec::decode_scanned(&f, false) {
        Ok(codec::ScannedLine::A(s)) => s,
        other => panic!("{other:?}"),
    };
    assert_eq!(s.outcome, ScannedOutcome::Ok);
    assert_eq!(s.classes.len(), 1);
    assert_eq!(
        codec::encode_scanned_form_a(&s),
        codec::ScannedFrame::Whole(frame_bytes(&f)),
        "byte-equal to summoner-proto's form A"
    );
    c.shutdown_exits_0(8);
    c.recorders_clean();
}

/// A VALIDATION failure (the lab's bad-state fixture: its own state is refused) keeps discovery's
/// class. Offered, form B carries it in the blob beside the unchanged `failed {state}` verdict, and summoner-proto's
/// decoder gives it back as `discovered` (`ScannedLine::into_verdict`); not offered, form A's `descriptions` stays
/// `[]`.
#[test]
fn a_validation_failures_discovered_classes_travel_in_form_b_only() {
    if lab_plugins::absent("a_validation_failures_discovered_classes_travel_in_form_b_only") {
        return;
    }
    for (name, format, rel) in [
        (
            "disc-vst3",
            ScanFormat::Vst3,
            "vst3/summoner-lab-badstate.vst3",
        ),
        (
            "disc-au",
            ScanFormat::Au,
            "au/summoner-lab-badstate.component",
        ),
    ] {
        for offered in [true, false] {
            let mut c = ScanChild::spawn(&format!("{name}-{offered}"), &[], false);
            c.hello();
            let f = c
                .scan_offered(7, format, &plugin(rel), offered)
                .expect("scanned");
            let line = codec::decode_scanned(&f, offered)
                .unwrap_or_else(|e| panic!("{name} {offered}: {e}"));
            assert_eq!(matches!(line, codec::ScannedLine::B(_)), offered, "{name}");
            let (verdict, discovered) = line.into_verdict();
            assert!(
                matches!(
                    verdict,
                    codec::ScanResult::Failed {
                        failure: codec::ScanFailure::State,
                        ..
                    }
                ),
                "{name} {offered}: {verdict:?}"
            );
            if offered {
                assert_eq!(discovered.len(), 1, "{name}");
                assert!(
                    shown(&discovered[0]).starts_with("Summoner Lab Bad State"),
                    "{name}: {:?}",
                    discovered[0]
                );
                assert_eq!(
                    matches!(discovered[0], summoner_proto::plugin::ScanClass::Vst3(_)),
                    format == ScanFormat::Vst3,
                    "{name}: a VST3 class for a VST3 bundle, an AU entry for an AU one"
                );
            } else {
                assert!(discovered.is_empty(), "{name}: form A carries none");
                let s: codec::Scanned = f.meta_as().unwrap();
                assert!(s.classes.is_empty(), "{name}: form A is frozen");
            }
            c.shutdown_exits_0(8);
            c.recorders_clean();
        }
    }
}

/// A synthetic 20,000-class report (the gain's class repeated), offered form B, passes the count bound
/// (`MAX_SCAN_DESCRIPTIONS`, 16,384): summoner-proto's encoder gives the form B meta with the TRUE `descriptionCount`
/// and `overLimit: "count"`, an empty blob, which summoner-proto's decoder takes as the engine's over-limit report (no
/// strike); no panic, and the conversation goes on to `shutdown` and exit 0.
#[test]
fn a_20000_class_report_offered_gives_over_limit_count_and_no_panic() {
    if lab_plugins::absent("a_20000_class_report_offered_gives_over_limit_count_and_no_panic") {
        return;
    }
    let mut c = ScanChild::spawn_env(
        "overlimit-count",
        &[],
        false,
        &[("SUMMONER_TEST_SCAN_REPEAT", "20000")],
    );
    c.hello();
    let f = c
        .scan_offered(
            7,
            ScanFormat::Vst3,
            &plugin("vst3/summoner-lab-gain.vst3"),
            true,
        )
        .expect("scanned");
    assert!(f.blob.is_empty(), "an empty blob");
    match codec::parse_scanned_meta(&f.meta) {
        Ok(codec::ScannedMeta::B(m)) => {
            assert_eq!(m.description_count, 20_000, "the true count");
            assert_eq!(m.over_limit.as_deref(), Some("count"));
            assert_eq!(m.outcome, ScannedOutcome::Ok);
        }
        other => panic!("not form B: {other:?}"),
    }
    assert_eq!(
        codec::decode_scanned(&f, true),
        Err(codec::ScannedFault::Limit(codec::ScannedLimit::OverLimit {
            word: "count".into(),
            description_count: 20_000
        })),
        "an engine limit, no strike"
    );
    const { assert!(20_000 > codec::MAX_SCAN_DESCRIPTIONS) };
    c.shutdown_exits_0(8);
    c.recorders_clean();
}

/// The same, through the host application's scan supervisor at the pin: the oversized line is an EngineFault (its why
/// starting OVERSIZED_SCANNED), no verdict, NO STRIKE; counted as one oversized `scanned`, not a protocol error. The
/// summoner supervisor offers form B on every scan, so 2,000 classes travel in the blob and are a plain `ok` verdict;
/// the oversized line is the one past the engine's count limit, 20,000 classes, refused by the host application's form
/// B decode as an engine limit. The leader is SIGKILLed by the supervisor while it waits for the host application's
/// EOF.
#[test]
fn the_cores_supervisor_records_an_oversized_scanned_as_an_engine_fault_not_a_strike() {
    if lab_plugins::absent(
        "the_cores_supervisor_records_an_oversized_scanned_as_an_engine_fault_not_a_strike",
    ) {
        return;
    }
    use std::sync::atomic::Ordering::Relaxed;
    use summoner_scan::scan::{
        OVERSIZED_SCANNED, Pedal, ScanConfig, ScanEnd, ScanReport, ScanSupervisor,
    };
    #[derive(Default)]
    struct Mem(Vec<(ScanEnd, bool)>);
    impl Pedal for Mem {
        fn pending(&mut self, _: &codec::Scan) -> std::io::Result<()> {
            Ok(())
        }
        fn settle(&mut self, r: &ScanReport) -> std::io::Result<()> {
            self.0.push((r.end.clone(), r.end.is_strike()));
            Ok(())
        }
    }
    let home = repo().join("target.noindex/scan-mode-oversize-sup");
    // Form B offered (the host application's request): 2,000 classes are an ordinary `ok`.
    let mut ok_sup = ScanSupervisor::new(ScanConfig::new(child_bin(), oversize_env(&home, 2000)));
    let scan = codec::Scan::new(ScanFormat::Vst3, plugin("vst3/summoner-lab-gain.vst3")).unwrap();
    let r = ok_sup.scan(&scan, &mut Mem::default());
    match r.end.verdict() {
        Some(codec::ScanResult::Ok { descriptions }) => assert_eq!(descriptions.len(), 2000),
        other => panic!(
            "2,000 classes in form B are a verdict: {other:?} ({:?})",
            r.end
        ),
    }
    let mut sup = ScanSupervisor::new(ScanConfig::new(child_bin(), oversize_env(&home, 20_000)));
    let mut pedal = Mem::default();
    let r = sup.scan(&scan, &mut pedal);
    assert!(r.scan_sent && !r.we_sent && !r.outer_limit, "{r:?}");
    match &r.end {
        ScanEnd::EngineFault { why } => assert!(why.starts_with(OVERSIZED_SCANNED), "{why}"),
        other => panic!(
            "an EngineFault, not {other:?}; stderr {:?}",
            r.stderr.text()
        ),
    }
    assert!(!r.end.is_strike() && r.end.verdict().is_none());
    assert_eq!(pedal.0, [(r.end.clone(), false)], "settled once, no strike");
    // The exact kill, asserted on purpose: the summoner supervisor SIGKILLs the group the moment a frame makes its
    // conversation's fault (the bridge's scan supervisor), and the refused header is one, so the child, inside its
    // bounded wait for the host application's EOF (`scan::SEND_TIMEOUT`), never exits
    // on its own. If the host application ever changes to close-and-wait, this goes red and flags it. (SIGKILL: no
    // .ips.) Our own kill is `Killed`, which the record and the logs still spell `Signaled(9)`.
    assert_eq!(
        r.exit,
        Some(runecraft_ipc::process::ChildExit::Killed),
        "the supervisor's own fault kill, during the child's wait for the host application's EOF"
    );
    assert_eq!(
        r.exit.map(|e| e.to_string()).as_deref(),
        Some("Signaled(9)")
    );
    let c = sup.counters();
    assert_eq!(
        (
            c.oversized_scanned.load(Relaxed),
            c.strikes.load(Relaxed),
            c.protocol_errors.load(Relaxed)
        ),
        (1, 0, 0)
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Through the FFI crate's `set_thread_qos_utility`: the probe's thread runs at QoS utility, read back
/// by `summoner_sys::qos` and said on stderr, which the summoner supervisor captures. A real scan of the lab's gain.
#[test]
fn the_probe_thread_runs_at_qos_utility() {
    if lab_plugins::absent("the_probe_thread_runs_at_qos_utility") {
        return;
    }
    use summoner_scan::scan::{Pedal, ScanConfig, ScanEnd, ScanReport, ScanSupervisor};
    struct NoPedal;
    impl Pedal for NoPedal {
        fn pending(&mut self, _: &codec::Scan) -> std::io::Result<()> {
            Ok(())
        }
        fn settle(&mut self, _: &ScanReport) -> std::io::Result<()> {
            Ok(())
        }
    }
    let home = repo().join("target.noindex/scan-mode-qos");
    let mut env = oversize_env(&home, 1);
    env.retain(|(k, _)| k != "SUMMONER_TEST_SCAN_REPEAT");
    let mut sup = ScanSupervisor::new(ScanConfig::new(child_bin(), env));
    let scan = codec::Scan::new(ScanFormat::Vst3, plugin("vst3/summoner-lab-gain.vst3")).unwrap();
    let r = sup.scan(&scan, &mut NoPedal);
    assert!(
        matches!(&r.end, ScanEnd::Verdict(codec::ScanResult::Ok { descriptions }) if descriptions.len() == 1),
        "{:?}; stderr {:?}",
        r.end,
        r.stderr.text()
    );
    assert!(
        r.stderr
            .text()
            .contains("the probe thread runs at QoS Utility"),
        "{:?}",
        r.stderr.text()
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// How long a scan child may take to crash and close stderr: seconds in practice; past this the step is named and the
/// child's process group killed.
const SCAN_CRASH_BOUND: Duration = Duration::from_secs(60);

/// Reads `r` to its end within `bound`. Past the bound the process group `pgid` (created by this test with
/// `process_group(0)`, so the child's own) is killed and the error names `step` and the bound: a test that waits on a
/// child's output fails loudly, it never hangs.
fn read_to_end_within(
    mut r: impl std::io::Read + Send + 'static,
    bound: Duration,
    pgid: u32,
    step: &str,
) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = String::new();
        let _ = r.read_to_string(&mut out);
        let _ = tx.send(out);
    });
    match rx.recv_timeout(bound) {
        Ok(out) => Ok(out),
        Err(_) => {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .status();
            if cfg!(mutant_scan_wait_unbounded) {
                return Ok(String::new());
            }
            Err(format!(
                "{step}: no end within {bound:?}; the child's process group {pgid} was killed"
            ))
        }
    }
}

/// The hang's regression (a scan that never crashes): with the bundle refused, the child answers and waits for the
/// next message while the socket stays open, so its stderr never ends; the bounded read names the step within its
/// bound and kills the child. Mutant: `--cfg mutant_scan_wait_unbounded` (the bound passes silently) turns it red.
#[test]
fn a_scan_child_that_never_ends_is_a_named_timeout_not_a_hang() {
    use std::io::Write;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    let home = repo().join("target.noindex/scan-mode-no-end");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("Library/Logs")).unwrap();
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", r#"exec 3<&0 0</dev/null; exec "$0" "$@""#])
        .arg(child_bin())
        .arg("--scan")
        .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            theirs,
        )))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .env("HOME", &home)
        .env("CFFIXED_USER_HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .process_group(0)
        .spawn()
        .unwrap();
    // A path outside every root: refused lexically, nothing touched, no crash.
    let refused = "/nowhere/summoner-lab-hostile.vst3";
    let _ = ours.write_all(&codec::encode_msg(
        kind::HELLO,
        0,
        1,
        &codec::Hello::new("scan".into(), 0),
        &[],
    ));
    let _ = ours.write_all(&codec::encode_msg(
        kind::SCAN,
        0,
        2,
        &codec::Scan::new(ScanFormat::Vst3, refused).unwrap(),
        &[],
    ));
    let t0 = Instant::now();
    let bound = Duration::from_secs(3);
    let got = read_to_end_within(
        child.stderr.take().unwrap(),
        bound,
        child.id(),
        "the no-end step",
    );
    let st = child.wait().unwrap();
    let e = got.expect_err("the child never ends while the socket is open: a named timeout");
    assert!(e.starts_with("the no-end step: no end within 3s"), "{e}");
    assert!(
        t0.elapsed() < bound + Duration::from_secs(10),
        "bounded: {:?}",
        t0.elapsed()
    );
    assert_eq!(st.signal(), Some(9), "the child's group was killed");
    drop(ours);
}
