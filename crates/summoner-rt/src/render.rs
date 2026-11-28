//! The render worker: a render set's synchronous exchange, and the render set's RAM admission.
//!
//! A render set is its own `chain.load {role: render}` on its own region, whose header carries `FLAG_RENDER |
//! FLAG_ACK_WAKE` (the supervisor writes them from the role: `summoner::supervisor::region_flags`; the child refuses a
//! disagreement). Its chain thread waits on the region's own `seq`, never a hub, and wakes `ack`
//! after every block. The worker here is the other end:
//!
//! - **One block per `seq`**: stage the block (input, MIDI, parameters, the request line), publish `seq` (Release),
//!   `os_sync_wake_by_address_any(&seq)`, then wait on `ack` until the child has acked exactly this `seq` and its slot
//!   echoes it. There is never more than one block in flight, so the render's chain latency is the plugin latency
//!   alone (no pipeline depth, no FIFO).
//! - **The wait**: in [`RenderConfig::slice`] slices (250 ms), up to the offline block budget
//!   ([`RenderConfig::block_budget`]: 30 s, 250 ms..=30 s from a setting the application passes
//!   ([`RenderConfig::with_budget_setting`], [`parse_block_budget`]), whose larger values are clamped to 30 s
//!   here). The cancel flag is read before
//!   the publish and at every slice.
//! - **Any block that times out fails the render, retryable** ([`RenderError::TimedOut`]); a cancel ends it
//!   ([`RenderError::Cancelled`]). Either leaves a block in flight that this worker no longer owns, so the worker is
//!   spent: every later call is refused ([`RenderError::Spent`]) and the caller unloads the render set (it is
//!   unloaded after the job anyway). A render never "succeeds" with silence.
//! - **Untrusted peer**: the output is read only after the Acquire of `ack` and the echo check; a non-finite sample is
//!   written as 0 and counted in the block's outcome (the live path's rule), never passed on.
//!
//! The wedge ping of a render set is the supervisor's (`wedge_detector`: a render set is suspected after
//! `summoner::supervisor::Config::render_suspect`, which `summoner::supervisor::Config::new` sets to the same budget, so
//! the supervisor never judges a block the worker is still allowed to wait for; it pings by waking `seq`).
//!
//! The worker runs on a render thread (never the audio callback): it may block, it never allocates per block.
// A render thread, not the callback: it waits on the child (`wait_timeout`), wakes `seq` (`wake_one`) and reads the
// clock, all of which the crate's N1 list keeps out of `rt`.
#![allow(clippy::disallowed_methods)]

use crate::midi::Event3;
use crate::region::Region;
use crate::rt::{ChainLink, Transport};
use runecraft_ipc::bridge::sync::Waited;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use summoner_proto::layout::*;

/// The offline block budget by default: 30 s.
pub const BLOCK_BUDGET: Duration = Duration::from_secs(30);
/// The budget's floor and ceiling when the environment sets it (250 ms..=120 s).
pub const BLOCK_BUDGET_MIN: Duration = Duration::from_millis(250);
/// The ceiling the host application accepts: 30 s. Today's engine takes up to 120 s, but here the
/// render set lives in the live child: the supervisor's wedge kill of a render set ([`RENDER_SUSPECT`]) and the
/// child's reactivation horizon for render sets (`handover::TAKE_HORIZON_RENDER`, 33 s = 30 s + the 2 s verdict + a
/// tick) are both fixed at the render design's 30 s, so a longer block budget would let a legal block be abandoned by
/// the child or judged a wedge (killing the live chains) before the worker gives up. A value above it is clamped to it.
/// Raising it needs the child's horizon to move with it (a host-child change).
pub const BLOCK_BUDGET_MAX: Duration = Duration::from_secs(30);
/// The supervisor's render-set suspicion (the wedge detector for `role: render`): a fixed 30 s, never the
/// environment's (a variable that tunes a render's failure must not tune the kill of the live child).
/// At least the worker's budget ([`BLOCK_BUDGET_MAX`]), so the worker always fails a slow block (retryable, the set
/// unloaded) before the supervisor could judge it.
pub const RENDER_SUSPECT: Duration = Duration::from_secs(30);
const _: () = assert!(RENDER_SUSPECT.as_millis() >= BLOCK_BUDGET_MAX.as_millis());
/// The wait slice: the cancel flag is read at least this often.
pub const SLICE: Duration = Duration::from_millis(250);
/// The fixed render block.
pub const RENDER_MAX_FRAMES: u32 = 512;

/// The region flags of a render set: render, and the ack wake.
pub const RENDER_FLAGS: u32 = FLAG_RENDER | FLAG_ACK_WAKE;

/// The offline block budget from a setting in milliseconds (the application's, e.g. an environment variable): unset, empty, a
/// non-digit or 0 is the default; digits are clamped to [`BLOCK_BUDGET_MIN`]..=[`BLOCK_BUDGET_MAX`] (an overlong
/// number saturates, never wraps).
pub fn parse_block_budget(env: Option<&str>) -> Duration {
    let Some(v) = env.filter(|v| !v.is_empty()) else {
        return BLOCK_BUDGET;
    };
    let cap = BLOCK_BUDGET_MAX.as_millis() as u64 * 10;
    let mut ms: u64 = 0;
    for c in v.bytes() {
        if !c.is_ascii_digit() {
            return BLOCK_BUDGET;
        }
        ms = (ms * 10 + u64::from(c - b'0')).min(cap);
    }
    if ms == 0 {
        return BLOCK_BUDGET;
    }
    Duration::from_millis(ms).clamp(BLOCK_BUDGET_MIN, BLOCK_BUDGET_MAX)
}

/// A render worker's timing: how long one block may take, and how often the wait reads the cancel flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderConfig {
    /// The offline block budget: past it a block's wait fails the render ([`RenderError::TimedOut`]).
    pub block_budget: Duration,
    /// The wait slice: the cancel flag is read at least this often.
    pub slice: Duration,
}

impl RenderConfig {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(block_budget: Duration, slice: Duration) -> RenderConfig {
        RenderConfig {
            block_budget,
            slice,
        }
    }
}

