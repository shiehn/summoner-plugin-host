//! The callback's side of a bridged chain: collect, push into the primed FIFO, publish, pop;
//! the miss and the breaker; the MIDI carry; the parameter changes; the ring of a group. N1: no allocation, lock or
//! syscall here except [`ring_group`]'s one `os_sync_wake_by_address_all` (the crate's clippy list binds this module;
//! the graph's N1 tests run it inside the allocator guard and the interposer).
//!
//! The node runs at its schedule position, once or twice per callback block (twice across a mid-block switch): the
//! first visit decides the block (collect, breaker), every visit stages its share of the input and pops its share of
//! the output, and the visit that ends the block publishes. So a mid-block switch never splits the exchange.

use crate::midi::{Event3, MidiCarry, Offered};
use crate::region::{ChildPage, Region};
use crate::sync::{AtomicU32, Ordering};
use std::sync::Arc;
use summoner_proto::layout::*;
// The musical clock's pure arithmetic lives in orrery-time; re-exported here so every caller keeps its path.
pub use orrery_time::{Tempo, TimedMidi as MidiEvent, Transport, chase_frames};
// The counters are the callback's own (never shared with the child): std atomics, also under loom.
use std::sync::atomic::AtomicU64;

/// The audio-section counters (the callback is the only writer; Relaxed),
/// plus three diagnostics that are not ABI.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct AudioCounters {
    /// `bridgeLate`: blocks whose ack was not there at collect (a miss), and blocks waited out while the breaker is
    /// open.
    pub late: AtomicU64,
    /// `bridgeSilenced`: blocks played as a silence ramp (a miss with no dry signal, or a non-finite block).
    pub silenced: AtomicU64,
    /// `bridgeDryPassed`: missed blocks played as the dry signal.
    pub dry_passed: AtomicU64,
    /// `bridgeBreakerOpens`: times a chain's breaker opened.
    pub breaker_opens: AtomicU64,
    /// `bridgeDoorbellErrors`: doorbell rings that failed.
    pub doorbell_errors: AtomicU64,
    /// `bridgeMidiCarried`: MIDI releases carried while the breaker was open.
    pub midi_carried: AtomicU64,
    /// `bridgeMidiDropped`: MIDI events dropped while the breaker was open (never a release).
    pub midi_dropped: AtomicU64,
    /// `bridgeMidiAllNotesOff`: CC 123s that replaced a channel's carried note-offs past the cap.
    pub midi_all_notes_off: AtomicU64,
    /// `bridgeNonFinite`: blocks the child returned with a non-finite sample (played as silence).
    pub non_finite: AtomicU64,
    /// `bridgeParamDeferred`: parameter changes deferred to a later block (the plane was full).
    pub param_deferred: AtomicU64,
    /// `bridgeSlotBypassed` (audio telemetry offset 1024): chain-blocks collected with `R_STATUS` bit 3 (a hosted
    /// slot passed its audio through during a reactivation). Not a miss: the block plays.
    pub slot_bypassed: AtomicU64,
    /// Diagnostics (not in the telemetry layout): ring syscalls made, and the ring time of the last ring and the worst
    /// one, in host ticks (`runecraft_ipc::bridge::ticks_to_ns` converts; the callback never converts).
    pub rings: AtomicU64,
    /// Ring time summed over every ring, in host ticks.
    pub ring_ticks_total: AtomicU64,
    /// The last ring's time, in host ticks.
    pub ring_ticks_last: AtomicU64,
    /// The worst ring's time, in host ticks.
    pub ring_ticks_max: AtomicU64,
    /// Diagnostic, not ABI (not in [`AudioCounters::named`], not in
    /// the telemetry layout): chasing edges skipped past [`MAX_CHASED_EDGES`] in one node visit (a Bridge's piece,
    /// a Sampler's wrap), cumulative. Those edges still released; they did not chase. Read on control
    /// (beside the bridge counters of the host application's play status).
    pub chase_skipped: AtomicU64,
}

#[inline]
fn bump(c: &AtomicU64, by: u64) {
    c.store(
        c.load(Ordering::Relaxed).wrapping_add(by),
        Ordering::Relaxed,
    );
}

