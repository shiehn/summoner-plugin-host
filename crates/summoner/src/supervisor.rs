//! The supervisor, reader and reaper threads.
//!
//! - the supervisor thread owns the child's lifecycle, the chain table, the generations, the wedge detector and the only
//!   socket writer. It runs a 250 ms tick through `recv_timeout` on its inbound channel; there is no event loop.
//! - the reader thread frames and validates the child's output ([`summoner_proto::codec`]) and forwards it.
//! - the reaper thread sits in a blocking `waitpid`.
//!
//! Control talks to the supervisor only through [`Bridge::send`] (a `try_send` on a bounded channel: full = BUSY,
//! retryable) and receives [`Event`]s on its own bounded channel; it never waits on the supervisor.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use crate::clock::Clock;
use crate::depth::{DepthChange, DepthController, DepthPolicy};
use runecraft_contracts::digest::StateHasher;
use runecraft_contracts::log::{AreaKey, Level, LogSink, NullSink, Record, Value};
use runecraft_ipc::bridge::{MachReceive, MachSend};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
// The bench seam and the tests write to a socket end by hand; the supervisor writes through its channel.
#[cfg(any(test, feature = "bench-seams"))]
use std::io::Write;
#[cfg(any(test, feature = "bench-seams"))]
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;
use summoner_proto::codec::{self, Direction, Frame, kind};
use summoner_proto::layout::*;
use summoner_proto::state_blob;
use summoner_rt::region::{ChildPage, Region};
use summoner_rt::rt::ChainLink;

/// One record through the supervisor's sink: written only when `cfg.log_area` enables `level` (nothing past the check
/// is evaluated otherwise), its `corr` the echo of the answer being handled.
macro_rules! rec {
    ($sup:expr, $level:expr, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {{
        let level: Level = $level;
        if $sup.cfg.log_area.enabled(level) {
            $sup.cfg.log.record(&Record {
                level,
                area: $sup.cfg.log_area,
                msg: std::borrow::Cow::Borrowed($msg),
                fields: vec![$((stringify!($k), Value::from($v))),*],
                corr: $sup.echo.as_deref(),
            });
        }
    }};
}

/// D6: control -> supervisor commands in flight.
pub const COMMAND_BOUND: usize = 1024;

/// A `param.list` page holds at most 256 parameters.
pub const PARAM_PAGE: u32 = 256;

/// The supervisor caches at most 16,384 parameters per chain (`chain.loaded`'s `paramCount` is clamped the
/// same way).
pub const PARAM_CAP: u32 = 16_384;

/// A parameter name is at most 1024 characters (the page's meta, at most `MAX_META`,
/// bounds the bytes).
pub const PARAM_NAME_CHARS: usize = summoner_plugin::params::NAME_CHARS;

/// A `param.list` entry as the host application takes it (V1, the child is untrusted): the name and label cut, `default` and
/// `value` clamped to [0, 1] (the page was refused when either is not finite). From a child below 3.7, every 3.7 field
/// is its default, whatever the child sent (never presence).
fn clamp_entry(p: codec::ParamEntry, v37: bool) -> codec::ParamEntry {
    let name = summoner_plugin::params::cut_chars(p.name, PARAM_NAME_CHARS);
    // `--cfg mutant_param_entry_unclamped` (never in a normal build): an older child's 3.7 fields pass through;
    // the unit test `a_pre_3_7_entry_keeps_only_its_id_and_name` fails.
    if !v37 && !cfg!(mutant_param_entry_unclamped) {
        return {
            let mut msg = codec::ParamEntry::default();
            msg.id = p.id;
            msg.name = name;
            msg
        };
    }
    {
        let mut msg = codec::ParamEntry::default();
        msg.id = p.id;
        msg.name = name;
        msg.plugin_id = p.plugin_id;
        msg.automatable = p.automatable;
        msg.default = p.default.clamp(0.0, 1.0);
        msg.value = p.value.clamp(0.0, 1.0);
        msg.step_count = p.step_count;
        msg.label =
            summoner_plugin::params::cut_chars(p.label, summoner_plugin::params::LABEL_CHARS);
        msg.order = p.order;
        msg
    }
}
/// D6: reader -> supervisor frames, and blob bytes, in flight. The frame bound is each reader's own (
/// 2026-11-01), enforced with the byte budget by [`BlobGate`]: every frame counts, a meta-only one included.
pub const READER_FRAMES: usize = 256;
/// D6's 256 MiB: the blob bytes the readers may hold between the socket and the supervisor, the frame the decoder is
/// filling included ([`BlobGate`]). Every frame's blob fits it (the codec's cap is 64 MiB), so a frame within the caps
/// always admits once the supervisor has taken the frames before it.
pub const READER_BLOB_BYTES: usize = 256 * 1024 * 1024;
const _: () = assert!(codec::MAX_BLOB <= READER_BLOB_BYTES);
/// D6: requests in flight to the child.
pub const IN_FLIGHT: usize = 64;
/// D7: respawn attempts before every chain stays silent.
pub const ATTEMPTS: u32 = 3;
/// Descriptors one child incarnation holds in the host application besides its chain loads' (T0, the host application's `FD_BUDGET`): the
/// socket and the reader's clone (2), and the child page's until HELLO is written (1). Each chain load's region
/// descriptor is held from `load_chain` until its frame is written; the frames wait behind [`IN_FLIGHT`] with no count
/// of their own yet (one per chain loading), so the budget counts [`IN_FLIGHT`] of them, an assumption to enforce
/// when the host application starts the bridge (T4).
pub const CHILD_FDS: usize = 3;
/// A `hello` reply not verified this long after the
/// `hello` was sent is a no-blame kill, with an attempt counted. Evaluated on the 250 ms tick, so the kill
/// lands at most one tick after it is due (as D7's thresholds).
pub const HELLO_DEADLINE: Duration = Duration::from_secs(2);

/// Until the incarnation's `hello` reply is verified, the only frame sent to it is `hello`. The mutation
/// check (`--cfg mutant_pre_hello_sends`, never in a normal build) restores the old pipelining (every chain's
/// `chain.load`, states and `chain.prepare` queued right behind `hello`, and `Load` / `SetState` sent at once), so
/// live_hello `nothing_but_hello_reaches_a_child_before_its_hello_is_verified` fails.
const WAIT_FOR_HELLO: bool = !cfg!(mutant_pre_hello_sends);
/// The protocol-major check of the live `hello` reply. The mutation check (`--cfg
/// mutant_major_unchecked`) removes it, so live_hello's major-mismatch test fails.
const CHECK_MAJOR: bool = !cfg!(mutant_major_unchecked);
/// A protocol-fault kill is never attributed. The mutation check (`--cfg mutant_protocol_blamed`)
/// attributes it by `inSlot` and the status record as before, so live_hello's no-blame tests fail.
const PROTOCOL_UNBLAMED: bool = !cfg!(mutant_protocol_blamed);
/// H-2: the hello deadline. The mutation check (`--cfg mutant_no_hello_deadline`) removes it, so live_hello
/// `a_hello_that_never_comes_is_killed_at_its_deadline_and_blames_nothing` fails.
const ENFORCE_HELLO_DEADLINE: bool = !cfg!(mutant_no_hello_deadline);
/// N4: a child that exits 2 before its `hello` is verified is a usage error, the
/// engine's: no blame, no respawn. `--cfg mutant_usage_exit_respawns` (never in a normal build) treats it as an
/// ordinary death again (the backoff loop); the host application's usage-exit test fails.
const USAGE_EXIT_TERMINAL: bool = !cfg!(mutant_usage_exit_respawns);
/// The child's usage exit (both children: an unknown flag, a missing value, a bad `--crash-log`, a bad `--step-ms`;
/// always before any byte on fd 3, the plugin-host child R6).
pub const EXIT_USAGE: i32 = 2;

/// The region header flags of a chain: a render set's are `FLAG_RENDER | FLAG_ACK_WAKE`, every
/// other chain's 0. The child refuses a load whose flags disagree with its role (the plugin-host child R1), so these two
/// always travel together, from the one place that writes them. `--cfg mutant_render_flags_zero` (never in a
/// normal build) writes 0 for every chain, as before; the host application's render tests fail.
pub fn region_flags(role: codec::Role) -> u32 {
    if role == codec::Role::Render && !cfg!(mutant_render_flags_zero) {
        FLAG_RENDER | FLAG_ACK_WAKE
    } else {
        0
    }
}

/// Every save point's burst budget (it applies at every save).
pub const BURST_BUDGET: Duration = Duration::from_millis(150);
/// A burst's byte budget, enforced as reads arrive (also the codec's blob cap).
pub const BURST_BYTES: u64 = 64 << 20;
/// A burst keeps at most this many reads in flight, so its memory is bounded by `BURST_BYTES` plus
/// this many blobs, never by the number of moved slots.
pub const BURST_WINDOW: usize = 2;

impl Command {
    /// A save point's burst with E4's budgets.
    pub fn save_burst(token: u64, remark: Vec<(u64, u32)>) -> Command {
        Command::SaveBurst {
            token,
            budget: BURST_BUDGET,
            bytes: BURST_BYTES,
            remark,
        }
    }
}

/// One read of a save burst.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Fresh {
    /// The chain's key.
    pub key: u64,
    /// The slot.
    pub slot: u32,
    /// The child's generation of the state read.
    pub child_gen: u64,
    /// The state blob.
    pub blob: Arc<[u8]>,
    /// The blob's SHA-256, lowercase hex, computed on the supervisor thread by [`Config::state_hasher`] (empty where
    /// the application hashes the blob itself).
    pub sha256_hex: String,
    /// Whether the slot's editor was open since its last completed read (the user moved it).
    pub editor_moved: bool,
}

impl Fresh {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        key: u64,
        slot: u32,
        child_gen: u64,
        blob: Arc<[u8]>,
        sha256_hex: String,
        editor_moved: bool,
    ) -> Fresh {
        Fresh {
            key,
            slot,
            child_gen,
            blob,
            sha256_hex,
            editor_moved,
        }
    }
}

/// One consistent read of the host application's level state, for a supervisor forwarding it to its child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct LevelState {
    /// How many times the levels were set so far (never moved by an expiry). Each supervisor keeps the last value it
    /// forwarded.
    pub generation: u64,
    /// The word in effect.
    pub levels: u64,
    /// The launch word, what the levels return to at the expiry.
    pub launch: u64,
    /// The time left of the expiry (ms, floored; 0 once due, until the application returns to the launch word); None:
    /// no expiry.
    pub expires_in_ms: Option<u64>,
}

impl LevelState {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        generation: u64,
        levels: u64,
        launch: u64,
        expires_in_ms: Option<u64>,
    ) -> LevelState {
        LevelState {
            generation,
            levels,
            launch,
            expires_in_ms,
        }
    }
}

/// A `log.set` sent to a child, and what the child answered: applied (the reply names the word sent), refused (an
/// error reply), or a different word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildLogSet {
    /// Sent; no answer yet.
    Sent,
    /// The child's reply names the word sent.
    Applied,
    /// The child answered with an error.
    Refused,
    /// The child's reply names a different word.
    Mismatch,
}

/// What the supervisor reads from the host application's diagnostic log at run time: the level state it forwards to
/// its child (`log.set`), where it counts each outcome, the child's log flags and environment, and the words of a
/// level word for its warnings. Called on the supervisor (or scan) thread, never on a realtime thread.
pub trait LevelSource: Send + Sync {
    /// The level state now.
    fn level_state(&self) -> LevelState;
    /// Count one `log.set` outcome.
    fn note_child_log_set(&self, outcome: ChildLogSet);
    /// The child's extra argv (its log folder); empty: the child gets no log folder, and no `log.set` is sent.
    fn child_args(&self) -> Vec<OsString>;
    /// The values of the child's two log variables, when the host application runs a log: its launch levels (the
    /// level grammar's `describe` of every area) and its salt. The supervisor writes them under the host's own names
    /// ([`host_child_env`]); `None`: the child gets neither.
    fn child_log_values(&self) -> Option<(OsString, OsString)>;
    /// A level word in words, for a record's field.
    fn describe(&self, word: u64) -> String;
}

impl std::fmt::Debug for dyn LevelSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("dyn LevelSource")
    }
}

/// The level word of a host with no diagnostic log: errors and warnings in every area.
const NO_LOG_LEVELS: u64 = 0x2222_2222_2222_2222;

/// The default [`LevelSource`]: a host with no diagnostic log. The levels never move (generation 0, the launch word),
/// the child gets no log folder (so no `log.set` is ever sent) and an unchanged environment, and nothing is counted.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoChildLog;

impl LevelSource for NoChildLog {
    fn level_state(&self) -> LevelState {
        LevelState {
            generation: 0,
            levels: NO_LOG_LEVELS,
            launch: NO_LOG_LEVELS,
            expires_in_ms: None,
        }
    }
    fn note_child_log_set(&self, _: ChildLogSet) {}
    fn child_args(&self) -> Vec<OsString> {
        Vec::new()
    }
    fn child_log_values(&self) -> Option<(OsString, OsString)> {
        None
    }
    fn describe(&self, word: u64) -> String {
        format!("{word:#018x}")
    }
}

/// The child's environment: `env` with the host's two log variables ([`summoner_proto::log_vocab::HOST_VOCAB`]'s
/// `env_levels` and `env_salt`) set from `levels` ([`LevelSource::child_log_values`]), or left as `env` has them when
/// the application runs no log. The host reads only these names, so the application's own variable names never reach
/// it. Used by both the live and the scan spawn.
pub fn host_child_env(
    levels: &dyn LevelSource,
    mut env: Vec<(OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    let host = &summoner_proto::log_vocab::HOST_VOCAB;
    // Its mutant (never in a normal build): the values are not written, so the host starts with no log.
    if cfg!(mutant_child_log_env_unwritten) {
        return env;
    }
    if let Some((lv, salt)) = levels.child_log_values() {
        env.retain(|(k, _)| k != host.env_levels && k != host.env_salt);
        env.push((host.env_levels.into(), lv));
        env.push((host.env_salt.into(), salt));
    }
    env
}

/// The host application's names for this crate's runtime output (and the scan supervisor's): the prefix of every
/// stderr line, and the stems of the threads' names. Data the application passes in [`Config::names`]
/// (and the scan's configuration); [`Names::NEUTRAL`] by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Names {
    /// Every stderr line starts `<stderr>: `.
    pub stderr: &'static str,
    /// The supervisor's threads: `<thread>-super`, `<thread>-read`, `<thread>-reap`.
    pub thread: &'static str,
    /// The scan supervisor's threads: `<scan_thread>-read`, `-wait`, `-err`, `-reap-late`.
    pub scan_thread: &'static str,
    /// The stem of route A's one-shot mach bootstrap name: `<mach_service>.<our pid>.<nonce hex>` (macOS).
    pub mach_service: &'static str,
}

impl Names {
    /// The crate's own words.
    pub const NEUTRAL: Names = Names {
        stderr: "summoner",
        thread: "summoner",
        scan_thread: "summoner-scan",
        mach_service: "summoner",
    };

    /// These names with `stderr` as every stderr line's prefix.
    pub const fn with_stderr(self, stderr: &'static str) -> Names {
        Names { stderr, ..self }
    }

    /// These names with `thread` as the supervisor threads' stem.
    pub const fn with_thread(self, thread: &'static str) -> Names {
        Names { thread, ..self }
    }

    /// These names with `scan_thread` as the scan supervisor threads' stem.
    pub const fn with_scan_thread(self, scan_thread: &'static str) -> Names {
        Names {
            scan_thread,
            ..self
        }
    }

    /// These names with `mach_service` as route A's bootstrap name stem.
    pub const fn with_mach_service(self, mach_service: &'static str) -> Names {
        Names {
            mach_service,
            ..self
        }
    }

    /// Route A's bootstrap name for this process and `nonce`.
    pub fn route_a_service(&self, pid: u32, nonce: u64) -> String {
        format!("{}.{pid}.{nonce:x}", self.mach_service)
    }
}

impl Default for Names {
    fn default() -> Names {
        Names::NEUTRAL
    }
}

/// The flag that names a child's log folder (its value the folder).
pub const CHILD_LOG_DIR_FLAG: &str = "--log-dir";

/// The default records' area: a private word with every area off, so a supervisor nobody gave a log writes nothing.
static NO_LOG_WORD: AtomicU64 = AtomicU64::new(0);

/// [`Config::log_area`]'s default: every level off.
pub const NO_LOG_AREA: AreaKey = AreaKey::new(&NO_LOG_WORD, 0);

/// the `log.set` to send for the level state `st`: the word in effect and the time left; at 0 ms left (the
/// expiry is due) the launch word the host application is about to return to, with no expiry. `at_hello`: a fresh child already
/// runs its launch levels, so nothing is sent when the state is (or is about to be) the launch word.
pub fn log_set_for(st: &LevelState, at_hello: bool) -> Option<codec::LogSet> {
    let m = match st.expires_in_ms {
        Some(0) => codec::LogSet::new(st.launch, None),
        e => codec::LogSet::new(st.levels, e),
    };
    let resend = !cfg!(mutant_log_set_no_hello_resync) && m.levels != st.launch;
    (!at_hello || resend).then_some(m)
}

/// whether a slot's plugin is an Audio Unit.
pub fn is_au_slot(r: Option<&codec::PluginRef>) -> bool {
    r.is_some_and(|r| r.format() == Some(codec::PluginFormat::Au))
}

/// The live child's argv (N3): the configured args, then `--crash-log <dir>` when a folder is configured and passes
/// [`crash_log_for_spawn`] now.
pub fn spawn_args(cfg: &Config) -> Vec<OsString> {
    let mut a = cfg.args.clone();
    if let Some(d) = cfg
        .crash_log
        .as_deref()
        .and_then(|d| crash_log_for_spawn(d, cfg.names.stderr))
    {
        a.push("--crash-log".into());
        a.push(d.into_os_string());
    }
    a
}

/// The crash-log folder is checked again before EACH spawn, as the child will check it
/// (`resolve_explicit_dir`: absolute, canonical, existing, no link), so a folder deleted after launch never turns a
/// respawn into a usage exit (permanent silence). A missing folder is recreated (the folder is the app's; recreating
/// it is what the app would do), loudly; one that still fails is not passed, loudly, and the child keeps its own
/// fallback (the host application then reads no crash log from it: a lost report, never a silenced host). Both children and the
/// scan use it.
pub fn crash_log_for_spawn(dir: &std::path::Path, stderr_tag: &str) -> Option<PathBuf> {
    if let Ok((_, p)) = runecraft_ipc::process::unix::resolve_explicit_dir(dir) {
        return Some(p);
    }
    let made = std::fs::create_dir_all(dir);
    match runecraft_ipc::process::unix::resolve_explicit_dir(dir) {
        Ok((_, p)) => {
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: WARNING: the crash-log folder {} was gone and was recreated before the spawn",
                dir.display()
            );
            Some(p)
        }
        Err(why) => {
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: WARNING: the crash-log folder {} cannot be passed ({why}; recreate: {made:?}): the child \
                 is spawned without --crash-log, and its crash reports are not read",
                dir.display()
            );
            None
        }
    }
}

/// The live `hello` reply's `protocol` is compatible when its major (`protocol >> 16`) is the host application's
/// ([`ABI_VERSION`]); a different minor is additive (D20) and accepted. A reply with no `protocol` decodes as 0
/// (`HelloReply` is `#[serde(default)]`), so it is a mismatch.
pub fn hello_compatible(child_protocol: u32) -> bool {
    !CHECK_MAJOR || child_protocol >> 16 == ABI_VERSION >> 16
}

/// Which mach route the supervisor offers (tests force route A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RoutePref {
    /// Route B: the child sends its channel hello to slot 0's port (the default).
    B,
    /// Route A only: a one-shot bootstrap name (tests).
    ForceA,
}

#[derive(Clone, Debug)]
/// How the supervisor spawns and watches its child: the program, its arguments and environment, the clock, the
/// timers and the policies. [`Config::new`] gives the ADR's constants.
#[non_exhaustive]
pub struct Config {
    /// The child's executable.
    pub program: PathBuf,
    /// Extra arguments, after the ones the supervisor adds ([`spawn_args`]).
    pub args: Vec<OsString>,
    /// N3: the crash log's folder, passed to every spawn as `--crash-log <dir>`
    /// ([`spawn_args`]). Production sets the folder its own `--crash-log` guard validated (the app's); a lab host application the
    /// `Library/Logs` folder the application's crash log resolved in the isolated home. `None` (tests): no flag,
    /// and the child uses its own fallback. The child exits 2 on a folder it refuses (one gone after launch): N4's
    /// usage exit ([`DownReason::Usage`]).
    pub crash_log: Option<PathBuf>,
    /// The child's environment (the log variables are added from [`Config::levels`]).
    pub env: Vec<(OsString, OsString)>,
    /// The id the `hello` names (`live` by default).
    pub child_id: String,
    /// The supervisor's clock (the system clock by default; tests pass a manual one).
    pub clock: Arc<dyn Clock>,
    /// The supervisor's timer tick (250 ms).
    pub tick: Duration,
    /// The respawn backoff after the first, second and third death (2, 4 and 8 s).
    pub backoff: [Duration; 3],
    /// How long a respawned child must stay up, its states confirmed, before the attempt counter resets (60 s).
    pub forgive_after: Duration,
    /// A live chain with work pending and no progress for this long is pinged (2 s).
    pub wedge_suspect: Duration,
    /// A pinged chain that has not answered after this long is wedged: the child is killed (2 s).
    pub wedge_verdict: Duration,
    /// The ping threshold of a render chain (fixed at 30 s, never the environment's).
    pub render_suspect: Duration,
    /// The control socket's send timeout; a send that stalls past it is a wedged child (2 s).
    pub send_timeout: Duration,
    /// [`HELLO_DEADLINE`] (H-2).
    pub hello_deadline: Duration,
    /// The generations poll's period while an editor is open (2 s).
    pub poll_interval: Duration,
    /// How long the generations poll goes on after the last editor closes (2 s).
    pub poll_tail: Duration,
    /// How often the adaptive pipeline depth is judged (1 s).
    pub depth_cadence: Duration,
    /// The adaptive pipeline depth's thresholds ([`DepthPolicy`]).
    pub depth_policy: DepthPolicy,
    /// Which mach route the supervisor offers ([`RoutePref`]).
    pub route: RoutePref,
    /// The sample rate of every region created (see [`Command::SetSampleRate`]).
    pub sample_rate: f64,
    /// The bound of the caller's event channel (4096); the supervisor itself does not read it.
    pub event_bound: usize,
    /// D6's reader -> supervisor blob budget, [`READER_BLOB_BYTES`]. A parameter for tests only (a small budget): a
    /// budget below the codec's 64 MiB blob cap makes a frame whose blob passes the whole budget a protocol fault
    /// (`ReaderEnd::Refused`, `NotAdmitted`) rather than a reader waiting forever; at [`READER_BLOB_BYTES`] no frame
    /// within the caps can be one.
    pub reader_blob_budget: usize,
    /// Where the supervisor's records go ([`NullSink`] by default).
    pub log: Arc<dyn LogSink>,
    /// The records' area: each is written only when this key enables its level ([`NO_LOG_AREA`] by default: none).
    pub log_area: AreaKey,
    /// The host's level state and the child's log flags ([`NoChildLog`] by default).
    pub levels: Arc<dyn LevelSource>,
    /// A state blob's SHA-256, computed on the supervisor thread for each read ([`Fresh`]'s hash).
    /// Always the application's choice ([`Config::new`] takes it): there is no default, so a host that forgets it
    /// cannot silently report equal empty hashes. [`NoStateHash`](runecraft_contracts::digest::NoStateHash) is the
    /// explicit "the application hashes the blob itself" (every hash empty).
    pub state_hasher: Arc<dyn StateHasher>,
    /// The words of the supervisor's stderr lines and thread names ([`Names::NEUTRAL`] by default).
    pub names: Names,
}

impl Config {
    /// The protocol's constants, the current environment, the system clock, and the application's state hasher (required:
    /// the hash a [`Fresh`] read carries is `state_hasher`'s, never a silent default).
    ///
    /// ```compile_fail
    /// // A configuration without a state hasher does not build.
    /// let _ = summoner::supervisor::Config::new("/x".into(), 48_000.0);
    /// ```
    ///
    /// ```
    /// use std::sync::Arc;
    /// let c = summoner::supervisor::Config::new(
    ///     "/x".into(),
    ///     48_000.0,
    ///     Arc::new(runecraft_contracts::digest::NoStateHash),
    /// );
    /// assert_eq!(c.state_hasher.sha256_hex(b"abc"), "");
    /// ```
    pub fn new(program: PathBuf, sample_rate: f64, state_hasher: Arc<dyn StateHasher>) -> Config {
        // `--cfg mutant_config_hasher_dropped` (never in a normal build): the given hasher is dropped for the
        // empty one (the fail-open default this argument replaced); tests `config_keeps_the_given_state_hasher`
        // fails.
        #[cfg(mutant_config_hasher_dropped)]
        let state_hasher: Arc<dyn StateHasher> = {
            drop(state_hasher);
            Arc::new(runecraft_contracts::digest::NoStateHash)
        };
        Config {
            program,
            args: Vec::new(),
            crash_log: None,
            env: std::env::vars_os().collect(),
            child_id: "live".into(),
            clock: crate::clock::SystemClock::new(),
            tick: Duration::from_millis(250),
            backoff: [
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ],
            forgive_after: Duration::from_secs(60),
            wedge_suspect: Duration::from_secs(2),
            wedge_verdict: Duration::from_secs(2),
            // D9: fixed at 30 s, never the environment's, and never under the worker's budget (which
            // is clamped to it): the worker fails a slow block before the supervisor could judge it.
            render_suspect: summoner_rt::render::RENDER_SUSPECT,
            send_timeout: Duration::from_secs(2),
            hello_deadline: HELLO_DEADLINE,
            poll_interval: Duration::from_secs(2),
            poll_tail: Duration::from_secs(2),
            depth_cadence: Duration::from_secs(1),
            depth_policy: DepthPolicy::default(),
            route: RoutePref::B,
            sample_rate,
            event_bound: 4096,
            reader_blob_budget: READER_BLOB_BYTES,
            log: Arc::new(NullSink),
            log_area: NO_LOG_AREA,
            levels: Arc::new(NoChildLog),
            state_hasher,
            names: Names::NEUTRAL,
        }
    }
}

/// What control asks a chain to be.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ChainSpec {
    /// The region geometry (block frames and channel counts).
    pub geometry: Geometry,
    /// Live or render.
    pub role: codec::Role,
    /// The ring group (0..=3) whose chain thread runs the chain in the child.
    pub group: u8,
    /// The pipeline depth the chain starts at.
    pub depth: u8,
    /// Whether the chain is the monitor chain (its misses pass the dry mix).
    pub monitor: bool,
    /// One plugin per slot, in chain order (≤ 32); `None`: an empty (quarantined) slot.
    pub slots: Vec<Option<codec::PluginRef>>,
    /// Per slot, whether it is enabled (empty, or shorter than `slots`: the rest enabled). A child
    /// below protocol 3.2 gets a disabled slot empty (the guard (B)).
    pub enabled: Vec<bool>,
    /// The plugin parameter id (`slot << 24 | index`) of each model parameter of the chain.
    pub param_ids: Arc<[u32]>,
}

impl ChainSpec {
    /// A chain of `slots` at `geometry` and `role`: ring group 0, depth 1, not the monitor chain, every slot enabled, no
    /// model parameters. The struct is `#[non_exhaustive]`: set the other fields on the value.
    pub fn new(
        geometry: Geometry,
        role: codec::Role,
        slots: Vec<Option<codec::PluginRef>>,
    ) -> ChainSpec {
        ChainSpec {
            geometry,
            role,
            group: 0,
            depth: 1,
            monitor: false,
            slots,
            enabled: Vec::new(),
            param_ids: Arc::from(Vec::new()),
        }
    }
}