impl RenderConfig {
    /// The budget from the application's setting ([`parse_block_budget`]; read once, at configuration, never per
    /// block) and [`SLICE`].
    pub fn with_budget_setting(setting: Option<&str>) -> RenderConfig {
        RenderConfig {
            block_budget: parse_block_budget(setting),
            slice: SLICE,
        }
    }
}

impl Default for RenderConfig {
    fn default() -> RenderConfig {
        RenderConfig {
            block_budget: BLOCK_BUDGET,
            slice: SLICE,
        }
    }
}

/// One block of a render, as the render's schedule gives it.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RenderBlock<'a> {
    /// Frames, 1..=the region's `maxFrames`.
    pub frames: usize,
    /// The chain's input (an FX chain's summed inputs; ignored for an instrument, or for a region with no inputs).
    /// Each at least `frames` long.
    pub input: [&'a [f32]; 2],
    /// `(offset in the block, bytes)`, in time order, at most the region's `maxMidi` (the caller splits a block that
    /// would carry more: nothing is carried or dropped here).
    pub midi: &'a [(u32, Event3)],
    /// `(parameter id, value 0..1)` changes for this block, at most the region's `maxParams` (as `midi`).
    pub params: &'a [(u32, f32)],
    /// The request line's transport ([`Transport::at`] of the render clock's position at the block's first frame).
    pub transport: Transport,
    /// `BF_RESET` on this block (all notes off and `reset()` before processing).
    pub reset: bool,
}

impl<'a> RenderBlock<'a> {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        frames: usize,
        input: [&'a [f32]; 2],
        midi: &'a [(u32, Event3)],
        params: &'a [(u32, f32)],
        transport: Transport,
        reset: bool,
    ) -> RenderBlock<'a> {
        RenderBlock {
            frames,
            input,
            midi,
            params,
            transport,
            reset,
        }
    }
}

/// What one acked block reported.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BlockOutcome {
    /// The block's `seq`.
    pub seq: u64,
    /// The response line's status word (`ST_*`), untrusted.
    pub status: u32,
    /// Samples the child returned non-finite (written as 0).
    pub non_finite: u32,
    /// Wait slices that ran out before the ack (0 when the child's ack wake arrived; each is a `slice` of waiting).
    pub slices_timed_out: u32,
}

impl BlockOutcome {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(seq: u64, status: u32, non_finite: u32, slices_timed_out: u32) -> BlockOutcome {
        BlockOutcome {
            seq,
            status,
            non_finite,
            slices_timed_out,
        }
    }
}

/// Why a render block failed, worded (built on control from a [`Fault`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RenderError {
    /// The link's region is not a render set's (its header does not carry [`RENDER_FLAGS`]).
    NotARenderSet {
        /// The region's flags.
        flags: u32,
    },
    /// A block the region cannot carry (frames, MIDI or parameters over the geometry, a buffer too short).
    Refused(String),
    /// The cancel flag was set (before the publish, or during the wait).
    Cancelled {
        /// The block's `seq`.
        seq: u64,
    },
    /// The block was not acked within the budget: the render fails, retryable.
    TimedOut {
        /// The block's `seq`.
        seq: u64,
        /// The budget it ran past.
        budget: Duration,
    },
    /// A previous block was cancelled or timed out: this worker owns no exchange any more.
    Spent,
    /// The child acked the block without processing it (`ST_SKIPPED`, or no `ST_PROCESSED`): the render fails,
    /// retryable.
    Skipped {
        /// The block's `seq`.
        seq: u64,
        /// The response line's status word.
        status: u32,
    },
    /// A plugin threw inside the block (`ST_THREW`): the render fails.
    Threw {
        /// The block's `seq`.
        seq: u64,
        /// The response line's status word.
        status: u32,
    },
    /// The OS refused the wait for the block's ack: the render fails, and this worker's exchange is spent.
    WaitFailed {
        /// The block's `seq`.
        seq: u64,
        /// The wait call's errno.
        errno: i32,
    },
}

impl RenderError {
    /// A timed-out block fails the render retryably.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            RenderError::TimedOut { .. } | RenderError::Skipped { .. }
        )
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderError::NotARenderSet { flags } => write!(
                f,
                "not a render set: the region's flags are {flags:#x}, a render set's are {RENDER_FLAGS:#x}"
            ),
            RenderError::Refused(why) => write!(f, "the render block was refused: {why}"),
            RenderError::Cancelled { seq } => write!(f, "the render was cancelled at block {seq}"),
            RenderError::TimedOut { seq, budget } => write!(
                f,
                "the plugin host did not finish render block {seq} within the offline block budget ({} ms); the \
                 render failed and can be retried",
                budget.as_millis()
            ),
            RenderError::Skipped { seq, status } => write!(
                f,
                "the plugin host skipped render block {seq} (status {status:#x}: the chain was not ready); the \
                 render failed and can be retried"
            ),
            RenderError::Threw { seq, status } => write!(
                f,
                "a plugin failed inside render block {seq} (status {status:#x}); the render failed"
            ),
            RenderError::WaitFailed { seq, errno } => write!(
                f,
                "the wait for render block {seq}'s ack failed (errno {errno}); the render failed"
            ),
            RenderError::Spent => write!(
                f,
                "the render set's exchange was abandoned by an earlier cancel or timeout"
            ),
        }
    }
}

impl std::error::Error for RenderError {}

