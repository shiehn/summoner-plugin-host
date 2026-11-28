//! The fake host: the Rust test double of the plugin-host child, so the host application side is proven end to end
//! without a real plugin. It speaks the real protocol (fd 3, route B / A, the region, the hubs, the frames) with
//! trivial DSP:
//!
//! - a slot is `<fake name="..." gain="g" delay="n" synth="1" ignore-cc123="1" sustain="1" probe="1"
//!   crash-load="1" echo="e" feedback="f"/>` (`descriptionXml`): a gain, then (`echo`) a decaying delay of e frames
//!   with feedback f (`y = fma (f, line[pos], x)`, a long tail), a fixed delay of n frames (its
//!   latency), an instrument gate
//!   (`synth`: output `gain × 0.25` while any note is held: a count of note-ons less note-offs, reset by CC 123 /
//!   CC 120 unless `ignore-cc123`, a synth that ignores all-notes-off, so only note-offs release it; with `sustain`,
//!   a note let go while its channel's CC 64 is ≥ 64 stays held until that CC 64 drops below 64); `probe` makes the
//!   chain's output channel 0 the request line's transport as the child read it (frame 0 ppq, 1 bpm, 2 time-signature
//!   numerator, 3 denominator, 4 loop start ppq, 5 loop end ppq, 6 bar start ppq, 7 blockFlags, 8 timeSeconds,
//!   9 timeSamples, 10 outputHostTime; the rest 0), so a test sees the transport through the child (D2);
//! - `drone="1"` (an instrument at `gain × 0.25` on every frame, notes or not) and `ignore-bypass="1"` (a
//!   hostile slot that keeps processing while disabled); a resolved `<PLUGIN name="fake: drone=1 …">` carries its
//!   knobs in its `name`, `fake:` then space-separated `knob=value` (the host application's D16 XML escapes its values);
//! - `inputs="n"` / `outputs="n"` (default 2 / 2): the slot's bus counts, under the child's C++ channel
//!   rule around every called slot (the plugin-host child `chain.rs` `stage_inputs` / `finish_outputs`):
//!   `P = max(max(1, in), out)`; with one input and `P != 2` channel 0 is the
//!   average `(L + R) * 0.5`; channels at or past `inputs` are zero before the plugin runs, channels at or past
//!   `outputs` after it, and at `P == 1` channel 1 is a copy of channel 0;
//! - a slot that arrives with an empty `descriptionXml` is a quarantined slot (D7): it is not loaded, keeps its slot
//!   number, passes audio through and reports latency 0 and paramCount 0;
//! - *(protocol 3.7)* `xparams="name|auto|clump|steps|label|default;..."`: more parameters at raw indices 4..,
//!   plugin ids 100.. (gain, crash, wedge, burn are 10-13; only gain is automatable and settable); `param.list`
//!   reports the 3.7 fields to a 3.7 hello (the AU clump order, pages within 48 KiB), and `param.set` writes the
//!   value for the chain thread's next block and into the state body at once (`gain=<v>`, `p<id>=<v>`), moving the
//!   slot's generation;
//! - parameters through the region (D10), `id = slot << 24 | index`: 0 gain (0..1), 1 crash inside processBlock
//!   (≥ 0.5), 2 wedge inside processBlock (≥ 0.5, never returns), 3 CPU burn (value × 100 ms per block);
//! - states (`state.set` blobs) are framed state blobs, format v1: `{mode: sasp}` only (any other word, 3.0's `raw`
//!   included, is answered `ok: false`); a fake slot is a `{builtin}` test double, so a builtin body is applied
//!   and a both-absent vst3 blob is the counted no-op (`ok: true`, nothing reaches the plugin); any other format or a
//!   refused blob is `ok: false`. The body's knobs: `gain=<f>` sets the gain; `CRASH` aborts inside
//!   the state-set call; `SLOWGET=<ms>` makes the state-get call sleep on the message thread; `SLOWSET=<ms>`
//!   applies the state (and sends its receipt) that much later; `FAIL` is applied with `ok: false`; `BAREGET` makes
//!   `state.get` and `editor.closed` answer the bare body, unframed (a child the host application must refuse, D21);
//!   `RESTART=<n>` raises `restartComponent(kLatencyChanged)` with the slot's latency becoming n frames, and
//!   `CYCLEMS=<ms>` makes that cycle take so long; `LAGPROBE=2` (T17's probe) makes that cycle's
//!   give-back and its `slot.reactivated` count land while the chain thread is inside a bypassed block, so both that
//!   block's end and the return fade's land after the count: the report trails the host application's count by exactly
//!   [`summoner_plugin::handover::child_bypass_lag_blocks`] (2 at B >= FADE), deterministically; `LAGPROBE=3` does the
//!   same and also drops one earlier count (the mutant: one block past the window). `state.get` / `editor.closed`
//!   answer `{builtin}` of the body;
//! - D22, the reactivation (the native child's design, as the shared [`summoner_plugin::handover`] machine): the main
//!   loop takes the restart flag at its next turn (a 16 ms tick, and after every op), announces, takes the slot's
//!   processor out of its cell (forced past its bound), runs the cycle (a fresh plugin at the new latency: its voices
//!   and lines reset, as `setActive(false)` / `(true)` does), puts it back, and sends `slot.reactivated`, then
//!   `latency.changed` if the latency moved. Meanwhile that slot passes its bypass signal through (an effect's input
//!   delayed by its applied latency; an instrument's silence), with D22's raised-cosine fades, and the response line
//!   carries `ST_BYPASSED`; the other slots keep processing;
//! - *(protocol 3.8)* `chain.prepareFresh` on a render set: every slot's DSP made again on the kept plugin (its
//!   lines and held notes from zero, its first-use latch kept), then `latency.changed` if the latency moved, then
//!   `chain.preparedFresh`. Knobs: `firstuse=<g>` (the instance's first block of its life at gain g),
//!   `freshdelay=<n>` (each fresh prepare adds n frames to the slot's delay, its latency: a plugin whose latency
//!   moves on a re-prepare) and `crash-fresh=1` (the child dies inside the fresh prepare); `skip-after=<n>`
//!   (from the instance's block n of its life on, every block acked `ST_SKIPPED` with silence);
//! - *(protocol 3.9)* `notices=<code>,…` (the slot's `SlotInfo.notices` at `chain.loaded`) and
//!   `prepnotices=<code>,…` (gained at `chain.prepare`: `chain.prepared`'s `slotNotices` reports each slot's whole set);
//!
//! (KI-68): `--hist-modelled`: a block's `processHist` entry is its modelled cost (the burn it applied, the
//! knob's value x 100 ms), not the wall time measured around it, so a test's heavy and light blocks land in their
//! buckets whatever the machine's load (a light block descheduled mid-block measured >= 75 % of the period).
//! Flags: `--ignore-slot0` (force route A), `--mutant-reader-pongs` (the T11 mutation: the reader answers the
//! wedge ping), `--lie-quarantined` (a hostile child: a quarantined slot reports latency 999 and paramCount 77, which
//! the host application must not believe, D7), `--hang-at-hello` (a child wedged at start: it never answers `hello`; the
//! spawn-lock test), `--tc-us <period>` (chain threads under the time-constraint policy), `--stats <path>` (write every
//! chain thread's wake-to-run, `key hub ns` per line, against its group's ring stamp, at shutdown), `--turns <path>`
//! (a test-only record of D22's main-loop turns, one `begin` and one `end` line per turn with a take to make: see
//! [`TurnLog`]), and T17's two mutations of the handover: `--mutant-hard-switch` (no fade: a hard
//! switch into and out of the bypass) and `--mutant-no-delay-line` (the bypass signal is the slot's input undelayed).
//! The live-hello knobs: `--hello-protocol <u32>` (the `hello` reply's `protocol` in
//! place of `ABI_VERSION`), `--hello-undecodable` (a reply whose meta does not decode), `--hello-delay-ms <ms>` (the
//! reply sent that late from its own thread while the reader goes on), `--hello-script <path>` (per-`hello` overrides
//! a test writes between incarnations: [`HelloScript`]), `--frames <path>` (a test-only record of the frames received
//! and the hello reply: [`FrameLog`]), and the slot attribute `proto-load="refused|orphan|garbage|fds|midframe"` (a
//! protocol fault inside `chain.load` of that slot: [`protocol_fault_at_load`]).
//! D6's live reader blob budget (branch `live-reader`, 2026-11-01) adds `--flood <n>:<bytes>`: right after the
//! `hello` reply (from its own thread, so the reader goes on reading), n `log` EVENTs, each carrying a blob of
//! `bytes` bytes (the host application ignores a log's blob), n at most 16 and bytes at most `MAX_BLOB`; or, with `bytes` 0,
//! n at most 512 META-ONLY frames (about 80 bytes each: the reader's frame bound): a bounded
//! test of the host application reader's bounds, never a load. It prints `fake-host: flooding <n> frames of <bytes> blob bytes` to stderr
//! first and `fake-host: flood sent` after the last (a host application reader that stops reading leaves it blocked in a write).
//!
//! **protocol 3.6, as the plugin-host child's double** (when its `hello` claims 3.6, the default):
//! - the `hello` reply carries `wiredBytes` / `unwiredBytes` (its page wired in place, L1a), and `chain.prepared` the
//!   chain's pair again (its region's plus one page, the stand-in for a prepare's processor blocks, L1b);
//!   `--chain-loaded-v0` leaves both out, as for `chain.loaded`;
//! - `log.set` (kind 5, Inline) is answered on the reader with the codec's checks (`LogSet::check`) and check 5 (it was
//!   given `--log-dir`), then applied to this process's word; `--log-set-refuse` answers every one `-32602`,
//!   `--log-set-mismatch` answers a word one off from the one applied;
//! - a request's valid `corr` is echoed on its `log.set` reply and on the `chain.loaded` / `chain.unloaded` /
//!   `chain.prepared` it causes (the double's subset of L3's echo set), never on another event; `--corr-echo-bad`
//!   echoes `"bad corr!"` instead (outside D7's grammar);
//! - `--frames` adds `log.set levels <hex> expiresInMs <n|none>` per `log.set`, and `corr <kind> <corr|invalid>` for
//!   every received frame that carries the key.
//!
//! **editor windows (protocol 3.4), as the plugin-host child's double:** `--no-editor` (every slot `hasEditor: false`
//! in `chain.loaded`, and `editor.open` answered with `editor.availability`, no window); `--editor-user-close-ms <ms>`
//! (a slot's first open editor is closed by its close button that long after: `editor.closed` + state, no
//! `editor.close`; later opens stay); a
//! render set or a quarantined slot answers `editor.open` with `editor.availability`; a re-open of an open editor
//! moves nothing (brought to front); `--frames` adds `editor.open key <k> slot <n> guardReset <b>` per open; a
//! `chain.load` for a hosted key is refused (logged, nothing changes).
//!
//! ** (B4's spawn), as the plugin-host child (3a1c29e) does it:** `--crash-log <dir>` (both modes: absolute, canonical,
//! an existing folder, at most 512 bytes, else exit 2; the scan mode's `crashlog@` writes there) and `--step-ms <n>`
//! (1..=120 000, else exit 2; only the scan mode would read it), both checked before fd 3 is touched, so a refused
//! argv writes nothing on fd 3 (R6). A render set (`role: render`) is R1's: its region's flags must be 0 or exactly
//! agree with the role (`FLAG_RENDER == role render`, `FLAG_ACK_WAKE == FLAG_RENDER`, no other bit), else
//! `chain.loaded {ok: false}`; its chain thread waits on its own `seq`, never a hub, and wakes `ack` after every block
//! (D9), as the real child's.
//!
//! **`--scan --parent-pid <pid>`: the scan mode** (for `summoner_scan::scan`'s tests): one bundle, no
//! chains, the conversation host's scan child holds [V host `scan_mode.rs`]: `hello` → its reply; `scan` → its `{}`
//! receipt, then `scanned` (an EVENT, id 0) echoing the request; `shutdown` → its reply, exit 0; EOF → exit 0. The
//! argv must be exactly `--scan --parent-pid <our parent>` and `hello` must say `fds: 0`, else it exits 2 / 4 (as
//! host's usage and protocol exits), so a test proves the supervisor's spawn and `hello`. The parent watch exits 3
//! (both modes: the parent is read on `main`'s first line and launchd as the parent is gone, so an orphan
//! exits 3 at once, before fd 3 is touched). It runs only with the death switch [`DEATH_SWITCH_ENV`] `=kill` (else exit 2): every death of the
//! mode is a SIGKILL on itself or an exit, so no crash report is ever written. What it does is scripted by [`SCAN_SCRIPT_ENV`], comma-separated `key=value`:
//! - `end=` `ok` (the default) | `failed` (`scanned` with `failed {reason: process}`) | `mismatch` (`scanned` echoes
//!   another path) | `error-reply` (the scan is answered `-32602`, as host refuses a bad scan) | `scanned-reply`
//!   (`scanned` framed as a REPLY with the scan's id) | `malformed` (`scanned`, an EVENT within the meta cap, whose
//!   meta is not JSON) | `bad-protocol` (the `hello` reply says protocol 2.0) | `midframe` (after the receipt, the
//!   first half of the `scanned` frame, then SIGKILL on itself: a death that leaves a half frame) |
//!   `midframe-exit:<n>` (the same half frame, then exit n) |
//!   `exit:<n>@<phase>` | `kill@<phase>` (SIGKILL on itself) | `hang@<phase>` (sleeps until killed) |
//!   `crashlog@<phase>` (writes the pid-named crash log in `$HOME/Library/Logs` as host's handler does, ONLY if
//!   that directory exists, else the report to stderr with `crash log NOT written`; then SIGKILL on itself). Phases:
//!   `hello` (on `hello`, before its reply), `receipt` (after the receipt, before `scanned`), `scanned` (after
//!   `scanned`, before the `shutdown` reply). An `exit:75` first prints a step-deadline line, as host's does;
//! - `oversize=1`: the `scanned` line carries enough descriptions (`fake-scan-class-<i>`) that its meta passes
//!   `MAX_META` (as a shell bundle's can), framed by hand (the codec's encoder refuses it), with the
//!   flags and id `end=` gives it; the fake prints `fake-scan: oversized scanned <n> bytes` to stderr first;
//! - `flood=<n>:<bytes>`: after the receipt, before `scanned`, n
//!   `scanned` EVENTs (id 0, echoing the request, an `ok` with no descriptions), each carrying a blob of `bytes` bytes
//!   (n at most 16, bytes at most `MAX_BLOB`: a bounded test of the reader's blob budget, never a load). It prints
//!   `fake-scan: flooding <n> frames of <bytes> blob bytes` to stderr first and `fake-scan: flood sent` after the last
//!   (a reader that stops reading leaves it blocked in a write until the group kill);
//! - `stderr=<n>`: after the receipt, n bytes of stderr, then the line `fake-scan: stderr end`;
//! - `class=gain`: an `ok` describes the bundle as the lab gain's VST3 class (the class-id vectors: its
//!   class id and the ids derived from it on macOS), named by the bundle's stem, so the host application's index keys it; an
//!   AU scan (a `.component`) as the lab gain AU with its `auCodes` and `auVersion`, as a 3.5 child reports it;
//! - `form=meta|auto` (default `meta`; an older child): `auto` answers through the
//!   codec's own encoder (`encode_scanned`), form B if and only if the `scan` offered it, as the plugin-host child does; a
//!   `failed` line then carries its classes as `discovered` in the blob;
//! - `classes=<n>` (n 1..=20000): the line's classes are n synthetic VST3 classes `fake-scan-class-<i>` (valid class
//!   ids), on `ok` and, in form B, as a `failed` line's `discovered`;
//! - `end=aucodes`: `scanned` with `failed {reason: auCodes}` (the child's F4 refusal of some
//!   AudioComponents entries), its classes (`class=` / `classes=`) the bundle's good siblings;
//! - `protocol=<u32>`: the `hello` reply's protocol in place of `ABI_VERSION` (an older scan child);
//! - `kill-if=<text>`: a scan whose path contains `<text>` dies by SIGKILL on itself after the receipt;
//! - `sleep-ms=<n>` (n at most 5000): after the receipt, n ms before `scanned` (a slow probe);
//! - `grandchild=1`: after the receipt, start `/bin/sleep 30` (it stays in the group) and print
//!   `fake-scan: grandchild <pid> pgid <pgid>` to stderr;
//! - `escapee=1`: after the receipt, start itself again with `--scan-escapee`, which calls `setsid()` (a new
//!   session and group: outside the scan's group), writes its pid to `$HOME/escapee.pid` and sleeps at most 30 s; the
//!   parent prints `fake-scan: escapee <pid>` and waits 300 ms (so the supervisor's tree walk sees it) before going on.
//!   The test kills it by that recorded pid;
//! - `escapee-socket=1` (with `escapee=1`): the escapee is given the scan socket as its stdin (fd 3 itself is
//!   close-on-exec), as a plugin's helper that forked with fd 3 would hold it: the socket stays open after the leader
//!   dies, so the supervisor's reader sees no EOF (the drain's bound, branch `scan-drain`).
//!
//! **The lab's death switch** (shared with the plugin host): [`DEATH_SWITCH_ENV`] `=kill`, read once
//! at the start of `main`. Set (to exactly `kill`), every death knob above (`crash-load`, the crash parameter, the
//! `CRASH` state) ends the process with SIGKILL on itself (`die`) instead of `abort()`, so a test that needs only "the
//! child died" writes no crash report into the user's `~/Library/Logs/DiagnosticReports`. Unset (or any other value),
//! the knobs abort as they always did: the one test that keeps a real SIGABRT on purpose clears it. The host application never reads the variable; the bridge tests' harness puts it in the child's env.
// no std `eprintln!` / `println!` (they panic on a closed pipe): `runecraft_rt::safe_eprintln!` / `safe_println!`.
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use runecraft_ipc::bridge::sync::Waited;
use runecraft_ipc::bridge::{MachReceive, MachSend};
use runecraft_ipc::process::EntryParent;
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use summoner::supervisor::CHILD_LOG_DIR_FLAG;
use summoner_plugin::handover::{self, Cell, ChainSide, DelayLine, Fade};
use summoner_proto::codec::{self, Direction, FF_ERROR, FF_EVENT, FF_REPLY, Frame, kind};
use summoner_proto::layout::*;
use summoner_proto::state_blob;

/// The lab's death switch, the host's own ([`summoner_proto::host_names::LAB_DEATH_ENV`]): the fake's death knobs act
/// only when this variable is exactly `kill` (the module docs).
pub const DEATH_SWITCH_ENV: &str = summoner_proto::host_names::LAB_DEATH_ENV;
/// The scan mode's script: comma-separated `key=value` knobs (the module docs).
pub const SCAN_SCRIPT_ENV: &str = "SUMMONER_FAKE_SCAN";
use summoner_rt::region::{ChildPage, Region};

/// [`DEATH_SWITCH_ENV`] `=kill`, read once at the start of `main` (the module docs).
static LAB_DEATH_KILL: AtomicBool = AtomicBool::new(false);
/// `--crash-log <dir>` (the module docs), checked in `main`.
static CRASH_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// A usage error (exit 2), before fd 3 is touched (R6).
fn usage_exit(why: &str) -> ! {
    runecraft_rt::safe_eprintln!("fake-host: {why}");
    std::process::exit(2);
}

/// `--crash-log <dir>`, as the plugin-host child checks it (N2's 512-byte cap, then `resolve_explicit_dir`).
fn crash_log_arg(v: Option<&String>) -> std::path::PathBuf {
    let Some(v) = v else {
        usage_exit("--crash-log needs a folder")
    };
    if v.len() > 512 {
        usage_exit(&format!(
            "--crash-log: the folder is {} bytes (at most 512)",
            v.len()
        ));
    }
    match runecraft_ipc::process::unix::resolve_explicit_dir(std::path::Path::new(v)) {
        Ok((_, p)) => p,
        Err(e) => usage_exit(&format!("--crash-log: {e}")),
    }
}

/// `--step-ms <n>`, as the plugin-host child R5 checks it.
fn step_ms_arg(v: Option<&String>) -> u64 {
    match v.and_then(|v| v.parse::<u64>().ok()) {
        Some(n) if (1..=120_000).contains(&n) => n,
        _ => usage_exit(&format!("--step-ms: {v:?} is not 1..=120000 ms")),
    }
}