impl AudioCounters {
    /// One counter's value (Relaxed).
    pub fn get(c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// The eleven audio counters under their telemetry names (audio section, offsets 936-1015 and 1024, in
    /// that order), as control reads them (Relaxed: each is single-writer, cumulative). Not the diagnostics.
    pub fn named(&self) -> [(&'static str, u64); 11] {
        let g = Self::get;
        [
            ("bridgeLate", g(&self.late)),
            ("bridgeSilenced", g(&self.silenced)),
            ("bridgeDryPassed", g(&self.dry_passed)),
            ("bridgeBreakerOpens", g(&self.breaker_opens)),
            ("bridgeDoorbellErrors", g(&self.doorbell_errors)),
            ("bridgeMidiCarried", g(&self.midi_carried)),
            ("bridgeMidiDropped", g(&self.midi_dropped)),
            ("bridgeMidiAllNotesOff", g(&self.midi_all_notes_off)),
            ("bridgeNonFinite", g(&self.non_finite)),
            ("bridgeParamDeferred", g(&self.param_deferred)),
            ("bridgeSlotBypassed", g(&self.slot_bypassed)),
        ]
    }
}

/// What a Bridge node refers to: immutable, shared as an `Arc` from the Schedule. Control
/// drops it only after the box that held it has retired.
#[derive(Debug)]
#[non_exhaustive]
pub struct ChainLink {
    /// The chain's region (the request and response lines, the planes).
    pub region: Arc<Region>,
    /// The child page (the hub words, the status record).
    pub page: Arc<ChildPage>,
    /// The ring group, as `chain.load` / `chain.unpark` gave it to the child.
    pub group: u8,
    /// Pipeline depth d ∈ {1, 2}.
    pub depth: u8,
    /// The master monitor chain: dry on a miss.
    pub monitor: bool,
    /// Σ of the chain's plugin latencies (`chain.loaded`, `latency.changed`).
    pub plugin_latency: u32,
    /// The plugin parameter id (`slot << 24 | index`) of each model parameter of the node.
    pub param_ids: Arc<[u32]>,
    /// A render set's job. `Some` only on a link a render job made ([`ChainLink::for_render`]);
    /// the graph's offline render then exchanges the node's blocks synchronously through it
    /// ([`crate::render::RenderJob::block`]) instead of this module's pipeline. A live link never has one.
    #[cfg(not(loom))]
    pub render: Option<Arc<crate::render::RenderJob>>,
}

impl ChainLink {
    /// A live chain's link (no render job). The struct is `#[non_exhaustive]`: set the other fields on the value.
    pub fn new(
        region: Arc<Region>,
        page: Arc<ChildPage>,
        group: u8,
        depth: u8,
        monitor: bool,
        plugin_latency: u32,
        param_ids: Arc<[u32]>,
    ) -> ChainLink {
        ChainLink {
            region,
            page,
            group,
            depth,
            monitor,
            plugin_latency,
            param_ids,
            #[cfg(not(loom))]
            render: None,
        }
    }

    /// The same region, page and parameters, with a render job attached (the render set's own link, made by the
    /// job on control).
    #[cfg(not(loom))]
    pub fn for_render(&self, job: Arc<crate::render::RenderJob>) -> ChainLink {
        ChainLink {
            region: self.region.clone(),
            page: self.page.clone(),
            group: self.group,
            depth: self.depth,
            monitor: self.monitor,
            plugin_latency: self.plugin_latency,
            param_ids: self.param_ids.clone(),
            render: Some(job),
        }
    }

    /// The latency reported to PDC: plugin latency + d × B. A render set's (synchronous): the plugin latency
    /// alone.
    pub fn pdc_latency(&self, max_block: usize) -> u32 {
        #[cfg(not(loom))]
        if self.render.is_some() {
            return self.plugin_latency;
        }
        self.plugin_latency + self.depth as u32 * max_block as u32
    }
}

/// One visit of the node in a callback block.
#[derive(Clone, Copy, Debug)]
pub struct Visit {
    /// The callback's block counter (the hub value of this block).
    pub block: u64,
    /// This visit's share of the block: frames [offset, offset + len) of `total`.
    pub offset: usize,
    /// This visit's frame count.
    pub len: usize,
    /// The block's frame count.
    pub total: usize,
    /// The chain's clock (its node's, clock A for a chain with no deck): its position at this
    /// visit's first frame (`offset`), where the events are looked up.
    pub pos: u64,
    /// The clock's position at the block's first frame: the request line's transport (one per block).
    pub block_pos: u64,
    /// The clock's loop length (0 = none), for the event lookup, the looping flag and the loop points.
    pub loop_frames: u64,
    /// Where the clock's wrap lands (the clock description's loop start; 0 without a loop
    /// range): the events after a wrap, the request line's loop start.
    pub loop_start: u64,
    /// The device's sample rate (the request line's transport).
    pub sample_rate: f64,
    /// The clock's tempo and time signature (constant per schedule: the graph has no tempo map yet).
    pub tempo: Tempo,
    /// The clock runs (the request line's bit 0 *playing*; a stopped clock sends no events).
    pub playing: bool,
    /// Release what the chain holds, at this visit's first frame: a stop edge, a reposition of a running clock, or a
    /// change of the node's clock tag. See [`HeldNotes::release`] (CC 123 only while not
    /// `playing`).
    pub release: bool,
}

/// The most chasing edges
/// per node visit. A natural wrap past it in the same visit (a range shorter than the block: `setLoopRange` accepts 1
/// frame) still releases (and the Sampler's loop-end release) but does not chase; the skipped chase is counted
/// on the node's state ([`ChainState::chase_skipped`]).
pub const MAX_CHASED_EDGES: u32 = 2;

/// One chased note-on: its pitch and the velocity byte of its regular on (channel 1, `0x90`; the Bridge
/// sends `velocity.max(1)`, as the compile's own note-ons).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChaseNote {
    /// The MIDI note number.
    pub note: u8,
    /// The note-on's velocity byte.
    pub velocity: u8,
}

impl ChaseNote {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(note: u8, velocity: u8) -> ChaseNote {
        ChaseNote { note, velocity }
    }
}

/// *(rule 1)* The note-ons sounding at a position: `on < p` and a paired end with `end - p > eps`, in compiled
/// order (the graph's chase index, built on control). Callback-safe: no allocation, lock or syscall.
pub trait ChaseQuery {
    /// Calls `f` with every note-on sounding at `p` (its end more than `eps` frames after `p`), in compiled order.
    fn sounding(&self, p: u64, eps: u64, f: &mut dyn FnMut(ChaseNote));
}

/// What a visit chases: the node's index (`None`: no chase, as deck B, every render and a description
/// without one), its schedule's loop-start list (natural wraps of a nonzero range), and whether this visit opens on an
/// explicit edge (rule 2 (b)-(d): a start, a running reposition, a released clock-tag change), chased at the piece's
/// wrapped position.
#[derive(Clone, Copy, Default)]
pub struct Chase<'a> {
    /// The node's chase index (`None`: no chase).
    pub index: Option<&'a dyn ChaseQuery>,
    /// The schedule's loop-start list: the notes a natural wrap chases.
    pub at_start: &'a [ChaseNote],
    /// This visit opens on an explicit edge (chased at the piece's wrapped position).
    pub edge: bool,
}

impl Chase<'_> {
    /// No chase: today's visit, bit for bit.
    pub const NONE: Chase<'static> = Chase {
        index: None,
        at_start: &[],
        edge: false,
    };
}

/// The chase work of the node's last visit: chasing edges, chased note-ons offered, edges skipped
/// past [`MAX_CHASED_EDGES`], and the release walks' steps ([`HeldNotes::release`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChaseStats {
    /// Chasing edges of the visit.
    pub edges: u32,
    /// Chased note-ons offered.
    pub chased: u32,
    /// Edges skipped past [`MAX_CHASED_EDGES`].
    pub skipped: u32,
    /// The release walks' steps.
    pub release_steps: u32,
}

impl ChaseStats {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(edges: u32, chased: u32, skipped: u32, release_steps: u32) -> ChaseStats {
        ChaseStats {
            edges,
            chased,
            skipped,
            release_steps,
        }
    }
}

/// What a piece chases (internal).
#[derive(Clone, Copy)]
enum ChaseWhat<'a> {
    /// An explicit edge: the query at the piece's wrapped position.
    Edge(&'a dyn ChaseQuery, u64),
    /// A natural wrap: the loop-start list.
    Start(&'a [ChaseNote]),
}

/// Held note and sustain obligations, implemented from the held-notes behaviour contract (its observable
/// requirements).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeldNotes {
    channels_by_note: [u16; 128],
    pedals: [bool; 16],
}

const _: [(); 272] = [(); core::mem::size_of::<HeldNotes>()];

impl Default for HeldNotes {
    fn default() -> Self {
        Self {
            channels_by_note: [0; 128],
            pedals: [false; 16],
        }
    }
}

impl HeldNotes {
    /// Whether an event may be sent: a note-on of a note the channel already holds is refused; a note-off or a sustain-off clears its mark. Call [`HeldNotes::sent`] after sending.
    pub fn admit(&mut self, event: [u8; 3]) -> bool {
        let [status, data, value] = event;
        let channel = usize::from(status & 0x0f);
        let note = usize::from(data & 0x7f);
        let channel_bit = 1u16 << channel;

        match status & 0xf0 {
            0x80 => {
                self.channels_by_note[note] &= !channel_bit;
            }
            0x90 if value == 0 => {
                self.channels_by_note[note] &= !channel_bit;
            }
            0x90 => {
                return self.channels_by_note[note] & channel_bit == 0;
            }
            0xb0 if data == 64 && value < 64 => {
                self.pedals[channel] = false;
            }
            _ => {}
        }

        true
    }

    /// Records a sent event: a note-on marks its note held on its channel, a CC 64 at 64 or above marks the pedal down.
    pub fn sent(&mut self, event: [u8; 3]) {
        let [status, data, value] = event;
        let channel = usize::from(status & 0x0f);

        match status & 0xf0 {
            0x90 if value != 0 => {
                let note = usize::from(data & 0x7f);
                self.channels_by_note[note] |= 1u16 << channel;
            }
            0xb0 if data == 64 && value >= 64 => {
                self.pedals[channel] = true;
            }
            _ => {}
        }
    }

    /// Whether `channel` holds `note`.
    pub fn is_held(&self, channel: u8, note: u8) -> bool {
        let channel_bit = 1u16 << (channel & 0x0f);
        self.channels_by_note[usize::from(note & 0x7f)] & channel_bit != 0
    }

    /// Whether `channel`'s sustain pedal is down.
    pub fn is_pedal_down(&self, channel: u8) -> bool {
        self.pedals[usize::from(channel & 0x0f)]
    }

    /// Whether nothing is held: no note and no pedal down.
    pub fn is_empty(&self) -> bool {
        self.channels_by_note.iter().all(|&channels| channels == 0)
            && self.pedals.iter().all(|&down| !down)
    }

