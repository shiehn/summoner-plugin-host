//! The child side of `restartComponent(kLatencyChanged)`: the two-phase handover of one hosted slot's
//! processor between its chain thread and the child's main thread, the bypass signal's delay line, and the fade.
//! Child-side code that lives here so the protocol has one source (a Rust host program uses it directly) and
//! so a loom model runs the shipped machine. The supervisor never runs it; the fake host
//! does.
//!
//! **The word.** Each hosted slot has one atomic state word with five values, moved only by compare-and-swap:
//! [`HOSTED`] → (main: announce) [`DRAINING`] → (chain: its next processed block) [`FADING`] → (chain: the fade done,
//! after it has released the processor) [`OUT`] → (main: the cycle done, the processor released) [`RETURNING`] →
//! (chain: the return fade done) [`HOSTED`]. Past its bound the main thread takes anyway: [`DRAINING`] → [`OUT`]
//! (a forced take, a stall).
//!
//! **Who touches the processor.** The chain thread reaches it only through the cell's `try_lock` (it never waits),
//! and only in [`HOSTED`], [`FADING`] after its own CAS, and [`RETURNING`]. A chain thread that reads [`DRAINING`]
//! CASes it to [`FADING`] **before** it touches the cell; if that CAS fails (the main thread forced the take), it
//! never touches the cell, holds no lock, and passes through. The
//! main thread locks the cell only in [`OUT`], with `try_lock` too, retrying in its own 1 ms polls: the one wait
//! that remains is the main thread's, after a forced take, on a chain thread still inside a block it began in
//! [`HOSTED`] (that block is late anyway; the host application's D5 handles it); no chain thread ever waits.
//! A chain thread's `try_lock` that misses in [`HOSTED`] (it read `HOSTED`, then
//! the main thread forced the take and took the cell) re-reads the word once and goes on from it: [`OUT`] is the
//! forced take's fade-in from silence, never a fault. `runecraft_rt::rt_lock`'s AcqRel / Acquire acquisition is what makes
//! that re-read see the force.
//!
//! **The output** of a slot is `x = y·(1 − g(xf)) + b·g(xf)·g(bin)`: `y` the plugin's output, `b` the bypass signal
//! (an effect's input delayed by its last applied latency L, through [`DelayLine`]; silence for an instrument), `g`
//! the raised cosine over F = 144 frames ([`FADE`], `declickFrames`), `xf` the crossfade position (0 = all y, F = all
//! b) and `bin` the bypass signal's own fade-in (F, except after a forced take, where it fades in from silence).
//! Where `xf` is 0 the output is `y` exactly, and where both are F it is `b` exactly (the alignment T17 asserts bit
//! for bit). Every sample-to-sample step obeys |Δx| ≤ max(|Δy|, |Δb|) + (π / (2F))·max|y − b| (D22's bound).
//!
//! N1 in the child: nothing on the chain thread's path here allocates, locks (it `try_lock`s) or makes a syscall.
//! [`DelayLine::new`] allocates, at `chain.prepare` (or load), on the main thread.
//!
//! **API for a child** (the plugin-host child and the fake host call exactly these). "Main" is the child's main thread
//! (its op loop and turns), "chain" a chain thread on its realtime path. Nothing on the chain side allocates, locks
//! (it `try_lock`s) or makes a syscall; the host program's tests run it under the allocation guard.
//!
//! | Item | Thread | Blocks | Allocates | Preconditions |
//! |---|---|---|---|---|
//! | `Cell::new(p)` | main | no | yes (and takes the mutex's lazy allocation) | one per hosted slot, at `chain.load` / `chain.prepare` |
//! | `Cell::state()` | either | no | no | none (Acquire load) |
//! | `Cell::announce()` | main | no | no | the restart flag is up; false unless the word is `HOSTED` (keep the flag) |
//! | `Cell::force()` | main | no | no | after `announce`, past `take_bound`; `Ok` only from `DRAINING` |
//! | `Cell::take()` | main | no (one `try_lock`) | no | after `announce`; `Ok` only in `OUT` |
//! | `Cell::try_take(past_bound)` | main | no | no | after `announce`; one per pending take per turn, and every take once `Turn::may_wait()` is false |
//! | `Cell::take_blocking(bound, poll, give_up)` | main | yes: polls, at most `give_up` (`TAKE_GIVE_UP`, 2 s) | no | after `announce`; only while `Turn::may_wait()`; on `Err`, `Turn::record_waits(1)` and the re-attempt path |
//! | `Cell::give_back(guard)` | main | no | no | the guard from a take, the cycle done; the word is `OUT` |
//! | `Turn::new / may_wait / record_waits / waits` | main | no | no | one `Turn` per main-loop turn; feed it every give-up and every `Attempt::waits` |
//! | `ChainSide::default()` | built anywhere, owned by one chain thread | no | no | one per hosted slot, never shared |
//! | `ChainSide::begin(cell)` | chain | never waits | no | once per processed block per hosted slot, then exactly one `end` |
//! | `ChainSide::end(cell, frames, begun)` | chain | no | no | after the plugin call (it releases the processor first); `frames` = the block's |
//! | `Begin.{guard, mix, fault, word}` | chain | no | no | call the plugin only if `guard` is `Some`; count `fault`; `word == OUT` implies no guard |
//! | `Mix::{at, bypassed, apply}`, `Mix::PLUGIN` | chain | no | no | `apply`: `out` holds y if the plugin ran; `b.len() >= out.len()`; set `ST_BYPASSED` when `end` returns true |
//! | `Fade::default()` / `Fade::g(n)` | build: main; `g`: chain | no | no | build once, before the chain thread runs |
//! | `DelayLine::new(ch, latency, max_frames)` | main | no | yes | one per hosted effect slot, at prepare; C = max(L, `DELAY_MIN_CAPACITY`) |
//! | `DelayLine::capacity()` | either | no | no | none |
//! | `DelayLine::{write, read, advance}` | chain | no | no | every block, before the plugin: `write` each channel, `read` each, then `advance(frames)`; `read` false = misaligned (count it) |
//! | `take_bound`, `cycle_budget`, `take_horizon(render)` | either | no | no | pure: the child's geometry and role |
//! | `TAKE_GIVE_UP`, `TAKE_HORIZON`, `TAKE_HORIZON_RENDER`, `FADE`, `DELAY_MIN_CAPACITY`, `K_LATENCY_CHANGED`, `HOSTED`, `DRAINING`, `FADING`, `OUT`, `RETURNING` | either | | | constants |
//!
//! **Pub, but not a child's contract:** `HandoverMutant` and `Cell::with_mutant` (`cfg(loom)` only, the model's
//! mutation set); `Attempt::waits` (always 0 in shipped code: pass it to `Turn::record_waits`, never branch on it);
//! `Taken::{contended, waited}` (diagnostics); `Cell::into_inner` (teardown and the loom model); `Mix`'s fields and
//! `state_name` (read them, log them; a `Mix` comes from `begin`, never built by a child).