/// A slot's state as the model references it (the package blob, D7 re-apply source). The blob is a framed state blob, format v1 (D21):
/// one that fails [`state_blob::check`] is never sent ([`Event::StateRefused`]).
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SlotState {
    /// The slot, in chain order.
    pub slot: u32,
    /// The model's generation of the state.
    pub generation: u64,
    /// The framed state blob.
    pub blob: Arc<[u8]>,
}

impl SlotState {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(slot: u32, generation: u64, blob: Arc<[u8]>) -> SlotState {
        SlotState {
            slot,
            generation,
            blob,
        }
    }
}

#[derive(Debug)]
/// What control sends the supervisor ([`Bridge::send`]).
#[non_exhaustive]
pub enum Command {
    /// Load and prepare a chain under a key.
    Load {
        /// The chain's key.
        key: u64,
        /// What the chain is.
        spec: ChainSpec,
        /// The states to apply after the load, before the prepare.
        states: Vec<SlotState>,
    },
    /// Unload a chain.
    Unload {
        /// The chain's key.
        key: u64,
    },
    /// The rate of every region created from now on (a device rate change; the chains already
    /// loaded keep theirs until control reloads them, make before break).
    SetSampleRate(f64),
    /// `slot.setEnabled` to a 3.2 child (recorded in the chain's spec for a reload either way;
    /// to a 3.1 child nothing is sent: the caller reloads the chain, make before break).
    SetEnabled {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The slot's new enabled flag.
        enabled: bool,
    },
    /// Apply a state to a live slot.
    SetState {
        /// The chain's key.
        key: u64,
        /// The state to apply.
        state: SlotState,
    },
    /// Park a chain: it stays loaded, its thread stops running it.
    Park {
        /// The chain's key.
        key: u64,
    },
    /// Unpark a chain onto a ring group.
    Unpark {
        /// The chain's key.
        key: u64,
        /// The ring group (0..=3) that runs it from now on.
        group: u8,
    },
    /// Read a slot's state (answered by [`Event::StateRead`] or a refusal).
    GetState {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
    },
    /// One `param.list` page of a slot, `count` (≤ [`PARAM_PAGE`]) parameters from `offset`,
    /// answered by [`Event::ParamList`]. MainThread in the child (it waits behind a running plugin op).
    ParamList {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The first parameter of the page.
        offset: u32,
        /// How many parameters the page asks for.
        count: u32,
    },
    /// (protocol 3.8): a render set's slots released and prepared again on their kept instances (the render
    /// warm-up's clean start); answered by [`Event::PreparedFresh`]. To a child below 3.8, or for a key that is not a
    /// render set the supervisor hosts, it is never sent: answered `ok: false` at once.
    PrepareFresh {
        /// The chain's key.
        key: u64,
    },
    /// (protocol 3.7): set one parameter of a live slot by the plugin's own id, normalized; answered by
    /// [`Event::ParamSet`]. To a child below 3.7 it is never sent: answered `ok: false` at once.
    ParamSet {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The plugin's own parameter id.
        plugin_id: u32,
        /// The normalized value, 0 to 1.
        value: f32,
    },
    /// D17: open the slot's editor window in the child. `guard_reset` asks for the editor-reset guard. A render
    /// chain never gets
    /// `editor.open` (`editor-windows.md`): the command is dropped and counted
    /// (`editor_opens_refused`).
    EditorOpen {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// Arm the editor-reset guard.
        guard_reset: bool,
    },
    /// Close a slot's editor window, or every window of the chain.
    EditorClose {
        /// The chain's key.
        key: u64,
        /// The slot; `None`: every slot of the chain.
        slot: Option<u32>,
    },
    /// C3: read the moved slots within the burst deadline, answer with [`Event::Burst`]:
    /// the burst carries its own time `budget` (the supervisor requests no read past it) and its `bytes` budget (once
    /// the blobs received reach it, no more reads are requested: the rest stay moved, listed stale); `remark` are the
    /// slots control could not [`Command::Remark`] on its own (a full channel), read again by this burst. A second
    /// `SaveBurst` supersedes a running one: the first is answered `superseded`, its unanswered slots carried over.
    SaveBurst {
        /// The burst's token, echoed in its [`Event::Burst`].
        token: u64,
        /// The time budget: no read is requested past it.
        budget: Duration,
        /// The byte budget: once the blobs received reach it, no more reads are requested.
        bytes: u64,
        /// (key, slot) pairs to read again whatever their generations say.
        remark: Vec<(u64, u32)>,
    },
    /// A slot whose capture control skipped (a full writer, an in-flight set, a degraded read, the
    /// byte budget, a late answer): the next burst reads it again, whatever its generations say.
    Remark {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
    },
    /// D8: hand a workgroup port to the child (the lab passes a stand-in port).
    AttachWorkgroup {
        /// The workgroup port.
        port: MachSend,
    },
    /// C10: a loop boundary or a transport stop; pending depth lowerings land.
    Boundary,
    /// `plugin.restartHost`: clears quarantine and the attempt counter, restarts the child.
    RestartHost,
    /// Re-evaluate timers now (tests with a manual clock).
    Tick,
    /// Answered once this command and the tick after it have run (tests: a barrier with a manual clock; a
    /// rendezvous channel also holds the supervisor still).
    Barrier(SyncSender<()>),
    /// Shut the child down and end the supervisor.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Why a chain (or the child) went down.
#[non_exhaustive]
pub enum DownReason {
    /// The child died.
    Crash,
    /// A chain stopped making progress and did not answer its ping.
    Wedge,
    /// A send to the child stalled past the send timeout.
    Stall,
    /// The supervisor killed the child for a protocol
    /// fault (a frame `check_to_core` refused, a reply to nothing, a frame the reader's decoder refused, descriptors
    /// from the child, a `hello` reply that does not decode or did not come within [`HELLO_DEADLINE`]). Never
    /// attributed; the respawn is armed and an attempt counted, as for any death. Never `Crash`.
    Protocol,
    /// The `hello` reply's protocol major is not the host application's. Never attributed, and no respawn: the
    /// supervisor goes straight to the exhausted state until `plugin.restartHost` or a reopen.
    Incompatible {
        /// The protocol the child's `hello` named (`major << 16 | minor`).
        child_protocol: u32,
        /// The protocol this supervisor speaks.
        core_protocol: u32,
    },
    /// N4: the child exited [`EXIT_USAGE`] before its `hello` was verified: the host application spawned it wrongly (a
    /// bad flag, a crash-log folder gone after launch). No plugin code ran: never attributed, and no respawn (the same
    /// argv would fail the same way): straight to the exhausted state until `plugin.restartHost` or a reopen.
    Usage,
}

impl DownReason {
    /// The reason word of `event.pluginHostRestarted {reason}` (D7, C7: `crash|wedge`). A send stall is a wedged
    /// child (D6's bounds table). `protocol` (H-5) and `incompatible` are [P] names, beside `crash` and `wedge`, for
    /// Acceptance at L3 (as C7's name).
    pub fn wire_word(self) -> &'static str {
        match self {
            DownReason::Crash => "crash",
            DownReason::Wedge | DownReason::Stall => "wedge",
            DownReason::Protocol => "protocol",
            DownReason::Incompatible { .. } => "incompatible",
            DownReason::Usage => "usage",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
/// A slot of a chain that was live when the child restarted.
#[non_exhaustive]
pub struct RestartedSlot {
    /// The chain's key.
    pub key: u64,
    /// The slot, in chain order.
    pub slot: u32,
    /// The slot's plugin, by name.
    pub plugin: String,
    /// How long the slot's state had gone unread (ms) when the child died.
    pub unread_ms: u64,
    /// Whether the slot's editor window was open.
    pub editor_was_open: bool,
}

impl RestartedSlot {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        key: u64,
        slot: u32,
        plugin: String,
        unread_ms: u64,
        editor_was_open: bool,
    ) -> RestartedSlot {
        RestartedSlot {
            key,
            slot,
            plugin,
            unread_ms,
            editor_was_open,
        }
    }
}

/// One slot of a ready chain as the host application holds it (D6 `chain.loaded`, clamped; D7 quarantine enforced).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadySlot {
    /// Whether the slot loaded.
    pub ok: bool,
    /// Whether the slot is quarantined (an empty pass-through).
    pub quarantined: bool,
    /// The slot's latency, in frames.
    pub latency: u32,
    /// The slot's parameter count.
    pub param_count: u32,
    /// Protocol 3.4: `SlotInfo.hasEditor` (true from a 3.3 child); false for a slot that is not loaded or is
    /// quarantined.
    pub has_editor: bool,
    /// *(protocol 3.9)* The slot's notice codes from the child (`SlotInfo.notices`, replaced by
    /// `chain.prepared`'s `slotNotices`), clamped ([`codec::slot_notices_clamped`]); none for a slot that is not
    /// loaded or is quarantined.
    pub notices: Vec<String>,
}

impl ReadySlot {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        ok: bool,
        quarantined: bool,
        latency: u32,
        param_count: u32,
        has_editor: bool,
        notices: Vec<String>,
    ) -> ReadySlot {
        ReadySlot {
            ok,
            quarantined,
            latency,
            param_count,
            has_editor,
            notices,
        }
    }
}

/// A child's slot notices as the supervisor keeps them. `--cfg mutant_notices_dropped` (never in
/// a normal build) drops them all; the bridge's and the host application's notice tests fail.
fn kept_notices(v: &[String]) -> Vec<String> {
    if cfg!(mutant_notices_dropped) {
        return Vec::new();
    }
    codec::slot_notices_clamped(v)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// How the child's audio threads joined the device's workgroup.
#[non_exhaustive]
pub enum WorkgroupRoute {
    /// No route yet.
    None,
    /// Route B: the port came with the child's channel hello.
    B,
    /// Route A: the child looked up a bootstrap name.
    A,
    /// The child could not join.
    NoJoin,
}

#[derive(Clone, Debug)]
/// What the supervisor tells control.
#[non_exhaustive]
pub enum Event {
    /// A child is up: its `hello` was verified.
    ChildUp {
        /// The child's pid.
        pid: u32,
        /// The incarnation (one per spawn).
        incarnation: u64,
        /// The workgroup route it took.
        route: WorkgroupRoute,
        /// The `hello` reply's `host` (the plugin host sends `"native"`, the test double `"fake"`).
        host: String,
    },
    /// The chain is loaded and prepared (`chain.prepared`): control (re)compiles its Bridge node.
    /// `slots` is per slot, in chain order: a quarantined slot keeps its number, as an empty pass-through with
    /// latency 0 and paramCount 0 (D7).
    ChainReady {
        /// The chain's key.
        key: u64,
        /// The chain's link for the realtime graph.
        link: Arc<ChainLink>,
        /// The slots' plugin names.
        names: Vec<String>,
        /// Per slot, in chain order.
        slots: Vec<ReadySlot>,
        /// The child incarnation that answered.
        incarnation: u64,
    },
    /// changed whole notice sets after readiness, with no link installation side effects.
    SlotNoticesChanged {
        /// The chain's key.
        key: u64,
        /// The child incarnation that answered.
        incarnation: u64,
        /// Per slot, in chain order, with the new notice sets.
        slots: Vec<ReadySlot>,
    },
    /// The chain's child died or the chain wedged: control recompiles it as `Silent` (D7).
    ChainDown {
        /// The chain's key.
        key: u64,
        /// Why.
        reason: DownReason,
    },
    /// A state's application finished (`state.applied`).
    StateApplied {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The generation of the state that was set.
        generation: u64,
        /// Whether the child applied it.
        ok: bool,
        /// The child's own generation of the slot when the applied state took effect
        /// (the host application's capture baseline). `None` from a 3.2 child.
        child_generation: Option<u64>,
        /// `Some` on a **partial** vst3 set (`ok: false`, yet the component chunk, say, reached the plugin):
        /// the plugin changed, so the slot is also reported moved ([`Event::Moved`]) and the next burst reads it;
        /// counted in `state_partial_sets`.
        applied: Option<codec::Applied>,
    },
    /// A slot's state was read (`state.get` or `editor.closed`).
    StateRead {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The child's generation of the slot when read.
        child_gen: u64,
        /// The framed state blob.
        blob: Arc<[u8]>,
        /// The child incarnation that answered (a capture names its chain's live one).
        incarnation: u64,
        /// The blob's SHA-256, lowercase hex, computed here on the supervisor thread (never on
        /// control); empty where the platform digest is absent (control hashes then).
        sha: String,
        /// The slot's editor was open since its last completed read (the user moved it).
        user: bool,
    },
    /// The child answered `state.get` `{ok: false, reason: "tooLarge", bytes}`. The
    /// slot keeps its stored state and is not left moved (read again only on its next `Moved`).
    StateTooLarge {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The child's generation of the slot.
        child_gen: u64,
        /// The size the child measured.
        bytes: u64,
    },
    /// A `param.list` page, as the child answered it, clamped (the child is untrusted): `total`
    /// ≤ [`PARAM_CAP`] (the per-chain cache bound), at most the `count` asked for (≤ [`PARAM_PAGE`]), each name cut
    /// to [`PARAM_NAME_CHARS`] characters and each label to `params::LABEL_CHARS`. `ids` are the child's, unjudged
    /// (`slot << 24 | index` by D10; a lying child's are its own, V1).: from a child below 3.7 the 3.7 fields
    /// are their defaults whatever it sent (`automatable` false). `ok` false: the child answered an error, a reply
    /// that is not a page, or (3.7) a `default` or `value` that is not finite.
    ParamList {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The page's first parameter.
        offset: u32,
        /// Whether the child answered a page.
        ok: bool,
        /// The slot's parameter count, clamped.
        total: u32,
        /// The page.
        params: Vec<codec::ParamEntry>,
    },
    /// (protocol 3.8): `chain.prepareFresh`'s completion. `ok` false: the child refused it (`error`), or it was
    /// never sent (a child below 3.8, a key that is not a hosted render set). `ms` is the child's time for it, `latency`
    /// the chain's latency after it.
    PreparedFresh {
        /// Per slot, in chain order, after the prepare.
        slots: Vec<ReadySlot>,
        /// Whether the child's answer carried the notice sets.
        notices_known: bool,
        /// The chain's key.
        key: u64,
        /// Whether the child prepared the set again.
        ok: bool,
        /// The child's time for it (ms).
        ms: f64,
        /// The chain's latency after it, in frames.
        latency: u32,
        /// The child's error, when it refused.
        error: Option<String>,
    },
    /// (protocol 3.7): `param.set`'s answer. `ok` false: an error reply, a reply that is not one, a non-finite
    /// read-back, or a child below 3.7 (never sent). `value` is the read-back, clamped to [0, 1].
    ParamSet {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The plugin's own parameter id.
        plugin_id: u32,
        /// Whether the set took.
        ok: bool,
        /// The read-back, clamped to [0, 1].
        value: f32,
    },
    /// The generations poll found these slots moved (control marks them dirty); also a read outside a burst
    /// whose generation is above the last one seen, just before its `StateRead` (or `StateRefused`).
    Moved {
        /// (key, slot, child generation) of each moved slot.
        slots: Vec<(u64, u32, u64)>,
    },
    /// A save burst's answer. `fresh`: (key, slot, child gen, blob) read within the budget. `stale`: moved slots not
    /// read (no answer in time, refused, past the byte budget, a 3.2 child's), still moved. `too_large`: (key, slot,
    /// bytes) answered `tooLarge` (not moved any more). `superseded`: a later `SaveBurst` replaced this one (its
    /// unanswered slots went to the later one, and are not in `stale`). `incarnation`: the child that answered.
    Burst {
        /// The token of the burst answered.
        token: u64,
        /// The reads within the budget.
        fresh: Vec<Fresh>,
        /// (key, slot) of each moved slot not read.
        stale: Vec<(u64, u32)>,
        /// (key, slot, bytes) of each slot answered `tooLarge`.
        too_large: Vec<(u64, u32, u64)>,
        /// Whether a later burst replaced this one.
        superseded: bool,
        /// The child incarnation that answered.
        incarnation: u64,
    },
    /// A slot was quarantined (two consecutive deaths, or an abandoned take).
    Quarantined {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The slot's plugin, by name.
        plugin: String,
        /// The operation the slot was in.
        op: String,
    },
    /// D17: the child closed the slot's editor window and said so (`editor.closed`: its close button, or
    /// `editor.close`). Emitted after the close's state read ([`Event::StateRead`], or the burst's). A chain unload
    /// closes its windows with NO `editor.closed` (the child's `close_windows`): see [`Event::EditorGone`].
    EditorClosed {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
    },
    /// the supervisor unloaded a chain itself (a quarantine reload, `chain.unload` then
    /// `chain.load` under the same key) while the slot's window was open. The child closes that window with no
    /// `editor.closed`; this says it is gone. Emitted before the unload is sent.
    EditorGone {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
    },
    /// Protocol 4.1, Windows only: a host-embedding child opened its editor in a container window for the host to
    /// parent (the `editor.open` receipt's `editorWindow`). Never on unix: a macOS child keeps its window in-child.
    ///
    /// **The window is the child's word, and the child is untrusted**: before it parents the window
    /// (`SetParent`), the application must check that the window belongs to the child that sent it
    /// (`GetWindowThreadProcessId(hwnd) == ` the pid of `incarnation`'s child) and that this incarnation is still
    /// the live one; a window of any other process, or of an older incarnation, is refused.
    EditorWindow {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The container window.
        window: codec::EditorWindow,
        /// The child incarnation whose receipt named the window ([`Event::ChildUp`]'s).
        incarnation: u64,
    },
    /// D17: `editor.availability`: the slot has no editor the child can show (no view, a render chain, the
    /// editor dylib missing); no window was opened. The slot's `hasEditor` is false from now on.
    EditorUnavailable {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
    },
    /// The child was restarted after a death.
    HostRestarted {
        /// Why it died.
        reason: DownReason,
        /// The slots that were live.
        slots: Vec<RestartedSlot>,
    },
    /// Three attempts failed: every chain stays silent until reopen or `plugin.restartHost`.
    Exhausted,
    /// A chain was parked.
    Parked {
        /// The chain's key.
        key: u64,
    },
    /// A chain was unparked.
    Unparked {
        /// The chain's key.
        key: u64,
        /// The ring group that runs it now.
        group: u8,
    },
    /// The chain's pipeline depth changed.
    DepthChange {
        /// The chain's key.
        key: u64,
        /// The chain's link at its new depth.
        link: Arc<ChainLink>,
        /// Raised or lowered.
        change: DepthChange,
    },
    /// The chain's latency changed (`latency.changed`).
    LatencyChanged {
        /// The chain's key.
        key: u64,
        /// The chain's link with its new latency.
        link: Arc<ChainLink>,
    },
    /// A workgroup attach finished.
    WorkgroupAttached {
        /// The attach's token.
        token: u64,
        /// Whether the child took the port.
        ok: bool,
        /// Whether its threads joined the workgroup.
        joined: bool,
    },
    /// A state blob failed the frame check (`stateFormatRefused`).
    /// - [`StateSource::Package`]: the model's blob was never sent; the slot loads with no state applied, and
    ///   `project.open` names it in a warning (the blob stays in the package, never rewritten). `generation` is the
    ///   model's.
    /// - [`StateSource::Child`]: a `state.get` or `editor.closed` blob was not stored; the slot keeps its last known
    ///   blob, stale (a save burst lists it in `stale`). `generation` is the child's.
    StateRefused {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The model's generation (package) or the child's (child).
        generation: u64,
        /// Where the blob came from.
        source: StateSource,
        /// Why it was refused.
        reason: state_blob::Refusal,
    },
    /// A slot's `restartComponent(kLatencyChanged)` cycle ended (`slot.reactivated`, clamped). The
    /// last three are the supervisor's per-(chain, slot) counts, saturating; `getState` surfaces a notice for a slot
    /// whose `total_bypassed_blocks` > 0 (its wire name is settled with 0001's catalogue at L3).
    SlotReactivated {
        /// The chain's key.
        key: u64,
        /// The slot, in chain order.
        slot: u32,
        /// The slot's latency after the cycle, in frames.
        latency: u32,
        /// The blocks the slot was bypassed for during the cycle.
        bypassed_blocks: u32,
        /// The cycle's length (ns).
        cycle_ns: u64,
        /// Whether the take was forced.
        forced_take: bool,
        /// The blocks whose bypass and return did not line up.
        misaligned: u32,
        /// The child gave the take up at its 2 s bound and completed it on a later turn.
        take_given_up: bool,
        /// The take never completed within the child's horizon; the slot is quarantined (an
        /// [`Event::Quarantined`] with op `"reactivation"` follows) and its chain reloaded.
        abandoned: bool,
        /// The cycle passed its budget (4 block budgets), the take was forced, or it was given up.
        stall: bool,
        /// The slot's reactivations so far.
        reactivations: u32,
        /// The slot's bypassed blocks so far.
        total_bypassed_blocks: u32,
        /// The slot's stalls so far.
        stalls: u32,
    },
    /// The child process exited.
    ChildExited {
        /// Its pid.
        pid: u32,
        /// Its exit status, as text.
        status: String,
    },
}

/// How long the reload of a chain after an abandoned take waits for its chain-mates' reads. The child's
/// main thread is free on this path (it has just given the take up), so a read answers at once unless a plugin's
/// `getState` is slow; past this the stored blob is re-applied for that slot (counted, `reload_state_fallbacks`).
pub const RELOAD_READ_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Reload {
    key: u64,
    deadline: Duration,
    waiting: BTreeSet<u32>,
}

/// Where a refused state blob came from (D21's two check points).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateSource {
    /// The model's blob, before it was sent.
    Package,
    /// A blob the child sent.
    Child,
}

/// The control-section counters (the supervisor writes them; the host application mirrors them into its telemetry). Read-only
/// outside the crate: each counter's accessor loads it (`Acquire`).
#[derive(Debug, Default)]
pub struct ControlCounters {
    /// The child's state's word ([`ChildState::code`]).
    pub(crate) child_state: AtomicU32,
    /// The live child's pid (0: none).
    pub(crate) child_pid: AtomicU32,
    /// Child deaths, cumulative.
    pub(crate) child_deaths: AtomicU64,
    /// Child respawns, cumulative.
    pub(crate) child_respawns: AtomicU64,
    /// State applications that failed, cumulative.
    pub(crate) state_apply_failures: AtomicU64,
    /// Slots quarantined now.
    pub(crate) chains_quarantined: AtomicU32,
    /// Chains down now.
    pub(crate) chains_down: AtomicU32,
    /// Chains ready and not parked now.
    pub(crate) chains_active: AtomicU32,
    /// Chains at pipeline depth 2 now.
    pub(crate) chains_depth2: AtomicU32,
    /// *(R3; not in 0004)* Gauges: the child's wired and unwired chain memory, summed over the loaded chains
    /// (`chain.loaded`'s `wiredBytes` / `unwiredBytes`; a child that sends neither counts 0). The host application reports them as
    /// `childWiredBytes` / `childUnwiredBytes`.
    pub(crate) child_wired_bytes: AtomicU64,
    /// The unwired half of [`ControlCounters::child_wired_bytes`]'s gauge.
    pub(crate) child_unwired_bytes: AtomicU64,
    /// Chains killed as wedged, cumulative.
    pub(crate) wedges: AtomicU64,
    /// Pinged chains found woken with work pending (a lost wakeup), cumulative.
    pub(crate) lost_wakes: AtomicU64,
    /// Moved slots a save burst left unread, cumulative.
    pub(crate) state_stale: AtomicU64,
    /// Moved slots the last save burst left unread.
    pub(crate) state_stale_now: AtomicU32,
    /// The live child's workgroup route: 0 none, 1 route B, 2 route A, 3 no join.
    pub(crate) workgroup_route: AtomicU32,
    /// The child's workgroup fallbacks (its page's count).
    pub(crate) workgroup_fallbacks: AtomicU64,
    /// The longest plugin operation seen on the child's main thread (ns).
    pub(crate) msg_thread_hold_max_ns: AtomicU64,
    /// Plugin operations on the child's main thread that held it past 1 s, cumulative.
    pub(crate) msg_thread_long_holds: AtomicU64,
    /// The child's spurious wakes (its page's count).
    pub(crate) spurious_wakes: AtomicU64,
    /// `bridgeReactivations` (control telemetry offset 440): `slot.reactivated` events, cumulative.
    pub(crate) reactivations: AtomicU64,
    /// D22 (`bridgeReactivationStalls`, control 448): reactivations whose cycle or take passed its bound.
    pub(crate) reactivation_stalls: AtomicU64,
    /// D21 (`stateFormatRefused`, control 456): state blobs refused by the frame check, from the child or the package.
    pub(crate) state_format_refused: AtomicU64,
    /// D21 (`stateEmptySets`, control 464): both-absent vst3 sets sent (no-ops in the child).
    pub(crate) state_empty_sets: AtomicU64,
    /// Vst3 sets that failed after a chunk reached the plugin.
    pub(crate) state_partial_sets: AtomicU64,
    /// `state.applied` with an invalid `applied` combination (a
    /// protocol error: counted and logged; the slot is still reported moved).
    pub(crate) state_applied_invalid: AtomicU64,
    /// Reactivation takes the child's main thread gave up
    /// at its 2 s bound (completed later, or abandoned at the horizon). Each is also a stall.
    pub(crate) take_give_ups: AtomicU64,
    /// Chain-mates of an abandoned slot reloaded from their
    /// stored blob because their read before the reload failed, was refused, or passed `RELOAD_READ_DEADLINE`.
    pub(crate) reload_state_fallbacks: AtomicU64,
    /// Not in 0004: the respawn attempt counter, for tests and logs.
    pub(crate) attempts: AtomicU32,
    /// Protocol faults that killed the child (counted loudly, never attributed), and, as before, a reader
    /// that ended at EOF mid-frame or on a `recv` error (those deaths stay attributed, H-4).
    pub(crate) protocol_errors: AtomicU64,
    /// The live hello (`child_incompatible`; not in the telemetry layout, placing it is an ABI
    /// step): `hello` replies whose protocol major was not the host application's, cumulative.
    pub(crate) child_incompatible: AtomicU64,
    /// N4 (not in 0004; for tests and logs): children that exited 2 before their `hello` was verified.
    pub(crate) child_usage_exits: AtomicU64,
    /// The live child's `hello` protocol (`major << 16 | minor`; 0 before a verified hello). A minor of
    /// 2 or more takes `SlotDesc.enabled` and `slot.setEnabled`.
    pub(crate) child_protocol: AtomicU32,
    /// not in 0004 (tests and logs): `EditorOpen` commands dropped because the chain is a render set (a render
    /// chain never opens an editor) or is not hosted.
    pub(crate) editor_opens_refused: AtomicU64,
    /// (rule 2), not in 0004: `Load` commands refused because their key is already
    /// a hosted chain (a key is never loaded twice; the child refuses it too). Loud.
    pub(crate) loads_refused_live_key: AtomicU64,
    /// Not in 0004, for tests and logs (as `attempts`): consecutive attributed deaths naming the same slot (D7's
    /// "two consecutive deaths"; 2 quarantines it), 0 after a death attributed to no slot. An unattributed death
    /// leaves it as it was, as it leaves `last_attribution`.
    pub(crate) attribution_run: AtomicU32,
    /// H-1 (a): 100 ms for a warm spawn (every spawn after a process's first). Not in 0004,
    /// for tests and logs: the last incarnation's `hello` round trip, from its send to its verified reply (decoded,
    /// major checked), in real time (ns), stored before its `ChildUp` is emitted. The 2 s [`HELLO_DEADLINE`] stays the
    /// hard kill ceiling.
    pub(crate) hello_rtt_ns: AtomicU64,
    /// H-1 (b): ≤ 500 ms for a cold launch (a process's first spawn). Not in 0004, for tests
    /// and logs: the last incarnation's launch, from the start of its `posix_spawn` to its verified `hello` reply, in
    /// real time (ns), stored before its `ChildUp` is emitted.
    pub(crate) hello_launch_ns: AtomicU64,
    /// D6's reader blob budget (branch `live-reader`, 2026-11-01), not in 0004, for tests and logs: the blob bytes the
    /// readers hold now (admitted from a frame's header, the frame the decoder is filling included, until the
    /// supervisor takes the frame or it is dropped). Never above `Config::reader_blob_budget`.
    pub(crate) reader_blob_held: AtomicU64,
    /// The most `reader_blob_held` has been.
    pub(crate) reader_blob_peak: AtomicU64,
    /// Blob bytes admitted, cumulative.
    pub(crate) reader_blob_admitted: AtomicU64,
    /// D6's frame bound, not in 0004, for tests and logs: the frames in flight from the
    /// readers to the supervisor now (admitted from a header until the supervisor takes the frame or it is dropped),
    /// summed over the readers.
    pub(crate) reader_frames_held: AtomicU64,
    /// The most frames ONE reader has had in flight. Never above [`READER_FRAMES`].
    pub(crate) reader_frames_peak: AtomicU64,
    /// Frames admitted, cumulative.
    pub(crate) reader_frames_admitted: AtomicU64,
    /// Frames whose admission waited for the supervisor to free blob bytes or a frame (the reader stopped reading
    /// meanwhile: a flooding child stalls only itself), cumulative.
    pub(crate) reader_blob_waits: AtomicU64,
    /// not in 0004 (tests and logs): the live child's `HelloReply.wiredBytes` / `unwiredBytes` (its page),
    /// 0 while no verified child runs. Already inside `child_wired_bytes` / `child_unwired_bytes`.
    pub(crate) child_page_wired_bytes: AtomicU64,
    /// The unwired half of [`ControlCounters::child_page_wired_bytes`].
    pub(crate) child_page_unwired_bytes: AtomicU64,
    /// not in 0004 (tests and logs; `system.logInfo.childLogSet` has the engine-wide sums): this bridge's
    /// `log.set` frames sent, replies naming the word sent, error replies, and replies whose word was not the one sent.
    pub(crate) log_set_sent: AtomicU64,
    /// `log.set` replies naming the word sent.
    pub(crate) log_set_applied: AtomicU64,
    /// `log.set` error replies.
    pub(crate) log_set_refused: AtomicU64,
    /// `log.set` replies naming another word.
    pub(crate) log_set_mismatch: AtomicU64,
    /// not in 0004 (tests and logs): requests sent with a `corr`; corrs not added (`with_corr` refused:
    /// invalid, meta-less, or the meta at its cap); echoes read on the child's answers; echoes ignored (malformed, or on
    /// a frame that is no answer).
    pub(crate) corr_sent: AtomicU64,
    /// Corrs not added.
    pub(crate) corr_dropped: AtomicU64,
    /// Corr echoes read on the child's answers.
    pub(crate) child_corr_echoed: AtomicU64,
    /// Corr echoes ignored.
    pub(crate) child_corr_ignored: AtomicU64,
}