    /// Emits the releases of everything held and clears it: per held note a note-off; per channel that held one, CC 66 = 0 and
    /// CC 64 = 0 (and CC 123 when `stopped`); then CC 64 = 0 on each pedal-only channel. Returns the walk's steps (0 when nothing was
    /// held).
    pub fn release(&mut self, stopped: bool, mut emit: impl FnMut([u8; 3])) -> u32 {
        let mut channels_released = 0u16;
        let mut distinct_pitches = 0u32;

        for note in 0u8..128 {
            let channels = self.channels_by_note[usize::from(note)];
            if channels == 0 {
                continue;
            }

            distinct_pitches += 1;
            self.channels_by_note[usize::from(note)] = 0;

            for channel in 0u8..16 {
                let channel_bit = 1u16 << channel;
                if channels & channel_bit == 0 {
                    continue;
                }

                emit([0x80 | channel, note, 0]);

                if channels_released & channel_bit == 0 {
                    channels_released |= channel_bit;
                    self.pedals[usize::from(channel)] = false;

                    emit([0xb0 | channel, 66, 0]);
                    emit([0xb0 | channel, 64, 0]);
                    if stopped {
                        emit([0xb0 | channel, 123, 0]);
                    }
                }
            }
        }

        let mut released_pedal_only = false;
        for channel in 0u8..16 {
            if self.pedals[usize::from(channel)] {
                self.pedals[usize::from(channel)] = false;
                released_pedal_only = true;
                emit([0xb0 | channel, 64, 0]);
            }
        }

        if distinct_pitches == 0 && !released_pedal_only {
            0
        } else {
            16 + 16 * distinct_pitches
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Not yet looked at the region (a fresh state).
    Init,
    Run,
    /// Waiting for `ack >= until` (and slot `until`'s echo) before publishing again: the open breaker, or a fresh
    /// state taking over a region with blocks in flight.
    Wait {
        until: u64,
        breaker: bool,
    },
}

/// The raised-cosine weight of position `i` of `n` (0 at i = 0, 1 at i = n): the graph crate's `raised_cosine`, the shape
/// of every declick of the graph. Its first and second differences are bounded (π/2n, π²/2n²), which keeps the graph's
/// second-difference bound across a fade.
#[inline]
fn raised_cosine(i: u32, n: u32) -> f32 {
    if n == 0 || i >= n {
        return 1.0;
    }
    0.5 - 0.5 * (std::f32::consts::PI * i as f32 / n as f32).cos()
}

/// The output envelope, by FIFO read position `p`: a fade-out over `out_len` frames ending at `cut`, then
/// `gap` frames of silence (the FIFO holds zeros there), then a fade-in over `in_len` frames. A breaker close is
/// `cut` = now, `out_len` = 0, `gap` = d × B (the primed FIFO); a depth change has all three.
#[derive(Clone, Copy, Debug)]
struct Env {
    on: bool,
    cut: u64,
    out_len: u32,
    gap: u32,
    in_len: u32,
}

impl Env {
    const UNITY: Env = Env {
        on: false,
        cut: 0,
        out_len: 0,
        gap: 0,
        in_len: 0,
    };

    #[inline]
    fn gain(&mut self, p: u64) -> f32 {
        if !self.on {
            return 1.0;
        }
        if p < self.cut {
            let left = self.cut - p; // >= 1
            return if left > self.out_len as u64 {
                1.0
            } else {
                raised_cosine(left as u32 - 1, self.out_len)
            };
        }
        let after = p - self.cut;
        if after < self.gap as u64 {
            return 0.0;
        }
        let k = after - self.gap as u64;
        if k >= self.in_len as u64 {
            self.on = false;
            return 1.0;
        }
        raised_cosine(k as u32, self.in_len)
    }

    /// Whether a fade is still ahead of read position `p`.
    fn busy(&self, p: u64) -> bool {
        self.on && p < self.cut + self.gap as u64 + self.in_len as u64
    }
}

/// The exchange's mutation set, for the loom model only (`--cfg loom`; absent from every other build): each variant
/// weakens ONE rule of the shipped exchange, and the model must fail on it.
#[cfg(loom)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mutant {
    /// The shipped code.
    #[default]
    None,
    /// `seq` published with a Relaxed store (the request line no longer happens-before the child's Acquire).
    RelaxedSeq,
    /// The collect loads `ack` Relaxed (the output plane no longer happens-after the child's Release).
    RelaxedAck,
    /// On a miss at d = 2 the oldest block is abandoned and the next one published anyway: into the slot the child
    /// still owns.
    PublishIntoChildSlot,
}

/// The node's state in the graph's state pool under (chain uuid, `bridge`): breaker, FIFO, pending seqs, depth, carried
/// MIDI. Built on control ([`ChainState::new`] allocates); the callback only moves data through it.
#[derive(Clone, Debug)]
pub struct ChainState {
    /// The region this state was built for (`Region::instance_id`): a new region is an incompatible change.
    instance: u64,
    depth: u8,
    max_block: usize,
    declick: u32,
    mode: Mode,
    /// Published, not yet collected: (seq, frames), oldest first.
    inflight: [(u64, u32); 2],
    n_inflight: usize,
    next_seq: u64,
    /// This block's decision (first visit).
    block: u64,
    started: bool,
    publishing: bool,
    staged_midi: u32,
    /// FIFO: 2 channels × `cap` frames.
    fifo: Box<[f32]>,
    cap: usize,
    write: u64,
    read: u64,
    env: Env,
    /// Silence / dry ramp: the last emitted sample per channel and the ramp position.
    last_out: [f32; 2],
    ramp_from: [f32; 2],
    ramp_pos: u32,
    silent: bool,
    carry: MidiCarry,
    discontinuity: bool,
    /// The last value sent per model parameter (NaN bits = never).
    sent: Box<[u32]>,
    /// The clock tag the node last ran on ([`ChainState::FRESH_TAG`]: never ran).
    clock_tag: u8,
    /// The notes the chain holds: note-ons sent, not yet sent their note-off ([`HeldNotes`]).
    held: HeldNotes,
    /// the range owning a deferred end edge. No edge transfers to a different range.
    ranged_loop: Option<(u64, u64)>,
    /// Reached the exclusive end at the last piece's end; release before the next running piece.
    wrap_pending: bool,
    /// Natural-wrap chases skipped past [`MAX_CHASED_EDGES`], cumulative (read on control).
    chase_skipped: u64,
    /// The last visit's chase work.
    chase_stats: ChaseStats,
    #[cfg(loom)]
    mutant: Mutant,
}

impl ChainState {
    /// Control side: the fresh state of a node (at install) for the region `instance`, at depth `depth`, with
    /// `n_params` model parameters. `max_block` = B, the device's max block.
    #[allow(clippy::disallowed_methods)] // control side: allocates the FIFO
    pub fn new(
        instance: u64,
        depth: u8,
        n_params: usize,
        max_block: usize,
        declick: u32,
    ) -> ChainState {
        let depth = depth.clamp(1, 2);
        // Room for depth 2 whatever the start depth (a depth change is in place): 2 × B primed or in flight, one
        // collected block, and one block of slack.
        let cap = 4 * max_block.max(1);
        // No page faults on the realtime path: the callback writes the FIFO first; touch its pages here, on control.
        let fifo = runecraft_rt::os::prefaulted(0.0f32, 2 * cap);
        ChainState {
            instance,
            depth,
            max_block,
            declick,
            mode: Mode::Init,
            inflight: [(0, 0); 2],
            n_inflight: 0,
            next_seq: 1,
            block: 0,
            started: false,
            publishing: false,
            staged_midi: 0,
            fifo,
            cap,
            write: 0,
            read: 0,
            env: Env::UNITY,
            last_out: [0.0; 2],
            ramp_from: [0.0; 2],
            ramp_pos: 0,
            silent: false,
            carry: MidiCarry::default(),
            discontinuity: false,
            sent: vec![f32::NAN.to_bits(); n_params].into_boxed_slice(),
            clock_tag: ChainState::FRESH_TAG,
            held: HeldNotes::default(),
            ranged_loop: None,
            wrap_pending: false,
            chase_skipped: 0,
            chase_stats: ChaseStats::default(),
            #[cfg(loom)]
            mutant: Mutant::None,
        }
    }