use runecraft_rt::rt_lock::{WiredLock, WiredLockGuard};
use summoner_rt::sync::{AtomicU32, Ordering};

/// The state word: the slot is hosted (the chain thread calls the plugin).
pub const HOSTED: u32 = 0;
/// The state word: the main thread announced a take; the chain thread's next block starts the fade.
pub const DRAINING: u32 = 1;
/// The state word: the chain thread fades the plugin's output into the bypass signal.
pub const FADING: u32 = 2;
/// The state word: the processor is out of the chain thread's reach (the main thread may take it).
pub const OUT: u32 = 3;
/// The state word: the cycle is done; the chain thread fades the plugin back in.
pub const RETURNING: u32 = 4;
/// F: the fade's length in frames (`declickFrames`, 0003 D2; the same 144 as 0007 D2's ramps).
pub const FADE: u32 = 144;
/// The delay line's least capacity C, in frames: C = max(L at prepare, 16,384).
pub const DELAY_MIN_CAPACITY: usize = 16_384;
/// The main thread's hostile bound on a take (`Cell::take_blocking`'s `give_up` in the children): past it the take is
/// given up and the main thread serves its ops again. A chain thread that holds the cell this long is inside a block
/// that never ends, a wedge: the host application's wedge detector (D7, 2 s + 2 s) downs the chain and attributes the slot.
pub const TAKE_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a given-up take is re-attempted (non-blocking, once per main-loop turn) before it is abandoned,
/// from its announce. 5 s: past D7's worst case for a wedged chain (2 s suspect + 2 s verdict + a 250 ms tick =
/// 4.25 s; T11's bound 4.5 s), so a block that never ends has had the host application's wedge detector kill the child first; a
/// child still alive here has a slot that would otherwise stay out of its cell silently, and the host application quarantines it.
pub const TAKE_HORIZON: std::time::Duration = std::time::Duration::from_secs(5);
/// The same horizon for a render set (D9: its wedge suspicion is the offline block budget, 30 s): 33 s, past
/// 30 s + 2 s + a 250 ms tick, so a legitimately slow offline block is never abandoned before D7 could judge it.
pub const TAKE_HORIZON_RENDER: std::time::Duration = std::time::Duration::from_secs(33);

/// The horizon for a chain of this role.
pub fn take_horizon(render: bool) -> std::time::Duration {
    if render {
        TAKE_HORIZON_RENDER
    } else {
        TAKE_HORIZON
    }
}
/// The `restartComponent` flag D22 answers (`Vst::RestartFlags::kLatencyChanged`, `ivsteditcontroller.h`).
pub const K_LATENCY_CHANGED: u32 = 1 << 3;

/// The state word's name (`"?"` for a value that is no state), for logs.
pub fn state_name(w: u32) -> &'static str {
    match w {
        HOSTED => "HOSTED",
        DRAINING => "DRAINING",
        FADING => "FADING",
        OUT => "OUT",
        RETURNING => "RETURNING",
        _ => "?",
    }
}

/// The take's bound (D22 (c)): 2 block budgets (2 × B / rate) when B ≥ 144; (⌈144 / B⌉ + 1) block budgets when
/// B < 144, so a fade is never cut.
pub fn take_bound(max_frames: u32, rate: f64) -> std::time::Duration {
    let b = max_frames.max(1);
    let blocks = if b >= FADE { 2 } else { FADE.div_ceil(b) + 1 };
    std::time::Duration::from_secs_f64(f64::from(blocks * b) / rate.max(1.0))
}

