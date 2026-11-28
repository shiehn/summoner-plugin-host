//! The scan supervisor. One supervisor for both children; one probe at a time (`&mut self`); one
//! process per bundle.
//!
//! **One scan** ([`ScanSupervisor::scan`]), in this order:
//! 1. the dead-man's pedal marks the path `pending` ([`Pedal::pending`]) BEFORE the spawn; a refusal spawns nothing;
//! 2. the spawn (`runecraft_ipc::process::spawn_group`): argv exactly `--scan --parent-pid <the host application's pid>
//!    --step-ms <n>` and, when a crash-log folder is configured, `--crash-log <dir>` (N3), the
//!    socketpair's child end on fd 3, the explicit env, `register: None`, its own process group (pgid = pid,
//!    recorded), its stderr on a pipe; the supervisor closes its copies of the child's ends at once; the budget
//!    starts here;
//! 3. the conversation, in this order and no other: `hello {childId, fds: 0}` (id 1; the reply's protocol major is
//!    checked) → `scan {format, path}` (64, a request, id 2) → its `{}` receipt (a reply, id 2) → `scanned` (65, an
//!    EVENT, id 0), whose `format` and `path` must echo the request's → `shutdown` (id 3) → its reply → the exit. The
//!    supervisor sends nothing between the receipt and `scanned`;
//! 4. the budget (30 s, [`BUDGET`]), from the spawn to the leader's exit: at it, the we-sent flag is set, then
//!    the group is SIGKILLed (re-sent every [`REKILL`] until the leader has exited);
//! 5. when the leader has exited (seen with `WNOWAIT`, so it stays our unreaped zombie and the group id cannot be
//!    reused), the group is SIGKILLed once more, so nothing it left behind in its group outlives the scan; then the
//!    leader is reaped; then *the drain*: before the verdict, the conversation takes
//!    what the reader still delivers, to the reader's end (EOF, or why not), within [`DRAIN`]. A dead child's bytes
//!    stay readable in the socket until EOF, so a line it wrote before it died (a `scanned` before a SIGKILL) is
//!    judged and kept whichever thread saw the end first. After the exit the conversation sends nothing;
//! 6. the verdict ([`judge`], the verdict table) with its record ([`ScanReport`]: the stderr capture, the crash
//!    log the child writes in `$HOME/Library/Logs`, named by its pid, if it exists, attached and never parsed), handed to
//!    the pedal ([`Pedal::settle`]). The supervisor never creates a directory.
//!
//! **Threads and channels** (no async runtime: `std::thread` and one bounded `sync_channel`). The caller's
//! thread runs the conversation and the budget (`recv_timeout` on the channel; it must be a thread of its own, never
//! control or the audio callback: a scan blocks it for up to the budget). Per scan: a reader thread (frames from the
//! socket, through the host application's `Decoder`), a wait thread (the leader's exit, `waitid(WNOWAIT)`) and a stderr thread
//! (the stderr capture). All three are joined before [`ScanSupervisor::scan`] returns.
//!
//! **The stderr capture**, ALWAYS on: read by the bounded reader up to
//! [`STDERR_CAP`] bytes, cut with a marker. It carries the step deadline's line and, when the crash log cannot be
//! opened, the whole crash report, so both are always the LAST bytes the child writes: the capture keeps the first
//! [`HEAD`] bytes and the latest bytes after them, and cuts the middle.
//!
//! Not realtime: nothing here runs on the audio callback or on control. Buffers are allocated once per scan; nothing
//! grows past [`STDERR_CAP`].
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use runecraft_ipc::process::{ChildExit, GroupKillError};

/// `SIGKILL`'s number (9 on every unix), for the verdicts that name our own kill.
const SIGKILL: i32 = 9;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use summoner_proto::codec::{
    self, Direction, ErrorReply, FF_ERROR, FF_EVENT, FF_REPLY, Frame, Header, HelloReply, MAX_BLOB,
    MAX_META, OsCode, Scan, ScanFailure, ScanResult, Scanned, TimeoutKind, kind,
};
use summoner_proto::layout::ABI_VERSION;

/// The most bytes of the child's stderr kept, marker included (bounded at 64 KiB).
pub const STDERR_CAP: usize = 64 * 1024;
/// Of those, the first bytes kept as they came (the child's start-up lines, a plugin's first complaint).
pub const HEAD: usize = 16 * 1024;
/// Room kept for the marker between the head and the tail.
const MARKER_ROOM: usize = 128;
/// After the leader is reaped, how long the reader still waits for EOF (a process outside the group, one that left
/// it with `setsid`, may hold the pipe open): then the capture is taken as it stands, marked `open`.
pub const STDERR_GRACE: Duration = Duration::from_secs(1);
/// The reader's `poll` slice: how soon it sees the supervisor's "reaped" flag.
const POLL_MS: i32 = 50;

/// What a bounded capture kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Captured {
    /// At most [`STDERR_CAP`] bytes: everything, or the head, the marker and the tail.
    pub bytes: Vec<u8>,
    /// Every byte read, kept or cut.
    pub total: u64,
    /// True when bytes were cut (the marker is in `bytes`).
    pub cut: bool,
    /// True when the source was still open when the capture was taken (no EOF within [`STDERR_GRACE`]).
    pub open: bool,
}

impl Captured {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(bytes: Vec<u8>, total: u64, cut: bool, open: bool) -> Captured {
        Captured {
            bytes,
            total,
            cut,
            open,
        }
    }
}

