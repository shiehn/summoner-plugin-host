//! One chain: its region, its slots and its thread.
//!
//! The thread waits on its group's hub (or, parked, on its own seq), and for every published seq runs the block:
//! parameters, inputs, MIDI (a reset first injects all-notes-off and resets every slot), the transport, the slots in
//! order, outputs, then the response line and the ack (Release). A block runs only when the chain is prepared and the
//! message thread does not hold its processors (a non-blocking try-lock): otherwise it is a silent, acked, SKIPPED
//! block. A slot that panics makes the block THREW and silent. The response line's +140 is reserved and
//! written 0: the chain's latency (its PDC, the sum of its hosted slots') reaches the host application only through
//! `chain.loaded` and `latency.changed`.
//!
//! The block's buffers, MIDI list and stats are allocated at load, never in the loop.
use runecraft_ipc::bridge::FanOutEndpoint;
use runecraft_ipc::bridge::sync::Waited;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use summoner_plugin::handover;
use summoner_proc::{Block, Dsp, Io, MidiEvent, Plugin, Transport};
use summoner_proto::layout::Geometry;
use summoner_proto::layout::*;
use summoner_rt::region::{ChildPage, Region};

/// One slot of a chain, message-thread side.
pub struct SlotEntry {
    pub name: String,
    /// `None`: quarantined or not a description this build hosts; the slot passes audio through.
    pub plugin: Option<Arc<dyn Plugin>>,
    /// What `chain.loaded` reported (a hostile `--lie-quarantined` child lies here; the host application must not
    /// believe it).
    pub reported_latency: u32,
    pub reported_params: u32,
    pub child_gen: AtomicU64,
    /// The plugin's edit count as the message thread last saw it ([`summoner_proc::Plugin::edits`]).
    pub edits_seen: AtomicU64,
    pub editor_open: AtomicBool,
    /// re-activations run for this slot (a VST3 `kLatencyChanged`); the blocks it passed the bypass signal
    /// through (ST_BYPASSED, the chain thread counts); the cycles over `handover::cycle_budget`; the bypassed blocks
    /// whose delay line could not hold the latency. `bypassed` and `misaligned` go up with each `slot.reactivated`
    /// (and are reset by it); the totals stay in `--counters`.
    pub reactivations: AtomicU64,
    pub bypassed: AtomicU64,
    pub bypassed_total: AtomicU64,
    pub stalls: AtomicU64,
    pub misaligned: AtomicU64,
    /// The plugin asked to be re-activated and this slot's word was not `HOSTED` yet (a cycle still returning): kept
    /// for the next turn (message thread).
    pub restart_pending: AtomicBool,
    /// A host `param.set` left a value for the processor; the poll moves `child_gen` once the plugin says
    /// none waits ([`summoner_proc::Plugin::host_set_pending`]), so a capture after it reads the applied state.
    pub host_set_waiting: AtomicBool,
    /// A take that was given up (or deferred: the turn's one wait spent), re-attempted each turn (message thread).
    pub pending: Mutex<Option<PendingTake>>,
    /// The slot is enabled (`chain.load`'s `enabled`, absent = true;
    /// `slot.setEnabled`). Stored by the message thread (Release), read by the chain thread once per block (Acquire).
    /// Disabled, the slot outputs its bypass signal (an effect's input delayed by its latency, an instrument's
    /// silence) after a crossfade of `handover::FADE` frames; the chain's latency does not move. A disabled plugin:
    /// with a latency above 0 the plugin is still called
    /// every block and its output discarded; at latency 0 it is not called once the fade is over.
    pub enabled: AtomicBool,
    /// The blocks the plugin was called for (chain thread; `--counters`).
    pub calls: AtomicU64,
    /// The editor-reset guard's deliberate writes (the deliberate-write epoch):
    /// bumped by every `state.set` to this slot (message thread).
    pub deliberate_epoch: AtomicU64,
    /// Host parameter writes applied to this slot (the region's parameter values, chain thread, relaxed): the reset
    /// guard stands down when it moves (automation is a deliberate write too).
    pub param_writes: AtomicU64,
}

/// One slot's enabled fader, chain-thread only: `xf` is the bypass signal's share of the output, 0 (the plugin alone)
/// to `handover::FADE` (the bypass signal alone), the same envelope and raised cosine as the re-activation handover.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fader {
    xf: u32,
}

// A fader is plain values (its one field asserted).
impl runecraft_rt::arena::RtMem for Fader {}
const _: () = runecraft_rt::arena::assert_rt_mem::<u32>();

impl Fader {
    /// The block's envelope for a slot whose flag reads `enabled`: toward the plugin when enabled, toward the bypass
    /// signal when not; still when there. `handover::Mix::PLUGIN` when the slot is enabled and the fade is over.
    fn mix(self, enabled: bool) -> handover::Mix {
        let dir = match (enabled, self.xf) {
            (true, 0) => 0,
            (true, _) => -1,
            (false, handover::FADE) => 0,
            (false, _) => 1,
        };
        handover::Mix::new(self.xf, handover::FADE, dir, false)
    }

    /// After a block of `frames` under `m`.
    fn advance(&mut self, m: handover::Mix, frames: usize) {
        self.xf = m.at(frames).0;
    }
}

/// `--test-slot-enabled` (test-hooks): `(slot, enabled, block)` changes the chain thread makes to every chain's slot
/// flag at the top of its `block`-th processed block (0-based; block 0 is applied at load, so the slot starts that way
/// with no fade), standing in for `chain.load`'s `enabled` and `slot.setEnabled` until summoner-proto's codec is at the
/// pin.
#[cfg(feature = "test-hooks")]
pub static TEST_SLOT_ENABLED: std::sync::OnceLock<Vec<(u32, bool, u64)>> =
    std::sync::OnceLock::new();

/// A re-activation's take in progress: when it was announced, and whether it was forced.
#[derive(Clone, Copy, Debug)]
pub struct PendingTake {
    pub announced: Instant,
    pub forced: bool,
}

/// One hosted slot's processor in its handover cell: `Some` once prepared. The chain thread reaches it only through
/// `handover::ChainSide::begin` (a `try_lock`, never a wait); the message thread only through the handover's take.
pub type SlotCell = handover::Cell<Option<Box<dyn Dsp>>>;

/// The chain record in runecraft-rt's wired arena, shared by the message thread and its chain thread (a shared single
/// value is an `ArenaArc<[T]>` of length 1).
#[derive(Clone)]
pub struct ChainArc(runecraft_rt::arena::ArenaArc<[Chain]>);

impl ChainArc {
    /// The record in a new arena block (message thread).
    pub fn new(c: Chain) -> ChainArc {
        let mut c = Some(c);
        ChainArc(runecraft_rt::arena::ArenaArc::from_fn(RT_OWNER, 1, |_| {
            c.take().expect("one chain")
        }))
    }
}