/// The window by which a child's `slot.reactivated` `bypassedBlocks` may trail the host application's own count of that cycle's
/// bypassed blocks (`bridgeSlotBypassed`, R_STATUS bit 3), at a block of `block_frames` frames: the child counts a bypassed block at its END ([`ChainSide::end`] releases the
/// processor before its CAS), and sends the count right after the give-back without waiting for the chain thread.
/// So at most these blocks end after the count and land in the next event: the one in flight at the give-back, and
/// the return fade's (`FADE` = 144 frames from the start of the block that reads `RETURNING`: one block when B ≥ 144,
/// ⌈144 / B⌉ otherwise). 2 at B ≥ 144. A child never reports MORE than the host application counted. The same window binds every
/// child that runs the cycle (the test double and the plugin host); a child that runs no cycle reports none
/// (V2). The host application's own count is exact; this bounds only the child's report of it.
pub const fn child_bypass_lag_blocks(block_frames: u32) -> u32 {
    let b = if block_frames == 0 { 1 } else { block_frames };
    1 + if b >= FADE { 1 } else { FADE.div_ceil(b) }
}

/// The cycle's budget (D22 step 5): 4 block budgets. A longer cycle is a stall.
pub fn cycle_budget(max_frames: u32, rate: f64) -> std::time::Duration {
    std::time::Duration::from_secs_f64(f64::from(4 * max_frames.max(1)) / rate.max(1.0))
}

/// The loom model's mutation set (`--cfg loom` only): each weakens ONE rule, and the model must fail on it.
#[cfg(loom)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HandoverMutant {
    /// The shipped rules.
    #[default]
    None,
    /// The chain thread `try_lock`s the cell before it CASes `DRAINING → FADING` (and releases it on a failed CAS).
    LockBeforeCas,
    /// The forced take stores `OUT` instead of CASing `DRAINING → OUT` (it overwrites a `FADING`).
    PlainStoreForce,
    /// The main thread takes the processor without waiting for `OUT`.
    TakeBeforeOut,
    /// [`Cell::try_take`] waits (polls, up to three times) inside the turn
    /// instead of making one attempt, so a turn that already gave a take up waits again.
    BlockingReattempt,
    /// A `try_lock` missed in `HOSTED` is a fault at once, with no re-read and no re-dispatch (the code
    /// before the re-read was added): the forced take's two models must fail on it.
    NoReread,
}

/// Why the main thread could not take the processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TakeError {
    /// The word is not `OUT` (the value read).
    NotOut(u32),
    /// The chain thread holds the cell (only possible after a forced take, inside a block it began in `HOSTED`).
    Contended,
}

impl std::fmt::Display for TakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TakeError::NotOut(w) => write!(f, "the cell is not out (its word is {w})"),
            TakeError::Contended => f.write_str("the chain thread holds the cell"),
        }
    }
}

impl std::error::Error for TakeError {}

/// One hosted slot's cell: the state word and the processor.
pub struct Cell<P> {
    word: AtomicU32,
    /// A one-word lock (`runecraft_rt::rt_lock`): no OS mutex box behind it, so the chain thread's `try_lock`
    /// touches only this cell's memory (wired when the cell is in the arena).
    proc: WiredLock<P>,
    #[cfg(loom)]
    mutant: HandoverMutant,
}

impl<P: std::fmt::Debug> std::fmt::Debug for Cell<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cell({})", state_name(self.state()))
    }
}

/// One compare-exchange, never a wait. No poisoning: a panic while holding it released the word in the guard's drop;
/// the processor is still the slot's.
#[inline]
fn try_lock<P>(m: &WiredLock<P>) -> Option<WiredLockGuard<'_, P>> {
    m.try_lock()
}

impl<P> Cell<P> {
    /// On the main thread (a child builds its cells at `chain.load` / `chain.prepare`). The lock is one
    /// word with no lazy OS allocation (std's `Mutex` boxed its pthread mutex on first use, which the old cell had to
    /// take here so a chain thread's first `try_lock` did not allocate; an allocation-guard test found it).
    pub fn new(p: P) -> Cell<P> {
        Cell {
            word: AtomicU32::new(HOSTED),
            proc: WiredLock::new(p),
            #[cfg(loom)]
            mutant: HandoverMutant::None,
        }
    }

    #[cfg(loom)]
    /// The loom model's cell: [`Cell::new`] with one mutation of the rules switched on.
    pub fn with_mutant(p: P, mutant: HandoverMutant) -> Cell<P> {
        Cell {
            mutant,
            ..Cell::new(p)
        }
    }

    /// The processor, once nothing else holds the cell (teardown; the loom model's final check).
    pub fn into_inner(self) -> P {
        self.proc.into_inner()
    }

    /// The word (Acquire).
    pub fn state(&self) -> u32 {
        self.word.load(Ordering::Acquire)
    }

    fn cas(&self, from: u32, to: u32) -> Result<u32, u32> {
        self.word
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
    }

    // ---- The main thread -------------------------------------------------------------------------------------------

    /// (a) Announce: `HOSTED → DRAINING`. False if a cycle is already under way (the word is not `HOSTED`).
    pub fn announce(&self) -> bool {
        self.cas(HOSTED, DRAINING).is_ok()
    }