    /// *(Test seam)* The faults the callback's first stores into the FIFO would take (0: prefaulted).
    #[doc(hidden)]
    pub fn fifo_faults_for_tests(&mut self) -> u64 {
        runecraft_rt::os::faults_storing(&mut self.fifo, 0.0)
    }

    /// The clock tag of a state that has not run yet (it records its first tag and releases nothing).
    pub const FRESH_TAG: u8 = u8::MAX;

    /// The tag the node last ran on ([`Self::FRESH_TAG`] before its first visit).
    #[inline]
    pub fn clock_tag(&self) -> u8 {
        self.clock_tag
    }

    /// The notes the chain holds (what the next release sends a note-off for).
    #[inline]
    pub fn held(&self) -> &HeldNotes {
        &self.held
    }

    /// The natural-wrap chases skipped past [`MAX_CHASED_EDGES`] since the state was made.
    #[inline]
    pub fn chase_skipped(&self) -> u64 {
        self.chase_skipped
    }

    /// The last visit's chase work.
    #[inline]
    pub fn chase_stats(&self) -> ChaseStats {
        self.chase_stats
    }

    /// Record the tag the node runs on.
    #[inline]
    pub fn set_clock_tag(&mut self, tag: u8) {
        self.clock_tag = tag;
    }

    /// The loom model's mutation.
    #[cfg(loom)]
    pub fn set_mutant(&mut self, m: Mutant) {
        self.mutant = m;
    }

    /// The `seq` publish edge: Release.
    #[inline(always)]
    fn publish_order(&self) -> Ordering {
        #[cfg(loom)]
        if self.mutant == Mutant::RelaxedSeq {
            return Ordering::Relaxed;
        }
        Ordering::Release
    }

    /// The collect edge: `ack` with Acquire.
    #[inline(always)]
    fn collect_order(&self) -> Ordering {
        #[cfg(loom)]
        if self.mutant == Mutant::RelaxedAck {
            return Ordering::Relaxed;
        }
        Ordering::Acquire
    }

    /// The pipeline depth d.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// The region instance this state was built for.
    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// The number of model parameters the state tracks.
    pub fn n_params(&self) -> usize {
        self.sent.len()
    }

    /// Whether the breaker is open (tests, telemetry inspection).
    pub fn breaker_open(&self) -> bool {
        matches!(self.mode, Mode::Wait { breaker: true, .. })
    }

    /// The seq the next publish will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// The chain's node stops running because its part is skipped (gated and held below the
    /// floor for the tail window). From here it neither publishes nor collects, so `seq` stops where it is and, once
    /// the child has acked what is in flight, `seq == ack`: the supervisor's wedge detector does not suspect it, and
    /// nothing decides a miss. The state is put on the breaker's **closing** path, with no breaker: the first visit
    /// after the part runs again (re-entry) discards the stale in-flight output, re-primes the FIFO, sets the request's
    /// discontinuity and fades in over declickFrames, counting nothing (a re-prime, not a miss). A fresh state is
    /// left as it is (its first visit takes over the region the same way). No allocation (N1).
    #[inline]
    pub fn suspend(&mut self) {
        if self.mode == Mode::Init {
            return;
        }
        let until = if self.n_inflight > 0 {
            self.inflight[self.n_inflight - 1].0
        } else {
            0
        };
        self.mode = Mode::Wait {
            until,
            breaker: false,
        };
        self.silent = true;
        self.last_out = [0.0; 2];
        self.ramp_from = [0.0; 2];
        self.ramp_pos = 0;
    }

    /// Whether the state waits on the breaker's closing path (a suspended chain, or an open breaker; tests).
    pub fn closing(&self) -> bool {
        matches!(self.mode, Mode::Wait { .. })
    }

    fn prime(&mut self, fade: bool) {
        let d = self.depth as usize * self.max_block;
        self.read = 0;
        self.write = d as u64;
        for v in self.fifo.iter_mut() {
            *v = 0.0;
        }
        self.env = if fade {
            Env {
                on: true,
                cut: 0,
                out_len: 0,
                gap: d as u32,
                in_len: self.declick,
            }
        } else {
            Env::UNITY
        };
    }

    /// Block `seq` is the host application's again when `ack >= seq` (Acquire: the child's Release of `ack` orders its
    /// output plane and its echo before this) and its slot echoes `seq`.
    #[inline]
    fn collectable(&self, r: &Region, seq: u64) -> bool {
        r.ack().load(self.collect_order()) >= seq
            && r.slot(seq).u64(R_ECHO).load(Ordering::Relaxed) == seq
    }

    /// Collect the oldest in-flight block into the FIFO. False on a miss.
    fn collect(&mut self, r: &Region, counters: &AudioCounters) -> bool {
        let (seq, frames) = self.inflight[0];
        if !self.collectable(r, seq) {
            return false;
        }
        self.inflight[0] = self.inflight[1];
        self.n_inflight -= 1;
        let n = (frames as usize).min(r.geometry().max_frames as usize);
        let slot = r.slot(seq);
        let mut finite = true;
        let w0 = self.write;
        for c in 0..2 {
            let src = slot.output(c.min(r.geometry().ch_out as usize - 1));
            let base = c * self.cap;
            for (i, s) in src[..n].iter().enumerate() {
                let v = f32::from_bits(s.load(Ordering::Relaxed));
                finite &= v.is_finite();
                self.fifo[base + ((w0 + i as u64) % self.cap as u64) as usize] = v;
            }
        }
        if !finite {
            // A NaN or Inf never reaches a downstream node's state: the block is silence.
            for c in 0..2 {
                for i in 0..n {
                    self.fifo[c * self.cap + ((w0 + i as u64) % self.cap as u64) as usize] = 0.0;
                }
            }
            bump(&counters.non_finite, 1);
            bump(&counters.silenced, 1);
        }
        // One Relaxed load after the ack's Acquire. A bypassed block is not a miss: it plays, counted.
        // `--cfg mutant_bypassed_uncounted` (never in a normal build) drops the count; the host application's latency
        // tests fail.
        if slot.u32(R_STATUS).load(Ordering::Relaxed) & ST_BYPASSED != 0
            && !cfg!(mutant_bypassed_uncounted)
        {
            bump(&counters.slot_bypassed, 1);
        }
        self.write = w0 + n as u64;
        true
    }