impl Captured {
    /// The bytes as text (lossy: a plugin's stderr need not be UTF-8).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

/// The marker put where bytes were cut.
pub fn marker(cut: u64) -> String {
    format!("\n[{cut} bytes of the scan child's stderr cut here]\n")
}

/// A head-and-tail keeper of at most `cap` bytes (`cap` > `head` + the marker's room), allocated once.
pub struct Bounded {
    cap: usize,
    head_cap: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl Bounded {
    /// A keeper of at most `cap` bytes whose first `head` bytes are kept whole.
    pub fn new(cap: usize, head: usize) -> Bounded {
        assert!(
            cap > head + MARKER_ROOM,
            "a cap with room for the tail and the marker"
        );
        Bounded {
            cap,
            head_cap: head,
            head: Vec::with_capacity(head),
            tail: VecDeque::with_capacity(cap - head),
            total: 0,
        }
    }

    /// [`STDERR_CAP`] with [`HEAD`].
    pub fn stderr() -> Bounded {
        Bounded::new(STDERR_CAP, HEAD)
    }

    /// Keep `data`: the head fills first, then the tail keeps the latest bytes.
    pub fn push(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        let room = self.head_cap - self.head.len();
        if room > 0 {
            let n = room.min(data.len());
            self.head.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
        let tail_cap = self.cap - self.head_cap;
        if data.len() >= tail_cap {
            self.tail.clear();
            data = &data[data.len() - tail_cap..];
        } else if self.tail.len() + data.len() > tail_cap {
            let excess = self.tail.len() + data.len() - tail_cap;
            self.tail.drain(..excess);
        }
        self.tail.extend(data);
    }

    /// The kept bytes: all of them when they fit in `cap`; else the head, the marker and the latest bytes, `cap` at
    /// most in all.
    pub fn finish(mut self, open: bool) -> Captured {
        let mut bytes = std::mem::take(&mut self.head);
        let cut = self.total > self.cap as u64;
        if cut {
            let keep = self.cap - self.head_cap - MARKER_ROOM;
            let from = self.tail.len().saturating_sub(keep);
            let dropped = self.total - bytes.len() as u64 - (self.tail.len() - from) as u64;
            let m = marker(dropped);
            debug_assert!(m.len() <= MARKER_ROOM);
            bytes.extend_from_slice(m.as_bytes());
            bytes.extend(self.tail.range(from..));
        } else {
            bytes.extend(self.tail.iter());
        }
        debug_assert!(bytes.len() <= self.cap);
        Captured {
            bytes,
            total: self.total,
            cut,
            open,
        }
    }
}

/// The stderr thread: read `src` (the pipe's read end) into a [`Bounded::stderr`] until EOF, draining (and counting)
/// what is past the cap so the child never blocks on a full pipe. Once `reaped` is set, it waits at most
/// [`STDERR_GRACE`] more for EOF, then returns what it has (`open: true`). Never blocks for longer than one
/// `POLL_MS` slice without looking at `reaped`.
pub fn spawn_stderr_reader(
    src: std::io::PipeReader,
    reaped: Arc<AtomicBool>,
    thread_word: &str,
) -> std::io::Result<JoinHandle<Captured>> {
    std::thread::Builder::new()
        .name(format!("{thread_word}-err"))
        .spawn(move || read_bounded(src, &reaped, STDERR_GRACE))
}

fn read_bounded(mut src: std::io::PipeReader, reaped: &AtomicBool, grace: Duration) -> Captured {
    let mut keep = Bounded::stderr();
    let mut buf = vec![0u8; 8 * 1024];
    let mut since: Option<Instant> = None;
    loop {
        let ready = match runecraft_ipc::process::unix::poll_readable(src.as_fd(), POLL_MS) {
            Ok(r) => r,
            Err(_) => return keep.finish(false),
        };
        if ready {
            match src.read(&mut buf) {
                Ok(0) => return keep.finish(false),
                Ok(n) => keep.push(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return keep.finish(false),
            }
        }
        if reaped.load(Ordering::Acquire) {
            let t = *since.get_or_insert_with(Instant::now);
            if t.elapsed() >= grace {
                return keep.finish(true);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The supervisor
// ---------------------------------------------------------------------------------------------------------------------

/// The budget, from the spawn to the leader's exit (not measured). A parameter ([`ScanConfig::budget`]): 30 s in
/// production, about 1 s in the tests.
pub const BUDGET: Duration = Duration::from_secs(30);
/// How often the group is SIGKILLed again while a killed leader has not yet exited (a process forked during the first
/// kill is caught by the next).
pub const REKILL: Duration = Duration::from_secs(1);
/// The outer limit after the FIRST kill: a leader that has not exited by then is
/// handed to a reaper thread and [`ScanSupervisor::scan`] returns, so its caller is never blocked forever.
pub const OUTER_LIMIT: Duration = Duration::from_secs(10);
/// How often the scan's process tree is walked while the leader runs (to see a descendant that leaves the group).
pub const TREE_POLL: Duration = Duration::from_millis(50);
/// The drain's bound *(branch `scan-drain`, 2026-11-01)*: after the leader's exit has been seen (and the group swept
/// and the leader reaped), how long the conversation still takes what the reader delivers before it judges. The bytes
/// it waits for are already in the socket when the exit is seen (a process's descriptors are closed before it becomes
/// a zombie), so what it waits for is the reader thread's turn and the death of the group members the sweep has just
/// killed (they hold fd 3 by inheritance): milliseconds. Past it only a holder outside the group (an escapee) keeps the
/// socket open: the record is judged on what arrived, counted in [`ScanCounters::drain_timeouts`] and logged. The same
/// bound as [`STDERR_GRACE`], the same kind of wait, and run at the same time as it, so a scan's worst case grows by
/// about one second, not two. A parameter ([`ScanConfig::drain`]).
pub const DRAIN: Duration = Duration::from_secs(1);
/// The write timeout on the socket (the protocol's 2 s stall): a write that stalls is left to the budget.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(2);
/// Inbound messages in flight from the reader and the waiter (the conversation is four frames).
pub const INBOUND_BOUND: usize = 32;
/// The scan reader's blob budget ("one blob in flight"): the most
/// blob bytes the reader thread admits in one conversation, summed over its frames, each frame's counted from its
/// header's declared `blobLen` BEFORE its body is allocated. One [`MAX_BLOB`] body: the one blob the protocol will
/// ever let a scan child send (form B's `scanned`). A parameter ([`ScanConfig::blob_budget`]), so the tests set it
/// small. With [`INBOUND_BOUND`] metas of at most [`MAX_META`] each, the scan side holds at most 64 MiB + 2 MiB of the
/// child's frames, where it could hold about 2.1 GiB without it.
pub const SCAN_BLOB_BUDGET: u64 = MAX_BLOB as u64;
/// The most bytes of a crash log attached (the same bound as the stderr capture).
pub const CRASH_LOG_CAP: usize = STDERR_CAP;

/// The child's step deadline the host application passes (`--step-ms`: explicit, never the child's
/// default): 25 000 ms, the plugin-host child's `scan_mode::DEFAULT_STEP_MS` (three real VST3s took ~21 s in
/// discovery when it was measured). A parameter ([`ScanConfig::step`]).
pub const STEP: Duration = Duration::from_millis(25_000);
/// The child's range for `--step-ms` (the plugin-host child's: 1..=120 000 ms, else exit 2).
pub const STEP_MAX: Duration = Duration::from_millis(120_000);
/// An AU probe's step deadline, 60 000 ms (the slowest
/// real AU took 23.4 s against 25 s when measured; large-sampler instantiations are slow, and a timeout is a strike). A
/// parameter ([`ScanConfig::au_step`]).
pub const STEP_AU: Duration = Duration::from_millis(60_000);
/// The budget for an AU probe, 70 s (VST3 keeps [`BUDGET`]). A parameter ([`ScanConfig::au_budget`]).
pub const BUDGET_AU: Duration = Duration::from_secs(70);
const _: () = assert!(STEP_AU.as_millis() <= STEP_MAX.as_millis());
const _: () = assert!(STEP_AU.as_millis() < BUDGET_AU.as_millis());

/// host's exit codes [V host `scan_mode.rs:19`, `main.rs:61-64`]: the step deadline.
pub const EXIT_STEP_DEADLINE: i32 = 75;
/// A usage error or no fd 3: the host application's bug.
pub const EXIT_USAGE: i32 = 2;
/// Orphaned (the parent's exit note, a re-parent, a parent ≤ 1).
pub const EXIT_ORPHANED: i32 = 3;
/// A protocol error in what the host application sent: the host application's bug.
pub const EXIT_PROTOCOL: i32 = 4;

/// The supervisor's configuration.
#[derive(Clone, Debug)]
pub struct ScanConfig {
    /// The scan child (the plugin-host child; the fake host in the tests).
    pub program: PathBuf,
    /// The child's whole environment, explicit (HOME and CFFIXED_USER_HOME included).
    pub env: Vec<(OsString, OsString)>,
    /// N3: the crash log's folder, passed as `--crash-log <dir>` and the one place
    /// a scan child's crash log is read from ([`read_crash_log`]); the `$HOME` derivation is gone. Production sets
    /// the folder its own `--crash-log` guard validated; a lab host application the `Library/Logs` folder it opened in the
    /// isolated home. `None` (the default, tests): no flag, the child keeps its own fallback, and no log is read. A
    /// folder the child refuses (gone after launch) is its exit 2 before any plugin code: [`ScanEnd::HostFault`], no
    /// strike (N4).
    pub crash_log: Option<PathBuf>,
    /// The child's step deadline, passed as `--step-ms` ([`STEP`] in production). It must be 1 ms..=[`STEP_MAX`] and
    /// under [`ScanConfig::budget`] (a step that outlives the budget is a deadline nobody can meet): otherwise
    /// nothing is spawned and the scan is [`ScanEnd::NotRun`] (the host application's configuration, no strike).
    pub step: Duration,
    /// An AU probe's step deadline ([`STEP_AU`] in production), under the same rules against
    /// [`ScanConfig::au_budget`].
    pub au_step: Duration,
    /// `hello`'s `childId`.
    pub child_id: String,
    /// [`BUDGET`] in production.
    pub budget: Duration,
    /// [`BUDGET_AU`] in production: an AU probe's budget.
    pub au_budget: Duration,
    /// [`OUTER_LIMIT`] in production.
    pub outer_limit: Duration,
    /// [`SCAN_BLOB_BUDGET`] in production: past it, the reader refuses the frame from its header (nothing of it is
    /// allocated) and the scan ends in an [`ScanEnd::EngineFault`] with the group killed.
    pub blob_budget: u64,
    /// [`DRAIN`] in production.
    pub drain: Duration,
    /// Where the scan's records go ([`runecraft_contracts::log::NullSink`] by default).
    pub log: std::sync::Arc<dyn runecraft_contracts::log::LogSink>,
    /// The records' area ([`summoner::supervisor::NO_LOG_AREA`] by default: none).
    pub log_area: runecraft_contracts::log::AreaKey,
    /// The child's log flags and environment ([`summoner::supervisor::NoChildLog`] by default: none).
    pub levels: std::sync::Arc<dyn summoner::supervisor::LevelSource>,
    /// The words of the scan's stderr lines and thread names ([`summoner::supervisor::Names::NEUTRAL`] by default).
    pub names: summoner::supervisor::Names,
    /// The outer limit's test seam (`scan-seams` only): the leader's exit is never seen, as if it ignored SIGKILL.
    hold_exit: bool,
    /// The drain's test seam (`scan-seams` only): the reader holds a `scanned` frame until the conversation has seen
    /// the leader's exit, so the line always comes after the death notice (the race the drain closes, made certain).
    hold_scanned: bool,
    /// The half frame's seams (`scan-seams` only): the reader holds its end until the exit has
    /// been seen (order b: the drain meets the EOF mid-frame) ...
    hold_end: bool,
    /// ... or the waiter holds the exit until the reader has sent its end (order a: the EOF mid-frame comes first).
    hold_exit_for_end: bool,
}

impl ScanConfig {
    /// [`BUDGET`], [`OUTER_LIMIT`], `childId` "scan".
    pub fn new(program: PathBuf, env: Vec<(OsString, OsString)>) -> ScanConfig {
        ScanConfig {
            program,
            env,
            child_id: "scan".into(),
            crash_log: None,
            step: STEP,
            au_step: STEP_AU,
            budget: BUDGET,
            au_budget: BUDGET_AU,
            outer_limit: OUTER_LIMIT,
            blob_budget: SCAN_BLOB_BUDGET,
            drain: DRAIN,
            log: std::sync::Arc::new(runecraft_contracts::log::NullSink),
            log_area: summoner::supervisor::NO_LOG_AREA,
            levels: std::sync::Arc::new(summoner::supervisor::NoChildLog),
            names: summoner::supervisor::Names::NEUTRAL,
            hold_exit: false,
            hold_scanned: false,
            hold_end: false,
            hold_exit_for_end: false,
        }
    }

    /// The test seam (feature `scan-seams`, which only this crate's tests enable): the supervisor ignores the leader's
    /// exit, so the outer limit is what ends the scan. A leader that survives SIGKILL cannot be built.
    #[cfg(feature = "scan-seams")]
    pub fn seam_hold_exit(&mut self) {
        self.hold_exit = true;
    }

    /// The drain's test seam (feature `scan-seams`, which only this crate's tests enable): the reader thread delivers a
    /// `scanned` frame only once the conversation has seen the leader's exit (at most [`SEAM_HOLD_MAX`] later), so a
    /// child that writes `scanned` and dies always has its line arrive after the death notice.
    #[cfg(feature = "scan-seams")]
    pub fn seam_hold_scanned_until_exit(&mut self) {
        self.hold_scanned = true;
    }

    /// The half frame's order b (feature `scan-seams`): the reader thread sends its end (`ReaderDone`) only once the
    /// conversation has seen the leader's exit (at most [`SEAM_HOLD_MAX`] later), so the drain is what meets it.
    #[cfg(feature = "scan-seams")]
    pub fn seam_hold_reader_end_until_exit(&mut self) {
        self.hold_end = true;
    }

    /// The half frame's order a (feature `scan-seams`): the wait thread sends the leader's exit only once the reader
    /// has sent its end (at most [`SEAM_HOLD_MAX`] later), so the main loop meets the reader's end first.
    #[cfg(feature = "scan-seams")]
    pub fn seam_hold_exit_until_reader_end(&mut self) {
        self.hold_exit_for_end = true;
    }

    /// The step deadline a probe of `format` passes. `--cfg mutant_au_step_not_passed`
    /// (never in a normal build) gives AU the VST3 step; the AU step test fails.
    pub fn step_for(&self, format: summoner_proto::codec::ScanFormat) -> Duration {
        match format {
            summoner_proto::codec::ScanFormat::Au if !cfg!(mutant_au_step_not_passed) => {
                self.au_step
            }
            _ => self.step,
        }
    }

    /// The budget of a probe of `format`.
    pub fn budget_for(&self, format: summoner_proto::codec::ScanFormat) -> Duration {
        match format {
            summoner_proto::codec::ScanFormat::Au if !cfg!(mutant_au_step_not_passed) => {
                self.au_budget
            }
            _ => self.budget,
        }
    }

    /// The step check, for both formats' pairs: `Err` names why a step cannot be passed.
    pub fn check_step(&self) -> Result<(), crate::Error> {
        self.step_why().map_err(crate::Error::new)
    }

    fn step_why(&self) -> Result<(), String> {
        for (step, budget) in [(self.step, self.budget), (self.au_step, self.au_budget)] {
            let ms = step.as_millis();
            if ms == 0 || step > STEP_MAX || !step.subsec_nanos().is_multiple_of(1_000_000) {
                return Err(format!(
                    "the step deadline {:?} is not a whole 1..={} ms (the child's --step-ms range)",
                    step,
                    STEP_MAX.as_millis()
                ));
            }
            if step >= budget {
                return Err(format!(
                    "the step deadline ({ms} ms) is not under the scan budget ({} ms): the budget would always end a \
                     slow step first",
                    budget.as_millis()
                ));
            }
        }
        Ok(())
    }

    /// The scan child's argv (N3) for a VST3 probe: [`ScanConfig::args_for`].
    pub fn args(&self, parent: u32) -> Vec<OsString> {
        self.args_for(parent, summoner_proto::codec::ScanFormat::Vst3)
    }

    /// The scan child's argv (N3): `--scan --parent-pid <pid> --step-ms <n>` (the step of the
    /// probe's format), then `--crash-log <dir>` when a folder is configured.
    pub fn args_for(
        &self,
        parent: u32,
        format: summoner_proto::codec::ScanFormat,
    ) -> Vec<OsString> {
        let mut a: Vec<OsString> = vec![
            "--scan".into(),
            "--parent-pid".into(),
            parent.to_string().into(),
            "--step-ms".into(),
            self.step_for(format).as_millis().to_string().into(),
        ];
        if let Some(d) = self
            .crash_log
            .as_deref()
            .and_then(|d| summoner::supervisor::crash_log_for_spawn(d, self.names.stderr))
        {
            a.push("--crash-log".into());
            a.push(d.into_os_string());
        }
        a
    }
}

/// How a scan ended.
///
/// **The strike principle**: a strike only when plugin code could have been running, i.e.
/// from the scan request on. Before it, every end is the engine's: no verdict, no strike, a loud engine-side error.
/// Generic over the classes of a verdict: the bridge reports the wire's raw [`codec::ScanClass`]; a host
/// that keeps its own descriptions maps them with [`ScanEnd::map_classes`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanEnd<D = codec::ScanClass> {
    /// The verdict: the outcome an application records for the bundle.
    Verdict(codec::ScanVerdict<D>),
    /// Exit 2 or 4: the HOST APPLICATION's fault. No verdict, no strike; an engine-side error,
    /// logged loudly and counted.
    HostFault {
        /// The child's exit code (2 or 4).
        exit_code: i32,
    },
    /// The engine's fault before plugin code could run (the strike principle): a
    /// protocol fault before the scan's receipt (an error reply, a refused frame, a frame out of order, a protocol
    /// major this host does not speak); the budget kill of a child that never got the scan request (a hang at
    /// `hello`); a death before the scan request. No verdict, no strike; logged loudly and counted.
    ///
    /// And one end after the receipt, the engine's limit and not the plugin's: a
    /// `scanned` line refused for its size alone (`MetaTooLong`, over [`MAX_META`]: a bundle of many classes); its
    /// `why` starts with [`OVERSIZED_SCANNED`]. Every other fault after the receipt stays `protocolAfterScan`.
    ///
    /// And on either side of the receipt, the reader's blob budget: a frame
    /// whose declared blob would take the bytes the reader admitted past [`ScanConfig::blob_budget`]; its `why` starts
    /// with [`BLOB_OVER_BUDGET`]. The group is killed at once.
    EngineFault {
        /// What the engine-side error was.
        why: String,
    },
    /// No probe ran: the pedal could not mark the path pending, or the spawn (or a thread) failed. No
    /// verdict, no strike; logged loudly and counted.
    NotRun {
        /// Why no probe ran.
        why: String,
    },
}

impl<D> ScanEnd<D> {
    /// The verdict, when the scan ended in one.
    pub fn verdict(&self) -> Option<&codec::ScanVerdict<D>> {
        match self {
            ScanEnd::Verdict(r) => Some(r),
            _ => None,
        }
    }

    /// The same end over other classes: `f` maps a verdict's.
    pub fn map_classes<E>(
        self,
        f: impl FnOnce(codec::ScanVerdict<D>) -> codec::ScanVerdict<E>,
    ) -> ScanEnd<E> {
        match self {
            ScanEnd::Verdict(v) => ScanEnd::Verdict(f(v)),
            ScanEnd::HostFault { exit_code } => ScanEnd::HostFault { exit_code },
            ScanEnd::EngineFault { why } => ScanEnd::EngineFault { why },
            ScanEnd::NotRun { why } => ScanEnd::NotRun { why },
        }
    }

    /// The dead-man's pedal's strike (a probe that dies or passes its budget is marked, and skipped by the next
    /// scan unless a rescan names it): a `crashed` or `timeout` verdict, or `failed {reason: protocolAfterScan}`;
    /// nothing else.
    pub fn is_strike(&self) -> bool {
        matches!(
            self,
            ScanEnd::Verdict(
                codec::ScanVerdict::Crashed { .. }
                    | codec::ScanVerdict::Timeout { .. }
                    | codec::ScanVerdict::Failed {
                        failure: ScanFailure::ProtocolAfterScan,
                        ..
                    }
            )
        )
    }
}

/// A protocol fault, and which side of the scan's receipt it fell on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fault {
    /// What the fault was.
    pub why: String,
    /// Whether it came after the scan's receipt (a strike) or before it.
    pub after_receipt: bool,
    /// The fault is the `scanned` line the conversation was waiting for, refused by the decoder for its size alone
    /// (`MetaTooLong`; its header passes every other check): its meta length. The engine's limit, not the plugin's
    /// . None for every other fault.
    pub oversized_scanned: Option<u32>,
    /// The fault is the reader's blob budget: an engine-side limit, on
    /// either side of the receipt. None for every other fault.
    pub over_budget: Option<OverBudget>,
    /// The `scanned` line passed one of the engine's limits
    /// after the header (the child's `overLimit`, the description count, a description line): the codec's text. The
    /// engine's limit, not the plugin's: no verdict, no strike. None for every other fault.
    pub scanned_limit: Option<String>,
}

impl Fault {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        why: String,
        after_receipt: bool,
        oversized_scanned: Option<u32>,
        over_budget: Option<OverBudget>,
        scanned_limit: Option<String>,
    ) -> Fault {
        Fault {
            why,
            after_receipt,
            oversized_scanned,
            over_budget,
            scanned_limit,
        }
    }
}

/// A frame the reader refused for its blob budget, from its header alone (nothing of its body was read or
/// allocated).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct OverBudget {
    /// The refused frame's header (its declared `blobLen` is what passed the budget).
    pub header: Header,
    /// The blob bytes the reader had admitted before it, in this conversation.
    pub admitted: u64,
    /// [`ScanConfig::blob_budget`].
    pub budget: u64,
}

impl OverBudget {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(header: Header, admitted: u64, budget: u64) -> OverBudget {
        OverBudget {
            header,
            admitted,
            budget,
        }
    }
}

/// The start of an [`ScanEnd::EngineFault`]'s `why` for the reader's blob budget (
/// 2026-11-01): what [`ScanSupervisor`]'s accounting counts it by.
pub const BLOB_OVER_BUDGET: &str = "an engine-side limit: the scan reader's blob budget";

/// The start of an [`ScanEnd::EngineFault`]'s `why` for a `scanned` line over [`MAX_META`] (
/// 2026-11-01): what [`ScanSupervisor`]'s accounting counts it by.
pub const OVERSIZED_SCANNED: &str = "an engine-side limit after the scan's receipt";

/// The verdict table with the strike boundary, from what arrived, whether the scan request was sent, and the leader's
/// `waitpid` status (None: the outer limit ended the scan first):
///
/// | what happened | end |
/// |---|---|
/// | a frame past the reader's blob budget, either side of the receipt | [`ScanEnd::EngineFault`] |
/// | the `scanned` line refused for its size alone (`MetaTooLong`), after the receipt | [`ScanEnd::EngineFault`] |
/// | any other protocol fault after the receipt | `failed {reason: protocolAfterScan}`, a strike |
/// | a protocol fault before the receipt | [`ScanEnd::EngineFault`] |
/// | half a frame, then the end (EOF mid-frame) | no fault: the rows below, by the end |
/// | the budget kill (`we_sent`), the scan request sent | `timeout {kind: budget}` |
/// | the budget kill before the scan request (a hang at `hello`) | [`ScanEnd::EngineFault`] |
/// | exit 2 or 4 | [`ScanEnd::HostFault`] |
/// | any other end before the scan request | [`ScanEnd::EngineFault`] (the strike principle) |
/// | a signal the supervisor did not send | `crashed {signal}` |
/// | exit 0 after `scanned` | the line's `ok` / `failed` |
/// | exit 75 | `timeout {kind: step}` |
/// | exit 0 without `scanned`, 3, or any code the table does not name | `crashed {exitCode}` |
///
/// **The death wins**: `scanned` counts only for a clean exit 0 after it.
pub fn judge(
    fault: Option<Fault>,
    scan_sent: bool,
    we_sent: bool,
    exit: Option<ChildExit>,
    scanned: Option<&Scanned>,
) -> ScanEnd {
    if let Some(f) = fault {
        // The reader's blob budget is the engine's limit, on either side of the
        // receipt: no verdict, no strike.
        if let Some(b) = f.over_budget {
            return ScanEnd::EngineFault {
                why: format!(
                    "{BLOB_OVER_BUDGET}: {} (kind {}, flags {}, id {}) declares a {}-byte blob, and the reader had \
                     admitted {} blob bytes of its {}-byte budget, so it refused the frame from its header (nothing \
                     of it was allocated) and the scan child's group was killed. The engine's limit, not the plugin's",
                    codec::kind_name(b.header.kind).unwrap_or("?"),
                    b.header.kind,
                    b.header.flags,
                    b.header.id,
                    b.header.blob_len,
                    b.admitted,
                    b.budget
                ),
            };
        }
        // The engine's own size limit is no fault of the plugin's. Its mutant (never
        // in a normal build) restores the old mapping: the oversized line struck as protocolAfterScan.
        #[cfg(not(mutant_oversized_scanned_struck))]
        if let Some(len) = f.oversized_scanned {
            return ScanEnd::EngineFault {
                why: format!(
                    "{OVERSIZED_SCANNED}: the scanned line's meta is {len} bytes (MAX_META {MAX_META} bytes), or its \
                     blob is over MAX_BLOB ({MAX_BLOB} bytes), so the host's decoder refused it for its size alone \
                     ({}). The engine's limit, not the plugin's",
                    f.why
                ),
            };
        }
        // Part 1: a form B line past an engine limit after its header.
        if let Some(limit) = f.scanned_limit {
            return ScanEnd::EngineFault {
                why: format!(
                    "{OVERSIZED_SCANNED}: the scanned line passes an engine limit ({limit}). The engine's limit, not \
                     the plugin's"
                ),
            };
        }
        // The strike boundary's mutant (never in a normal build): a fault after the receipt is not struck.
        #[cfg(mutant_fault_after_receipt_unstruck)]
        let after = false;
        #[cfg(not(mutant_fault_after_receipt_unstruck))]
        let after = f.after_receipt;
        return if after {
            ScanEnd::Verdict(ScanResult::Failed {
                failure: ScanFailure::ProtocolAfterScan,
                detail: f.why,
            })
        } else {
            ScanEnd::EngineFault {
                why: format!("a protocol fault before the scan's receipt: {}", f.why),
            }
        };
    }
    if we_sent {
        if !scan_sent {
            return ScanEnd::EngineFault {
                why: "the budget ran out before the scan request was sent (a hang at hello)".into(),
            };
        }
        // Mutant (a) (never in a normal build): the budget kill read as the crash it looks like.
        #[cfg(mutant_budget_kill_crashed)]
        return ScanEnd::Verdict(ScanResult::crashed_by(OsCode::Signal(SIGKILL)));
        #[cfg(not(mutant_budget_kill_crashed))]
        return ScanEnd::Verdict(ScanResult::Timeout {
            kind: TimeoutKind::Budget,
        });
    }
    let Some(exit) = exit else {
        return ScanEnd::EngineFault {
            why: "the leader's exit was never seen".into(),
        };
    };
    match exit {
        // Mutant (c) (never in a normal build): exit 2 falls through to `crashed`, a strike.
        #[cfg(not(mutant_usage_exit_strike))]
        ChildExit::Exited(c @ (EXIT_USAGE | EXIT_PROTOCOL)) => {
            return ScanEnd::HostFault { exit_code: c };
        }
        #[cfg(mutant_usage_exit_strike)]
        ChildExit::Exited(c @ EXIT_PROTOCOL) => return ScanEnd::HostFault { exit_code: c },
        _ => {}
    }
    if !scan_sent {
        return ScanEnd::EngineFault {
            why: format!("the scan child ended ({exit}) before the scan request was sent"),
        };
    }
    match exit {
        ChildExit::Signaled(sig) => ScanEnd::Verdict(ScanResult::crashed_by(OsCode::Signal(sig))),
        // Our own kill after its exit was seen (the sweep is not its death): judged as the SIGKILL it was, so every
        // verdict and record keeps its bytes.
        ChildExit::Killed => ScanEnd::Verdict(ScanResult::crashed_by(OsCode::Signal(SIGKILL))),
        ChildExit::Exited(0) if scanned.is_some() => {
            ScanEnd::Verdict(ScanResult::from_scanned(scanned.cloned().expect("is_some")))
        }
        ChildExit::Exited(EXIT_STEP_DEADLINE) => ScanEnd::Verdict(ScanResult::Timeout {
            kind: TimeoutKind::Step,
        }),
        ChildExit::Exited(c) => ScanEnd::Verdict(ScanResult::exited_early(c)),
        // Never on macOS: an unhandled exception (Windows) is a crash, its code the exit code the OS reports.
        // The mutation check (never in a normal build): the exception read as an exit code (its kind lost), as before.
        // `the_verdict_table_as_ruled`'s exception row must fail.
        #[cfg(mutant_exception_as_exit)]
        ChildExit::Exception(code) => ScanEnd::Verdict(ScanResult::exited_early(code as i32)),
        #[cfg(not(mutant_exception_as_exit))]
        ChildExit::Exception(code) => {
            ScanEnd::Verdict(ScanResult::crashed_by(OsCode::Exception(code)))
        }
        // An end a later floor names: no verdict and no strike, an engine fault, worded.
        _ => ScanEnd::EngineFault {
            why: format!("the scan child ended ({exit}), an end this table does not judge"),
        },
    }
}

/// One scan's record: the end, and everything attached to it.
#[derive(Debug)]
#[non_exhaustive]
pub struct ScanReport {
    /// The scan that was run.
    pub scan: Scan,
    /// How it ended.
    pub end: ScanEnd,
    /// The leader's pid, which is the group's id (recorded at the spawn); None when nothing was spawned.
    pub pid: Option<u32>,
    /// The process group's id (the leader's pid); None when nothing was spawned.
    pub pgid: Option<u32>,
    /// The leader's status, from `waitpid`; None when the outer limit ended the scan first (the leader then went to a
    /// reaper thread).
    pub exit: Option<ChildExit>,
    /// The scan request was written (the strike boundary: plugin code may run from here on).
    pub scan_sent: bool,
    /// The supervisor sent the budget kill (the flag was set before the signal).
    pub we_sent: bool,
    /// The outer limit ended the wait: the leader had not exited [`ScanConfig::outer_limit`] after the
    /// first kill.
    pub outer_limit: bool,
    /// The `scanned` line, if one arrived and passed its checks (kept even when the death wins). A form B line is
    /// kept in form A's shape: `classes` the blob's on `ok`, `[]` on `failed` (its classes are [`Self::discovered`]).
    pub scanned: Option<Scanned>,
    /// The classes discovery found beside a `failed` verdict: a form B `failed` line's
    /// blob, in the child's order. Only when that line IS the verdict (a clean exit 0 after it: the death wins);
    /// `[]` otherwise, and always for form A. The pedal records them as the entry's `discovered`.
    pub discovered: Vec<codec::ScanClass>,
    /// The child's stderr, bounded (always captured).
    pub stderr: Captured,
    /// The child's crash log in `$HOME/Library/Logs` (named by its pid), if the child wrote one: attached, never parsed (the
    /// supervisor knows the path it sent).
    pub crash_log: Option<Captured>,
    /// The members of the scan's group other than the leader, alive (not zombies) when the leader's exit was seen,
    /// just before the sweep killed them.
    pub leftovers: u32,
    /// The group kill made after the leader's exit, before its reap (Err(Os(ESRCH)): nothing was left in the group).
    pub sweep: Option<Result<(), GroupKillError>>,
    /// Escapees: descendants seen while the leader ran that left the scan's group (`setsid`, `setpgid`)
    /// and were still alive after the sweep and the reap. Stray processes on the user's machine: logged loudly,
    /// counted, and NOT killed (they are outside the group; the supervisor signals only the group). A descendant that
    /// leaves and outlives its parent between two walks ([`TREE_POLL`]) is not seen.
    pub escapees: Vec<u32>,
    /// When the pedal had marked the path pending, and when the spawn returned.
    pub pending_at: Option<Instant>,
    /// When the spawn returned.
    pub spawned_at: Option<Instant>,
    /// From the spawn to the leader's exit (or to the outer limit).
    pub elapsed: Duration,
    /// The pedal's error when it settled the scan.
    pub pedal_error: Option<String>,
    /// The blob bytes the reader admitted in this conversation, each frame's counted from its header before its body
    /// was allocated: never more than [`ScanConfig::blob_budget`].
    pub reader_blob_bytes: u64,
    /// The drain *(branch `scan-drain`)*: how long the conversation took what the reader still delivered after the
    /// leader's exit (None: no drain was needed: the reader had ended first, the budget or a fault had stopped the
    /// conversation taking frames, or the outer limit ended the scan).
    pub drain: Option<Duration>,
    /// The drain ran out ([`ScanConfig::drain`]) before the reader's end: something outside the group held the socket
    /// open. The record was judged on what had arrived.
    pub drain_timeout: bool,
    /// The child's socket ended in the middle of a frame (EOF mid-frame): it left half a frame. No fault: the end
    /// (the death, or the exit) decides.
    pub half_frame: bool,
}

/// The dead-man's pedal and the cache (the application's scan record, not specified or built here): the
/// supervisor calls [`Pedal::pending`] before every spawn and [`Pedal::settle`] after every scan it started.
pub trait Pedal {
    /// Mark the path `pending`, durably, BEFORE the spawn. An error: no spawn ([`ScanEnd::NotRun`]).
    fn pending(&mut self, scan: &Scan) -> std::io::Result<()>;
    /// Record the end: `report.end.verdict()` when there is one (a strike when [`ScanEnd::is_strike`]); else the
    /// pending mark is withdrawn with no strike.
    fn settle(&mut self, report: &ScanReport) -> std::io::Result<()>;
}

/// The supervisor's counters (for the host application's telemetry).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct ScanCounters {
    /// The protocol of the last scan child whose `hello` was verified (`major << 16 | minor`; 0
    /// before any).
    pub child_protocol: std::sync::atomic::AtomicU32,
    /// Scans run, cumulative.
    pub scans: AtomicU64,
    /// Scans that ended in a verdict, cumulative.
    pub verdicts: AtomicU64,
    /// Strikes recorded against a plugin, cumulative.
    pub strikes: AtomicU64,
    /// Scans the budget killed, cumulative.
    pub budget_kills: AtomicU64,
    /// Protocol faults on either side of the receipt.
    pub protocol_errors: AtomicU64,
    /// Exits 2 and 4: the engine-side errors.
    pub host_faults: AtomicU64,
    /// [`ScanEnd::EngineFault`]s.
    pub engine_faults: AtomicU64,
    /// Of those, the `scanned` lines over [`MAX_META`]: the evidence for the size question,
    /// the descriptions in the frame's blob part.
    pub oversized_scanned: AtomicU64,
    /// Of those, the frames the reader refused for its blob budget.
    pub blob_over_budget: AtomicU64,
    /// [`ScanEnd::NotRun`]s.
    pub not_run: AtomicU64,
    /// Scans whose stderr pipe was still held open past the grace.
    pub stderr_held_open: AtomicU64,
    /// Leftovers killed by the sweep, summed.
    pub leftovers: AtomicU64,
    /// Escapees, summed.
    pub escapees: AtomicU64,
    /// Scans ended by the outer limit.
    pub outer_limits: AtomicU64,
    /// Drains that ran out before the reader's end ([`ScanReport::drain_timeout`]).
    pub drain_timeouts: AtomicU64,
}

enum Inbound {
    Frame(Frame),
    /// The reader ended: `Ok(())` at a clean EOF, else why (a refused frame, EOF mid-frame, descriptors).
    ReaderDone(Result<(), ReaderFault>),
    LeaderExited(std::io::Result<ChildExit>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Hello,
    Receipt,
    Scanned,
    Shutdown,
    Done,
}

const ID_HELLO: u64 = 1;
const ID_SCAN: u64 = 2;
const ID_SHUTDOWN: u64 = 3;

/// Why the reader ended early.
struct ReaderFault {
    why: String,
    /// The header of a frame the decoder refused for its meta size alone ([`codec::Decoder::oversized_meta_header`]).
    oversized: Option<Header>,
    /// A frame refused for the blob budget.
    over_budget: Option<OverBudget>,
    /// The socket ended in the middle of a frame (EOF mid-frame): half a frame left by an end.
    half_frame: bool,
}

impl ReaderFault {
    fn other(why: String) -> ReaderFault {
        ReaderFault {
            why,
            oversized: None,
            over_budget: None,
            half_frame: false,
        }
    }
}

/// The scan's process tree, walked every [`TREE_POLL`] while the leader runs: every descendant seen, by pid and start
/// time (so a reused pid is never taken for it).
struct Tree {
    root: u32,
    seen: std::collections::BTreeMap<u32, u64>,
}

impl Tree {
    fn walk(&mut self) {
        let mut stack = vec![self.root];
        let mut visited = 0usize;
        while let Some(p) = stack.pop() {
            visited += 1;
            if visited > runecraft_ipc::process::PID_LIST_MAX {
                break;
            }
            for c in runecraft_ipc::process::child_pids(p) {
                if let Some(i) = runecraft_ipc::process::proc_info(c) {
                    self.seen.entry(c).or_insert(i.start_us);
                }
                stack.push(c);
            }
        }
    }

    /// The descendants seen that are alive now, the same processes (start time), and outside `pgid`.
    fn escapees(&self, pgid: u32) -> Vec<u32> {
        self.seen
            .iter()
            .filter(|(pid, start)| {
                runecraft_ipc::process::proc_info(**pid)
                    .is_some_and(|i| i.start_us == **start && !i.zombie && i.pgid != pgid)
            })
            .map(|(pid, _)| *pid)
            .collect()
    }
}

/// The live members of group `pgid` other than `leader`.
fn leftovers(pgid: u32, leader: u32) -> u32 {
    runecraft_ipc::process::group_pids(pgid)
        .into_iter()
        .filter(|p| {
            *p != leader && runecraft_ipc::process::proc_info(*p).is_some_and(|i| !i.zombie)
        })
        .count() as u32
}

/// The scan supervisor: one probe at a time (`&mut self`).
pub struct ScanSupervisor {
    cfg: ScanConfig,
    counters: Arc<ScanCounters>,
}

impl ScanSupervisor {
    /// A supervisor with this configuration and zeroed counters.
    pub fn new(cfg: ScanConfig) -> ScanSupervisor {
        ScanSupervisor {
            cfg,
            counters: Arc::new(ScanCounters::default()),
        }
    }

    /// The supervisor's counters.
    pub fn counters(&self) -> &Arc<ScanCounters> {
        &self.counters
    }

    /// Its configuration (the scan record identifies the child by `program`'s bytes).
    pub fn config(&self) -> &ScanConfig {
        &self.cfg
    }

    /// Probe one bundle, start to end, and hand the record to `pedal`. Blocks for at most the budget, the outer
    /// limit after the first kill, and [`STDERR_GRACE`] (plus the reap of a leader that has exited).
    pub fn scan(&mut self, scan: &Scan, pedal: &mut dyn Pedal) -> ScanReport {
        self.counters.scans.fetch_add(1, Ordering::Relaxed);
        let mut report = ScanReport {
            scan: scan.clone(),
            end: ScanEnd::NotRun { why: String::new() },
            pid: None,
            pgid: None,
            exit: None,
            scan_sent: false,
            we_sent: false,
            outer_limit: false,
            scanned: None,
            discovered: Vec::new(),
            stderr: Captured::default(),
            crash_log: None,
            leftovers: 0,
            sweep: None,
            escapees: Vec::new(),
            pending_at: None,
            spawned_at: None,
            elapsed: Duration::ZERO,
            pedal_error: None,
            reader_blob_bytes: 0,
            drain: None,
            drain_timeout: false,
            half_frame: false,
        };
        // The configuration is checked before the pedal marks anything pending.
        if let Err(why) = self.cfg.check_step() {
            report.end = ScanEnd::NotRun {
                why: format!("the scan supervisor's configuration: {why}"),
            };
            // Nothing marked, nothing spawned: nothing to settle (as a pedal refusal).
            self.account(&report);
            return report;
        }
        if let Err(e) = pedal.pending(scan) {
            // Nothing was spawned, and the pending mark did not land: nothing to settle.
            report.end = ScanEnd::NotRun {
                why: format!("the pedal could not mark the path pending: {e}"),
            };
            self.account(&report);
            return report;
        }
        report.pending_at = Some(Instant::now());
        if let Err(why) = self.run(scan, &mut report) {
            report.end = ScanEnd::NotRun { why };
        }
        self.account(&report);
        if let Err(e) = pedal.settle(&report) {
            report.pedal_error = Some(e.to_string());
        }
        report
    }

    fn account(&self, report: &ScanReport) {
        let c = &self.counters;
        let path = report.scan.path();
        match &report.end {
            ScanEnd::Verdict(r) => {
                c.verdicts.fetch_add(1, Ordering::Relaxed);
                if report.end.is_strike() {
                    c.strikes.fetch_add(1, Ordering::Relaxed);
                }
                if let ScanResult::Failed {
                    failure: ScanFailure::ProtocolAfterScan,
                    detail,
                } = r
                {
                    c.protocol_errors.fetch_add(1, Ordering::Relaxed);
                    let stderr_tag = self.cfg.names.stderr;
                    runecraft_rt::safe_eprintln!(
                        "{stderr_tag}: scan {path}: a protocol fault after the scan's receipt: a STRIKE \
                         (protocolAfterScan): {detail}"
                    );
                }
            }
            ScanEnd::HostFault { exit_code } => {
                c.host_faults.fetch_add(1, Ordering::Relaxed);
                let meaning = if *exit_code == EXIT_USAGE {
                    "a usage error or no fd 3"
                } else {
                    "a protocol error in what the host sent"
                };
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!(
                    "{stderr_tag}: scan {path}: ENGINE-SIDE ERROR: the scan child exited {exit_code} ({meaning}). The \
                     host's fault, not the plugin's: no verdict, no strike. Its stderr: {:?}",
                    report.stderr.text()
                );
            }
            ScanEnd::EngineFault { why } if why.starts_with(BLOB_OVER_BUDGET) => {
                c.engine_faults.fetch_add(1, Ordering::Relaxed);
                c.blob_over_budget.fetch_add(1, Ordering::Relaxed);
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!(
                    "{stderr_tag}: scan {path}: ENGINE-SIDE ERROR: {why}. No verdict, no strike. Its \
                     stderr: {:?}",
                    report.stderr.text()
                );
            }
            ScanEnd::EngineFault { why } if why.starts_with(OVERSIZED_SCANNED) => {
                c.engine_faults.fetch_add(1, Ordering::Relaxed);
                c.oversized_scanned.fetch_add(1, Ordering::Relaxed);
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!(
                    "{stderr_tag}: scan {path}: ENGINE-SIDE ERROR: {why}. No verdict, no strike. \
                     Its stderr: {:?}",
                    report.stderr.text()
                );
            }
            ScanEnd::EngineFault { why } => {
                c.engine_faults.fetch_add(1, Ordering::Relaxed);
                if why.starts_with("a protocol fault") {
                    c.protocol_errors.fetch_add(1, Ordering::Relaxed);
                }
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!(
                    "{stderr_tag}: scan {path}: ENGINE-SIDE ERROR before any plugin code could run: {why}. No verdict, \
                     no strike. Its stderr: {:?}",
                    report.stderr.text()
                );
            }
            ScanEnd::NotRun { why } => {
                c.not_run.fetch_add(1, Ordering::Relaxed);
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!(
                    "{stderr_tag}: scan {path}: ENGINE-SIDE ERROR: not run: {why}. No verdict, no strike"
                );
            }
        }
        if report.we_sent {
            c.budget_kills.fetch_add(1, Ordering::Relaxed);
        }
        if report.stderr.open {
            c.stderr_held_open.fetch_add(1, Ordering::Relaxed);
        }
        c.leftovers
            .fetch_add(report.leftovers as u64, Ordering::Relaxed);
        if !report.escapees.is_empty() {
            c.escapees
                .fetch_add(report.escapees.len() as u64, Ordering::Relaxed);
            let stderr_tag = self.cfg.names.stderr;
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: scan {path}: STRAY PROCESSES: {} process(es) left the scan's group and outlived it, pids \
                 {:?}. They are outside the group, so no group kill reached them: they are still running.",
                report.escapees.len(),
                report.escapees
            );
        }
        if report.drain_timeout {
            c.drain_timeouts.fetch_add(1, Ordering::Relaxed);
            let stderr_tag = self.cfg.names.stderr;
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: scan {path}: DRAIN TIMEOUT: the scan socket was still open {:?} after the leader's exit \
                 (held by a process outside the group); judged on what had arrived",
                self.cfg.drain
            );
        }
        if report.outer_limit {
            c.outer_limits.fetch_add(1, Ordering::Relaxed);
            let stderr_tag = self.cfg.names.stderr;
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: scan {path}: OUTER LIMIT: the leader {:?} had not exited {:?} after the first kill; it \
                 was handed to a reaper thread",
                report.pid,
                self.cfg.outer_limit
            );
        }
    }