    /// (c) Past the bound: `DRAINING → OUT` by CAS. Ok: no processed block began a drain in the window. Err: the word
    /// read (`FADING`: a fade is under way; let it finish).
    pub fn force(&self) -> Result<(), u32> {
        #[cfg(loom)]
        if self.mutant == HandoverMutant::PlainStoreForce {
            self.word.store(OUT, Ordering::Release);
            return Ok(());
        }
        self.cas(DRAINING, OUT).map(|_| ())
    }

    /// (c) Take the processor: only in `OUT`, and only by `try_lock` (the main thread retries; it never blocks on
    /// the chain thread's hold).
    pub fn take(&self) -> Result<WiredLockGuard<'_, P>, TakeError> {
        let w = self.state();
        #[cfg(loom)]
        let w = if self.mutant == HandoverMutant::TakeBeforeOut {
            OUT
        } else {
            w
        };
        if w != OUT {
            return Err(TakeError::NotOut(w));
        }
        try_lock(&self.proc).ok_or(TakeError::Contended)
    }

    /// (d) Return: release the processor, then `OUT → RETURNING` (Release). The main thread does not wait for the
    /// return fade. False only if the word was not `OUT` (a protocol bug).
    pub fn give_back(&self, g: WiredLockGuard<'_, P>) -> bool {
        drop(g);
        self.cas(OUT, RETURNING).is_ok()
    }

    /// (c) The main thread's whole take: poll in `poll` sleeps (1 ms in production; the chain thread never signals
    /// it), force past `bound`, let a fade under way finish, and give up after `give_up` (a chain thread that stopped
    /// inside a fade: the wedge detector's case). Not modelled by loom (its steps are). The child's main thread
    /// only: it sleeps and reads the clock, which no chain thread does.
    #[cfg(not(loom))]
    #[allow(clippy::disallowed_methods)]
    pub fn take_blocking(
        &self,
        bound: std::time::Duration,
        poll: std::time::Duration,
        give_up: std::time::Duration,
    ) -> Result<Taken<'_, P>, TakeError> {
        let start = std::time::Instant::now();
        let mut forced = false;
        let mut contended = 0u32;
        loop {
            match self.take() {
                Ok(guard) => {
                    return Ok(Taken {
                        guard,
                        forced,
                        contended,
                        waited: start.elapsed(),
                    });
                }
                Err(TakeError::Contended) => contended += 1,
                Err(TakeError::NotOut(DRAINING)) if start.elapsed() >= bound => {
                    if self.force().is_ok() {
                        forced = true;
                        continue;
                    }
                }
                Err(TakeError::NotOut(DRAINING | FADING)) => {}
                Err(e) => return Err(e),
            }
            if start.elapsed() >= give_up {
                return Err(TakeError::NotOut(self.state()));
            }
            std::thread::sleep(poll);
        }
    }
}

/// One non-blocking take attempt ([`Cell::try_take`]): the processor if taken, whether this attempt forced
/// `DRAINING → OUT`, and how many times it waited (always 0 in the shipped code; the loom model counts it).
#[non_exhaustive]
pub struct Attempt<'a, P> {
    /// The processor, if this attempt took it.
    pub guard: Option<WiredLockGuard<'a, P>>,
    /// This attempt forced `DRAINING → OUT`.
    pub forced: bool,
    /// How many times this attempt waited (always 0 in the shipped code).
    pub waits: u32,
}

impl<'a, P> Attempt<'a, P> {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(guard: Option<WiredLockGuard<'a, P>>, forced: bool, waits: u32) -> Attempt<'a, P> {
        Attempt {
            guard,
            forced,
            waits,
        }
    }
}

impl<P> Cell<P> {
    /// **one** non-blocking attempt, made once per main-loop turn for a take that was given up
    /// (or deferred because the turn's one wait was spent). Past the take's bound (`past_bound`) it forces
    /// `DRAINING → OUT`, then takes (`OUT` plus `try_lock`). It never waits.
    pub fn try_take(&self, past_bound: bool) -> Attempt<'_, P> {
        let mut forced = false;
        #[allow(unused_mut)]
        let mut waits = 0;
        loop {
            match self.take() {
                Ok(g) => {
                    return Attempt {
                        guard: Some(g),
                        forced,
                        waits,
                    };
                }
                Err(TakeError::NotOut(DRAINING)) if past_bound && !forced => {
                    if self.force().is_ok() {
                        forced = true;
                        continue;
                    }
                }
                Err(_) => {}
            }
            #[cfg(loom)]
            if self.mutant == HandoverMutant::BlockingReattempt && waits < 3 {
                waits += 1;
                loom::thread::yield_now();
                continue;
            }
            return Attempt {
                guard: None,
                forced,
                waits,
            };
        }
    }
}

/// The main loop's turn, which waits at most once. The first take of a turn that needs to may block
/// (`Cell::take_blocking`, up to `TAKE_GIVE_UP`); once one take has waited to its give-up, [`Turn::may_wait`] is
/// false and every later take of the turn is one [`Cell::try_take`]. The children's main loops keep one `Turn` per
/// turn; the loom model counts its waits.
#[derive(Debug, Default)]
pub struct Turn {
    waits: u32,
}

impl Turn {
    /// A new turn, which has not waited yet.
    pub fn new() -> Turn {
        Turn::default()
    }

    /// Whether the next take of this turn may block.
    pub fn may_wait(&self) -> bool {
        self.waits == 0
    }