    /// The first visit of a block: the depth and miss decision. Returns whether this block publishes. `want` is the
    /// link's depth (a depth change keeps this state: the change happens here, in place).
    fn begin(&mut self, r: &Region, counters: &AudioCounters, want: u8) -> bool {
        let want = want.clamp(1, 2);
        if want != self.depth && self.mode != Mode::Run {
            // A fresh state, or an open breaker: the (re-)prime at the close takes the new depth.
            self.depth = want;
        }
        if self.mode == Mode::Init {
            // The callback is the only writer of seq, so its own read is exact; ack is the child's.
            let seq = r.seq().load(Ordering::Relaxed);
            self.next_seq = seq + 1;
            self.n_inflight = 0;
            self.prime(false);
            self.mode = if seq > r.ack().load(self.collect_order()) {
                Mode::Wait {
                    until: seq,
                    breaker: false,
                }
            } else {
                Mode::Run
            };
            if seq > 0 {
                self.discontinuity = true;
            }
        }
        match self.mode {
            Mode::Init => unreachable!(),
            Mode::Run => {
                if self.n_inflight >= self.depth as usize && !self.collect(r, counters) {
                    #[cfg(loom)]
                    if self.mutant == Mutant::PublishIntoChildSlot && self.depth == 2 {
                        // The mutant: abandon the oldest block and publish the next into its slot.
                        self.inflight[0] = self.inflight[1];
                        self.n_inflight -= 1;
                        return true;
                    }
                    // A miss. Silence (or dry), no publish, the breaker opens; the close primes `want`.
                    let newest = self.inflight[self.n_inflight - 1].0;
                    self.mode = Mode::Wait {
                        until: newest,
                        breaker: true,
                    };
                    self.depth = want;
                    bump(&counters.breaker_opens, 1);
                    bump(&counters.late, 1);
                    self.enter_silence();
                    return false;
                }
                if want != self.depth {
                    self.change_depth(want);
                }
                true
            }
            Mode::Wait { until, breaker } => {
                if until == 0 || self.collectable(r, until) {
                    // Close: the stale output is discarded, the FIFO re-primed, the next request has discontinuity
                    // and the carried releases at offset 0, and the output fades in.
                    self.n_inflight = 0;
                    self.prime(true);
                    self.mode = Mode::Run;
                    self.discontinuity = true;
                    self.silent = false;
                    true
                } else {
                    if breaker {
                        bump(&counters.late, 1);
                    }
                    self.enter_silence();
                    false
                }
            }
        }
    }

    /// A depth change, in place, after this block's collect at the old depth (a fade-out of the last old-depth
    /// block, never a hard cut). The FIFO's newest frame is `cut`; the frames before it fade out over the declick
    /// length (or what of them is still unplayed, at least this block's), then the gap, then a fade-in:
    /// - a **raise** (1 → 2) inserts B frames of zeros at `cut`: one block of silence, and the extra block of delay;
    /// - a **lowering** (2 → 1) drops the newer of the two blocks in flight (its output is never played; its slot is
    ///   free again once the next collect sees `ack` reach the block after it) and pads the gap to B − its frames.
    ///
    /// Deferred, block by block, while a fade is still ahead or the pipeline is not full at the old depth (a change
    /// right after a close); `ack` and slots are untouched, so no slot changes hands.
    fn change_depth(&mut self, want: u8) {
        if self.env.busy(self.read) || self.n_inflight + 1 < self.depth as usize {
            return;
        }
        let b = self.max_block as u64;
        let cut = self.write;
        let gap = if want > self.depth {
            b
        } else {
            // n_inflight is depth - 1 = 1 here (this block's collect took the oldest): drop it.
            let (_, frames) = self.inflight[0];
            self.n_inflight = 0;
            b.saturating_sub(frames as u64)
        };
        for k in 0..gap {
            let at = ((cut + k) % self.cap as u64) as usize;
            self.fifo[at] = 0.0;
            self.fifo[self.cap + at] = 0.0;
        }
        self.write = cut + gap;
        self.env = Env {
            on: true,
            cut,
            out_len: (self.declick as u64).min(cut - self.read) as u32,
            gap: gap as u32,
            in_len: self.declick,
        };
        self.depth = want;
    }

    fn enter_silence(&mut self) {
        if !self.silent {
            self.silent = true;
            self.ramp_from = self.last_out;
            self.ramp_pos = 0;
        }
    }

    /// One visit. `io` holds the node's input (the summed inputs of an FX chain; ignored for an instrument) and
    /// receives its output. `params` are the node's model parameters (0..1). Returns true when this visit published
    /// (the caller marks the chain's ring group).
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn visit(
        &mut self,
        link: &ChainLink,
        v: &Visit,
        io: [&mut [f32]; 2],
        events: &[MidiEvent],
        params: &[f64],
        counters: &AudioCounters,
    ) -> bool {
        self.visit_chased(link, v, io, events, params, counters, &Chase::NONE)
    }

    /// [`ChainState::visit`] with the node's chase: at an edge, the release first
    /// (`Visit.release`'s at an explicit edge), then the events before `o + d`, then the chased
    /// note-ons at `o + d` (clamped to the piece's last frame) through the one `ChainState::stage_midi` path, then the
    /// rest. One chase per piece: an explicit edge replaces the loop-start list in its piece and consumes the pending
    /// wrap. At most [`MAX_CHASED_EDGES`] chasing edges per visit. With [`Chase::NONE`], today's visit bit for bit.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    pub fn visit_chased(
        &mut self,
        link: &ChainLink,
        v: &Visit,
        io: [&mut [f32]; 2],
        events: &[MidiEvent],
        params: &[f64],
        counters: &AudioCounters,
        chase: &Chase<'_>,
    ) -> bool {
        let r = &*link.region;
        let geo = *r.geometry();
        let [l, rr] = io;
        let len = v.len.min(l.len()).min(rr.len());
        if !self.started || self.block != v.block {
            self.started = true;
            self.block = v.block;
            self.staged_midi = 0;
            // An oversized block (> maxFrames) is silence, counted like a miss but without a breaker change.
            self.publishing = if v.total > geo.max_frames as usize {
                self.enter_silence();
                false
            } else {
                self.begin(r, counters, link.depth)
            };
            if self.publishing && !self.carry.is_empty() {
                // The carried releases go first, at offset 0.
                let slot = r.slot(self.next_seq);
                let mut k = 0u32;
                let max = geo.max_midi;
                let n = self.carry.drain(|e| {
                    if k < max {
                        write_midi(slot.midi(k as usize), 0, e);
                        k += 1;
                    }
                });
                bump(&counters.midi_carried, n as u64);
                self.staged_midi = k;
            }
        }
        // Stage this visit's input and events.
        let slot = r.slot(self.next_seq);
        if self.publishing && geo.ch_in > 0 {
            for c in 0..(geo.ch_in as usize).min(2) {
                let src: &[f32] = if c == 0 { l } else { rr };
                let dst = slot.input(c);
                for i in 0..len {
                    dst[v.offset + i].store(src[i].to_bits(), Ordering::Relaxed);
                }
            }
        }
        // Only nonzero ranges acquire natural release edges. A stopped visit,
        // an explicit transport edge or a new range consumes/discards the old pending boundary.
        let range = (v.loop_start > 0 && v.loop_start < v.loop_frames)
            .then_some((v.loop_start, v.loop_frames));
        if self.ranged_loop != range || v.release || !v.playing {
            self.wrap_pending = false;
        }
        self.ranged_loop = range;
        self.chase_stats = ChaseStats::default();
        if v.release {
            self.release_midi(r, v.offset, !v.playing, counters);
        }
        if v.playing {
            let (d, eps) = chase_frames(v.sample_rate);
            // rule 2 (b)-(d): an explicit edge chases at this visit's first frame, if the node has an index.
            let edge = if chase.edge { chase.index } else { None };
            if let Some((start, end)) = range {
                let mut pos = if v.pos >= end {
                    start + (v.pos - start) % (end - start)
                } else {
                    v.pos
                };
                let mut offset = 0;
                while offset < len {
                    let natural = self.wrap_pending;
                    if natural && !cfg!(mutant_chase_before_release) {
                        #[cfg(not(mutant_release_omitted))]
                        self.release_midi(r, v.offset + offset, false, counters);
                    }
                    self.wrap_pending = false;
                    let n = ((end - pos).min((len - offset) as u64)) as usize;
                    // Mutation: releasing before this piece cuts its last held note early.
                    #[cfg(mutant_release_early)]
                    if pos + n as u64 == end {
                        self.release_midi(r, v.offset + offset, false, counters);
                    }
                    // One chase per piece, queried at the piece's wrapped position (mutation
                    // `mutant_chase_raw_pos`: the visit's raw position).
                    let p = if cfg!(mutant_chase_raw_pos) {
                        v.pos
                    } else {
                        pos
                    };
                    let what = match edge {
                        Some(q) if offset == 0 => Some(ChaseWhat::Edge(q, p)),
                        _ if natural
                            && !chase.at_start.is_empty()
                            && !cfg!(mutant_no_wrap_chase) =>
                        {
                            Some(ChaseWhat::Start(chase.at_start))
                        }
                        _ => None,
                    };
                    // Mutation `mutant_double_chase`: an edge at a pending wrap chases the loop-start
                    // list as well.
                    let also = (cfg!(mutant_double_chase)
                        && natural
                        && matches!(what, Some(ChaseWhat::Edge(..))))
                    .then_some(ChaseWhat::Start(chase.at_start));
                    self.stage_piece(
                        r,
                        v.offset + offset,
                        events,
                        (0, 0),
                        pos,
                        n,
                        [what, also],
                        (d, eps),
                        natural && cfg!(mutant_chase_before_release),
                        counters,
                    );
                    offset += n;
                    self.wrap_pending = pos + n as u64 == end;
                    #[cfg(not(mutant_release_resume_zero))]
                    {
                        pos = start;
                    }
                    #[cfg(mutant_release_resume_zero)]
                    {
                        pos = 0;
                    }
                }
            } else if len > 0 {
                let what = edge.map(|q| {
                    let lf = v.loop_frames;
                    let ls = if v.loop_start < lf { v.loop_start } else { 0 };
                    let p = if lf > 0 && v.pos >= lf && !cfg!(mutant_chase_raw_pos) {
                        ls + (v.pos - ls) % (lf - ls)
                    } else {
                        v.pos
                    };
                    ChaseWhat::Edge(q, p)
                });
                self.stage_piece(
                    r,
                    v.offset,
                    events,
                    (v.loop_start, v.loop_frames),
                    v.pos,
                    len,
                    [what, None],
                    (d, eps),
                    false,
                    counters,
                );
            }
        }
        // Output.
        if self.silent {
            self.emit_silence(l, rr, len, link.monitor, counters, v.offset == 0);
        } else {
            for i in 0..len {
                let g = self.env.gain(self.read);
                let at = (self.read % self.cap as u64) as usize;
                l[i] = self.fifo[at] * g;
                rr[i] = self.fifo[self.cap + at] * g;
                self.read += 1;
            }
            if len > 0 {
                self.last_out = [l[len - 1], rr[len - 1]];
            }
        }
        // Publish at the end of the block.
        if self.publishing && v.offset + len >= v.total {
            self.publish(r, v, params, link, counters);
            return true;
        }
        false
    }