/// Why a block failed, without allocating (the graph's render op runs inside the allocator guard): the codes of
/// [`RenderJob::fault`]. [`RenderError`] is its worded form, built on control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// The link's region is not a render set's.
    NotARenderSet {
        /// The region's flags.
        flags: u32,
    },
    /// The block's frames are outside the region's 1..=max.
    Frames {
        /// The block's frames.
        frames: u32,
        /// The region's `maxFrames`.
        max: u32,
    },
    /// The block carries more MIDI events than the region holds.
    Midi {
        /// The block's events.
        count: u32,
        /// The region's `maxMidi`.
        max: u32,
    },
    /// The block carries more parameter changes than the region holds.
    Params {
        /// The block's changes.
        count: u32,
        /// The region's `maxParams`.
        max: u32,
    },
    /// An input or output buffer is shorter than the block.
    ShortBuffer,
    /// The graph split the block across two visits (a mid-block switch): never in an offline render.
    Split,
    /// The cancel flag was set.
    Cancelled {
        /// The block's `seq`.
        seq: u64,
    },
    /// The block was not acked within the budget.
    TimedOut {
        /// The block's `seq`.
        seq: u64,
    },
    /// A previous block was cancelled or timed out: the exchange is abandoned.
    Spent,
    /// The child acked the block without processing it (`ST_SKIPPED`: prepare or release held the chain; or no
    /// `ST_PROCESSED`): its output is silence. Retryable.
    Skipped {
        /// The block's `seq`.
        seq: u64,
        /// The response line's status word.
        status: u32,
    },
    /// A plugin threw inside the block (`ST_THREW`): its output was zeroed.
    Threw {
        /// The block's `seq`.
        seq: u64,
        /// The response line's status word.
        status: u32,
    },
    /// The chain has more than two inputs (the worker stages a stereo pair; render sets are stereo).
    Inputs {
        /// The chain's input channels.
        ch_in: u32,
    },
    /// The OS refused the wait for the block's ack ([`Waited::Failed`]): the block is in flight, the exchange is
    /// spent.
    WaitFailed {
        /// The block's `seq`.
        seq: u64,
        /// The wait call's errno.
        errno: i32,
    },
}

impl Fault {
    fn worded(self, budget: Duration) -> RenderError {
        match self {
            Fault::NotARenderSet { flags } => RenderError::NotARenderSet { flags },
            Fault::Frames { frames, max } => {
                RenderError::Refused(format!("{frames} frames (the region carries 1..={max})"))
            }
            Fault::Midi { count, max } => RenderError::Refused(format!(
                "{count} MIDI events (the region carries {max} per block)"
            )),
            Fault::Params { count, max } => RenderError::Refused(format!(
                "{count} parameter changes (the region carries {max} per block)"
            )),
            Fault::ShortBuffer => RenderError::Refused("a buffer shorter than the block".into()),
            Fault::Split => RenderError::Refused(
                "the block was split across two visits (a schedule switch inside a render)".into(),
            ),
            Fault::Cancelled { seq } => RenderError::Cancelled { seq },
            Fault::TimedOut { seq } => RenderError::TimedOut { seq, budget },
            Fault::Spent => RenderError::Spent,
            Fault::Skipped { seq, status } => RenderError::Skipped { seq, status },
            Fault::Threw { seq, status } => RenderError::Threw { seq, status },
            Fault::Inputs { ch_in } => RenderError::Refused(format!(
                "a render set with {ch_in} inputs (the worker stages at most 2)"
            )),
            Fault::WaitFailed { seq, errno } => RenderError::WaitFailed { seq, errno },
        }
    }

    fn code(self) -> (u32, u64) {
        match self {
            Fault::NotARenderSet { flags } => (1, flags as u64),
            Fault::Frames { frames, max } => (2, (frames as u64) << 32 | max as u64),
            Fault::Midi { count, max } => (3, (count as u64) << 32 | max as u64),
            Fault::Params { count, max } => (4, (count as u64) << 32 | max as u64),
            Fault::ShortBuffer => (5, 0),
            Fault::Split => (6, 0),
            Fault::Cancelled { seq } => (7, seq),
            Fault::TimedOut { seq } => (8, seq),
            Fault::Spent => (9, 0),
            Fault::Skipped { seq, status } => (10, seq << 8 | (status & 0xff) as u64),
            Fault::Threw { seq, status } => (11, seq << 8 | (status & 0xff) as u64),
            Fault::Inputs { ch_in } => (12, ch_in as u64),
            Fault::WaitFailed { seq, errno } => (13 | (errno as u32) << 8, seq),
        }
    }

    fn from_code(code: u32, v: u64) -> Option<Fault> {
        let (hi, lo) = ((v >> 32) as u32, v as u32);
        Some(match code {
            1 => Fault::NotARenderSet { flags: v as u32 },
            2 => Fault::Frames {
                frames: hi,
                max: lo,
            },
            3 => Fault::Midi { count: hi, max: lo },
            4 => Fault::Params { count: hi, max: lo },
            5 => Fault::ShortBuffer,
            6 => Fault::Split,
            7 => Fault::Cancelled { seq: v },
            8 => Fault::TimedOut { seq: v },
            9 => Fault::Spent,
            10 => Fault::Skipped {
                seq: v >> 8,
                status: (v & 0xff) as u32,
            },
            11 => Fault::Threw {
                seq: v >> 8,
                status: (v & 0xff) as u32,
            },
            12 => Fault::Inputs { ch_in: v as u32 },
            c if c & 0xff == 13 => Fault::WaitFailed {
                seq: v,
                errno: (c >> 8) as i32,
            },
            _ => return None,
        })
    }
}