impl ControlCounters {
    /// The child's state now.
    pub fn child_state(&self) -> ChildState {
        ChildState::from_code(self.child_state.load(Ordering::Acquire))
    }

    /// The live child's pid (0: none).
    pub fn child_pid(&self) -> u32 {
        self.child_pid.load(Ordering::Acquire)
    }

    /// Child deaths, cumulative.
    pub fn child_deaths(&self) -> u64 {
        self.child_deaths.load(Ordering::Acquire)
    }

    /// Child respawns, cumulative.
    pub fn child_respawns(&self) -> u64 {
        self.child_respawns.load(Ordering::Acquire)
    }

    /// State applications that failed, cumulative.
    pub fn state_apply_failures(&self) -> u64 {
        self.state_apply_failures.load(Ordering::Acquire)
    }

    /// Slots quarantined now.
    pub fn chains_quarantined(&self) -> u32 {
        self.chains_quarantined.load(Ordering::Acquire)
    }

    /// Chains down now.
    pub fn chains_down(&self) -> u32 {
        self.chains_down.load(Ordering::Acquire)
    }

    /// Chains ready and not parked now.
    pub fn chains_active(&self) -> u32 {
        self.chains_active.load(Ordering::Acquire)
    }

    /// Chains at pipeline depth 2 now.
    pub fn chains_depth2(&self) -> u32 {
        self.chains_depth2.load(Ordering::Acquire)
    }

    /// *(R3; not in 0004)* Gauges: the child's wired and unwired chain memory, summed over the loaded chains
    /// (`chain.loaded`'s `wiredBytes` / `unwiredBytes`; a child that sends neither counts 0). The host application reports them as
    /// `childWiredBytes` / `childUnwiredBytes`.
    pub fn child_wired_bytes(&self) -> u64 {
        self.child_wired_bytes.load(Ordering::Acquire)
    }

    /// The unwired half of [`ControlCounters::child_wired_bytes`]'s gauge.
    pub fn child_unwired_bytes(&self) -> u64 {
        self.child_unwired_bytes.load(Ordering::Acquire)
    }

    /// Chains killed as wedged, cumulative.
    pub fn wedges(&self) -> u64 {
        self.wedges.load(Ordering::Acquire)
    }

    /// Pinged chains found woken with work pending (a lost wakeup), cumulative.
    pub fn lost_wakes(&self) -> u64 {
        self.lost_wakes.load(Ordering::Acquire)
    }

    /// Moved slots a save burst left unread, cumulative.
    pub fn state_stale(&self) -> u64 {
        self.state_stale.load(Ordering::Acquire)
    }

    /// Moved slots the last save burst left unread.
    pub fn state_stale_now(&self) -> u32 {
        self.state_stale_now.load(Ordering::Acquire)
    }

    /// The live child's workgroup route: 0 none, 1 route B, 2 route A, 3 no join.
    pub fn workgroup_route(&self) -> u32 {
        self.workgroup_route.load(Ordering::Acquire)
    }

    /// The child's workgroup fallbacks (its page's count).
    pub fn workgroup_fallbacks(&self) -> u64 {
        self.workgroup_fallbacks.load(Ordering::Acquire)
    }

    /// The longest plugin operation seen on the child's main thread (ns).
    pub fn msg_thread_hold_max_ns(&self) -> u64 {
        self.msg_thread_hold_max_ns.load(Ordering::Acquire)
    }

    /// Plugin operations on the child's main thread that held it past 1 s, cumulative.
    pub fn msg_thread_long_holds(&self) -> u64 {
        self.msg_thread_long_holds.load(Ordering::Acquire)
    }

    /// The child's spurious wakes (its page's count).
    pub fn spurious_wakes(&self) -> u64 {
        self.spurious_wakes.load(Ordering::Acquire)
    }

    /// `bridgeReactivations` (control telemetry offset 440): `slot.reactivated` events, cumulative.
    pub fn reactivations(&self) -> u64 {
        self.reactivations.load(Ordering::Acquire)
    }

    /// D22 (`bridgeReactivationStalls`, control 448): reactivations whose cycle or take passed its bound.
    pub fn reactivation_stalls(&self) -> u64 {
        self.reactivation_stalls.load(Ordering::Acquire)
    }

    /// D21 (`stateFormatRefused`, control 456): state blobs refused by the frame check, from the child or the package.
    pub fn state_format_refused(&self) -> u64 {
        self.state_format_refused.load(Ordering::Acquire)
    }

    /// D21 (`stateEmptySets`, control 464): both-absent vst3 sets sent (no-ops in the child).
    pub fn state_empty_sets(&self) -> u64 {
        self.state_empty_sets.load(Ordering::Acquire)
    }

    /// Vst3 sets that failed after a chunk reached the plugin.
    pub fn state_partial_sets(&self) -> u64 {
        self.state_partial_sets.load(Ordering::Acquire)
    }

    /// `state.applied` with an invalid `applied` combination (a
    /// protocol error: counted and logged; the slot is still reported moved).
    pub fn state_applied_invalid(&self) -> u64 {
        self.state_applied_invalid.load(Ordering::Acquire)
    }

    /// Reactivation takes the child's main thread gave up
    /// at its 2 s bound (completed later, or abandoned at the horizon). Each is also a stall.
    pub fn take_give_ups(&self) -> u64 {
        self.take_give_ups.load(Ordering::Acquire)
    }

    /// Chain-mates of an abandoned slot reloaded from their
    /// stored blob because their read before the reload failed, was refused, or passed `RELOAD_READ_DEADLINE`.
    pub fn reload_state_fallbacks(&self) -> u64 {
        self.reload_state_fallbacks.load(Ordering::Acquire)
    }

    /// Not in 0004: the respawn attempt counter, for tests and logs.
    pub fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::Acquire)
    }

    /// Protocol faults that killed the child (counted loudly, never attributed), and, as before, a reader
    /// that ended at EOF mid-frame or on a `recv` error (those deaths stay attributed, H-4).
    pub fn protocol_errors(&self) -> u64 {
        self.protocol_errors.load(Ordering::Acquire)
    }

    /// The live hello (`child_incompatible`; not in the telemetry layout, placing it is an ABI
    /// step): `hello` replies whose protocol major was not the host application's, cumulative.
    pub fn child_incompatible(&self) -> u64 {
        self.child_incompatible.load(Ordering::Acquire)
    }

    /// N4 (not in 0004; for tests and logs): children that exited 2 before their `hello` was verified.
    pub fn child_usage_exits(&self) -> u64 {
        self.child_usage_exits.load(Ordering::Acquire)
    }

    /// The live child's `hello` protocol (`major << 16 | minor`; 0 before a verified hello). A minor of
    /// 2 or more takes `SlotDesc.enabled` and `slot.setEnabled`.
    pub fn child_protocol(&self) -> u32 {
        self.child_protocol.load(Ordering::Acquire)
    }

    /// not in 0004 (tests and logs): `EditorOpen` commands dropped because the chain is a render set (a render
    /// chain never opens an editor) or is not hosted.
    pub fn editor_opens_refused(&self) -> u64 {
        self.editor_opens_refused.load(Ordering::Acquire)
    }

    /// (rule 2), not in 0004: `Load` commands refused because their key is already
    /// a hosted chain (a key is never loaded twice; the child refuses it too). Loud.
    pub fn loads_refused_live_key(&self) -> u64 {
        self.loads_refused_live_key.load(Ordering::Acquire)
    }

    /// Not in 0004, for tests and logs (as `attempts`): consecutive attributed deaths naming the same slot (D7's
    /// "two consecutive deaths"; 2 quarantines it), 0 after a death attributed to no slot. An unattributed death
    /// leaves it as it was, as it leaves `last_attribution`.
    pub fn attribution_run(&self) -> u32 {
        self.attribution_run.load(Ordering::Acquire)
    }

    /// H-1 (a): 100 ms for a warm spawn (every spawn after a process's first). Not in 0004,
    /// for tests and logs: the last incarnation's `hello` round trip, from its send to its verified reply (decoded,
    /// major checked), in real time (ns), stored before its `ChildUp` is emitted. The 2 s [`HELLO_DEADLINE`] stays the
    /// hard kill ceiling.
    pub fn hello_rtt_ns(&self) -> u64 {
        self.hello_rtt_ns.load(Ordering::Acquire)
    }

    /// H-1 (b): ≤ 500 ms for a cold launch (a process's first spawn). Not in 0004, for tests
    /// and logs: the last incarnation's launch, from the start of its `posix_spawn` to its verified `hello` reply, in
    /// real time (ns), stored before its `ChildUp` is emitted.
    pub fn hello_launch_ns(&self) -> u64 {
        self.hello_launch_ns.load(Ordering::Acquire)
    }

    /// D6's reader blob budget (branch `live-reader`, 2026-11-01), not in 0004, for tests and logs: the blob bytes the
    /// readers hold now (admitted from a frame's header, the frame the decoder is filling included, until the
    /// supervisor takes the frame or it is dropped). Never above `Config::reader_blob_budget`.
    pub fn reader_blob_held(&self) -> u64 {
        self.reader_blob_held.load(Ordering::Acquire)
    }

    /// The most `reader_blob_held` has been.
    pub fn reader_blob_peak(&self) -> u64 {
        self.reader_blob_peak.load(Ordering::Acquire)
    }

    /// Blob bytes admitted, cumulative.
    pub fn reader_blob_admitted(&self) -> u64 {
        self.reader_blob_admitted.load(Ordering::Acquire)
    }

    /// D6's frame bound, not in 0004, for tests and logs: the frames in flight from the
    /// readers to the supervisor now (admitted from a header until the supervisor takes the frame or it is dropped),
    /// summed over the readers.
    pub fn reader_frames_held(&self) -> u64 {
        self.reader_frames_held.load(Ordering::Acquire)
    }

    /// The most frames ONE reader has had in flight. Never above [`READER_FRAMES`].
    pub fn reader_frames_peak(&self) -> u64 {
        self.reader_frames_peak.load(Ordering::Acquire)
    }

    /// Frames admitted, cumulative.
    pub fn reader_frames_admitted(&self) -> u64 {
        self.reader_frames_admitted.load(Ordering::Acquire)
    }

    /// Frames whose admission waited for the supervisor to free blob bytes or a frame (the reader stopped reading
    /// meanwhile: a flooding child stalls only itself), cumulative.
    pub fn reader_blob_waits(&self) -> u64 {
        self.reader_blob_waits.load(Ordering::Acquire)
    }

    /// not in 0004 (tests and logs): the live child's `HelloReply.wiredBytes` / `unwiredBytes` (its page),
    /// 0 while no verified child runs. Already inside `child_wired_bytes` / `child_unwired_bytes`.
    pub fn child_page_wired_bytes(&self) -> u64 {
        self.child_page_wired_bytes.load(Ordering::Acquire)
    }

    /// The unwired half of [`ControlCounters::child_page_wired_bytes`].
    pub fn child_page_unwired_bytes(&self) -> u64 {
        self.child_page_unwired_bytes.load(Ordering::Acquire)
    }

    /// not in 0004 (tests and logs; `system.logInfo.childLogSet` has the engine-wide sums): this bridge's
    /// `log.set` frames sent, replies naming the word sent, error replies, and replies whose word was not the one sent.
    pub fn log_set_sent(&self) -> u64 {
        self.log_set_sent.load(Ordering::Acquire)
    }

    /// `log.set` replies naming the word sent.
    pub fn log_set_applied(&self) -> u64 {
        self.log_set_applied.load(Ordering::Acquire)
    }

    /// `log.set` error replies.
    pub fn log_set_refused(&self) -> u64 {
        self.log_set_refused.load(Ordering::Acquire)
    }

    /// `log.set` replies naming another word.
    pub fn log_set_mismatch(&self) -> u64 {
        self.log_set_mismatch.load(Ordering::Acquire)
    }

    /// not in 0004 (tests and logs): requests sent with a `corr`; corrs not added (`with_corr` refused:
    /// invalid, meta-less, or the meta at its cap); echoes read on the child's answers; echoes ignored (malformed, or on
    /// a frame that is no answer).
    pub fn corr_sent(&self) -> u64 {
        self.corr_sent.load(Ordering::Acquire)
    }

    /// Corrs not added.
    pub fn corr_dropped(&self) -> u64 {
        self.corr_dropped.load(Ordering::Acquire)
    }

    /// Corr echoes read on the child's answers.
    pub fn child_corr_echoed(&self) -> u64 {
        self.child_corr_echoed.load(Ordering::Acquire)
    }

    /// Corr echoes ignored.
    pub fn child_corr_ignored(&self) -> u64 {
        self.child_corr_ignored.load(Ordering::Acquire)
    }
}

/// The child's state, as [`ControlCounters::child_state`] reads it. Its [`ChildState::code`] is the control section's
/// `childState` word (0 none, 1 starting, 2 up, 3 backoff, 4 exhausted).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChildState {
    /// No child.
    None,
    /// A child is spawning (its `hello` not verified yet).
    Starting,
    /// A verified child is up.
    Up,
    /// Waiting out a respawn backoff.
    Backoff,
    /// Three attempts failed: no child until a restart or a reopen.
    Exhausted,
}

impl ChildState {
    /// The control section's word.
    pub fn code(self) -> u32 {
        match self {
            ChildState::None => CHILD_NONE,
            ChildState::Starting => CHILD_STARTING,
            ChildState::Up => CHILD_UP,
            ChildState::Backoff => CHILD_BACKOFF,
            ChildState::Exhausted => CHILD_EXHAUSTED,
        }
    }

    fn from_code(c: u32) -> ChildState {
        match c {
            CHILD_STARTING => ChildState::Starting,
            CHILD_UP => ChildState::Up,
            CHILD_BACKOFF => ChildState::Backoff,
            CHILD_EXHAUSTED => ChildState::Exhausted,
            _ => ChildState::None,
        }
    }
}

/// No child.
const CHILD_NONE: u32 = 0;
/// A child is spawning (its `hello` not verified yet).
const CHILD_STARTING: u32 = 1;
/// A verified child is up.
const CHILD_UP: u32 = 2;
/// Waiting out a respawn backoff.
const CHILD_BACKOFF: u32 = 3;
/// Three attempts failed: no child until `plugin.restartHost` or a reopen.
const CHILD_EXHAUSTED: u32 = 4;

/// Control's handle. Dropping it shuts the child down and joins the supervisor.
pub struct Bridge {
    tx: SyncSender<Inbound>,
    pending: Arc<AtomicUsize>,
    counters: Arc<ControlCounters>,
    thread: Option<JoinHandle<()>>,
}

/// The command queue is full; retry. It hands the command back.
#[derive(Debug)]
pub struct Busy(pub Command);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the plugin host's command queue is full; retry")
    }
}

impl std::error::Error for Busy {}

impl Bridge {
    /// Start the supervisor and the child. Events go to `events` (control's bounded channel).
    pub fn start(cfg: Config, events: SyncSender<Event>) -> std::io::Result<Bridge> {
        let (tx, rx) = sync_channel(COMMAND_BOUND + READER_FRAMES + 16);
        let pending = Arc::new(AtomicUsize::new(0));
        let counters = Arc::new(ControlCounters::default());
        let thread_word = cfg.names.thread;
        let mut sup = Supervisor::new(cfg, tx.clone(), events, pending.clone(), counters.clone());
        let rx = InboundRx {
            rx,
            gate: sup.blob_gate.clone(),
        };
        let thread = std::thread::Builder::new()
            .name(format!("{thread_word}-super"))
            .spawn(move || sup.run(rx))?;
        Ok(Bridge {
            tx,
            pending,
            counters,
            thread: Some(thread),
        })
    }

    /// Never blocks: a full queue is `Busy` (D6). The command carries no `corr` ([`Bridge::send_corr`]).
    // `Busy` hands the command back unmoved (no allocation on the BUSY path); its size is the command's.
    #[allow(clippy::result_large_err)]
    pub fn send(&self, c: Command) -> Result<(), Busy> {
        self.push(c, None)
    }

    /// [`Bridge::send`] for a command sent on a request's behalf: `corr` (the request's correlation id) travels with
    /// it, and the frames it sends carry it to a verified child. Only a valid one ([`corr_ok`]); an invalid one is
    /// dropped.
    #[allow(clippy::result_large_err)]
    pub fn send_corr(&self, c: Command, corr: Option<String>) -> Result<(), Busy> {
        self.push(c, corr.filter(|c| runecraft_contracts::log::corr_ok(c)))
    }

    #[allow(clippy::result_large_err)]
    fn push(&self, c: Command, corr: Option<String>) -> Result<(), Busy> {
        if self.pending.fetch_add(1, Ordering::AcqRel) >= COMMAND_BOUND {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(Busy(c));
        }
        match self.tx.try_send(Inbound::Cmd(c, corr)) {
            Ok(()) => Ok(()),
            Err(
                TrySendError::Full(Inbound::Cmd(c, _))
                | TrySendError::Disconnected(Inbound::Cmd(c, _)),
            ) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Err(Busy(c))
            }
            Err(_) => unreachable!(),
        }
    }

    /// The control-section counters the supervisor writes.
    pub fn counters(&self) -> &Arc<ControlCounters> {
        &self.counters
    }

    /// Shut down and join (at most ~3 s; the child is killed if it does not exit).
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = self.tx.send(Inbound::Cmd(Command::Shutdown, None));
            let _ = t.join();
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Inbound {
    /// A command and the `corr` of the request control was serving when it sent it.
    Cmd(Command, Option<String>),
    /// A frame and its share of D6's reader bounds, its frame and its blob bytes ([`BlobTicket`]).
    Frame(u64, Frame, BlobTicket),
    ReaderDone(u64, ReaderEnd),
    Died(u64, runecraft_ipc::process::ChildExit),
}

/// Why the reader thread stopped (H-4 sorts them).
#[derive(Debug)]
enum ReaderEnd {
    /// EOF between frames: an ordinary death, attributed.
    Eof,
    /// EOF inside a frame: an ordinary death, attributed (a crash can cut a frame in half); counted as before.
    EofMidFrame,
    /// A `recv` error: an ordinary death, attributed; counted as before.
    Recv(String),
    /// The decoder refused the stream ("protocol: …"): a protocol fault, never attributed.
    Refused(codec::ProtoError),
    /// The child sent descriptors (it never may): a protocol fault, never attributed.
    Descriptors,
}

#[derive(Debug)]
enum Pending {
    Hello,
    Generations {
        burst: Option<u64>,
    },
    Receipt,
    StateGet {
        key: u64,
        slot: u32,
        burst: Option<u64>,
    },
    /// A chain-mate's read before an abandoned slot's chain is reloaded.
    ReloadRead {
        key: u64,
        slot: u32,
    },
    ParamList {
        key: u64,
        slot: u32,
        offset: u32,
        count: u32,
    },
    /// `param.set`'s reply.
    ParamSet {
        key: u64,
        slot: u32,
        plugin_id: u32,
    },
    /// `slot.setEnabled`'s reply (`ok: false` is logged; the spec already holds the flag).
    SlotEnabled,
    /// `editor.open`'s receipt: a receipt, which may carry the Windows container window ([`Event::EditorWindow`]).
    EditorOpen {
        key: u64,
        slot: u32,
    },
    /// `log.set`'s reply, the word sent.
    LogSet {
        levels: u64,
    },
    Shutdown,
}

/// Whether a death is attributed (D7) or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Blame {
    Attributed,
    None,
}

/// What follows a death: D7's backoff, or the exhausted state at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Respawn {
    Backoff,
    Never,
}

/// "reply" / "event" / "error reply", for a protocol fault's line.
fn frame_word(h: &codec::Header) -> &'static str {
    match (h.is_reply(), h.is_error()) {
        (true, true) => "error reply",
        (true, false) => "reply",
        _ => "event",
    }
}

struct Outgoing {
    frame: Vec<u8>,
    /// The objects riding with the frame: the region or page, then its doorbells' (none on unix, so at most one).
    objs: Vec<runecraft_ipc::channel::Transferable>,
    pending: Option<Pending>,
}

#[derive(Debug, Default)]
struct Wedge {
    last_ack: u64,
    progress_at: Duration,
    ping: Option<(u64, Duration)>,
}

#[derive(Debug)]
struct SlotEntry {
    state: Option<SlotState>,
    applied: bool,
    child_gen: u64,
    /// The child generation of the last completed read: a slot with `child_gen != read_gen` has moved (D6).
    read_gen: u64,
    editor_open: bool,
    /// Protocol 3.4: `SlotInfo.hasEditor`, corrected by `editor.availability`.
    has_editor: bool,
    last_read_at: Duration,
    name: String,
    /// From `chain.loaded` (D6), as clamped by the host application; 0 for a quarantined slot whatever the child said (D7).
    loaded_ok: bool,
    /// the child's notices for the slot (`chain.loaded`, then `chain.prepared`), clamped.
    notices: Vec<String>,
    latency: u32,
    param_count: u32,
    /// D22, per (chain, slot), saturating (the child is untrusted): cycles, bypassed blocks, stalls.
    reactivations: u32,
    bypassed_blocks: u32,
    stalls: u32,
    /// A partial set changed the plugin since the last completed read (the model's blob is not its state).
    partial: bool,
    /// Control re-marked the slot ([`Command::Remark`]): the next burst reads it.
    remarked: bool,
    /// The editor was open at some point since the last completed read.
    editor_seen: bool,
}

struct Chain {
    spec: ChainSpec,
    slots: Vec<SlotEntry>,
    region: Option<Arc<Region>>,
    loaded: bool,
    ready: bool,
    awaiting_states: usize,
    parked: bool,
    down: bool,
    latency: u32,
    wedge: Wedge,
    depth: DepthController,
    /// The enabled flags the last `chain.load` carried (a toggle while it loads is re-sent once loaded).
    sent_enabled: Vec<bool>,
    /// *(R3)* The child's wired and unwired chain memory, as its last `chain.loaded` reported (0 from a child
    /// that sends neither).
    wired_bytes: u64,
    unwired_bytes: u64,
}

struct Child {
    pid: u32,
    /// The spawned process (its pid pinned until the reaper's wait reaps it): every kill goes through it.
    process: Arc<runecraft_ipc::process::Child>,
    incarnation: u64,
    sock: runecraft_ipc::channel::ChanTx,
    page: Arc<ChildPage>,
    up_at: Duration,
    /// The reply to `hello` was decoded and passed the major check. Until then only `hello` is sent.
    hello_ok: bool,
    /// H-2: when the unverified `hello` becomes a no-blame kill (the hello's send time + [`Config::hello_deadline`]).
    hello_by: Duration,
    /// H-1: the hello's send, in real time (the round trip is measured whatever the configured clock).
    hello_sent: std::time::Instant,
    /// H-1 (b): the start of this incarnation's `posix_spawn`, in real time.
    spawn_started: std::time::Instant,
    dead: bool,
    mach_rx: Option<MachReceive>,
    /// Route A's one-shot name, while the child's channel hello is awaited.
    mach_a: Option<MachReceive>,
    child_port: Option<MachSend>,
    /// (key, slot, gen) re-applied by this incarnation and not yet confirmed.
    unconfirmed: BTreeSet<(u64, u32, u64)>,
    kill_reason: Option<DownReason>,
    inflight: BTreeMap<u64, Pending>,
    queue: VecDeque<Outgoing>,
    hold_seen: u64,
}

#[derive(Debug)]
struct Burst {
    token: u64,
    deadline: Duration,
    /// E1: the byte budget, and the blob bytes received so far.
    byte_budget: u64,
    bytes: u64,
    /// Reads sent.
    requested: BTreeSet<(u64, u32)>,
    /// Reads still to send (at most [`BURST_WINDOW`] in flight).
    queue: VecDeque<(u64, u32)>,
    /// Reads answered, whatever the answer (a blob, a refusal, `tooLarge`, an error).
    answered: BTreeSet<(u64, u32)>,
    fresh: Vec<Fresh>,
    too_large: Vec<(u64, u32, u64)>,
    generations_answered: bool,
}

struct Supervisor {
    cfg: Config,
    self_tx: SyncSender<Inbound>,
    events: SyncSender<Event>,
    pending_cmds: Arc<AtomicUsize>,
    counters: Arc<ControlCounters>,
    chains: BTreeMap<u64, Chain>,
    child: Option<Child>,
    incarnation: u64,
    attempts: u32,
    respawn_at: Option<Duration>,
    exhausted: bool,
    restart_reason: Option<DownReason>,
    restarted_slots: Vec<RestartedSlot>,
    last_attribution: Option<(u64, u32)>,
    /// Consecutive attributed deaths naming `last_attribution` (mirrored in `ControlCounters::attribution_run`).
    attribution_run: u32,
    quarantine: BTreeSet<(u64, u32)>,
    next_id: u64,
    poll_until: Duration,
    next_poll: Duration,
    next_depth: Duration,
    burst: Option<Burst>,
    /// D6's reader -> supervisor blob budget, shared with every incarnation's reader.
    blob_gate: Arc<BlobGate>,
    shutting_down: bool,
    shutdown_by: Option<std::time::Instant>,
    /// The chain the wedge detector declared down: the death's attribution starts there.
    wedged: Option<u64>,
    barrier: Option<SyncSender<()>>,
    /// Chains waiting to be reloaded after an abandoned take, their chain-mates' reads outstanding.
    reloads: Vec<Reload>,
    rng: u64,
    /// the `corr` of the command being handled (None outside [`Supervisor::command`]).
    corr: Option<String>,
    /// The child's valid `corr` echo on the answer being handled (None outside [`Supervisor::frame`]): the
    /// `corr` of every record written meanwhile.
    echo: Option<String>,
    /// the last level generation this supervisor forwarded (or recorded with no
    /// child to send it to); its own, so every bridge of the process forwards each `system.setLog`.
    log_gen_sent: u64,
    /// the verified child's page memory (wired, unwired), 0 with none.
    page_wired: (u64, u64),
}