/// `--load-tokens`: waits (bounded: 60 s) until `dir` holds a file, then removes the first by name.
fn take_token(dir: &str) {
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();
        names.sort();
        if let Some(first) = names.first()
            && std::fs::remove_file(first).is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Every death knob's end: SIGKILL on itself under the death switch (no crash report, no unwinding), else
/// `abort()` (a real SIGABRT, as a crashing plugin's).
fn die() -> ! {
    if LAB_DEATH_KILL.load(Ordering::Relaxed) {
        runecraft_ipc::process::unix::sigkill_self();
    }
    std::process::abort()
}

#[derive(Default, Clone)]
struct Opts {
    ignore_slot0: bool,
    mutant_pongs: bool,
    lie_quarantined: bool,
    hang_at_hello: bool,
    tc_us: u64,
    stats: Option<String>,
    turns: Option<String>,
    mutant_hard_switch: bool,
    mutant_no_delay_line: bool,
    /// `--hello-protocol <u32>`: the `protocol` the `hello` reply sends in place of `ABI_VERSION` (the
    /// supervisor's major check).
    hello_protocol: Option<u32>,
    /// `--hello-undecodable`: the `hello` reply's meta does not decode as a `HelloReply` (H-3).
    hello_undecodable: bool,
    /// `--hello-delay-ms <ms>`: the `hello` reply is sent that late, from its own thread, while the reader goes on
    /// reading (the hello test: anything the host application sends meanwhile is recorded before the reply).
    hello_delay_ms: u64,
    /// `--prepare-fresh-ms <ms>`: `chain.prepareFresh` sleeps that long before it answers (a busy message
    /// thread; the host application's 10 s bound).
    prepare_fresh_ms: u64,
    /// `--load-gate <path>`: while that file exists, `chain.load` waits on the message thread before it loads
    /// (every later op waits behind it, as the child's serial loads do), so a test decides when chains become ready.
    load_gate: Option<String>,
    /// `--load-tokens <dir>`: each `chain.load` waits on the message thread until `<dir>` holds a file, and
    /// takes one (removes the first by name), so a test makes chains ready one at a time (bounded: 60 s).
    load_tokens: Option<String>,
    /// `--hello-script <path>`: read at every `hello`, so a test can change one incarnation's answer (see [`hello`]).
    hello_script: Option<String>,
    /// `--frames <path>`: the test-only record of the frames this child receives and its `hello` reply.
    frames: Option<String>,
    /// `--hist-modelled`: `processHist` from each block's modelled cost (the module docs).
    hist_modelled: bool,
    /// `--flood <n>:<bytes>`: n blob-carrying `log` events after the `hello` reply (the module docs).
    flood: Option<(usize, usize)>,
    /// `--no-editor`: every slot reports `hasEditor: false` in `chain.loaded` (protocol 3.4) and answers
    /// `editor.open` with `editor.availability` (no window), as the child's double without an `editor` attribute.
    no_editor: bool,
    /// `--editor-fails-at-open` (with `--no-editor`): the slots report `hasEditor: true` at load and say no
    /// only at `editor.open` (`editor.availability`), as a plugin that claims an editor and cannot make its view. It
    /// stands in for the 3.3 child (no `hasEditor`) the tests used before protocol 4.0, which has no 3.x path.
    editor_fails_at_open: bool,
    /// `--chain-loaded-v0`: answer `chain.loaded` as a child from before `wiredBytes` / `unwiredBytes`.
    chain_loaded_v0: bool,
    /// `--editor-user-close-ms <ms>`: a slot's first opened editor is closed by the "user" (its close button)
    /// that long after the open: `editor.closed` + its state, with no `editor.close` from the host application. Later opens of that
    /// slot stay open.
    editor_user_close_ms: Option<u64>,
    /// `--editor-window-hwnd <n>`: answer `editor.open` as a host-embedding (Windows) child does, its receipt carrying
    /// `editorWindow: {"hwnd": n}` (protocol 4.1). Absent, the receipt is `{}` as every unix child's.
    editor_window_hwnd: Option<u64>,
    /// `--log-dir <dir>` was given (the host application passes it when its drainer runs; check 5 of `log.set`).
    log_dir: bool,
    /// `--log-set-refuse`: every `log.set` is answered `-32602`.
    log_set_refuse: bool,
    /// `--log-set-mismatch`: a `log.set` reply names a word one off from the one applied.
    log_set_mismatch: bool,
    /// `--corr-echo-bad`: every echo is `"bad corr!"` (outside D7's grammar).
    corr_echo_bad: bool,
}

thread_local! {
    /// the `corr` of the op the message thread is running (set from [`Op::Echo`], cleared after the op).
    static ECHO: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The echo to add for `corr` (or the bad one, `--corr-echo-bad`) to an encoded meta.
fn echoed(meta: Vec<u8>, corr: Option<&str>, bad: bool) -> Vec<u8> {
    let Some(c) = corr else { return meta };
    if bad {
        let Ok(serde_json::Value::Object(mut o)) =
            serde_json::from_slice::<serde_json::Value>(&meta)
        else {
            return meta;
        };
        o.insert("corr".into(), serde_json::Value::String("bad corr!".into()));
        return serde_json::to_vec(&o).expect("an object encodes");
    }
    let mut m = meta;
    let _ = codec::with_corr(&mut m, c);
    m
}

static ECHO_BAD: AtomicBool = AtomicBool::new(false);
/// The level word an applied `log.set` left (the launch word, every area at warn, until one is applied). The fake runs
/// no drainer, so an expiry is never acted on.
static LEVELS: AtomicU64 = AtomicU64::new(0x2222_2222_2222_2222);

type Writer = Arc<Mutex<UnixStream>>;

fn send(w: &Writer, frame: Vec<u8>) {
    if let Ok(mut s) = w.lock() {
        let _ = s.write_all(&frame);
    }
}

fn reply<T: serde::Serialize>(w: &Writer, k: u16, id: u64, meta: &T, blob: &[u8]) {
    send(w, codec::encode_msg(k, FF_REPLY, id, meta, blob));
}

fn event<T: serde::Serialize>(w: &Writer, k: u16, meta: &T, blob: &[u8]) {
    // a completion event the message thread sends for an op that came with a `corr` echoes it.
    let completes = matches!(
        k,
        kind::CHAIN_LOADED | kind::CHAIN_UNLOADED | kind::CHAIN_PREPARED
    );
    let corr = if completes {
        ECHO.with(|e| e.borrow().clone())
    } else {
        None
    };
    let m = serde_json::to_vec(meta).expect("meta serialises");
    let m = echoed(m, corr.as_deref(), ECHO_BAD.load(Ordering::Relaxed));
    send(w, codec::encode(k, FF_EVENT, 0, &m, blob));
}

fn empty() -> serde_json::Value {
    serde_json::json!({})
}

/// Parameters per loaded fake plugin: gain, crash, wedge, burn.
const FAKE_PARAMS: u32 = 4;

/// (protocol 3.7): one parameter of a fake plugin as `param.list` reports it and `param.set` reaches it. The four
/// built-in ones (gain, crash, wedge, burn) have plugin ids 10-13; only gain is automatable and settable (the other
/// three are hostile region hooks). `xparams="name|auto|clump|steps|label|default;..."` adds more at raw indices 4..,
/// plugin ids 100..: `auto` 1 or 0, `clump` 0 for none (the order is the AU clump order, `params::au_clump_order`).
struct FakeParam {
    name: String,
    plugin_id: u32,
    automatable: bool,
    clump: Option<u32>,
    steps: u32,
    label: String,
    default: f32,
    settable: bool,
    /// A host set not yet applied by the chain thread, and its value.
    pending: AtomicU32,
    dirty: AtomicBool,
    /// The applied value of an `xparams` entry (gain's is [`Plugin::gain`]).
    applied: AtomicU32,
}

fn fake_params(x: &Option<codec::PluginRef>, gain: f32) -> Vec<FakeParam> {
    let base = |name: &str, id: u32, auto: bool, v: f32| FakeParam {
        name: name.into(),
        plugin_id: id,
        automatable: auto,
        clump: None,
        steps: 0,
        label: String::new(),
        default: v,
        settable: auto,
        pending: AtomicU32::new(v.to_bits()),
        dirty: AtomicBool::new(false),
        applied: AtomicU32::new(v.to_bits()),
    };
    let mut v = vec![
        base("gain", 10, true, gain.clamp(0.0, 1.0)),
        base("crash", 11, false, 0.0),
        base("wedge", 12, false, 0.0),
        base("burn", 13, false, 0.0),
    ];
    if let Some(spec) = attr(x, "xparams") {
        for (j, e) in spec.split(';').filter(|e| !e.is_empty()).enumerate() {
            let f: Vec<&str> = e.split('|').collect();
            let get = |i: usize| f.get(i).copied().unwrap_or("");
            let d: f32 = get(5).parse().unwrap_or(0.0);
            let clump: u32 = get(2).parse().unwrap_or(0);
            v.push(FakeParam {
                name: get(0).into(),
                plugin_id: 100 + j as u32,
                automatable: get(1) != "0",
                clump: (clump != 0).then_some(clump),
                steps: get(3).parse().unwrap_or(0),
                label: get(4).into(),
                default: d,
                settable: true,
                pending: AtomicU32::new(d.to_bits()),
                dirty: AtomicBool::new(false),
                applied: AtomicU32::new(d.to_bits()),
            });
        }
    }
    v
}

/// A fake parameter's value by `ParamEntry.value`'s rule: an unapplied host set's value, else the applied one.
fn fake_param_value(p: &Plugin, i: usize) -> f32 {
    let Some(x) = p.params.get(i) else { return 0.0 };
    if x.dirty.load(Ordering::SeqCst) {
        return f32::from_bits(x.pending.load(Ordering::SeqCst));
    }
    if i == 0 {
        f32::from_bits(p.gain.load(Ordering::SeqCst))
    } else {
        f32::from_bits(x.applied.load(Ordering::SeqCst))
    }
}

/// `knob=<v>` in a `;`-separated state body: replaced in place, or appended.
fn set_knob(body: &mut Vec<u8>, knob: &str, v: f32) {
    let text = String::from_utf8_lossy(body).into_owned();
    let pre = format!("{knob}=");
    let mut parts: Vec<String> = text
        .split(';')
        .filter(|s| !s.is_empty() && !s.starts_with(&pre))
        .map(str::to_string)
        .collect();
    parts.push(format!("{pre}{v}"));
    *body = parts.join(";").into_bytes();
}

/// A comma-separated notice knob (none on a quarantined slot).
fn notice_knob(x: &Option<codec::PluginRef>, name: &str, quarantined: bool) -> Vec<String> {
    if quarantined {
        return Vec::new();
    }
    notice_codes(attr(x, name))
}

fn notice_codes(v: Option<String>) -> Vec<String> {
    v.map_or_else(Vec::new, |v| {
        v.split(',')
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect()
    })
}

/// A knob in a fake slot's state body (`name="value"`, the body is the test's text).
fn body_knob(body: &str, name: &str) -> Option<String> {
    let pat = format!("{name}=\"");
    let i = body.find(&pat)? + pat.len();
    let j = body[i..].find('"')? + i;
    Some(body[i..j].to_string())
}

/// A VST3 class as a factory scan reports it: created, stereo in and out.
fn fake_vst3_class(path: &str, name: &str, class_id: &str, cats: &[&str]) -> codec::ScanClass {
    codec::ScanClass::Vst3({
        let mut msg = summoner_proto::plugin::Vst3Class::default();
        msg.path = path.into();
        msg.bundle_mtime_ms = 0;
        msg.source = Some(summoner_proto::plugin::ScanSource::Factory);
        msg.class_id = class_id.into();
        msg.category = "Audio Module Class".into();
        msg.name = name.into();
        msg.categories = cats.iter().map(|c| c.to_string()).collect();
        msg.vendor = String::new();
        msg.class_vendor = None;
        msg.version = String::new();
        msg.sdk_version = String::new();
        msg.instantiated = Some(true);
        msg.inputs = Some(2);
        msg.outputs = Some(2);
        msg
    })
}

/// A slot's knob `name`: a test double's (`{"format":"fake","fake":{…}}`) by its key; `None` for an empty
/// (quarantined) slot. A resolved slot is a vst3 / au ref from a scan-index entry a host application test writes: its
/// `name` is the name, and its knobs ride in that name as `fake: <knob>=<value> …`.
fn attr(x: &Option<codec::PluginRef>, name: &str) -> Option<String> {
    match x.as_ref()? {
        r if r.is_fake() => r.fake_knob(name),
        r => {
            if name == "name" {
                return Some(r.name().to_string());
            }
            let knobs = r.name().strip_prefix("fake:")?;
            knobs.split_whitespace().find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == name).then(|| v.to_string())
            })
        }
    }
}

/// One fake plugin's shared knobs (message thread and chain thread).
struct Plugin {
    name: String,
    gain: AtomicU32,
    synth: bool,
    /// `drone="1"`: an instrument that outputs `gain × 0.25` on every frame, notes or not.
    drone: bool,
    /// `ignore-bypass="1"`: a hostile instrument that keeps processing while its slot is disabled (its
    /// output is never replaced by the bypass signal), so the host application must not depend on a child honouring the bypass.
    ignore_bypass: bool,
    /// `inputs="n"` / `outputs="n"`: the bus counts the child's channel rule reads (default 2 / 2).
    io: (u32, u32),
    /// The synth ignores CC 123 and CC 120 (only note-offs release it: the hung-note test).
    ignore_cc123: bool,
    /// The synth honours the sustain pedal per channel (CC 64 ≥ 64 down): a note let go while its channel's pedal
    /// is down keeps sounding until the pedal comes up.
    sustain: bool,
    probe: bool,
    /// The decaying delay: a feedback delay of `echo` frames with feedback `feedback`, after the gain
    /// and before the latency line (`y = fma (feedback, line[pos], x)`, `line[pos] = y`, a value under 1e-30 in
    /// magnitude flushed to 0).
    echo: usize,
    /// `firstuse="<g>"`: first-use state as a real effect's (its first block after load differs). The instance's first
    /// processed block of its life runs at gain `g` instead of its gain; every later one, across any prepare, at
    /// its gain.
    first_use: Option<f32>,
    used: AtomicBool,
    /// `freshdelay="<n>"`: each `chain.prepareFresh` adds n frames to the slot's delay (its
    /// latency), as a plugin whose latency moves on a re-prepare.
    fresh_delay: usize,
    /// `crash-fresh="1"`: the child dies inside `chain.prepareFresh`.
    crash_fresh: bool,
    /// `skip-after="<n>"`: from the instance's block n of its life on (0: the first), every block is acked
    /// `ST_SKIPPED` with silence, as a chain the host could not process: a render must fail, never write the silence.
    skip_after: Option<u64>,
    /// *(protocol 3.9)* `notices="<code>,<code>"`: the slot's notices at `chain.loaded` (`SlotInfo.notices`).
    notices: Vec<String>,
    /// `prepnotices` is the complete prepare set (default: load set), never a union.
    /// `notice-on-state` reports a replacement set from each successful state's prepnotices knob.
    /// `freshnotices` is a semicolon-separated sequence of complete fresh-prepare sets.
    prep_notices: Vec<String>,
    notice_on_state: bool,
    current_notices: Mutex<Vec<String>>,
    fresh_notices: Vec<Vec<String>>,
    fresh_notice_index: AtomicU64,
    omit_fresh_notices: bool,
    fresh_gate: Option<String>,
    fail_fresh_after: Option<u64>,
    /// The blocks this instance has processed.
    blocks_seen: AtomicU64,
    feedback: f32,
    quarantined: bool,
    /// The plugin's own state bytes (the `{builtin}` body; `state.get` frames them, format v1).
    state: Mutex<Vec<u8>>,
    slow_get_ms: AtomicU64,
    /// `BAREGET`: `state.get` / `editor.closed` answer the bare body (a 3.0 child's blob, which the host application refuses).
    bare_get: AtomicBool,
    /// `controller-refuses="1"`: a hostile vst3-shaped double. It takes `{vst3}` sets: the component chunk
    /// applies (as its knob body), then the controller's `setState` fails, so the set is partial; its `state.get` is
    /// `{vst3}` with the component chunk only.
    controller_refuses: bool,
    /// `applied-lie="1"`: a hostile double that answers a whole builtin set `ok: true` with an `applied`
    /// (an invalid combination the host application must count, log and still report moved).
    applied_lie: bool,
    child_gen: AtomicU64,
    editor_open: AtomicBool,
    /// `DRIFT`: the state moves by itself at every generations poll.
    drift: AtomicBool,
    /// `DRIFTGET`: the state moves by itself just before each `state.get` answers, its generation bumped with
    /// no generations poll in between (a plugin that re-serialises): the read is the first to see the movement.
    drift_get: AtomicBool,
    /// `TOOLARGE=<bytes>`: `state.get` answers `tooLarge` with these bytes (3.3 only).
    too_large: AtomicU64,
    /// D22: the processor (the DSP state) in its handover cell; the chain thread reaches it only by `try_lock`.
    cell: Cell<Dsp>,
    /// D22: the slot's last applied latency L (`chain.loaded`, then each `latency.changed`): the bypass delay.
    latency: AtomicU32,
    /// D22: a pending `restartComponent(kLatencyChanged)`: 0 none, else `(new latency + 1) | cycle ms << 32`.
    restart: AtomicU64,
    /// D22, chain thread -> main thread: blocks bypassed, and of those misaligned, since the last `slot.reactivated`.
    bypassed: AtomicU32,
    misaligned: AtomicU32,
    /// A take that went to the re-attempt path (given up at the 2 s bound, or deferred because the turn's
    /// one wait was spent); the main thread only.
    pending: Mutex<Option<PendingTake>>,
    /// `BLOCKMS=<ms>`: the next processed block of this slot takes that long inside processBlock (holding the cell).
    block_once_ms: AtomicU64,
    /// The slot is enabled; disabled, it outputs its input delayed by its latency
    /// (an instrument: silence).
    enabled: AtomicBool,
    /// `LAGPROBE=<k>` (T17's probe, the module docs): 0 off, else armed for the next cycle (one-shot).
    lag_probe: AtomicU32,
    /// The probe's rendezvous: the main thread's cycle is done and waits to give back; the chain thread is inside a
    /// bypassed block and waits for the count; the main thread has given back and counted.
    probe_ready: AtomicBool,
    probe_in_block: AtomicBool,
    probe_counted: AtomicBool,
    /// `LAGPROBE=3`: the next bypassed block's count is dropped (the mutant).
    probe_drop_one: AtomicBool,
    /// (protocol 3.7): the parameters `param.list` reports and `param.set` reaches, and their tree order.
    params: Vec<FakeParam>,
    orders: Vec<u32>,
}

/// A reactivation whose take did not complete on its first attempt.
#[derive(Clone, Copy, Debug)]
struct PendingTake {
    new_latency: usize,
    cycle_ms: u64,
    announced: Instant,
    forced: bool,
}

struct Chain {
    key: u64,
    role: codec::Role,
    geometry: Geometry,
    rate: f64,
    region: Arc<Region>,
    plugins: Vec<Arc<Plugin>>,
    parked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<u64>>>,
}

struct Shared {
    page: Mutex<Option<Arc<ChildPage>>>,
    chains: Mutex<BTreeMap<u64, Chain>>,
    opts: Opts,
    /// Ops the reader has queued for the message thread and it has not yet taken.
    queued: std::sync::atomic::AtomicUsize,
    /// `--turns <path>`: the test-only turn record (never opened without the flag).
    turns: Option<TurnLog>,
    /// `--frames <path>`: the test-only frame record (never opened without the flag).
    frames: Option<FrameLog>,
    /// The protocol this incarnation's `hello` reply said (3.3 and later send
    /// `state.applied.childGeneration` and may answer `state.get` `tooLarge`).
    said: std::sync::atomic::AtomicU32,
}

/// `--frames <path>`, a test-only surface (as `--turns`): one line per frame this child receives, `<pid> recv <kind>`,
/// in arrival order (the reader thread), and `<pid> sent hello` just BEFORE the `hello` reply is written, so a frame
/// the host application sent only after it read the reply is always recorded after that line. Each line is one `write` to a file opened `O_APPEND`, shared by every incarnation.
struct FrameLog {
    file: std::fs::File,
}

impl FrameLog {
    fn open(path: &str) -> FrameLog {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("--frames: open the frame record");
        FrameLog { file }
    }

    fn line(&self, text: &str) {
        let _ = (&self.file).write_all(format!("{} {text}\n", std::process::id()).as_bytes());
    }
}

/// What this incarnation answers to `hello` (`--hello-script <path>`, read at the `hello`): the file's trimmed text is
/// `protocol=<u32>` (that protocol), `undecodable` (meta that does not decode), `hang` (never answer); a missing or
/// empty file means the flags' answer. A test writes the file between incarnations to vary one of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelloScript {
    Flags,
    Protocol(u32),
    Undecodable,
    Hang,
}