/// The exchange of one block (the module docs), the one implementation [`RenderWorker`] and [`RenderJob`] share. No
/// allocation, no lock; it waits on `ack` (the render thread's wait). `next_seq` advances only when the block is
/// published.
fn exchange(
    r: &Region,
    cfg: &RenderConfig,
    next_seq: &mut u64,
    b: &RenderBlock<'_>,
    out: [&mut [f32]; 2],
    cancel: &AtomicBool,
) -> Result<BlockOutcome, Fault> {
    let flags = r.flags();
    if flags != RENDER_FLAGS {
        return Err(Fault::NotARenderSet { flags });
    }
    let geo = *r.geometry();
    if geo.ch_in > 2 {
        return Err(Fault::Inputs { ch_in: geo.ch_in });
    }
    let n = b.frames;
    if n == 0 || n > geo.max_frames as usize {
        return Err(Fault::Frames {
            frames: n.min(u32::MAX as usize) as u32,
            max: geo.max_frames,
        });
    }
    if b.midi.len() > geo.max_midi as usize {
        return Err(Fault::Midi {
            count: b.midi.len().min(u32::MAX as usize) as u32,
            max: geo.max_midi,
        });
    }
    if b.params.len() > geo.max_params as usize {
        return Err(Fault::Params {
            count: b.params.len().min(u32::MAX as usize) as u32,
            max: geo.max_params,
        });
    }
    let [ol, or] = out;
    if ol.len() < n || or.len() < n || b.input.iter().any(|i| i.len() < n) {
        return Err(Fault::ShortBuffer);
    }
    let seq = *next_seq;
    if cancel.load(Ordering::Acquire) {
        return Err(Fault::Cancelled { seq });
    }
    let slot = r.slot(seq);
    for c in 0..(geo.ch_in as usize).min(2) {
        let dst = slot.input(c);
        for (d, s) in dst[..n].iter().zip(&b.input[c][..n]) {
            d.store(s.to_bits(), Ordering::Relaxed);
        }
    }
    for (i, &(off, e)) in b.midi.iter().enumerate() {
        let rec = slot.midi(i);
        rec[0].store(off.min(n as u32 - 1), Ordering::Relaxed);
        rec[1].store(3, Ordering::Relaxed);
        rec[2].store(u32::from_le_bytes([e[0], e[1], e[2], 0]), Ordering::Relaxed);
        rec[3].store(0, Ordering::Relaxed);
        rec[4].store(0, Ordering::Relaxed);
        rec[5].store(0, Ordering::Relaxed);
    }
    for (i, &(id, v)) in b.params.iter().enumerate() {
        let rec = slot.param(i);
        rec[0].store(id, Ordering::Relaxed);
        rec[1].store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
    // The request line, as `rt::ChainState::publish` writes it (the same fields, the same order); the edge is the
    // Release store of seq below.
    let t = &b.transport;
    let mut bflags = t.flags;
    if b.reset {
        bflags |= BF_RESET;
    }
    slot.u32(S_FRAMES).store(n as u32, Ordering::Relaxed);
    slot.u32(S_BLOCK_FLAGS).store(bflags, Ordering::Relaxed);
    slot.u32(S_MIDI_COUNT)
        .store(b.midi.len() as u32, Ordering::Relaxed);
    slot.u32(S_PARAM_COUNT)
        .store(b.params.len() as u32, Ordering::Relaxed);
    slot.u32(S_TIME_SIG).store(t.time_sig, Ordering::Relaxed);
    slot.u64(S_PPQ).store(t.ppq.to_bits(), Ordering::Relaxed);
    slot.u64(S_BPM).store(t.bpm.to_bits(), Ordering::Relaxed);
    slot.u64(S_TIME_SECONDS)
        .store(t.time_seconds.to_bits(), Ordering::Relaxed);
    slot.u64(S_TIME_SAMPLES)
        .store(t.time_samples, Ordering::Relaxed);
    slot.u64(S_LOOP_START_PPQ)
        .store(t.loop_start_ppq.to_bits(), Ordering::Relaxed);
    slot.u64(S_LOOP_END_PPQ)
        .store(t.loop_end_ppq.to_bits(), Ordering::Relaxed);
    slot.u64(S_BAR_START_PPQ)
        .store(t.bar_start_ppq.to_bits(), Ordering::Relaxed);
    slot.u64(S_OUTPUT_HOST_TIME)
        .store(t.output_host_time, Ordering::Relaxed);
    slot.u64(S_BLOCK_SEQ).store(seq, Ordering::Relaxed);
    // The child's render thread waits on seq. An error other than ENOENT is not fatal: the wait below still
    // bounds the block, and the supervisor's ping wakes seq again.
    let _ = r.seq_waker().ring(seq);
    *next_seq = seq + 1;
    let started = Instant::now();
    let mut slices_timed_out = 0u32;
    loop {
        let ack = r.ack().load(Ordering::Acquire);
        if ack >= seq && slot.u64(R_ECHO).load(Ordering::Relaxed) == seq {
            break;
        }
        if cancel.load(Ordering::Acquire) {
            return Err(Fault::Cancelled { seq });
        }
        let left = cfg.block_budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            return Err(Fault::TimedOut { seq });
        }
        let w = cfg.slice.min(left);
        match r.ack_waker().wait(ack, Some(Instant::now() + w)) {
            Waited::Woken => {}
            Waited::TimedOut => slices_timed_out += 1,
            // A wait the OS refuses would return at once every time (a spin to the budget): the block fails now,
            // and the exchange is spent, as for a timeout (the block is still in flight).
            Waited::Failed(e) => {
                return Err(Fault::WaitFailed {
                    seq,
                    errno: e.errno(),
                });
            }
            // A wait outcome this worker does not know: treated as a failure, never as a wake.
            _ => return Err(Fault::WaitFailed { seq, errno: 0 }),
        }
    }
    // A block the child acked without processing it, or in which a plugin threw, is silence: it fails
    // the render (never Ok with a silent hole). `--cfg mutant_render_status_ignored` (never in a normal build)
    // accepts it again; the host application's render-status tests fail.
    let status = slot.u32(R_STATUS).load(Ordering::Relaxed);
    if !cfg!(mutant_render_status_ignored) {
        if status & ST_THREW != 0 {
            return Err(Fault::Threw { seq, status });
        }
        if status & ST_SKIPPED != 0 || status & ST_PROCESSED == 0 {
            return Err(Fault::Skipped { seq, status });
        }
    }
    let mut non_finite = 0u32;
    let chans = (geo.ch_out as usize).clamp(1, 2);
    for (c, o) in [ol, or].into_iter().enumerate() {
        let src = slot.output(c.min(chans - 1));
        for (d, s) in o[..n].iter_mut().zip(&src[..n]) {
            let v = f32::from_bits(s.load(Ordering::Relaxed));
            if v.is_finite() {
                *d = v;
            } else {
                *d = 0.0;
                non_finite += 1;
            }
        }
    }
    Ok(BlockOutcome {
        seq,
        status: slot.u32(R_STATUS).load(Ordering::Relaxed),
        non_finite,
        slices_timed_out,
    })
}