fn now_ticks() -> u64 {
    runecraft_ipc::bridge::host_ticks()
}

impl Supervisor {
    fn new(
        cfg: Config,
        self_tx: SyncSender<Inbound>,
        events: SyncSender<Event>,
        pending_cmds: Arc<AtomicUsize>,
        counters: Arc<ControlCounters>,
    ) -> Supervisor {
        let blob_gate = BlobGate::new(cfg.reader_blob_budget, counters.clone());
        let log_gen_sent = cfg.levels.level_state().generation;
        Supervisor {
            cfg,
            self_tx,
            events,
            pending_cmds,
            counters,
            chains: BTreeMap::new(),
            child: None,
            incarnation: 0,
            attempts: 0,
            respawn_at: None,
            exhausted: false,
            restart_reason: None,
            restarted_slots: Vec::new(),
            last_attribution: None,
            attribution_run: 0,
            quarantine: BTreeSet::new(),
            next_id: 1,
            poll_until: Duration::ZERO,
            next_poll: Duration::ZERO,
            next_depth: Duration::ZERO,
            burst: None,
            blob_gate,
            shutting_down: false,
            shutdown_by: None,
            wedged: None,
            barrier: None,
            reloads: Vec::new(),
            rng: runecraft_ipc::instance::instance_id() | 1,
            log_gen_sent,
            corr: None,
            echo: None,
            page_wired: (0, 0),
        }
    }

    fn now(&self) -> Duration {
        self.cfg.clock.now()
    }

    fn nonce(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x | 1
    }

    fn emit(&self, e: Event) {
        self.log_event(&e);
        // The supervisor waits on control's channel; control never waits on the supervisor (D6).
        let _ = self.events.send(e);
    }