    /// A take of this turn waited (a blocking take given up, or a re-attempt that waited).
    pub fn record_waits(&mut self, n: u32) {
        self.waits += n;
    }

    /// How many waits this turn has recorded.
    pub fn waits(&self) -> u32 {
        self.waits
    }
}

/// What [`Cell::take_blocking`] got.
#[cfg(not(loom))]
#[non_exhaustive]
pub struct Taken<'a, P> {
    /// The processor.
    pub guard: WiredLockGuard<'a, P>,
    /// The take was forced past its bound (a stall, `forcedTake`).
    pub forced: bool,
    /// `try_lock` misses in `OUT` (only after a forced take).
    pub contended: u32,
    /// How long the take waited in all.
    pub waited: std::time::Duration,
}

impl<'a, P> Taken<'a, P> {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        guard: WiredLockGuard<'a, P>,
        forced: bool,
        contended: u32,
        waited: std::time::Duration,
    ) -> Taken<'a, P> {
        Taken {
            guard,
            forced,
            contended,
            waited,
        }
    }
}

// ---- The chain thread -----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Hosted,
    /// This chain thread CASed `DRAINING → FADING`: its own drain, fading y → b.
    Draining,
    Out,
    Returning,
}

/// The envelope of one block of one slot: where `xf` and `bin` start and which way `xf` moves (+1 toward b, −1 toward
/// y, 0 still). `pass` is a protocol fault's pass-through: the output is `b`, whatever the envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mix {
    /// The crossfade position at the block's first frame (0 = all plugin output, F = all bypass signal).
    pub xf: u32,
    /// The bypass signal's own fade-in position at the block's first frame (F = full level).
    pub bin: u32,
    /// The direction `xf` moves over the block: +1 toward the bypass signal, −1 toward the plugin, 0 still.
    pub dir: i32,
    /// A protocol fault's pass-through: the output is the bypass signal, whatever the envelope.
    pub pass: bool,
}

impl Mix {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(xf: u32, bin: u32, dir: i32, pass: bool) -> Mix {
        Mix { xf, bin, dir, pass }
    }
}

impl Mix {
    /// The hosted envelope: the plugin's output alone, unchanged.
    pub const PLUGIN: Mix = Mix {
        xf: 0,
        bin: FADE,
        dir: 0,
        pass: false,
    };

    /// (xf, bin) at frame `i` of the block.
    #[inline]
    pub fn at(&self, i: usize) -> (u32, u32) {
        let i = i.min(FADE as usize) as i64;
        let xf = (i64::from(self.xf) + i64::from(self.dir) * i).clamp(0, i64::from(FADE)) as u32;
        let bin = (i64::from(self.bin) + i).min(i64::from(FADE)) as u32;
        (xf, bin)
    }

    /// Whether the bypass signal reaches the output anywhere in this block (the block sets `ST_BYPASSED`).
    pub fn bypassed(&self) -> bool {
        self.pass || self.xf > 0 || self.dir > 0
    }

    /// The output into `out` (which holds `y` if the plugin ran this block; it is ignored where `xf` = F) from the
    /// bypass signal `b`, frame by frame. No allocation.
    #[inline]
    pub fn apply(&self, fade: &Fade, out: &mut [f32], b: &[f32]) {
        if self.pass {
            out.copy_from_slice(&b[..out.len()]);
            return;
        }
        if *self == Mix::PLUGIN {
            return;
        }
        for (i, (x, &bv)) in out.iter_mut().zip(b).enumerate() {
            let (xf, bin) = self.at(i);
            *x = if xf == 0 {
                *x
            } else if xf == FADE {
                if bin == FADE { bv } else { bv * fade.g(bin) }
            } else {
                let g = fade.g(xf);
                let gb = if bin == FADE { g } else { g * fade.g(bin) };
                *x * (1.0 - g) + bv * gb
            };
        }
    }
}

/// The raised cosine g(n) = (1 − cos(π·n / F)) / 2, n = 0…F, as a table (built once, off the chain thread).
#[derive(Clone, Debug)]
pub struct Fade {
    g: [f32; FADE as usize + 1],
}

impl Default for Fade {
    fn default() -> Fade {
        let mut g = [0.0f32; FADE as usize + 1];
        for (n, v) in g.iter_mut().enumerate() {
            *v = ((1.0 - (std::f64::consts::PI * n as f64 / f64::from(FADE)).cos()) / 2.0) as f32;
        }
        g[0] = 0.0;
        g[FADE as usize] = 1.0;
        Fade { g }
    }
}

impl Fade {
    /// g(n), with `n` clamped to F.
    #[inline]
    pub fn g(&self, n: u32) -> f32 {
        self.g[n.min(FADE) as usize]
    }
}

/// [`ChainSide::begin`]'s answer: the processor (when this block may call the plugin) and the envelope.
#[non_exhaustive]
pub struct Begin<'a, P> {
    /// The processor, when this block may call the plugin.
    pub guard: Option<WiredLockGuard<'a, P>>,
    /// This block's envelope.
    pub mix: Mix,
    /// A protocol fault: a `try_lock` miss where the protocol rules it out (counted, as a pass-through).
    pub fault: bool,
    /// The word this block dispatched on (after a failed `DRAINING → FADING` CAS, the value that CAS read). A block
    /// that dispatched on `OUT` never holds the processor (the loom model asserts it).
    pub word: u32,
}