/// Whether a fault leaves a block in flight that the exchange no longer owns (the exchange is then spent).
fn spends(f: Fault) -> bool {
    matches!(
        f,
        Fault::Cancelled { .. } | Fault::TimedOut { .. } | Fault::WaitFailed { .. }
    )
}

/// The render worker of one render set (the module docs). Owned by the render thread; never shared.
#[derive(Debug)]
pub struct RenderWorker {
    link: Arc<ChainLink>,
    cfg: RenderConfig,
    next_seq: u64,
    spent: bool,
}

impl RenderWorker {
    /// The worker of `link`'s render set. Refused unless the region's header carries exactly [`RENDER_FLAGS`].
    pub fn new(link: Arc<ChainLink>, cfg: RenderConfig) -> Result<RenderWorker, RenderError> {
        let flags = link.region.flags();
        if flags != RENDER_FLAGS {
            return Err(RenderError::NotARenderSet { flags });
        }
        // The worker is the only writer of seq: its own read is exact.
        let next_seq = link.region.seq().load(Ordering::Relaxed) + 1;
        Ok(RenderWorker {
            link,
            cfg,
            next_seq,
            spent: false,
        })
    }

    /// The chain link this worker renders through.
    pub fn link(&self) -> &Arc<ChainLink> {
        &self.link
    }

    /// The `seq` the next block is published as.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// One block, synchronously: stage, publish, wake, wait for its ack, read the output into `out` (each at least
    /// `b.frames` long; a mono chain's channel 0 goes to both).
    pub fn block(
        &mut self,
        b: &RenderBlock<'_>,
        out: [&mut [f32]; 2],
        cancel: &AtomicBool,
    ) -> Result<BlockOutcome, RenderError> {
        if self.spent {
            return Err(RenderError::Spent);
        }
        match exchange(
            &self.link.region,
            &self.cfg,
            &mut self.next_seq,
            b,
            out,
            cancel,
        ) {
            Ok(o) => Ok(o),
            Err(f) => {
                // A cancel before the publish leaves nothing in flight, but a cancelled render does not go on.
                if spends(f) {
                    self.spent = true;
                }
                Err(f.worded(self.cfg.block_budget))
            }
        }
    }
}

/// A render job's state for one render set, shared by the job (control side: cancel, the fault) and the graph's
/// render op (which runs each block through it, [`RenderJob::block`]): atomics only, so the op neither allocates nor
/// locks. Attached to the render set's [`ChainLink`] (`ChainLink::render`) by the job; a link without one is never
/// exchanged synchronously.
#[derive(Debug)]
pub struct RenderJob {
    cfg: RenderConfig,
    cancel: Arc<AtomicBool>,
    next_seq: AtomicU64,
    fault: AtomicU32,
    fault_value: AtomicU64,
    blocks: AtomicU64,
    slices_timed_out: AtomicU64,
    non_finite: AtomicU64,
    /// The value last sent per model parameter (f32 bits; NaN bits = never sent), so a block carries only changes.
    sent: Box<[AtomicU32]>,
}

impl RenderJob {
    /// A job for a render set with `params` model parameters, cancelled through `cancel` (the job's flag).
    pub fn new(cfg: RenderConfig, cancel: Arc<AtomicBool>, params: usize) -> RenderJob {
        RenderJob {
            cfg,
            cancel,
            next_seq: AtomicU64::new(0),
            fault: AtomicU32::new(0),
            fault_value: AtomicU64::new(0),
            blocks: AtomicU64::new(0),
            slices_timed_out: AtomicU64::new(0),
            non_finite: AtomicU64::new(0),
            sent: (0..params)
                .map(|_| AtomicU32::new(f32::NAN.to_bits()))
                .collect(),
        }
    }

    /// The cancel flag: set it (from any thread) to stop the render at its next read.
    pub fn cancel_flag(&self) -> &Arc<AtomicBool> {
        &self.cancel
    }

    /// The first fault of the job, worded (None: every block so far was acked).
    pub fn fault(&self) -> Option<RenderError> {
        let code = self.fault.load(Ordering::Acquire);
        Fault::from_code(code, self.fault_value.load(Ordering::Relaxed))
            .map(|f| f.worded(self.cfg.block_budget))
    }

    /// Blocks acked, wait slices that ran out, non-finite samples zeroed.
    pub fn stats(&self) -> (u64, u64, u64) {
        (
            self.blocks.load(Ordering::Relaxed),
            self.slices_timed_out.load(Ordering::Relaxed),
            self.non_finite.load(Ordering::Relaxed),
        )
    }

    /// The model parameter `i`'s value if it changed since it was last sent (and marks it sent).
    pub fn param_change(&self, i: usize, v: f32) -> Option<f32> {
        let s = self.sent.get(i)?;
        (s.swap(v.to_bits(), Ordering::Relaxed) != v.to_bits()).then_some(v)
    }

    /// Record a fault the caller found (the first one wins).
    pub fn fail(&self, f: Fault) {
        let (code, v) = f.code();
        if self
            .fault
            .compare_exchange(0, code, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            self.fault_value.store(v, Ordering::Release);
        }
    }

    /// One block of `link`'s render set (the graph's render op). After the first fault every block is refused
    /// (`Spent`): the caller writes silence and the job fails with [`RenderJob::fault`]. Single caller (the render
    /// thread); the atomics are for the job's reads on control.
    pub fn block(
        &self,
        link: &ChainLink,
        b: &RenderBlock<'_>,
        out: [&mut [f32]; 2],
    ) -> Result<BlockOutcome, Fault> {
        if self.fault.load(Ordering::Acquire) != 0 {
            return Err(Fault::Spent);
        }
        let mut next = self.next_seq.load(Ordering::Relaxed);
        if next == 0 {
            // The job is the only writer of seq: its own read is exact.
            next = link.region.seq().load(Ordering::Relaxed) + 1;
        }
        let r = exchange(&link.region, &self.cfg, &mut next, b, out, &self.cancel);
        self.next_seq.store(next, Ordering::Relaxed);
        match r {
            Ok(o) => {
                self.blocks.fetch_add(1, Ordering::Relaxed);
                self.slices_timed_out
                    .fetch_add(o.slices_timed_out as u64, Ordering::Relaxed);
                self.non_finite
                    .fetch_add(o.non_finite as u64, Ordering::Relaxed);
                Ok(o)
            }
            Err(f) => {
                self.fail(f);
                Err(f)
            }
        }
    }
}