    fn run(&mut self, inbox: InboundRx) {
        let rx = &inbox.rx;
        self.spawn();
        loop {
            let mut wait = self.cfg.tick;
            if let Some(b) = &self.burst {
                wait = wait.min(
                    b.deadline
                        .saturating_sub(self.now())
                        .max(Duration::from_millis(1)),
                );
            }
            match rx.recv_timeout(wait) {
                Ok(Inbound::Cmd(c, corr)) => {
                    self.pending_cmds.fetch_sub(1, Ordering::AcqRel);
                    // a `system.setLog` that completed before this command was sent reaches the child first.
                    self.sync_log_levels();
                    self.corr = corr;
                    let stop = self.command(c);
                    self.corr = None;
                    if stop {
                        break;
                    }
                }
                Ok(Inbound::Frame(inc, f, ticket)) => {
                    // D6: the frame is taken, so it and its blob bytes leave the readers' bounds (a waiting reader
                    // admits).
                    ticket.redeem();
                    if self
                        .child
                        .as_ref()
                        .is_some_and(|c| c.incarnation == inc && !c.dead)
                    {
                        self.frame(f);
                    }
                }
                Ok(Inbound::ReaderDone(inc, end)) => {
                    if self.child.as_ref().is_some_and(|c| c.incarnation == inc) {
                        match end {
                            // H-4: frames the host application refuses, as the protocol fault rule's; the reader sent this before it closed the
                            // socket, so the child's own exit (its EOF) cannot be seen first.
                            ReaderEnd::Refused(e) => self.protocol_fault(&format!(
                                "the reader's decoder refused the stream: {e}"
                            )),
                            ReaderEnd::Descriptors => self
                                .protocol_fault("descriptors from the child (it never sends any)"),
                            // N4: an EOF before the verified hello waits for the exit status (the reaper's report
                            // follows at once), so a usage exit is told from a crash. A child that closed fd 3 and
                            // lives on is the hello deadline's (H-2).
                            ReaderEnd::Eof
                                if USAGE_EXIT_TERMINAL
                                    && self.child.as_ref().is_some_and(|c| {
                                        !c.hello_ok || cfg!(mutant_usage_exit_ignores_hello)
                                    }) => {}
                            ReaderEnd::Eof => self.death(DownReason::Crash),
                            // A crash can cut a frame in half: attributed, as before (H-4), and counted as before.
                            ReaderEnd::EofMidFrame | ReaderEnd::Recv(_) => {
                                let what = match &end {
                                    ReaderEnd::Recv(e) => format!("a recv error: {e}"),
                                    _ => "EOF mid-frame".into(),
                                };
                                let stderr_tag = self.cfg.names.stderr;
                                runecraft_rt::safe_eprintln!(
                                    "{stderr_tag}: the child's stream ended ({what}): an ordinary death, attributed"
                                );
                                self.counters
                                    .protocol_errors
                                    .fetch_add(1, Ordering::Relaxed);
                                self.death(DownReason::Crash);
                            }
                        }
                    }
                }
                Ok(Inbound::Died(inc, st)) => {
                    if let Some(c) = &self.child
                        && c.incarnation == inc
                    {
                        let pid = c.pid;
                        if USAGE_EXIT_TERMINAL
                            && (!c.hello_ok || cfg!(mutant_usage_exit_ignores_hello))
                            && st == runecraft_ipc::process::ChildExit::Exited(EXIT_USAGE)
                        {
                            self.usage_exit();
                        } else {
                            self.death(DownReason::Crash);
                        }
                        self.emit(Event::ChildExited {
                            pid,
                            status: format!("{st}"),
                        });
                        self.child = None;
                        if self.shutting_down {
                            break;
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if let Some(by) = self.shutdown_by
                && std::time::Instant::now() >= by
            {
                break;
            }
            // an idle supervisor forwards a `system.setLog` within one tick.
            self.sync_log_levels();
            self.tick();
            if let Some(b) = self.barrier.take() {
                let _ = b.send(());
            }
        }
        if let Some(c) = self.child.take()
            && !c.dead
        {
            c.process.kill();
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Spawn
    // -----------------------------------------------------------------------------------------------------------------

    fn spawn(&mut self) {
        if self.shutting_down {
            return;
        }
        self.counters
            .child_state
            .store(CHILD_STARTING, Ordering::Relaxed);
        match self.try_spawn() {
            Ok(()) => {}
            Err(e) => {
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!("{stderr_tag}: spawn failed: {e}");
                self.after_death(DownReason::Crash);
            }
        }
    }

    fn try_spawn(&mut self) -> std::io::Result<()> {
        let (ours, theirs) = runecraft_ipc::channel::channel_pair()?;
        let mach_rx = MachReceive::new().ok();
        let send = match (&mach_rx, self.cfg.route) {
            (Some(r), RoutePref::B) => r.make_send().ok(),
            _ => None,
        };
        let spawn_started = std::time::Instant::now();
        // (N3: `--crash-log`) and the log (the child's log folder, levels and salt, when the application has a log).
        let mut args = spawn_args(&self.cfg);
        args.extend(self.cfg.levels.child_args());
        let env = host_child_env(&*self.cfg.levels, self.cfg.env.clone());
        let spawn = runecraft_ipc::process::Spawn::new(&self.cfg.program, &args, &env);
        // The child's end goes with the spawn (the parent's copy closes as it returns); route B registers the port.
        #[cfg(target_os = "macos")]
        let process = match send.as_ref() {
            Some(port) => {
                runecraft_ipc::process::macos::spawn_child_registering(&spawn, theirs, port)?
            }
            None => runecraft_ipc::process::spawn_child(&spawn, theirs)?,
        };
        #[cfg(not(target_os = "macos"))]
        let process = runecraft_ipc::process::spawn_child(&spawn, theirs)?;
        drop(send);
        let process = Arc::new(process);
        let pid = process.pid();
        let (ours, rsock) = ours.bind(&*process)?;
        self.incarnation += 1;
        let inc = self.incarnation;
        let (page, page_fd) = ChildPage::create()?;
        // The page, then its hub doorbells' objects (none on unix: one object, `fds: 1`, as ever).
        let mut page_objs = vec![page_fd];
        page_objs.extend(page.bell_transferables()?);
        let page = Arc::new(page);
        // Reader and reaper for this incarnation.
        let tx = self.self_tx.clone();
        let blob = self.blob_gate.clone();
        std::thread::Builder::new()
            .name(format!("{}-read", self.cfg.names.thread))
            .spawn(move || reader(rsock, inc, tx, blob))?;
        let tx = self.self_tx.clone();
        let reaped = process.clone();
        std::thread::Builder::new()
            .name(format!("{}-reap", self.cfg.names.thread))
            .spawn(move || {
                if let Ok(Some(st)) = reaped.wait(None) {
                    let _ = tx.send(Inbound::Died(inc, st));
                }
            })?;
        self.counters.child_pid.store(pid, Ordering::Relaxed);
        let now = self.now();
        self.child = Some(Child {
            pid,
            process,
            incarnation: inc,
            sock: ours,
            page,
            up_at: now,
            hello_ok: false,
            hello_by: now + self.cfg.hello_deadline,
            hello_sent: std::time::Instant::now(),
            spawn_started,
            dead: false,
            mach_rx,
            mach_a: None,
            child_port: None,
            unconfirmed: BTreeSet::new(),
            kill_reason: None,
            inflight: BTreeMap::new(),
            queue: VecDeque::new(),
            hold_seen: 0,
        });
        let hello = codec::Hello::new(self.cfg.child_id.clone(), page_objs.len() as u32);
        self.request(kind::HELLO, &hello, &[], page_objs, Pending::Hello);
        // H-2: the deadline runs from the hello's send (`request` writes it at once; a stall there is a Stall kill).
        let sent = self.now();
        if let Some(c) = &mut self.child {
            c.hello_by = sent + self.cfg.hello_deadline;
            c.hello_sent = std::time::Instant::now();
        }
        // Nothing else until the reply is verified ([`Supervisor::on_hello`] then loads every chain).
        if !WAIT_FOR_HELLO {
            self.load_all();
        }
        Ok(())
    }

    /// Every chain loads on this child, in order, from the table (D7 re-apply): `chain.load`, its states, then
    /// `chain.prepare`.
    fn load_all(&mut self) {
        let keys: Vec<u64> = self.chains.keys().copied().collect();
        for k in keys {
            self.load_chain(k);
        }
    }

    /// The live child may be sent more than `hello` (alive, and its `hello` reply verified). Also
    /// the one test that replaced the 3.x minor gates (`log.set`, `corr`, `param.set`): protocol 4.0 carries them all.
    fn verified(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|c| !c.dead && (c.hello_ok || !WAIT_FOR_HELLO))
    }

    fn load_chain(&mut self, key: u64) {
        let Some(ch) = self.chains.get(&key) else {
            return;
        };
        let spec = ch.spec.clone();
        let flags = region_flags(spec.role);
        let (region, fd) = match Region::create(spec.geometry, key, self.cfg.sample_rate, flags) {
            Ok(x) => x,
            Err(e) => {
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!("{stderr_tag}: region for chain {key}: {e}");
                return;
            }
        };
        // The region, then its doorbells' objects (none on unix: one object, `fds: 1`, as ever).
        let mut region_objs = vec![fd];
        match region.bell_transferables() {
            Ok(b) => region_objs.extend(b),
            Err(e) => {
                let stderr_tag = self.cfg.names.stderr;
                runecraft_rt::safe_eprintln!("{stderr_tag}: region doorbells for chain {key}: {e}");
                return;
            }
        }
        let region = Arc::new(region);
        let g = spec.geometry;
        let slots: Vec<codec::SlotDesc> = spec
            .slots
            .iter()
            .enumerate()
            .map(|(i, x)| {
                let enabled = spec.enabled.get(i).copied().unwrap_or(true);
                let quarantined = self.quarantine.contains(&(key, i as u32));
                codec::SlotDesc::new(if quarantined { None } else { x.clone() }, enabled)
            })
            .collect();
        let sent_enabled: Vec<bool> = slots.iter().map(|s: &codec::SlotDesc| s.enabled).collect();
        if let Some(ch) = self.chains.get_mut(&key) {
            ch.sent_enabled = sent_enabled;
        }
        let msg = {
            let mut msg = codec::ChainLoad::default();
            msg.chain_key = key;
            msg.instance_id = region.instance_id();
            msg.role = spec.role;
            msg.group = spec.group as u32;
            msg.rate = self.cfg.sample_rate;
            msg.max_frames = g.max_frames;
            msg.ch_in = g.ch_in;
            msg.ch_out = g.ch_out;
            msg.max_midi = g.max_midi;
            msg.max_params = g.max_params;
            msg.tap_slot = g.tap_slot;
            msg.slots = slots;
            msg.fds = region_objs.len() as u32;
            // Windows only (4.1, additive): this chain's FanOut slot. Never written on unix.
            msg.fan_slot = None;
            msg
        };
        let now = self.now();
        let ch = self.chains.get_mut(&key).expect("present");
        ch.region = Some(region);
        ch.loaded = false;
        self.publish_wired();
        let ch = self.chains.get_mut(&key).expect("present");
        ch.ready = false;
        ch.down = false;
        ch.wedge = Wedge {
            progress_at: now,
            ..Default::default()
        };
        self.request(kind::CHAIN_LOAD, &msg, &[], region_objs, Pending::Receipt);
        // Then the states, from the model's blobs, in `sasp` mode (D7, D21), then prepare. A blob that fails the frame
        // check is never sent: the slot loads with no state applied (and nothing waits on its confirmation).
        let mut sets = Vec::new();
        let ch = self.chains.get_mut(&key).expect("present");
        ch.awaiting_states = 0;
        for (i, s) in ch.slots.iter_mut().enumerate() {
            s.applied = false;
            if let Some(st) = &s.state
                && !self.quarantine.contains(&(key, i as u32))
            {
                sets.push(st.clone());
            }
        }
        let mut sent = 0;
        for st in sets {
            if !self.send_state(key, &st) {
                continue;
            }
            sent += 1;
            if let Some(c) = &mut self.child
                && self.incarnation > 1
            {
                c.unconfirmed.insert((key, st.slot, st.generation));
            }
        }
        if let Some(ch) = self.chains.get_mut(&key) {
            ch.awaiting_states = sent;
        }
        self.request(
            kind::CHAIN_PREPARE,
            &codec::ChainRef::new(key, true),
            &[],
            Vec::new(),
            Pending::Receipt,
        );
    }

    /// `state.set {mode: sasp}` (D21: the one production mode). The package blob is checked first: a refused one is
    /// never sent (counted, [`Event::StateRefused`]), and false is returned. A both-absent vst3 blob is sent and
    /// counted (`stateEmptySets`): the child answers it `ok: true` without touching the plugin.
    fn send_state(&mut self, key: u64, st: &SlotState) -> bool {
        let checked = state_blob::check(&st.blob);
        // The mutation check (never in a normal build): the package blob's frame check is skipped, so a refused blob
        // is sent (e2e_state `a_bare_package_blob_is_never_sent_…` fails).
        #[cfg(mutant_sasp_package_unchecked)]
        let checked = checked.or_else(|_| state_blob::check(&state_blob::builtin(&[])));
        match checked {
            Ok(c) => {
                if c.both_absent {
                    self.counters
                        .state_empty_sets
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Err(reason) => {
                self.counters
                    .state_format_refused
                    .fetch_add(1, Ordering::Relaxed);
                self.emit(Event::StateRefused {
                    key,
                    slot: st.slot,
                    generation: st.generation,
                    source: StateSource::Package,
                    reason,
                });
                return false;
            }
        }
        let msg = codec::StateSet::new(key, st.slot, st.generation, codec::StateMode::V1);
        let blob = st.blob.clone();
        self.request(kind::STATE_SET, &msg, &blob, Vec::new(), Pending::Receipt);
        true
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Writing (the only socket writer)
    // -----------------------------------------------------------------------------------------------------------------

    fn request<T: serde::Serialize>(
        &mut self,
        k: u16,
        meta: &T,
        blob: &[u8],
        objs: Vec<runecraft_ipc::channel::Transferable>,
        pending: Pending,
    ) {
        let Some(c) = &mut self.child else { return };
        if c.dead {
            return;
        }
        // The one choke point: before the verified hello, `hello` is the only frame this incarnation is
        // sent. Anything else is dropped here, as during a backoff (its descriptor, if any, is closed); the verified
        // hello then loads every chain from the table.
        if WAIT_FOR_HELLO && !c.hello_ok && k != kind::HELLO {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let flags = codec::handles_flag(objs.len());
        let mut m = serde_json::to_vec(meta).expect("meta serialises");
        // the command's `corr`, to a verified child, through the one splice. A refusal (the meta at its cap,
        // a meta-less frame) sends the frame without it, counted.
        // (`c` is the live child; verified as in [`Self::verified`].)
        let verified = c.hello_ok || !WAIT_FOR_HELLO;
        if let Some(corr) = self.corr.as_deref()
            && verified
        {
            let n = if codec::with_corr(&mut m, corr).is_ok() {
                &self.counters.corr_sent
            } else {
                &self.counters.corr_dropped
            };
            n.fetch_add(1, Ordering::Relaxed);
        }
        let frame = codec::encode(k, flags, id, &m, blob);
        c.queue.push_back(Outgoing {
            frame,
            // The region or page, given read-write (the child maps it shared), and its doorbells' objects.
            objs,
            pending: Some(pending),
        });
        self.flush();
    }

    fn flush(&mut self) {
        let mut stalled = false;
        let send_timeout = self.cfg.send_timeout;
        if let Some(c) = &mut self.child {
            while c.inflight.len() < IN_FLIGHT
                && !c.dead
                && let Some(o) = c.queue.pop_front()
            {
                let id = u64::from_le_bytes(o.frame[F_ID..F_ID + 8].try_into().expect("8"));
                // Each frame's send window is the configured send timeout from now (on unix each
                // blocking write of it waits at most that long, as the fixed socket timeout did).
                let by = std::time::Instant::now() + send_timeout;
                let r = match o.objs.as_slice() {
                    [] => c.sock.send(&o.frame, &[], by),
                    [t] => c.sock.send(&o.frame, &[t], by),
                    more => c.sock.send(&o.frame, &more.iter().collect::<Vec<_>>(), by),
                };
                match r {
                    Ok(()) => {
                        if let Some(p) = o.pending {
                            c.inflight.insert(id, p);
                        }
                    }
                    Err(e) => {
                        // A stall past SO_SNDTIMEO (2 s) is a wedged child; any other error is a dead one (D6).
                        stalled = matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        );
                        break;
                    }
                }
            }
        }
        if stalled {
            self.kill(DownReason::Stall);
        }
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------------------------------------------------

    /// Returns true to stop the loop.
    fn command(&mut self, c: Command) -> bool {
        let now = self.now();
        match c {
            Command::Load { key, spec, states } => {
                // A key is loaded once. A second `Load` for a hosted key
                // would replace the table entry while the child still runs the first chain; refused, counted and
                // logged, the hosted chain untouched. `--cfg mutant_load_live_key` (never in a normal build)
                // takes it; the editor tests fail.
                if self.chains.contains_key(&key) && !cfg!(mutant_load_live_key) {
                    self.counters
                        .loads_refused_live_key
                        .fetch_add(1, Ordering::Relaxed);
                    let stderr_tag = self.cfg.names.stderr;
                    runecraft_rt::safe_eprintln!(
                        "{stderr_tag}: chain.load for key {key:#x} refused: that key is already a hosted chain (a key is \
                         loaded once; unload it first)"
                    );
                    return false;
                }
                let n = spec.slots.len().min(MAX_SLOTS);
                let mut slots: Vec<SlotEntry> = (0..n)
                    .map(|_| SlotEntry {
                        state: None,
                        applied: false,
                        child_gen: 0,
                        read_gen: 0,
                        editor_open: false,
                        has_editor: true,
                        last_read_at: now,
                        name: String::new(),
                        loaded_ok: false,
                        notices: Vec::new(),
                        latency: 0,
                        param_count: 0,
                        reactivations: 0,
                        bypassed_blocks: 0,
                        stalls: 0,
                        partial: false,
                        remarked: false,
                        editor_seen: false,
                    })
                    .collect();
                for st in states {
                    if let Some(s) = slots.get_mut(st.slot as usize) {
                        s.state = Some(st);
                    }
                }
                let depth = DepthController::new(self.cfg.depth_policy, spec.depth, now);
                self.chains.insert(
                    key,
                    Chain {
                        spec,
                        slots,
                        region: None,
                        loaded: false,
                        ready: false,
                        awaiting_states: 0,
                        parked: false,
                        down: false,
                        latency: 0,
                        wedge: Wedge::default(),
                        depth,
                        sent_enabled: Vec::new(),
                        wired_bytes: 0,
                        unwired_bytes: 0,
                    },
                );
                // Before the verified hello the table is updated and nothing is sent (as in a backoff).
                if self.verified() {
                    self.load_chain(key);
                }
            }
            Command::SetSampleRate(r) => {
                if r.is_finite() && r > 0.0 {
                    self.cfg.sample_rate = r;
                }
            }
            Command::SetEnabled { key, slot, enabled } => {
                let Some(ch) = self.chains.get_mut(&key) else {
                    return false;
                };
                let n = ch.spec.slots.len();
                if (slot as usize) < n {
                    if ch.spec.enabled.len() < n {
                        ch.spec.enabled.resize(n, true);
                    }
                    ch.spec.enabled[slot as usize] = enabled;
                    let loaded = ch.loaded;
                    if loaded {
                        self.request(
                            kind::SLOT_SET_ENABLED,
                            &codec::SlotEnabled::new(key, slot, enabled),
                            &[],
                            Vec::new(),
                            Pending::SlotEnabled,
                        );
                    }
                }
            }
            Command::Unload { key } => {
                if self.chains.remove(&key).is_some() {
                    self.publish_wired();
                    self.request(
                        kind::CHAIN_UNLOAD,
                        &codec::ChainRef::new(key, true),
                        &[],
                        Vec::new(),
                        Pending::Receipt,
                    );
                }
            }
            Command::SetState { key, state } => {
                if let Some(ch) = self.chains.get_mut(&key)
                    && let Some(s) = ch.slots.get_mut(state.slot as usize)
                {
                    s.state = Some(state.clone());
                    s.applied = false;
                    // Before the verified hello only the table changes; the hello's load pass sends it.
                    if !self.quarantine.contains(&(key, state.slot)) && self.verified() {
                        let _ = self.send_state(key, &state);
                    }
                }
            }
            Command::Park { key } => {
                if let Some(ch) = self.chains.get_mut(&key) {
                    ch.parked = true;
                }
                self.request(
                    kind::CHAIN_PARK,
                    &codec::ChainRef::new(key, true),
                    &[],
                    Vec::new(),
                    Pending::Receipt,
                );
            }
            Command::Unpark { key, group } => {
                if let Some(ch) = self.chains.get_mut(&key) {
                    ch.spec.group = group;
                }
                self.request(
                    kind::CHAIN_UNPARK,
                    &codec::ChainUnpark::new(key, group as u32),
                    &[],
                    Vec::new(),
                    Pending::Receipt,
                );
            }
            Command::GetState { key, slot } => {
                self.request(
                    kind::STATE_GET,
                    &codec::StateGet::new(key, slot, false),
                    &[],
                    Vec::new(),
                    Pending::StateGet {
                        key,
                        slot,
                        burst: None,
                    },
                );
            }
            Command::ParamList {
                key,
                slot,
                offset,
                count,
            } => {
                let count = count.min(PARAM_PAGE);
                self.request(
                    kind::PARAM_LIST,
                    &codec::ParamList::new(key, slot, offset, count),
                    &[],
                    Vec::new(),
                    Pending::ParamList {
                        key,
                        slot,
                        offset,
                        count,
                    },
                );
            }
            Command::PrepareFresh { key } => {
                let render = self
                    .chains
                    .get(&key)
                    .is_some_and(|c| c.spec.role == codec::Role::Render);
                // every verified 4.x child takes kind 25 (its gate went with R3's).
                if !render {
                    self.emit(Event::PreparedFresh {
                        slots: Vec::new(),
                        notices_known: false,
                        key,
                        ok: false,
                        ms: 0.0,
                        latency: 0,
                        error: Some("not a render set".into()),
                    });
                } else {
                    self.request(
                        kind::CHAIN_PREPARE_FRESH,
                        &codec::ChainRef::new(key, true),
                        &[],
                        Vec::new(),
                        Pending::Receipt,
                    );
                }
            }
            Command::ParamSet {
                key,
                slot,
                plugin_id,
                value,
            } => {
                if !self.verified() || !value.is_finite() {
                    self.emit(Event::ParamSet {
                        key,
                        slot,
                        plugin_id,
                        ok: false,
                        value: 0.0,
                    });
                } else {
                    self.request(
                        kind::PARAM_SET,
                        &codec::ParamSet::new(key, slot, plugin_id, value.clamp(0.0, 1.0)),
                        &[],
                        Vec::new(),
                        Pending::ParamSet {
                            key,
                            slot,
                            plugin_id,
                        },
                    );
                }
            }
            Command::EditorOpen {
                key,
                slot,
                guard_reset,
            } => {
                // A render set never opens an editor, and a key the supervisor does not host has nothing to open.
                // `--cfg mutant_editor_open_render` (never in a normal build) lets a render chain's through;
                // the editor tests fail.
                let hosted = self.chains.get(&key).filter(|c| {
                    c.spec.role != codec::Role::Render || cfg!(mutant_editor_open_render)
                });
                if hosted.is_none_or(|c| slot as usize >= c.slots.len()) {
                    self.counters
                        .editor_opens_refused
                        .fetch_add(1, Ordering::Relaxed);
                    // Control hears that no window opened (it recorded the open when the send was taken).
                    self.emit(Event::EditorUnavailable { key, slot });
                    return false;
                }
                if let Some(s) = self
                    .chains
                    .get_mut(&key)
                    .and_then(|c| c.slots.get_mut(slot as usize))
                {
                    s.editor_open = true;
                    s.editor_seen = true;
                }
                self.next_poll = now + self.cfg.poll_interval;
                self.request(
                    kind::EDITOR_OPEN,
                    &codec::EditorRef::new(key, Some(slot), guard_reset),
                    &[],
                    Vec::new(),
                    Pending::EditorOpen { key, slot },
                );
            }
            Command::EditorClose { key, slot } => {
                if let Some(ch) = self.chains.get_mut(&key) {
                    for (i, s) in ch.slots.iter_mut().enumerate() {
                        if slot.is_none_or(|x| x as usize == i) {
                            s.editor_open = false;
                        }
                    }
                }
                self.poll_until = now + self.cfg.poll_tail;
                self.request(
                    kind::EDITOR_CLOSE,
                    &codec::EditorRef::new(key, slot, false),
                    &[],
                    Vec::new(),
                    Pending::Receipt,
                );
            }
            Command::SaveBurst {
                token,
                budget,
                bytes,
                remark,
            } => self.start_burst(token, budget, bytes, remark),
            Command::Remark { key, slot } => self.remark(key, slot),
            Command::AttachWorkgroup { port } => {
                let token = self.nonce();
                let ready = self.child.as_ref().is_some_and(|c| c.child_port.is_some());
                if ready {
                    self.request(
                        kind::WORKGROUP_ATTACH,
                        &codec::WorkgroupAttach::new(token, None),
                        &[],
                        Vec::new(),
                        Pending::Receipt,
                    );
                    if let Some(c) = &self.child
                        && let Some(p) = &c.child_port
                        && p.send(token, Some(&port), 1000).is_err()
                    {
                        self.emit(Event::WorkgroupAttached {
                            token,
                            ok: false,
                            joined: false,
                        });
                    }
                } else {
                    self.counters.workgroup_route.store(3, Ordering::Relaxed);
                    self.emit(Event::WorkgroupAttached {
                        token,
                        ok: false,
                        joined: false,
                    });
                }
            }
            Command::Boundary => {
                let keys: Vec<u64> = self.chains.keys().copied().collect();
                for k in keys {
                    let ch = self.chains.get_mut(&k).expect("present");
                    if let Some(DepthChange::Lower) = ch.depth.at_boundary(now) {
                        self.depth_event(k, DepthChange::Lower);
                    }
                }
            }
            Command::RestartHost => {
                self.quarantine.clear();
                self.counters.chains_quarantined.store(0, Ordering::Relaxed);
                self.attempts = 0;
                self.exhausted = false;
                self.last_attribution = None;
                self.attribution_run = 0;
                self.counters.attribution_run.store(0, Ordering::Relaxed);
                if self.child.as_ref().is_some_and(|c| !c.dead) {
                    self.kill(DownReason::Crash);
                }
                self.attempts = 0;
                self.respawn_at = Some(now);
            }
            Command::Tick => {}
            Command::Barrier(tx) => self.barrier = Some(tx),
            Command::Shutdown => {
                self.shutting_down = true;
                // A child whose hello is not verified is sent nothing and runs no plugin: `run` kills it.
                if self.child.as_ref().is_some_and(|c| !c.dead && c.hello_ok) {
                    self.request(
                        kind::SHUTDOWN,
                        &serde_json::json!({}),
                        &[],
                        Vec::new(),
                        Pending::Shutdown,
                    );
                    // The reaper's report ends the loop; past 2 s the child is killed (`run`).
                    self.shutdown_by = Some(std::time::Instant::now() + Duration::from_secs(2));
                    return false;
                }
                return true;
            }
        }
        false
    }

    /// A save burst. The slots known to have moved (the poll, a partial set, a remark) are
    /// queued, the generations poll finds the rest; reads go out at most [`BURST_WINDOW`] at a time until the byte
    /// budget is reached, and the burst answers once every read is answered, or at its time budget, whichever is
    /// first. A running burst is superseded: answered `superseded`, its unanswered slots carried into this one.
    fn start_burst(
        &mut self,
        token: u64,
        budget: Duration,
        byte_budget: u64,
        remark: Vec<(u64, u32)>,
    ) {
        let mut carried: Vec<(u64, u32)> = Vec::new();
        if let Some(old) = self.burst.take() {
            carried.extend(
                old.requested
                    .iter()
                    .filter(|x| !old.answered.contains(x))
                    .chain(old.queue.iter())
                    .copied(),
            );
            self.emit(Event::Burst {
                token: old.token,
                fresh: old.fresh,
                stale: Vec::new(),
                too_large: old.too_large,
                superseded: true,
                incarnation: self.incarnation,
            });
        }
        for (k, sl) in remark {
            self.remark(k, sl);
        }
        let deadline = self.now() + budget;
        let mut queue = VecDeque::new();
        for (&k, ch) in &self.chains {
            for (i, s) in ch.slots.iter().enumerate() {
                if s.child_gen != s.read_gen || s.partial || s.remarked {
                    queue.push_back((k, i as u32));
                }
            }
        }
        for x in carried {
            if !queue.contains(&x)
                && self
                    .chains
                    .get(&x.0)
                    .is_some_and(|c| (x.1 as usize) < c.slots.len())
            {
                queue.push_back(x);
            }
        }
        self.burst = Some(Burst {
            token,
            deadline,
            byte_budget,
            bytes: 0,
            requested: BTreeSet::new(),
            queue,
            answered: BTreeSet::new(),
            fresh: Vec::new(),
            too_large: Vec::new(),
            generations_answered: false,
        });
        self.request(
            kind::STATE_GENERATIONS,
            &serde_json::json!({}),
            &[],
            Vec::new(),
            Pending::Generations { burst: Some(token) },
        );
        self.pump_burst();
    }

    /// E5: the next burst reads this slot again.
    fn remark(&mut self, key: u64, slot: u32) {
        if let Some(s) = self
            .chains
            .get_mut(&key)
            .and_then(|c| c.slots.get_mut(slot as usize))
            && !cfg!(mutant_e_remark_ignored)
        {
            s.remarked = true;
        }
    }

    /// Send the burst's next reads (the window, the byte budget), then answer it if nothing is left to wait for.
    fn pump_burst(&mut self) {
        loop {
            let Some(b) = &mut self.burst else { return };
            let in_flight = b.requested.len() - b.requested.intersection(&b.answered).count();
            // `--cfg mutant_e_no_byte_budget` (never in a normal build): the byte budget is ignored.
            let over = b.bytes >= b.byte_budget && !cfg!(mutant_e_no_byte_budget);
            if over || in_flight >= BURST_WINDOW {
                break;
            }
            let Some(x) = b.queue.pop_front() else { break };
            if !b.requested.insert(x) {
                continue;
            }
            let token = b.token;
            self.burst_get(token, x.0, x.1);
        }
        let done = self.burst.as_ref().is_some_and(|b| {
            let over = b.bytes >= b.byte_budget && !cfg!(mutant_e_no_byte_budget);
            b.generations_answered
                && (b.queue.is_empty() || over)
                && b.requested.iter().all(|x| b.answered.contains(x))
        });
        if done {
            self.finish_burst();
        }
    }

    /// A burst read was answered (any answer): counted, and the burst goes on.
    fn burst_answered(&mut self, token: Option<u64>, key: u64, slot: u32) {
        if let (Some(t), Some(b)) = (token, &mut self.burst)
            && b.token == t
        {
            b.answered.insert((key, slot));
            self.pump_burst();
        }
    }

    fn burst_get(&mut self, token: u64, key: u64, slot: u32) {
        self.request(
            kind::STATE_GET,
            &codec::StateGet::new(key, slot, false),
            &[],
            Vec::new(),
            Pending::StateGet {
                key,
                slot,
                burst: Some(token),
            },
        );
    }

    fn finish_burst(&mut self) {
        let Some(b) = self.burst.take() else { return };
        let taken = |k: u64, s: u32| {
            b.fresh.iter().any(|f| f.key == k && f.slot == s)
                || b.too_large.iter().any(|f| f.0 == k && f.1 == s)
        };
        let mut stale: Vec<(u64, u32)> = b
            .requested
            .iter()
            .chain(b.queue.iter())
            .filter(|(k, s)| !taken(*k, *s))
            .copied()
            .collect();
        stale.sort_unstable();
        stale.dedup();
        if !b.generations_answered {
            // A dead or wedged reader: every slot with an open editor is stale (D6).
            for (&k, ch) in &self.chains {
                for (i, s) in ch.slots.iter().enumerate() {
                    if s.editor_open && !stale.contains(&(k, i as u32)) {
                        stale.push((k, i as u32));
                    }
                }
            }
        }
        self.counters
            .state_stale
            .fetch_add(stale.len() as u64, Ordering::Relaxed);
        self.counters
            .state_stale_now
            .store(stale.len() as u32, Ordering::Relaxed);
        self.emit(Event::Burst {
            token: b.token,
            fresh: b.fresh,
            stale,
            too_large: b.too_large,
            superseded: false,
            incarnation: self.incarnation,
        });
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Frames from the child
    // -----------------------------------------------------------------------------------------------------------------

    fn frame(&mut self, f: Frame) {
        if let Err(e) = codec::check_to_core(&f) {
            // A protocol fault (and H-3: an undecodable `hello` reply is refused here, before `on_hello`).
            let h = f.header;
            self.protocol_fault(&format!(
                "a {} {} (id {}) the core refuses: {e}",
                codec::kind_name(h.kind).unwrap_or("?"),
                frame_word(&h),
                h.id
            ));
            return;
        }
        // the child's echo of a request's `corr`, read leniently (never a fault), scopes this frame's records.
        let echo = self.echo_of(&f);
        let outer = std::mem::replace(&mut self.echo, echo);
        self.frame_echoed(f);
        self.echo = outer;
    }

    fn frame_echoed(&mut self, f: Frame) {
        let h = f.header;
        if h.is_reply() {
            let p = self.child.as_mut().and_then(|c| c.inflight.remove(&h.id));
            self.flush();
            match p {
                Some(Pending::Hello) => self.on_hello(&f),
                Some(Pending::Generations { burst }) => self.on_generations(&f, burst),
                Some(Pending::StateGet { key, slot, burst }) => {
                    self.on_state_get(&f, key, slot, burst)
                }
                Some(Pending::ReloadRead { key, slot }) => self.on_reload_read(&f, key, slot),
                Some(Pending::ParamList {
                    key,
                    slot,
                    offset,
                    count,
                }) => self.on_param_list(&f, key, slot, offset, count),
                Some(Pending::ParamSet {
                    key,
                    slot,
                    plugin_id,
                }) => self.on_param_set(&f, key, slot, plugin_id),
                Some(Pending::LogSet { levels }) => self.on_log_set(&f, levels),
                Some(Pending::EditorOpen { key, slot }) => {
                    // Read leniently: a receipt without the key (every unix child's `{}`), or one this host cannot
                    // read, opens nothing for the host.
                    if let Ok(codec::EditorOpened {
                        editor_window: Some(window),
                        ..
                    }) = f.meta_as::<codec::EditorOpened>()
                    {
                        let incarnation = self.incarnation;
                        self.emit(Event::EditorWindow {
                            key,
                            slot,
                            window,
                            incarnation,
                        });
                    }
                }
                Some(Pending::SlotEnabled) => {
                    if let Ok(m) = f.meta_as::<codec::SlotEnabledReply>()
                        && !m.ok
                    {
                        let stderr_tag = self.cfg.names.stderr;
                        runecraft_rt::safe_eprintln!(
                            "{stderr_tag}: slot.setEnabled chain {} slot {} answered ok: false (no such chain or slot)",
                            m.chain_key,
                            m.slot
                        );
                    }
                }
                Some(_) => {}
                None => {
                    // A reply to nothing: a protocol fault.
                    self.protocol_fault(&format!(
                        "a {} reply to nothing (id {})",
                        codec::kind_name(h.kind).unwrap_or("?"),
                        h.id
                    ));
                }
            }
            return;
        }
        match h.kind {
            kind::CHAIN_LOADED => {
                let Ok(m) = f.meta_as::<codec::ChainLoaded>() else {
                    return;
                };
                // A toggle made while the chain loaded reaches the child now.
                let resend: Vec<(u32, bool)> = match self.chains.get(&m.chain_key) {
                    Some(ch) if m.ok => ch
                        .spec
                        .enabled
                        .iter()
                        .enumerate()
                        .filter(|(i, e)| ch.sent_enabled.get(*i).copied().unwrap_or(true) != **e)
                        .map(|(i, e)| (i as u32, *e))
                        .collect(),
                    _ => Vec::new(),
                };
                for (slot, enabled) in resend {
                    self.request(
                        kind::SLOT_SET_ENABLED,
                        &codec::SlotEnabled::new(m.chain_key, slot, enabled),
                        &[],
                        Vec::new(),
                        Pending::SlotEnabled,
                    );
                }
                if let Some(ch) = self.chains.get_mut(&m.chain_key) {
                    ch.wired_bytes = if m.ok { m.wired_bytes } else { 0 };
                    ch.unwired_bytes = if m.ok { m.unwired_bytes } else { 0 };
                }
                if self.chains.contains_key(&m.chain_key) {
                    self.chains.get_mut(&m.chain_key).expect("present").loaded = m.ok;
                    self.publish_wired();
                }
                if let Some(ch) = self.chains.get_mut(&m.chain_key) {
                    // D7: a quarantined slot is an empty pass-through that keeps its number; it adds no
                    // latency and no parameter, whatever the child reports for it.
                    let mut q_latency = 0u32;
                    for (i, s) in ch.slots.iter_mut().enumerate() {
                        let info = m.slots.get(i);
                        let quarantined = self.quarantine.contains(&(m.chain_key, i as u32));
                        s.name = info
                            .map(|x| x.name.chars().take(64).collect())
                            .unwrap_or_default();
                        s.loaded_ok = info.is_some_and(|x| x.ok) && !quarantined;
                        // Protocol 3.4: absent (a 3.3 child) decodes true.
                        s.has_editor = s.loaded_ok && info.is_some_and(|x| x.has_editor);
                        // (3.9): absent (a 3.8 child) is none.
                        s.notices = match info {
                            Some(x) if s.loaded_ok => kept_notices(&x.notices),
                            _ => Vec::new(),
                        };
                        let (lat, pc) = info
                            .map(|x| (x.latency.min(1 << 20), x.param_count.min(16_384)))
                            .unwrap_or((0, 0));
                        if quarantined {
                            q_latency = q_latency.saturating_add(lat);
                            s.latency = 0;
                            s.param_count = 0;
                        } else {
                            s.latency = lat;
                            s.param_count = pc;
                        }
                    }
                    ch.latency = m.latency.saturating_sub(q_latency).min(1 << 20);
                }
            }
            kind::STATE_APPLIED => {
                let Ok(m) = f.meta_as::<codec::StateApplied>() else {
                    return;
                };
                let mut ok_gen = false;
                if let Some(ch) = self.chains.get_mut(&m.chain_key)
                    && let Some(s) = ch.slots.get_mut(m.slot as usize)
                {
                    // Anything older than the pending gen is ignored (D6).
                    if s.state
                        .as_ref()
                        .is_some_and(|x| x.generation == m.generation)
                    {
                        s.applied = m.ok;
                        ok_gen = true;
                        ch.awaiting_states = ch.awaiting_states.saturating_sub(1);
                    }
                }
                if !m.ok {
                    self.counters
                        .state_apply_failures
                        .fetch_add(1, Ordering::Relaxed);
                }
                // A partial vst3 set changed the plugin; it is never "nothing changed". Counted, and the
                // slot is reported moved (control marks it dirty; the next burst reads it). An invalid `applied`
                // (N12: any combination but ok:false with exactly one chunk) is a protocol error, counted and
                // logged, and the slot is reported moved too (something may have changed).
                let kind = m.classify();
                let partial = kind == codec::AppliedKind::Partial;
                let mut moved = None;
                if kind == codec::AppliedKind::Invalid {
                    self.counters
                        .state_applied_invalid
                        .fetch_add(1, Ordering::Relaxed);
                    let stderr_tag = self.cfg.names.stderr;
                    runecraft_rt::safe_eprintln!(
                        "{stderr_tag}: chain {} slot {}: state.applied with an invalid `applied` {:?} (ok {}): a protocol \
                         error; the slot is reported moved",
                        m.chain_key,
                        m.slot,
                        m.applied,
                        m.ok
                    );
                }
                if kind != codec::AppliedKind::Whole {
                    if partial {
                        self.counters
                            .state_partial_sets
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if let Some(s) = self
                        .chains
                        .get_mut(&m.chain_key)
                        .and_then(|c| c.slots.get_mut(m.slot as usize))
                    {
                        s.partial = true;
                        moved = Some((m.chain_key, m.slot, s.child_gen));
                    }
                }
                if ok_gen {
                    if m.ok
                        && let Some(c) = &mut self.child
                    {
                        c.unconfirmed.remove(&(m.chain_key, m.slot, m.generation));
                    }
                    // A whole set's child generation is the host application's own state, so the poll does
                    // not report it moved (a 3.3 child; a 3.2 child's set is read back as before, and listed stale).
                    if m.ok
                        && kind == codec::AppliedKind::Whole
                        && let Some(g) = m.child_generation
                        && let Some(s) = self
                            .chains
                            .get_mut(&m.chain_key)
                            .and_then(|c| c.slots.get_mut(m.slot as usize))
                    {
                        s.child_gen = s.child_gen.max(g);
                        s.read_gen = s.read_gen.max(g);
                    }
                    self.emit(Event::StateApplied {
                        key: m.chain_key,
                        slot: m.slot,
                        generation: m.generation,
                        ok: m.ok,
                        child_generation: m.child_generation,
                        applied: if partial { m.applied } else { None },
                    });
                }
                if let Some(x) = moved {
                    self.emit(Event::Moved { slots: vec![x] });
                }
            }
            kind::CHAIN_PREPARED => {
                let Ok(m) = f.meta_as::<codec::ChainPrepared>() else {
                    return;
                };
                if !m.ok {
                    return;
                }
                let changed = self.replace_notices(m.chain_key, &m.slot_notices);
                // a 3.6 child's pair after the prepare replaces the chain's; absent keeps `chain.loaded`'s.
                // `--cfg mutant_prepared_absent_zero` (never in a normal build) reads absent as 0; child_wired.rs's
                // 3.5 prepare test fails.
                let zero = cfg!(mutant_prepared_absent_zero).then_some(0);
                if let Some(ch) = self.chains.get_mut(&m.chain_key) {
                    if let Some(w) = m.wired_bytes.or(zero) {
                        ch.wired_bytes = w;
                    }
                    if let Some(u) = m.unwired_bytes.or(zero) {
                        ch.unwired_bytes = u;
                    }
                    self.publish_wired();
                }
                if changed
                    && self.chains.get(&m.chain_key).is_some_and(|c| c.ready)
                    && !cfg!(mutant_notice_updates_dropped)
                {
                    self.emit(Event::SlotNoticesChanged {
                        key: m.chain_key,
                        incarnation: self.incarnation,
                        slots: self.notice_slots(m.chain_key),
                    });
                }
                self.maybe_ready(m.chain_key);
            }
            kind::CHAIN_PREPARED_FRESH => {
                let Ok(m) = f.meta_as::<codec::ChainPreparedFresh>() else {
                    return;
                };
                let ms = if m.ms.is_finite() { m.ms.max(0.0) } else { 0.0 };
                let latency = m.latency.min(1 << 20);
                if m.ok
                    && let Some(v) = &m.slot_notices
                {
                    self.replace_notices(m.chain_key, v);
                }
                let mut slots = self.notice_slots(m.chain_key);
                if m.slot_notices.is_none() {
                    for s in &mut slots {
                        s.notices.clear();
                    }
                }
                self.emit(Event::PreparedFresh {
                    slots,
                    notices_known: m.slot_notices.is_some(),
                    key: m.chain_key,
                    ok: m.ok,
                    ms,
                    latency,
                    error: m.error,
                });
            }
            kind::CHAIN_PARKED => {
                if let Ok(m) = f.meta_as::<codec::ChainRef>() {
                    self.emit(Event::Parked { key: m.chain_key });
                }
            }
            kind::CHAIN_UNPARKED => {
                if let Ok(m) = f.meta_as::<codec::ChainUnpark>() {
                    if let Some(ch) = self.chains.get_mut(&m.chain_key) {
                        ch.parked = false;
                    }
                    self.emit(Event::Unparked {
                        key: m.chain_key,
                        group: m.group.min(3) as u8,
                    });
                }
            }
            kind::LATENCY_CHANGED => {
                if let Ok(m) = f.meta_as::<codec::LatencyChanged>()
                    && let Some(ch) = self.chains.get_mut(&m.chain_key)
                {
                    ch.latency = m.latency.min(1 << 20);
                    if let Some(link) = self.link_of(m.chain_key) {
                        self.emit(Event::LatencyChanged {
                            key: m.chain_key,
                            link,
                        });
                    }
                }
            }
            kind::EDITOR_CLOSED => {
                if let Ok(m) = f.meta_as::<codec::EditorClosed>() {
                    self.read_arrived(m.chain_key, m.slot, m.child_gen, f.blob, None);
                    // the window is gone (a close the host application did not ask for included: the close button).
                    if let Some(s) = self
                        .chains
                        .get_mut(&m.chain_key)
                        .and_then(|c| c.slots.get_mut(m.slot as usize))
                    {
                        s.editor_open = false;
                        self.emit(Event::EditorClosed {
                            key: m.chain_key,
                            slot: m.slot,
                        });
                    }
                }
            }
            kind::EDITOR_AVAILABILITY => {
                // was dropped (`_ => {}`) before. An unknown chain or slot is ignored (the child is untrusted).
                if let Ok(m) = f.meta_as::<codec::EditorRef>() {
                    let slot = m.slot.unwrap_or(0);
                    if let Some(s) = self
                        .chains
                        .get_mut(&m.chain_key)
                        .and_then(|c| c.slots.get_mut(slot as usize))
                    {
                        s.editor_open = false;
                        s.has_editor = false;
                        self.emit(Event::EditorUnavailable {
                            key: m.chain_key,
                            slot,
                        });
                    }
                }
            }
            kind::SLOT_REACTIVATED => {
                if let Ok(m) = f.meta_as::<codec::SlotReactivated>() {
                    self.on_reactivated(m);
                }
            }
            kind::WORKGROUP_ATTACHED => {
                if let Ok(m) = f.meta_as::<codec::WorkgroupAttached>() {
                    self.emit(Event::WorkgroupAttached {
                        token: m.token,
                        ok: m.ok,
                        joined: m.joined,
                    });
                }
            }
            _ => {}
        }
    }

    /// D22: a slot's cycle ended. The child is untrusted: an unknown chain or slot, or a quarantined one (it has no
    /// processor), is ignored; the counts are clamped and the supervisor's own saturate. The stall rule is the host application's,
    /// from its own geometry: a forced take, or a cycle over 4 block budgets.
    fn on_reactivated(&mut self, m: codec::SlotReactivated) {
        if self.quarantine.contains(&(m.chain_key, m.slot)) {
            return;
        }
        let rate = self.cfg.sample_rate;
        let Some(ch) = self.chains.get_mut(&m.chain_key) else {
            return;
        };
        let budget = summoner_plugin::handover::cycle_budget(ch.spec.geometry.max_frames, rate);
        let Some(s) = ch.slots.get_mut(m.slot as usize) else {
            return;
        };
        let given_up = m.take_given_up || m.abandoned;
        let stall = m.forced_take || given_up || u128::from(m.cycle_ns) > budget.as_nanos();
        let blocks = m.bypassed_blocks;
        s.reactivations = s.reactivations.saturating_add(1);
        s.bypassed_blocks = s.bypassed_blocks.saturating_add(blocks);
        if stall {
            s.stalls = s.stalls.saturating_add(1);
        }
        let ev = Event::SlotReactivated {
            key: m.chain_key,
            slot: m.slot,
            latency: m.latency.min(1 << 20),
            bypassed_blocks: blocks,
            cycle_ns: m.cycle_ns,
            forced_take: m.forced_take,
            misaligned: m.misaligned.min(blocks),
            take_given_up: m.take_given_up,
            abandoned: m.abandoned,
            stall,
            reactivations: s.reactivations,
            total_bypassed_blocks: s.bypassed_blocks,
            stalls: s.stalls,
        };
        let plugin = s.name.clone();
        self.counters.reactivations.fetch_add(1, Ordering::Relaxed);
        if stall {
            self.counters
                .reactivation_stalls
                .fetch_add(1, Ordering::Relaxed);
        }
        if given_up {
            self.counters.take_give_ups.fetch_add(1, Ordering::Relaxed);
        }
        self.emit(ev);
        // A take abandoned at the horizon never leaves the slot silently out of its cell. It is quarantined
        // (the notice is `Event::Quarantined`, op "reactivation"), and its chain is reloaded so the child hosts the
        // slot as D7's empty pass-through (latency 0, no parameters), as the host application now models it.
        if m.abandoned && self.quarantine.insert((m.chain_key, m.slot)) {
            self.counters
                .chains_quarantined
                .store(self.quarantine.len() as u32, Ordering::Relaxed);
            self.emit(Event::Quarantined {
                key: m.chain_key,
                slot: m.slot,
                plugin,
                op: "reactivation".into(),
            });
            self.start_reload(m.chain_key);
        }
    }

    /// Before an abandoned slot's chain is reloaded, read every chain-mate's current state (the child is
    /// alive on this path), bounded by [`RELOAD_READ_DEADLINE`], so the mates come back as they are now rather than as
    /// last saved. A mate whose read fails, is refused or is late keeps its stored blob (counted).
    fn start_reload(&mut self, key: u64) {
        let Some(ch) = self.chains.get(&key) else {
            return;
        };
        let mates: BTreeSet<u32> = ch
            .slots
            .iter()
            .enumerate()
            .filter(|(i, s)| s.loaded_ok && !self.quarantine.contains(&(key, *i as u32)))
            .map(|(i, _)| i as u32)
            .collect();
        for &slot in &mates {
            self.request(
                kind::STATE_GET,
                &codec::StateGet::new(key, slot, false),
                &[],
                Vec::new(),
                Pending::ReloadRead { key, slot },
            );
        }
        if mates.is_empty() {
            self.reload(key);
        } else {
            let deadline = self.now() + RELOAD_READ_DEADLINE;
            self.reloads.push(Reload {
                key,
                deadline,
                waiting: mates,
            });
        }
    }

    fn on_reload_read(&mut self, f: &Frame, key: u64, slot: u32) {
        let pending = self
            .reloads
            .iter()
            .position(|r| r.key == key && r.waiting.contains(&slot));
        let ok = !f.header.is_error() && state_blob::check(&f.blob).is_ok();
        let reply = f.meta_as::<codec::StateGetReply>().ok();
        let child_gen = reply.as_ref().map_or(0, |m| m.child_gen);
        if let Some(bytes) = reply.as_ref().and_then(|m| m.too_large())
            && !f.header.is_error()
        {
            // E1: the stored blob is re-applied (ok is false: an empty blob), and control hears `tooLarge`.
            self.too_large(key, slot, child_gen, bytes, None);
        } else if !f.header.is_error() {
            // An ordinary read too: control adopts it (StateRead), or hears the refusal (StateRefused).
            self.read_arrived(key, slot, child_gen, f.blob.clone(), None);
        }
        let Some(i) = pending else {
            return; // late: the reload went on with the stored blob
        };
        if ok {
            if let Some(s) = self
                .chains
                .get_mut(&key)
                .and_then(|c| c.slots.get_mut(slot as usize))
            {
                let generation = s.state.as_ref().map_or(0, |x| x.generation);
                s.state = Some(SlotState {
                    slot,
                    generation,
                    blob: f.blob.clone().into(),
                });
            }
        } else {
            self.counters
                .reload_state_fallbacks
                .fetch_add(1, Ordering::Relaxed);
        }
        self.reloads[i].waiting.remove(&slot);
        if self.reloads[i].waiting.is_empty() {
            let r = self.reloads.remove(i);
            self.reload(r.key);
        }
    }

    /// The reload itself: `chain.unload`, then `chain.load` with the quarantined slot empty (D7's pass-through).
    fn reload(&mut self, key: u64) {
        // (S4 (a)): the child's unload closes this chain's windows silently; say so first. `--cfg
        // mutant_editor_reload_silent` (never in a normal build) says nothing; e2e_recovery's horizon test fails.
        let open: Vec<u32> = self.chains.get_mut(&key).map_or(Vec::new(), |ch| {
            ch.slots
                .iter_mut()
                .enumerate()
                .filter(|(_, s)| s.editor_open)
                .map(|(i, s)| {
                    s.editor_open = false;
                    i as u32
                })
                .collect()
        });
        if !cfg!(mutant_editor_reload_silent) {
            for slot in open {
                self.emit(Event::EditorGone { key, slot });
            }
        }
        self.request(
            kind::CHAIN_UNLOAD,
            &codec::ChainRef::new(key, true),
            &[],
            Vec::new(),
            Pending::Receipt,
        );
        self.load_chain(key);
    }

    fn link_of(&self, key: u64) -> Option<Arc<ChainLink>> {
        let ch = self.chains.get(&key)?;
        let c = self.child.as_ref()?;
        Some(Arc::new(ChainLink::new(
            ch.region.clone()?,
            c.page.clone(),
            ch.spec.group,
            ch.depth.depth(),
            ch.spec.monitor,
            ch.latency,
            ch.spec.param_ids.clone(),
        )))
    }

    fn replace_notices(&mut self, key: u64, notices: &[Vec<String>]) -> bool {
        let Some(ch) = self.chains.get_mut(&key) else {
            return false;
        };
        let mut changed = false;
        for (i, slot) in ch.slots.iter_mut().enumerate() {
            let mut next = if slot.loaded_ok && !self.quarantine.contains(&(key, i as u32)) {
                notices.get(i).map_or_else(Vec::new, |v| kept_notices(v))
            } else {
                Vec::new()
            };
            if cfg!(mutant_union_notices) {
                for code in &slot.notices {
                    if !next.contains(code) {
                        next.push(code.clone());
                    }
                }
            }
            changed |= slot.notices != next;
            slot.notices = next;
        }
        changed
    }

    fn notice_slots(&self, key: u64) -> Vec<ReadySlot> {
        self.chains.get(&key).map_or_else(Vec::new, |ch| {
            ch.slots
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let quarantined = self.quarantine.contains(&(key, i as u32));
                    ReadySlot {
                        ok: s.loaded_ok,
                        quarantined,
                        latency: s.latency,
                        param_count: s.param_count,
                        has_editor: s.has_editor && !quarantined,
                        notices: if s.loaded_ok && !quarantined {
                            s.notices.clone()
                        } else {
                            Vec::new()
                        },
                    }
                })
                .collect()
        })
    }

    fn maybe_ready(&mut self, key: u64) {
        let Some(ch) = self.chains.get_mut(&key) else {
            return;
        };
        if ch.ready || !ch.loaded {
            return;
        }
        ch.ready = true;
        let names = ch.slots.iter().map(|s| s.name.clone()).collect();
        let slots = ch
            .slots
            .iter()
            .enumerate()
            .map(|(i, s)| ReadySlot {
                ok: s.loaded_ok,
                quarantined: self.quarantine.contains(&(key, i as u32)),
                latency: s.latency,
                param_count: s.param_count,
                has_editor: s.has_editor && !self.quarantine.contains(&(key, i as u32)),
                notices: if self.quarantine.contains(&(key, i as u32)) {
                    Vec::new()
                } else {
                    s.notices.clone()
                },
            })
            .collect();
        let inc = self.incarnation;
        if let Some(link) = self.link_of(key) {
            self.emit(Event::ChainReady {
                key,
                link,
                names,
                slots,
                incarnation: inc,
            });
        }
        // After a respawn, once every chain is back: tell the app what each re-applied slot may have lost (C7).
        if let Some(reason) = self.restart_reason
            && self.chains.values().all(|c| c.ready)
        {
            self.restart_reason = None;
            let slots = std::mem::take(&mut self.restarted_slots);
            self.emit(Event::HostRestarted { reason, slots });
        }
    }

    fn on_hello(&mut self, f: &Frame) {
        // H-3: a reply that does not decode is a protocol fault, no blame (`check_to_core` refuses it first; this is
        // the same rule should the two ever differ).
        let m = match f.meta_as::<codec::HelloReply>() {
            Ok(m) => m,
            Err(e) => {
                self.protocol_fault(&format!("a hello reply that does not decode: {e}"));
                return;
            }
        };
        // The major. An error reply carries no `protocol`, so it decodes as 0 and is a mismatch.
        if !hello_compatible(m.protocol) {
            self.incompatible(&m, f.header.is_error());
            return;
        }
        // Verified: from here the incarnation may be sent more than `hello`.
        if let Some(c) = &mut self.child {
            self.counters
                .child_protocol
                .store(m.protocol, Ordering::Relaxed);
            c.hello_ok = true;
            // H-1: send -> verified, and posix_spawn's start -> verified (the budgets, 100 ms warm and 500 ms cold,
            // are asserted in the timing suite).
            let ns = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            self.counters
                .hello_rtt_ns
                .store(ns(c.hello_sent.elapsed()), Ordering::Relaxed);
            self.counters
                .hello_launch_ns
                .store(ns(c.spawn_started.elapsed()), Ordering::Relaxed);
        }
        let route_b = m.mach_slot0;
        let pid = self.child.as_ref().map(|c| c.pid).unwrap_or(0);
        let mut route = WorkgroupRoute::None;
        if route_b {
            // The child sent its channel hello to slot 0's port before answering.
            let port = self
                .child
                .as_ref()
                .and_then(|c| c.mach_rx.as_ref())
                .and_then(|rx| rx.recv(200).ok());
            if let Some(msg) = port
                && msg.sender_pid == pid as i32
                && msg.port.is_some()
            {
                if let Some(c) = &mut self.child {
                    c.child_port = msg.port;
                }
                route = WorkgroupRoute::B;
                self.counters.workgroup_route.store(1, Ordering::Relaxed);
            }
        }
        if route == WorkgroupRoute::None {
            // Route A: a one-shot bootstrap name; the child looks it up and sends its hello there.
            let nonce = self.nonce();
            let name = self.cfg.names.route_a_service(std::process::id(), nonce);
            if let Ok(rx) = MachReceive::check_in(&name) {
                if let Some(c) = &mut self.child {
                    c.mach_a = Some(rx);
                }
                self.request(
                    kind::WORKGROUP_ATTACH,
                    &codec::WorkgroupAttach::new(0, Some(name)),
                    &[],
                    Vec::new(),
                    Pending::Receipt,
                );
            }
        }
        let now = self.now();
        if let Some(c) = &mut self.child {
            c.up_at = now;
        }
        self.counters.child_state.store(CHILD_UP, Ordering::Relaxed);
        self.emit(Event::ChildUp {
            pid,
            incarnation: self.incarnation,
            route,
            host: m.host.chars().take(64).collect(),
        });
        // the child-wide memory it wired at `hello` (absent from a 3.5 child: 0).
        self.page_wired = (m.wired_bytes, m.unwired_bytes);
        self.publish_wired();
        // a child spawned or respawned while the levels differ from the launch levels gets them
        // (with the time LEFT) before its first chain.
        let st = self.cfg.levels.level_state();
        self.log_gen_sent = st.generation;
        if let Some(m) = log_set_for(&st, true) {
            self.send_log_set(m);
        }
        // The pass `try_spawn` used to queue behind `hello`, now behind its verified reply.
        if WAIT_FOR_HELLO {
            self.load_all();
        }
    }

    /// forward a `system.setLog` that completed since the last turn (the level generation moved). With no
    /// verified child the generation is only recorded: the next `hello` sends what differs from launch.
    fn sync_log_levels(&mut self) {
        let st = self.cfg.levels.level_state();
        if st.generation == self.log_gen_sent && !cfg!(mutant_log_set_shared_gen) {
            return;
        }
        // The mutant forwards a generation once per process (as if one bridge's record served them all): the two-bridge
        // test fails.
        if cfg!(mutant_log_set_shared_gen) {
            static SHARED: AtomicU64 = AtomicU64::new(0);
            if SHARED.swap(st.generation, Ordering::AcqRel) == st.generation {
                self.log_gen_sent = st.generation;
                return;
            }
        }
        self.log_gen_sent = st.generation;
        if self.child.as_ref().is_some_and(|c| c.hello_ok && !c.dead)
            && let Some(m) = log_set_for(&st, false)
        {
            self.send_log_set(m);
        }
    }

    /// one `log.set` to the verified child, only when it was given a log folder (a child without a drainer
    /// could never expire a level; it refuses too). `--cfg mutant_log_set_without_log_dir` (never in a normal
    /// build) skips the folder check; the log-set folder test fails.
    fn send_log_set(&mut self, m: codec::LogSet) {
        if !self.verified() {
            return;
        }
        if self.cfg.levels.child_args().is_empty() && !cfg!(mutant_log_set_without_log_dir) {
            return;
        }
        self.counters.log_set_sent.fetch_add(1, Ordering::Relaxed);
        self.cfg.levels.note_child_log_set(ChildLogSet::Sent);
        let levels = m.levels;
        self.request(
            kind::LOG_SET,
            &m,
            &[],
            Vec::new(),
            Pending::LogSet { levels },
        );
    }

    /// `log.set`'s answer: an error reply or a different word is counted and logged once each, never a fault.
    fn on_log_set(&mut self, f: &Frame, sent: u64) {
        if f.header.is_error() {
            let why = f
                .meta_as::<codec::ErrorReply>()
                .map(|e| e.message.chars().take(160).collect::<String>())
                .unwrap_or_default();
            self.counters
                .log_set_refused
                .fetch_add(1, Ordering::Relaxed);
            self.cfg.levels.note_child_log_set(ChildLogSet::Refused);
            let stderr_tag = self.cfg.names.stderr;
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: WARNING: the plugin-host child refused log.set {sent:#018x}: {why}"
            );
            rec!(
                self,
                Level::Warn,
                "bridge.logSetRefused",
                levels = self.cfg.levels.describe(sent),
                reason = why.as_str()
            );
            return;
        }
        let Ok(r) = f.meta_as::<codec::LogSetReply>() else {
            return;
        };
        if r.levels == sent {
            self.counters
                .log_set_applied
                .fetch_add(1, Ordering::Relaxed);
            self.cfg.levels.note_child_log_set(ChildLogSet::Applied);
        } else {
            self.counters
                .log_set_mismatch
                .fetch_add(1, Ordering::Relaxed);
            self.cfg.levels.note_child_log_set(ChildLogSet::Mismatch);
            rec!(
                self,
                Level::Warn,
                "bridge.logSetMismatch",
                sent = self.cfg.levels.describe(sent),
                applied = self.cfg.levels.describe(r.levels)
            );
        }
    }

    /// a frame's `corr` echo. Read only from a verified child; a valid one on an answer (a reply, or a
    /// completion event) is the `corr` of the records written while the frame is handled; a malformed one, or one on a
    /// frame that answers nothing, is ignored and counted. `--cfg mutant_corr_echo_unchecked` (never in a normal
    /// build) takes any string; the bad-echo test fails.
    fn echo_of(&self, f: &Frame) -> Option<String> {
        if !self.verified() {
            return None;
        }
        let read = codec::corr_of_meta(&f.meta);
        #[cfg(mutant_corr_echo_unchecked)]
        let read = match read {
            codec::CorrRead::Invalid => f
                .meta_as::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("corr").and_then(|c| c.as_str()).map(str::to_string))
                .map_or(codec::CorrRead::Invalid, codec::CorrRead::Valid),
            r => r,
        };
        match read {
            codec::CorrRead::Absent => None,
            codec::CorrRead::Valid(c) if codec::answers_a_request(&f.header) => {
                self.counters
                    .child_corr_echoed
                    .fetch_add(1, Ordering::Relaxed);
                Some(c)
            }
            _ => {
                self.counters
                    .child_corr_ignored
                    .fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// The `hello` reply's major is not the host application's. Killed and reaped as any death, never
    /// attributed, and no respawn armed (no backoff, no attempt counted): straight to the exhausted state, every chain
    /// `ChainDown {Incompatible}`, then `Exhausted`, until `plugin.restartHost` or a reopen.
    fn incompatible(&mut self, m: &codec::HelloReply, error_reply: bool) {
        let Some(c) = &mut self.child else { return };
        if c.dead {
            return;
        }
        let reason = DownReason::Incompatible {
            child_protocol: m.protocol,
            core_protocol: ABI_VERSION,
        };
        self.counters
            .child_incompatible
            .fetch_add(1, Ordering::Relaxed);
        let ver = |p: u32| format!("{}.{} ({p:#x})", p >> 16, p & 0xffff);
        let stderr_tag = self.cfg.names.stderr;
        runecraft_rt::safe_eprintln!(
            "{stderr_tag}: INCOMPATIBLE plugin-host child (pid {}, incarnation {}): its hello{} says protocol {}, the \
             core speaks {} (the major must match); host {:?}. The child is killed and not respawned: every \
             chain stays silent until plugin.restartHost or a reopen. No plugin is blamed.",
            c.pid,
            c.incarnation,
            if error_reply { " (an error reply)" } else { "" },
            ver(m.protocol),
            ver(ABI_VERSION),
            m.host.chars().take(64).collect::<String>(),
        );
        c.kill_reason.get_or_insert(reason);
        // Through the Child, which sends nothing once its reaper has reaped it (never a raw pid signal).
        c.process.kill();
        self.death_as(reason, Blame::None, Respawn::Never);
    }

    /// N4: the child exited [`EXIT_USAGE`] before its verified `hello`: one loud engine-side error, no blame,
    /// no respawn ([`DownReason::Usage`]).
    fn usage_exit(&mut self) {
        let Some(c) = &self.child else { return };
        if c.dead {
            return;
        }
        self.counters
            .child_usage_exits
            .fetch_add(1, Ordering::Relaxed);
        let stderr_tag = self.cfg.names.stderr;
        runecraft_rt::safe_eprintln!(
            "{stderr_tag}: ENGINE-SIDE ERROR: the plugin-host child (pid {}, incarnation {}) exited {EXIT_USAGE} (a usage \
             error) before its hello: the core started it with an argv it refuses ({:?} {:?}). No plugin is blamed and \
             it is not respawned: every chain stays silent until plugin.restartHost or a reopen.",
            c.pid,
            c.incarnation,
            self.cfg.program,
            spawn_args(&self.cfg),
        );
        self.death_as(DownReason::Usage, Blame::None, Respawn::Never);
    }

    fn poll_route_a(&mut self) {
        let Some(c) = &mut self.child else { return };
        let Some(rx) = &c.mach_a else { return };
        match rx.recv(0) {
            Ok(msg) if msg.sender_pid == c.pid as i32 && msg.port.is_some() => {
                c.child_port = msg.port;
                c.mach_a = None; // the name dies with the receive right
                self.counters.workgroup_route.store(2, Ordering::Relaxed);
            }
            Ok(_) => {} // not from our child: ignored (the audit pid check)
            Err(_) => {}
        }
    }

    fn on_generations(&mut self, f: &Frame, burst: Option<u64>) {
        let Ok(m) = f.meta_as::<codec::Generations>() else {
            return;
        };
        let mut moved = Vec::new();
        for e in m.slots.iter().take(16_384) {
            if let Some(s) = self
                .chains
                .get_mut(&e.chain_key)
                .and_then(|c| c.slots.get_mut(e.slot as usize))
                && e.child_gen != s.child_gen
            {
                // E0(b): an editor the generations poll found open marks the slot's movement as the user's.
                s.editor_seen |= e.editor_open;
                s.child_gen = e.child_gen;
                moved.push((e.chain_key, e.slot, e.child_gen));
            }
        }
        if let Some(t) = burst
            && let Some(b) = &mut self.burst
            && b.token == t
        {
            b.generations_answered = true;
            for x in moved.iter().map(|m| (m.0, m.1)) {
                if !b.requested.contains(&x) && !b.queue.contains(&x) {
                    b.queue.push_back(x);
                }
            }
        }
        if !moved.is_empty() {
            self.emit(Event::Moved { slots: moved });
        }
        if burst.is_some() {
            self.pump_burst();
        }
    }

    fn on_param_list(&mut self, f: &Frame, key: u64, slot: u32, offset: u32, count: u32) {
        let page = if f.header.is_error() {
            None
        } else {
            f.meta_as::<codec::ParamListReply>().ok()
        };
        let page = page.filter(|m| {
            m.params
                .iter()
                .all(|p| p.default.is_finite() && p.value.is_finite())
        });
        let (ok, total, params) = match page {
            Some(m) => (
                true,
                m.total.min(PARAM_CAP),
                m.params
                    .into_iter()
                    .take(count as usize)
                    .map(|p| clamp_entry(p, true))
                    .collect(),
            ),
            None => (false, 0, Vec::new()),
        };
        self.emit(Event::ParamList {
            key,
            slot,
            offset,
            ok,
            total,
            params,
        });
    }

    fn on_param_set(&mut self, f: &Frame, key: u64, slot: u32, plugin_id: u32) {
        let reply = if f.header.is_error() {
            None
        } else {
            f.meta_as::<codec::ParamSetReply>()
                .ok()
                .filter(|m| m.value.is_finite())
        };
        if f.header.is_error()
            && let Ok(e) = f.meta_as::<codec::ErrorReply>()
        {
            let stderr_tag = self.cfg.names.stderr;
            runecraft_rt::safe_eprintln!(
                "{stderr_tag}: param.set chain {key} slot {slot} id {plugin_id} refused: {}",
                e.message
            );
        }
        self.emit(Event::ParamSet {
            key,
            slot,
            plugin_id,
            ok: reply.is_some(),
            value: reply.map_or(0.0, |m| m.value.clamp(0.0, 1.0)),
        });
    }

    fn on_state_get(&mut self, f: &Frame, key: u64, slot: u32, burst: Option<u64>) {
        if !f.header.is_error()
            && let Ok(m) = f.meta_as::<codec::StateGetReply>()
        {
            if let Some(bytes) = m.too_large() {
                self.too_large(key, slot, m.child_gen, bytes, burst);
            } else {
                self.read_arrived(key, slot, m.child_gen, f.blob.clone(), burst);
            }
        }
        self.burst_answered(burst, key, slot);
    }

    /// E1 (protocol 3.3): a state over the frame cap. The slot keeps its stored state and is not left moved (its
    /// read generation catches up), so only its next `Moved` reads it again; control raises the notice.
    fn too_large(&mut self, key: u64, slot: u32, child_gen: u64, bytes: u64, burst: Option<u64>) {
        if let Some(s) = self
            .chains
            .get_mut(&key)
            .and_then(|c| c.slots.get_mut(slot as usize))
        {
            s.child_gen = s.child_gen.max(child_gen);
            s.read_gen = s.read_gen.max(child_gen);
            s.partial = false;
            s.remarked = false;
        }
        if let (Some(t), Some(b)) = (burst, &mut self.burst)
            && b.token == t
        {
            b.too_large.push((key, slot, bytes));
        }
        self.emit(Event::StateTooLarge {
            key,
            slot,
            child_gen,
            bytes,
        });
    }

    fn read_arrived(
        &mut self,
        key: u64,
        slot: u32,
        child_gen: u64,
        blob: Vec<u8>,
        burst: Option<u64>,
    ) {
        let now = self.now();
        // D21: the host application checks the frame only. A refused blob is not stored, and the read counts as failed: the slot
        // keeps its last known blob, marked stale as for a read that did not answer (it stays "moved", so the next
        // burst asks again; a burst that requested it lists it in `stale`).
        let checked = state_blob::check(&blob);
        // The mutation check (never in a normal build): the child blob's frame check is skipped, so a refused blob is
        // stored (e2e_state `a_bare_blob_from_the_child_is_not_stored_…` fails).
        #[cfg(mutant_sasp_child_unchecked)]
        let checked = checked.or_else(|_| state_blob::check(&state_blob::builtin(&[])));
        if let Err(reason) = checked {
            self.counters
                .state_format_refused
                .fetch_add(1, Ordering::Relaxed);
            let mut newer = false;
            if let Some(s) = self
                .chains
                .get_mut(&key)
                .and_then(|c| c.slots.get_mut(slot as usize))
            {
                // as below; the movement stays reported, the refused read leaves the slot moved.
                newer = burst.is_none() && child_gen > s.child_gen;
                s.child_gen = s.child_gen.max(child_gen);
            }
            // The mutation check (never in a normal build): the refused read swallows the movement; e_capture_bridge's
            // `a_refused_live_read_at_a_newer_generation_reports_moved` fails.
            if newer && !cfg!(mutant_refused_swallows_move) {
                self.emit(Event::Moved {
                    slots: vec![(key, slot, child_gen)],
                });
            }
            self.emit(Event::StateRefused {
                key,
                slot,
                generation: child_gen,
                source: StateSource::Child,
                reason,
            });
            return;
        }
        // a read outside a burst (a live `state.get`, an editor's close, a reload's) whose generation is above
        // the last one seen moved since: reported as the generations poll would have ([`Event::Moved`], before the read),
        // since raising `child_gen` here leaves no difference for the next poll to find. A burst's own read (it asked
        // because the slot moved) and a generation already seen (a whole set's, E2) report nothing.
        let mut newer = false;
        if let Some(s) = self
            .chains
            .get_mut(&key)
            .and_then(|c| c.slots.get_mut(slot as usize))
        {
            newer = burst.is_none() && child_gen > s.child_gen;
            s.last_read_at = now;
            s.partial = false;
            s.remarked = false;
            s.child_gen = s.child_gen.max(child_gen);
            s.read_gen = s.read_gen.max(child_gen);
        }
        // The mutation check (never in a normal build): the read swallows the movement (the old behaviour);
        // e_capture_bridge's `a_live_read_at_a_newer_generation_reports_moved_before_the_read` and the engine's
        // capture_tests `a_live_read_that_finds_a_movement_is_captured_at_once` fail.
        if newer && !cfg!(mutant_read_swallows_move) {
            self.emit(Event::Moved {
                slots: vec![(key, slot, child_gen)],
            });
        }
        // E0(b): the editor was open since the last completed read (an editor still open stays seen).
        let user = self
            .chains
            .get_mut(&key)
            .and_then(|c| c.slots.get_mut(slot as usize))
            .is_some_and(|s| {
                let u = s.editor_seen || s.editor_open;
                s.editor_seen = s.editor_open;
                u
            });
        let blob: Arc<[u8]> = blob.into();
        // `--cfg mutant_state_hasher_ignored` (never in a normal build): the configured hasher is not asked;
        // e_capture_bridge's `a_whole_set_names_its_generation_and_is_never_read_back` (its hash check) fails.
        let sha = if cfg!(mutant_state_hasher_ignored) {
            String::new()
        } else {
            self.cfg.state_hasher.sha256_hex(&blob)
        };
        if let (Some(t), Some(b)) = (burst, &mut self.burst)
            && b.token == t
            && now <= b.deadline
        {
            // E1: the byte budget counts as reads arrive.
            b.bytes = b.bytes.saturating_add(blob.len() as u64);
            b.fresh
                .push(Fresh::new(key, slot, child_gen, blob, sha, user));
            return;
        }
        // A late read (or an editor close): an ordinary state change for control (D6).
        self.emit(Event::StateRead {
            key,
            slot,
            child_gen,
            blob,
            incarnation: self.incarnation,
            sha,
            user,
        });
    }

    // -----------------------------------------------------------------------------------------------------------------
    // Death, attribution, respawn (D7)
    // -----------------------------------------------------------------------------------------------------------------

    fn kill(&mut self, reason: DownReason) {
        if let Some(c) = &mut self.child
            && !c.dead
        {
            c.kill_reason.get_or_insert(reason);
            c.process.kill();
            self.death(reason);
        }
    }

    /// The protocol fault, with the hello faults (H-2, H-3) and the reader's own
    /// refusals (H-4): the supervisor kills the child for a protocol fault. Counted loudly (`protocol_errors` and a
    /// stderr line naming the fault), never attributed (no `inSlot` or status-record blame, no quarantine,
    /// `last_attribution` untouched), reason [`DownReason::Protocol`]. The respawn is armed and an attempt counted, as
    /// for any death, so a child that garbles fd 3 every time reaches `Exhausted` after its attempts. Only when this
    /// kill is the death's first sign: a child already dead was attributed as its first sign said.
    /// The child's wired / unwired gauges: the sum over the loaded chains.
    fn publish_wired(&self) {
        let (w, u) =
            self.chains
                .values()
                .filter(|c| c.loaded)
                .fold(self.page_wired, |(w, u), c| {
                    (
                        w.saturating_add(c.wired_bytes),
                        u.saturating_add(c.unwired_bytes),
                    )
                });
        self.counters
            .child_page_wired_bytes
            .store(self.page_wired.0, Ordering::Relaxed);
        self.counters
            .child_page_unwired_bytes
            .store(self.page_wired.1, Ordering::Relaxed);
        self.counters.child_wired_bytes.store(w, Ordering::Relaxed);
        self.counters
            .child_unwired_bytes
            .store(u, Ordering::Relaxed);
    }

    fn protocol_fault(&mut self, what: &str) {
        let Some(c) = &mut self.child else { return };
        if c.dead {
            return;
        }
        self.counters
            .protocol_errors
            .fetch_add(1, Ordering::Relaxed);
        // [P] The status record, for diagnosis only: it attributes nothing.
        let st = c.page.read_status();
        let status = if st.op_start_ticks != 0 {
            format!(
                "{} chain {} slot {} {:?}",
                op_name(st.op),
                st.chain_key,
                st.slot,
                st.name.chars().take(64).collect::<String>()
            )
        } else {
            "idle".into()
        };
        let stderr_tag = self.cfg.names.stderr;
        runecraft_rt::safe_eprintln!(
            "{stderr_tag}: PROTOCOL FAULT from the plugin-host child (pid {}, incarnation {}): {what}. The child is \
             killed; no plugin is blamed (status record, unattributed: {status}).",
            c.pid,
            c.incarnation
        );
        c.kill_reason.get_or_insert(DownReason::Protocol);
        c.process.kill();
        let blame = if PROTOCOL_UNBLAMED {
            Blame::None
        } else {
            Blame::Attributed
        };
        self.death_as(DownReason::Protocol, blame, Respawn::Backoff);
    }

    /// The first sign of a death (EOF, waitpid, a stall, a wedge kill): attribute it, silence its chains, schedule the
    /// respawn. Later signs of the same death are ignored.
    fn death(&mut self, reason: DownReason) {
        self.death_as(reason, Blame::Attributed, Respawn::Backoff);
    }

    /// [`Supervisor::death`], with the attribution and the respawn chosen: `Blame::None` reads neither `inSlot` nor the
    /// status record and leaves `last_attribution` as it is (an unattributed death neither breaks
    /// nor completes D7's run of two); `Respawn::Never` arms no backoff and counts no attempt.
    fn death_as(&mut self, reason: DownReason, blame: Blame, respawn: Respawn) {
        let Some(c) = &mut self.child else { return };
        if c.dead {
            return;
        }
        c.dead = true;
        // The death's first sign is often the reaper's `Died`, which comes AFTER the
        // reap, so this kill must go through the Child, which then sends nothing (the pid may be another process's).
        // A raw pid signal does not compile here: the floor keeps it crate-private.
        c.process.kill();
        let _ = c.sock.shutdown();
        let reason = c.kill_reason.unwrap_or(reason);
        // Attribution: an audio thread inside a plugin (inSlot), else the message thread's status record.
        let mut who: Option<(u64, u32, String)> = None;
        let wedged = self.wedged.take();
        if blame == Blame::Attributed {
            let order = wedged
                .into_iter()
                .chain(self.chains.keys().copied().filter(|k| Some(*k) != wedged));
            for k in order {
                let Some(ch) = self.chains.get(&k) else {
                    continue;
                };
                if let Some(r) = &ch.region {
                    let s = r.in_slot().load(Ordering::Acquire);
                    if s != 0 && (s as usize) <= ch.slots.len() {
                        who = Some((k, s - 1, "process".into()));
                        break;
                    }
                }
            }
            if who.is_none() {
                let st = c.page.read_status();
                if st.op_start_ticks != 0
                    && self
                        .chains
                        .get(&st.chain_key)
                        .is_some_and(|ch| (st.slot as usize) < ch.slots.len())
                {
                    who = Some((st.chain_key, st.slot, op_name(st.op).into()));
                }
            }
        }
        let child_page = c.page.clone();
        // (The child is no longer borrowed from here on.) Arm the respawn (respawn_at, attempts, child_state) BEFORE any event of this death is emitted: a consumer
        // that reacts to `Quarantined` / `ChainDown` (a test advancing a manual clock, host's harness) must find the
        // backoff already armed, so the respawn is measured from the death, never from a clock moved after it.
        // `Exhausted` is still emitted last. Mutation: `--cfg mutant_emit_before_arm` restores the old order.
        // An incompatible child arms nothing; the exhausted state is set, also before any event.
        #[cfg(not(mutant_emit_before_arm))]
        let exhausted = self.after_death_state(respawn);
        self.counters.child_deaths.fetch_add(1, Ordering::Relaxed);
        self.counters.spurious_wakes.store(
            child_page.word64(P_SPURIOUS_WAKES).load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        if blame == Blame::Attributed {
            if let Some((k, s, op)) = &who {
                if self.last_attribution == Some((*k, *s)) {
                    self.attribution_run = self.attribution_run.saturating_add(1);
                    if self.quarantine.insert((*k, *s)) {
                        let plugin = self
                            .chains
                            .get(k)
                            .and_then(|ch| ch.slots.get(*s as usize))
                            .map(|x| x.name.clone())
                            .unwrap_or_default();
                        self.counters
                            .chains_quarantined
                            .store(self.quarantine.len() as u32, Ordering::Relaxed);
                        self.emit(Event::Quarantined {
                            key: *k,
                            slot: *s,
                            plugin,
                            op: op.clone(),
                        });
                    }
                } else {
                    self.attribution_run = 1;
                }
                self.last_attribution = Some((*k, *s));
            } else {
                self.last_attribution = None;
                self.attribution_run = 0;
            }
            self.counters
                .attribution_run
                .store(self.attribution_run, Ordering::Relaxed);
        }
        // C7: what each slot may lose, for the restart event.
        let now = self.now();
        self.restarted_slots.clear();
        for (&k, ch) in &self.chains {
            for (i, s) in ch.slots.iter().enumerate() {
                if s.state.is_some() || s.editor_open {
                    self.restarted_slots.push(RestartedSlot {
                        key: k,
                        slot: i as u32,
                        plugin: s.name.clone(),
                        unread_ms: now.saturating_sub(s.last_read_at).as_millis() as u64,
                        editor_was_open: s.editor_open,
                    });
                }
            }
        }
        // the windows died with the child; the respawn opens none (the host application reports each as `childDied`).
        for ch in self.chains.values_mut() {
            for s in ch.slots.iter_mut() {
                s.editor_open = false;
            }
        }
        self.restart_reason = Some(reason);
        // A death re-loads every chain at the respawn anyway: no reload waits on this child's reads.
        self.reloads.clear();
        let keys: Vec<u64> = self.chains.keys().copied().collect();
        for k in keys {
            let ch = self.chains.get_mut(&k).expect("present");
            ch.down = true;
            ch.ready = false;
            ch.loaded = false;
            self.emit(Event::ChainDown { key: k, reason });
        }
        // the dead incarnation's page leaves the sum with its chains.
        self.page_wired = (0, 0);
        self.publish_wired();
        self.counters
            .chains_down
            .store(self.chains.len() as u32, Ordering::Relaxed);
        if let Some(b) = &self.burst {
            let _ = b;
            self.finish_burst();
        }
        // The mutant holds the window open (emit, then arm 50 ms later) so the test sees it every time.
        #[cfg(mutant_emit_before_arm)]
        let exhausted = {
            std::thread::sleep(std::time::Duration::from_millis(50));
            self.after_death_state(respawn)
        };
        if exhausted {
            self.emit(Event::Exhausted);
        }
    }

    /// A death without a child to attribute (a failed spawn): arm the respawn, then report exhaustion.
    fn after_death(&mut self, _reason: DownReason) {
        if self.arm_respawn() {
            self.emit(Event::Exhausted);
        }
    }

    /// A death's state, before any of its events: the backoff ([`Supervisor::arm_respawn`]) or, for an incompatible
    /// child, the exhausted state with no attempt counted ([`Supervisor::exhaust`]). True when the caller must emit
    /// `Event::Exhausted`, after the death's other events.
    fn after_death_state(&mut self, respawn: Respawn) -> bool {
        match respawn {
            Respawn::Backoff => self.arm_respawn(),
            Respawn::Never => self.exhaust(),
        }
    }

    /// The backoff after a death: `respawn_at`, `attempts` and `child_state`, and nothing emitted. True when the
    /// attempts are exhausted (the caller then emits `Event::Exhausted`, after the death's other events).
    fn arm_respawn(&mut self) -> bool {
        let now = self.now();
        if self.shutting_down {
            return false;
        }
        let exhausted = if self.attempts < ATTEMPTS {
            let wait = self.cfg.backoff[self.attempts as usize];
            self.attempts += 1;
            self.respawn_at = Some(now + wait);
            self.counters
                .child_state
                .store(CHILD_BACKOFF, Ordering::Relaxed);
            false
        } else {
            self.exhaust()
        };
        self.counters
            .attempts
            .store(self.attempts, Ordering::Relaxed);
        exhausted
    }

    /// The exhausted state (`exhausted`, `child_state` = EXHAUSTED, no respawn), nothing emitted and no attempt counted:
    /// the cap of [`Supervisor::arm_respawn`], and an incompatible child. True unless shutting down (the caller
    /// then emits `Event::Exhausted`, last).
    fn exhaust(&mut self) -> bool {
        if self.shutting_down {
            return false;
        }
        self.respawn_at = None;
        self.exhausted = true;
        self.counters
            .child_state
            .store(CHILD_EXHAUSTED, Ordering::Relaxed);
        true
    }

    // -----------------------------------------------------------------------------------------------------------------
    // The tick
    // -----------------------------------------------------------------------------------------------------------------

    fn tick(&mut self) {
        let now = self.now();
        if self.burst.as_ref().is_some_and(|b| now >= b.deadline) {
            self.finish_burst();
        }
        // A reload whose chain-mates' reads passed their deadline goes on with the stored blobs.
        let due: Vec<Reload> = {
            let (due, keep): (Vec<Reload>, Vec<Reload>) = std::mem::take(&mut self.reloads)
                .into_iter()
                .partition(|r| now >= r.deadline);
            self.reloads = keep;
            due
        };
        for r in due {
            self.counters
                .reload_state_fallbacks
                .fetch_add(r.waiting.len() as u64, Ordering::Relaxed);
            self.reload(r.key);
        }
        if let Some(at) = self.respawn_at
            && now >= at
            && self.child.as_ref().is_none_or(|c| c.dead)
        {
            self.respawn_at = None;
            // Wait for the reaper of the old incarnation before reusing the slot: a dead child's reaper report only
            // clears `self.child`; spawning drops the old record.
            self.counters.child_respawns.fetch_add(1, Ordering::Relaxed);
            self.child = None;
            self.spawn();
        }
        let Some(c) = &self.child else { return };
        // H-2: a hello not verified by its deadline is a no-blame kill, an attempt counted.
        if ENFORCE_HELLO_DEADLINE && !c.dead && !c.hello_ok && now >= c.hello_by {
            let waited = now.saturating_sub(c.hello_by) + self.cfg.hello_deadline;
            self.protocol_fault(&format!(
                "no verified hello reply {} ms after the hello (the deadline is {} ms)",
                waited.as_millis(),
                self.cfg.hello_deadline.as_millis()
            ));
            return;
        }
        if c.dead || !c.hello_ok {
            return;
        }
        self.poll_route_a();
        // The gate: forgiveness only after 60 s up AND every re-applied state confirmed.
        if let Some(c) = &self.child
            && self.attempts > 0
            && now.saturating_sub(c.up_at) >= self.cfg.forgive_after
            && c.unconfirmed.is_empty()
            && self.chains.values().all(|ch| ch.ready)
        {
            self.attempts = 0;
            self.counters.attempts.store(0, Ordering::Relaxed);
        }
        self.message_thread_hold();
        self.wedge_detector(now);
        if now >= self.next_depth {
            self.next_depth = now + self.cfg.depth_cadence;
            self.depth_tick(now);
        }
        // The generations poll: while any editor is open, and for 2 s after the last one closes.
        let open = self
            .chains
            .values()
            .any(|ch| ch.slots.iter().any(|s| s.editor_open));
        if (open || now < self.poll_until) && now >= self.next_poll {
            self.next_poll = now + self.cfg.poll_interval;
            self.request(
                kind::STATE_GENERATIONS,
                &serde_json::json!({}),
                &[],
                Vec::new(),
                Pending::Generations { burst: None },
            );
        }
        let page = &self.child.as_ref().expect("child").page;
        self.counters.spurious_wakes.store(
            page.word64(P_SPURIOUS_WAKES).load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        self.counters.workgroup_fallbacks.store(
            page.word64(P_WG_FALLBACKS).load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        let active = self
            .chains
            .values()
            .filter(|c| c.ready && !c.parked)
            .count();
        self.counters
            .chains_active
            .store(active as u32, Ordering::Relaxed);
        let deep = self
            .chains
            .values()
            .filter(|c| c.depth.depth() == 2)
            .count();
        self.counters
            .chains_depth2
            .store(deep as u32, Ordering::Relaxed);
        let down = self.chains.values().filter(|c| c.down).count();
        self.counters
            .chains_down
            .store(down as u32, Ordering::Relaxed);
    }

    fn message_thread_hold(&mut self) {
        let Some(c) = &mut self.child else { return };
        let st = c.page.read_status();
        if st.op_start_ticks == 0 {
            return;
        }
        let held =
            runecraft_ipc::bridge::ticks_to_ns(now_ticks().saturating_sub(st.op_start_ticks));
        if held > self.counters.msg_thread_hold_max_ns.load(Ordering::Relaxed) {
            self.counters
                .msg_thread_hold_max_ns
                .store(held, Ordering::Relaxed);
        }
        if held > 1_000_000_000 && c.hold_seen != st.op_start_ticks {
            c.hold_seen = st.op_start_ticks;
            self.counters
                .msg_thread_long_holds
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// C1 (D7): suspect after 2 s of an outstanding block without ack progress; ping through the region (the reader
    /// cannot answer for the chain thread); 2 s later, no pong = down: kill, and the slot inside processBlock is
    /// attributed.
    fn wedge_detector(&mut self, now: Duration) {
        let mut wedged: Option<u64> = None;
        let mut pings: Vec<(u64, u64)> = Vec::new();
        let suspect = self.cfg.wedge_suspect;
        let render_suspect = self.cfg.render_suspect;
        let verdict = self.cfg.wedge_verdict;
        for (&k, ch) in self.chains.iter_mut() {
            if !ch.ready || ch.down {
                continue;
            }
            let Some(r) = &ch.region else { continue };
            let seq = r.seq().load(Ordering::Acquire);
            let ack = r.ack().load(Ordering::Acquire);
            let w = &mut ch.wedge;
            if seq <= ack || ack != w.last_ack {
                w.last_ack = ack;
                w.progress_at = now;
                w.ping = None;
                continue;
            }
            let limit = if ch.spec.role == codec::Role::Render {
                render_suspect
            } else {
                suspect
            };
            match w.ping {
                None if now.saturating_sub(w.progress_at) >= limit => pings.push((k, 0)),
                Some((nonce, at)) if now.saturating_sub(at) >= verdict => {
                    if r.pong().load(Ordering::Acquire) == nonce {
                        // Woken by the ping with work pending: a lost wakeup, a protocol bug. Counted, not killed.
                        self.counters.lost_wakes.fetch_add(1, Ordering::Relaxed);
                        w.ping = None;
                        w.progress_at = now;
                    } else {
                        wedged = Some(k);
                        break;
                    }
                }
                _ => {}
            }
        }
        for (k, _) in pings {
            let nonce = self.nonce();
            let page = self.child.as_ref().map(|c| c.page.clone());
            let ch = self.chains.get_mut(&k).expect("present");
            let r = ch.region.as_ref().expect("region");
            r.ping().store(nonce, Ordering::Release);
            ch.wedge.ping = Some((nonce, now));
            if ch.parked || ch.spec.role == codec::Role::Render {
                let _ = r.seq_waker().wake();
            } else if let Some(p) = page {
                let _ = p.hub_waker(ch.spec.group as usize).wake();
            }
        }
        if let Some(k) = wedged {
            self.counters.wedges.fetch_add(1, Ordering::Relaxed);
            self.wedged = Some(k);
            self.kill(DownReason::Wedge);
        }
    }

    fn depth_tick(&mut self, now: Duration) {
        let keys: Vec<u64> = self.chains.keys().copied().collect();
        for k in keys {
            let ch = self.chains.get_mut(&k).expect("present");
            if !ch.ready || ch.down || ch.spec.role == codec::Role::Render {
                continue;
            }
            let Some(r) = &ch.region else { continue };
            let mut h = [0u32; HIST_BUCKETS];
            for (i, w) in r.hist().iter().enumerate() {
                h[i] = w.load(Ordering::Relaxed);
            }
            if let Some(DepthChange::Raise) = ch.depth.observe(now, h) {
                self.depth_event(k, DepthChange::Raise);
            }
        }
    }

    fn depth_event(&mut self, key: u64, change: DepthChange) {
        if let Some(link) = self.link_of(key) {
            self.emit(Event::DepthChange { key, link, change });
        }
    }
}

/// D6's reader -> supervisor bounds (branch `live-reader`): the blob bytes every
/// incarnation's reader holds between the socket and the supervisor, the frame the decoder is filling included (one
/// budget, shared), and each reader's frames in flight (its own bound; [`READER_FRAMES`]).
///
/// - **Admitted from the header, before any allocation** ([`codec::Decoder::feed_admitting`]): a frame is admitted only
///   if its reader has fewer than the frame bound in flight AND the bytes held plus its DECLARED blob stay within the
///   budget; else the reader waits, before the decoder reserves the body, so both bounds are exact (no overshoot).
///   Every frame counts against the frame bound, a meta-only one included.
/// - **Wait, never fault**: a waiting reader stops reading its socket, so the child's writes back up; a flooding child
///   stalls only itself (D6's table). It blocks on a condvar, woken when the supervisor frees a frame or when the
///   supervisor's receiver is gone ([`InboundRx`]'s drop closes the gate): then it ends, so no reader outlives the
///   supervisor.
/// - **Freed by a ticket**: each admitted frame has a [`BlobTicket`] riding with it (its frame and its blob bytes); the
///   supervisor redeems it when it takes the frame, which is where "in flight" ends, and a ticket dropped anywhere else
///   (a frame the channel discards, the reservation of a frame the reader never finished) frees both on drop, so
///   nothing leaks.
///
/// The lock is held for a few instructions by the readers and the supervisor only, never by the audio callback (N1).
struct BlobGate {
    budget: usize,
    frame_bound: usize,
    state: Mutex<GateState>,
    freed: Condvar,
    counters: Arc<ControlCounters>,
}

#[derive(Default)]
struct GateState {
    held: usize,
    /// Frames in flight, summed over the readers (each reader's own count is its [`ReaderFrames`]).
    frames: usize,
    /// The supervisor's receiver is gone: every waiting and later admission ends its reader.
    closed: bool,
}

/// One reader's frames in flight. Read and written ONLY under [`BlobGate`]'s lock (an atomic so the tickets can share
/// it across threads; the lock orders it, so `Relaxed` is enough and the condvar's predicate stays consistent).
#[derive(Default)]
struct ReaderFrames(AtomicUsize);

/// What [`BlobGate::admit`] decided for one header.
enum Admission {
    /// Admitted; the ticket is the frame's share (its frame and its blob bytes).
    Admitted(BlobTicket),
    /// The receiver is gone: the reader ends.
    Closed,
    /// The blob alone passes the whole budget (only a test budget below the codec's cap): a protocol fault, never a
    /// reader waiting forever.
    TooLarge,
}

/// The mutation checks (never in a normal build): `--cfg mutant_live_reader_post_alloc` restores the old order
/// (every header admitted, the decoded frame then waits while MORE than the budget is held, and is added), so the
/// budget unit tests (1.25 MiB held against 1 MiB) and the live reader test fail; `--cfg
/// mutant_live_reader_no_close` makes the receiver's drop close nothing, so
/// `a_stalled_reader_ends_when_the_receiver_is_dropped_while_its_bytes_are_held` fails (the reader never ends); `--cfg
/// mutant_live_reader_leak_on_drop` frees a frame's share only when the supervisor redeems it (the old counter),
/// so the drop and mid-frame tests fail (bytes stay held); `--cfg mutant_live_reader_no_frame_bound` drops the
/// frame bound (frames still counted), so the meta-only flood tests and the live reader's meta-only test fail.
const ADMIT_BEFORE_ALLOC: bool = !cfg!(mutant_live_reader_post_alloc);
const CLOSE_ON_DISCONNECT: bool = !cfg!(mutant_live_reader_no_close);
const FREE_ON_DROP: bool = !cfg!(mutant_live_reader_leak_on_drop);
const FRAME_BOUND: bool = !cfg!(mutant_live_reader_no_frame_bound);

impl BlobGate {
    fn new(budget: usize, counters: Arc<ControlCounters>) -> Arc<BlobGate> {
        Arc::new(BlobGate {
            budget,
            frame_bound: READER_FRAMES,
            state: Mutex::new(GateState::default()),
            freed: Condvar::new(),
            counters,
        })
    }

    /// A poisoned lock still holds a consistent count (every critical section is a few integer updates).
    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit one frame of `reader` from its header's declared blob length: at once if both bounds allow it, else wait
    /// (nothing allocated) until the supervisor frees enough, or the gate closes.
    fn admit(self: &Arc<Self>, reader: &Arc<ReaderFrames>, bytes: usize) -> Admission {
        if bytes > self.budget {
            return Admission::TooLarge;
        }
        let mut st = self.lock();
        let mut waited = false;
        // held ≤ budget and bytes ≤ budget, so the sum cannot overflow.
        while !st.closed
            && ((FRAME_BOUND && reader.0.load(Ordering::Relaxed) >= self.frame_bound)
                || st.held + bytes > self.budget)
        {
            waited = true;
            st = self.freed.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        if st.closed {
            return Admission::Closed;
        }
        Admission::Admitted(self.take_share(&mut st, reader, bytes, waited))
    }

    /// The mutant's order (`mutant_live_reader_post_alloc`): the decoded frame waits while more than the budget
    /// is held, then is added, so up to the budget plus one frame is queued and one more is decoded.
    fn admit_late(self: &Arc<Self>, reader: &Arc<ReaderFrames>, bytes: usize) -> BlobTicket {
        let mut st = self.lock();
        let mut waited = false;
        while !st.closed && st.held > self.budget {
            waited = true;
            st = self.freed.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
        self.take_share(&mut st, reader, bytes, waited)
    }

    /// Count one admitted frame (under the lock).
    fn take_share(
        self: &Arc<Self>,
        st: &mut GateState,
        reader: &Arc<ReaderFrames>,
        bytes: usize,
        waited: bool,
    ) -> BlobTicket {
        st.held += bytes;
        st.frames += 1;
        let mine = reader.0.load(Ordering::Relaxed) + 1;
        reader.0.store(mine, Ordering::Relaxed);
        let c = &self.counters;
        c.reader_blob_held.store(st.held as u64, Ordering::Relaxed);
        c.reader_blob_peak
            .fetch_max(st.held as u64, Ordering::Relaxed);
        c.reader_blob_admitted
            .fetch_add(bytes as u64, Ordering::Relaxed);
        c.reader_frames_held
            .store(st.frames as u64, Ordering::Relaxed);
        c.reader_frames_peak
            .fetch_max(mine as u64, Ordering::Relaxed);
        c.reader_frames_admitted.fetch_add(1, Ordering::Relaxed);
        if waited {
            c.reader_blob_waits.fetch_add(1, Ordering::Relaxed);
        }
        BlobTicket {
            gate: self.clone(),
            reader: reader.clone(),
            bytes,
            live: true,
        }
    }

    /// One frame of `reader` and its `bytes` leave the bounds.
    fn release(&self, reader: &ReaderFrames, bytes: usize) {
        let mut st = self.lock();
        debug_assert!(st.held >= bytes && st.frames >= 1, "a share freed twice");
        st.held = st.held.saturating_sub(bytes);
        st.frames = st.frames.saturating_sub(1);
        let mine = reader.0.load(Ordering::Relaxed);
        debug_assert!(mine >= 1, "a reader's frame freed twice");
        reader.0.store(mine.saturating_sub(1), Ordering::Relaxed);
        let c = &self.counters;
        c.reader_blob_held.store(st.held as u64, Ordering::Relaxed);
        c.reader_frames_held
            .store(st.frames as u64, Ordering::Relaxed);
        drop(st);
        self.freed.notify_all();
    }

    /// The supervisor's receiver is gone: wake every waiting reader to end.
    fn close(&self) {
        if !CLOSE_ON_DISCONNECT {
            return;
        }
        self.lock().closed = true;
        self.freed.notify_all();
    }

    #[cfg(test)]
    fn held(&self) -> usize {
        self.lock().held
    }

    #[cfg(test)]
    fn frames(&self) -> usize {
        self.lock().frames
    }
}

/// One admitted frame's share of [`BlobGate`] (its frame and its blob bytes): redeemed by the supervisor when it
/// takes the frame, freed on drop otherwise.
struct BlobTicket {
    gate: Arc<BlobGate>,
    reader: Arc<ReaderFrames>,
    bytes: usize,
    live: bool,
}

impl BlobTicket {
    /// The supervisor took the frame.
    fn redeem(mut self) {
        self.free();
    }

    fn free(&mut self) {
        if std::mem::take(&mut self.live) {
            self.gate.release(&self.reader, self.bytes);
        }
    }
}

impl Drop for BlobTicket {
    fn drop(&mut self) {
        if FREE_ON_DROP {
            self.free();
        }
    }
}

/// The supervisor's inbound receiver: dropping it (the supervisor ended, or never started) closes the blob gate, so a
/// reader waiting on it ends rather than waiting for bytes no one will free.
struct InboundRx {
    rx: Receiver<Inbound>,
    gate: Arc<BlobGate>,
}

impl Drop for InboundRx {
    fn drop(&mut self) {
        self.gate.close();
    }
}

/// The reader thread: frames and validates the child's output; a protocol error or EOF ends this incarnation. Each
/// frame is admitted by D6's bounds from its header, before the decoder allocates its body ([`BlobGate`]); the reader
/// ends without a word when the supervisor's receiver is gone.
fn reader(
    mut sock: runecraft_ipc::channel::ChanRx,
    inc: u64,
    tx: SyncSender<Inbound>,
    gate: Arc<BlobGate>,
) {
    let mut dec = codec::Decoder::new(Direction::ToCore);
    let mut buf = vec![0u8; 64 * 1024];
    let mut fds: Vec<runecraft_ipc::channel::Transferable> = Vec::new();
    // This reader's frames in flight (D6's frame bound is per reader).
    let mine = Arc::new(ReaderFrames::default());
    // The share of the frame the decoder is filling: it leaves with the frame, or is freed (dropped) when the reader
    // ends mid-frame.
    let filling: Cell<Option<BlobTicket>> = Cell::new(None);
    // The supervisor's receiver is gone (the gate closed, or a send failed).
    let gone = Cell::new(false);
    let why = loop {
        let n = match sock.recv(&mut buf, &mut fds, None) {
            Ok(runecraft_ipc::channel::RecvStatus::Eof) => {
                break if dec.at_boundary() {
                    ReaderEnd::Eof
                } else {
                    ReaderEnd::EofMidFrame
                };
            }
            Ok(runecraft_ipc::channel::RecvStatus::Data(n)) => n,
            // No deadline: a timeout is never seen (the read blocks until data or the end).
            Ok(runecraft_ipc::channel::RecvStatus::TimedOut) => continue,
            Err(e) => break ReaderEnd::Recv(e.to_string()),
        };
        if !fds.is_empty() {
            fds.clear(); // closes them: the child never sends descriptors
            break ReaderEnd::Descriptors;
        }
        let r = dec.feed_admitting(
            &buf[..n],
            &mut |h| {
                if gone.get() {
                    return false;
                }
                if !ADMIT_BEFORE_ALLOC {
                    return true;
                }
                match gate.admit(&mine, h.blob_len as usize) {
                    Admission::Admitted(t) => {
                        filling.set(Some(t));
                        true
                    }
                    Admission::Closed => {
                        gone.set(true);
                        false
                    }
                    Admission::TooLarge => false,
                }
            },
            &mut |f| {
                let t = if ADMIT_BEFORE_ALLOC {
                    filling.take().expect("every admitted frame has its share")
                } else {
                    gate.admit_late(&mine, f.blob.len())
                };
                // Sent at once, never collected: a frame held here would count against the bounds while the reader
                // waits to admit the next, and could never be freed.
                if !gone.get() && tx.send(Inbound::Frame(inc, f, t)).is_err() {
                    gone.set(true);
                }
            },
        );
        if gone.get() {
            return;
        }
        if let Err(e) = r {
            break ReaderEnd::Refused(e);
        }
    };
    drop(filling.take());
    // The end is queued BEFORE the socket closes (H-4): a child that exits on the EOF this shutdown gives it cannot
    // have its reaper's report reach the supervisor first, so a refusal is always the death's first sign.
    let _ = tx.send(Inbound::ReaderDone(inc, why));
    let _ = sock.shutdown();
}

/// The bench seam (feature `bench-seams`): D6's reader bounds
/// ([`BlobGate`], [`ReaderFrames`], [`BlobTicket`]) and the shipped reader thread ([`reader`]), for
/// a reader bench. It adds no behaviour: every call is the shipped function, so the bench times the code the
/// supervisor runs. Enabled only by this crate's own dev-dependency, so no `cargo build` of a binary has it.
#[cfg(feature = "bench-seams")]
pub mod bench_seam {
    use super::*;

    /// A [`BlobGate`] of `budget` bytes, with its own counters.
    pub struct Gate(Arc<BlobGate>);

    /// One reader's frames in flight ([`ReaderFrames`]).
    pub struct Share(Arc<ReaderFrames>);

    /// One admitted frame's share of the gate ([`BlobTicket`]).
    pub struct Ticket(BlobTicket);

    impl Gate {
        /// A gate of `budget` bytes, with its own zeroed counters.
        pub fn new(budget: usize) -> Gate {
            Gate(BlobGate::new(budget, Arc::new(ControlCounters::default())))
        }

        /// A new reader's share (its own frame bound, [`READER_FRAMES`]).
        pub fn reader(&self) -> Share {
            Share(Arc::new(ReaderFrames::default()))
        }

        /// [`BlobGate::admit`] for a header that declares `bytes` of blob (it waits while either bound is full).
        /// None when the gate refused it: closed, or the blob alone passes the budget.
        pub fn admit(&self, reader: &Share, bytes: usize) -> Option<Ticket> {
            match self.0.admit(&reader.0, bytes) {
                Admission::Admitted(t) => Some(Ticket(t)),
                Admission::Closed | Admission::TooLarge => None,
            }
        }
    }

    impl Ticket {
        /// [`BlobTicket::redeem`]: the supervisor took the frame.
        pub fn redeem(self) {
            self.0.redeem();
        }
    }

    /// The shipped reader ([`reader`], the reader thread's body) on one end of a socket pair. The bench is the child on the
    /// other end ([`ReaderPipe::write`]) and the supervisor ([`ReaderPipe::take`]: it takes each frame off the
    /// channel and redeems its ticket, as `Supervisor` does).
    pub struct ReaderPipe {
        child: Option<UnixStream>,
        inbox: Option<InboundRx>,
        thread: Option<JoinHandle<()>>,
    }

    impl ReaderPipe {
        /// Start the shipped reader on one end of a new socket pair, behind a gate of `budget` bytes.
        pub fn start(budget: usize) -> std::io::Result<ReaderPipe> {
            let (ours, child) = runecraft_ipc::channel::channel_pair()?;
            let (_, ours) = ours.bind(&runecraft_ipc::process::testing::Unspawned(0))?;
            let child = UnixStream::from(runecraft_ipc::channel::unix::ChildEndExt::into_fd(child));
            let gate = BlobGate::new(budget, Arc::new(ControlCounters::default()));
            let (tx, rx) = sync_channel(COMMAND_BOUND + READER_FRAMES + 16);
            let g = gate.clone();
            let thread = std::thread::Builder::new()
                .name("bench-bridge-read".into())
                .spawn(move || reader(ours, 1, tx, g))?;
            Ok(ReaderPipe {
                child: Some(child),
                inbox: Some(InboundRx { rx, gate }),
                thread: Some(thread),
            })
        }

        /// Write `bytes` (whole frames) as the child does.
        pub fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
            match self.child.as_mut() {
                Some(c) => c.write_all(bytes),
                None => Err(std::io::ErrorKind::NotConnected.into()),
            }
        }

        /// Take the next frame as the supervisor does, redeem its ticket and drop the frame; its blob's length.
        pub fn take(&self, timeout: Duration) -> Result<usize, String> {
            let Some(inbox) = self.inbox.as_ref() else {
                return Err("closed".into());
            };
            match inbox.rx.recv_timeout(timeout) {
                Ok(Inbound::Frame(_, f, t)) => {
                    t.redeem();
                    Ok(f.blob.len())
                }
                Ok(Inbound::ReaderDone(_, e)) => Err(format!("the reader ended: {e:?}")),
                Ok(_) => Err("not a frame".into()),
                Err(e) => Err(format!("no frame: {e:?}")),
            }
        }
    }

    impl Drop for ReaderPipe {
        /// The receiver first (it closes the gate), then the child's end (the reader sees EOF), then the join.
        fn drop(&mut self) {
            drop(self.inbox.take());
            if let Some(c) = self.child.take() {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// One `log` event frame (child → supervisor) with a `blob`-byte blob, encoded as the child encodes it.
    pub fn log_frame(blob: usize) -> Vec<u8> {
        codec::encode_msg(
            kind::LOG,
            codec::FF_EVENT,
            0,
            &codec::Log::new("info".into(), "bench".into()),
            &vec![0xa5; blob],
        )
    }
}

/// One `bridge.*` record per supervisor event (debug; a death, a quarantine or exhaustion at warn), each
/// with the `chain` key where it has one (the merge tool joins the child's records on `chain` and `mono`). Plugin
/// names are hashed (D8).
impl Supervisor {
    fn log_event(&self, e: &Event) {
        match e {
            Event::SlotNoticesChanged { .. } => {}
            Event::ChildUp {
                pid, incarnation, ..
            } => {
                rec!(
                    self,
                    Level::Info,
                    "bridge.childUp",
                    pid = *pid,
                    incarnation = *incarnation
                )
            }
            Event::ChildExited { pid, status } => {
                let level = if status == "Exited(0)" {
                    Level::Info
                } else {
                    Level::Warn
                };
                rec!(
                    self,
                    level,
                    "bridge.childExited",
                    pid = *pid,
                    status = status.as_str()
                )
            }
            Event::ChainReady {
                key,
                slots,
                incarnation,
                ..
            } => rec!(
                self,
                Level::Debug,
                "bridge.chainReady",
                chain = *key,
                slots = slots.len(),
                incarnation = *incarnation
            ),
            Event::ChainDown { key, reason } => rec!(
                self,
                Level::Warn,
                "bridge.chainDown",
                chain = *key,
                reason = reason.wire_word()
            ),
            Event::Quarantined {
                key,
                slot,
                plugin,
                op,
            } => rec!(
                self,
                Level::Warn,
                "bridge.quarantined",
                chain = *key,
                slot = *slot,
                plugin = Value::Hashed(plugin.clone()),
                op = op.as_str()
            ),
            Event::EditorClosed { key, slot } => rec!(
                self,
                Level::Debug,
                "bridge.editorClosed",
                chain = *key,
                slot = *slot
            ),
            Event::EditorGone { key, slot } => rec!(
                self,
                Level::Info,
                "bridge.editorGone",
                chain = *key,
                slot = *slot
            ),
            Event::EditorUnavailable { key, slot } => rec!(
                self,
                Level::Info,
                "bridge.editorUnavailable",
                chain = *key,
                slot = *slot
            ),
            // Windows only (protocol 4.1): never logged on unix, where no child sends a container.
            Event::EditorWindow { key, slot, .. } => rec!(
                self,
                Level::Info,
                "bridge.editorWindow",
                chain = *key,
                slot = *slot
            ),
            Event::HostRestarted { reason, slots } => rec!(
                self,
                Level::Warn,
                "bridge.hostRestarted",
                reason = reason.wire_word(),
                slots = slots.len()
            ),
            Event::Exhausted => rec!(self, Level::Error, "bridge.exhausted"),
            Event::Parked { key } => rec!(self, Level::Debug, "bridge.parked", chain = *key),
            Event::Unparked { key, group } => {
                rec!(
                    self,
                    Level::Debug,
                    "bridge.unparked",
                    chain = *key,
                    group = *group
                )
            }
            Event::StateApplied {
                key,
                slot,
                generation,
                ok,
                ..
            } => rec!(
                self,
                Level::Debug,
                "bridge.stateApplied",
                chain = *key,
                slot = *slot,
                generation = *generation,
                ok = *ok
            ),
            Event::StateRefused {
                key,
                slot,
                generation,
                ..
            } => rec!(
                self,
                Level::Warn,
                "bridge.stateRefused",
                chain = *key,
                slot = *slot,
                generation = *generation
            ),
            Event::SlotReactivated {
                key, slot, stall, ..
            } => rec!(
                self,
                Level::Info,
                "bridge.slotReactivated",
                chain = *key,
                slot = *slot,
                stall = *stall
            ),
            Event::DepthChange { key, .. } => {
                rec!(self, Level::Debug, "bridge.depthChange", chain = *key)
            }
            Event::PreparedFresh { key, ok, ms, .. } => rec!(
                self,
                if *ok { Level::Info } else { Level::Warn },
                "bridge.preparedFresh",
                chain = *key,
                ok = *ok,
                ms = *ms
            ),
            Event::LatencyChanged { key, .. } => {
                rec!(self, Level::Debug, "bridge.latencyChanged", chain = *key)
            }
            Event::StateRead { key, slot, .. }
            | Event::ParamList { key, slot, .. }
            | Event::ParamSet { key, slot, .. } => {
                rec!(
                    self,
                    Level::Trace,
                    "bridge.read",
                    chain = *key,
                    slot = *slot
                )
            }
            Event::StateTooLarge {
                key, slot, bytes, ..
            } => rec!(
                self,
                Level::Info,
                "bridge.stateTooLarge",
                chain = *key,
                slot = *slot,
                bytes = *bytes
            ),
            Event::Moved { slots } => rec!(self, Level::Trace, "bridge.moved", slots = slots.len()),
            Event::Burst { token, .. } => {
                rec!(self, Level::Debug, "bridge.burst", token = *token)
            }
            Event::WorkgroupAttached { ok, joined, .. } => {
                rec!(
                    self,
                    Level::Debug,
                    "bridge.workgroup",
                    ok = *ok,
                    joined = *joined
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spawned host's log variables: the application's values under the host's own
    /// names, a stale value of those names replaced, every other variable kept; with no application log, the
    /// environment as given. Mutant `mutant_child_log_env_unwritten` fails it.
    #[test]
    fn the_host_gets_the_log_values_under_its_own_names() {
        struct Levels;
        impl LevelSource for Levels {
            fn level_state(&self) -> LevelState {
                NoChildLog.level_state()
            }
            fn note_child_log_set(&self, _: ChildLogSet) {}
            fn child_args(&self) -> Vec<OsString> {
                Vec::new()
            }
            fn child_log_values(&self) -> Option<(OsString, OsString)> {
                Some(("wire=warn".into(), "00ff".into()))
            }
            fn describe(&self, word: u64) -> String {
                format!("{word:#x}")
            }
        }
        let given: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), "/h".into()),
            ("SUMMONER_LOG".into(), "stale".into()),
            ("APP_LOG".into(), "theirs".into()),
        ];
        let env = host_child_env(&Levels, given.clone());
        let get = |k: &str| {
            env.iter()
                .filter(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("SUMMONER_LOG"), vec![OsString::from("wire=warn")]);
        assert_eq!(get("SUMMONER_LOG_SALT"), vec![OsString::from("00ff")]);
        assert_eq!(get("HOME"), vec![OsString::from("/h")]);
        assert_eq!(get("APP_LOG"), vec![OsString::from("theirs")]);
        assert_eq!(host_child_env(&NoChildLog, given.clone()), given);
    }

    /// The hasher the application passes is the one every read is hashed with.
    #[test]
    fn config_keeps_the_given_state_hasher() {
        struct Fixed;
        impl StateHasher for Fixed {
            fn sha256_hex(&self, bytes: &[u8]) -> String {
                format!("fixed-{}", bytes.len())
            }
        }
        let c = Config::new(PathBuf::from("/x"), 48_000.0, Arc::new(Fixed));
        assert_eq!(c.state_hasher.sha256_hex(b"abc"), "fixed-3");
    }

    /// The same major is compatible whatever the minor (D20: additive); another major is not.
    #[test]
    fn the_hello_check_compares_the_protocol_major_only() {
        assert_eq!(ABI_VERSION >> 16, 4, "the core speaks 4.x");
        for ok in [4 << 16, 4 << 16 | 1, 4 << 16 | 7, 4 << 16 | 0xffff] {
            assert!(hello_compatible(ok), "{ok:#x}");
        }
        // A 3.x child is another major: refused (no 3.x path in protocol 4.0).
        for bad in [0, 1, 2 << 16, 3 << 16, 3 << 16 | 8, 5 << 16, u32::MAX] {
            assert!(!hello_compatible(bad), "{bad:#x}");
        }
    }

    /// A reply with no `protocol` decodes as 0 (`HelloReply` is `#[serde(default)]`), so it is a mismatch.
    #[test]
    fn a_hello_reply_without_a_protocol_decodes_as_0_and_is_a_mismatch() {
        let m: codec::HelloReply =
            codec::parse_meta(br#"{"pid": 5, "host": "fake", "toolkit": ""}"#).expect("decodes");
        assert_eq!(m.protocol, 0);
        assert!(!hello_compatible(m.protocol));
        // An error reply's meta, as the supervisor reads it at `hello`: no protocol either.
        let e: codec::HelloReply =
            codec::parse_meta(br#"{"code": -32601, "message": "no"}"#).expect("decodes");
        assert!(!hello_compatible(e.protocol));
    }

    /// H-3: a `hello` reply whose meta does not decode is refused by `check_to_core`, so it reaches `protocol_fault`.
    #[test]
    fn an_undecodable_hello_reply_is_refused_before_on_hello() {
        let bytes = codec::encode(
            kind::HELLO,
            codec::FF_REPLY,
            1,
            br#"{"protocol": "3.1"}"#,
            &[],
        );
        let mut d = codec::Decoder::new(Direction::ToCore);
        let mut frames = Vec::new();
        d.feed(&bytes, &mut |f| frames.push(f))
            .expect("the decoder takes it");
        assert_eq!(frames.len(), 1);
        assert!(codec::check_to_core(&frames[0]).is_err());
        assert!(frames[0].meta_as::<codec::HelloReply>().is_err());
    }

    // -----------------------------------------------------------------------------------------------------------------
    // D6's reader blob budget (branch `live-reader`): `reader` driven over a socket pair, this test the child (it
    // writes a few bounded frames) and the consumer (it holds the receiver, so it decides when bytes are freed).
    // -----------------------------------------------------------------------------------------------------------------

    const KIB: usize = 1024;
    const MIB: usize = 1024 * 1024;
    /// How long a reader is given to show it admits nothing more (it is blocked on the gate's condvar).
    const SETTLE: Duration = Duration::from_millis(150);

    fn flood_frame(blob: usize) -> Vec<u8> {
        codec::encode_msg(
            kind::LOG,
            codec::FF_EVENT,
            0,
            &codec::Log::new("info".into(), "flood".into()),
            &vec![0xa5; blob],
        )
    }

    struct ReaderRig {
        gate: Arc<BlobGate>,
        counters: Arc<ControlCounters>,
        inbox: Option<InboundRx>,
        child: Option<UnixStream>,
        reader: Option<JoinHandle<()>>,
    }

    impl ReaderRig {
        /// A reader with `budget`, and a child that writes `bytes` (from its own thread: its write blocks once the
        /// reader stops reading, and fails once the reader's end is closed).
        fn start(budget: usize, bytes: Vec<u8>) -> ReaderRig {
            let (ours, child) = runecraft_ipc::channel::channel_pair().expect("a channel pair");
            let (_, ours) = ours
                .bind(&runecraft_ipc::process::testing::Unspawned(0))
                .expect("bind");
            let child = UnixStream::from(runecraft_ipc::channel::unix::ChildEndExt::into_fd(child));
            let counters = Arc::new(ControlCounters::default());
            let gate = BlobGate::new(budget, counters.clone());
            let (tx, rx) = sync_channel(COMMAND_BOUND + READER_FRAMES + 16);
            let g = gate.clone();
            let reader = std::thread::Builder::new()
                .name("test-bridge-read".into())
                .spawn(move || reader(ours, 1, tx, g))
                .expect("spawn the reader");
            let mut w = child.try_clone().expect("clone");
            std::thread::spawn(move || {
                let _ = w.write_all(&bytes);
            });
            ReaderRig {
                inbox: Some(InboundRx {
                    rx,
                    gate: gate.clone(),
                }),
                gate,
                counters,
                child: Some(child),
                reader: Some(reader),
            }
        }

        fn flood(budget: usize, blobs: &[usize]) -> ReaderRig {
            ReaderRig::start(budget, blobs.iter().flat_map(|&b| flood_frame(b)).collect())
        }

        fn rx(&self) -> &Receiver<Inbound> {
            &self.inbox.as_ref().expect("the receiver").rx
        }

        /// The next frame, its ticket kept (it stays in flight until the test redeems or drops the ticket).
        fn take(&self) -> (Frame, BlobTicket) {
            match self.rx().recv_timeout(Duration::from_secs(5)) {
                Ok(Inbound::Frame(1, f, t)) => (f, t),
                Ok(Inbound::ReaderDone(_, e)) => panic!("the reader ended: {e:?}"),
                Ok(_) => panic!("not a frame"),
                Err(e) => panic!("no frame: {e:?}"),
            }
        }

        fn end(&self) -> ReaderEnd {
            match self.rx().recv_timeout(Duration::from_secs(5)) {
                Ok(Inbound::ReaderDone(1, e)) => e,
                Ok(Inbound::Frame(..)) => panic!("a frame, not the end"),
                Ok(_) => panic!("not the end"),
                Err(e) => panic!("no end: {e:?}"),
            }
        }

        /// The reader reaches `held` and admits nothing more: the held bytes stay `held` over [`SETTLE`]. (Only the
        /// reader adds bytes meanwhile, so it waits for at least `held`, then asserts exactly `held`.)
        fn settled_at(&self, held: usize) {
            wait_for(
                || self.gate.held() >= held,
                "the reader holds the expected bytes",
            );
            std::thread::sleep(SETTLE);
            assert_eq!(self.gate.held(), held, "held bytes after the settle");
        }

        /// The reader reaches `frames` in flight and admits nothing more over [`SETTLE`] (as [`Self::settled_at`]).
        fn frames_settled_at(&self, frames: usize) {
            wait_for(
                || self.gate.frames() >= frames,
                "the reader holds the expected frames",
            );
            std::thread::sleep(SETTLE);
            assert_eq!(
                self.gate.frames(),
                frames,
                "frames in flight after the settle"
            );
        }

        /// No frame is queued (every admitted one was taken).
        fn none_queued(&self) {
            assert!(
                matches!(
                    self.rx().try_recv(),
                    Err(std::sync::mpsc::TryRecvError::Empty)
                ),
                "no frame past the admitted ones"
            );
        }

        /// The reader thread is finished within 1 s of now; joined.
        fn joined_within_1s(&mut self) {
            let r = self.reader.take().expect("the reader");
            let until = std::time::Instant::now() + Duration::from_secs(1);
            while !r.is_finished() && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(r.is_finished(), "the reader thread did not end within 1 s");
            r.join().expect("the reader did not panic");
        }
    }

    impl Drop for ReaderRig {
        fn drop(&mut self) {
            // Unblocks a child write the reader no longer reads (the test thread never waits on it).
            if let Some(c) = self.child.take() {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
        let until = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < until, "timed out: {what}");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Fix 1 and 2: a flood of 8 × 256 KiB against a 1 MiB budget is held at EXACTLY the budget (4 frames), never
    /// one frame more: the 5th header waits before its body is allocated. Mutant `mutant_live_reader_post_alloc`
    /// holds 1.25 MiB (the old wait-while-over, then add) and fails here.
    #[test]
    fn the_reader_holds_a_flood_at_exactly_its_budget() {
        let rig = ReaderRig::flood(MIB, &[256 * KIB; 8]);
        rig.settled_at(MIB);
        assert_eq!(
            rig.counters.reader_blob_held.load(Ordering::Relaxed),
            MIB as u64
        );
        assert_eq!(
            rig.counters.reader_blob_peak.load(Ordering::Relaxed),
            MIB as u64,
            "never more than the budget"
        );
        let mut held = Vec::new();
        for _ in 0..4 {
            let (f, t) = rig.take();
            assert_eq!(f.blob.len(), 256 * KIB);
            held.push(t);
        }
        rig.none_queued();
        rig.settled_at(MIB);
        rig.none_queued();
    }

    /// Fix 1 and 2: freeing one frame admits exactly the next admissible frame, at the boundary (`held + declared ≤
    /// budget`): 512 + 256 KiB held, the next 512 KiB waits; the 256 KiB frame freed, it is admitted (exactly 1 MiB)
    /// and the 128 KiB after it waits; the first 512 KiB freed, the 128 KiB is admitted.
    #[test]
    fn freeing_one_frame_admits_exactly_the_next_admissible_frame() {
        let rig = ReaderRig::flood(MIB, &[512 * KIB, 256 * KIB, 512 * KIB, 128 * KIB]);
        rig.settled_at(768 * KIB);
        let (a, ta) = rig.take();
        let (b, tb) = rig.take();
        assert_eq!((a.blob.len(), b.blob.len()), (512 * KIB, 256 * KIB));
        rig.none_queued();
        rig.settled_at(768 * KIB);
        rig.none_queued();
        tb.redeem();
        let (c, tc) = rig.take();
        assert_eq!(c.blob.len(), 512 * KIB, "exactly the next admissible frame");
        rig.settled_at(MIB);
        rig.none_queued();
        assert_eq!(rig.counters.reader_blob_waits.load(Ordering::Relaxed), 1);
        ta.redeem();
        let (d, td) = rig.take();
        assert_eq!(d.blob.len(), 128 * KIB);
        rig.settled_at(640 * KIB);
        rig.none_queued();
        assert_eq!(
            rig.counters.reader_blob_peak.load(Ordering::Relaxed),
            MIB as u64
        );
        drop((tc, td));
        assert_eq!(rig.gate.held(), 0);
    }

    /// Fix 3: the receiver dropped while the reader is stalled at its budget (the frames still queued) ends the reader
    /// within 1 s, and the queued frames' bytes are freed with them (no leak). Mutant
    /// `mutant_live_reader_leak_on_drop` leaves 1 MiB held and fails here.
    #[test]
    fn a_stalled_reader_ends_when_the_receiver_is_dropped() {
        let mut rig = ReaderRig::flood(MIB, &[256 * KIB; 8]);
        rig.settled_at(MIB);
        drop(rig.inbox.take());
        rig.joined_within_1s();
        assert_eq!(rig.gate.held(), 0, "the dropped frames freed their bytes");
        assert_eq!(rig.counters.reader_blob_held.load(Ordering::Relaxed), 0);
    }

    /// Fix 3, the receiver's own signal: the consumer still holds every admitted byte (so no free can wake the reader)
    /// and drops the receiver: the reader ends within 1 s. Mutant `mutant_live_reader_no_close` never wakes it
    /// and fails here.
    #[test]
    fn a_stalled_reader_ends_when_the_receiver_is_dropped_while_its_bytes_are_held() {
        let mut rig = ReaderRig::flood(MIB, &[256 * KIB; 8]);
        rig.settled_at(MIB);
        let tickets: Vec<_> = (0..4).map(|_| rig.take().1).collect();
        rig.none_queued();
        rig.settled_at(MIB);
        drop(rig.inbox.take());
        rig.joined_within_1s();
        assert_eq!(
            rig.gate.held(),
            MIB,
            "the consumer's frames are still its own"
        );
        drop(tickets);
        assert_eq!(rig.gate.held(), 0);
    }

    /// The reservation of a frame the reader never finished (EOF mid-body) is freed when the reader ends: admitted
    /// from its header (the peak shows it), returned at the end. Mutant `mutant_live_reader_leak_on_drop` fails.
    #[test]
    fn a_reservation_is_freed_when_the_reader_ends_mid_frame() {
        let mut half = flood_frame(512 * KIB);
        half.truncate(half.len() / 2);
        let mut rig = ReaderRig::start(MIB, half);
        wait_for(
            || rig.counters.reader_blob_peak.load(Ordering::Relaxed) == 512 * KIB as u64,
            "the header admitted",
        );
        rig.child
            .take()
            .expect("the child")
            .shutdown(std::net::Shutdown::Both)
            .ok();
        assert!(matches!(rig.end(), ReaderEnd::EofMidFrame));
        rig.joined_within_1s();
        assert_eq!(rig.gate.held(), 0);
    }

    /// A blob past the whole budget (a test budget below the codec's cap) is refused from its header, a protocol
    /// fault, never a reader waiting forever; nothing of it was admitted.
    #[test]
    fn a_blob_past_the_whole_budget_is_refused_not_waited_for() {
        let mut rig = ReaderRig::flood(MIB, &[2 * MIB]);
        match rig.end() {
            ReaderEnd::Refused(codec::ProtoError::NotAdmitted { kind: k, blob_len }) => {
                assert_eq!((k, blob_len as usize), (kind::LOG, 2 * MIB));
            }
            e => panic!("refused from its header: {e:?}"),
        }
        rig.joined_within_1s();
        assert_eq!(rig.counters.reader_blob_peak.load(Ordering::Relaxed), 0);
    }

    /// The control: traffic within the budget (frames with and without blobs) passes in order and unchanged, each
    /// frame's bytes freed as it is taken; the end is a clean EOF; no admission waited.
    #[test]
    fn traffic_within_the_budget_passes_unchanged() {
        let blobs = [0, 3, 0, 64 * KIB, 100_000, 0, 1];
        let mut rig = ReaderRig::flood(MIB, &blobs);
        for &b in &blobs {
            let (f, t) = rig.take();
            assert_eq!(f.header.kind, kind::LOG);
            assert_eq!(f.blob, vec![0xa5; b]);
            t.redeem();
        }
        rig.child
            .take()
            .expect("the child")
            .shutdown(std::net::Shutdown::Both)
            .ok();
        assert!(matches!(rig.end(), ReaderEnd::Eof));
        rig.joined_within_1s();
        assert_eq!((rig.gate.held(), rig.gate.frames()), (0, 0));
        assert_eq!(rig.counters.reader_blob_waits.load(Ordering::Relaxed), 0);
    }

    /// D6's frame bound: a flood of 300 META-ONLY frames (no blob, so the byte budget never
    /// binds) is held at EXACTLY 256 in flight for the reader, the 257th waiting at its header. Mutant
    /// `mutant_live_reader_no_frame_bound` admits all 300 and fails here.
    #[test]
    fn the_reader_holds_a_meta_only_flood_at_exactly_its_frame_bound() {
        let rig = ReaderRig::flood(MIB, &[0; 300]);
        rig.frames_settled_at(READER_FRAMES);
        assert_eq!(rig.gate.held(), 0, "no blob bytes");
        assert_eq!(
            rig.counters.reader_frames_peak.load(Ordering::Relaxed),
            READER_FRAMES as u64,
            "never more than the bound"
        );
        let tickets: Vec<_> = (0..READER_FRAMES).map(|_| rig.take().1).collect();
        rig.none_queued();
        rig.frames_settled_at(READER_FRAMES);
        rig.none_queued();
        drop(tickets);
    }

    /// The frame bound: freeing one frame admits exactly one more (twice), and the rest wait.
    #[test]
    fn freeing_one_frame_at_the_frame_bound_admits_exactly_one_more() {
        let rig = ReaderRig::flood(MIB, &[0; 300]);
        rig.frames_settled_at(READER_FRAMES);
        let mut tickets: VecDeque<_> = (0..READER_FRAMES).map(|_| rig.take().1).collect();
        rig.none_queued();
        let waits = rig.counters.reader_blob_waits.load(Ordering::Relaxed);
        for _ in 0..2 {
            tickets.pop_front().expect("a ticket").redeem();
            let (f, t) = rig.take();
            assert!(f.blob.is_empty());
            tickets.push_back(t);
            rig.frames_settled_at(READER_FRAMES);
            rig.none_queued();
        }
        assert_eq!(
            rig.counters.reader_blob_waits.load(Ordering::Relaxed),
            waits + 2,
            "each admitted after a wait"
        );
        assert_eq!(
            rig.counters.reader_frames_peak.load(Ordering::Relaxed),
            READER_FRAMES as u64
        );
    }

    /// The frame bound and the receiver's signal: the consumer holds every one of the 256 frames in flight and drops
    /// the receiver; the reader ends within 1 s. Then the frames are freed.
    #[test]
    fn a_reader_stalled_at_its_frame_bound_ends_when_the_receiver_is_dropped() {
        let mut rig = ReaderRig::flood(MIB, &[0; 300]);
        rig.frames_settled_at(READER_FRAMES);
        let tickets: Vec<_> = (0..READER_FRAMES).map(|_| rig.take().1).collect();
        rig.none_queued();
        drop(rig.inbox.take());
        rig.joined_within_1s();
        assert_eq!(rig.gate.frames(), READER_FRAMES);
        drop(tickets);
        assert_eq!(rig.gate.frames(), 0);
        assert_eq!(rig.counters.reader_frames_held.load(Ordering::Relaxed), 0);
    }

    /// The frame bound is each reader's own: a second reader admits while the first is stalled at its bound, on the
    /// same gate (the byte budget is the one shared bound).
    #[test]
    fn the_frame_bound_is_per_reader() {
        let first = ReaderRig::flood(MIB, &[0; 300]);
        first.frames_settled_at(READER_FRAMES);
        let (ours, child) = runecraft_ipc::channel::channel_pair().expect("a channel pair");
        let (_, ours) = ours
            .bind(&runecraft_ipc::process::testing::Unspawned(0))
            .expect("bind");
        let child = UnixStream::from(runecraft_ipc::channel::unix::ChildEndExt::into_fd(child));
        let (tx, rx) = sync_channel(16);
        let g = first.gate.clone();
        let second = std::thread::spawn(move || reader(ours, 2, tx, g));
        (&child).write_all(&flood_frame(0)).expect("write");
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Inbound::Frame(2, f, t)) => {
                assert!(f.blob.is_empty());
                t.redeem();
            }
            _ => panic!("the second reader's frame"),
        }
        child.shutdown(std::net::Shutdown::Both).ok();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(Inbound::ReaderDone(2, ReaderEnd::Eof))
        ));
        second.join().expect("the second reader");
        assert_eq!(first.gate.frames(), READER_FRAMES);
    }

    /// H-5: the no-blame reasons have their own words, never `crash`.
    #[test]
    fn the_reasons_wire_words() {
        assert_eq!(DownReason::Crash.wire_word(), "crash");
        assert_eq!(DownReason::Wedge.wire_word(), "wedge");
        assert_eq!(DownReason::Stall.wire_word(), "wedge");
        assert_eq!(DownReason::Protocol.wire_word(), "protocol");
        let i = DownReason::Incompatible {
            child_protocol: 2 << 16,
            core_protocol: ABI_VERSION,
        };
        assert_eq!(i.wire_word(), "incompatible");
    }

    #[test]
    fn a_parameter_name_is_cut_at_a_character_boundary() {
        let cut = |n: String| summoner_plugin::params::cut_chars(n, PARAM_NAME_CHARS);
        assert_eq!(cut("gain".into()), "gain");
        assert_eq!(cut("x".repeat(1024)).len(), 1024);
        assert_eq!(cut("x".repeat(1025)).len(), 1024);
        // characters, not bytes.
        assert_eq!(cut("é".repeat(1025)).chars().count(), 1024);
        assert_eq!(summoner_plugin::params::cut_chars("ééé".into(), 2), "éé");
    }

    #[test]
    fn a_pre_3_7_entry_keeps_only_its_id_and_name() {
        let e = {
            let mut msg = codec::ParamEntry::default();
            msg.id = 5;
            msg.name = "Cutoff".into();
            msg.plugin_id = 0;
            msg.automatable = true;
            msg.default = 2.0;
            msg.value = -1.0;
            msg.step_count = 3;
            msg.label = "Hz".into();
            msg.order = 4;
            msg
        };
        let old = clamp_entry(e.clone(), false);
        assert_eq!(
            (old.id, old.name.as_str(), old.automatable, old.order),
            (5, "Cutoff", false, 0)
        );
        let new = clamp_entry(e, true);
        assert_eq!(
            (new.default, new.value, new.order, new.automatable),
            (1.0, 0.0, 4, true)
        );
    }
}