impl<'a, P> Begin<'a, P> {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        guard: Option<WiredLockGuard<'a, P>>,
        mix: Mix,
        fault: bool,
        word: u32,
    ) -> Begin<'a, P> {
        Begin {
            guard,
            mix,
            fault,
            word,
        }
    }
}

/// One hosted slot's chain-thread side: its phase and envelope. Plain data, owned by the chain thread.
#[derive(Clone, Copy, Debug)]
pub struct ChainSide {
    phase: Phase,
    xf: u32,
    bin: u32,
}

impl Default for ChainSide {
    fn default() -> ChainSide {
        ChainSide {
            phase: Phase::Hosted,
            xf: 0,
            bin: FADE,
        }
    }
}

impl ChainSide {
    fn pass<'a, P>(&self, word: u32) -> Begin<'a, P> {
        Begin {
            guard: None,
            mix: Mix {
                xf: self.xf,
                bin: self.bin,
                dir: 0,
                pass: true,
            },
            fault: true,
            word,
        }
    }

    /// The top of a processed block, for one slot: read the word and decide whether the plugin runs and how its
    /// output mixes with the bypass signal. Never waits.
    pub fn begin<'a, P>(&mut self, cell: &'a Cell<P>) -> Begin<'a, P> {
        self.dispatch(cell, cell.state(), true)
    }

    /// [`ChainSide::begin`] on the word `w`. `first` is false on the one re-dispatch, after a failed
    /// `DRAINING → FADING` CAS, on the value that CAS read (`OUT`: the take was forced; `RETURNING`: forced, cycled
    /// and given back, all before this thread's CAS).
    fn dispatch<'a, P>(&mut self, cell: &'a Cell<P>, w: u32, first: bool) -> Begin<'a, P> {
        let mut dir = 0;
        let guard = match w {
            HOSTED => {
                if self.phase != Phase::Hosted {
                    *self = ChainSide::default();
                }
                match try_lock(&cell.proc) {
                    Some(g) => Some(g),
                    None => return self.missed(cell, w, first),
                }
            }
            DRAINING if first => {
                #[cfg(loom)]
                let early = (cell.mutant == HandoverMutant::LockBeforeCas)
                    .then(|| try_lock(&cell.proc))
                    .flatten();
                match cell.cas(DRAINING, FADING) {
                    Ok(_) => {
                        self.phase = Phase::Draining;
                        dir = 1;
                        #[cfg(loom)]
                        let got = early.or_else(|| try_lock(&cell.proc));
                        #[cfg(not(loom))]
                        let got = try_lock(&cell.proc);
                        match got {
                            Some(g) => Some(g),
                            None => return self.pass(w),
                        }
                    }
                    Err(now) => {
                        // The main thread forced the take: this thread never touched the cell (CAS first), holds
                        // nothing (any try_lock released at once), and goes on from the word it read: OUT passes
                        // through, fading the bypass signal in from silence; RETURNING resumes the plugin.
                        #[cfg(loom)]
                        drop(early);
                        return self.dispatch(cell, now, false);
                    }
                }
            }
            FADING => {
                if self.phase != Phase::Draining {
                    return self.pass(w);
                }
                dir = 1;
                match try_lock(&cell.proc) {
                    Some(g) => Some(g),
                    None => return self.pass(w),
                }
            }
            OUT => match self.phase {
                // The take was forced before this thread saw DRAINING.
                Phase::Hosted => {
                    self.forced_out();
                    None
                }
                Phase::Out => None,
                Phase::Draining | Phase::Returning => return self.pass(w),
            },
            RETURNING => {
                self.phase = Phase::Returning;
                dir = -1;
                match try_lock(&cell.proc) {
                    Some(g) => Some(g),
                    None => return self.pass(w),
                }
            }
            _ => return self.pass(w),
        };
        Begin {
            guard,
            mix: Mix {
                xf: self.xf,
                bin: self.bin,
                dir,
                pass: false,
            },
            fault: false,
            word: w,
        }
    }

    /// A `try_lock` missed in `HOSTED`. Only the main thread holds a cell, and
    /// only in `OUT` (after a forced take this thread did not see: it read `HOSTED` before the force). The miss
    /// happens-after that hold's acquisition (`rt_lock`'s R1: AcqRel / Acquire), so one re-read of the word sees `OUT`
    /// or later (`RETURNING`), and the block goes on from it: `OUT` is the won force's fade-in from silence, no fault;
    /// `RETURNING` resumes the plugin. Once per block: the re-dispatch is not `first`, and a miss there, or a re-read
    /// that is still `HOSTED` (the protocol rules it out), is a fault as before. One Acquire load; no wait.
    fn missed<'a, P>(&mut self, cell: &'a Cell<P>, w: u32, first: bool) -> Begin<'a, P> {
        #[cfg(loom)]
        let first = first && cell.mutant != HandoverMutant::NoReread;
        if first {
            let now = cell.state();
            if now != w {
                return self.dispatch(cell, now, false);
            }
        }
        self.pass(w)
    }

    fn forced_out(&mut self) {
        self.phase = Phase::Out;
        self.xf = FADE;
        self.bin = 0;
    }

    /// The end of the block, for one slot: release the processor first, then advance the envelope and make the
    /// chain thread's CASes (`FADING → OUT`, `RETURNING → HOSTED`, both Release). Returns whether the block passed
    /// the bypass signal through (`ST_BYPASSED`).
    pub fn end<P>(&mut self, cell: &Cell<P>, frames: usize, begun: Begin<'_, P>) -> bool {
        let Begin {
            guard, mix, fault, ..
        } = begun;
        drop(guard);
        let bypassed = mix.bypassed();
        if fault {
            return bypassed;
        }
        let (xf, bin) = mix.at(frames);
        self.xf = xf;
        self.bin = bin;
        // Only this thread moves FADING and RETURNING; a failed CAS is a protocol bug (the word is left as it is).
        if self.phase == Phase::Draining && self.xf == FADE {
            if cell.cas(FADING, OUT).is_ok() {
                self.phase = Phase::Out;
            }
        } else if self.phase == Phase::Returning
            && self.xf == 0
            && cell.cas(RETURNING, HOSTED).is_ok()
        {
            *self = ChainSide::default();
        }
        bypassed
    }
}