/// The graph's render op for a Bridge node whose link carries a [`RenderJob`]: the whole block
/// in one synchronous exchange, in place in `io` (an FX chain's summed inputs in, the chain's output out; an
/// instrument's buffers arrive as silence). The node's MIDI for the block comes from `events` (the clock's, loop-aware,
/// as the live pipeline reads them; none while the clock is stopped) and its parameters are sent when they change (at
/// most the region's `maxParams` per block, the rest the next block, as the live rule).
///
/// No allocation and no lock (the callback runs inside the allocator guard): the input is copied to the stack (at
/// most [`RENDER_MAX_FRAMES`], the fixed render block), MIDI is gathered on the stack. It does wait on the child (the
/// render worker): only a render job's link has a job, and only an offline render compiles one. Any fault writes
/// silence and fails the job ([`RenderJob::fault`]): a render never succeeds with a silenced chain. A visit that is not
/// the whole block (a schedule switch inside a render) is such a fault. Returns true when the block was exchanged.
#[allow(clippy::too_many_arguments)]
pub fn render_visit(
    job: &RenderJob,
    link: &ChainLink,
    v: &crate::rt::Visit,
    io: [&mut [f32]; 2],
    events: &[crate::rt::MidiEvent],
    params: &[f64],
) -> bool {
    let [l, r] = io;
    let len = v.len.min(l.len()).min(r.len());
    let silence = |l: &mut [f32], r: &mut [f32]| {
        l[..len].fill(0.0);
        r[..len].fill(0.0);
    };
    if v.offset != 0 || len != v.total {
        job.fail(Fault::Split);
        silence(l, r);
        return false;
    }
    if len > RENDER_MAX_FRAMES as usize {
        job.fail(Fault::Frames {
            frames: len as u32,
            max: RENDER_MAX_FRAMES,
        });
        silence(l, r);
        return false;
    }
    let geo = *link.region.geometry();
    let mut inl = [0.0f32; RENDER_MAX_FRAMES as usize];
    let mut inr = [0.0f32; RENDER_MAX_FRAMES as usize];
    inl[..len].copy_from_slice(&l[..len]);
    inr[..len].copy_from_slice(&r[..len]);
    let mut midi = [(0u32, [0u8; 3]); MAX_MIDI as usize];
    let mut nm = 0usize;
    let mut over = 0usize;
    if v.playing {
        crate::rt::for_each_event_ranged(
            events,
            v.loop_start,
            v.loop_frames,
            v.pos,
            len,
            |at, e| {
                if nm < geo.max_midi as usize {
                    midi[nm] = (at as u32, e);
                    nm += 1;
                } else {
                    over += 1;
                }
            },
        );
    }
    if over > 0 {
        job.fail(Fault::Midi {
            count: (nm + over) as u32,
            max: geo.max_midi,
        });
        silence(l, r);
        return false;
    }
    let mut ps = [(0u32, 0.0f32); MAX_PARAMS as usize];
    let mut np = 0usize;
    for (i, &id) in link.param_ids.iter().enumerate() {
        if np >= (geo.max_params as usize).min(ps.len()) {
            break;
        }
        let val = params.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0) as f32;
        if let Some(x) = job.param_change(i, val) {
            ps[np] = (id, x);
            np += 1;
        }
    }
    let t = crate::rt::Transport::at_ranged(
        v.block_pos,
        v.loop_start,
        v.loop_frames,
        v.sample_rate,
        v.tempo,
        v.playing,
    );
    let b = RenderBlock {
        frames: len,
        input: [&inl[..len], &inr[..len]],
        midi: &midi[..nm],
        params: &ps[..np],
        transport: t,
        reset: false,
    };
    // `--cfg mutant_render_visit_skipped` (never in a normal build): the op passes the input through without
    // an exchange; the host application's render test fails (the chain's gain never applies, the child processes nothing).
    if cfg!(mutant_render_visit_skipped) {
        return false;
    }
    match job.block(link, &b, [&mut l[..len], &mut r[..len]]) {
        Ok(_) => true,
        Err(_) => {
            silence(l, r);
            false
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The render-set RAM admission
// ---------------------------------------------------------------------------------------------------------------------

/// The headroom the admission adds to the estimate: 512 MiB.
pub const RENDER_SET_EXTRA: u64 = 512 << 20;
/// What the admission keeps free for everything else: 2 GiB.
pub const RENDER_SET_RESERVE: u64 = 2 << 30;

/// The numbers the admission is judged on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderMemory {
    /// The live child's size, bytes: the larger of its resident size and its `phys_footprint`
    /// (compressed pages count).
    pub live_child_rss: u64,
    /// Chains the live child hosts now (live and parked; render sets excluded).
    pub live_chains: u32,
    /// Chains the render job would load.
    pub render_chains: u32,
    /// Physical memory the system could give without touching an active page: free, speculative, purgeable and every
    /// inactive page ([`runecraft_rt::os::available_memory`]) (the admission's "freePhysical").
    pub free_physical: u64,
}

impl RenderMemory {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        live_child_rss: u64,
        live_chains: u32,
        render_chains: u32,
        free_physical: u64,
    ) -> RenderMemory {
        RenderMemory {
            live_child_rss,
            live_chains,
            render_chains,
            free_physical,
        }
    }
}

/// The admission's refusal, worded for the job's error (the numbers in it).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RenderRefused {
    /// The bytes the render set needs.
    pub needed: u64,
    /// The bytes the formula found available.
    pub available: u64,
    /// The numbers the formula used.
    pub memory: RenderMemory,
}

impl RenderRefused {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(needed: u64, available: u64, memory: RenderMemory) -> RenderRefused {
        RenderRefused {
            needed,
            available,
            memory,
        }
    }
}