impl std::ops::Deref for ChainArc {
    type Target = Chain;
    fn deref(&self) -> &Chain {
        &self.0[0]
    }
}

/// The chain's prepared gate and what each prepare builds, behind the lock the message thread holds to prepare or
/// release the chain (the chain thread try-locks it: held, the block is SKIPPED).
///
/// A re-activation never takes this lock (a miss here would silence every slot of the chain):
/// it reaches its slot's cell through [`Chain::cells`], the message thread's own clone of the same `Arc`, and the
/// handover machine passes only that slot's bypass signal through meanwhile.
pub struct Procs {
    pub prepared: bool,
    /// One cell per slot (unhosted slots' stay empty), made at prepare (`handover::Cell::new` primes its mutex).
    pub cells: Option<runecraft_rt::arena::ArenaArc<[SlotCell]>>,
    /// Each hosted EFFECT slot's bypass delay line (the reactivation handover: its input delayed by its latency; an
    /// instrument's bypass is silence), allocated at prepare on the message thread, written by the chain thread every
    /// block.
    pub lines: runecraft_rt::arena::ArenaSlice<Option<handover::DelayLine>>,
    /// The chain thread's side of each slot's handover, reset at every prepare.
    pub sides: runecraft_rt::arena::ArenaSlice<handover::ChainSide>,
    /// Each slot's [`Dsp::io`], read once at prepare (message thread), never per block (an unhosted slot's
    /// is [`Io::STEREO`], unused: it is never called).
    pub ios: runecraft_rt::arena::ArenaSlice<Io>,
}

pub struct Chain {
    pub key: u64,
    pub geo: Geometry,
    pub rate: f64,
    pub region: Region,
    pub page: Arc<ChildPage>,
    /// This chain's waiter endpoint of its ring group's hub (`chain.load`'s `fanSlot` on Windows; slot 0, the shared
    /// word, on unix).
    fan: FanOutEndpoint,
    /// The slots' records in the wired arena (the chain thread reads each slot's atomics and plugin every block).
    pub slots: runecraft_rt::arena::ArenaSlice<SlotEntry>,
    pub procs: runecraft_rt::rt_lock::WiredLock<Procs>,
    /// The message thread's clone of the prepared cells (the handover's take runs on it; never the chain thread's).
    pub cells: Mutex<Option<runecraft_rt::arena::ArenaArc<[SlotCell]>>>,
    /// The raised-cosine table, built before the chain thread runs.
    fade: handover::Fade,
    pub parked: AtomicBool,
    /// a render set (`chain.load {role: render}`, or the region's render flag): its thread runs at QoS
    /// user-initiated (no time constraint, no workgroup), waits on its own `seq` (never a hub) and wakes `ack` after
    /// every block (the render worker waits on it); its slots are prepared offline (`Plugin::prepare_mode`).
    pub render: bool,
    /// The latency `chain.loaded` or the last `latency.changed` reported (message thread).
    pub reported_latency: AtomicU32,
    stop: AtomicBool,
    tc_us: Option<u64>,
    stats: bool,
    thread: Mutex<Option<JoinHandle<Vec<u64>>>>,
    /// `--noalloc-blocks` (test-hooks): the injected counter and how many first blocks it counts.
    noalloc: Option<(summoner_sys::alloc_count::Counter, u32)>,
    /// The inventory (debug builds): the working set's buffers checked against the arena's wired chunks at start, and
    /// those not in one.
    pub rt_checked: AtomicU64,
    pub rt_unwired: AtomicU64,
    /// The first blocks' allocations on the chain thread, and how many blocks were counted.
    pub noalloc_allocs: AtomicU64,
    pub noalloc_counted: AtomicU32,
    /// The chain thread's wired stack top (bytes), the bytes a refusal left unwired, and whether the thread
    /// has tried (set once, before its loop; [`Chain::start`] waits for it so `chain.loaded` can count it).
    pub stack_wired: AtomicU64,
    pub stack_unwired: AtomicU64,
    /// The chain's memory as last reported to the host application (`chain.loaded`, then each `chain.prepared`):
    /// wired, and what could not be wired.
    pub reported_wired: AtomicU64,
    pub reported_unwired: AtomicU64,
    stack_tried: AtomicBool,
    /// `--stack-water` (test-hooks): the deepest the chain thread's stack went while it ran (`stack_high_water`),
    /// bytes.
    pub stack_used: AtomicU64,
}

/// The top of each chain thread's stack wired before its loop (measured per thread, with a 256 KiB floor). Measured
/// with `--stack-water` on the lab's plugins (`rt_memory.rs` asserts the deepest use stays under half of it); a
/// plugin's frames that go deeper are a known residual.
pub const STACK_WIRE_BYTES: usize = 256 << 10;

/// `--stack-water` (test-hooks): each chain thread runs its loop under `stack_high_water` and records the depth.
#[cfg(feature = "test-hooks")]
pub static STACK_WATER: AtomicBool = AtomicBool::new(false);