    /// The one MIDI admission/staging/carry path, shared by explicit edges and ranged wraps.
    #[inline]
    fn stage_midi(&mut self, r: &Region, offset: usize, e: Event3, counters: &AudioCounters) {
        if !self.held.admit(e) {
            return;
        }
        if self.publishing && self.staged_midi < r.geometry().max_midi {
            write_midi(
                r.slot(self.next_seq).midi(self.staged_midi as usize),
                offset as u32,
                e,
            );
            self.staged_midi += 1;
            self.held.sent(e);
            return;
        }
        match self.carry.offer(e) {
            Offered::Dropped => bump(&counters.midi_dropped, 1),
            Offered::AllNotesOff => bump(&counters.midi_all_notes_off, 1),
            Offered::Carried => {}
        }
    }

    #[inline]
    fn release_midi(&mut self, r: &Region, offset: usize, stopped: bool, counters: &AudioCounters) {
        // Clear before staging: a same-key note-on at this offset must be a fresh trigger.
        let mut released = std::mem::take(&mut self.held);
        let steps = released.release(stopped, |e| self.stage_midi(r, offset, e, counters));
        self.chase_stats.release_steps += steps;
    }

    /// One piece of `n` frames from clock position `pos`, its events looked up on `(loop_start,
    /// loop_frames)` (`(0, 0)`: linear, a ranged piece), staged at `at + k`. With a chase (`what[0]`, at most
    /// [`MAX_CHASED_EDGES`] per visit, else counted as skipped): the events before `d`, the chased ons at `at + d` (`d`
    /// clamped to the piece's last frame), the rest. `what[1]` only under a mutant. `release_after`: only under the
    /// `mutant_chase_before_release` mutant (the wrap's release after the chase).
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn stage_piece(
        &mut self,
        r: &Region,
        at: usize,
        events: &[MidiEvent],
        (ls, lf): (u64, u64),
        pos: u64,
        n: usize,
        what: [Option<ChaseWhat<'_>>; 2],
        (d, eps): (u64, u64),
        release_after: bool,
        counters: &AudioCounters,
    ) {
        let mut first = what[0];
        if first.is_some() {
            if self.chase_stats.edges < MAX_CHASED_EDGES || cfg!(mutant_chase_unbounded_edges) {
                self.chase_stats.edges += 1;
            } else {
                self.chase_stats.skipped += 1;
                if !cfg!(mutant_chase_skip_uncounted) {
                    self.chase_skipped += 1;
                    bump(&counters.chase_skipped, 1);
                }
                first = None;
            }
        }
        if first.is_none() {
            for_each_event_ranged(events, ls, lf, pos, n, |k, e| {
                self.stage_midi(r, at + k, e, counters);
            });
            if release_after {
                self.release_midi(r, at, false, counters);
            }
            return;
        }
        let a = (d as usize).min(n.saturating_sub(1));
        for_each_event_ranged(events, ls, lf, pos, a, |k, e| {
            self.stage_midi(r, at + k, e, counters);
        });
        let o = at + a;
        for w in [first, what[1]].into_iter().flatten() {
            let mut on = |c: ChaseNote| {
                self.chase_stats.chased += 1;
                let e = [0x90, c.note & 0x7f, c.velocity.max(1)];
                if cfg!(mutant_chase_skips_held) {
                    // Mutation: the chase written past the one staging path (never held, so never released).
                    if self.publishing && self.staged_midi < r.geometry().max_midi {
                        write_midi(
                            r.slot(self.next_seq).midi(self.staged_midi as usize),
                            o as u32,
                            e,
                        );
                        self.staged_midi += 1;
                    }
                    return;
                }
                self.stage_midi(r, o, e, counters);
            };
            match w {
                ChaseWhat::Edge(q, p) => q.sounding(p, eps, &mut on),
                ChaseWhat::Start(list) => {
                    for &c in list {
                        on(c);
                    }
                }
            }
        }
        if release_after {
            self.release_midi(r, o, false, counters);
        }
        for_each_event_ranged(events, ls, lf, pos + a as u64, n - a, |k, e| {
            self.stage_midi(r, o + k, e, counters);
        });
    }

    fn emit_silence(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        len: usize,
        monitor: bool,
        counters: &AudioCounters,
        count: bool,
    ) {
        let n = self.declick.max(1);
        // mutation `mutant_monitor_silent_on_miss` silences the monitor chain's misses.
        if monitor && !cfg!(mutant_monitor_silent_on_miss) {
            // The monitor chain passes its dry input, the offset from the last emitted sample decaying
            // linearly to 0 over declickFrames.
            if self.ramp_pos == 0 && len > 0 {
                self.ramp_from = [self.ramp_from[0] - l[0], self.ramp_from[1] - r[0]];
            }
            for i in 0..len {
                let k = (self.ramp_pos + i as u32).min(n);
                let w = 1.0 - k as f32 / n as f32;
                l[i] += self.ramp_from[0] * w;
                r[i] += self.ramp_from[1] * w;
            }
            if count {
                bump(&counters.dry_passed, 1);
            }
        } else {
            for i in 0..len {
                let k = (self.ramp_pos + i as u32).min(n);
                let w = 1.0 - k as f32 / n as f32;
                l[i] = self.ramp_from[0] * w;
                r[i] = self.ramp_from[1] * w;
            }
            if count {
                bump(&counters.silenced, 1);
            }
        }
        self.ramp_pos = self.ramp_pos.saturating_add(len as u32);
        if len > 0 {
            self.last_out = [l[len - 1], r[len - 1]];
        }
    }

    fn publish(
        &mut self,
        r: &Region,
        v: &Visit,
        params: &[f64],
        link: &ChainLink,
        counters: &AudioCounters,
    ) {
        let seq = self.next_seq;
        let slot = r.slot(seq);
        let geo = r.geometry();
        // Changed parameters, at most maxParams per block, the rest next block.
        let mut np = 0u32;
        let mut deferred = 0u64;
        for (i, (&id, sent)) in link.param_ids.iter().zip(self.sent.iter_mut()).enumerate() {
            let val = params.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0) as f32;
            if val.to_bits() == *sent {
                continue;
            }
            if np < geo.max_params {
                let rec = slot.param(np as usize);
                rec[0].store(id, Ordering::Relaxed);
                rec[1].store(val.to_bits(), Ordering::Relaxed);
                *sent = val.to_bits();
                np += 1;
            } else {
                deferred += 1;
            }
        }
        if deferred > 0 {
            bump(&counters.param_deferred, deferred);
        }
        let t = Transport::at_ranged(
            v.block_pos,
            v.loop_start,
            v.loop_frames,
            v.sample_rate,
            v.tempo,
            v.playing,
        );
        let mut flags = t.flags;
        if self.discontinuity {
            flags |= BF_DISCONTINUITY;
            self.discontinuity = false;
        }
        slot.u32(S_FRAMES).store(v.total as u32, Ordering::Relaxed);
        slot.u32(S_BLOCK_FLAGS).store(flags, Ordering::Relaxed);
        slot.u32(S_MIDI_COUNT)
            .store(self.staged_midi, Ordering::Relaxed);
        slot.u32(S_PARAM_COUNT).store(np, Ordering::Relaxed);
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
        // The edge: everything above happens-before the child's Acquire of seq.
        r.seq().store(seq, self.publish_order());
        self.inflight[self.n_inflight] = (seq, v.total as u32);
        self.n_inflight += 1;
        self.next_seq = seq + 1;
    }
}