impl std::fmt::Display for RenderRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mib = |b: u64| b >> 20;
        let m = &self.memory;
        write!(
            f,
            "not enough memory for a separate render of the plugins: {} render chain(s) need about {} MiB (the plugin \
             host's {} MiB for {} live chain(s), scaled, plus {} MiB), and {} MiB is free after the {} MiB reserve; \
             free some memory, then render again",
            m.render_chains,
            mib(self.needed),
            mib(m.live_child_rss),
            m.live_chains,
            mib(RENDER_SET_EXTRA),
            mib(self.available),
            mib(RENDER_SET_RESERVE),
        )
    }
}

impl std::error::Error for RenderRefused {}

/// Refuse a render set when `liveChildRss × (renderChains / liveChains) + 512 MiB > freePhysical −
/// 2 GiB`. With no live chain the child's whole RSS is taken per render chain (the ratio is undefined there; this
/// is the cautious reading). Saturating integer arithmetic (no float rounding, no overflow).
pub fn admit_render_set(m: RenderMemory) -> Result<(), RenderRefused> {
    let per = m.live_child_rss / u64::from(m.live_chains.max(1));
    let rem = m.live_child_rss % u64::from(m.live_chains.max(1));
    // rss × render / live, rounded up, without overflow: (per × render) + ceil(rem × render / live).
    let live = u64::from(m.live_chains.max(1));
    let render = u64::from(m.render_chains);
    let scaled = per
        .saturating_mul(render)
        .saturating_add(rem.saturating_mul(render).div_ceil(live));
    let needed = scaled.saturating_add(RENDER_SET_EXTRA);
    let available = m.free_physical.saturating_sub(RENDER_SET_RESERVE);
    if needed > available {
        return Err(RenderRefused {
            needed,
            available,
            memory: m,
        });
    }
    Ok(())
}

/// [`admit_render_set`] on the system's numbers now: the child's RSS by pid and the available memory. A query that
/// fails refuses (stability first: an unmeasured render set is not loaded), worded.
pub fn admit_render_set_now(
    child_pid: u32,
    live_chains: u32,
    render_chains: u32,
) -> Result<RenderMemory, crate::Error> {
    admit_now(child_pid, live_chains, render_chains).map_err(crate::Error::new)
}

fn admit_now(child_pid: u32, live_chains: u32, render_chains: u32) -> Result<RenderMemory, String> {
    let rss = runecraft_rt::os::process_memory_of(child_pid)
        .ok_or_else(|| {
            format!("the plugin host's memory (pid {child_pid}) could not be read: no render set")
        })
        // The larger of resident and phys_footprint (compressed sample pools count).
        .map(|m| m.resident.max(m.footprint))?;
    let free = runecraft_rt::os::available_memory()
        .ok_or_else(|| "the system's free memory could not be read: no render set".to_string())?;
    let m = RenderMemory {
        live_child_rss: rss,
        live_chains,
        render_chains,
        free_physical: free,
    };
    admit_at(m, runecraft_rt::os::memory_pressure_level())
}

/// The kernel's memory-pressure levels (`kern.memorystatus_vm_pressure_level`, xnu's `DISPATCH_MEMORYPRESSURE_*`).
pub const PRESSURE_WARN: u32 = 2;
/// The kernel's CRITICAL memory-pressure level (a render set is refused at it).
pub const PRESSURE_CRITICAL: u32 = 4;

/// The admission with the second gate: the formula ([`admit_render_set`]), and a refusal
/// whenever the kernel reports CRITICAL memory pressure; WARN only logs. An unknown level, or none, is the formula
/// alone (logged when unknown). Worded as says.
pub fn admit_render_set_at(
    m: RenderMemory,
    pressure: Option<u32>,
) -> Result<RenderMemory, crate::Error> {
    admit_at(m, pressure).map_err(crate::Error::new)
}