/// The bypass signal of a hosted effect slot (D22): its input, delayed by its last applied latency L. Preallocated
/// (`channels × (C + maxFrames)` f32, C = max(L at prepare, 16,384)); written on **every** block before the plugin
/// is called, so it is primed whenever a bypass begins. A latency above C uses a delay of C (counted,
/// `bypassMisaligned`); the line is resized only at the next prepare, never while in use.
#[derive(Clone, Debug)]
pub struct DelayLine {
    buf: Vec<f32>,
    channels: usize,
    len: usize,
    cap: usize,
    w: usize,
}

impl DelayLine {
    /// On the main thread (allocates).
    #[allow(clippy::disallowed_methods)]
    pub fn new(channels: usize, latency: usize, max_frames: usize) -> DelayLine {
        let cap = latency.max(DELAY_MIN_CAPACITY);
        let len = cap + max_frames.max(1);
        DelayLine {
            buf: vec![0.0; channels.max(1) * len],
            channels: channels.max(1),
            len,
            cap,
            w: 0,
        }
    }

    /// C, in frames.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Write this block's input of channel `c` (call for every channel, then [`DelayLine::advance`]).
    #[inline]
    pub fn write(&mut self, c: usize, x: &[f32]) {
        if c >= self.channels {
            return;
        }
        let base = c * self.len;
        for (i, v) in x.iter().enumerate().take(self.len - self.cap) {
            self.buf[base + (self.w + i) % self.len] = *v;
        }
    }

    /// Read channel `c`'s bypass signal for this block (after its [`DelayLine::write`], before
    /// [`DelayLine::advance`]): the input `latency` frames ago. Returns false when `latency` exceeded C (a delay of C
    /// was used: misaligned).
    #[inline]
    pub fn read(&self, c: usize, latency: usize, out: &mut [f32]) -> bool {
        let d = latency.min(self.cap);
        let c = c.min(self.channels - 1);
        let base = c * self.len;
        let start = self.w + self.len - d;
        for (i, o) in out.iter_mut().enumerate().take(self.len - self.cap) {
            *o = self.buf[base + (start + i) % self.len];
        }
        latency <= self.cap
    }