    /// The spawn, the conversation, the budget, the reap and the verdict. `Err`: nothing was probed (the spawn or a
    /// thread failed; a leader already spawned is killed and reaped by its `Drop`).
    fn run(&mut self, scan: &Scan, report: &mut ScanReport) -> Result<(), String> {
        let (ours, theirs) =
            runecraft_ipc::channel::channel_pair().map_err(|e| format!("socketpair: {e}"))?;
        let (err_r, err_w) = std::io::pipe().map_err(|e| format!("stderr pipe: {e}"))?;
        // (N3, the step) and the log (`--log-dir`, its levels and salt in the environment).
        let mut args = self.cfg.args_for(std::process::id(), scan.format());
        args.extend(self.cfg.levels.child_args());
        let env = summoner::supervisor::host_child_env(&*self.cfg.levels, self.cfg.env.clone());
        let t0 = Instant::now();
        let leader = runecraft_ipc::process::spawn_group(
            &runecraft_ipc::process::Spawn::new(&self.cfg.program, &args, &env),
            theirs,
            Some(&err_w),
        )
        .map_err(|e| format!("spawn {}: {e}", self.cfg.program.display()))?;
        // The socket end: our copies of the child's ends go at once, so its death reaches the reader as EOF (and
        // the stderr reader sees EOF when the last writer of the group is gone). The channel end went with the spawn.
        drop(err_w);
        let (mut ours, rsock) = ours
            .bind(&leader)
            .map_err(|e| format!("socket clone: {e}"))?;
        report.spawned_at = Some(Instant::now());
        report.pid = Some(leader.pid());
        report.pgid = Some(leader.pgid());
        let deadline = t0 + self.cfg.budget_for(scan.format());

        let reaped = Arc::new(AtomicBool::new(false));
        let err_thread = spawn_stderr_reader(err_r, reaped.clone(), self.cfg.names.scan_thread)
            .map_err(|e| format!("the stderr reader: {e}"))?;
        let (tx, rx) = sync_channel::<Inbound>(INBOUND_BOUND);
        let rtx = tx.clone();
        // The test seams' flags (read only under a seam): the conversation has seen the leader's exit; the reader has
        // sent its end.
        let seams = Seams {
            hold_scanned: self.cfg.hold_scanned,
            hold_end: self.cfg.hold_end,
            exit_seen: Arc::new(AtomicBool::new(false)),
            reader_ended: Arc::new(AtomicBool::new(false)),
        };
        let exit_seen = seams.exit_seen.clone();
        let reader_ended = seams.reader_ended.clone();
        let hold_exit_for_end = self.cfg.hold_exit_for_end;
        let blob_budget = self.cfg.blob_budget;
        let reader = std::thread::Builder::new()
            .name(format!("{}-read", self.cfg.names.scan_thread))
            .spawn(move || read_frames(rsock, rtx, seams, blob_budget))
            .map_err(|e| format!("the reader: {e}"))?;
        let pid = leader.pid();
        let pgid = leader.pgid();
        let exit_waiter = leader.exit_waiter();
        let waiter = std::thread::Builder::new()
            .name(format!("{}-wait", self.cfg.names.scan_thread))
            .spawn(move || {
                let exit = exit_waiter.wait();
                if hold_exit_for_end {
                    seam_wait(&reader_ended);
                }
                let _ = tx.send(Inbound::LeaderExited(exit));
            })
            .map_err(|e| format!("the waiter: {e}"))?;

        let mut conv = Conversation {
            scan,
            child_id: &self.cfg.child_id,
            sock: &mut ours,
            quiet: false,
            step: Step::Hello,
            scan_sent: false,
            scanned: None,
            discovered: Vec::new(),
            fault: None,
            half_frame: false,
            protocol: 0,
        };
        conv.send_hello();
        let mut tree = Tree {
            root: pid,
            seen: Default::default(),
        };
        tree.walk();
        let mut last_walk = Instant::now();
        let mut we_sent = false;
        let mut first_kill: Option<Instant> = None;
        let mut killed_at: Option<Instant> = None;
        let kill = |at: Instant, first: &mut Option<Instant>, last: &mut Option<Instant>| {
            let _ = leader.kill_group();
            first.get_or_insert(at);
            *last = Some(at);
        };
        let mut outer = false;
        let mut exited_at: Option<Instant> = None;
        let mut reader_done = false;
        loop {
            let now = Instant::now();
            if now >= last_walk + TREE_POLL {
                tree.walk();
                last_walk = now;
            }
            if !we_sent && conv.fault.is_none() && now >= deadline {
                tree.walk();
                // The flag first, then the signal (the budget's rule; the host's `we_sent`).
                we_sent = true;
                kill(now, &mut first_kill, &mut killed_at);
            }
            if let Some(k) = killed_at
                && now >= k + REKILL
            {
                kill(now, &mut first_kill, &mut killed_at);
            }
            // The outer limit; its mutant (never in a normal build) has none.
            #[cfg(not(mutant_no_outer_limit))]
            if let Some(f) = first_kill
                && now >= f + self.cfg.outer_limit
            {
                outer = true;
                break;
            }
            let mut wait = TREE_POLL;
            if !we_sent && conv.fault.is_none() {
                wait = wait.min(deadline.saturating_duration_since(now));
            }
            if let Some(k) = killed_at {
                wait = wait.min((k + REKILL).saturating_duration_since(now));
            }
            if let Some(f) = first_kill {
                wait = wait.min((f + self.cfg.outer_limit).saturating_duration_since(now));
            }
            let wait = wait.max(Duration::from_millis(1));
            match rx.recv_timeout(wait) {
                Ok(Inbound::Frame(f)) => {
                    if !we_sent && conv.fault.is_none() {
                        conv.frame(f);
                        if conv.protocol != 0 {
                            self.counters
                                .child_protocol
                                .store(conv.protocol, Ordering::Relaxed);
                        }
                        if conv.fault.is_some() {
                            kill(Instant::now(), &mut first_kill, &mut killed_at);
                        }
                    }
                }
                Ok(Inbound::ReaderDone(r)) => {
                    reader_done = true;
                    if let Err(f) = r
                        && !we_sent
                        && conv.fault.is_none()
                    {
                        conv.reader_fault(f);
                        // A half frame is no fault (the end decides): no kill, the budget still runs.
                        if conv.fault.is_some() {
                            kill(Instant::now(), &mut first_kill, &mut killed_at);
                        }
                    }
                }
                Ok(Inbound::LeaderExited(r)) => {
                    if let Err(e) = &r {
                        // The contract says this cannot happen (only `reap` reaps the leader); the reap below
                        // reports the truth.
                        let stderr_tag = self.cfg.names.stderr;
                        runecraft_rt::safe_eprintln!(
                            "{stderr_tag}: scan: waitid(WNOWAIT) on {pid}: {e}"
                        );
                    }
                    if !self.cfg.hold_exit {
                        exited_at = Some(Instant::now());
                        exit_seen.store(true, Ordering::Release);
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if !self.cfg.hold_exit {
                        // The waiter is gone without a word: fall through to the blocking reap.
                        exited_at = Some(Instant::now());
                        exit_seen.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(wait);
                }
            }
        }
        // The leader has exited (or the outer limit ran out) and is still our unreaped child: whatever it left in its
        // group is counted and goes now, then the leader is reaped (and only then can its id be reused).
        report.leftovers = leftovers(pgid, pid);
        report.sweep = Some(leader.kill_group());
        let exit = if outer {
            reap_later(leader, self.cfg.names);
            None
        } else {
            Some(leader.reap().map_err(|e| format!("reap {pid}: {e}"))?)
        };
        reaped.store(true, Ordering::Release);
        // The drain (after the sweep, so the group members holding fd 3 are dying; after the reaped flag, so it runs
        // at the same time as the stderr grace): what the dead child wrote before it died is judged and kept, never
        // lost to which thread saw the end first. Its mutant (never in a normal build) has none.
        #[cfg(not(mutant_no_scan_drain))]
        if !outer && !reader_done && !we_sent && conv.fault.is_none() {
            let (took, timed_out) = conv.drain(&rx, self.cfg.drain);
            report.drain = Some(took);
            report.drain_timeout = timed_out;
        }
        let _ = conv.sock.shutdown();
        drop(rx);
        report.reader_blob_bytes = reader.join().unwrap_or(0);
        if !outer {
            let _ = waiter.join();
        } // else detached: it ends when the leader does
        report.stderr = err_thread.join().unwrap_or_default();
        report.escapees = tree.escapees(pgid);
        report.elapsed = exited_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(t0);
        report.exit = exit;
        report.we_sent = we_sent;
        report.outer_limit = outer;
        report.scan_sent = conv.scan_sent;
        report.half_frame = conv.half_frame;
        report.crash_log = self
            .cfg
            .crash_log
            .as_deref()
            .and_then(|d| read_crash_log(d, pid));
        report.end = judge(
            conv.fault.take(),
            conv.scan_sent,
            we_sent,
            exit,
            conv.scanned.as_ref(),
        );
        // Discovery's classes count only beside the line's own `failed` verdict (a clean exit 0 after it; a child
        // cannot send protocolAfterScan, so a `failed` verdict that is not the supervisor's own is the line's).
        if matches!(&report.end, ScanEnd::Verdict(ScanResult::Failed { failure, .. })
            if *failure != ScanFailure::ProtocolAfterScan)
        {
            report.discovered = std::mem::take(&mut conv.discovered);
        }
        report.scanned = conv.scanned.take();
        Ok(())
    }
}

/// A leader that outlived the outer limit is reaped on a late-reaper thread, so the scan returns. If even
/// that thread cannot be made, the leader is left unreaped (forgotten: a zombie until the host application exits) rather than
/// block the caller.
fn reap_later(leader: runecraft_ipc::process::GroupLeader, names: summoner::supervisor::Names) {
    let slot = Arc::new(std::sync::Mutex::new(Some(leader)));
    let theirs = slot.clone();
    let spawned = std::thread::Builder::new()
        .name(format!("{}-reap-late", names.scan_thread))
        .spawn(move || {
            let l = theirs.lock().ok().and_then(|mut s| s.take());
            if let Some(l) = l {
                let _ = l.reap();
            }
        });
    if spawned.is_err()
        && let Some(l) = slot.lock().ok().and_then(|mut s| s.take())
    {
        let stderr_tag = names.stderr;
        runecraft_rt::safe_eprintln!(
            "{stderr_tag}: scan: no reaper thread for leader {}: left unreaped",
            l.pid()
        );
        std::mem::forget(l);
    }
}

/// The host application's side of one conversation (the scan's order, and no other).
struct Conversation<'a> {
    scan: &'a Scan,
    child_id: &'a str,
    sock: &'a mut runecraft_ipc::channel::ChanTx,
    /// Set for the drain: the leader has exited, so nothing more is written (no one to answer it, and a write to a
    /// socket an escapee holds could stall the drain past its bound). A write the conversation would make is not made
    /// and reads as failed (so a `hello` reply drained after the death never marks the scan request sent).
    quiet: bool,
    step: Step,
    /// The scan request was written: the strike boundary.
    scan_sent: bool,
    scanned: Option<Scanned>,
    /// A form B `failed` line's classes, beside [`Self::scanned`].
    discovered: Vec<codec::ScanClass>,
    fault: Option<Fault>,
    /// The reader's end was EOF mid-frame ([`ScanReport::half_frame`]).
    half_frame: bool,
    /// The verified `hello`'s protocol (0 before it).
    protocol: u32,
}

impl Conversation<'_> {
    /// True when the frame was written. A write that fails (the child gone: EPIPE; Rust ignores SIGPIPE) or stalls
    /// past SEND_TIMEOUT is no verdict: the exit or the budget decides.
    fn write<T: serde::Serialize>(&mut self, k: u16, id: u64, meta: &T) -> bool {
        if self.quiet {
            return false;
        }
        self.sock
            .send(
                &codec::encode_msg(k, 0, id, meta, &[]),
                &[],
                Instant::now() + SEND_TIMEOUT,
            )
            .is_ok()
    }