fn hello_script(opts: &Opts) -> HelloScript {
    let Some(path) = &opts.hello_script else {
        return HelloScript::Flags;
    };
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match text.trim() {
        "" => HelloScript::Flags,
        "undecodable" => HelloScript::Undecodable,
        "hang" => HelloScript::Hang,
        t => match t.strip_prefix("protocol=").and_then(|v| v.parse().ok()) {
            Some(p) => HelloScript::Protocol(p),
            None => {
                runecraft_rt::safe_eprintln!(
                    "fake-host: --hello-script {path}: {t:?} is not a script word"
                );
                std::process::exit(2)
            }
        },
    }
}

/// `--flood <n>:<bytes>` (the module docs): n `log` events of `bytes` blob bytes each, every write whole under the
/// writer's lock (a frame is never interleaved with another reply).
fn flood(w: &Writer, n: usize, bytes: usize) {
    let frame = codec::encode_msg(
        kind::LOG,
        FF_EVENT,
        0,
        &codec::Log::new("info".into(), "fake flood".into()),
        &vec![0xa5; bytes],
    );
    runecraft_rt::safe_eprintln!("fake-host: flooding {n} frames of {bytes} blob bytes");
    for _ in 0..n {
        send(w, frame.clone());
    }
    runecraft_rt::safe_eprintln!("fake-host: flood sent");
}

/// `proto-load="<fault>"` on a slot: inside `chain.load` of that
/// slot, with the status record naming it, the child writes a fault on fd 3, then holds the load (and so the status
/// record) until it is killed:
/// - `refused`: a `chain.loaded` event whose meta `check_to_core` refuses;
/// - `orphan`: a reply to a request the host application never sent;
/// - `garbage`: a frame header the host application's decoder refuses (an unknown kind);
/// - `fds`: a well-formed event carrying a descriptor (the child never sends any);
/// - `midframe`: half a frame, then the child dies at once (`die`: SIGKILL under the death switch), so the host application
///   sees EOF mid-frame: an ordinary, attributed death.
fn protocol_fault_at_load(w: &Writer, fault: &str) -> ! {
    let log = codec::encode_msg(
        kind::LOG,
        FF_EVENT,
        0,
        &serde_json::json!({"level": "info", "message": "fake"}),
        &[],
    );
    match fault {
        "refused" => send(
            w,
            codec::encode(
                kind::CHAIN_LOADED,
                FF_EVENT,
                0,
                br#"{"chainKey": "not a number"}"#,
                &[],
            ),
        ),
        "orphan" => send(
            w,
            codec::encode_msg(kind::PING, FF_REPLY, 0x7fff_ffff_ffff_fff1, &empty(), &[]),
        ),
        "garbage" => send(w, codec::encode(0xfff0, FF_EVENT, 0, b"{}", &[])),
        "fds" => {
            let null = std::fs::File::open("/dev/null").expect("/dev/null");
            if let Ok(s) = w.lock() {
                let _ = runecraft_ipc::uds::send_with_fd(&*s, &log, null.as_fd());
            }
        }
        "midframe" => {
            if let Ok(mut s) = w.lock() {
                let _ = s.write_all(&log[..log.len() / 2]);
            }
            die();
        }
        other => {
            runecraft_rt::safe_eprintln!("fake-host: proto-load={other:?} is not a fault");
            std::process::exit(2)
        }
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// `--turns <path>`, a test-only surface (the fake is a test double; no production path writes it): the record of
/// each D22 main-loop turn that has a take to make (a restart flag up or a re-attempt pending), so a test can see which
/// takes shared a turn and what the turn waited, instead of inferring it from timing. Two lines per such turn, each
/// one `write` to a file opened `O_APPEND`, written only by the message thread:
///
/// - `begin <seq> flags=<k/s,..|-> pending=<k/s,..|->` before the turn's first take: the slots whose restart flag is
///   up (this turn announces them) and those on the re-attempt path;
/// - `end <seq> announced=<k/s,..|-> waits=<n> us=<µs>` after its last: what it announced, `Turn::waits()`, and the
///   turn's wall time.
struct TurnLog {
    file: std::fs::File,
    seq: AtomicU64,
}

impl TurnLog {
    fn open(path: &str) -> TurnLog {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("--turns: open the turn record");
        TurnLog {
            file,
            seq: AtomicU64::new(0),
        }
    }

    fn line(&self, text: String) {
        let _ = (&self.file).write_all(text.as_bytes());
    }

    fn list(v: &[String]) -> String {
        if v.is_empty() {
            "-".into()
        } else {
            v.join(",")
        }
    }
}

/// The reader's only way to queue an op: counted first, so the message thread's snapshot never undercounts.
fn enqueue(sh: &Shared, ops: &Sender<Op>, op: Op) {
    sh.queued.fetch_add(1, Ordering::SeqCst);
    let _ = ops.send(op);
}

enum Op {
    /// the next op's `corr` (queued just before it by the reader).
    Echo(String),
    Load(u64, codec::ChainLoad, OwnedFd),
    Unload(u64),
    Prepare(u64),
    /// (protocol 3.8): a render set's slots released and prepared again.
    PrepareFresh(u64),
    SetState(codec::StateSet, Vec<u8>),
    GetState(u64, codec::StateGet),
    EditorOpen(codec::EditorRef),
    EditorClose(codec::EditorRef),
    Workgroup(codec::WorkgroupAttach),
    ParamList(u64, codec::ParamList),
    ParamSet(u64, codec::ParamSet),
    Shutdown(u64),
}

/// `--scan-escapee` (the scan mode's `escapee=1`): leave the group and the session, record the pid, sleep at most 30 s.
fn scan_escapee() -> ! {
    let _ = runecraft_ipc::process::unix::setsid();
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let _ = std::fs::write(
            std::path::Path::new(&h).join("escapee.pid"),
            std::process::id().to_string(),
        );
    }
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(0)
}

/// the parent both orphan watches (and the scan mode's argv check) compare
/// against, read on `main`'s first line. The watch read it after set-up before, so a fake host orphaned before then
/// read launchd's 1 and never saw a change (it outlived its parent). The fake host is a supervised child only (it has
/// no mode without the watch), so launchd as its parent is gone too.
fn watched_parent(entry: EntryParent) -> EntryParent {
    // mutation check (never in a normal build): `--cfg mutant_fake_parent_read_late` reads the parent late, as before.
    if cfg!(mutant_fake_parent_read_late) {
        EntryParent::read()
    } else {
        entry
    }
}

/// The parent is gone: [`EntryParent::gone`]. Under `--cfg mutant_fake_parent_read_late` or `mutant_fake_parent_launchd_alive`
/// (never in a normal build) only a changed parent is (launchd not taken as gone);
/// the orphan-watch tests fail under each.
fn parent_gone(parent: EntryParent) -> bool {
    if cfg!(mutant_fake_parent_read_late) || cfg!(mutant_fake_parent_launchd_alive) {
        parent.changed()
    } else {
        parent.gone()
    }
}

/// The orphan watch thread: our parent gone means exit 3 (host's orphan exit; a 100 ms poll here).
fn watch_parent(parent: EntryParent) {
    let parent = watched_parent(parent);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if parent_gone(parent) {
                std::process::exit(3);
            }
        }
    });
}

/// The whole fake host program: the scan mode (`--scan`), the escapee, or the live child on descriptor 3 (the
/// module docs).
pub fn main() {
    // first, so a parent that dies during set-up is seen.
    let parent = EntryParent::read();
    // `--scan-escapee` goes before the gone-check on purpose: the escapee models a process that left the scan's group
    // and outlives the scan child (the scan supervisor's tests read it as still running after that child is gone). It
    // is no exit-with-parent process (it has no watch), so [`EntryParent::gone`]'s rules are not its own; first, the
    // orphan exit can never end it, even when it starts late (a loaded machine) after its parent already died.
    if std::env::args().nth(1).as_deref() == Some("--scan-escapee") {
        scan_escapee();
    }
    // Orphaned before `main` (launchd is our parent already): host's orphan exit, before fd 3 is touched.
    if parent_gone(parent) {
        std::process::exit(3);
    }
    LAB_DEATH_KILL.store(
        std::env::var_os(DEATH_SWITCH_ENV).is_some_and(|v| v == "kill"),
        Ordering::Relaxed,
    );
    let mut opts = Opts::default();
    let mut scan = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ignore-slot0" => opts.ignore_slot0 = true,
            "--hist-modelled" => opts.hist_modelled = true,
            "--mutant-reader-pongs" => opts.mutant_pongs = true,
            "--lie-quarantined" => opts.lie_quarantined = true,
            "--hang-at-hello" => opts.hang_at_hello = true,
            "--mutant-hard-switch" => opts.mutant_hard_switch = true,
            "--mutant-no-delay-line" => opts.mutant_no_delay_line = true,
            "--tc-us" => {
                i += 1;
                opts.tc_us = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--stats" => {
                i += 1;
                opts.stats = args.get(i).cloned();
            }
            "--turns" => {
                i += 1;
                opts.turns = args.get(i).cloned();
            }
            "--hello-protocol" => {
                i += 1;
                opts.hello_protocol = args.get(i).and_then(|s| s.parse().ok());
            }
            "--hello-undecodable" => opts.hello_undecodable = true,
            "--no-editor" => opts.no_editor = true,
            "--editor-fails-at-open" => opts.editor_fails_at_open = true,
            "--chain-loaded-v0" => opts.chain_loaded_v0 = true,
            "--log-set-refuse" => opts.log_set_refuse = true,
            "--log-set-mismatch" => opts.log_set_mismatch = true,
            "--corr-echo-bad" => opts.corr_echo_bad = true,
            "--editor-user-close-ms" => {
                i += 1;
                opts.editor_user_close_ms = args.get(i).and_then(|s| s.parse().ok());
            }
            "--editor-window-hwnd" => {
                i += 1;
                opts.editor_window_hwnd = args.get(i).and_then(|s| s.parse().ok());
            }
            "--hello-delay-ms" => {
                i += 1;
                opts.hello_delay_ms = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--prepare-fresh-ms" => {
                i += 1;
                opts.prepare_fresh_ms = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--load-gate" => {
                i += 1;
                opts.load_gate = args.get(i).cloned();
            }
            "--load-tokens" => {
                i += 1;
                opts.load_tokens = args.get(i).cloned();
            }
            "--hello-script" => {
                i += 1;
                opts.hello_script = args.get(i).cloned();
            }
            "--frames" => {
                i += 1;
                opts.frames = args.get(i).cloned();
            }
            "--flood" => {
                i += 1;
                let parsed = args
                    .get(i)
                    .and_then(|v| v.split_once(':'))
                    .and_then(|(n, b)| Some((n.parse().ok()?, b.parse().ok()?)))
                    .filter(|&(n, b): &(usize, usize)| match b {
                        0 => (1..=512).contains(&n),
                        _ => (1..=16).contains(&n) && b <= codec::MAX_BLOB,
                    });
                let Some(f) = parsed else {
                    runecraft_rt::safe_eprintln!(
                        "fake-host: --flood <n>:<bytes>, n 1..=16 and bytes 1..=MAX_BLOB, or n 1..=512 and bytes 0"
                    );
                    std::process::exit(2);
                };
                opts.flood = Some(f);
            }
            "--scan" => scan = true,
            "--crash-log" => {
                i += 1;
                let d = crash_log_arg(args.get(i));
                let _ = CRASH_LOG.set(d);
            }
            "--step-ms" => {
                i += 1;
                let _ = step_ms_arg(args.get(i));
            }
            f if f == CHILD_LOG_DIR_FLAG => {
                i += 1;
                opts.log_dir = args.get(i).is_some();
            }
            _ => {}
        }
        i += 1;
    }
    ECHO_BAD.store(opts.corr_echo_bad, Ordering::Relaxed);
    let Some(fd) = runecraft_ipc::process::unix::take_fd3() else {
        runecraft_rt::safe_eprintln!("fake-host: no descriptor 3 (run me through the supervisor)");
        std::process::exit(2);
    };
    let sock = UnixStream::from(fd);
    let writer: Writer = Arc::new(Mutex::new(sock.try_clone().expect("clone the socket")));
    // Route B (D8): take slot 0 and clear the registration before anything else.
    let slot0 = runecraft_ipc::bridge::take_registered_port();
    if scan {
        drop(slot0); // a scan child takes slot 0 and drops it (S1)
        drop(writer);
        scan_mode::run(&args, sock, parent);
    }
    let slot0 = if opts.ignore_slot0 { None } else { slot0 };
    let own_rx = MachReceive::new().ok().map(Arc::new);
    let shared = Arc::new(Shared {
        page: Mutex::new(None),
        chains: Mutex::new(BTreeMap::new()),
        opts: opts.clone(),
        queued: std::sync::atomic::AtomicUsize::new(0),
        turns: opts.turns.as_deref().map(TurnLog::open),
        frames: opts.frames.as_deref().map(FrameLog::open),
        said: std::sync::atomic::AtomicU32::new(0),
    });
    // The orphan watch: the parent gone means exit (today's NOTE_EXIT; a 100 ms poll here).
    watch_parent(parent);
    // The mach channel thread: workgroup ports arrive here.
    if let Some(rx) = own_rx.clone() {
        let w = writer.clone();
        let sh = shared.clone();
        std::thread::spawn(move || {
            loop {
                match rx.recv(1000) {
                    Ok(m) => {
                        let ok = m.port.is_some();
                        // No real workgroup in the lab (P1's open point): count each chain thread as a
                        // time-constraint-only fallback, the counted defect.
                        if let Some(p) = sh.page.lock().unwrap().as_ref() {
                            let n = sh.chains.lock().unwrap().len() as u64;
                            p.word64(P_WG_FALLBACKS)
                                .fetch_add(n.max(1), Ordering::Relaxed);
                        }
                        event(
                            &w,
                            kind::WORKGROUP_ATTACHED,
                            &codec::WorkgroupAttached::new(m.token, ok, false),
                            &[],
                        );
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
    }
    let (op_tx, op_rx) = channel::<Op>();
    {
        let w = writer.clone();
        let sh = shared.clone();
        let own = own_rx.clone();
        std::thread::Builder::new()
            .name("fake-read".into())
            .spawn(move || reader_thread(sock, w, sh, op_tx, slot0, own))
            .expect("reader");
    }
    if opts.mutant_pongs {
        let sh = shared.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(20));
                for c in sh.chains.lock().unwrap().values() {
                    let p = c.region.ping().load(Ordering::Acquire);
                    if p != 0 {
                        c.region.pong().store(p, Ordering::Release);
                    }
                }
            }
        });
    }
    message_thread(op_rx, writer, shared, own_rx);
}

/// Whether this double claims 3.6's additions: `log.set`, `corr`, the page and `chain.prepared` pairs: a 3.6+
/// claim, or 4.0 (which carries every 3.x addition).
fn takes_3_6(said: u32) -> bool {
    said >> 16 >= 4 || (said >> 16 == 3 && said & 0xffff >= 6)
}

/// Whether the claim `said` carries 3.`minor`'s additions: a 3.x claim at that minor or later, or 4.0.
fn claims_3(said: u32, minor: u32) -> bool {
    said >> 16 >= 4 || (said >> 16 == 3 && said & 0xffff >= minor)
}

/// `log.set`, Inline: the codec's checks 1-4, then check 5 (`--log-dir` given), then this process's word.
fn log_set(sh: &Shared, w: &Writer, id: u64, f: &Frame, corr: Option<&str>) {
    let refuse = |why: &str| {
        let m = serde_json::to_vec(&codec::ErrorReply::new(-32602, why.into())).expect("encodes");
        let m = echoed(m, corr, sh.opts.corr_echo_bad);
        send(
            w,
            codec::encode(kind::LOG_SET, FF_REPLY | FF_ERROR, id, &m, &[]),
        );
    };
    let m: codec::LogSet = match f.meta_as() {
        Ok(m) => m,
        Err(e) => return refuse(&format!("log.set: {e}")),
    };
    if let Some(l) = &sh.frames {
        l.line(&format!(
            "log.set levels {:#018x} expiresInMs {}",
            m.levels,
            m.expires_in_ms
                .map_or("none".to_string(), |v| v.to_string())
        ));
    }
    let (word, expires) = match m.check() {
        Ok(v) => v,
        Err(why) => return refuse(why),
    };
    if !sh.opts.log_dir {
        return refuse("log.set: no log folder (--log-dir): nothing could expire a level");
    }
    if sh.opts.log_set_refuse {
        return refuse("log.set: refused (--log-set-refuse)");
    }
    let _ = expires;
    LEVELS.store(word, Ordering::Relaxed);
    let levels = LEVELS.load(Ordering::Relaxed) ^ u64::from(sh.opts.log_set_mismatch);
    let r = serde_json::to_vec(&codec::LogSetReply::new(levels)).expect("encodes");
    let r = echoed(r, corr, sh.opts.corr_echo_bad);
    send(w, codec::encode(kind::LOG_SET, FF_REPLY, id, &r, &[]));
}