    /// Moves the write position on by the block's `frames` (after every channel's `write` and `read`).
    pub fn advance(&mut self, frames: usize) {
        self.w = (self.w + frames.min(self.len - self.cap)) % self.len;
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;

    #[test]
    fn the_fade_is_a_raised_cosine_with_its_bound() {
        let f = Fade::default();
        assert_eq!((f.g(0), f.g(FADE), f.g(FADE + 7)), (0.0, 1.0, 1.0));
        assert!((f.g(FADE / 2) - 0.5).abs() < 1e-6);
        let worst = (1..=FADE)
            .map(|n| f.g(n) - f.g(n - 1))
            .fold(0.0f32, f32::max);
        // The largest per-sample slope is π / (2F) = 0.0109 at F = 144.
        assert!(worst <= std::f32::consts::PI / (2.0 * FADE as f32) + 1e-6);
        assert!(worst > 0.0108);
    }

    #[test]
    fn the_take_bound_never_cuts_a_fade() {
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
        assert!((ms(take_bound(512, 48_000.0)) - 2.0 * 512.0 / 48.0).abs() < 1e-5);
        assert!((ms(take_bound(144, 48_000.0)) - 2.0 * 144.0 / 48.0).abs() < 1e-5);
        // B = 64 < 144: ⌈144 / 64⌉ + 1 = 4 blocks.
        assert!((ms(take_bound(64, 48_000.0)) - 4.0 * 64.0 / 48.0).abs() < 1e-5);
        assert!((ms(cycle_budget(512, 48_000.0)) - 4.0 * 512.0 / 48.0).abs() < 1e-5);
    }

    #[test]
    fn the_delay_line_delays_by_exactly_l() {
        for (l, block) in [(0usize, 32usize), (7, 32), (100, 64), (16_384, 512)] {
            let mut d = DelayLine::new(2, l, 512);
            let mut t = 0usize;
            for _ in 0..80 {
                let x: Vec<f32> = (0..block).map(|i| (t + i) as f32 + 1.0).collect();
                let y: Vec<f32> = x.iter().map(|v| -v).collect();
                d.write(0, &x);
                d.write(1, &y);
                let mut o = vec![0.0; block];
                assert!(d.read(0, l, &mut o));
                for (i, v) in o.iter().enumerate() {
                    let want = if t + i >= l {
                        (t + i - l) as f32 + 1.0
                    } else {
                        0.0
                    };
                    assert_eq!(*v, want, "L {l}, frame {}", t + i);
                }
                d.read(1, l, &mut o);
                assert!(o.iter().all(|v| *v <= 0.0));
                d.advance(block);
                t += block;
            }
        }
        // Past C: a delay of C, reported.
        let d = DelayLine::new(1, 10, 64);
        assert_eq!(d.capacity(), DELAY_MIN_CAPACITY);
        let mut o = [0.0; 8];
        assert!(!d.read(0, DELAY_MIN_CAPACITY + 1, &mut o));
    }

    /// Drive the shipped machine through a whole cycle on one thread (the interleavings are loom's).
    #[test]
    fn a_whole_cycle_drains_takes_returns_and_hosts_again() {
        let cell = Cell::new(0u32);
        let mut side = ChainSide::default();
        let run = |side: &mut ChainSide, cell: &Cell<u32>, frames: usize| {
            let mut b = side.begin(cell);
            let ran = b.guard.as_deref_mut().map(|p| *p += 1).is_some();
            let mix = b.mix;
            let by = side.end(cell, frames, b);
            (ran, mix, by)
        };
        assert_eq!(run(&mut side, &cell, 64), (true, Mix::PLUGIN, false));
        assert!(cell.announce());
        assert!(!cell.announce(), "one cycle at a time");
        assert_eq!(cell.take().err(), Some(TakeError::NotOut(DRAINING)));
        // B = 64 < 144: the drain spans three blocks, the plugin runs in each, and the word reaches OUT after it.
        let (ran, mix, by) = run(&mut side, &cell, 64);
        assert!(ran && by && mix.dir == 1 && mix.xf == 0);
        assert_eq!(cell.state(), FADING);
        assert_eq!(
            cell.force(),
            Err(FADING),
            "a fade under way is never forced"
        );
        run(&mut side, &cell, 64);
        assert_eq!(cell.state(), FADING);
        let (ran, _, _) = run(&mut side, &cell, 64);
        assert!(ran);
        assert_eq!(cell.state(), OUT);
        let g = cell.take().expect("OUT: the main thread takes it at once");
        let (ran, mix, by) = run(&mut side, &cell, 64);
        assert!(!ran && by && mix.xf == FADE && mix.bin == FADE);
        assert!(cell.give_back(g));
        assert_eq!(cell.state(), RETURNING);
        let (ran, mix, _) = run(&mut side, &cell, 100);
        assert!(ran && mix.dir == -1 && mix.xf == FADE);
        assert_eq!(cell.state(), RETURNING);
        run(&mut side, &cell, 100);
        assert_eq!(cell.state(), HOSTED);
        assert_eq!(run(&mut side, &cell, 64), (true, Mix::PLUGIN, false));
    }

    #[test]
    fn a_forced_take_fades_the_bypass_in_from_silence() {
        let cell = Cell::new(());
        let mut side = ChainSide::default();
        assert!(cell.announce());
        assert_eq!(cell.force(), Ok(()));
        let _g = cell.take().unwrap();
        let b = side.begin(&cell);
        assert!(b.guard.is_none() && !b.fault);
        assert_eq!((b.mix.xf, b.mix.bin, b.mix.dir), (FADE, 0, 0));
        let mut out = [9.0f32; 4];
        b.mix.apply(&Fade::default(), &mut out, &[1.0; 4]);
        assert_eq!(
            out[0], 0.0,
            "from silence, not from a y the plugin never made"
        );
        assert!(out[3] > 0.0 && out[3] < 0.01);
        assert!(side.end(&cell, 4, b));
    }

    #[test]
    fn the_mix_is_exact_at_its_ends_and_bounded_between() {
        let f = Fade::default();
        let n = 600;
        let y: Vec<f32> = (0..n).map(|i| 0.25 * (i as f32 * 0.13).sin()).collect();
        let b: Vec<f32> = (0..n).map(|i| (i as f32 * 0.13 + 0.4).sin()).collect();
        for dir in [1, -1] {
            let start = if dir == 1 { 0 } else { FADE };
            let m = Mix {
                xf: start,
                bin: FADE,
                dir,
                pass: false,
            };
            let mut x = y.clone();
            m.apply(&f, &mut x, &b);
            let (first, last) = if dir == 1 { (&y, &b) } else { (&b, &y) };
            assert_eq!(x[0].to_bits(), first[0].to_bits());
            assert_eq!(x[n - 1].to_bits(), last[n - 1].to_bits());
            for i in 1..n {
                let bound = (y[i] - y[i - 1]).abs().max((b[i] - b[i - 1]).abs())
                    + std::f32::consts::PI / (2.0 * FADE as f32)
                        * (y[i] - b[i]).abs().max((y[i - 1] - b[i - 1]).abs())
                    + 1e-6;
                assert!((x[i] - x[i - 1]).abs() <= bound, "dir {dir} frame {i}");
            }
        }
    }
}