    fn send_hello(&mut self) {
        let hello = codec::Hello::new(self.child_id.to_string(), 0);
        self.write(kind::HELLO, ID_HELLO, &hello);
    }

    /// The drain *(branch `scan-drain`, 2026-11-01)*, after the leader's exit: every frame and the reader's end are
    /// taken exactly as the main loop takes them (the same checks, the same faults: the verdict table is unchanged; only
    /// what reaches it no longer depends on which thread ran first), until the reader's end, a fault (from then on
    /// nothing is taken, as in the main loop) or `bound`. Nothing is written ([`Self::quiet`]). Returns how long it took
    /// and whether `bound` ran out first. Never blocks past `bound`: one `recv_timeout` on what is left of it per
    /// message, and taking a frame neither blocks nor writes.
    fn drain(
        &mut self,
        rx: &std::sync::mpsc::Receiver<Inbound>,
        bound: Duration,
    ) -> (Duration, bool) {
        self.quiet = true;
        let t0 = Instant::now();
        let until = t0 + bound;
        let timed_out = loop {
            if self.fault.is_some() {
                break false;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break true;
            }
            match rx.recv_timeout(left) {
                Ok(Inbound::Frame(f)) => self.frame(f),
                Ok(Inbound::ReaderDone(r)) => {
                    if let Err(f) = r {
                        self.reader_fault(f);
                    }
                    break false;
                }
                // The waiter sends once, and it has.
                Ok(Inbound::LeaderExited(_)) => {}
                Err(RecvTimeoutError::Timeout) => break true,
                // Every sender is gone: the reader has ended (its `ReaderDone` is its last send).
                Err(RecvTimeoutError::Disconnected) => break false,
            }
        };
        (t0.elapsed(), timed_out)
    }