#[inline]
fn write_midi(rec: &[AtomicU32], offset: u32, e: Event3) {
    rec[0].store(offset, Ordering::Relaxed);
    rec[1].store(3, Ordering::Relaxed);
    rec[2].store(u32::from_le_bytes([e[0], e[1], e[2], 0]), Ordering::Relaxed);
    rec[3].store(0, Ordering::Relaxed);
    rec[4].store(0, Ordering::Relaxed);
    rec[5].store(0, Ordering::Relaxed);
}

/// The events of the `len` frames from clock position `pos`, loop-aware (wrapping at `loop_frames`, several times for
/// a loop shorter than the run), with their offset from the first frame.
#[cfg(test)]
#[inline]
pub(crate) fn for_each_event(
    events: &[MidiEvent],
    loop_frames: u64,
    pos: u64,
    len: usize,
    f: impl FnMut(usize, Event3),
) {
    for_each_event_ranged(events, 0, loop_frames, pos, len, f);
}

/// [`for_each_event`] on a clock looping `[loop_start, loop_frames)`: after each wrap the lookup goes on at
/// the loop start. At 0, today's.
#[inline]
pub(crate) fn for_each_event_ranged(
    events: &[MidiEvent],
    loop_start: u64,
    loop_frames: u64,
    pos: u64,
    len: usize,
    mut f: impl FnMut(usize, Event3),
) {
    if events.is_empty() || len == 0 {
        return;
    }
    let start = if loop_start < loop_frames {
        loop_start
    } else {
        0
    };
    let len = len as u64;
    if loop_frames == 0 {
        let a = events.partition_point(|e| e.frame < pos);
        for e in events[a..].iter().take_while(|e| e.frame < pos + len) {
            f((e.frame - pos) as usize, e.bytes);
        }
        return;
    }
    let mut t = 0u64;
    let mut p = if pos >= loop_frames {
        start + (pos - start) % (loop_frames - start)
    } else {
        pos
    };
    while t < len {
        let seg = (loop_frames - p).min(len - t);
        let a = events.partition_point(|e| e.frame < p);
        for e in events[a..].iter().take_while(|e| e.frame < p + seg) {
            f((t + (e.frame - p)) as usize, e.bytes);
        }
        t += seg;
        p = start;
    }
}

/// The ring: after the group's last level has run, `hub[g].store(block, Release)` and one
/// `os_sync_wake_by_address_all`. `ENOENT` is normal; other errors are counted.
#[inline]
pub fn ring_group(page: &ChildPage, group: usize, block: u64, counters: &AudioCounters) {
    let hub = page.hub_waker(group);
    let t0 = crate::sync::host_ticks();
    // Diagnostic: a Relaxed stamp of the ring for the child's wake-to-run timing; not part of the child
    // contract.
    page.word64(P_HUB[group.min(RING_GROUPS - 1)] + P_RING_STAMP)
        .store(t0, Ordering::Relaxed);
    if hub.ring(block).is_err() {
        bump(&counters.doorbell_errors, 1);
    }
    let dt = crate::sync::host_ticks().wrapping_sub(t0);
    bump(&counters.rings, 1);
    bump(&counters.ring_ticks_total, dt);
    counters.ring_ticks_last.store(dt, Ordering::Relaxed);
    if dt > counters.ring_ticks_max.load(Ordering::Relaxed) {
        counters.ring_ticks_max.store(dt, Ordering::Relaxed);
    }
}

/// built only under `--cfg lint_probe` (never in a normal build), to show that this crate's
/// clippy.toml ban on `runecraft_rt::stderr::{err,out}` fires through the `#[macro_export]` macros expanded here (a
/// realtime module), and so does the ban on a doorbell wait. `RUSTFLAGS="--cfg lint_probe" cargo clippy -p summoner-rt` must
/// report three disallowed methods.
#[cfg(lint_probe)]
pub fn lint_probe_stderr() {
    runecraft_rt::safe_eprintln!("lint-probe");
    runecraft_rt::safe_println!("lint-probe");
    let w = AtomicU64::new(0);
    let bell =
        runecraft_ipc::bridge::Doorbell::create(runecraft_ipc::bridge::DoorbellKind::One).unwrap();
    match bell.waker(&w).wait(1, None) {
        runecraft_ipc::bridge::sync::Waited::Woken
        | runecraft_ipc::bridge::sync::Waited::TimedOut
        | runecraft_ipc::bridge::sync::Waited::Failed(_) => {}
        _ => {}
    }
}

#[cfg(all(test, not(loom)))]
#[allow(clippy::disallowed_methods)] // test-side vectors
mod clock_tests {
    //! The request line and the events from the chain's clock.
    use super::*;