impl Chain {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: u64,
        geo: Geometry,
        rate: f64,
        region: Region,
        page: Arc<ChildPage>,
        slots: Vec<SlotEntry>,
        tc_us: Option<u64>,
        stats: bool,
        noalloc_blocks: u32,
        render: bool,
        fan: FanOutEndpoint,
    ) -> Chain {
        let c = Chain {
            key,
            geo,
            rate,
            region,
            page,
            fan,
            slots: {
                let n = slots.len();
                let mut it = slots.into_iter();
                runecraft_rt::arena::ArenaSlice::from_fn(RT_OWNER, n, |_| {
                    it.next().expect("n slots")
                })
            },
            procs: runecraft_rt::rt_lock::WiredLock::new(Procs {
                prepared: false,
                cells: None,
                lines: runecraft_rt::arena::ArenaSlice::from_fn(RT_OWNER, 0, |_| None),
                sides: runecraft_rt::arena::ArenaSlice::from_fn(RT_OWNER, 0, |_| {
                    handover::ChainSide::default()
                }),
                ios: runecraft_rt::arena::ArenaSlice::from_fn(RT_OWNER, 0, |_| Io::STEREO),
            }),
            cells: Mutex::new(None),
            fade: handover::Fade::default(),
            parked: AtomicBool::new(false),
            render,
            reported_latency: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            tc_us,
            stats,
            thread: Mutex::new(None),
            noalloc: (noalloc_blocks > 0)
                .then(summoner_sys::alloc_count::Counter::resolve)
                .flatten()
                .map(|k| (k, noalloc_blocks)),
            noalloc_allocs: AtomicU64::new(0),
            rt_checked: AtomicU64::new(0),
            rt_unwired: AtomicU64::new(0),
            noalloc_counted: AtomicU32::new(0),
            stack_wired: AtomicU64::new(0),
            stack_unwired: AtomicU64::new(0),
            reported_wired: AtomicU64::new(0),
            reported_unwired: AtomicU64::new(0),
            stack_tried: AtomicBool::new(false),
            stack_used: AtomicU64::new(0),
        };
        // The procs gate and each slot's cell are `WiredLock`s (one atomic word beside the value, in this
        // record's wired arena block): no OS mutex to box on first use, so nothing to prime.
        c
    }

    pub fn start(this: &ChainArc) {
        let c = this.clone();
        let this: &Chain = this;
        // The thread's diagnostic RT ring, made (and prefaulted) here, on the message thread.
        let rt = crate::diag::rt_ring("hc-chain", this.key);
        // The working set in the wired arena, made here (the arena is never called on a chain thread).
        // A slot loaded disabled starts bypassed, with no fade at its first block.
        let mf = this.geo.max_frames as usize;
        let nbuf = 2.max(this.geo.ch_in).max(this.geo.ch_out) as usize;
        let faders = this.slots.iter().map(|e| Fader {
            xf: if e.enabled.load(Ordering::Acquire) {
                0
            } else {
                handover::FADE
            },
        });
        let s = Scratch::new(
            mf,
            nbuf,
            this.geo.max_midi as usize + 16,
            this.stats,
            rt,
            faders,
        );
        // The inventory (debug builds: `RtArena::contains` is test-only): every buffer of the working set checked
        // against the arena's wired chunks, the counts in `--counters`.
        #[cfg(debug_assertions)]
        {
            let arena = runecraft_rt::arena::RtArena::global();
            let check = |p: *const u8, len: usize| {
                if len == 0 {
                    return;
                }
                this.rt_checked.fetch_add(1, Ordering::Relaxed);
                if arena.contains(p, len) != Some(true) {
                    this.rt_unwired.fetch_add(1, Ordering::Relaxed);
                }
            };
            for b in s.bufs.iter().chain(s.byp.iter()) {
                check(b.as_ptr().cast(), std::mem::size_of_val(&**b));
            }
            check(s.bufs.as_ptr().cast(), std::mem::size_of_val(&*s.bufs));
            check(s.byp.as_ptr().cast(), std::mem::size_of_val(&*s.byp));
            check(
                s.midi.storage().as_ptr().cast(),
                std::mem::size_of_val(s.midi.storage()),
            );
            check(
                s.stats.storage().as_ptr().cast(),
                std::mem::size_of_val(s.stats.storage()),
            );
            check(s.faders.as_ptr().cast(), std::mem::size_of_val(&*s.faders));
            // The chain record itself and its slots' records.
            check(
                std::ptr::from_ref::<Chain>(this).cast(),
                std::mem::size_of::<Chain>(),
            );
            check(
                this.slots.as_ptr().cast(),
                std::mem::size_of_val(&*this.slots),
            );
            // The procs gate the chain thread try-locks every block (its lock word and value, inside the record).
            check(
                std::ptr::from_ref(&this.procs).cast(),
                std::mem::size_of_val(&this.procs),
            );
        }
        let t = std::thread::Builder::new()
            .name(format!("hc-chain-{}", this.key))
            .spawn(move || run(&c, s))
            .expect("the chain thread");
        *this.thread.lock().unwrap() = Some(t);
        // Row 13: the thread wires its stack top first thing; wait for it (bounded) so `chain.loaded` counts it.
        let until = Instant::now() + Duration::from_secs(1);
        while !this.stack_tried.load(Ordering::Acquire) && Instant::now() < until {
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// Stop the thread and join it within `ms`; `None` when it is wedged inside a plugin (it is abandoned: the
    /// caller keeps the chain alive and never touches its processors again). `Some` carries the `--stats` pairs.
    pub fn stop_and_join(&self, ms: u64) -> Option<Vec<u64>> {
        self.stop.store(true, Ordering::SeqCst);
        let mut guard = self.thread.lock().unwrap();
        let deadline = Instant::now() + Duration::from_millis(ms);
        while guard.as_ref().is_some_and(|t| !t.is_finished()) && Instant::now() < deadline {
            let _ = self.region.seq_waker().wake_all();
            for g in 0..RING_GROUPS {
                let _ = self.page.hub_waker(g).wake_all();
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        match guard.take() {
            Some(t) if t.is_finished() => Some(t.join().unwrap_or_default()),
            Some(t) => {
                *guard = Some(t);
                None
            }
            None => Some(Vec::new()),
        }
    }

    /// The chain's latency in frames: the sum of its hosted slots'. A quarantined slot counts 0, whatever a
    /// lying child reported.
    pub fn latency(&self) -> u32 {
        self.slots
            .iter()
            .filter_map(|s| s.plugin.as_ref())
            .map(|p| p.latency())
            .fold(0u32, u32::saturating_add)
    }
}

fn answer_ping(r: &Region) {
    let p = r.ping().load(Ordering::Acquire);
    if p != 0 && r.pong().load(Ordering::Relaxed) != p {
        r.pong().store(p, Ordering::Release);
    }
}

/// The arena owner of this child's blocks (one constant, `summoner_proc::RT_OWNER`).
pub use summoner_proc::RT_OWNER;

pub use summoner_proc::RtVec;

/// The chain thread's working set, all of it in runecraft-rt's wired arena (one wired arena for the realtime path):
/// built on the message thread at `Chain::start` and moved into the thread, which only borrows it.
struct Scratch {
    bufs: runecraft_rt::arena::ArenaSlice<runecraft_rt::arena::ArenaSlice<f32>>,
    /// the bypass signal of the slot being processed (its delayed input, or silence).
    byp: runecraft_rt::arena::ArenaSlice<runecraft_rt::arena::ArenaSlice<f32>>,
    midi: RtVec<MidiEvent>,
    /// `--stats`: (hub value, wake-to-run ticks) pairs.
    stats: RtVec<u64>,
    /// This chain thread's diagnostic RT ring (made on the message thread before the thread starts).
    rt: crate::diag::RtLog,
    /// Each slot's enabled fader (set from the slots' flags when the thread starts).
    faders: runecraft_rt::arena::ArenaSlice<Fader>,
    /// The blocks processed (prepared and run), for `--test-slot-enabled`.
    #[cfg(feature = "test-hooks")]
    blocks: u64,
}

impl Scratch {
    /// The working set (message thread): `nbuf` channel buffers and as many bypass buffers of `mf` frames, the MIDI
    /// buffer, the `--stats` pairs, the slots' faders. The arena's chunks are wired (and prefaulted where a wire is
    /// refused, counted), so no store in a block faults. The values are zeros: no rendered bit changes.
    fn new(
        mf: usize,
        nbuf: usize,
        midi: usize,
        stats: bool,
        rt: crate::diag::RtLog,
        faders: impl Iterator<Item = Fader>,
    ) -> Scratch {
        use runecraft_rt::arena::ArenaSlice;
        let faders: Vec<Fader> = faders.collect();
        Scratch {
            bufs: ArenaSlice::from_fn(RT_OWNER, nbuf, |_| ArenaSlice::filled(RT_OWNER, mf, 0.0f32)),
            byp: ArenaSlice::from_fn(RT_OWNER, nbuf, |_| ArenaSlice::filled(RT_OWNER, mf, 0.0f32)),
            midi: RtVec::new(midi, MidiEvent::default()),
            stats: RtVec::new(if stats { 1 << 20 } else { 0 }, 0u64),
            rt,
            faders: ArenaSlice::from_slice(RT_OWNER, &faders),
            #[cfg(feature = "test-hooks")]
            blocks: 0,
        }
    }
}

// Everything the chain thread holds is wired memory or plain values (`RtMem`), proved at compile time.
const _: () = {
    use runecraft_rt::arena::{ArenaSlice, assert_rt_mem};
    assert_rt_mem::<ArenaSlice<ArenaSlice<f32>>>();
    assert_rt_mem::<ArenaSlice<Fader>>();
    assert_rt_mem::<ArenaSlice<Io>>();
    assert_rt_mem::<ArenaSlice<u64>>();
    assert_rt_mem::<u64>();
    assert_rt_mem::<RtVec<MidiEvent>>();
    assert_rt_mem::<RtVec<u64>>();
};

fn run(c: &Chain, mut s: Scratch) -> Vec<u64> {
    // Every thread-local the block path reads is touched here, before the loop, so dyld's
    // TLV allocation of this thread's block never happens inside a block.
    runecraft_rt::alloc_guard::prime_thread();
    // The top of this thread's own stack wired before it enters realtime,
    // counted; the guard is dropped on this thread, when `run` returns.
    let stack = runecraft_rt::os::wire_stack_top(STACK_WIRE_BYTES);
    match &stack {
        Ok(w) => c.stack_wired.store(w.len() as u64, Ordering::Relaxed),
        Err(e) => {
            c.stack_unwired.store(e.bytes as u64, Ordering::Relaxed);
            eprintln!(
                "[summoner-host] chain {}: the stack's top was not wired ({e:?}); it runs unwired",
                c.key
            );
        }
    }
    c.stack_tried.store(true, Ordering::Release);
    #[cfg(feature = "test-hooks")]
    if STACK_WATER.load(Ordering::Relaxed) {
        let mut out = Vec::new();
        let w = runecraft_rt::os::stack_high_water(|| out = run_loop(c, &mut s));
        c.stack_used
            .store(w.map_or(0, |w| w.used as u64), Ordering::Relaxed);
        drop(stack);
        return out;
    }
    let out = run_loop(c, &mut s);
    drop(stack);
    out
}

fn run_loop(c: &Chain, s: &mut Scratch) -> Vec<u64> {
    if c.render {
        // The QoS table: render sets run user-initiated, never under a time constraint.
        runecraft_rt::os::set_thread_qos_user_initiated();
    } else if let Some(us) = c.tc_us {
        let period = if us > 0 {
            Duration::from_micros(us)
        } else {
            Duration::from_secs_f64(c.geo.max_frames as f64 / c.rate)
        };
        if !period.is_zero() {
            runecraft_ipc::bridge::set_time_constraint(period, period / 2, period);
        }
    }
    let r = &c.region;
    let mut done = r.ack().load(Ordering::Acquire);
    let mut woke = false;
    loop {
        // The hub switch: group, then its hub, then seq, every iteration.
        let g = (r.group().load(Ordering::Acquire) as usize).min(RING_GROUPS - 1);
        let h = c.page.hub(g).load(Ordering::Acquire);
        let seq = r.seq().load(Ordering::Acquire);
        answer_ping(r);
        if c.stop.load(Ordering::SeqCst) {
            return s.stats.to_vec();
        }
        if seq > done {
            if woke && c.stats {
                // Wake-to-run against the group's ring stamp (the hub line's diagnostic word).
                let t = runecraft_ipc::bridge::host_ticks();
                let stamp = c
                    .page
                    .word64(P_HUB[g] + P_RING_STAMP)
                    .load(Ordering::Relaxed);
                if stamp != 0 && t > stamp && s.stats.len() + 2 <= s.stats.capacity() {
                    s.stats.push(h);
                    s.stats.push(t - stamp);
                }
            }
            woke = false;
            for n in done + 1..=seq {
                process_seq(c, s, n);
            }
            done = seq;
            continue;
        }
        if seq < done {
            done = seq; // a reset seq: follow it
        }
        // a parked chain and a render set wait on their own seq, never on a hub.
        let waited = if c.render || c.parked.load(Ordering::SeqCst) {
            r.seq_waker()
                .wait(seq, Some(Instant::now() + Duration::from_millis(250)))
        } else {
            c.fan.wait(
                c.page.hub(g),
                h,
                Some(Instant::now() + Duration::from_millis(250)),
            )
        };
        woke = waited == Waited::Woken;
        if woke && r.seq().load(Ordering::Acquire) == seq {
            c.page
                .word64(P_SPURIOUS_WAKES)
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn process_seq(c: &Chain, s: &mut Scratch, n: u64) {
    // `--noalloc-blocks`: the counter armed around this whole block, ack included (the first blocks only).
    let counting = c
        .noalloc
        .as_ref()
        .filter(|(_, max)| c.noalloc_counted.load(Ordering::Relaxed) < *max)
        .map(|(k, _)| {
            k.arm();
            k
        });
    process_block(c, s, n);
    if let Some(k) = counting {
        let a = k.disarm();
        c.noalloc_allocs.fetch_add(a, Ordering::Relaxed);
        c.noalloc_counted.fetch_add(1, Ordering::Relaxed);
    }
}

/// `--cfg mutant_io_rule` (never in a normal build): `SUMMONER_MUTANT_IO=stage_before_line` applies the slot channel
/// rule before the bypass line's write instead of after it (a call-site mutant). The rule's own five branches are
/// source-patch mutants of the shared `summoner_plugin::channels`, kept outside this repository.
#[cfg(mutant_io_rule)]
fn io_mutant(name: &str) -> bool {
    static M: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    M.get_or_init(|| std::env::var("SUMMONER_MUTANT_IO").unwrap_or_default()) == name
}

#[cfg(not(mutant_io_rule))]
#[inline(always)]
fn io_mutant(_: &str) -> bool {
    false
}

fn process_block(c: &Chain, s: &mut Scratch, n: u64) {
    // The block's own time (R_PROCESS_NS and the histogram) on the host clock: `mach_absolute_time`, a counter read,
    // converted once at the end. `Instant` went through `clock_gettime`'s conversion twice per block (measured:
    // the largest share of this child's own work per block).
    let t0 = runecraft_ipc::bridge::host_ticks();
    let r = &c.region;
    let geo = &c.geo;
    let slot = r.slot(n);
    let frames = (slot.u32(S_FRAMES).load(Ordering::Relaxed) as usize).min(geo.max_frames as usize);
    let flags = slot.u32(S_BLOCK_FLAGS).load(Ordering::Relaxed);
    let mut status = ST_PROCESSED;

    // Parameters through the region, before the block: `id = slot << 24 | index`.
    let np = slot
        .u32(S_PARAM_COUNT)
        .load(Ordering::Relaxed)
        .min(geo.max_params);
    for i in 0..np as usize {
        let rec = slot.param(i);
        let id = rec[0].load(Ordering::Relaxed);
        let (k, idx) = ((id >> 24) as usize, id & 0xff_ffff);
        if let Some(p) = c.slots.get(k).and_then(|e| e.plugin.as_ref()) {
            let v = f32::from_bits(rec[1].load(Ordering::Relaxed));
            if idx < p.param_count() {
                p.set_param(idx, v);
                if let Some(e) = c.slots.get(k) {
                    e.param_writes.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                // The test double's hidden parameters (the host application's test double 4 / 5), test builds only.
                #[cfg(feature = "test-hooks")]
                if let Some(t) = p.test_hooks() {
                    let _ = t.test_param(idx, v);
                }
            }
        }
    }

    // Input plane -> the block's buffers (the rest zero).
    for (ch, b) in s.bufs.iter_mut().enumerate() {
        if (ch as u32) < geo.ch_in {
            for (x, a) in b.iter_mut().zip(slot.input(ch)).take(frames) {
                *x = f32::from_bits(a.load(Ordering::Relaxed));
            }
        } else {
            b[..frames].fill(0.0);
        }
    }

    // MIDI plane -> the block's events, offsets clamped into the block, in offset order (stable). A reset is all
    // notes off on every channel at frame 0, ahead of the block's own events.
    s.midi.clear();
    if flags & BF_RESET != 0 {
        for ch in 0..16u8 {
            s.midi.push(MidiEvent::short(0, [0xb0 | ch, 123, 0]));
        }
    }
    let nm = slot
        .u32(S_MIDI_COUNT)
        .load(Ordering::Relaxed)
        .min(geo.max_midi) as usize;
    for i in 0..nm {
        let rec = slot.midi(i);
        let len = rec[1].load(Ordering::Relaxed).min(16) as u8;
        if len == 0 {
            continue;
        }
        let mut data = [0u8; 16];
        for (j, w) in rec[2..6].iter().enumerate() {
            data[j * 4..j * 4 + 4].copy_from_slice(&w.load(Ordering::Relaxed).to_le_bytes());
        }
        let offset = rec[0]
            .load(Ordering::Relaxed)
            .min((frames as u32).saturating_sub(1));
        s.midi.push(MidiEvent { offset, len, data });
    }
    // Stable and in place (insertion): the list is short and mostly ordered; `sort_by_key` would allocate.
    for i in 1..s.midi.len() {
        let mut j = i;
        while j > 0 && s.midi[j - 1].offset > s.midi[j].offset {
            s.midi.swap(j - 1, j);
            j -= 1;
        }
    }

    // The transport.
    let f64at = |o: usize| f64::from_bits(slot.u64(o).load(Ordering::Relaxed));
    let host_time = slot.u64(S_OUTPUT_HOST_TIME).load(Ordering::Relaxed);
    let transport = Transport {
        ppq: f64at(S_PPQ),
        bpm: f64at(S_BPM),
        time_sig: slot.u32(S_TIME_SIG).load(Ordering::Relaxed),
        loop_start_ppq: f64at(S_LOOP_START_PPQ),
        loop_end_ppq: f64at(S_LOOP_END_PPQ),
        bar_start_ppq: f64at(S_BAR_START_PPQ),
        flags,
        time_seconds: f64at(S_TIME_SECONDS),
        time_samples: slot.u64(S_TIME_SAMPLES).load(Ordering::Relaxed),
        output_host_time: host_time,
        output_host_time_ns: if host_time != 0 {
            runecraft_ipc::bridge::ticks_to_ns(host_time)
        } else {
            0
        },
    };

    let mut procs = c.procs.try_lock();
    #[allow(unused_mut)]
    let mut run = frames > 0 && procs.as_ref().is_some_and(|p| p.prepared);
    // The test double's skipped block (the host application's test double parameter 4: acked SKIPPED and silent), test
    // builds only.
    #[cfg(feature = "test-hooks")]
    if run
        && c.slots.iter().any(|e| {
            e.plugin
                .as_ref()
                .and_then(|p| p.test_hooks())
                .is_some_and(|t| t.test_skip())
        })
    {
        run = false;
    }
    if !run {
        if frames > 0 {
            status = ST_SKIPPED; // prepare or release holds the chain: a silent, acked block
        }
        for b in s.bufs.iter_mut() {
            b[..frames].fill(0.0);
        }
    } else {
        // FZ around the block, restored after it.
        let _fz = runecraft_rt::fpu::FzScope::enter();
        let pr = &mut **procs.as_mut().expect("held: run");
        let (cells, lines, sides, ios) =
            (pr.cells.as_deref(), &mut pr.lines, &mut pr.sides, &pr.ios);
        #[cfg(feature = "test-hooks")]
        {
            if let Some(t) = TEST_SLOT_ENABLED.get() {
                for &(slot, on, at) in t {
                    if at == s.blocks
                        && at > 0
                        && let Some(e) = c.slots.get(slot as usize)
                    {
                        e.enabled.store(on, Ordering::Release);
                    }
                }
            }
            s.blocks += 1;
        }
        for (k, entry) in c.slots.iter().enumerate() {
            // A quarantined or unhosted slot passes the audio through and keeps its number, and never sets
            // ST_BYPASSED.
            let (Some(plugin), Some(cell)) = (entry.plugin.as_ref(), cells.and_then(|v| v.get(k)))
            else {
                continue;
            };
            // The handover word decides whether the plugin runs and how its output mixes with the bypass signal.
            let mut begun = sides[k].begin(cell);
            // The enabled fader acts only while the handover's mix is the plugin's alone: a re-activation handover in
            // progress on this slot wins (its envelope already bypasses), and the fader holds still meanwhile.
            let enabled = entry.enabled.load(Ordering::Acquire);
            let fader = s.faders[k];
            let (mix, fading) = if begun.mix == handover::Mix::PLUGIN {
                (fader.mix(enabled), true)
            } else {
                (begun.mix, false)
            };
            // Disabled and faded out: the plugin is not called at latency 0; above 0 it is called and its output
            // discarded.
            let call = !(fading && !enabled && fader.xf == handover::FADE && plugin.latency() == 0);
            // The bypass signal: an effect's input delayed by its latency, written every block before the plugin is
            // called (so it is primed whenever a bypass begins); an instrument's silence.
            // Read only when the mix uses it: `Mix::PLUGIN` (the plugin's output alone, every block but a handover's)
            // never reads the bypass signal (`Mix::apply` returns at once) and never counts it misaligned (only a
            // bypassed block does). The write is every block either way: the line stays primed (measured: the
            // read's per-sample modulo was half of this child's own work per block).
            let uses_bypass = mix != handover::Mix::PLUGIN;
            if io_mutant("stage_before_line") && call {
                let io = ios.get(k).copied().unwrap_or_default();
                summoner_plugin::channels::channel_rule_before(
                    io.inputs,
                    io.outputs,
                    &mut s.bufs[..],
                    frames,
                );
            }
            match lines.get_mut(k).and_then(Option::as_mut) {
                Some(line) => {
                    for (ch, b) in s.bufs.iter().enumerate() {
                        line.write(ch, &b[..frames]);
                    }
                    if uses_bypass {
                        let l = plugin.latency() as usize;
                        let mut aligned = true;
                        for (ch, b) in s.byp.iter_mut().enumerate() {
                            aligned &= line.read(ch, l, &mut b[..frames]);
                        }
                        if !aligned && begun.mix.bypassed() {
                            entry.misaligned.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    line.advance(frames);
                }
                None if uses_bypass => {
                    for b in s.byp.iter_mut() {
                        b[..frames].fill(0.0);
                    }
                }
                None => {}
            }
            if let Some(d) = begun
                .guard
                .as_deref_mut()
                .and_then(|g| g.as_mut())
                .filter(|_| call)
            {
                entry.calls.fetch_add(1, Ordering::Relaxed);
                r.in_slot().store(k as u32 + 1, Ordering::Release);
                // The slot channel rule around the call, on the slot's buses as cached at prepare. After the
                // bypass line's write above (it keeps the full input) and before the bypass mix below (the mix
                // blends the rule's output); a slot that is not called is untouched.
                let io = ios.get(k).copied().unwrap_or_default();
                summoner_plugin::channels::channel_rule_before(
                    io.inputs,
                    io.outputs,
                    &mut s.bufs[..],
                    frames,
                );
                // A failed block (false) or a Rust panic: THREW.
                let ok = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    if flags & BF_RESET != 0 {
                        d.reset();
                    }
                    d.process(&mut Block {
                        frames,
                        channels: &mut s.bufs[..],
                        midi: &s.midi,
                        transport: &transport,
                    })
                }))
                .unwrap_or(false);
                summoner_plugin::channels::channel_rule_after(
                    io.inputs,
                    io.outputs,
                    &mut s.bufs[..],
                    frames,
                );
                if !ok {
                    status |= ST_THREW;
                }
                r.in_slot().store(0, Ordering::Release);
            }
            for (out, b) in s.bufs.iter_mut().zip(s.byp.iter()) {
                mix.apply(&c.fade, &mut out[..frames], &b[..frames]);
            }
            if fading {
                s.faders[k].advance(mix, frames);
            }
            // The processor is released before the chain thread's CAS (FADING -> OUT, RETURNING -> HOSTED).
            if sides[k].end(cell, frames, begun) {
                status |= ST_BYPASSED;
                entry.bypassed.fetch_add(1, Ordering::Relaxed);
                entry.bypassed_total.fetch_add(1, Ordering::Relaxed);
            }
        }
        if status & ST_THREW != 0 {
            for b in s.bufs.iter_mut() {
                b[..frames].fill(0.0);
            }
        }
    }
    drop(procs);

    // The buffers -> the output plane, then the response line, then ack (Release).
    for ch in 0..geo.ch_out as usize {
        for (o, x) in slot.output(ch).iter().zip(s.bufs[ch].iter()).take(frames) {
            o.store(x.to_bits(), Ordering::Relaxed);
        }
    }
    let ticks = runecraft_ipc::bridge::host_ticks().saturating_sub(t0);
    let ns = runecraft_ipc::bridge::ticks_to_ns(ticks);
    let period_ns = (frames.max(1) as f64 / c.rate * 1e9) as u64;
    // RT records through the host application's log ring, no formatting or allocation (a disabled level costs one
    // relaxed load). The host application's rows for the child, with the chain's key: `chain.block` (trace) for every
    // block and `chain.slow` (info) for a block over its period, so the default (every area at warn) writes nothing
    // new.
    s.rt.block(ticks, frames as u64, n);
    if ns > period_ns {
        // The period in host ticks, at this block's own tick-to-ns ratio (the FFI crate converts only the other way).
        let period_ticks =
            (u128::from(period_ns) * u128::from(ticks) / u128::from(ns.max(1))) as u64;
        s.rt.slow(ticks, period_ticks, frames as u64, n);
    }
    let bucket = ((ns * 20) / period_ns.max(1)).min(HIST_BUCKETS as u64 - 1) as usize;
    let hist = &r.hist()[bucket];
    hist.store(hist.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    slot.u64(R_ECHO).store(n, Ordering::Relaxed);
    slot.u32(R_STATUS).store(status, Ordering::Relaxed);
    // Protocol 3.1: +140 is reserved, written 0, never read (PDC moves only through chain.loaded and
    // latency.changed).
    slot.u32(R_RESERVED_LATENCY).store(0, Ordering::Relaxed);
    slot.u32(R_PROCESS_NS)
        .store(ns.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    r.processed().fetch_add(1, Ordering::Relaxed);
    r.ack().store(n, Ordering::Release);
    if c.render {
        // the render worker waits on ack (publish, wake seq, wait ack); only render sets wake it.
        let _ = r.ack_waker().wake();
    }
    answer_ping(r);
}

#[cfg(test)]
mod prefault_tests {
    use super::Scratch;

    /// One wired arena for the realtime path (applied to this child's chain thread): every buffer of the chain's
    /// working set lies in the host application's arena, in a wired chunk or large region (`RtArena::contains` says
    /// `Some(true)`), and storing into it takes no page fault. The control: a fresh, untouched heap buffer of the same
    /// size faults (else the instrument, `runecraft_rt::os::faults_storing`, proves nothing). The fault counter is
    /// process-wide and other tests run in parallel, so the bound is a fraction of the pages, not zero.
    #[test]
    fn the_chain_scratch_is_wired_arena_memory() {
        let arena = runecraft_rt::arena::RtArena::global();
        arena.init();
        let mf = 1 << 20; // 4 MiB per buffer: the arena's large-object path
        let pages = (mf * 4 / 16_384) as u64;
        let mut fresh = vec![0.0f32; mf];
        let control = runecraft_rt::os::faults_storing(&mut fresh, 0.0);
        assert!(
            control >= pages / 2,
            "the control faulted only {control} of {pages} pages"
        );
        let mut s = Scratch::new(
            mf,
            2,
            4096,
            true,
            crate::diag::RtLog::none(),
            std::iter::empty(),
        );
        for b in s.bufs.iter_mut().chain(s.byp.iter_mut()) {
            let wired = arena.contains(b.as_ptr().cast(), b.len() * 4);
            assert_eq!(wired, Some(true), "a scratch buffer in a wired arena block");
            let f = runecraft_rt::os::faults_storing(b, 0.0);
            assert!(
                f <= pages / 8,
                "{f} faults storing into a scratch buffer of {pages} pages"
            );
            assert!(b.iter().all(|x| *x == 0.0), "still zeros");
        }
        // The pushed-into buffers: their capacity is wired too, and a push faults nothing.
        let cap = s.stats.capacity();
        let before = runecraft_rt::os::minor_faults();
        for _ in 0..cap {
            assert!(s.stats.push(7u64));
        }
        assert!(!s.stats.push(7u64), "full: refused, never grown");
        let pushed = runecraft_rt::os::minor_faults() - before;
        assert!(
            pushed <= (cap as u64 * 8 / 16_384) / 8,
            "{pushed} faults pushing {cap} stats"
        );
    }
}

#[cfg(test)]
mod io_rule_tests {
    //! [`stage_inputs`] / [`finish_outputs`] against the expected path, modelled literally: an external-plugin node
    //! on a 2-wide track buffer, the child's staging and copy-back, and the proxy's pop into the track buffer.
    //! Plain `Vec` buffers (the rule is generic over the block's buffer type): no arena traffic here, so the
    //! parallel `prefault_tests` control keeps faulting fresh pages.
    use super::Io;

    // The slot channel rule is the shared `summoner_plugin::channels`; these tests keep their bodies through two local
    // adapters taking the slot's `Io`.
    fn stage_inputs<B: std::ops::DerefMut<Target = [f32]>>(io: Io, bufs: &mut [B], frames: usize) {
        summoner_plugin::channels::channel_rule_before(io.inputs, io.outputs, bufs, frames);
    }
    fn finish_outputs<B: std::ops::DerefMut<Target = [f32]>>(
        io: Io,
        bufs: &mut [B],
        frames: usize,
    ) {
        summoner_plugin::channels::channel_rule_after(io.inputs, io.outputs, bufs, frames);
    }

    const L: [f32; 4] = [0.75, 0.1, -0.3, 1e-3];
    const R: [f32; 4] = [-0.25, 0.7, 0.2, -0.6];

    type Plug = fn(&mut [Vec<f32>], usize);

    /// A plugin that leaves its buffer as it is.
    fn identity(_: &mut [Vec<f32>], _: usize) {}
    /// A plugin that adds its own signal (10, 20) to each of its outputs, at most the block's two.
    fn adds(b: &mut [Vec<f32>], outputs: usize) {
        for (c, ch) in b.iter_mut().enumerate().take(outputs.min(2)) {
            for x in ch.iter_mut() {
                *x += 10.0 * (c + 1) as f32;
            }
        }
    }
    /// A plugin that writes 0.5 into channel 0 only (when it has an output).
    fn ch0_only(b: &mut [Vec<f32>], outputs: usize) {
        if outputs > 0 {
            b[0].fill(0.5);
        }
    }

    /// The expected path, literally, for `inputs` / `outputs` and the track's `[L, R]`.
    fn cpp(inputs: usize, outputs: usize, plugin: Plug) -> [Vec<f32>; 2] {
        let n = L.len();
        let mut dest = [L.to_vec(), R.to_vec()];
        // The child: scratch of max(in, out, 2), chIn = inputs staged, the rest cleared; process; chOut = outputs
        // copied back; the pop fills the proxy's buffer from them, every channel past them zero.
        let child = |buf: &mut Vec<Vec<f32>>| {
            let total = inputs.max(outputs).max(2);
            let mut scratch: Vec<Vec<f32>> = (0..total)
                .map(|ch| {
                    if ch < inputs && ch < buf.len() {
                        buf[ch].clone()
                    } else {
                        vec![0.0; n]
                    }
                })
                .collect();
            plugin(&mut scratch, outputs);
            for (ch, b) in buf.iter_mut().enumerate() {
                *b = if ch < outputs {
                    scratch[ch].clone()
                } else {
                    vec![0.0; n]
                };
            }
        };
        let p = inputs.max(1).max(outputs);
        if p == 2 {
            let mut d = dest.to_vec();
            child(&mut d);
            dest = [d[0].clone(), d[1].clone()];
        } else {
            let mut buf: Vec<Vec<f32>> = (0..p)
                .map(|i| if i < 2 { dest[i].clone() } else { vec![0.0; n] })
                .collect();
            if inputs == 1 {
                for j in 0..n {
                    buf[0][j] += dest[1][j];
                    buf[0][j] *= 0.5;
                }
            }
            child(&mut buf);
            for (i, d) in dest.iter_mut().enumerate() {
                *d = if i < p {
                    buf[i].clone()
                } else if p > 0 {
                    buf[0].clone()
                } else {
                    vec![0.0; n]
                };
            }
        }
        dest
    }

    /// This child's path: the rule around the same plugin on the 2-wide block.
    fn ours(inputs: u32, outputs: u32, plugin: Plug) -> [Vec<f32>; 2] {
        let io = Io::new(inputs, outputs);
        let mut bufs = [L.to_vec(), R.to_vec()];
        stage_inputs(io, &mut bufs, L.len());
        let mut v: Vec<Vec<f32>> = bufs.iter().map(|b| b.to_vec()).collect();
        plugin(&mut v, outputs as usize);
        for (b, x) in bufs.iter_mut().zip(&v) {
            b.copy_from_slice(x);
        }
        finish_outputs(io, &mut bufs, L.len());
        [bufs[0].to_vec(), bufs[1].to_vec()]
    }

    fn bits(v: &[Vec<f32>; 2]) -> [Vec<u32>; 2] {
        v.clone().map(|c| c.iter().map(|x| x.to_bits()).collect())
    }

    /// Five reviewed cases against the expected samples, written out (the identity plugin: what reaches the
    /// next slot is what the rule left).
    #[test]
    fn the_five_cases_give_cpps_samples() {
        let z = vec![0.0f32; L.len()];
        let avg: Vec<f32> = L.iter().zip(&R).map(|(l, r)| (l + r) * 0.5).collect();
        let cases: [(&str, u32, u32, [Vec<f32>; 2]); 6] = [
            ("in0/out1", 0, 1, [z.clone(), z.clone()]),
            ("in1/out2", 1, 2, [L.to_vec(), z.clone()]),
            ("in2/out1", 2, 1, [L.to_vec(), z.clone()]),
            ("in1/out1", 1, 1, [avg.clone(), avg.clone()]),
            ("in2/out0", 2, 0, [z.clone(), z.clone()]),
            ("in2/out2", 2, 2, [L.to_vec(), R.to_vec()]),
        ];
        for (what, i, o, want) in cases {
            let got = ours(i, o, identity);
            assert_eq!(bits(&got), bits(&want), "{what}: {got:?}");
            assert_eq!(
                bits(&cpp(i as usize, o as usize, identity)),
                bits(&want),
                "{what}: the C++ model agrees with the table"
            );
        }
        // A 0-input mono-out instrument's own signal is duplicated (in0/out1 with a writer).
        let got = ours(0, 1, adds);
        assert_eq!(got, [vec![10.0; L.len()], vec![10.0; L.len()]]);
    }

    /// Every bus shape from 0 to 4 each way (and 6, 8), three plugins, bit for bit against the model (in1 with
    /// more than two outputs averages too: the host's branch for a channel count unlike the plugin's).
    #[test]
    fn every_shape_matches_the_cpp_model() {
        for (name, plugin) in [
            ("identity", identity as Plug),
            ("adds", adds),
            ("ch0_only", ch0_only),
        ] {
            // The wide buses a VST3 keeps when it refuses stereo (6 and 8) too.
            for i in [0u32, 1, 2, 3, 4, 6, 8] {
                for o in [0u32, 1, 2, 3, 4, 6, 8] {
                    assert_eq!(
                        bits(&ours(i, o, plugin)),
                        bits(&cpp(i as usize, o as usize, plugin)),
                        "{name} in{i}/out{o}"
                    );
                }
            }
        }
    }

    /// A 0-input instrument fed the chain's input hears none of it: whatever it writes (nothing, channel 0 only, or
    /// accumulating), the output is zeros or only its own signal.
    #[test]
    fn a_zero_input_instrument_never_passes_the_input_through() {
        for o in [1u32, 2] {
            let none = ours(0, o, identity);
            assert!(none.iter().flatten().all(|x| *x == 0.0), "out{o} none");
            let add = ours(0, o, adds);
            let own = if o == 1 { [10.0, 10.0] } else { [10.0, 20.0] };
            for (c, ch) in add.iter().enumerate() {
                assert!(ch.iter().all(|x| *x == own[c]), "out{o} add: {add:?}");
            }
            let one = ours(0, o, ch0_only);
            assert!(one[0].iter().all(|x| *x == 0.5), "out{o} ch0");
            let ch1 = if o == 1 { 0.5 } else { 0.0 };
            assert!(one[1].iter().all(|x| *x == ch1), "out{o} ch0: {one:?}");
        }
    }

    /// A mono-input effect on a stereo chain does not see the right channel: it is zero before its process.
    #[test]
    fn a_mono_input_effect_sees_channel_one_zero() {
        let io = Io::new(1, 2);
        let mut bufs = [L.to_vec(), R.to_vec()];
        stage_inputs(io, &mut bufs, L.len());
        assert_eq!(&bufs[0][..], &L[..], "channel 0 as it was");
        assert!(bufs[1].iter().all(|x| *x == 0.0), "channel 1 zeroed");
    }

    /// Only the block's frames are touched: a short block leaves the buffers' tail as it was.
    #[test]
    fn only_the_blocks_frames_are_touched() {
        let io = Io::new(0, 1);
        let mut bufs = [L.to_vec(), R.to_vec()];
        stage_inputs(io, &mut bufs, 2);
        finish_outputs(io, &mut bufs, 2);
        assert_eq!(&bufs[0][2..], &L[2..]);
        assert_eq!(&bufs[1][2..], &R[2..]);
    }
    /// The slot channel rule's truth table (the same data as the host application's fake's `io_rule_tests`): the
    /// whole truth table, as data, for every bus shape in {0, 1, 2, 3, 4, 6, 8} each way. Columns: inputs, outputs,
    /// what the plugin sees on channels 0 and 1, what the next slot gets after a plugin that writes nothing, and after
    /// one that writes 5 to channel 0 and 7 to channel 1 (each only when it has that output). Symbols: L and R the
    /// block's channels, A = (L + R) * 0.5 in f32, Z zero.
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
            stage_inputs(Io::new(i, o), &mut b, 3);
            let seen = [b[0][0], b[1][0]];
            assert_eq!(
                seen.map(f32::to_bits),
                want(2).map(f32::to_bits),
                "{row}: seen"
            );
            let mut id = b.clone();
            finish_outputs(Io::new(i, o), &mut id, 3);
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
            finish_outputs(Io::new(i, o), &mut w, 3);
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
            stage_inputs(Io::new(i, o), &mut b, frames);
            eat(&mut h, &b);
            finish_outputs(Io::new(i, o), &mut b, frames);
            eat(&mut h, &b);
        }
        assert_eq!(h, 0xAA93032B0F163152, "digest (seeded blocks)");
    }

    /// A clean-room pin on this (old) code: block widths 0..=4, three plugins between the steps. SplitMix64 seed
    /// 0x5EED_C019, 4096 blocks; per block, drawn in this order: inputs and outputs from {0, 1, 2, 3, 4, 6, 8}, the
    /// width `next % 5`, `frames` = `next % 7`, the plugin `next % 3` (0 writes nothing; 1 fills channel 0 with 0.5
    /// when it has an output; 2 adds 10 * (c + 1) to each channel c below min(outputs, width)), then each channel's 6
    /// samples (`sample`). FNV-1a 64 over every sample's bits (a NaN as 0x7FC0_0000) after the before step and after
    /// the after step.
    #[test]
    fn the_seeded_width_digest_is_todays() {
        const SHAPES: [u32; 7] = [0, 1, 2, 3, 4, 6, 8];
        let mut s = 0x5EED_C019u64;
        let mut h = 0xCBF2_9CE4_8422_2325u64;
        let eat = |h: &mut u64, b: &[Vec<f32>]| {
            for x in b.iter().flatten() {
                let bits = if x.is_nan() { 0x7FC0_0000 } else { x.to_bits() };
                for byte in bits.to_le_bytes() {
                    *h ^= u64::from(byte);
                    *h = h.wrapping_mul(0x0000_0100_0000_01B3);
                }
            }
        };
        for _ in 0..4096 {
            let i = SHAPES[(splitmix(&mut s) % 7) as usize];
            let o = SHAPES[(splitmix(&mut s) % 7) as usize];
            let width = (splitmix(&mut s) % 5) as usize;
            let frames = (splitmix(&mut s) % 7) as usize;
            let plugin = splitmix(&mut s) % 3;
            let mut b: Vec<Vec<f32>> = (0..width)
                .map(|_| (0..6).map(|_| sample(&mut s)).collect())
                .collect();
            let io = Io::new(i, o);
            stage_inputs(io, &mut b, frames);
            eat(&mut h, &b);
            match plugin {
                1 if o > 0 && width > 0 => b[0][..frames].fill(0.5),
                2 => {
                    for (c, ch) in b.iter_mut().enumerate().take((o as usize).min(width)) {
                        for x in ch[..frames].iter_mut() {
                            *x += 10.0 * (c + 1) as f32;
                        }
                    }
                }
                _ => {}
            }
            finish_outputs(io, &mut b, frames);
            eat(&mut h, &b);
        }
        assert_eq!(h, 0xB6100A1FD3594882, "digest (seeded widths)");
    }
}