    /// A protocol fault, on the side of the receipt the conversation is on.
    fn fail(&mut self, why: String) {
        self.fault = Some(Fault {
            why,
            after_receipt: self.step >= Step::Scanned,
            oversized_scanned: None,
            over_budget: None,
            scanned_limit: None,
        });
    }

    /// A `scanned` refused after its header: an engine limit (the description limits) is no verdict and
    /// no strike; anything else is [`Self::fail`] (a strike after the receipt). A form A meta with a blob keeps the
    /// words the conversation always used for a blob it did not expect.
    fn scanned_fault(&mut self, what: String, e: codec::ScannedFault) {
        match e {
            codec::ScannedFault::Limit(_) => {
                self.fail(format!("{what}: {e}"));
                if let Some(f) = &mut self.fault {
                    f.scanned_limit = Some(e.to_string());
                }
            }
            codec::ScannedFault::Malformed(codec::ScannedMalformed::FormABlob { blob_len }) => {
                self.fail(format!("{what} carries a {blob_len}-byte blob (form A)"))
            }
            // A malformed line, or a fault a later codec names: a strike, in the codec's words.
            _ => self.fail(format!("scanned: {e}")),
        }
    }

    /// The reader ended early. Exactly one case is the engine's limit and not a protocol fault *(
    /// 2026-11-01)*: the conversation waits for `scanned`, and the decoder refused a `scanned` EVENT (id 0) for its
    /// meta size alone. Anything else, an oversized frame of another kind or flags included, is [`Self::fail`].
    ///
    /// **Half a frame, then the end** *(matching the live side)*: an EOF mid-frame is no
    /// protocol fault but the trace of the end that cut the frame, so the end decides by the verdict table (a signal:
    /// `crashed {signal}`; an exit: its row), the same whichever thread saw it first. It is kept in the record. Its
    /// mutant (never in a normal build) restores the old mapping, `protocolAfterScan` after the receipt.
    ///
    /// The reader's blob budget is the engine's limit too, at any step.
    fn reader_fault(&mut self, f: ReaderFault) {
        #[cfg(not(mutant_half_frame_struck))]
        if f.half_frame {
            self.half_frame = true;
            return;
        }
        self.fail(f.why);
        if let Some(b) = f.over_budget
            && let Some(fault) = &mut self.fault
        {
            fault.over_budget = Some(b);
            return;
        }
        if self.step == Step::Scanned
            && let Some(h) = f.oversized
            && h.kind == kind::SCANNED
            && h.flags == FF_EVENT
            && h.id == 0
            && let Some(fault) = &mut self.fault
        {
            fault.oversized_scanned = Some(h.meta_len);
        }
    }