fn reader_thread(
    sock: UnixStream,
    w: Writer,
    sh: Arc<Shared>,
    ops: Sender<Op>,
    slot0: Option<MachSend>,
    own: Option<Arc<MachReceive>>,
) {
    let mut dec = codec::Decoder::new(Direction::ToChild);
    let mut buf = vec![0u8; 64 * 1024];
    let mut fds: VecDeque<OwnedFd> = VecDeque::new();
    let mut slot0 = slot0;
    loop {
        let mut got = Vec::new();
        let n = match runecraft_ipc::uds::recv_with_fds(&sock, &mut buf, &mut got) {
            Ok(0) | Err(_) => std::process::exit(0),
            Ok(n) => n,
        };
        fds.extend(got);
        let mut frames: Vec<Frame> = Vec::new();
        if dec.feed(&buf[..n], &mut |f| frames.push(f)).is_err() {
            std::process::exit(4);
        }
        for f in frames {
            let id = f.header.id;
            // the request's `corr` (lenient: a bad one is ignored).
            let corr = match codec::corr_of_meta(&f.meta) {
                codec::CorrRead::Valid(c) => Some(c),
                _ => None,
            };
            if let Some(l) = &sh.frames {
                l.line(&format!(
                    "recv {}",
                    codec::kind_name(f.header.kind).unwrap_or("?")
                ));
                match codec::corr_of_meta(&f.meta) {
                    codec::CorrRead::Valid(c) => l.line(&format!(
                        "corr {} {c}",
                        codec::kind_name(f.header.kind).unwrap_or("?")
                    )),
                    codec::CorrRead::Invalid => l.line(&format!(
                        "corr {} invalid",
                        codec::kind_name(f.header.kind).unwrap_or("?")
                    )),
                    // Absent, or a reading a later codec names: nothing to log.
                    _ => {}
                }
            }
            if let Some(c) = &corr
                && matches!(
                    f.header.kind,
                    kind::CHAIN_LOAD | kind::CHAIN_UNLOAD | kind::CHAIN_PREPARE
                )
            {
                enqueue(&sh, &ops, Op::Echo(c.clone()));
            }
            if let Some(l) = &sh.frames {
                // what an `editor.open` asked for (a line without " recv ", so the kind counts are unchanged).
                if f.header.kind == kind::EDITOR_OPEN
                    && let Ok(m) = f.meta_as::<codec::EditorRef>()
                {
                    l.line(&format!(
                        "editor.open key {} slot {} guardReset {}",
                        m.chain_key,
                        m.slot.unwrap_or(0),
                        m.guard_reset
                    ));
                }
            }
            let script = if f.header.kind == kind::HELLO {
                hello_script(&sh.opts)
            } else {
                HelloScript::Flags
            };
            match f.header.kind {
                kind::HELLO if sh.opts.hang_at_hello || script == HelloScript::Hang => loop {
                    // Wedged before its first answer (the orphan watch still ends it with the parent).
                    std::thread::sleep(Duration::from_secs(3600));
                },
                kind::HELLO => {
                    let Some(pfd) = fds.pop_front() else {
                        std::process::exit(5)
                    };
                    let page =
                        Arc::new(ChildPage::open(received(pfd), &mut Vec::new()).expect("page"));
                    *sh.page.lock().unwrap() = Some(page);
                    let m: codec::Hello = f.meta_as().unwrap_or_default();
                    // Route B: the channel hello goes to slot 0's port before the reply (D8).
                    let mut b = false;
                    if let (Some(s0), Some(own)) = (slot0.take(), own.as_ref()) {
                        b = s0.send_making(std::process::id() as u64, own, 1000).is_ok();
                    }
                    let protocol = match script {
                        HelloScript::Protocol(p) => p,
                        _ => sh.opts.hello_protocol.unwrap_or(ABI_VERSION),
                    };
                    sh.said.store(protocol, Ordering::SeqCst);
                    // a 3.6 double wires its page in place and reports it (the plugin-host child's R3 row 2).
                    let (wired_bytes, unwired_bytes) =
                        if takes_3_6(protocol) && !sh.opts.chain_loaded_v0 {
                            match sh.page.lock().unwrap().as_ref().map(|p| p.wire()) {
                                Some(Ok(n)) => (n, 0),
                                Some(Err(e)) => (0, e.bytes as u64),
                                None => (0, 0),
                            }
                        } else {
                            (0, 0)
                        };
                    let frame = if sh.opts.hello_undecodable || script == HelloScript::Undecodable {
                        // H-3: a JSON object, but not a HelloReply (`protocol` is a string).
                        codec::encode(
                            kind::HELLO,
                            FF_REPLY,
                            id,
                            br#"{"protocol": "3.1", "host": "fake"}"#,
                            &[],
                        )
                    } else {
                        codec::encode_msg(
                            kind::HELLO,
                            FF_REPLY,
                            id,
                            &{
                                let mut msg = codec::HelloReply::default();
                                msg.pid = std::process::id();
                                msg.protocol = protocol;
                                msg.child_id = m.child_id;
                                msg.host = "fake".into();
                                msg.formats = vec!["fake".into()];
                                msg.mach_slot0 = b;
                                msg.wired_bytes = wired_bytes;
                                msg.unwired_bytes = unwired_bytes;
                                msg
                            },
                            &[],
                        )
                    };
                    let (w2, sh2) = (w.clone(), sh.clone());
                    let answer = move || {
                        if let Some(l) = &sh2.frames {
                            l.line("sent hello");
                        }
                        send(&w2, frame);
                        if let Some((n, bytes)) = sh2.opts.flood {
                            std::thread::spawn(move || flood(&w2, n, bytes));
                        }
                    };
                    match sh.opts.hello_delay_ms {
                        0 => answer(),
                        ms => {
                            std::thread::spawn(move || {
                                std::thread::sleep(Duration::from_millis(ms));
                                answer();
                            });
                        }
                    }
                }
                kind::PING => reply(&w, kind::PING, id, &empty(), &[]),
                kind::LOG_SET => log_set(&sh, &w, id, &f, corr.as_deref()),
                kind::STATE_GENERATIONS => {
                    let mut slots = Vec::new();
                    for c in sh.chains.lock().unwrap().values() {
                        for (i, p) in c.plugins.iter().enumerate() {
                            // `DRIFT`: the plugin moves by itself (an LFO phase, a seed), its
                            // generation bumped at each poll, its editor never open.
                            if p.drift.load(Ordering::SeqCst) {
                                p.state.lock().unwrap().extend_from_slice(b";d");
                                p.child_gen.fetch_add(1, Ordering::SeqCst);
                            }
                            slots.push(codec::GenEntry::new(
                                c.key,
                                i as u32,
                                p.child_gen.load(Ordering::SeqCst),
                                p.editor_open.load(Ordering::SeqCst),
                            ));
                        }
                    }
                    reply(
                        &w,
                        kind::STATE_GENERATIONS,
                        id,
                        &codec::Generations::new(slots),
                        &[],
                    );
                }
                kind::SHUTDOWN => {
                    enqueue(&sh, &ops, Op::Shutdown(id));
                }
                kind::CHAIN_LOAD => {
                    let Ok(m) = f.meta_as::<codec::ChainLoad>() else {
                        std::process::exit(6)
                    };
                    // the chain a load names (a line without " recv ").
                    if let Some(l) = &sh.frames {
                        l.line(&format!("chain.load key {}", m.chain_key));
                    }
                    let Some(rfd) = fds.pop_front() else {
                        std::process::exit(7)
                    };
                    reply(&w, kind::CHAIN_LOAD, id, &empty(), &[]);
                    enqueue(&sh, &ops, Op::Load(m.chain_key, m, rfd));
                }
                kind::CHAIN_PARK | kind::CHAIN_UNPARK => {
                    reply(&w, f.header.kind, id, &empty(), &[]);
                    let m: codec::ChainUnpark = f.meta_as().unwrap_or_default();
                    let chains = sh.chains.lock().unwrap();
                    let page = sh.page.lock().unwrap().clone();
                    if let Some(c) = chains.get(&m.chain_key) {
                        if f.header.kind == kind::CHAIN_PARK {
                            c.parked.store(true, Ordering::SeqCst);
                            // Wake it off its hub so it re-checks and waits on its own seq.
                            if let Some(p) = &page {
                                let g = c.region.group().load(Ordering::Acquire) as usize;
                                let _ = p.hub_waker(g).wake();
                            }
                            event(
                                &w,
                                kind::CHAIN_PARKED,
                                &codec::ChainRef::new(m.chain_key, true),
                                &[],
                            );
                        } else {
                            // C9 hub switch: store the new group, wake the OLD hub once, wake a parked thread's seq,
                            // then answer chain.unparked.
                            let old =
                                c.region.group().swap(m.group.min(3), Ordering::AcqRel) as usize;
                            c.parked.store(false, Ordering::SeqCst);
                            if let Some(p) = &page {
                                let _ = p.hub_waker(old).wake();
                            }
                            let _ = c.region.seq_waker().wake_all();
                            event(
                                &w,
                                kind::CHAIN_UNPARKED,
                                &codec::ChainUnpark::new(m.chain_key, m.group),
                                &[],
                            );
                        }
                    }
                }
                kind::CHAIN_PREPARE_FRESH => {
                    reply(&w, f.header.kind, id, &empty(), &[]);
                    let m: codec::ChainRef = f.meta_as().unwrap_or_default();
                    enqueue(&sh, &ops, Op::PrepareFresh(m.chain_key));
                }
                kind::CHAIN_UNLOAD | kind::CHAIN_PREPARE => {
                    reply(&w, f.header.kind, id, &empty(), &[]);
                    let m: codec::ChainRef = f.meta_as().unwrap_or_default();
                    enqueue(
                        &sh,
                        &ops,
                        if f.header.kind == kind::CHAIN_UNLOAD {
                            Op::Unload(m.chain_key)
                        } else {
                            Op::Prepare(m.chain_key)
                        },
                    );
                }
                kind::SLOT_SET_ENABLED => {
                    // Applied at once, then answered (MainThread).
                    let m: Option<codec::SlotEnabled> = f.meta_as().ok();
                    let mut ok = false;
                    if let Some(m) = &m
                        && let Some(p) = plugin_of(&sh, m.chain_key, m.slot)
                    {
                        p.enabled.store(m.enabled, Ordering::Release);
                        ok = true;
                    }
                    let m = m.unwrap_or(codec::SlotEnabled::new(0, 0, true));
                    reply(
                        &w,
                        kind::SLOT_SET_ENABLED,
                        id,
                        &codec::SlotEnabledReply::new(m.chain_key, m.slot, m.enabled, ok),
                        &[],
                    );
                }
                kind::STATE_SET => {
                    reply(&w, f.header.kind, id, &empty(), &[]);
                    let m: codec::StateSet = f.meta_as().unwrap_or_default();
                    // which chain and slot a state was set to, and its builtin body (a line without " recv ").
                    if let Some(l) = &sh.frames {
                        let body = match state_blob::decode(&f.blob) {
                            Ok(state_blob::Blob::Builtin { body }) => {
                                String::from_utf8_lossy(body).to_string()
                            }
                            _ => String::new(),
                        };
                        l.line(&format!(
                            "state.set key {} slot {} body {body}",
                            m.chain_key, m.slot
                        ));
                    }
                    enqueue(&sh, &ops, Op::SetState(m, f.blob));
                }
                kind::EDITOR_OPEN | kind::EDITOR_CLOSE => {
                    match sh.opts.editor_window_hwnd {
                        Some(h) if f.header.kind == kind::EDITOR_OPEN => reply(
                            &w,
                            f.header.kind,
                            id,
                            &codec::EditorOpened::new(Some(codec::EditorWindow::Hwnd(h))),
                            &[],
                        ),
                        _ => reply(&w, f.header.kind, id, &empty(), &[]),
                    }
                    let m: codec::EditorRef = f.meta_as().unwrap_or_default();
                    enqueue(
                        &sh,
                        &ops,
                        if f.header.kind == kind::EDITOR_OPEN {
                            Op::EditorOpen(m)
                        } else {
                            Op::EditorClose(m)
                        },
                    );
                }
                kind::WORKGROUP_ATTACH => {
                    reply(&w, f.header.kind, id, &empty(), &[]);
                    let m: codec::WorkgroupAttach = f.meta_as().unwrap_or_default();
                    enqueue(&sh, &ops, Op::Workgroup(m));
                }
                kind::STATE_GET => {
                    let m: codec::StateGet = f.meta_as().unwrap_or_default();
                    // which chain and slot a state was read from (a line without " recv ").
                    if let Some(l) = &sh.frames {
                        l.line(&format!("state.get key {} slot {}", m.chain_key, m.slot));
                    }
                    enqueue(&sh, &ops, Op::GetState(id, m));
                }
                kind::PARAM_LIST => {
                    let m: codec::ParamList = f.meta_as().unwrap_or_default();
                    enqueue(&sh, &ops, Op::ParamList(id, m));
                }
                // (protocol 3.7): every field is required; a meta missing one is invalid params.
                kind::PARAM_SET => match f.meta_as::<codec::ParamSet>() {
                    Ok(m) => enqueue(&sh, &ops, Op::ParamSet(id, m)),
                    Err(e) => send(
                        &w,
                        codec::encode_msg(
                            kind::PARAM_SET,
                            FF_REPLY | FF_ERROR,
                            id,
                            &codec::ErrorReply::new(-32602, format!("param.set: {e:?}")),
                            &[],
                        ),
                    ),
                },
                k => send(
                    &w,
                    codec::encode_msg(
                        k,
                        FF_REPLY | FF_ERROR,
                        id,
                        &codec::ErrorReply::new(-32601, "not in the fake".into()),
                        &[],
                    ),
                ),
            }
        }
    }
}

fn status(sh: &Shared, op: u32, key: u64, slot: u32, name: &str) {
    if let Some(p) = sh.page.lock().unwrap().as_ref() {
        p.write_status(
            op,
            key,
            slot,
            name,
            runecraft_ipc::bridge::host_ticks().max(1),
        );
    }
}

fn clear_status(sh: &Shared) {
    if let Some(p) = sh.page.lock().unwrap().as_ref() {
        p.clear_status();
    }
}

fn plugin_of(sh: &Shared, key: u64, slot: u32) -> Option<Arc<Plugin>> {
    sh.chains
        .lock()
        .unwrap()
        .get(&key)
        .and_then(|c| c.plugins.get(slot as usize).cloned())
}

/// A descriptor the fake received with a frame, as the object the region views map.
fn received(fd: OwnedFd) -> runecraft_ipc::channel::Transferable {
    runecraft_ipc::channel::unix::TransferableExt::from_fd(
        fd,
        runecraft_ipc::channel::Access::ReadWrite,
    )
}

fn load(sh: &Arc<Shared>, w: &Writer, key: u64, m: codec::ChainLoad, fd: OwnedFd) {
    // (rule 2), as the child: a key already hosted is never loaded again (the host application's
    // supervisor refuses to send it); the hosted chain is left as it is.
    if sh.chains.lock().unwrap().contains_key(&key) {
        runecraft_rt::safe_eprintln!("fake-host: chain.load for hosted key {key:#x} refused");
        return;
    }
    let geo = Geometry {
        max_frames: m.max_frames,
        ch_in: m.ch_in,
        ch_out: m.ch_out,
        max_midi: m.max_midi,
        max_params: m.max_params,
        tap_slot: m.tap_slot,
    };
    let region = match Region::open(received(fd), &mut Vec::new(), geo, key) {
        Ok(r) => Arc::new(r),
        Err(e) => {
            runecraft_rt::safe_eprintln!("fake-host: region: {e}");
            let fail = {
                let mut msg = codec::ChainLoaded::default();
                msg.chain_key = key;
                msg
            };
            event(w, kind::CHAIN_LOADED, &fail, &[]);
            return;
        }
    };
    // as the plugin-host child's `render_mode`: flags 0, the role decides; else they must agree with it.
    let render = match render_mode(m.role, region.flags()) {
        Ok(r) => r,
        Err(why) => {
            runecraft_rt::safe_eprintln!("fake-host: chain {key}: {why}");
            let fail = {
                let mut msg = codec::ChainLoaded::default();
                msg.chain_key = key;
                msg
            };
            event(w, kind::CHAIN_LOADED, &fail, &[]);
            return;
        }
    };
    region.group().store(m.group.min(3), Ordering::Release);
    let mut plugins = Vec::new();
    let mut infos = Vec::new();
    let mut latency = 0u32;
    for (i, s) in m.slots.iter().enumerate() {
        let x = &s.plugin;
        let name = attr(x, "name").unwrap_or_else(|| format!("fake{i}"));
        status(sh, OP_LOAD, key, i as u32, &name);
        if attr(x, "crash-load").is_some() {
            die();
        }
        // a plugin that calls exit(n) inside chain.load (after the verified hello).
        if let Some(n) = attr(x, "exit-load").and_then(|v| v.parse::<i32>().ok()) {
            std::process::exit(n);
        }
        if let Some(fault) = attr(x, "proto-load") {
            protocol_fault_at_load(w, &fault);
        }
        // A quarantined slot arrives empty: it is not loaded, and passes audio through unchanged (D7).
        let quarantined = x.is_none();
        let delay = if quarantined {
            0
        } else {
            attr(x, "delay")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0usize)
        };
        let gain: f32 = if quarantined {
            1.0
        } else {
            attr(x, "gain").and_then(|v| v.parse().ok()).unwrap_or(1.0)
        };
        let lie = quarantined && sh.opts.lie_quarantined;
        let reported_delay = if lie { 999 } else { delay as u32 };
        latency += reported_delay;
        let fparams = if quarantined {
            Vec::new()
        } else {
            fake_params(x, gain)
        };
        let clumps: Vec<Option<u32>> = fparams.iter().map(|p| p.clump).collect();
        let orders = summoner_plugin::params::au_clump_order(&clumps);
        let params = if lie {
            77
        } else if quarantined {
            0
        } else {
            FAKE_PARAMS.max(fparams.len() as u32)
        };
        let echo = if quarantined {
            0
        } else {
            attr(x, "echo")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
                .min(1 << 20)
        };
        plugins.push(Arc::new(Plugin {
            name: name.clone(),
            gain: AtomicU32::new(gain.to_bits()),
            synth: !quarantined && (attr(x, "synth").is_some() || attr(x, "drone").is_some()),
            drone: !quarantined && attr(x, "drone").is_some(),
            ignore_bypass: !quarantined && attr(x, "ignore-bypass").is_some(),
            io: (
                attr(x, "inputs")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(2),
                attr(x, "outputs")
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(2),
            ),
            ignore_cc123: attr(x, "ignore-cc123").is_some(),
            sustain: attr(x, "sustain").is_some(),
            probe: !quarantined && attr(x, "probe").is_some(),
            echo,
            first_use: attr(x, "firstuse").and_then(|v| v.parse::<f32>().ok()),
            used: AtomicBool::new(false),
            fresh_delay: attr(x, "freshdelay")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0)
                .min(1 << 16),
            crash_fresh: !quarantined && attr(x, "crash-fresh").is_some(),
            notices: notice_knob(x, "notices", quarantined),
            prep_notices: if attr(x, "prepnotices").is_some() {
                notice_knob(x, "prepnotices", quarantined)
            } else {
                notice_knob(x, "notices", quarantined)
            },
            notice_on_state: attr(x, "notice-on-state").is_some(),
            current_notices: Mutex::new(Vec::new()),
            fresh_notices: attr(x, "freshnotices").map_or_else(Vec::new, |v| {
                v.split(';')
                    .map(|set| {
                        if quarantined {
                            Vec::new()
                        } else {
                            set.split(',')
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .collect()
                        }
                    })
                    .collect()
            }),
            fresh_notice_index: AtomicU64::new(0),
            omit_fresh_notices: attr(x, "omit-fresh-notices").is_some(),
            fresh_gate: attr(x, "fresh-gate"),
            fail_fresh_after: attr(x, "fail-fresh-after").and_then(|v| v.parse().ok()),
            skip_after: attr(x, "skip-after").and_then(|v| v.parse::<u64>().ok()),
            blocks_seen: AtomicU64::new(0),
            feedback: attr(x, "feedback")
                .and_then(|v| v.parse::<f32>().ok())
                .unwrap_or(0.0),
            quarantined,
            state: Mutex::new(Vec::new()),
            slow_get_ms: AtomicU64::new(0),
            bare_get: AtomicBool::new(false),
            controller_refuses: !quarantined && attr(x, "controller-refuses").is_some(),
            applied_lie: !quarantined && attr(x, "applied-lie").is_some(),
            child_gen: AtomicU64::new(1),
            editor_open: AtomicBool::new(false),
            drift: AtomicBool::new(false),
            drift_get: AtomicBool::new(false),
            // `toolarge=<bytes>`: the slot starts as a `TOOLARGE=<bytes>` body would leave it (a test of
            // a stale slot with no state at all; before 4.0 such a slot came from a 3.2 child, which has no 4.0 path).
            too_large: AtomicU64::new(
                attr(x, "toolarge")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            ),
            cell: Cell::new(Dsp::new(delay, echo)),
            latency: AtomicU32::new(delay as u32),
            restart: AtomicU64::new(0),
            bypassed: AtomicU32::new(0),
            misaligned: AtomicU32::new(0),
            pending: Mutex::new(None),
            block_once_ms: AtomicU64::new(0),
            enabled: AtomicBool::new(s.enabled),
            lag_probe: AtomicU32::new(0),
            probe_ready: AtomicBool::new(false),
            probe_in_block: AtomicBool::new(false),
            probe_counted: AtomicBool::new(false),
            probe_drop_one: AtomicBool::new(false),
            params: fparams,
            orders,
        }));
        infos.push(codec::SlotInfo::new(
            !quarantined,
            name,
            m.ch_out,
            reported_delay,
            params,
            // Protocol 3.4: written only when false (absent = true). A child that says it speaks 3.3
            // (`--hello-protocol`) has no such field: its slots decode true, and `editor.open` is where it says no.
            !quarantined
                && !(sh.opts.no_editor
                    && !sh.opts.editor_fails_at_open
                    && claims_3(sh.opts.hello_protocol.unwrap_or(ABI_VERSION), 4)),
            plugins
                .last()
                .map(|p| p.notices.clone())
                .unwrap_or_default(),
        ));
    }
    clear_status(sh);
    let parked = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let page = sh
        .page
        .lock()
        .unwrap()
        .clone()
        .expect("hello before chain.load");
    let thread = {
        let (r, pk, st, pl) = (
            region.clone(),
            parked.clone(),
            stop.clone(),
            plugins.clone(),
        );
        let tc = sh.opts.tc_us;
        let sr = m.rate;
        let stats = sh.opts.stats.is_some();
        // D22: one bypass delay line per hosted effect slot, preallocated here on the main thread (the fake's
        // chain.prepare is a no-op), C = max(L, 16,384) frames.
        let lines: Vec<Option<DelayLine>> = plugins
            .iter()
            .map(|p| {
                (!p.quarantined && !p.synth).then(|| {
                    DelayLine::new(
                        2,
                        p.latency.load(Ordering::Relaxed) as usize,
                        geo.max_frames as usize,
                    )
                })
            })
            .collect();
        let mutants = (sh.opts.mutant_hard_switch, sh.opts.mutant_no_delay_line);
        let modelled = sh.opts.hist_modelled;
        std::thread::Builder::new()
            .name(format!("fake-chain-{key}"))
            .spawn(move || {
                chain_thread(
                    r, page, pk, st, pl, lines, tc, sr, stats, mutants, render, modelled,
                )
            })
            .expect("chain thread")
    };
    // (R3 row 1, as the plugin-host child will): this process's view of the region wired in place, reported.
    let (wired_bytes, unwired_bytes) = match region.wire() {
        Ok(n) => (n, 0),
        Err(e) => (0, e.bytes as u64),
    };
    let n: u32 = infos.iter().map(|i| i.param_count).sum();
    sh.chains.lock().unwrap().insert(
        key,
        Chain {
            key,
            role: m.role,
            geometry: geo,
            rate: m.rate,
            region,
            plugins,
            parked,
            stop,
            thread: Some(thread),
        },
    );
    let loaded = codec::ChainLoaded::new(key, true, infos, latency, n, wired_bytes, unwired_bytes);
    if sh.opts.chain_loaded_v0 {
        // An older child: neither field on the wire.
        let mut v = serde_json::to_value(&loaded).expect("a ChainLoaded encodes");
        if let Some(o) = v.as_object_mut() {
            o.remove("wiredBytes");
            o.remove("unwiredBytes");
        }
        event(w, kind::CHAIN_LOADED, &v, &[]);
    } else {
        event(w, kind::CHAIN_LOADED, &loaded, &[]);
    }
}