    /// a chain on a clock looping `[loop_start, loop_frames)`: the request line's position wraps to the loop
    /// start and its loop points are `[start, end)` in quarter notes (the existing fields, no ABI change); the events
    /// after a wrap go on at the start; at a start of 0 both are today's, bit for bit.
    #[test]
    fn a_ranged_loop_wraps_the_request_lines_position_and_the_events_to_its_start() {
        let sr = 48_000.0;
        let tempo = Tempo::default();
        for (pos, lf) in [
            (0u64, 0u64),
            (100, 0),
            (47_999, 48_000),
            (48_005, 48_000),
            (10, 96_000),
        ] {
            assert_eq!(
                Transport::at_ranged(pos, 0, lf, sr, tempo, true),
                Transport::at(pos, lf, sr, tempo, true)
            );
        }
        let t = Transport::at_ranged(48_010, 24_000, 48_000, sr, tempo, true);
        assert_eq!(t.time_samples, 24_010);
        assert_eq!(t.loop_start_ppq, 1.0, "half a second at 120 bpm");
        assert_eq!(t.loop_end_ppq, 2.0);
        assert!(t.flags & BF_LOOPING != 0 && t.flags & BF_LOOP_POINTS_VALID != 0);
        let before = Transport::at_ranged(1_000, 24_000, 48_000, sr, tempo, true);
        assert_eq!(
            before.time_samples, 1_000,
            "a position before the start rolls in"
        );
        let events = [
            ev(5, [0x90, 1, 1]),
            ev(30, [0x90, 2, 1]),
            ev(38, [0x90, 3, 1]),
        ];
        // [20, 40) from 34 for 30 frames: 38 at 4; the wrap at 6 to 20: 30 at 16, 38 at 24; the wrap at 26: nothing.
        let mut got = Vec::new();
        for_each_event_ranged(&events, 20, 40, 34, 30, |at, e| got.push((at, e[1])));
        assert_eq!(got, vec![(4, 3), (16, 2), (24, 3)]);
        let mut zero = Vec::new();
        let mut old = Vec::new();
        for_each_event_ranged(&events, 0, 40, 28, 90, |at, e| zero.push((at, e[1])));
        for_each_event(&events, 40, 28, 90, |at, e| old.push((at, e[1])));
        assert_eq!(zero, old);
    }

    fn ev(frame: u64, b: Event3) -> MidiEvent {
        MidiEvent::new(frame, b)
    }

    #[test]
    fn events_resolve_against_the_clock_position_and_wrap_at_its_loop() {
        let events = [ev(5, [0x90, 60, 100]), ev(30, [0x80, 60, 0])];
        let mut got = Vec::new();
        // A loop of 40 from position 28, 30 frames: 30 at offset 2, the wrap, 5 at offset 17.
        for_each_event(&events, 40, 28, 30, |at, e| got.push((at, e[0])));
        assert_eq!(got, vec![(2, 0x80), (17, 0x90)]);
        // A loop shorter than the run wraps several times.
        got.clear();
        for_each_event(&events, 40, 0, 100, |at, e| got.push((at, e[0])));
        assert_eq!(
            got,
            vec![(5, 0x90), (30, 0x80), (45, 0x90), (70, 0x80), (85, 0x90)]
        );
        // No loop: frames from the clock's position.
        got.clear();
        for_each_event(&events, 0, 10, 30, |at, e| got.push((at, e[0])));
        assert_eq!(got, vec![(20, 0x80)]);
    }

    #[test]
    fn a_release_is_a_note_off_per_held_note_then_sostenuto_and_sustain_and_cc_123_only_stopped() {
        let mut h = HeldNotes::default();
        assert_eq!(
            std::mem::size_of::<HeldNotes>(),
            272,
            "16 channels x 128 notes, one bit each, and the 16 pedals (u128-aligned)"
        );
        // Held: channel 3 notes 64 and 60, channel 1 note 64, channel 16 note 127; channel 2's note was let go.
        for e in [
            [0x92, 64, 100],
            [0x92, 60, 90],
            [0x90, 64, 80],
            [0x9f, 127, 1],
            [0x91, 70, 100],
            [0x81, 70, 0],
        ] {
            assert!(h.admit(e));
            h.sent(e);
        }
        // A note-on of a held note is dropped (the held-notes behaviour contract); 0x90 at velocity 0 is a
        // note-off; a CC 123 does not clear the set.
        assert!(!h.admit([0x92, 60, 127]));
        assert!(h.admit([0xb2, 123, 0]));
        assert!(h.is_held(2, 60));
        let snapshot = h;
        let mut got = Vec::new();
        h.release(true, |e| got.push(e));
        // Note-major, channel 1 first; a channel's controllers after its first note-off.
        assert_eq!(
            got,
            vec![
                [0x82, 60, 0],
                [0xb2, 66, 0],
                [0xb2, 64, 0],
                [0xb2, 123, 0],
                [0x80, 64, 0],
                [0xb0, 66, 0],
                [0xb0, 64, 0],
                [0xb0, 123, 0],
                [0x82, 64, 0],
                [0x8f, 127, 0],
                [0xbf, 66, 0],
                [0xbf, 64, 0],
                [0xbf, 123, 0],
            ]
        );
        assert!(h.is_empty(), "released");
        // While playing (a running reposition, a tag change onto a running clock): no CC 123.
        let mut h = snapshot;
        got.clear();
        h.release(false, |e| got.push(e));
        assert_eq!(got.len(), 4 + 3 * 2);
        assert!(got.iter().all(|e| e[0] & 0xf0 != 0xb0 || e[1] != 123));
        // Nothing held and no pedal down: nothing sent.
        got.clear();
        HeldNotes::default().release(true, |e| got.push(e));
        assert!(got.is_empty());
        // 0x90 at velocity 0 lets a note go.
        let mut h = HeldNotes::default();
        h.sent([0x95, 33, 9]);
        assert!(h.admit([0x95, 33, 0]));
        assert!(h.is_empty());
        // Every event of a release but CC 66 is a release in the carry's sense: carried while the breaker is
        // open (the carry drops CC 66, as before).
        let mut h = snapshot;
        h.release(true, |e| assert!(crate::midi::is_release(e) || e[1] == 66));
    }

    /// A channel whose sustain pedal is down gets
    /// CC 64 = 0 at every edge, whether or not it holds a note; channels holding a note keep their
    /// order, and the pedal-only channels follow, channel 1 first. A pedal is down once a CC 64 ≥ 64 reached the slot
    /// (`sent`), and up again at an admitted CC 64 < 64.
    ///
    /// **Mutation** (the pedal ignored: CC 64 = 0 only where a note is held): the pedal-only channels get nothing.
    #[test]
    fn a_release_lifts_the_pedal_on_every_pedal_down_channel() {
        let mut h = HeldNotes::default();
        for e in [
            [0xb4, 64, 127], // channel 5: pedal down, a note let go under it (not held)
            [0x94, 60, 100],
            [0x84, 60, 0],
            [0xb1, 64, 64], // channel 2: pedal down and a note held
            [0x91, 62, 100],
            [0xb9, 64, 100], // channel 10: pedal down, then up: nothing
            [0xb9, 64, 0],
            [0xb0, 64, 127], // channel 1: pedal down only
            [0xb3, 64, 63],  // channel 4: below 64 is up
        ] {
            assert!(h.admit(e));
            h.sent(e);
        }
        assert!(h.is_pedal_down(0) && h.is_pedal_down(1) && h.is_pedal_down(4));
        assert!(!h.is_pedal_down(9) && !h.is_pedal_down(3));
        // A pedal-down the carry dropped (never sent) is not down.
        let mut d = HeldNotes::default();
        assert!(d.admit([0xb6, 64, 127]));
        assert!(d.is_empty());
        let snapshot = h;
        for stopped in [true, false] {
            let mut h = snapshot;
            let mut got = Vec::new();
            h.release(stopped, |e| got.push(e));
            let mut want = vec![[0x81, 62, 0], [0xb1, 66, 0], [0xb1, 64, 0]];
            if stopped {
                want.push([0xb1, 123, 0]);
            }
            want.extend([[0xb0, 64, 0], [0xb4, 64, 0]]);
            assert_eq!(got, want, "stopped {stopped}");
            assert!(h.is_empty(), "every pedal up after the release");
        }
        // A pedal alone is something to release.
        let mut h = HeldNotes::default();
        h.sent([0xbf, 64, 127]);
        assert!(!h.is_empty());
        let mut got = Vec::new();
        h.release(true, |e| got.push(e));
        assert_eq!(got, vec![[0xbf, 64, 0]]);
    }
}