fn admit_at(m: RenderMemory, pressure: Option<u32>) -> Result<RenderMemory, String> {
    match pressure {
        Some(PRESSURE_CRITICAL) => {
            return Err(
                "the system is under critical memory pressure, so the plugins cannot be rendered now; free some \
                 memory, then render again"
                    .into(),
            );
        }
        Some(PRESSURE_WARN) => runecraft_rt::safe_eprintln!(
            "summoner-rt: render admission: the system reports memory pressure WARN (admitted on its memory numbers)"
        ),
        Some(1) | None => {}
        Some(l) => {
            runecraft_rt::safe_eprintln!(
                "summoner-rt: render admission: an unknown memory-pressure level {l} (ignored)"
            )
        }
    }
    admit_render_set(m).map(|()| m).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_parses_as_todays_engine_does() {
        assert_eq!(parse_block_budget(None), BLOCK_BUDGET);
        assert_eq!(parse_block_budget(Some("")), BLOCK_BUDGET);
        assert_eq!(parse_block_budget(Some("0")), BLOCK_BUDGET);
        assert_eq!(parse_block_budget(Some("12a")), BLOCK_BUDGET);
        assert_eq!(parse_block_budget(Some("-5")), BLOCK_BUDGET);
        assert_eq!(parse_block_budget(Some("1")), BLOCK_BUDGET_MIN);
        assert_eq!(parse_block_budget(Some("249")), BLOCK_BUDGET_MIN);
        assert_eq!(
            parse_block_budget(Some("5000")),
            Duration::from_millis(5000)
        );
        assert_eq!(parse_block_budget(Some("120001")), BLOCK_BUDGET_MAX);
        assert_eq!(parse_block_budget(Some("60000")), Duration::from_secs(30));
        assert_eq!(
            parse_block_budget(Some("29999")),
            Duration::from_millis(29_999)
        );
        assert_eq!(
            parse_block_budget(Some("99999999999999999999999999")),
            BLOCK_BUDGET_MAX
        );
    }

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn the_ram_admission_is_d9s_formula() {
        // The large sampler's case: a 5.3 GB child on a 16 GB machine, one live chain, one render chain.
        let m = RenderMemory {
            live_child_rss: 5_300 * MIB,
            live_chains: 1,
            render_chains: 1,
            free_physical: 6 * GIB,
        };
        // 5300 + 512 = 5812 MiB needed > 6144 - 2048 = 4096 MiB.
        let e = admit_render_set(m).unwrap_err();
        assert_eq!(e.needed, 5_812 * MIB);
        assert_eq!(e.available, 4 * GIB);
        let text = e.to_string();
        assert!(
            text.contains("5812 MiB") && text.contains("4096 MiB"),
            "{text}"
        );
        assert!(
            text.ends_with("free some memory, then render again"),
            "{text}"
        );
        // Enough: 8 GiB free.
        assert!(
            admit_render_set(RenderMemory {
                free_physical: 8 * GIB,
                ..m
            })
            .is_ok()
        );
        // The boundary: needed == available is admitted, one byte more is refused.
        let at = RenderMemory {
            live_child_rss: GIB,
            live_chains: 4,
            render_chains: 2,
            free_physical: 2 * GIB + GIB / 2 + 512 * MIB,
        };
        assert!(admit_render_set(at).is_ok());
        assert!(
            admit_render_set(RenderMemory {
                free_physical: at.free_physical - 1,
                ..at
            })
            .is_err()
        );
        // Scaled by render / live, rounded up: 3 bytes over 2 chains for 1 render chain = 2 bytes.
        let odd = RenderMemory {
            live_child_rss: 3,
            live_chains: 2,
            render_chains: 1,
            free_physical: 2 * GIB + 512 * MIB + 2,
        };
        assert!(admit_render_set(odd).is_ok());
        assert!(
            admit_render_set(RenderMemory {
                free_physical: odd.free_physical - 1,
                ..odd
            })
            .is_err()
        );
        // No live chain: the whole RSS per render chain.
        let none = RenderMemory {
            live_child_rss: GIB,
            live_chains: 0,
            render_chains: 2,
            free_physical: 100 * GIB,
        };
        assert!(admit_render_set(none).is_ok());
        assert_eq!(
            admit_render_set(RenderMemory {
                free_physical: 0,
                ..none
            })
            .unwrap_err()
            .needed,
            2 * GIB + 512 * MIB
        );
        // Less free than the reserve: refused, never an underflow.
        assert_eq!(
            admit_render_set(RenderMemory {
                live_child_rss: 0,
                live_chains: 1,
                render_chains: 0,
                free_physical: GIB,
            })
            .unwrap_err()
            .available,
            0
        );
        // Saturation, not overflow.
        assert!(
            admit_render_set(RenderMemory {
                live_child_rss: u64::MAX,
                live_chains: 1,
                render_chains: u32::MAX,
                free_physical: u64::MAX,
            })
            .is_err()
        );
    }

    /// Critical pressure refuses whatever the numbers; warn and normal follow the formula.
    /// And the measured case, a Mac at 02:44: free 32,816 + speculative 34,398 + purgeable 303 + inactive
    /// 358,941 pages of 16 KiB = 6.51 GiB, admitted for a small child.
    #[test]
    fn the_pressure_gate_and_the_inactive_count() {
        let pages: u64 = 32_816 + 34_398 + 303 + 358_941;
        let m = RenderMemory {
            live_child_rss: 200 << 20,
            live_chains: 1,
            render_chains: 1,
            free_physical: pages * 16_384,
        };
        assert!(admit_render_set_at(m, None).is_ok());
        assert!(admit_render_set_at(m, Some(1)).is_ok());
        assert!(admit_render_set_at(m, Some(PRESSURE_WARN)).is_ok());
        let e = admit_render_set_at(m, Some(PRESSURE_CRITICAL)).unwrap_err();
        assert!(e.as_str().contains("critical memory pressure"), "{e}");
        assert!(
            e.as_str().ends_with("free some memory, then render again"),
            "{e}"
        );
        // The earlier lower bound on the same numbers (23,634 pages, 1.39 GiB) was under the reserve: refused.
        let old = RenderMemory {
            free_physical: (32_816 + 34_398 + 303 + 23_634) * 16_384,
            ..m
        };
        assert!(admit_render_set_at(old, None).is_err());
        // The formula still refuses when the numbers do not fit, at any pressure.
        let tight = RenderMemory {
            live_child_rss: 5_300 << 20,
            free_physical: 6 << 30,
            ..m
        };
        assert!(admit_render_set_at(tight, Some(1)).is_err());
    }

    #[test]
    fn the_system_numbers_are_read() {
        // This process stands in for the child: its own RSS is readable, and so is the available memory.
        let m = admit_render_set_now(std::process::id(), 1, 0);
        // The reads exist on macOS and Windows; on Linux they are not yet implemented (see the test below), so the
        // admission refuses there, worded.
        #[cfg(not(target_os = "linux"))]
        {
            match m {
                Ok(m) => {
                    assert!(m.live_child_rss > 0);
                    assert!(m.free_physical > 0);
                }
                // A loaded machine may refuse; the refusal must carry the admission's numbers.
                Err(e) => assert!(e.as_str().contains("MiB"), "{e}"),
            }
            assert!(runecraft_rt::os::process_memory_of(std::process::id()).is_some());
            assert!(runecraft_rt::os::available_memory().is_some_and(|b| b > 0));
        }
        #[cfg(target_os = "linux")]
        {
            let e = m.unwrap_err();
            assert!(e.as_str().contains("could not be read"), "{e}");
        }
        // A pid that is not ours to read (or does not exist) is refused, worded.
        let e = admit_render_set_now(u32::MAX >> 1, 1, 1).unwrap_err();
        assert!(e.as_str().contains("could not be read"), "{e}");
    }

    /// The documented gap: on Linux a process's memory and the system's available memory are not yet read, so
    /// every render set is refused there. This test flips (and goes) when the Linux reads land.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_system_numbers_are_not_yet_read_on_linux() {
        assert!(
            runecraft_rt::os::process_memory_of(std::process::id()).is_none(),
            "process_memory_of is implemented on Linux now: update the_system_numbers_are_read"
        );
        assert!(
            runecraft_rt::os::available_memory().is_none(),
            "available_memory is implemented on Linux now: update the_system_numbers_are_read"
        );
    }
}