/// the plugin-host child's R1 (`host.rs` `render_mode`), the same rule: whether a `chain.load` is a render set.
fn render_mode(role: codec::Role, flags: u32) -> Result<bool, &'static str> {
    let render = role == codec::Role::Render;
    if flags == 0 {
        return Ok(render);
    }
    if flags & !(FLAG_RENDER | FLAG_ACK_WAKE) != 0 {
        return Err("the region's flags carry an unknown bit");
    }
    let (fr, fa) = (flags & FLAG_RENDER != 0, flags & FLAG_ACK_WAKE != 0);
    if fr != render || fa != fr {
        return Err("the region's flags disagree with the chain's role");
    }
    Ok(render)
}

fn message_thread(ops: Receiver<Op>, w: Writer, sh: Arc<Shared>, own: Option<Arc<MachReceive>>) {
    // `--editor-user-close-ms`: the windows the "user" closes, and when.
    let mut user_close: Vec<(u64, u32, Instant)> = Vec::new();
    // The "user" closes each slot's window once (its first open), so a test can re-open it and keep it.
    let mut user_closed: std::collections::BTreeSet<(u64, u32)> = std::collections::BTreeSet::new();
    loop {
        let now = Instant::now();
        user_close.retain(|&(key, slot, at)| {
            if at > now {
                return true;
            }
            if let Some(p) = plugin_of(&sh, key, slot)
                && p.editor_open.swap(false, Ordering::SeqCst)
            {
                let blob = framed_state(&p);
                event(
                    &w,
                    kind::EDITOR_CLOSED,
                    &codec::EditorClosed::new(key, slot, p.child_gen.load(Ordering::SeqCst)),
                    &blob,
                );
            }
            false
        });
        // D14's tick (60 Hz) for D22's restart flags. Each pass is one drain, then one turn: the drain handles exactly
        // the ops queued when it starts (a snapshot of the reader's count), so the flags they raise are taken
        // in one turn (P1b), and a steady op stream cannot starve the turns: what arrives during the drain waits for
        // the next pass.
        let first = match ops.recv_timeout(Duration::from_millis(16)) {
            Ok(op) => op,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                reactivations(&sh, &w);
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let snapshot = sh.queued.load(Ordering::SeqCst).max(1);
        let mut batch = vec![first];
        while batch.len() < snapshot {
            match ops.try_recv() {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        sh.queued.fetch_sub(batch.len(), Ordering::SeqCst);
        for op in batch {
            // an op's `corr` is the echo of the completion events it sends, and of nothing after it.
            if let Op::Echo(c) = op {
                ECHO.with(|e| *e.borrow_mut() = Some(c));
                continue;
            }
            match op {
                Op::Echo(_) => {}
                Op::Load(key, m, fd) => {
                    // held while the test's gate file exists (bounded: 60 s).
                    if let Some(g) = &sh.opts.load_gate {
                        let end = Instant::now() + Duration::from_secs(60);
                        while std::path::Path::new(g).exists() && Instant::now() < end {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                    if let Some(dir) = &sh.opts.load_tokens {
                        take_token(dir);
                    }
                    load(&sh, &w, key, m, fd)
                }
                Op::Unload(key) => {
                    let c = sh.chains.lock().unwrap().remove(&key);
                    if let Some(mut c) = c {
                        status(&sh, OP_UNLOAD, key, 0, "");
                        stop_chain(&mut c, &sh);
                        clear_status(&sh);
                    }
                    let m = codec::ChainRef::new(key, true);
                    event(&w, kind::CHAIN_UNLOADED, &m, &[]);
                }
                Op::Prepare(key) => {
                    // a 3.6 double reports the chain's pair again: its region's wired view plus one page,
                    // the double's stand-in for the processor blocks a real prepare rebuilds (so a test sees the
                    // prepare's pair replace the load's).
                    let pair = sh
                        .chains
                        .lock()
                        .unwrap()
                        .get(&key)
                        .map(|c| c.region.wired_bytes() + runecraft_rt::os::page_size() as u64)
                        .filter(|_| {
                            takes_3_6(sh.said.load(Ordering::SeqCst)) && !sh.opts.chain_loaded_v0
                        });
                    // every slot's whole set after the prepare, sent only when one has any (as the child).
                    let mut slot_notices: Vec<Vec<String>> = sh
                        .chains
                        .lock()
                        .unwrap()
                        .get(&key)
                        .map(|c| {
                            c.plugins
                                .iter()
                                .map(|p| {
                                    *p.current_notices.lock().unwrap() = p.prep_notices.clone();
                                    p.prep_notices.clone()
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if slot_notices.iter().all(Vec::is_empty) {
                        slot_notices.clear();
                    }
                    let m =
                        codec::ChainPrepared::new(key, true, pair, pair.map(|_| 0), slot_notices);
                    event(&w, kind::CHAIN_PREPARED, &m, &[]);
                }
                Op::PrepareFresh(key) => {
                    // as the plugin-host child's prepare on a render set: every slot's processor released and made
                    // again on the kept plugin (its DSP state, the echo's tail and the held notes, from zero; its
                    // first-use latch kept). A live chain or an unknown key is refused.
                    if sh.opts.prepare_fresh_ms > 0 {
                        std::thread::sleep(Duration::from_millis(sh.opts.prepare_fresh_ms));
                    }
                    let t0 = Instant::now();
                    let found = sh
                        .chains
                        .lock()
                        .unwrap()
                        .get(&key)
                        .map(|c| (c.role, c.plugins.clone()));
                    let mut m = {
                        let mut msg = codec::ChainPreparedFresh::default();
                        msg.chain_key = key;
                        msg
                    };
                    match found {
                        Some((codec::Role::Render, plugins)) => {
                            if plugins.iter().any(|p| p.crash_fresh) {
                                die();
                            }
                            let mut ok = true;
                            let before: u32 = plugins
                                .iter()
                                .map(|p| p.latency.load(Ordering::SeqCst))
                                .sum();
                            for p in &plugins {
                                if !p.cell.announce() {
                                    ok = false;
                                    continue;
                                }
                                // A render set between passes has no block in flight (the host application asks only between
                                // them), so the take is forced at once rather than waiting for a block to drain it.
                                match p.cell.take_blocking(
                                    Duration::from_millis(1),
                                    Duration::from_millis(1),
                                    handover::TAKE_GIVE_UP,
                                ) {
                                    Ok(t) => {
                                        let mut g = t.guard;
                                        let delay = g.delay + p.fresh_delay;
                                        *g = Dsp::new(delay, p.echo);
                                        p.cell.give_back(g);
                                        p.latency.store(delay as u32, Ordering::SeqCst);
                                    }
                                    Err(_) => ok = false,
                                }
                            }
                            m.ok = ok;
                            if ok
                                && sh.said.load(Ordering::SeqCst) >= (3 << 16 | 10)
                                && !plugins.iter().any(|p| p.omit_fresh_notices)
                            {
                                m.slot_notices = Some(
                                    plugins
                                        .iter()
                                        .map(|p| {
                                            let i =
                                                p.fresh_notice_index.fetch_add(1, Ordering::SeqCst)
                                                    as usize;
                                            let v = p
                                                .fresh_notices
                                                .get(i)
                                                .or_else(|| p.fresh_notices.last())
                                                .unwrap_or(&p.prep_notices)
                                                .clone();
                                            *p.current_notices.lock().unwrap() = v.clone();
                                            v
                                        })
                                        .collect(),
                                );
                            }
                            // partial-output tests: deterministic barrier at the second fresh completion,
                            // after B is prepared but before it can contribute an output. Lab fake only.
                            for p in &plugins {
                                let count = p.fresh_notice_index.load(Ordering::SeqCst);
                                if count == 2
                                    && let Some(gate) = &p.fresh_gate
                                {
                                    std::fs::write(format!("{gate}.entered"), b"ready").unwrap();
                                    let until = Instant::now() + Duration::from_secs(30);
                                    while !std::path::Path::new(&format!("{gate}.release")).exists()
                                    {
                                        assert!(
                                            Instant::now() < until,
                                            "fresh gate was not released"
                                        );
                                        std::thread::sleep(Duration::from_millis(1));
                                    }
                                }
                                if p.fail_fresh_after.is_some_and(|n| count > n) {
                                    m.ok = false;
                                    m.error = Some("fixture fresh prepare refused".into());
                                }
                            }
                            m.latency = plugins
                                .iter()
                                .map(|p| p.latency.load(Ordering::SeqCst))
                                .sum();
                            // As the plugin-host child's: `latency.changed` first when the prepare moved it.
                            if m.latency != before {
                                event(
                                    &w,
                                    kind::LATENCY_CHANGED,
                                    &codec::LatencyChanged::new(key, m.latency),
                                    &[],
                                );
                            }
                            if !ok {
                                m.error = Some("a slot's processor could not be taken".into());
                            }
                        }
                        Some(_) => m.error = Some("not a render set".into()),
                        None => m.error = Some("unknown chain".into()),
                    }
                    m.ms = t0.elapsed().as_secs_f64() * 1000.0;
                    event(&w, kind::CHAIN_PREPARED_FRESH, &m, &[]);
                }
                Op::SetState(m, blob) => {
                    // D21: `sasp` is the only mode; the fake slot is a {builtin} test double (or, with
                    // `controller-refuses`, a vst3-shaped one). A both-absent vst3 blob is the no-op (ok, nothing reaches
                    // the plugin); anything else is refused before the plugin.
                    let decoded = if m.mode == codec::StateMode::V1 {
                        state_blob::decode(&blob).ok()
                    } else {
                        None
                    };
                    let plugin = plugin_of(&sh, m.chain_key, m.slot).filter(|p| !p.quarantined);
                    let (ok, applied) = match (plugin, decoded) {
                        (Some(_), Some(b)) if b.both_absent() => (true, None),
                        (Some(p), Some(state_blob::Blob::Builtin { body }))
                            if !p.controller_refuses =>
                        {
                            if p.notice_on_state {
                                *p.current_notices.lock().unwrap() = notice_codes(body_knob(
                                    &String::from_utf8_lossy(body),
                                    "prepnotices",
                                ));
                            }
                            let ok = set_state(&sh, &p, &m, body.to_vec());
                            // The hostile `applied-lie` double: ok:true with an `applied` (invalid, N12).
                            let lie =
                                (ok && p.applied_lie).then_some(codec::Applied::new(true, false));
                            (ok, lie)
                        }
                        (
                            Some(p),
                            Some(state_blob::Blob::Vst3 {
                                component,
                                controller,
                            }),
                        ) if p.controller_refuses => {
                            // The component's setState, then the controller's, which refuses: a partial set when the
                            // component chunk reached the plugin.
                            let comp = component.map(|c| set_state(&sh, &p, &m, c.to_vec()));
                            match (comp, controller.is_some()) {
                                (Some(_), true) => (false, Some(codec::Applied::new(true, false))),
                                (Some(c), false) => (c, None),
                                (None, refused) => (!refused, None),
                            }
                        }
                        _ => (false, None),
                    };
                    if ok && plugin_of(&sh, m.chain_key, m.slot).is_some_and(|p| p.notice_on_state)
                    {
                        let slot_notices = sh.chains.lock().unwrap().get(&m.chain_key).map_or_else(
                            Vec::new,
                            |c| {
                                c.plugins
                                    .iter()
                                    .map(|p| p.current_notices.lock().unwrap().clone())
                                    .collect()
                            },
                        );
                        event(
                            &w,
                            kind::CHAIN_PREPARED,
                            &{
                                let mut msg = codec::ChainPrepared::default();
                                msg.chain_key = m.chain_key;
                                msg.ok = true;
                                msg.slot_notices = slot_notices;
                                msg
                            },
                            &[],
                        );
                    }
                    event(
                        &w,
                        kind::STATE_APPLIED,
                        &codec::StateApplied::new(
                            m.chain_key,
                            m.slot,
                            m.generation,
                            ok,
                            applied,
                            // The slot's generation once the set took effect.
                            claims_3(sh.said.load(Ordering::SeqCst), 3).then(|| {
                                plugin_of(&sh, m.chain_key, m.slot)
                                    .map_or(0, |p| p.child_gen.load(Ordering::SeqCst))
                            }),
                        ),
                        &[],
                    );
                }
                Op::GetState(id, m) => match plugin_of(&sh, m.chain_key, m.slot) {
                    Some(p) => {
                        status(&sh, OP_GET_STATE, m.chain_key, m.slot, &p.name);
                        let ms = p.slow_get_ms.load(Ordering::SeqCst);
                        if ms > 0 {
                            std::thread::sleep(Duration::from_millis(ms));
                        }
                        if p.drift_get.load(Ordering::SeqCst) {
                            p.state.lock().unwrap().extend_from_slice(b";g");
                            p.child_gen.fetch_add(1, Ordering::SeqCst);
                        }
                        let blob = framed_state(&p);
                        clear_status(&sh);
                        // `TOOLARGE=<bytes>` (protocol 3.3): the state "exceeds the frame cap"; the
                        // reply says so and carries no blob.
                        let big = p.too_large.load(Ordering::SeqCst);
                        if big > 0 && claims_3(sh.said.load(Ordering::SeqCst), 3) {
                            reply(
                                &w,
                                kind::STATE_GET,
                                id,
                                &codec::StateGetReply::new(
                                    m.chain_key,
                                    m.slot,
                                    p.child_gen.load(Ordering::SeqCst),
                                    false,
                                    Some(codec::StateGetReply::TOO_LARGE.into()),
                                    Some(big),
                                ),
                                &[],
                            );
                        } else {
                            reply(
                                &w,
                                kind::STATE_GET,
                                id,
                                &{
                                    let mut msg = codec::StateGetReply::default();
                                    msg.chain_key = m.chain_key;
                                    msg.slot = m.slot;
                                    msg.child_gen = p.child_gen.load(Ordering::SeqCst);
                                    msg
                                },
                                &blob,
                            );
                        }
                    }
                    None => send(
                        &w,
                        codec::encode_msg(
                            kind::STATE_GET,
                            FF_REPLY | FF_ERROR,
                            id,
                            &codec::ErrorReply::new(-32602, "no such slot".into()),
                            &[],
                        ),
                    ),
                },
                Op::EditorOpen(m) => {
                    let slot = m.slot.unwrap_or(0);
                    let render = sh
                        .chains
                        .lock()
                        .unwrap()
                        .get(&m.chain_key)
                        .is_some_and(|c| c.role == codec::Role::Render);
                    if let Some(p) = plugin_of(&sh, m.chain_key, slot) {
                        // As the child: a render set, a quarantined slot or no editor (`--no-editor`) opens no
                        // window and answers `editor.availability`.
                        if render || p.quarantined || sh.opts.no_editor {
                            let r = codec::EditorRef::new(m.chain_key, Some(slot), false);
                            event(&w, kind::EDITOR_AVAILABILITY, &r, &[]);
                        } else if !p.editor_open.swap(true, Ordering::SeqCst) {
                            // An editor edit: the state moves under the host application's feet (C3). Already open: brought to
                            // front, nothing moves (gap 1).
                            p.state.lock().unwrap().extend_from_slice(b";edited");
                            p.child_gen.fetch_add(1, Ordering::SeqCst);
                            if let Some(ms) = sh.opts.editor_user_close_ms
                                && user_closed.insert((m.chain_key, slot))
                            {
                                user_close.push((
                                    m.chain_key,
                                    slot,
                                    Instant::now() + Duration::from_millis(ms),
                                ));
                            }
                        }
                    }
                }
                Op::EditorClose(m) => {
                    let plugins: Vec<Arc<Plugin>> = sh
                        .chains
                        .lock()
                        .unwrap()
                        .get(&m.chain_key)
                        .map(|c| c.plugins.clone())
                        .unwrap_or_default();
                    for (i, p) in plugins.iter().enumerate() {
                        if m.slot.is_none_or(|s| s as usize == i)
                            && p.editor_open.swap(false, Ordering::SeqCst)
                        {
                            let blob = framed_state(p);
                            event(
                                &w,
                                kind::EDITOR_CLOSED,
                                &codec::EditorClosed::new(
                                    m.chain_key,
                                    i as u32,
                                    p.child_gen.load(Ordering::SeqCst),
                                ),
                                &blob,
                            );
                        }
                    }
                }
                Op::Workgroup(m) => {
                    if let (Some(name), Some(own)) = (m.bootstrap_name, own.as_ref()) {
                        // Route A: look the one-shot name up and send the channel hello there.
                        if let Ok(s) = MachSend::look_up(&name) {
                            let _ = s.send_making(std::process::id() as u64, own, 1000);
                        }
                    }
                }
                Op::ParamList(id, m) => {
                    let plugin = plugin_of(&sh, m.chain_key, m.slot).filter(|p| !p.quarantined);
                    let n = plugin.as_ref().map_or(0, |p| p.params.len() as u32);
                    let v37 = claims_3(sh.said.load(Ordering::SeqCst), 7);
                    let params: Vec<codec::ParamEntry> = (m.offset
                        ..(m.offset + m.count.min(256)).min(n))
                        .filter_map(|i| {
                            let p = plugin.as_ref()?;
                            let x = p.params.get(i as usize)?;
                            Some(if v37 {
                                {
                                    let mut msg = codec::ParamEntry::default();
                                    msg.id = m.slot << 24 | i;
                                    msg.name = x.name.clone();
                                    msg.plugin_id = x.plugin_id;
                                    msg.automatable = x.automatable;
                                    msg.default = x.default;
                                    msg.value = fake_param_value(p, i as usize);
                                    msg.step_count = x.steps;
                                    msg.label = x.label.clone();
                                    msg.order =
                                        p.orders.get(i as usize).copied().unwrap_or(u32::MAX);
                                    msg
                                }
                            } else {
                                {
                                    let mut msg = codec::ParamEntry::default();
                                    msg.id = m.slot << 24 | i;
                                    msg.name = x.name.clone();
                                    msg
                                }
                            })
                        })
                        .collect();
                    // a 3.7 page stays within 48 KiB of meta (the host application pages by what it receives).
                    let params = if v37 {
                        codec::page_within(params, summoner_plugin::params::PAGE_BYTES)
                    } else {
                        params
                    };
                    reply(
                        &w,
                        kind::PARAM_LIST,
                        id,
                        &codec::ParamListReply::new(m.chain_key, m.slot, n, params),
                        &[],
                    );
                }
                // (protocol 3.7), as the plugin-host child's double: the value goes to the processor (applied by the
                // chain thread before its next block) and, as a plugin's controller does, into the state at
                // once; the slot's generation moves; the reply is the read-back.
                Op::ParamSet(id, m) => {
                    let p = plugin_of(&sh, m.chain_key, m.slot).filter(|p| !p.quarantined);
                    let at = p.as_ref().and_then(|p| {
                        p.params
                            .iter()
                            .position(|x| x.plugin_id == m.plugin_id && x.settable)
                    });
                    match (p, at) {
                        (Some(p), Some(i)) if m.value.is_finite() => {
                            let v = m.value.clamp(0.0, 1.0);
                            let x = &p.params[i];
                            x.pending.store(v.to_bits(), Ordering::SeqCst);
                            x.dirty.store(true, Ordering::SeqCst);
                            {
                                let mut body = p.state.lock().unwrap();
                                if i == 0 {
                                    set_knob(&mut body, "gain", v);
                                } else {
                                    set_knob(&mut body, &format!("p{}", x.plugin_id), v);
                                }
                            }
                            p.child_gen.fetch_add(1, Ordering::SeqCst);
                            reply(
                                &w,
                                kind::PARAM_SET,
                                id,
                                &codec::ParamSetReply::new(
                                    m.chain_key,
                                    m.slot,
                                    m.plugin_id,
                                    fake_param_value(&p, i),
                                ),
                                &[],
                            );
                        }
                        (p, _) => send(
                            &w,
                            codec::encode_msg(
                                kind::PARAM_SET,
                                FF_REPLY | FF_ERROR,
                                id,
                                &codec::ErrorReply::new(
                                    -32602,
                                    if p.is_none() {
                                        "param.set: no such chain or slot".into()
                                    } else if !m.value.is_finite() {
                                        "param.set: the value is not finite".into()
                                    } else {
                                        format!("param.set: no settable parameter {}", m.plugin_id)
                                    },
                                ),
                                &[],
                            ),
                        ),
                    }
                }
                Op::Shutdown(id) => {
                    let mut chains = std::mem::take(&mut *sh.chains.lock().unwrap());
                    let mut all: Vec<(u64, Vec<u64>)> = Vec::new();
                    for (k, c) in chains.iter_mut() {
                        all.push((*k, stop_chain(c, &sh)));
                    }
                    if let Some(path) = &sh.opts.stats {
                        let mut text = String::new();
                        for (k, v) in &all {
                            for p in v.as_chunks::<2>().0 {
                                text.push_str(&format!(
                                    "{k} {} {}\n",
                                    p[0],
                                    runecraft_ipc::bridge::ticks_to_ns(p[1])
                                ));
                            }
                        }
                        let _ = std::fs::write(path, text);
                    }
                    reply(&w, kind::SHUTDOWN, id, &empty(), &[]);
                    std::process::exit(0);
                }
            }
            ECHO.with(|e| *e.borrow_mut() = None);
        }
        reactivations(&sh, &w);
    }
    std::process::exit(0);
}

/// Apply a `{builtin}` body to a fake plugin (the message thread, inside the state-set call).
fn set_state(sh: &Shared, p: &Plugin, m: &codec::StateSet, body: Vec<u8>) -> bool {
    status(sh, OP_SET_STATE, m.chain_key, m.slot, &p.name);
    let text = String::from_utf8_lossy(&body).to_string();
    if text.starts_with("CRASH") {
        die();
    }
    // `SLOWSET=<ms>`: the state is applied (and its receipt sent) that much later.
    for part in text.split(';') {
        if let Some(ms) = part
            .strip_prefix("SLOWSET=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(Duration::from_millis(ms.min(60_000)));
        }
    }
    let mut restart: Option<u64> = None;
    let mut cycle_ms = 0u64;
    for part in text.split(';') {
        if let Some(g) = part
            .strip_prefix("gain=")
            .and_then(|v| v.parse::<f32>().ok())
        {
            p.gain.store(g.to_bits(), Ordering::SeqCst);
        }
        // `p<pluginId>=<v>`, an `xparams` entry's value (what a `param.set` writes into the body).
        if let Some((id, v)) = part
            .strip_prefix('p')
            .and_then(|r| r.split_once('='))
            .and_then(|(id, v)| Some((id.parse::<u32>().ok()?, v.parse::<f32>().ok()?)))
            && let Some(x) = p.params.iter().find(|x| x.plugin_id == id && id >= 100)
        {
            x.applied.store(v.to_bits(), Ordering::SeqCst);
        }
        if let Some(ms) = part
            .strip_prefix("SLOWGET=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            p.slow_get_ms.store(ms, Ordering::SeqCst);
        }
        if part == "BAREGET" {
            p.bare_get.store(true, Ordering::SeqCst);
        }
        if part == "DRIFT" {
            p.drift.store(true, Ordering::SeqCst);
        }
        if part == "DRIFTGET" {
            p.drift_get.store(true, Ordering::SeqCst);
        }
        if let Some(n) = part
            .strip_prefix("TOOLARGE=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            p.too_large.store(n, Ordering::SeqCst);
        }
        if let Some(ms) = part
            .strip_prefix("BLOCKMS=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            p.block_once_ms.store(ms.min(60_000), Ordering::SeqCst);
        }
        if let Some(n) = part
            .strip_prefix("RESTART=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            restart = Some(n.min(1 << 20));
        }
        if let Some(ms) = part
            .strip_prefix("CYCLEMS=")
            .and_then(|v| v.parse::<u64>().ok())
        {
            cycle_ms = ms.min(60_000);
        }
        if let Some(k) = part
            .strip_prefix("LAGPROBE=")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|k| matches!(k, 2 | 3))
        {
            p.probe_ready.store(false, Ordering::SeqCst);
            p.probe_in_block.store(false, Ordering::SeqCst);
            p.probe_counted.store(false, Ordering::SeqCst);
            p.probe_drop_one.store(k == 3, Ordering::SeqCst);
            p.lag_probe.store(k, Ordering::SeqCst);
        }
    }
    let fail = text.starts_with("FAIL");
    *p.state.lock().unwrap() = body;
    p.child_gen.fetch_add(1, Ordering::SeqCst);
    // `restartComponent(kLatencyChanged)` from inside setState: the flag only; the main loop takes it (D22 step 1).
    if let Some(n) = restart {
        p.restart.store((n + 1) | cycle_ms << 32, Ordering::SeqCst);
    }
    clear_status(sh);
    !fail
}

/// What `state.get` and `editor.closed` answer: the body framed as a `{builtin}` state blob (D21), or bare under `BAREGET`.
fn framed_state(p: &Plugin) -> Vec<u8> {
    let body = p.state.lock().unwrap().clone();
    if p.bare_get.load(Ordering::SeqCst) {
        body
    } else if p.controller_refuses {
        state_blob::encode(&state_blob::Blob::Vst3 {
            component: Some(&body),
            controller: None,
        })
        .unwrap_or_default()
    } else {
        state_blob::builtin(&body)
    }
}

/// D22 on the main thread, one turn: every slot whose restart flag is up announces and takes its processor, runs the
/// cycle, gives it back, and sends `slot.reactivated` (then `latency.changed` if the latency moved). A slot whose word
/// is not `HOSTED` (a cycle still returning) keeps its flag for the next turn.
///
/// The turn's **one** blocking take (`take_blocking`, bounded by `TAKE_GIVE_UP`, 2 s) is the first
/// one; once a take in this turn has been given up, every later take goes to the re-attempt path, as does the given-up
/// one: a non-blocking attempt per turn (force past the take's bound, then `take()`: `OUT` plus `try_lock`), until it
/// succeeds (the cycle runs, `slot.reactivated {takeGivenUp}`) or `TAKE_HORIZON` (5 s from its announce) passes
/// (`slot.reactivated {abandoned}`: the host application quarantines the slot). So a turn waits at most 2 s, whatever the number
/// of slots, and no slot stays out of its cell silently.
fn reactivations(sh: &Arc<Shared>, w: &Writer) {
    type Snapshot = (u64, codec::Role, Geometry, f64, Vec<Arc<Plugin>>);
    let chains: Vec<Snapshot> = sh
        .chains
        .lock()
        .unwrap()
        .values()
        .map(|c| (c.key, c.role, c.geometry, c.rate, c.plugins.clone()))
        .collect();
    let mut turn = handover::Turn::new();
    // `--turns` only (test-only): the turn's `begin` line, if it has a take to make.
    let record = sh.turns.as_ref().and_then(|log| {
        let (mut flags, mut pending) = (Vec::new(), Vec::new());
        for (key, _, _, _, plugins) in &chains {
            for (i, p) in plugins.iter().enumerate() {
                if p.quarantined {
                    continue;
                }
                if p.pending.lock().unwrap().is_some() {
                    pending.push(format!("{key}/{i}"));
                } else if p.restart.load(Ordering::SeqCst) != 0 {
                    flags.push(format!("{key}/{i}"));
                }
            }
        }
        if flags.is_empty() && pending.is_empty() {
            return None;
        }
        let seq = log.seq.fetch_add(1, Ordering::Relaxed);
        log.line(format!(
            "begin {seq} flags={} pending={}\n",
            TurnLog::list(&flags),
            TurnLog::list(&pending)
        ));
        Some((log, seq, Instant::now()))
    });
    let mut announced: Vec<String> = Vec::new();
    for (key, role, geo, rate, plugins) in chains {
        let bound = handover::take_bound(geo.max_frames, rate);
        let horizon = handover::take_horizon(role == codec::Role::Render);
        for (i, p) in plugins.iter().enumerate() {
            if p.quarantined {
                continue;
            }
            let mut pend = p.pending.lock().unwrap();
            if pend.is_none() {
                let req = p.restart.swap(0, Ordering::SeqCst);
                if req == 0 {
                    continue;
                }
                if !p.cell.announce() {
                    p.restart.store(req, Ordering::SeqCst);
                    continue;
                }
                if record.is_some() {
                    announced.push(format!("{key}/{i}"));
                }
                let pt = PendingTake {
                    new_latency: ((req & 0xffff_ffff) - 1) as usize,
                    cycle_ms: req >> 32,
                    announced: Instant::now(),
                    forced: false,
                };
                if turn.may_wait() {
                    status(sh, OP_PREPARE, key, i as u32, &p.name);
                    let r = p.cell.take_blocking(
                        bound,
                        Duration::from_millis(1),
                        handover::TAKE_GIVE_UP,
                    );
                    clear_status(sh);
                    match r {
                        Ok(t) => {
                            let forced = t.forced;
                            cycle(w, key, i, p, &plugins, t.guard, pt, forced, false);
                            continue;
                        }
                        Err(e) => {
                            turn.record_waits(1);
                            runecraft_rt::safe_eprintln!(
                                "fake-host: chain {key} slot {i}: the take gave up at its bound ({e:?}); re-attempted each turn"
                            );
                            *pend = Some(PendingTake {
                                forced: p.cell.state() == handover::OUT,
                                ..pt
                            });
                            continue;
                        }
                    }
                }
                // This turn's one wait is spent: the re-attempt path, starting now.
                *pend = Some(pt);
            }
            let Some(mut pt) = *pend else { continue };
            let attempt = p.cell.try_take(pt.announced.elapsed() >= bound);
            pt.forced |= attempt.forced;
            turn.record_waits(attempt.waits);
            match attempt.guard {
                Some(g) => {
                    *pend = None;
                    cycle(w, key, i, p, &plugins, g, pt, pt.forced, true);
                }
                None if pt.announced.elapsed() >= horizon => {
                    *pend = None;
                    runecraft_rt::safe_eprintln!(
                        "fake-host: chain {key} slot {i}: the take did not complete in {horizon:?}; abandoned"
                    );
                    event(
                        w,
                        kind::SLOT_REACTIVATED,
                        &{
                            let mut msg = codec::SlotReactivated::default();
                            msg.chain_key = key;
                            msg.slot = i as u32;
                            msg.flags = handover::K_LATENCY_CHANGED;
                            msg.bypassed_blocks = p.bypassed.swap(0, Ordering::SeqCst);
                            msg.cycle_ns = 0;
                            msg.latency = p.latency.load(Ordering::SeqCst);
                            msg.forced_take = pt.forced;
                            msg.misaligned = p.misaligned.swap(0, Ordering::SeqCst);
                            msg.take_given_up = true;
                            msg.abandoned = true;
                            msg
                        },
                        &[],
                    );
                }
                None => *pend = Some(pt),
            }
        }
    }
    if let Some((log, seq, t)) = record {
        log.line(format!(
            "end {seq} announced={} waits={} us={}\n",
            TurnLog::list(&announced),
            turn.waits(),
            t.elapsed().as_micros()
        ));
    }
}

/// The cycle, with the processor taken: setProcessing(false), setActive(false), setupProcessing, setActive(true),
/// setProcessing(true) (a fake plugin comes back fresh, voices and lines, at its new latency); then the give-back,
/// `slot.reactivated`, and `latency.changed` if the latency moved.
#[allow(clippy::too_many_arguments)]
fn cycle(
    w: &Writer,
    key: u64,
    i: usize,
    p: &Plugin,
    plugins: &[Arc<Plugin>],
    mut guard: runecraft_rt::rt_lock::WiredLockGuard<'_, Dsp>,
    pt: PendingTake,
    forced: bool,
    given_up: bool,
) {
    let t0 = Instant::now();
    *guard = Dsp::new(pt.new_latency, p.echo);
    if pt.cycle_ms > 0 {
        std::thread::sleep(Duration::from_millis(pt.cycle_ms));
    }
    let cycle_ns = t0.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    // T17's probe: give back only once the chain thread is inside a bypassed block (bounded: 2 s, then go on).
    let probe = p.lag_probe.load(Ordering::SeqCst) != 0;
    if probe {
        p.probe_ready.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(2);
        while !p.probe_in_block.load(Ordering::SeqCst) && Instant::now() < until {
            std::thread::sleep(Duration::from_micros(50));
        }
    }
    p.cell.give_back(guard);
    let old = p.latency.load(Ordering::SeqCst) as usize;
    event(
        w,
        kind::SLOT_REACTIVATED,
        &{
            let mut msg = codec::SlotReactivated::default();
            msg.chain_key = key;
            msg.slot = i as u32;
            msg.flags = handover::K_LATENCY_CHANGED;
            msg.bypassed_blocks = p.bypassed.swap(0, Ordering::SeqCst);
            msg.cycle_ns = cycle_ns;
            msg.latency = pt.new_latency as u32;
            msg.forced_take = forced;
            msg.misaligned = p.misaligned.swap(0, Ordering::SeqCst);
            msg.take_given_up = given_up;
            msg.abandoned = false;
            msg
        },
        &[],
    );
    if probe {
        // Counted: the chain thread may end its block (one-shot: disarmed).
        p.lag_probe.store(0, Ordering::SeqCst);
        p.probe_counted.store(true, Ordering::SeqCst);
    }
    if pt.new_latency != old {
        p.latency.store(pt.new_latency as u32, Ordering::SeqCst);
        let total = plugins
            .iter()
            .map(|q| q.latency.load(Ordering::SeqCst))
            .sum();
        event(
            w,
            kind::LATENCY_CHANGED,
            &codec::LatencyChanged::new(key, total),
            &[],
        );
    }
}

fn stop_chain(c: &mut Chain, sh: &Shared) -> Vec<u64> {
    c.stop.store(true, Ordering::SeqCst);
    let page = sh.page.lock().unwrap().clone();
    let deadline = Instant::now() + Duration::from_millis(500);
    while c.thread.as_ref().is_some_and(|t| !t.is_finished()) && Instant::now() < deadline {
        let _ = c.region.seq_waker().wake_all();
        if let Some(p) = &page {
            for g in 0..RING_GROUPS {
                let _ = p.hub_waker(g).wake();
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    match c.thread.take() {
        Some(t) if t.is_finished() => t.join().unwrap_or_default(),
        _ => Vec::new(), // a wedged thread is abandoned
    }
}

/// A fake plugin's DSP state: the processor D22's cell holds. The chain thread reaches it only by `try_lock`; the main
/// thread replaces it whole in a reactivation cycle (a fresh plugin at its new latency).
struct Dsp {
    /// The plugin's latency: a fixed delay of `delay` frames.
    delay: usize,
    line: [Vec<f32>; 2],
    pos: usize,
    /// The decaying delay's line and position (`echo`).
    echo: [Vec<f32>; 2],
    epos: usize,
    held: u32,
    /// `sustain`: the channels whose pedal is down, and per channel the notes let go under it (still in `held`).
    pedal: u16,
    sustained: [u32; 16],
}

impl Dsp {
    fn new(delay: usize, echo: usize) -> Dsp {
        Dsp {
            delay,
            line: [vec![0.0; delay.max(1)], vec![0.0; delay.max(1)]],
            pos: 0,
            echo: [vec![0.0; echo.max(1)], vec![0.0; echo.max(1)]],
            epos: 0,
            held: 0,
            pedal: 0,
            sustained: [0; 16],
        }
    }
}

fn answer_ping(r: &Region) {
    let p = r.ping().load(Ordering::Acquire);
    if p != 0 && r.pong().load(Ordering::Relaxed) != p {
        r.pong().store(p, Ordering::Release);
    }
}

/// T17's mutation `--mutant-hard-switch`: the bypass signal wherever the envelope has left y, y elsewhere: no fade.
fn hard_switch(mix: &handover::Mix, out: &mut [f32], b: &[f32]) {
    for (i, (x, &bv)) in out.iter_mut().zip(b).enumerate() {
        if mix.pass || mix.at(i).0 > 0 {
            *x = bv;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn chain_thread(
    r: Arc<Region>,
    page: Arc<ChildPage>,
    parked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    plugins: Vec<Arc<Plugin>>,
    mut lines: Vec<Option<DelayLine>>,
    tc_us: u64,
    sample_rate: f64,
    stats: bool,
    (hard, undelayed): (bool, bool),
    render: bool,
    hist_modelled: bool,
) -> Vec<u64> {
    if tc_us > 0 {
        let p = Duration::from_micros(tc_us);
        runecraft_ipc::bridge::set_time_constraint(p, p / 2, p);
    }
    let geo = *r.geometry();
    let mf = geo.max_frames as usize;
    let fade = Fade::default();
    let mut sides = vec![ChainSide::default(); plugins.len()];
    let mut buf = [vec![0.0f32; mf], vec![0.0f32; mf]];
    // D22: the bypass signal of the slot being processed.
    let mut byp = [vec![0.0f32; mf], vec![0.0f32; mf]];
    let mut lat: Vec<u64> = Vec::with_capacity(if stats { 1 << 20 } else { 0 });
    let mut done = r.ack().load(Ordering::Acquire);
    let mut crash = vec![false; plugins.len()];
    let mut wedge = vec![false; plugins.len()];
    let mut burn = vec![0u32; plugins.len()];
    // parameter 4 (>= 0.5) acks the block with ST_SKIPPED and silence, as a chain held by
    // prepare; parameter 5 with ST_THREW and silence, as a plugin that threw.
    let mut skip = false;
    let mut threw = false;
    let mut midi: Vec<(usize, [u8; 3])> = Vec::with_capacity(geo.max_midi as usize);
    let mut woke = false;
    loop {
        // C9: group, then its hub, then seq, every iteration.
        let g = r.group().load(Ordering::Acquire) as usize;
        let h = page.hub(g).load(Ordering::Acquire);
        let s = r.seq().load(Ordering::Acquire);
        answer_ping(&r);
        if stop.load(Ordering::SeqCst) {
            return lat;
        }
        if s > done {
            if woke && stats {
                // Wake-to-run against the group's ring stamp (the hub line's diagnostic word).
                let t = runecraft_ipc::bridge::host_ticks();
                let stamp = page
                    .word64(P_HUB[g.min(3)] + P_RING_STAMP)
                    .load(Ordering::Relaxed);
                if stamp != 0 && t > stamp && lat.len() + 1 < lat.capacity() {
                    lat.push(h);
                    lat.push(t - stamp);
                }
            }
            woke = false;
            for n in done + 1..=s {
                let t0 = Instant::now();
                // the burn this block applied (`--hist-modelled`'s cost).
                let mut modelled_ns = 0u64;
                let slot = r.slot(n);
                let frames = (slot.u32(S_FRAMES).load(Ordering::Relaxed) as usize).min(mf);
                // host sets (`param.set`), applied before the block, then the region's (D10).
                for p in plugins.iter() {
                    for (i, x) in p.params.iter().enumerate() {
                        if x.dirty.swap(false, Ordering::AcqRel) {
                            let v = x.pending.load(Ordering::Relaxed);
                            if i == 0 {
                                p.gain.store(v, Ordering::Relaxed);
                            } else {
                                x.applied.store(v, Ordering::Relaxed);
                            }
                        }
                    }
                }
                // Parameters (D10).
                let np = slot
                    .u32(S_PARAM_COUNT)
                    .load(Ordering::Relaxed)
                    .min(geo.max_params);
                for i in 0..np as usize {
                    let rec = slot.param(i);
                    let id = rec[0].load(Ordering::Relaxed);
                    let v = f32::from_bits(rec[1].load(Ordering::Relaxed));
                    let (k, idx) = ((id >> 24) as usize, id & 0xff_ffff);
                    if let Some(p) = plugins.get(k) {
                        match idx {
                            0 => p.gain.store(v.to_bits(), Ordering::Relaxed),
                            1 => crash[k] = v >= 0.5,
                            2 => wedge[k] = v >= 0.5,
                            3 => burn[k] = (v.clamp(0.0, 1.0) * 100_000.0) as u32,
                            4 => skip = v >= 0.5,
                            5 => threw = v >= 0.5,
                            _ => {}
                        }
                    }
                }
                // `skip-after`: an instance past its block n skips every block.
                for p in plugins.iter() {
                    let seen = p.blocks_seen.fetch_add(1, Ordering::Relaxed);
                    if p.skip_after.is_some_and(|n| seen >= n) {
                        skip = true;
                    }
                }
                let flags = slot.u32(S_BLOCK_FLAGS).load(Ordering::Relaxed);
                for (c, b) in buf.iter_mut().enumerate() {
                    for (i, x) in b.iter_mut().enumerate().take(frames) {
                        *x = if (c as u32) < geo.ch_in {
                            f32::from_bits(slot.input(c)[i].load(Ordering::Relaxed))
                        } else {
                            0.0
                        };
                    }
                }
                let nm = slot
                    .u32(S_MIDI_COUNT)
                    .load(Ordering::Relaxed)
                    .min(geo.max_midi) as usize;
                midi.clear();
                for i in 0..nm {
                    let rec = slot.midi(i);
                    let off =
                        (rec[0].load(Ordering::Relaxed) as usize).min(frames.saturating_sub(1));
                    let b = rec[2].load(Ordering::Relaxed).to_le_bytes();
                    midi.push((off, [b[0], b[1], b[2]]));
                }
                midi.sort_by_key(|m| m.0);
                let mut status = ST_PROCESSED;
                for (k, p) in plugins.iter().enumerate() {
                    // A quarantined slot has no processor: it passes its audio through unchanged, and never sets
                    // ST_BYPASSED (D7, D22).
                    if p.quarantined {
                        continue;
                    }
                    // D22: the word decides whether the plugin runs and how its output mixes with the bypass signal.
                    let mut begun = sides[k].begin(&p.cell);
                    // T17's probe: inside the first bypassed block after the main thread's cycle is done, wait for its
                    // give-back and count, so this block's end and the return fade's land after the count (bounded:
                    // 2 s, then go on). Off (one load) unless `LAGPROBE` armed it.
                    if p.lag_probe.load(Ordering::SeqCst) != 0
                        && p.probe_ready.load(Ordering::SeqCst)
                        && begun.guard.is_none()
                        && begun.mix.bypassed()
                        && !p.probe_in_block.swap(true, Ordering::SeqCst)
                    {
                        let until = Instant::now() + Duration::from_secs(2);
                        while !p.probe_counted.load(Ordering::SeqCst) && Instant::now() < until {
                            std::thread::sleep(Duration::from_micros(50));
                        }
                    }
                    // The bypass signal: an effect's input delayed by its applied latency L, written on every block
                    // before the plugin is called; an instrument's silence.
                    match &mut lines[k] {
                        Some(line) => {
                            line.write(0, &buf[0][..frames]);
                            line.write(1, &buf[1][..frames]);
                            let l = p.latency.load(Ordering::Relaxed) as usize;
                            let aligned = line.read(0, l, &mut byp[0][..frames]);
                            line.read(1, l, &mut byp[1][..frames]);
                            line.advance(frames);
                            if undelayed {
                                for c in 0..2 {
                                    byp[c][..frames].copy_from_slice(&buf[c][..frames]);
                                }
                            }
                            if !aligned && begun.mix.bypassed() {
                                p.misaligned.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        None => {
                            for b in byp.iter_mut() {
                                b[..frames].fill(0.0);
                            }
                        }
                    }
                    if let Some(d) = begun.guard.as_deref_mut() {
                        r.in_slot().store(k as u32 + 1, Ordering::Release);
                        if crash[k] {
                            die();
                        }
                        if wedge[k] {
                            // Hung inside processBlock: never acks, never answers the ping (C1). It sleeps rather
                            // than spins: the same hang for the host application, no core burnt while the session runs.
                            loop {
                                std::thread::sleep(Duration::from_secs(1));
                            }
                        }
                        let once = p.block_once_ms.swap(0, Ordering::Relaxed);
                        if once > 0 {
                            // A slow block that holds the cell.
                            std::thread::sleep(Duration::from_millis(once));
                        }
                        if burn[k] > 0 {
                            modelled_ns += u64::from(burn[k]) * 1_000;
                            let until = Instant::now() + Duration::from_micros(burn[k] as u64);
                            while Instant::now() < until {
                                std::hint::spin_loop();
                            }
                        }
                        let mut gain = f32::from_bits(p.gain.load(Ordering::Relaxed));
                        // `firstuse`: the instance's first block of its life.
                        if let Some(g0) = p.first_use
                            && !p.used.swap(true, Ordering::Relaxed)
                        {
                            gain = g0;
                        }
                        if flags & BF_RESET != 0 {
                            d.held = 0;
                            d.pedal = 0;
                            d.sustained = [0; 16];
                        }
                        summoner_plugin::channels::channel_rule_before(
                            p.io.0, p.io.1, &mut buf, frames,
                        );
                        let mut mi = 0;
                        for i in 0..frames {
                            if p.synth {
                                while mi < midi.len() && midi[mi].0 <= i {
                                    let e = midi[mi].1;
                                    let ch = (e[0] & 0x0f) as usize;
                                    match e[0] & 0xf0 {
                                        0x90 if e[2] > 0 => d.held += 1,
                                        0x80 | 0x90 if p.sustain && d.pedal >> ch & 1 != 0 => {
                                            d.sustained[ch] += 1
                                        }
                                        0x80 | 0x90 => d.held = d.held.saturating_sub(1),
                                        0xb0 if (e[1] == 123 || e[1] == 120) && !p.ignore_cc123 => {
                                            d.held = 0;
                                            d.sustained = [0; 16];
                                        }
                                        0xb0 if e[1] == 64 && p.sustain => {
                                            if e[2] >= 64 {
                                                d.pedal |= 1 << ch;
                                            } else {
                                                d.pedal &= !(1 << ch);
                                                d.held = d.held.saturating_sub(d.sustained[ch]);
                                                d.sustained[ch] = 0;
                                            }
                                        }
                                        _ => {}
                                    }
                                    mi += 1;
                                }
                                let v = if d.held > 0 || p.drone { 0.25 } else { 0.0 };
                                buf[0][i] = v;
                                buf[1][i] = v;
                            }
                            for (c, b) in buf.iter_mut().enumerate() {
                                let mut x = b[i] * gain;
                                if p.echo > 0 {
                                    let mut y = p.feedback.mul_add(d.echo[c][d.epos], x);
                                    if y.abs() < 1e-30 {
                                        y = 0.0;
                                    }
                                    d.echo[c][d.epos] = y;
                                    x = y;
                                }
                                b[i] = if d.delay > 0 {
                                    let y = d.line[c][d.pos];
                                    d.line[c][d.pos] = x;
                                    y
                                } else {
                                    x
                                };
                            }
                            if d.delay > 0 {
                                d.pos = (d.pos + 1) % d.delay;
                            }
                            if p.echo > 0 {
                                d.epos = (d.epos + 1) % p.echo;
                            }
                        }
                        summoner_plugin::channels::channel_rule_after(
                            p.io.0, p.io.1, &mut buf, frames,
                        );
                        r.in_slot().store(0, Ordering::Release);
                    }
                    for c in 0..2 {
                        if hard {
                            hard_switch(&begun.mix, &mut buf[c][..frames], &byp[c][..frames]);
                        } else {
                            begun
                                .mix
                                .apply(&fade, &mut buf[c][..frames], &byp[c][..frames]);
                        }
                    }
                    // A disabled slot's output is its bypass signal (its input delayed by its latency; silence for
                    // an instrument).
                    if !p.enabled.load(Ordering::Acquire) && !p.ignore_bypass {
                        for c in 0..2 {
                            buf[c][..frames].copy_from_slice(&byp[c][..frames]);
                        }
                    }
                    // The processor is released before the chain thread's CAS (FADING -> OUT, RETURNING -> HOSTED).
                    if sides[k].end(&p.cell, frames, begun) {
                        status |= ST_BYPASSED;
                        // `LAGPROBE=3`, the mutant: one bypassed block goes uncounted (one past the window).
                        if !p.probe_drop_one.swap(false, Ordering::Relaxed) {
                            p.bypassed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                if plugins.iter().any(|p| p.probe) {
                    let f64at = |o: usize| f64::from_bits(slot.u64(o).load(Ordering::Relaxed));
                    let ts = slot.u32(S_TIME_SIG).load(Ordering::Relaxed);
                    let probe = [
                        f64at(S_PPQ),
                        f64at(S_BPM),
                        f64::from(ts & 0xffff),
                        f64::from(ts >> 16),
                        f64at(S_LOOP_START_PPQ),
                        f64at(S_LOOP_END_PPQ),
                        f64at(S_BAR_START_PPQ),
                        f64::from(flags),
                        f64at(S_TIME_SECONDS),
                        slot.u64(S_TIME_SAMPLES).load(Ordering::Relaxed) as f64,
                        slot.u64(S_OUTPUT_HOST_TIME).load(Ordering::Relaxed) as f64,
                    ];
                    for (c, b) in buf.iter_mut().enumerate() {
                        for (i, x) in b.iter_mut().enumerate().take(frames) {
                            *x = if c == 0 && i < probe.len() {
                                probe[i] as f32
                            } else {
                                0.0
                            };
                        }
                    }
                }
                if skip || threw {
                    for b in buf.iter_mut() {
                        b[..frames].fill(0.0);
                    }
                    status = if skip {
                        ST_SKIPPED
                    } else {
                        ST_PROCESSED | ST_THREW
                    };
                }
                for c in 0..geo.ch_out as usize {
                    let out = slot.output(c);
                    for (i, o) in out.iter().enumerate().take(frames) {
                        o.store(buf[c.min(1)][i].to_bits(), Ordering::Relaxed);
                    }
                }
                let ns = if hist_modelled {
                    modelled_ns
                } else {
                    t0.elapsed().as_nanos() as u64
                };
                let period_ns = (frames.max(1) as f64 / sample_rate * 1e9) as u64;
                let bucket = ((ns * 20) / period_ns.max(1)).min(15) as usize;
                let hist = &r.hist()[bucket];
                hist.store(hist.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
                slot.u64(R_ECHO).store(n, Ordering::Relaxed);
                slot.u32(R_STATUS).store(status, Ordering::Relaxed);
                // D23: +140 is reserved (3.0's latencyFrames): written 0, never read, never reused.
                slot.u32(R_RESERVED_LATENCY).store(0, Ordering::Relaxed);
                slot.u32(R_PROCESS_NS)
                    .store(ns.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                r.processed().fetch_add(1, Ordering::Relaxed);
                r.ack().store(n, Ordering::Release);
                if render {
                    // D9: the render worker waits on ack; it is woken after every block.
                    let _ = r.ack_waker().wake();
                }
                answer_ping(&r);
            }
            done = s;
            continue;
        }
        if s < done {
            done = s; // a reset seq: follow it
        }
        // D9: a render set waits on its own seq, never a hub (the worker and the supervisor's ping wake it).
        let waited = if render || parked.load(Ordering::SeqCst) {
            r.seq_waker()
                .wait(s, Some(Instant::now() + Duration::from_millis(250)))
        } else {
            page.hub_waker(g)
                .wait(h, Some(Instant::now() + Duration::from_millis(250)))
        };
        woke = waited == Waited::Woken;
        if woke && r.seq().load(Ordering::Acquire) == s {
            page.word64(P_SPURIOUS_WAKES)
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The scan mode (the module docs): `summoner_scan::scan`'s test double of host's scan child.
mod scan_mode {
    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Phase {
        Hello,
        Receipt,
        Scanned,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum End {
        Ok,
        Failed,
        Mismatch,
        ErrorReply,
        ScannedReply,
        Malformed,
        BadProtocol,
        MidFrame(Option<i32>),
        Exit(i32, Phase),
        Kill(Phase),
        Hang(Phase),
        CrashLog(Phase),
    }

    struct Script {
        end: End,
        /// `form=auto`: answer through `codec::encode_scanned`, form B when offered.
        form_auto: bool,
        /// `classes=<n>`: n synthetic classes.
        classes: usize,
        /// `end=aucodes`: `failed {reason: auCodes}` with the classes as the good siblings.
        au_codes_failed: bool,
        stderr: usize,
        grandchild: bool,
        escapee: bool,
        escapee_socket: bool,
        oversize: bool,
        /// `flood=<n>:<bytes>`: (frames, blob bytes each).
        flood: Option<(usize, usize)>,
        /// `class=gain`: an `ok` describes the bundle as the lab gain's class (D15's vectors), named by the
        /// bundle's stem, so the host application's index keys it.
        class_gain: bool,
        /// `protocol=<u32>`: the `hello` reply's protocol in place of `ABI_VERSION` (a 3.4 scan child).
        protocol: Option<u32>,
        /// `kill-if=<text>`: a scan whose path contains it dies by SIGKILL after the receipt (a crash, a strike).
        kill_if: Option<String>,
        /// `sleep-ms=<n>` (at most 5000): after the receipt, before `scanned` (a slow probe, for a cancel).
        sleep_ms: u64,
    }

    fn usage(why: &str) -> ! {
        runecraft_rt::safe_eprintln!("fake-host --scan: {why}");
        std::process::exit(2);
    }

    fn phase(p: &str) -> Phase {
        match p {
            "hello" => Phase::Hello,
            "receipt" => Phase::Receipt,
            "scanned" => Phase::Scanned,
            _ => usage(&format!("unknown phase {p:?}")),
        }
    }

    fn script() -> Script {
        let mut sc = Script {
            end: End::Ok,
            form_auto: false,
            classes: 0,
            au_codes_failed: false,
            stderr: 0,
            grandchild: false,
            escapee: false,
            escapee_socket: false,
            oversize: false,
            flood: None,
            class_gain: false,
            kill_if: None,
            sleep_ms: 0,
            protocol: None,
        };
        let text = std::env::var(SCAN_SCRIPT_ENV).unwrap_or_default();
        for item in text.split(',').filter(|x| !x.is_empty()) {
            let (k, v) = item.split_once('=').unwrap_or((item, ""));
            match k {
                "stderr" => sc.stderr = v.parse().unwrap_or_else(|_| usage("stderr=<n>")),
                "grandchild" => sc.grandchild = v == "1",
                "escapee" => sc.escapee = v == "1",
                "escapee-socket" => sc.escapee_socket = v == "1",
                "oversize" => sc.oversize = v == "1",
                "class" if v == "gain" => sc.class_gain = true,
                "form" => {
                    sc.form_auto = match v {
                        "meta" => false,
                        "auto" => true,
                        _ => usage("form=meta|auto"),
                    }
                }
                "classes" => {
                    sc.classes = v
                        .parse()
                        .ok()
                        .filter(|n| (1..=20_000).contains(n))
                        .unwrap_or_else(|| usage("classes=<n>, n 1..=20000"));
                }
                "kill-if" if !v.is_empty() => sc.kill_if = Some(v.to_string()),
                "protocol" => {
                    sc.protocol = Some(v.parse().unwrap_or_else(|_| usage("protocol=<u32>")));
                }
                "sleep-ms" => {
                    sc.sleep_ms = v
                        .parse()
                        .ok()
                        .filter(|n| *n <= 5000)
                        .unwrap_or_else(|| usage("sleep-ms=<n>, n at most 5000"));
                }
                "flood" => {
                    let parsed = v
                        .split_once(':')
                        .and_then(|(n, b)| Some((n.parse().ok()?, b.parse().ok()?)))
                        .filter(|&(n, b): &(usize, usize)| {
                            (1..=16).contains(&n) && (1..=codec::MAX_BLOB).contains(&b)
                        });
                    sc.flood = Some(parsed.unwrap_or_else(|| {
                        usage("flood=<n>:<bytes>, n 1..=16, bytes 1..=MAX_BLOB")
                    }));
                }
                "end" if v == "aucodes" => {
                    sc.end = End::Failed;
                    sc.au_codes_failed = true;
                }
                "end" => {
                    sc.end = match v.split_once('@') {
                        None => match v {
                            "ok" => End::Ok,
                            "failed" => End::Failed,
                            "mismatch" => End::Mismatch,
                            "error-reply" => End::ErrorReply,
                            "scanned-reply" => End::ScannedReply,
                            "malformed" => End::Malformed,
                            "bad-protocol" => End::BadProtocol,
                            "midframe" => End::MidFrame(None),
                            _ => match v.strip_prefix("midframe-exit:") {
                                Some(n) => End::MidFrame(Some(
                                    n.parse().unwrap_or_else(|_| usage("midframe-exit:<n>")),
                                )),
                                None => usage(&format!("unknown end {v:?}")),
                            },
                        },
                        Some((act, p)) => {
                            let p = phase(p);
                            match act.split_once(':') {
                                Some(("exit", n)) => End::Exit(
                                    n.parse().unwrap_or_else(|_| usage("exit:<n>@<phase>")),
                                    p,
                                ),
                                None if act == "kill" => End::Kill(p),
                                None if act == "hang" => End::Hang(p),
                                None if act == "crashlog" => End::CrashLog(p),
                                _ => usage(&format!("unknown end {v:?}")),
                            }
                        }
                    }
                }
                _ => usage(&format!("unknown SUMMONER_FAKE_SCAN key {k:?}")),
            }
        }
        sc
    }

    fn put(sock: &UnixStream, frame: Vec<u8>) {
        let _ = (&*sock).write_all(&frame);
    }

    /// A frame with no blob, framed by hand in the D6 header layout, for a meta over `MAX_META` (which the codec's
    /// encoder asserts against).
    fn frame_unchecked(kind: u16, flags: u16, id: u64, meta: &[u8]) -> Vec<u8> {
        let len = u32::try_from(meta.len()).unwrap_or_else(|_| usage("a meta over 4 GiB"));
        let mut v = Vec::with_capacity(24 + meta.len());
        v.extend_from_slice(&(20 + len).to_le_bytes());
        v.extend_from_slice(&kind.to_le_bytes());
        v.extend_from_slice(&flags.to_le_bytes());
        v.extend_from_slice(&id.to_le_bytes());
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(meta);
        v
    }

    /// host's crash log, as its handler writes it [V host `crash.rs`]: only into an existing directory.
    fn crash_log(path: &str) -> ! {
        let report = format!(
            "{}\nSignal: SIGSEGV (11)\nPID: {}\nBacktrace:\n  0 fake frame\nplugin-op: \
             plugin.scan {path}\n",
            summoner_proto::host_names::CRASH_BANNER,
            std::process::id()
        );
        // (N3): the `--crash-log` folder when given, else host's fallback, `$HOME/Library/Logs`.
        let dir = CRASH_LOG.get().cloned().or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| std::path::Path::new(&h).join("Library/Logs"))
        });
        let written = dir.filter(|d| d.is_dir()).and_then(|d| {
            let f = d.join(summoner_proto::host_names::crash_log_name(
                std::process::id(),
            ));
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(f)
                .and_then(|mut f| f.write_all(report.as_bytes()))
                .ok()
        });
        if written.is_none() {
            runecraft_rt::safe_eprintln!("{report}[fake-host] crash log NOT written");
        }
        die()
    }

    /// The end, if it is due at `at`.
    fn act(end: &End, at: Phase, path: &str) {
        match end {
            End::Exit(n, p) if *p == at => {
                if *n == 75 {
                    runecraft_rt::safe_eprintln!(
                        "[fake-host] scan: step instantiate passed its 10000 ms deadline (exit 75)"
                    );
                }
                std::process::exit(*n)
            }
            End::Kill(p) if *p == at => die(),
            End::Hang(p) if *p == at => loop {
                std::thread::sleep(Duration::from_secs(1));
            },
            End::CrashLog(p) if *p == at => crash_log(path),
            _ => {}
        }
    }

    pub fn run(args: &[String], sock: UnixStream, parent: EntryParent) -> ! {
        if !LAB_DEATH_KILL.load(Ordering::Relaxed) {
            usage("the scan mode runs only with SUMMONER_LAB_DEATH=kill");
        }
        let parent = super::watched_parent(parent);
        let want = [
            "--scan".to_string(),
            "--parent-pid".into(),
            parent.pid().to_string(),
        ];
        // `--step-ms <n>` and `--crash-log <dir>`; `--log-dir <dir>`; each at most once, after the
        // scan pair (their values were checked in `main`, the log dir by the logger).
        let rest = args.get(4..).unwrap_or(&[]);
        let flags = ["--step-ms", "--crash-log", CHILD_LOG_DIR_FLAG];
        let tail_ok = rest.len() % 2 == 0
            && rest.chunks(2).all(|p| flags.contains(&p[0].as_str()))
            && flags
                .iter()
                .all(|f| rest.chunks(2).filter(|p| p[0] == *f).count() <= 1);
        if args.len() < 4 || args[1..4] != want || !tail_ok {
            usage(&format!(
                "argv must be exactly {want:?} [--step-ms <n>] [--crash-log <dir>] [--log-dir <dir>], got {:?}",
                &args[1..]
            ));
        }
        if let Some(p) = rest.chunks(2).find(|p| p[0] == CHILD_LOG_DIR_FLAG) {
            // Say what the supervisor passed (the scan child log test reads it from the captured stderr).
            runecraft_rt::safe_eprintln!(
                "fake-scan: log-dir {} {} {} salt {}",
                p[1],
                summoner_proto::log_vocab::HOST_VOCAB.env_levels,
                std::env::var(summoner_proto::log_vocab::HOST_VOCAB.env_levels).unwrap_or_default(),
                std::env::var(summoner_proto::log_vocab::HOST_VOCAB.env_salt)
                    .map_or(0, |v| v.len())
            );
        }
        let sc = script();
        // The parent watch: our parent gone means exit 3 (host's orphan exit).
        super::watch_parent(parent);
        let mut dec = codec::Decoder::new(Direction::ToChild);
        let mut buf = vec![0u8; 64 * 1024];
        let mut fds: Vec<OwnedFd> = Vec::new();
        loop {
            let n = match runecraft_ipc::uds::recv_with_fds(&sock, &mut buf, &mut fds) {
                Ok(0) | Err(_) => std::process::exit(0),
                Ok(n) => n,
            };
            if !fds.is_empty() {
                runecraft_rt::safe_eprintln!("fake-host --scan: descriptors on a scan hello");
                std::process::exit(4);
            }
            let mut frames: Vec<Frame> = Vec::new();
            if let Err(e) = dec.feed(&buf[..n], &mut |f| frames.push(f)) {
                runecraft_rt::safe_eprintln!("fake-host --scan: protocol error: {e}");
                std::process::exit(4);
            }
            for f in frames {
                let (k, id) = (f.header.kind, f.header.id);
                match k {
                    kind::HELLO => {
                        let m: codec::Hello = f.meta_as().unwrap_or_else(|e| {
                            runecraft_rt::safe_eprintln!("fake-host --scan: hello: {e}");
                            std::process::exit(4)
                        });
                        if m.fds != 0 {
                            runecraft_rt::safe_eprintln!(
                                "fake-host --scan: hello says fds {}",
                                m.fds
                            );
                            std::process::exit(4);
                        }
                        act(&sc.end, Phase::Hello, "");
                        let protocol = if sc.end == End::BadProtocol {
                            2 << 16
                        } else {
                            sc.protocol.unwrap_or(ABI_VERSION)
                        };
                        let r = {
                            let mut msg = codec::HelloReply::default();
                            msg.pid = std::process::id();
                            msg.protocol = protocol;
                            msg.child_id = m.child_id;
                            msg.host = "fake".into();
                            msg.formats = vec!["VST3".into(), "AudioUnit".into()];
                            msg.mach_slot0 = false;
                            msg.wired_bytes = 0;
                            msg.unwired_bytes = 0;
                            msg
                        };
                        put(&sock, codec::encode_msg(k, FF_REPLY, id, &r, &[]));
                    }
                    kind::SCAN => {
                        let s: codec::Scan = match f.meta_as() {
                            Ok(s) => s,
                            Err(e) => {
                                let er = codec::ErrorReply::new(-32602, format!("not a scan: {e}"));
                                put(
                                    &sock,
                                    codec::encode_msg(k, FF_REPLY | FF_ERROR, id, &er, &[]),
                                );
                                continue;
                            }
                        };
                        if sc.end == End::ErrorReply {
                            let er = codec::ErrorReply::new(-32602, "not a scan (scripted)".into());
                            put(
                                &sock,
                                codec::encode_msg(k, FF_REPLY | FF_ERROR, id, &er, &[]),
                            );
                            continue;
                        }
                        put(&sock, codec::encode_msg(k, FF_REPLY, id, &empty(), &[]));
                        runecraft_rt::safe_eprintln!("fake-scan: probing {}", s.path());
                        if sc.kill_if.as_deref().is_some_and(|k| s.path().contains(k)) {
                            runecraft_rt::safe_eprintln!("fake-scan: kill-if matched {}", s.path());
                            die();
                        }
                        if sc.sleep_ms > 0 {
                            std::thread::sleep(Duration::from_millis(sc.sleep_ms));
                        }
                        if sc.stderr > 0 {
                            let line = [b'e'; 63];
                            let mut err = std::io::stderr().lock();
                            let mut left = sc.stderr;
                            while left > 0 {
                                let n = left.min(64);
                                let _ = err.write_all(&line[..n - 1]);
                                let _ = err.write_all(b"\n");
                                left -= n;
                            }
                            let _ = err.write_all(b"fake-scan: stderr end\n");
                        }
                        if sc.grandchild {
                            match std::process::Command::new("/bin/sleep").arg("30").spawn() {
                                Ok(c) => runecraft_rt::safe_eprintln!(
                                    "fake-scan: grandchild {} pgid {}",
                                    c.id(),
                                    runecraft_ipc::process::process_group_of(c.id()).unwrap_or(0)
                                ),
                                Err(e) => {
                                    runecraft_rt::safe_eprintln!("fake-scan: no grandchild: {e}")
                                }
                            }
                        }
                        if sc.escapee {
                            let me = std::env::current_exe().unwrap_or_default();
                            let mut cmd = std::process::Command::new(me);
                            cmd.arg("--scan-escapee");
                            if sc.escapee_socket {
                                match sock.try_clone() {
                                    Ok(c) => {
                                        cmd.stdin(std::process::Stdio::from(OwnedFd::from(c)));
                                    }
                                    Err(e) => usage(&format!("escapee-socket: {e}")),
                                }
                            }
                            match cmd.spawn() {
                                Ok(c) => {
                                    runecraft_rt::safe_eprintln!("fake-scan: escapee {}", c.id());
                                    std::thread::sleep(Duration::from_millis(300));
                                }
                                Err(e) => {
                                    runecraft_rt::safe_eprintln!("fake-scan: no escapee: {e}")
                                }
                            }
                        }
                        act(&sc.end, Phase::Receipt, s.path());
                        if let Some((n, bytes)) = sc.flood {
                            let meta = codec::Scanned::new(
                                s.format(),
                                s.path().to_string(),
                                codec::ScannedOutcome::Ok,
                                vec![],
                            );
                            let frame = codec::encode_msg(
                                kind::SCANNED,
                                FF_EVENT,
                                0,
                                &meta,
                                &vec![0xa5; bytes],
                            );
                            runecraft_rt::safe_eprintln!(
                                "fake-scan: flooding {n} frames of {bytes} blob bytes"
                            );
                            for _ in 0..n {
                                let _ = (&sock).write_all(&frame);
                            }
                            runecraft_rt::safe_eprintln!("fake-scan: flood sent");
                        }
                        let failed = sc.end == End::Failed;
                        let scanned = codec::Scanned::new(
                            s.format(),
                            if sc.end == End::Mismatch {
                                "/elsewhere/another.vst3".into()
                            } else {
                                s.path().to_string()
                            },
                            if sc.au_codes_failed {
                                codec::ScannedOutcome::Failed {
                                    failure: codec::ScanFailure::Other {
                                        reason: "auCodes".into(),
                                    },
                                    detail: "fake-bad-entry: the type code is zero".into(),
                                }
                            } else if failed {
                                codec::ScannedOutcome::Failed {
                                    failure: codec::ScanFailure::Process,
                                    detail: "a fake failure".into(),
                                }
                            } else {
                                codec::ScannedOutcome::Ok
                            },
                            // A failed line's classes are form B's `discovered` (the encoder writes `[]` in form A).
                            if failed && !sc.form_auto {
                                vec![]
                            } else if sc.classes > 0 {
                                (0..sc.classes)
                                    .map(|i| {
                                        fake_vst3_class(
                                            s.path(),
                                            &format!("fake-scan-class-{i:05}"),
                                            &format!("{:032x}", i + 1),
                                            &[],
                                        )
                                    })
                                    .collect()
                            } else if sc.class_gain && s.format() == codec::ScanFormat::Au {
                                let stem = s
                                    .path()
                                    .rsplit('/')
                                    .next()
                                    .and_then(|n| n.strip_suffix(".component"))
                                    .unwrap_or("fake")
                                    .to_string();
                                vec![codec::ScanClass::Au({
                                    let mut msg = summoner_proto::plugin::AuClass::default();
                                    msg.path = s.path().to_string();
                                    msg.bundle_mtime_ms = 0;
                                    msg.entry = Some(0);
                                    msg.au_codes = "61756678-534c676e-5361534c".into();
                                    msg.component_name = format!("Example Lab: {stem}");
                                    msg.au_version = Some(0x0001_0000);
                                    msg.inputs = Some(2);
                                    msg.outputs = Some(2);
                                    msg.refused = None;
                                    msg
                                })]
                            } else if sc.class_gain {
                                let stem = s
                                    .path()
                                    .rsplit('/')
                                    .next()
                                    .and_then(|n| n.strip_suffix(".vst3"))
                                    .unwrap_or("fake")
                                    .to_string();
                                let mut c = fake_vst3_class(
                                    s.path(),
                                    &stem,
                                    "5341534c4c4142010000005000000000",
                                    &["Fx"],
                                );
                                if let codec::ScanClass::Vst3(v) = &mut c {
                                    v.vendor = "Example Lab".into();
                                    v.version = "1.0.0".into();
                                }
                                vec![c]
                            } else {
                                // uniqueId 0x5ca7: the class id's last word (its three first are 0).
                                vec![fake_vst3_class(
                                    s.path(),
                                    "fake-scan",
                                    "00000000000000000000000000005ca7",
                                    &[],
                                )]
                            },
                        );
                        let (flags, sid) = if sc.end == End::ScannedReply {
                            (FF_REPLY, id)
                        } else {
                            (FF_EVENT, 0)
                        };
                        if let End::MidFrame(code) = sc.end {
                            let whole = codec::encode_msg(kind::SCANNED, flags, sid, &scanned, &[]);
                            put(&sock, whole[..whole.len() / 2].to_vec());
                            match code {
                                Some(n) => std::process::exit(n),
                                None => die(),
                            }
                        } else if sc.end == End::Malformed {
                            let meta = br#"{"format":"vst3","path":"#;
                            put(&sock, codec::encode(kind::SCANNED, flags, sid, meta, &[]));
                        } else if sc.oversize {
                            let mut scanned = scanned;
                            let one = fake_vst3_class(
                                s.path(),
                                "fake-scan-class-0000",
                                &format!("{:032x}", 1),
                                &[],
                            );
                            let each = serde_json::to_vec(&one).map_or(1, |v| v.len().max(1));
                            for i in 0..codec::MAX_META / each + 2 {
                                scanned.classes.push(fake_vst3_class(
                                    s.path(),
                                    &format!("fake-scan-class-{i:04}"),
                                    &format!("{:032x}", i + 1),
                                    &[],
                                ));
                            }
                            let meta = serde_json::to_vec(&scanned)
                                .unwrap_or_else(|e| usage(&format!("scanned: {e}")));
                            if meta.len() <= codec::MAX_META {
                                usage(&format!("oversize=1 built only {} bytes", meta.len()));
                            }
                            runecraft_rt::safe_eprintln!(
                                "fake-scan: oversized scanned {} bytes",
                                meta.len()
                            );
                            put(&sock, frame_unchecked(kind::SCANNED, flags, sid, &meta));
                        } else if sc.form_auto || sc.classes > 0 {
                            // As the plugin-host child sends it: the codec's encoder, form B when
                            // offered (and `form=auto`); past a bound its overLimit or header-only frame, never a
                            // panic.
                            let b = sc.form_auto && s.offers_descriptions_blob();
                            let f = codec::encode_scanned(&scanned, b);
                            runecraft_rt::safe_eprintln!(
                                "fake-scan: form {} scanned, {} classes",
                                if b { "B" } else { "A" },
                                scanned.classes.len()
                            );
                            put(&sock, f.bytes().to_vec());
                        } else {
                            put(
                                &sock,
                                codec::encode_msg(kind::SCANNED, flags, sid, &scanned, &[]),
                            );
                        }
                        act(&sc.end, Phase::Scanned, s.path());
                    }
                    kind::SHUTDOWN => {
                        put(&sock, codec::encode_msg(k, FF_REPLY, id, &empty(), &[]));
                        std::process::exit(0);
                    }
                    _ => {
                        let er = codec::ErrorReply::new(-32601, "not in scan mode".into());
                        put(
                            &sock,
                            codec::encode_msg(k, FF_REPLY | FF_ERROR, id, &er, &[]),
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod io_rule_tests {
    //! The fake's channel rule is `summoner_plugin::channels` (the one shared implementation); these tests keep
    //! their bodies through two local adapters taking the fake's `(inputs, outputs)` knob pair.
    fn stage_inputs<B: std::ops::DerefMut<Target = [f32]>>(
        (i, o): (u32, u32),
        bufs: &mut [B],
        frames: usize,
    ) {
        summoner_plugin::channels::channel_rule_before(i, o, bufs, frames);
    }
    fn finish_outputs<B: std::ops::DerefMut<Target = [f32]>>(
        (i, o): (u32, u32),
        bufs: &mut [B],
        frames: usize,
    ) {
        summoner_plugin::channels::channel_rule_after(i, o, bufs, frames);
    }

    /// The fake's D8 rule around an identity plugin on a 2-wide block of `(L, R) = (1, 3)`, every row worked by
    /// hand from the D8 rule (the staging before the call and the pop after it).
    #[test]
    fn the_channel_rule_is_the_childs_for_every_case() {
        let rows: [((u32, u32), (f32, f32)); 9] = [
            ((2, 2), (1.0, 3.0)), // a stereo plugin: untouched
            ((0, 1), (0.0, 0.0)), // in0/out1: zeros in, ch1 = ch0 out
            ((0, 2), (0.0, 0.0)), // a 0-input synth: zeros in
            ((1, 2), (1.0, 0.0)), // in1/out2: ch1 zeroed in, no average (P == 2)
            ((2, 1), (1.0, 0.0)), // in2/out1: ch1 ZERO out (P == 2, no copy)
            ((1, 1), (2.0, 2.0)), // in1/out1: the average in, ch1 = ch0 out
            ((2, 0), (0.0, 0.0)), // out0: silence
            ((1, 3), (2.0, 0.0)), // in1, three outputs: P == 3, the average in, ch1 zero in
            ((0, 0), (0.0, 0.0)), // nothing
        ];
        for (io, want) in rows {
            let mut b = [vec![1.0f32; 4], vec![3.0f32; 4]];
            stage_inputs(io, &mut b, 4);
            finish_outputs(io, &mut b, 4);
            assert!(b[0].iter().all(|&x| x == want.0), "{io:?} ch0 {:?}", b[0]);
            assert!(b[1].iter().all(|&x| x == want.1), "{io:?} ch1 {:?}", b[1]);
        }
    }

    /// Only the block's frames move: the tail past `frames` keeps its samples.
    #[test]
    fn the_rule_touches_only_the_blocks_frames() {
        let mut b = [vec![1.0f32; 4], vec![3.0f32; 4]];
        stage_inputs((0, 2), &mut b, 2);
        assert_eq!(b[0], [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(b[1], [0.0, 0.0, 3.0, 3.0]);
    }

    /// A clean-room pin (the slot channel rule's behaviour specification): the whole truth table, as data, for every
    /// bus shape in {0, 1, 2, 3, 4, 6, 8} each way. Columns: inputs, outputs, what the plugin sees on channels 0 and 1, what the
    /// next slot gets after a plugin that writes nothing, and after one that writes 5 to channel 0 and 7 to channel 1
    /// (each only when it has that output). Symbols: L and R the block's channels, A = (L + R) * 0.5 in f32, Z zero.
    const TABLE: [&str; 49] = [
        "0 0 ZZ ZZ ZZ",
        "0 1 ZZ ZZ 55",
        "0 2 ZZ ZZ 57",
        "0 3 ZZ ZZ 57",
        "0 4 ZZ ZZ 57",
        "0 6 ZZ ZZ 57",
        "0 8 ZZ ZZ 57",
        "1 0 AZ ZZ ZZ",
        "1 1 AZ AA 55",
        "1 2 LZ LZ 57",
        "1 3 AZ AZ 57",
        "1 4 AZ AZ 57",
        "1 6 AZ AZ 57",
        "1 8 AZ AZ 57",
        "2 0 LR ZZ ZZ",
        "2 1 LR LZ 5Z",
        "2 2 LR LR 57",
        "2 3 LR LR 57",
        "2 4 LR LR 57",
        "2 6 LR LR 57",
        "2 8 LR LR 57",
        "3 0 LR ZZ ZZ",
        "3 1 LR LZ 5Z",
        "3 2 LR LR 57",
        "3 3 LR LR 57",
        "3 4 LR LR 57",
        "3 6 LR LR 57",
        "3 8 LR LR 57",
        "4 0 LR ZZ ZZ",
        "4 1 LR LZ 5Z",
        "4 2 LR LR 57",
        "4 3 LR LR 57",
        "4 4 LR LR 57",
        "4 6 LR LR 57",
        "4 8 LR LR 57",
        "6 0 LR ZZ ZZ",
        "6 1 LR LZ 5Z",
        "6 2 LR LR 57",
        "6 3 LR LR 57",
        "6 4 LR LR 57",
        "6 6 LR LR 57",
        "6 8 LR LR 57",
        "8 0 LR ZZ ZZ",
        "8 1 LR LZ 5Z",
        "8 2 LR LR 57",
        "8 3 LR LR 57",
        "8 4 LR LR 57",
        "8 6 LR LR 57",
        "8 8 LR LR 57",
    ];

    fn sym(c: u8, l: f32, r: f32) -> f32 {
        match c {
            b'L' => l,
            b'R' => r,
            b'A' => (l + r) * 0.5,
            b'Z' => 0.0,
            b'5' => 5.0,
            b'7' => 7.0,
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_rule_is_the_specifications_truth_table() {
        let (l, r) = (0.75f32, -0.3f32);
        for row in TABLE {
            let f: Vec<&str> = row.split(' ').collect();
            let (i, o): (u32, u32) = (f[0].parse().unwrap(), f[1].parse().unwrap());
            let want = |k: usize| [sym(f[k].as_bytes()[0], l, r), sym(f[k].as_bytes()[1], l, r)];
            let mut b = [vec![l; 3], vec![r; 3]];
            stage_inputs((i, o), &mut b, 3);
            let seen = [b[0][0], b[1][0]];
            assert_eq!(
                seen.map(f32::to_bits),
                want(2).map(f32::to_bits),
                "{row}: seen"
            );
            let mut id = b.clone();
            finish_outputs((i, o), &mut id, 3);
            assert_eq!(
                [id[0][2], id[1][2]].map(f32::to_bits),
                want(3).map(f32::to_bits),
                "{row}: identity"
            );
            let mut w = b.clone();
            for (c, v) in [5.0f32, 7.0]
                .into_iter()
                .enumerate()
                .take(o.min(2) as usize)
            {
                w[c].fill(v);
            }
            finish_outputs((i, o), &mut w, 3);
            assert_eq!(
                [w[0][1], w[1][1]].map(f32::to_bits),
                want(4).map(f32::to_bits),
                "{row}: writer"
            );
        }
    }

    /// SplitMix64 (the specification's generator).
    fn splitmix(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A sample: mostly finite values of any exponent, plus the edges (both zeros, both infinities, subnormals); never
    /// a NaN (its payload is the platform's, so it stays out of a digest).
    fn sample(s: &mut u64) -> f32 {
        let v = splitmix(s);
        match v % 8 {
            0 => [
                0.0,
                -0.0,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::MIN_POSITIVE / 4.0,
                -f32::MIN_POSITIVE / 3.0,
                f32::MAX,
                -f32::MAX,
            ][(v >> 8) as usize % 8],
            1 => ((v >> 8) as u32 & 0x007F_FFFF) as f32 * f32::EPSILON,
            _ => {
                let x = f32::from_bits((v >> 16) as u32);
                if x.is_nan() { 1.5 } else { x }
            }
        }
    }

    /// 2048 generated blocks (seed 0x5EED_C018): shape from {0, 1, 2, 3, 4, 6, 8} each way, 6-frame channels, `frames`
    /// 0..=6, the rule before and after a plugin that writes nothing. The FNV-1a 64 digest covers every sample's bits
    /// of both channels after the before step and after the after step, the untouched tail included (a NaN as
    /// 0x7FC0_0000).
    #[test]
    fn the_seeded_digest_is_todays() {
        const SHAPES: [u32; 7] = [0, 1, 2, 3, 4, 6, 8];
        let mut s = 0x5EED_C018u64;
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        let eat = |h: &mut u64, b: &[Vec<f32>; 2]| {
            for x in b.iter().flatten() {
                // inf + -inf is a NaN whose bits are the platform's: one canonical NaN in the digest.
                let bits = if x.is_nan() { 0x7FC0_0000 } else { x.to_bits() };
                for byte in bits.to_le_bytes() {
                    *h ^= u64::from(byte);
                    *h = h.wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
        };
        for _ in 0..2048 {
            let i = SHAPES[(splitmix(&mut s) % 7) as usize];
            let o = SHAPES[(splitmix(&mut s) % 7) as usize];
            let frames = (splitmix(&mut s) % 7) as usize;
            let mut b = [
                (0..6).map(|_| sample(&mut s)).collect::<Vec<f32>>(),
                Vec::new(),
            ];
            b[1] = (0..6).map(|_| sample(&mut s)).collect();
            stage_inputs((i, o), &mut b, frames);
            eat(&mut h, &b);
            finish_outputs((i, o), &mut b, frames);
            eat(&mut h, &b);
        }
        assert_eq!(h, 0xAA93032B0F163152, "digest (seeded blocks)");
    }
}