    /// One frame from the child, checked against the step it may answer.
    fn frame(&mut self, f: Frame) {
        let h = f.header;
        let what = || {
            format!(
                "{} (kind {}, flags {}, id {})",
                codec::kind_name(h.kind).unwrap_or("?"),
                h.kind,
                h.flags,
                h.id
            )
        };
        // Only the `scanned` EVENT (id 0) the conversation waits for may carry a blob (form B's); the reader has
        // already refused any other blob, and a second one, from its header.
        let scanned_now = self.step == Step::Scanned
            && h.kind == kind::SCANNED
            && h.flags == FF_EVENT
            && h.id == 0;
        if !f.blob.is_empty() && !scanned_now {
            return self.fail(format!("{} carries a {}-byte blob", what(), f.blob.len()));
        }
        if let Err(e) = codec::check_to_host(&f) {
            return self.fail(format!("{}: {e}", what()));
        }
        if h.is_error() {
            let e = f.meta_as::<ErrorReply>().unwrap_or_default();
            return self.fail(format!(
                "an error reply to {}: {} {:?}",
                what(),
                e.code,
                e.message
            ));
        }
        let reply = h.flags & FF_REPLY != 0;
        let event = h.flags & FF_EVENT != 0;
        debug_assert!(h.flags & FF_ERROR == 0);
        match (self.step, h.kind, reply, event, h.id) {
            (Step::Hello, kind::HELLO, true, false, ID_HELLO) => {
                let m: HelloReply = match f.meta_as() {
                    Ok(m) => m,
                    Err(e) => return self.fail(format!("hello reply: {e}")),
                };
                if m.protocol >> 16 != ABI_VERSION >> 16 {
                    return self.fail(format!(
                        "protocol {}.{}, this host speaks {}.x",
                        m.protocol >> 16,
                        m.protocol & 0xffff,
                        ABI_VERSION >> 16
                    ));
                }
                self.protocol = m.protocol;
                self.step = Step::Receipt;
                // part 1: this host reads form B, so every scan request offers it; a child that
                // knows form B then answers in it whatever the size (a failed validation's `discovered` included).
                // The mutant `--cfg mutant_scan_no_offer` (never in a normal build) sends the bare request.
                let scan = if cfg!(mutant_scan_no_offer) {
                    self.scan.clone()
                } else {
                    self.scan.clone().offer_descriptions_blob()
                };
                self.scan_sent = self.write(kind::SCAN, ID_SCAN, &scan);
            }
            (Step::Receipt, kind::SCAN, true, false, ID_SCAN) => {
                self.step = Step::Scanned;
            }
            (Step::Scanned, kind::SCANNED, false, true, 0) => {
                // The meta, form A or form B; then the echo check; then the blob's checks.
                let meta = match codec::parse_scanned_meta(&f.meta) {
                    Ok(m) => m,
                    Err(e) => return self.scanned_fault(what(), e),
                };
                if meta.format() != self.scan.format() || meta.path() != self.scan.path() {
                    return self.fail(format!(
                        "scanned echoes {:?} {:?}, the request was {:?} {:?}",
                        meta.format(),
                        meta.path(),
                        self.scan.format(),
                        self.scan.path()
                    ));
                }
                let offered = !cfg!(mutant_scan_no_offer);
                let line = match meta.with_blob(&f.blob, offered) {
                    Ok(l) => l,
                    Err(e) => return self.scanned_fault(what(), e),
                };
                let s = match &line {
                    codec::ScannedLine::A(s) => s.clone(),
                    codec::ScannedLine::B(b) => Scanned::new(
                        b.format,
                        b.path.clone(),
                        b.outcome.clone(),
                        match b.outcome {
                            codec::ScannedOutcome::Ok => b.descriptions.clone(),
                            _ => Vec::new(),
                        },
                    ),
                };
                // Discovery's classes through the codec's one function (its mutant `mutant_discovered_dropped` drops
                // them).
                self.discovered = line.into_verdict().1;
                self.scanned = Some(s);
                self.step = Step::Shutdown;
                let _ = self.write(kind::SHUTDOWN, ID_SHUTDOWN, &serde_json::json!({}));
            }
            (Step::Shutdown, kind::SHUTDOWN, true, false, ID_SHUTDOWN) => {
                self.step = Step::Done;
            }
            (step, ..) => self.fail(format!("{} out of order (waiting for {step:?})", what())),
        }
    }
}

/// The test seams' longest hold: a seam waits for its flag at most this long (a test that misuses one is slowed,
/// never hung).
pub const SEAM_HOLD_MAX: Duration = Duration::from_secs(10);

/// The reader's and the waiter's test seams ([`ScanConfig::seam_hold_scanned_until_exit`],
/// [`ScanConfig::seam_hold_reader_end_until_exit`], [`ScanConfig::seam_hold_exit_until_reader_end`]): both holds false
/// in production, where the flags are set and never read.
struct Seams {
    hold_scanned: bool,
    hold_end: bool,
    exit_seen: Arc<AtomicBool>,
    reader_ended: Arc<AtomicBool>,
}

/// A seam's hold: until `flag` is set, at most [`SEAM_HOLD_MAX`].
fn seam_wait(flag: &AtomicBool) {
    let until = Instant::now() + SEAM_HOLD_MAX;
    while !flag.load(Ordering::Acquire) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The reader's blob budget, the decoder's admission gate: it sees each header
/// before its body is allocated and admits it only if its declared blob keeps the conversation's admitted blob bytes
/// within the budget. The sum never goes down (a conversation has one legal blob at most), so what it admits is a
/// function of the byte stream alone, never of how fast the conversation drains the channel.
struct BlobGate {
    budget: u64,
    admitted: u64,
    refused: Option<OverBudget>,
    /// *(The blob count rule)* The blob frames
    /// admitted in this conversation (at most one).
    blobs: u32,
    /// The frame the blob count rule refused: a blob on anything but a `scanned` EVENT with id 0, or a second blob.
    refused_count: Option<Header>,
}

impl BlobGate {
    fn admit(&mut self, h: &Header) -> bool {
        if h.blob_len == 0 {
            return true;
        }
        // One blob per conversation, and only on a `scanned` EVENT with id 0. Its mutant (never in a normal
        // build): the rule skipped, so only the budget bounds the blobs.
        let legal = h.kind == kind::SCANNED && h.flags == FF_EVENT && h.id == 0 && self.blobs == 0;
        if !legal && !cfg!(mutant_scan_reader_unbounded) {
            self.refused_count = Some(*h);
            return false;
        }
        let after = self.admitted.saturating_add(u64::from(h.blob_len));
        // Its mutant (never in a normal build): no budget, every blob admitted (and still counted).
        #[cfg(mutant_scan_blob_budget_off)]
        let over = false;
        #[cfg(not(mutant_scan_blob_budget_off))]
        let over = after > self.budget;
        if over {
            self.refused = Some(OverBudget {
                header: *h,
                admitted: self.admitted,
                budget: self.budget,
            });
            return false;
        }
        self.admitted = after;
        self.blobs += 1;
        true
    }
}

/// The reader thread: frames from the child through the host application's `Decoder`, each admitted by the blob budget from its
/// header before its body is allocated; the end (clean EOF, or why not) last. Returns the blob bytes it admitted.
fn read_frames(
    mut sock: runecraft_ipc::channel::ChanRx,
    tx: SyncSender<Inbound>,
    seams: Seams,
    blob_budget: u64,
) -> u64 {
    let mut gate = BlobGate {
        budget: blob_budget,
        admitted: 0,
        refused: None,
        blobs: 0,
        refused_count: None,
    };
    let mut dec = codec::Decoder::new(Direction::ToHost);
    let mut buf = vec![0u8; 64 * 1024];
    let mut fds: Vec<runecraft_ipc::channel::Transferable> = Vec::new();
    let end = loop {
        let n = match sock.recv(&mut buf, &mut fds, None) {
            Ok(runecraft_ipc::channel::RecvStatus::Eof) => {
                break if dec.at_boundary() {
                    Ok(())
                } else {
                    Err(ReaderFault {
                        half_frame: true,
                        ..ReaderFault::other("EOF mid-frame".into())
                    })
                };
            }
            Ok(runecraft_ipc::channel::RecvStatus::Data(n)) => n,
            // No deadline: a timeout is never seen (the read blocks until data or the end).
            Ok(runecraft_ipc::channel::RecvStatus::TimedOut) => continue,
            Err(e) => break Err(ReaderFault::other(format!("recv: {e}"))),
        };
        if !fds.is_empty() {
            fds.clear(); // closes them: a scan child never sends descriptors
            break Err(ReaderFault::other("descriptors from the scan child".into()));
        }
        let mut out: Vec<Frame> = Vec::new();
        let r = dec.feed_admitting(&buf[..n], &mut |h| gate.admit(h), &mut |f| out.push(f));
        for f in out {
            if seams.hold_scanned && f.header.kind == kind::SCANNED {
                seam_wait(&seams.exit_seen);
            }
            if tx.send(Inbound::Frame(f)).is_err() {
                return gate.admitted;
            }
        }
        if let Err(e) = r {
            if let Some(h) = gate.refused_count {
                // The blob count rule: the child's protocol fault (a strike after the receipt), never a limit.
                break Err(ReaderFault::other(format!(
                    "a frame the reader refused from its header (one blob per conversation, on the \
                     scanned EVENT with id 0 only): {} (kind {}, flags {}, id {}) declares a {}-byte blob, after {} \
                     blob frame(s): {e}",
                    codec::kind_name(h.kind).unwrap_or("?"),
                    h.kind,
                    h.flags,
                    h.id,
                    h.blob_len,
                    gate.blobs
                )));
            }
            break Err(match gate.refused {
                Some(b) => ReaderFault {
                    why: format!(
                        "a frame past the reader's blob budget: {e}: {} blob bytes admitted, {} declared, a \
                         {}-byte budget",
                        b.admitted, b.header.blob_len, b.budget
                    ),
                    oversized: None,
                    over_budget: Some(b),
                    half_frame: false,
                },
                None => ReaderFault {
                    why: format!("a frame the decoder refused: {e}"),
                    // The size limits: the meta cap, the blob cap, or both (form B's blob may pass MAX_BLOB).
                    oversized: dec.oversized_header(),
                    over_budget: None,
                    half_frame: false,
                },
            });
        }
    };
    if seams.hold_end {
        seam_wait(&seams.exit_seen);
    }
    let _ = tx.send(Inbound::ReaderDone(end));
    seams.reader_ended.store(true, Ordering::Release);
    gate.admitted
}

/// The crash log of `pid` in `dir`, bounded, if it exists. Read only: no directory is ever created, and an absent
/// directory or file is simply no log.
pub fn read_crash_log(dir: &Path, pid: u32) -> Option<Captured> {
    let path = dir.join(summoner_proto::host_names::crash_log_name(pid));
    let f = std::fs::File::open(&path).ok()?;
    let mut keep = Bounded::new(CRASH_LOG_CAP, HEAD);
    let mut buf = vec![0u8; 8 * 1024];
    let mut f = f.take(16 * 1024 * 1024); // a log that grows without end is read no further than this
    loop {
        match f.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => keep.push(&buf[..n]),
        }
    }
    Some(keep.finish(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use summoner_proto::codec::{ScanFormat, ScannedOutcome};

    fn line() -> Scanned {
        Scanned::new(
            ScanFormat::Vst3,
            "/x/g.vst3".into(),
            ScannedOutcome::Ok,
            vec![],
        )
    }

    fn crashed(signal: Option<i32>, exit_code: Option<i32>) -> ScanEnd {
        ScanEnd::Verdict(match (signal, exit_code) {
            (Some(s), None) => ScanResult::crashed_by(OsCode::Signal(s)),
            (None, Some(c)) => ScanResult::exited_early(c),
            other => panic!("not a crash's combination: {other:?}"),
        })
    }

    /// The verdict table, row by row, with and without an earlier `scanned` (the death wins; only a clean exit
    /// 0 keeps the line), on both sides of the strike boundary. The mutants
    /// `mutant_budget_kill_crashed`, `mutant_usage_exit_strike` and
    /// `mutant_fault_after_receipt_unstruck` fail here.
    #[test]
    fn the_verdict_table_as_ruled() {
        let s = line();
        let budget = ScanEnd::Verdict(ScanResult::Timeout {
            kind: TimeoutKind::Budget,
        });
        let step = ScanEnd::Verdict(ScanResult::Timeout {
            kind: TimeoutKind::Step,
        });
        let ex = |e| Some(e);
        for sc in [None, Some(&s)] {
            // From the scan request on.
            assert_eq!(
                judge(None, true, true, ex(ChildExit::Signaled(9)), sc),
                budget
            );
            assert_eq!(
                judge(None, true, true, ex(ChildExit::Exited(0)), sc),
                budget,
                "the flag decides"
            );
            assert_eq!(judge(None, true, true, None, sc), budget, "the outer limit");
            assert_eq!(
                judge(None, true, false, ex(ChildExit::Signaled(9)), sc),
                crashed(Some(9), None)
            );
            assert_eq!(
                judge(None, true, false, ex(ChildExit::Signaled(11)), sc),
                crashed(Some(11), None)
            );
            // An unhandled exception (Windows' NTSTATUS; never produced on macOS) is a crash with its code, as an
            // exception (its kind is kept, so its name is `STATUS_ACCESS_VIOLATION`).
            let av = judge(None, true, false, ex(ChildExit::Exception(0xC000_0005)), sc);
            assert_eq!(
                av,
                ScanEnd::Verdict(ScanResult::crashed_by(OsCode::Exception(0xC000_0005)))
            );
            assert_eq!(
                judge(None, true, false, ex(ChildExit::Exited(75)), sc),
                step
            );
            assert_eq!(
                judge(None, true, false, ex(ChildExit::Exited(3)), sc),
                crashed(None, Some(3))
            );
            for c in [1, 5, 9, 74, 76, 255] {
                assert_eq!(
                    judge(None, true, false, ex(ChildExit::Exited(c)), sc),
                    crashed(None, Some(c))
                );
            }
            for sent in [false, true] {
                for c in [2, 4] {
                    let e = judge(None, sent, false, ex(ChildExit::Exited(c)), sc);
                    assert_eq!(e, ScanEnd::HostFault { exit_code: c });
                    assert!(!e.is_strike() && e.verdict().is_none());
                }
            }
            // A protocol fault: struck after the receipt (its own reason), not before.
            let after = Fault {
                why: "x".into(),
                after_receipt: true,
                scanned_limit: None,
                oversized_scanned: None,
                over_budget: None,
            };
            let p = judge(Some(after), true, false, ex(ChildExit::Signaled(9)), sc);
            assert_eq!(
                p,
                ScanEnd::Verdict(ScanResult::Failed {
                    failure: ScanFailure::ProtocolAfterScan,
                    detail: "x".into()
                })
            );
            assert!(p.is_strike());
            let before = Fault {
                why: "x".into(),
                after_receipt: false,
                scanned_limit: None,
                oversized_scanned: None,
                over_budget: None,
            };
            let p = judge(Some(before), true, false, ex(ChildExit::Signaled(9)), sc);
            assert!(matches!(p, ScanEnd::EngineFault { .. }), "{p:?}");
            assert!(!p.is_strike());
            // The `scanned` line over MAX_META, after the receipt, is the engine's
            // limit: no verdict, no strike, whatever the end. Its mutant `mutant_oversized_scanned_struck`
            // fails here.
            for e in [
                None,
                ex(ChildExit::Signaled(9)),
                ex(ChildExit::Exited(0)),
                ex(ChildExit::Exited(75)),
            ] {
                let big = Fault {
                    why: "a frame the decoder refused: MetaTooLong(70000)".into(),
                    after_receipt: true,
                    scanned_limit: None,
                    oversized_scanned: Some(70_000),
                    over_budget: None,
                };
                let p = judge(Some(big), true, false, e, sc);
                match &p {
                    ScanEnd::EngineFault { why } => {
                        assert!(why.starts_with(OVERSIZED_SCANNED), "{why}");
                        assert!(why.contains("70000 bytes"), "{why}");
                    }
                    other => panic!("an engine fault: {other:?}"),
                }
                assert!(!p.is_strike() && p.verdict().is_none());
            }
            // The reader's blob budget is the engine's limit on either side of
            // the receipt, whatever the end: no verdict, no strike.
            for (after_receipt, e) in [
                (true, None),
                (true, ex(ChildExit::Signaled(9))),
                (true, ex(ChildExit::Exited(0))),
                (false, ex(ChildExit::Signaled(9))),
            ] {
                let over = Fault {
                    why: "a frame past the reader's blob budget".into(),
                    after_receipt,
                    scanned_limit: None,
                    oversized_scanned: None,
                    over_budget: Some(OverBudget {
                        header: Header {
                            kind: kind::SCANNED,
                            flags: FF_EVENT,
                            id: 0,
                            meta_len: 2,
                            blob_len: 3_000_000,
                        },
                        admitted: 0,
                        budget: 1 << 20,
                    }),
                };
                let p = judge(Some(over), true, false, e, sc);
                match &p {
                    ScanEnd::EngineFault { why } => {
                        assert!(why.starts_with(BLOB_OVER_BUDGET), "{why}");
                        assert!(!why.starts_with(OVERSIZED_SCANNED), "{why}");
                        assert!(why.contains("3000000-byte blob"), "{why}");
                        assert!(why.contains("1048576-byte budget"), "{why}");
                    }
                    other => panic!("an engine fault: {other:?}"),
                }
                assert!(!p.is_strike() && p.verdict().is_none());
            }
        }
        // Before the scan request: never a strike (the strike principle).
        for e in [
            None,
            ex(ChildExit::Signaled(9)),
            ex(ChildExit::Exited(0)),
            ex(ChildExit::Exited(3)),
            ex(ChildExit::Exited(75)),
        ] {
            for we in [false, true] {
                let end = judge(None, false, we, e, None);
                assert!(
                    matches!(end, ScanEnd::EngineFault { .. }),
                    "{e:?} {we}: {end:?}"
                );
            }
        }
        assert_eq!(
            judge(None, true, false, ex(ChildExit::Exited(0)), None),
            crashed(None, Some(0))
        );
        assert_eq!(
            judge(None, true, false, ex(ChildExit::Exited(0)), Some(&s)),
            ScanEnd::Verdict(ScanResult::Ok {
                descriptions: vec![]
            })
        );
        assert!(budget.is_strike() && step.is_strike() && crashed(None, Some(0)).is_strike());
        assert!(
            !ScanEnd::Verdict(ScanResult::Ok {
                descriptions: vec![]
            })
            .is_strike()
        );
        assert!(
            !ScanEnd::Verdict(ScanResult::Failed {
                failure: ScanFailure::Process,
                detail: String::new()
            })
            .is_strike()
        );
    }

    /// the child's refusal of an AU code that is no identity is `failed {reason: "auCodes"}`.
    /// The host application reads it as a plain failure, the untagged `ScanFailure::Other` keeping the child's word: the frame
    /// passes the codec, the verdict is the child's `failed`, and it is never a strike (only `crashed`, `timeout` and
    /// the supervisor's own `protocolAfterScan` are).
    #[test]
    fn an_au_codes_refusal_is_a_plain_failure_never_a_strike() {
        let detail =
            "AudioComponents entry 1: the subtype code's byte 1 is outside printable ASCII";
        let meta = serde_json::json!({
            "format": "au",
            "path": "/x/plugins/au/badcodes.component",
            "outcome": "failed",
            "reason": "auCodes",
            "detail": detail,
            "descriptions": [],
        });
        let mut fs = Vec::new();
        codec::Decoder::new(Direction::ToHost)
            .feed(
                &codec::encode_msg(kind::SCANNED, FF_EVENT, 0, &meta, &[]),
                &mut |f| fs.push(f),
            )
            .unwrap();
        assert_eq!(fs.len(), 1);
        codec::check_to_host(&fs[0]).expect("the child's word passes the codec");
        let s: Scanned = fs[0].meta_as().unwrap();
        let want = ScanResult::Failed {
            failure: ScanFailure::Other {
                reason: "auCodes".into(),
            },
            detail: detail.into(),
        };
        assert_eq!(ScanResult::from_scanned(s.clone()), want);
        // The supervisor's end for the line and a clean exit 0: the child's verdict, kept, and no strike.
        let end = judge(None, true, false, Some(ChildExit::Exited(0)), Some(&s));
        assert_eq!(end, ScanEnd::Verdict(want));
        assert!(!end.is_strike(), "auCodes is the child's plain failure");
        assert_eq!(end.verdict().map(ScanResult::tag), Some("failed"));
    }

    /// Runs `read_frames` on one end of a socketpair while a writer thread sends `frames` on the other, then closes
    /// it. The channel is never drained until the reader ends, so what it admits cannot depend on a consumer.
    fn read_with_budget(frames: Vec<Vec<u8>>, budget: u64) -> (Vec<Inbound>, u64) {
        // Only the receive half, as the socketpair's one end it replaces: a reader that stops reading closes the
        // parent's end, so the writer's next write fails instead of blocking.
        let (tx, ours, theirs) = chan();
        drop(tx);
        let writer = std::thread::spawn(move || {
            let mut w = &theirs;
            for f in frames {
                if w.write_all(&f).is_err() {
                    break; // the reader stopped: its socket is gone
                }
            }
        });
        let (tx, rx) = sync_channel::<Inbound>(INBOUND_BOUND);
        let seams = Seams {
            hold_scanned: false,
            hold_end: false,
            exit_seen: Arc::new(AtomicBool::new(false)),
            reader_ended: Arc::new(AtomicBool::new(false)),
        };
        let admitted = read_frames(ours, tx, seams, budget);
        writer.join().unwrap();
        (rx.try_iter().collect(), admitted)
    }

    fn scanned_with_blob(n: usize) -> Vec<u8> {
        codec::encode(kind::SCANNED, FF_EVENT, 0, b"{}", &vec![0xa5; n])
    }

    /// The reader's blob budget refuses, from its header, the frame whose blob
    /// would pass it, whatever the consumer does: here one blob of the budget plus one byte. Nothing of it is
    /// allocated (the bytes admitted stay at 0), and the reader ends with the budget's fault. *( part 1: since
    /// The blob count rule runs first, a conversation holds one blob at most, so the budget is
    /// shown on that one blob; the two-blob case is the count rule's, below.)* Its mutant
    /// `mutant_scan_blob_budget_off` fails here.
    #[test]
    fn the_reader_refuses_the_blob_that_would_pass_its_budget_from_its_header() {
        let blob = (1 << 20) + 1;
        let (got, admitted) = read_with_budget(vec![scanned_with_blob(blob)], 1 << 20);
        assert_eq!(admitted, 0, "nothing was admitted");
        assert_eq!(got.len(), 1, "only the reader's end");
        match &got[0] {
            Inbound::ReaderDone(Err(f)) => {
                let b = f.over_budget.expect("the budget's fault");
                assert_eq!(b.header.blob_len as usize, blob);
                assert_eq!((b.admitted, b.budget), (0, 1 << 20));
                assert!(f.oversized.is_none());
                assert!(f.why.contains("NotAdmitted"), "{}", f.why);
            }
            _ => panic!("the reader ends with the budget's fault"),
        }
    }

    /// The boundary: a blob of exactly the budget is admitted; one byte more is refused. A frame with no blob is
    /// never counted, and past nothing the reader ends at a clean EOF.
    #[test]
    fn blobs_up_to_the_budget_are_admitted_and_one_byte_more_is_not() {
        let empty = codec::encode(kind::SCANNED, FF_EVENT, 0, b"{}", &[]);
        let (got, admitted) = read_with_budget(
            vec![empty.clone(), scanned_with_blob(1 << 20), empty.clone()],
            1 << 20,
        );
        assert_eq!(admitted, 1 << 20);
        assert_eq!(got.len(), 4);
        assert!(matches!(&got[3], Inbound::ReaderDone(Ok(()))));
        let (got, admitted) =
            read_with_budget(vec![empty, scanned_with_blob((1 << 20) + 1)], 1 << 20);
        assert_eq!(admitted, 0, "the one byte more was not admitted");
        assert_eq!(got.len(), 2, "one frame, then the reader's end");
        assert!(matches!(
            &got[1],
            Inbound::ReaderDone(Err(ReaderFault {
                over_budget: Some(OverBudget { admitted: 0, .. }),
                ..
            }))
        ));
    }

    /// A
    /// conversation admits ONE blob, on a `scanned` EVENT with id 0. A second blob, well within the budget, is refused
    /// from its header by the count rule (a protocol fault, never the budget's limit), and so is a blob on any other
    /// frame. Its mutant `mutant_scan_reader_unbounded` (the rule skipped) fails here: both blobs admitted.
    #[test]
    fn the_reader_admits_one_blob_per_conversation_and_only_on_scanned() {
        let (got, admitted) = read_with_budget(
            vec![scanned_with_blob(1000), scanned_with_blob(1000)],
            1 << 20,
        );
        assert_eq!(
            admitted, 1000,
            "the second blob was refused by the count rule"
        );
        assert_eq!(got.len(), 2, "one frame, then the reader's end");
        match &got[1] {
            Inbound::ReaderDone(Err(f)) => {
                assert!(f.over_budget.is_none(), "not the budget's");
                assert!(f.why.contains("one blob per conversation"), "{}", f.why);
                assert!(f.why.contains("after 1 blob frame"), "{}", f.why);
            }
            _ => panic!("the reader ends with the count rule's fault"),
        }
        // A blob on a reply (the scan's receipt), first in the conversation: refused all the same.
        let receipt = codec::encode(kind::SCAN, FF_REPLY, ID_SCAN, b"{}", &[1, 2, 3]);
        let (got, admitted) = read_with_budget(vec![receipt], 1 << 20);
        assert_eq!(admitted, 0);
        assert!(
            matches!(&got[0], Inbound::ReaderDone(Err(f)) if f.why.contains("one blob per conversation") && f.over_budget.is_none())
        );
    }

    use std::os::unix::net::UnixStream;

    /// A bound channel's halves and the child's end as a plain socket (what the spawned child holds).
    fn chan() -> (
        runecraft_ipc::channel::ChanTx,
        runecraft_ipc::channel::ChanRx,
        UnixStream,
    ) {
        let (p, c) = runecraft_ipc::channel::channel_pair().unwrap();
        let (tx, rx) = p
            .bind(&runecraft_ipc::process::testing::Unspawned(0))
            .unwrap();
        (
            tx,
            rx,
            UnixStream::from(runecraft_ipc::channel::unix::ChildEndExt::into_fd(c)),
        )
    }

    fn conversation<'a>(
        scan: &'a Scan,
        sock: &'a mut runecraft_ipc::channel::ChanTx,
    ) -> Conversation<'a> {
        Conversation {
            scan,
            child_id: "scan",
            sock,
            quiet: false,
            step: Step::Scanned,
            scan_sent: true,
            scanned: None,
            discovered: Vec::new(),
            fault: None,
            half_frame: false,
            protocol: summoner_proto::layout::ABI_VERSION,
        }
    }

    /// After a verified `hello`, the scan request this host writes offers form B
    /// (`descriptionsBlob: 1`), and a form B `failed` line then gives the verdict's line plus its `discovered`. The
    /// mutant `mutant_scan_no_offer` fails here (no offer written, and the form B line refused as not offered).
    #[test]
    fn the_scan_request_offers_form_b_and_a_form_b_failed_line_keeps_its_classes() {
        let scan = Scan::new(summoner_proto::codec::ScanFormat::Vst3, "/x/g.vst3").unwrap();
        let (mut ours, _rx, theirs) = chan();
        let mut conv = conversation(&scan, &mut ours);
        conv.step = Step::Hello;
        conv.scan_sent = false;
        let reply = {
            let mut msg = HelloReply::default();
            msg.pid = 1;
            msg.protocol = summoner_proto::layout::ABI_VERSION;
            msg.child_id = "scan".into();
            msg.host = "test".into();
            msg.formats = vec!["VST3".into()];
            msg.mach_slot0 = false;
            msg
        };
        let mut fs = Vec::new();
        codec::Decoder::new(Direction::ToHost)
            .feed(
                &codec::encode_msg(kind::HELLO, FF_REPLY, ID_HELLO, &reply, &[]),
                &mut |f| fs.push(f),
            )
            .unwrap();
        conv.frame(fs.remove(0));
        assert!(conv.fault.is_none(), "{:?}", conv.fault);
        assert!(conv.scan_sent);
        let mut buf = vec![0u8; 4096];
        let n = (&theirs).read(&mut buf).unwrap();
        let mut got = Vec::new();
        codec::Decoder::new(Direction::ToChild)
            .feed(&buf[..n], &mut |f| got.push(f))
            .unwrap();
        let sent: Scan = got[0].meta_as().unwrap();
        assert!(sent.offers_descriptions_blob(), "the request offers form B");
        // The receipt, then a form B failed line with two classes.
        conv.step = Step::Scanned;
        let d = |name: &str| {
            codec::ScanClass::Vst3({
                let mut msg = summoner_proto::plugin::Vst3Class::default();
                msg.path = "/x/g.vst3".into();
                msg.source = Some(summoner_proto::plugin::ScanSource::Factory);
                msg.class_id = "0123456789abcdef0123456789abcdef".into();
                msg.category = "Audio Module Class".into();
                msg.name = name.into();
                msg.instantiated = Some(true);
                msg
            })
        };
        let line = Scanned::new(
            summoner_proto::codec::ScanFormat::Vst3,
            "/x/g.vst3".into(),
            codec::ScannedOutcome::Failed {
                failure: ScanFailure::Process,
                detail: "x".into(),
            },
            vec![d("A"), d("B")],
        );
        let mut fs = Vec::new();
        codec::Decoder::new(Direction::ToHost)
            .feed(codec::encode_scanned(&line, true).bytes(), &mut |f| {
                fs.push(f)
            })
            .unwrap();
        conv.frame(fs.remove(0));
        assert!(conv.fault.is_none(), "{:?}", conv.fault);
        assert_eq!(conv.discovered.len(), 2);
        assert!(conv.scanned.as_ref().is_some_and(|s| s.classes.is_empty()));
    }

    /// Half a frame, then the end: the EOF mid-frame is no fault, on either
    /// side of the receipt, so the verdict table judges the end: a signal is `crashed {signal}`, an exit is its row. A
    /// frame the decoder refused stays a fault. The mutant `mutant_half_frame_struck` fails here.
    #[test]
    fn a_half_frame_is_judged_by_the_end() {
        let scan = Scan::new(summoner_proto::codec::ScanFormat::Vst3, "/x/g.vst3").unwrap();
        let (mut ours, _rx, _theirs) = chan();
        let half = || ReaderFault {
            half_frame: true,
            ..ReaderFault::other("EOF mid-frame".into())
        };
        let s = line();
        for step in [Step::Receipt, Step::Scanned, Step::Shutdown] {
            let mut conv = conversation(&scan, &mut ours);
            conv.step = step;
            conv.reader_fault(half());
            assert!(conv.fault.is_none() && conv.half_frame, "{step:?}");
            let sc = (step == Step::Shutdown).then_some(&s);
            let j = |e| judge(conv.fault.clone(), true, false, Some(e), sc);
            assert_eq!(
                j(ChildExit::Signaled(9)),
                crashed(Some(9), None),
                "{step:?}"
            );
            assert_eq!(j(ChildExit::Signaled(11)), crashed(Some(11), None));
            assert_eq!(
                j(ChildExit::Exited(75)),
                ScanEnd::Verdict(ScanResult::Timeout {
                    kind: TimeoutKind::Step
                })
            );
            assert_eq!(j(ChildExit::Exited(4)), ScanEnd::HostFault { exit_code: 4 });
            assert_eq!(j(ChildExit::Exited(3)), crashed(None, Some(3)));
            if sc.is_none() {
                assert_eq!(j(ChildExit::Exited(0)), crashed(None, Some(0)));
            }
        }
        let mut conv = conversation(&scan, &mut ours);
        conv.reader_fault(ReaderFault::other("a frame the decoder refused: x".into()));
        assert!(conv.fault.as_ref().is_some_and(|f| f.after_receipt) && !conv.half_frame);
    }

    /// The drain never waits past its bound (a sender alive, nothing sent: an escapee holding the socket), ends at once
    /// when every sender is gone, and writes nothing.
    #[test]
    fn the_drain_never_waits_past_its_bound() {
        let scan = Scan::new(summoner_proto::codec::ScanFormat::Vst3, "/x/g.vst3").unwrap();
        let (mut ours, _rx, theirs) = chan();
        let mut conv = conversation(&scan, &mut ours);
        let (tx, rx) = sync_channel::<Inbound>(INBOUND_BOUND);
        let t = Instant::now();
        let (took, timed_out) = conv.drain(&rx, Duration::from_millis(200));
        assert!(timed_out && took >= Duration::from_millis(200));
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(conv.quiet && !conv.write(kind::SHUTDOWN, ID_SHUTDOWN, &serde_json::json!({})));
        theirs.set_nonblocking(true).unwrap();
        assert!(
            (&theirs).read(&mut [0u8; 64]).is_err(),
            "nothing was written to the child"
        );
        drop(tx);
        let mut conv = conversation(&scan, &mut ours);
        let (took, timed_out) = conv.drain(&rx, Duration::from_secs(5));
        assert!(
            !timed_out,
            "ended at the disconnect, not the bound: {took:?}"
        );
    }

    #[test]
    fn what_fits_is_kept_whole() {
        let mut b = Bounded::new(1024, 256);
        let data: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        b.push(&data[..300]);
        b.push(&data[300..]);
        let c = b.finish(false);
        assert_eq!(c.bytes, data);
        assert!(!c.cut);
        assert_eq!(c.total, 1024);
    }

    /// Past the cap: the head, the marker (naming the bytes cut) and the latest bytes; never more than the cap, in
    /// any chunking.
    #[test]
    fn past_the_cap_the_middle_is_cut_with_a_marker() {
        let data: Vec<u8> = (0..10_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        for chunk in [1, 7, 300, 5000, 10_000] {
            let mut b = Bounded::new(1024, 256);
            for c in data.chunks(chunk) {
                b.push(c);
            }
            let c = b.finish(false);
            assert!(c.cut);
            assert_eq!(c.total, 10_000);
            assert!(c.bytes.len() <= 1024, "{}", c.bytes.len());
            assert_eq!(&c.bytes[..256], &data[..256], "the head");
            let keep = 1024 - 256 - MARKER_ROOM;
            assert!(c.bytes.ends_with(&data[data.len() - keep..]), "the tail");
            let m = marker(10_000 - 256 - keep as u64);
            assert_eq!(&c.bytes[256..256 + m.len()], m.as_bytes(), "chunk {chunk}");
        }
    }

    /// The reader thread: a pipe with more than 64 KiB written, then closed, gives the cap's worth with the marker
    /// and the last line intact; EOF ends it with `open: false`.
    #[test]
    fn the_reader_cuts_a_long_stderr_and_ends_at_eof() {
        let (r, mut w) = std::io::pipe().unwrap();
        let reaped = Arc::new(AtomicBool::new(false));
        let h = spawn_stderr_reader(r, reaped, "scan-test").unwrap();
        let line = [b'x'; 99];
        for _ in 0..1000 {
            w.write_all(&line).unwrap();
            w.write_all(b"\n").unwrap();
        }
        w.write_all(b"the last line\n").unwrap();
        drop(w);
        let c = h.join().unwrap();
        assert!(c.cut && !c.open);
        assert_eq!(c.total, 100_000 + 14);
        assert!(c.bytes.len() <= STDERR_CAP);
        assert!(c.text().ends_with("the last line\n"));
        assert!(c.text().contains("of the scan child's stderr cut here]"));
    }

    /// A writer that never closes (a process outside the group holding the pipe): once `reaped` is set the reader
    /// returns within the grace, marked `open`.
    #[test]
    fn a_pipe_held_open_is_taken_after_the_grace() {
        let (r, mut w) = std::io::pipe().unwrap();
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = reaped.clone();
        let h = std::thread::spawn(move || read_bounded(r, &flag, Duration::from_millis(200)));
        w.write_all(b"still here\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let t = Instant::now();
        reaped.store(true, Ordering::Release);
        let c = h.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(2));
        assert!(c.open);
        assert_eq!(c.text(), "still here\n");
        drop(w);
    }
}
