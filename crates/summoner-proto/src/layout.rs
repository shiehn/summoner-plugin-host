//! The byte-exact region (protocol 4.x; the region bytes are 3.1's, additive), the child page, and the control frame header: offsets as constants, checked at compile time, plus the field table a C header can be generated from
//! ([`SECTIONS`]). Little-endian; every field a naturally aligned `u32` / `u64`; `f32` / `f64` stored as bits.
//!
//! Region: `[0, 128)` header (supervisor, once) · `[128, 256)` doorbell line (supervisor; `ping` by the supervisor) · `[256, 384)`
//! ack line (child) · slot 0 at 384 · slot 1 at `384 + slotBytes`. A slot: `[0, 128)` request line (supervisor) ·
//! `[128, 256)` response line (child) · planes from 256 (`in`, `out`, MIDI records, params).

/// `"SASC"`.
pub const MAGIC: u32 = 0x4353_4153;
/// Protocol 4.0 (a major bump; the host application and the child are a pinned pair, so there is no 3.x path: each side
/// refuses the other major with the existing version-mismatch error): `SlotDesc.plugin` ([`crate::plugin::PluginRef`])
/// replaces the 3.x description XML, `scanned.classes` ([`crate::plugin::ScanClass`], raw facts) replaces `descriptions`,
/// the 3.x hello's toolkit field and its test state mode are gone. Every 3.x addition below is part of 4.0.
///
/// `major << 16 | minor`: 3.10, following 3.9 (additive: `SlotInfo.notices` on `chain.loaded`
/// and `chain.prepared`'s per-slot notices, a slot's notice codes, absent none and written only when not empty; the
/// region is 3.1's, unchanged; nothing is gated: an older child sends none). 3.8 (additive: kind 25 `chain.prepareFresh` (ReceiptAck)
/// and its completion event kind 61 `chain.preparedFresh {chainKey, ok, ms, latency, error?}`: a render set's slots
/// released and prepared again on their kept instances, the render warm-up's clean start; the region is 3.1's,
/// unchanged; the host application sends it only to a child whose `hello` claims 3.8). 3.7 (additive: `ParamEntry`'s plugin id,
/// `automatable`, `default`, `value`, `stepCount`, `label` and `order`, names cut at 1024 characters and pages bounded at
/// 48 KiB of meta, and kind 35 `param.set` (MainThread, answered with its read-back); the region is 3.1's, unchanged;
/// the host application sends `param.set` and reads the new fields only from a child whose `hello` claims 3.7). 3.6 (additive: `log.set` (kind 5, Inline), the
/// optional top-level `corr` on request metas and its echo on the answers, `HelloReply.wiredBytes` /
/// `unwiredBytes` (absent 0) and `chain.prepared`'s pair (absent keeps `chain.loaded`'s); the region is 3.1's,
/// unchanged; the host application sends `log.set` and `corr` only to a child whose `hello` claims 3.6). 3.5 (additive: the description's AU codes and
/// version, an AU's component codes and version, absent for a VST3 and from an older child; the region is 3.1's,
/// unchanged). 3.4 (`editor-windows.md` gaps 4 and 5, additive: `chain.loaded`'s
/// `SlotInfo.hasEditor`, absent true, and `editor.open`'s `guardReset`, absent false; the region is 3.1's, unchanged).
/// 3.3 is additive: `state.applied.childGeneration`, and `state.get`'s
/// `{ok: false, reason: "tooLarge", bytes}`. 3.2 is additive:
/// `SlotDesc.enabled`, `slot.setEnabled`. Today's C++ protocol is 2. 3.1 is additive: `ST_BYPASSED`,
/// the `slot.reactivated` event, `latencyFrames` at +140 removed and reserved, and the `state.set`
/// mode `sasp` (the state blob's framing). Both children check only the major.
///
/// 4.1 is the Windows-only additive minor (a Windows host application and child advertise it: `chain.load`'s `fanSlot`, the
/// `editor.open` receipt's `editorWindow`, the FanOut registration region); on unix the constant stays 4.0, so
/// every unix byte is unchanged, and a unix host application accepts any 4.x. The next unix additive change takes 4.2, never 4.1.
#[cfg(not(windows))]
pub const ABI_VERSION: u32 = 4 << 16;
/// Protocol 4.1's Windows-only additive layout (the Windows port's file).
#[cfg(windows)]
pub mod win41;

/// The protocol version, Windows: 4.1 (see the unix constant's note).
#[cfg(windows)]
pub const ABI_VERSION: u32 = (4 << 16) | 1;

/// One cache line of the region: 128 bytes.
pub const LINE: usize = 128;
/// The header line's offset in the region.
pub const HEADER_LINE: usize = 0;
/// The doorbell line's offset in the region.
pub const DOORBELL_LINE: usize = 128;
/// The ack line's offset in the region.
pub const ACK_LINE: usize = 256;
/// Slot 0's offset in the region (slot 1 follows at `SLOTS_BASE + slotBytes`).
pub const SLOTS_BASE: usize = 384;

// Header [0, 128), written by the host application once.
/// Header: `magic` ([`MAGIC`]).
pub const H_MAGIC: usize = 0;
/// Header: `abi_version` ([`ABI_VERSION`]).
pub const H_ABI: usize = 4;
/// Header: `region_bytes` ([`Geometry::region_bytes`]).
pub const H_REGION_BYTES: usize = 8;
/// Header: `flags` ([`FLAG_RENDER`], [`FLAG_ACK_WAKE`]).
pub const H_FLAGS: usize = 12;
/// Header: `chain_key`, the control-assigned chain key (`u64`).
pub const H_CHAIN_KEY: usize = 16;
/// Header: `instance_id`, a random id per region, also sent in `chain.load` (`u64`).
pub const H_INSTANCE_ID: usize = 24;
/// Header: `sample_rate`, as `f64` bits.
pub const H_SAMPLE_RATE: usize = 32;
/// Header: `max_frames`, at most [`MAX_FRAMES`], a multiple of 32.
pub const H_MAX_FRAMES: usize = 40;
/// Header: `ch_in`, the input channels, at most [`MAX_CHANNELS`].
pub const H_CH_IN: usize = 44;
/// Header: `ch_out`, the output channels, at most [`MAX_CHANNELS`].
pub const H_CH_OUT: usize = 48;
/// Header: `max_midi`, at most [`MAX_MIDI`] records per block.
pub const H_MAX_MIDI: usize = 52;
/// Header: `max_params`, at most [`MAX_PARAMS`] changes per block.
pub const H_MAX_PARAMS: usize = 56;
/// Header: `tap_slot`, the slot after which the tap sits ([`NO_TAP`]: none).
pub const H_TAP_SLOT: usize = 60;
/// Header: `slot_bytes` ([`Geometry::slot_bytes`]).
pub const H_SLOT_BYTES: usize = 64;
/// Header: `slot_off0`, slot 0's offset ([`SLOTS_BASE`]).
pub const H_SLOT_OFF0: usize = 68;
/// Header: `slot_off1`, slot 1's offset (`SLOTS_BASE + slot_bytes`).
pub const H_SLOT_OFF1: usize = 72;
/// Header: `in_off`, the input plane's offset from a slot's base.
pub const H_IN_OFF: usize = 76;
/// Header: `out_off`, the output plane's offset from a slot's base.
pub const H_OUT_OFF: usize = 80;
/// Header: `midi_off`, the MIDI plane's offset from a slot's base.
pub const H_MIDI_OFF: usize = 84;
/// Header: `param_off`, the parameter plane's offset from a slot's base.
pub const H_PARAM_OFF: usize = 88;
/// Header: reserved to the end of the line, written 0.
pub const H_RESERVED: usize = 92;

/// Header flags.
pub const FLAG_RENDER: u32 = 1 << 0;
/// Header flag bit 1: the child wakes the host application's waiter after each ack (a render set's synchronous exchange).
pub const FLAG_ACK_WAKE: u32 = 1 << 1;

// Doorbell line [128, 256), supervisor -> child.
/// Doorbell: `seq`, the last published block (supervisor, Release).
pub const D_SEQ: usize = 128;
/// Doorbell: `ping`, the supervisor's wedge-detector nonce (0: none).
pub const D_PING: usize = 136;
/// Doorbell: reserved to the end of the line, written 0.
pub const D_RESERVED: usize = 144;

// Ack line [256, 384), child -> supervisor.
/// Ack: `ack`, the seq of the last completed block (child, Release).
pub const A_ACK: usize = 256;
/// Ack: `processed_blocks`, cumulative.
pub const A_PROCESSED: usize = 264;
/// Ack: `pong`, the last ping the chain thread answered (Release).
pub const A_PONG: usize = 272;
/// Ack: `in_slot`, slot + 1 while the chain thread is inside a plugin's process call, else 0.
pub const A_IN_SLOT: usize = 280;
/// The chain's ring group as the child's reader last stored it (the hub switch: accepted at +284 of the ack line,
/// the child's line, written by the child's reader). The host application never reads it for the exchange.
pub const A_GROUP: usize = 284;
/// Ack: `process_hist`, block counts by the child's process time over the period, in 1/20 steps.
pub const A_HIST: usize = 288;
/// The buckets of `process_hist` (the last one counts 75 % of the period and over).
pub const HIST_BUCKETS: usize = 16;
/// Ack: reserved to the end of the line, written 0.
pub const A_RESERVED: usize = 352;

// Slot request line [0, 128), supervisor -> child (offsets from the slot's base).
/// Request: `block_seq`, the seq this slot holds.
pub const S_BLOCK_SEQ: usize = 0;
/// Request: `frames`, the block's frames, at most `max_frames`.
pub const S_FRAMES: usize = 8;
/// Request: `block_flags` (the transport bits, [`BF_RESET`], [`BF_DISCONTINUITY`]).
pub const S_BLOCK_FLAGS: usize = 12;
/// Request: `midi_count`, at most `max_midi`.
pub const S_MIDI_COUNT: usize = 16;
/// Request: `param_count`, at most `max_params`.
pub const S_PARAM_COUNT: usize = 20;
/// Request: `time_sig`, `numerator | denominator << 16`.
pub const S_TIME_SIG: usize = 24;
/// Request: reserved, written 0.
pub const S_RESERVED0: usize = 28;
/// Request: `ppq`, the block's position in quarter notes (`f64` bits).
pub const S_PPQ: usize = 32;
/// Request: `bpm` (`f64` bits).
pub const S_BPM: usize = 40;
/// Request: `time_seconds`, the block's position in seconds (`f64` bits).
pub const S_TIME_SECONDS: usize = 48;
/// Request: `time_samples`, the block's position in frames (`i64` bits).
pub const S_TIME_SAMPLES: usize = 56;
/// Request: `loop_start_ppq` (`f64` bits).
pub const S_LOOP_START_PPQ: usize = 64;
/// Request: `loop_end_ppq` (`f64` bits).
pub const S_LOOP_END_PPQ: usize = 72;
/// Request: `bar_start_ppq`, the current bar's start (`f64` bits).
pub const S_BAR_START_PPQ: usize = 80;
/// Request: `output_host_time`, when the block is heard, in host ticks.
pub const S_OUTPUT_HOST_TIME: usize = 88;
/// Request: reserved to the end of the line, written 0.
pub const S_RESERVED1: usize = 96;

// Slot response line [128, 256), child -> supervisor.
/// Response: `echo_seq`, the seq the child processed into this slot.
pub const R_ECHO: usize = 128;
/// Response: `status` (the `ST_*` bits).
pub const R_STATUS: usize = 136;
/// Was 3.0's `latencyFrames` (the chain's latency now), which nothing read: PDC is a compile-time
/// quantity that only `chain.loaded` and `latency.changed` move. Reserved: written 0, never read, and **never
/// reused**, so a 3.0 child that still writes a latency here cannot be misread as any 3.1 field.
pub const R_RESERVED_LATENCY: usize = 140;
/// Response: `process_ns`, the child's time for this block.
pub const R_PROCESS_NS: usize = 144;
/// Response: `tap_peak_l`, the tap's left block peak before the insert (`f32` bits).
pub const R_TAP_PEAK_L: usize = 148;
/// Response: `tap_peak_r`, the tap's right block peak before the insert (`f32` bits).
pub const R_TAP_PEAK_R: usize = 152;
/// Response: `tap_sumsq_l`, the tap's left sum of squares before the insert (`f32` bits).
pub const R_TAP_SUMSQ_L: usize = 156;
/// Response: `tap_sumsq_r`, the tap's right sum of squares before the insert (`f32` bits).
pub const R_TAP_SUMSQ_R: usize = 160;
/// Response: reserved to the end of the line, written 0.
pub const R_RESERVED: usize = 164;

/// Where a slot's planes start.
pub const PLANES: usize = 256;

/// `block_flags`' transport bits live with the transport that sets them (orrery-time), re-exported here so the
/// layout keeps every flag; reset and discontinuity are the region's own.
pub use orrery_time::{
    BF_BAR_START_VALID, BF_LOOP_POINTS_VALID, BF_LOOPING, BF_PLAYING, BF_RECORDING,
};
/// All notes off + `reset()` before processing.
pub const BF_RESET: u32 = 1 << 8;
/// Blocks were skipped before this one (a breaker close, a depth change).
pub const BF_DISCONTINUITY: u32 = 1 << 9;

/// Response `status` bits.
pub const ST_PROCESSED: u32 = 1 << 0;
/// Response `status` bit 1: the block was skipped (the chain was busy).
pub const ST_SKIPPED: u32 = 1 << 1;
/// Response `status` bit 2: a plugin threw during the block.
pub const ST_THREW: u32 = 1 << 2;
/// At least one **hosted** slot passed its audio through in this block (a reactivation's handover or
/// bypass; a quarantined slot never sets it). Not a miss: the block plays, and the host application counts `bridgeSlotBypassed`.
pub const ST_BYPASSED: u32 = 1 << 3;

/// A MIDI record `{offset u32, bytes u32, data[16]}` and a parameter change `{id u32, value f32}`.
pub const MIDI_RECORD: usize = 24;
/// The size of a parameter change record `{id u32, value f32}`.
pub const PARAM_RECORD: usize = 8;

// Limits.
/// The most frames of one block.
pub const MAX_FRAMES: u32 = 8192;
/// The most channels in or out.
pub const MAX_CHANNELS: u32 = 8;
/// The most MIDI records of one block.
pub const MAX_MIDI: u32 = 1024;
/// The most parameter changes of one block.
pub const MAX_PARAMS: u32 = 64;
/// The most slots of one chain.
pub const MAX_SLOTS: usize = 32;
/// `tap_slot`'s value for no tap.
pub const NO_TAP: u32 = u32::MAX;

// The child page (for attribution): one 16 KiB page. Lines 0-3: the ring-group hub words (supervisor -> child). From 1,024
// the child's status record and its own counters (child -> supervisor).
/// The child page's size: one 16 KiB page.
pub const CHILD_PAGE_BYTES: usize = 16 * 1024;
/// The ring groups, one hub line each.
pub const RING_GROUPS: usize = 4;
/// Each ring group's hub word offset in the child page (supervisor, Release).
pub const P_HUB: [usize; RING_GROUPS] = [0, 128, 256, 384];
/// On each hub line, after the hub word: the host-ticks stamp of the group's last ring. A documented diagnostic word
/// a Relaxed store by the callback, for the child's wake-to-run measurement; explicitly NOT part of
/// the child contract (a child must not depend on it; the host application never reads it).
pub const P_RING_STAMP: usize = 8;
/// Where the child's status record starts in the child page.
pub const P_STATUS: usize = 1024;
/// Status: `op`, the message-thread op in progress (`OP_*`).
pub const P_OP: usize = 1024;
/// Status: `slot`, the slot the op is on.
pub const P_SLOT: usize = 1028;
/// Status: `chain_key`, the chain the op is on.
pub const P_CHAIN_KEY: usize = 1032;
/// Status: `op_start_ticks`, when the op started, in host ticks (0 when it ends).
pub const P_OP_START_TICKS: usize = 1040;
/// Status: `name`, the plugin's name (UTF-8, NUL padded).
pub const P_NAME: usize = 1048;
/// The size of `name` in bytes.
pub const P_NAME_BYTES: usize = 64;
/// Where the child's own counters start in the child page.
pub const P_COUNTERS: usize = 1152;
/// Chain-thread wakes that found `seq` unchanged (child-counted).
pub const P_SPURIOUS_WAKES: usize = 1152;
/// Chain-thread joins that fell back to time-constraint only.
pub const P_WG_FALLBACKS: usize = 1160;
/// Chain threads currently joined to the workgroup.
pub const P_WG_JOINED: usize = 1168;

/// Status record `op` codes (the message-thread op in progress).
pub const OP_NONE: u32 = 0;
/// Status op: loading a plugin.
pub const OP_LOAD: u32 = 1;
/// Status op: preparing a plugin.
pub const OP_PREPARE: u32 = 2;
/// Status op: setting a plugin's state.
pub const OP_SET_STATE: u32 = 3;
/// Status op: reading a plugin's state.
pub const OP_GET_STATE: u32 = 4;
/// Status op: an editor call.
pub const OP_EDITOR: u32 = 5;
/// Status op: unloading a plugin.
pub const OP_UNLOAD: u32 = 6;

/// The op's name, for reports (`none` for an unknown code).
pub fn op_name(op: u32) -> &'static str {
    match op {
        OP_LOAD => "load",
        OP_PREPARE => "prepare",
        OP_SET_STATE => "setState",
        OP_GET_STATE => "getState",
        OP_EDITOR => "editor",
        OP_UNLOAD => "unload",
        _ => "none",
    }
}

// The control frame header: 24 bytes, little-endian.
/// Frame header: `frame_len`, the bytes after this field (`20 + meta_len + blob_len`).
pub const F_FRAME_LEN: usize = 0;
/// Frame header: `kind` (`u16`).
pub const F_KIND: usize = 4;
/// Frame header: `flags` (`u16`).
pub const F_FLAGS: usize = 6;
/// Frame header: `id`, the request id (`u64`).
pub const F_ID: usize = 8;
/// Frame header: `meta_len`, the JSON meta's length.
pub const F_META_LEN: usize = 16;
/// Frame header: `blob_len`, the raw blob's length.
pub const F_BLOB_LEN: usize = 20;
/// The frame header's size in bytes.
pub const FRAME_HEADER: usize = 24;

const _: () = {
    assert!(H_RESERVED + 9 * 4 == DOORBELL_LINE);
    assert!(D_RESERVED + 14 * 8 == ACK_LINE);
    assert!(A_HIST + HIST_BUCKETS * 4 == A_RESERVED);
    assert!(A_RESERVED + 4 * 8 == SLOTS_BASE);
    assert!(S_RESERVED1 + 4 * 8 == LINE);
    assert!(S_OUTPUT_HOST_TIME + 8 == S_RESERVED1);
    assert!(R_RESERVED + 23 * 4 == PLANES);
    assert!(H_CHAIN_KEY.is_multiple_of(8) && H_SAMPLE_RATE.is_multiple_of(8));
    assert!(D_SEQ.is_multiple_of(8) && A_ACK.is_multiple_of(8) && R_ECHO.is_multiple_of(8));
    assert!(P_NAME + P_NAME_BYTES <= P_COUNTERS);
    assert!(P_WG_JOINED + 8 <= CHILD_PAGE_BYTES);
    assert!(F_BLOB_LEN + 4 == FRAME_HEADER);
};

/// A chain's geometry: chosen by the host application, kept by the host application, never read back from the region.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Geometry {
    /// The most frames of one block.
    pub max_frames: u32,
    /// The input channels.
    pub ch_in: u32,
    /// The output channels.
    pub ch_out: u32,
    /// The most MIDI records of one block.
    pub max_midi: u32,
    /// The most parameter changes of one block.
    pub max_params: u32,
    /// The slot after which the tap sits ([`NO_TAP`]: none).
    pub tap_slot: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Why a [`Geometry`] is refused: its `Display` names the bound it breaks.
pub struct GeometryError(&'static str);

impl GeometryError {
    /// The bound the geometry breaks.
    pub fn bound(&self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for GeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the geometry breaks its bound: {}", self.0)
    }
}

impl std::error::Error for GeometryError {}

impl Geometry {
    /// The common stereo chain: 2 / 2 channels, 1,024 MIDI records, 64 params, no tap.
    pub fn stereo(max_frames: u32) -> Geometry {
        Geometry {
            max_frames,
            ch_in: 2,
            ch_out: 2,
            max_midi: MAX_MIDI,
            max_params: MAX_PARAMS,
            tap_slot: NO_TAP,
        }
    }

    /// The geometry's limits: an error names the first one broken.
    pub fn validate(&self) -> Result<(), GeometryError> {
        if self.max_frames == 0
            || !self.max_frames.is_multiple_of(32)
            || self.max_frames > MAX_FRAMES
        {
            return Err(GeometryError(
                "maxFrames must be a multiple of 32 in 32..=8192",
            ));
        }
        if self.ch_in > MAX_CHANNELS || self.ch_out == 0 || self.ch_out > MAX_CHANNELS {
            return Err(GeometryError("chIn <= 8, 1 <= chOut <= 8"));
        }
        if self.max_midi > MAX_MIDI || self.max_params > MAX_PARAMS {
            return Err(GeometryError("maxMidi <= 1024, maxParams <= 64"));
        }
        if self.tap_slot != NO_TAP && self.tap_slot as usize >= MAX_SLOTS {
            return Err(GeometryError("tapSlot < 32 or none"));
        }
        Ok(())
    }

    /// The plane offsets from a slot's base.
    #[inline]
    pub fn in_off(&self) -> usize {
        PLANES
    }

    #[inline]
    /// The output plane's offset from a slot's base.
    pub fn out_off(&self) -> usize {
        PLANES + (self.ch_in * self.max_frames) as usize * 4
    }

    #[inline]
    /// The MIDI plane's offset from a slot's base.
    pub fn midi_off(&self) -> usize {
        self.out_off() + (self.ch_out * self.max_frames) as usize * 4
    }

    #[inline]
    /// The parameter plane's offset from a slot's base.
    pub fn param_off(&self) -> usize {
        self.midi_off() + self.max_midi as usize * MIDI_RECORD
    }

    /// round₁₂₈(256 + (chIn + chOut) × maxFrames × 4 + maxMidi × 24 + maxParams × 8).
    #[inline]
    pub fn slot_bytes(&self) -> usize {
        (self.param_off() + self.max_params as usize * PARAM_RECORD).div_ceil(LINE) * LINE
    }

    #[inline]
    /// Slot `k`'s offset in the region (by `k`'s parity: two slots).
    pub fn slot_off(&self, k: usize) -> usize {
        SLOTS_BASE + (k & 1) * self.slot_bytes()
    }

    /// 384 + 2 × slotBytes (the header's `regionBytes`).
    #[inline]
    pub fn region_bytes(&self) -> usize {
        SLOTS_BASE + 2 * self.slot_bytes()
    }

    /// The mapped size: `region_bytes` rounded up to 16 KiB pages.
    #[inline]
    pub fn mapped_bytes(&self) -> usize {
        self.region_bytes().div_ceil(CHILD_PAGE_BYTES) * CHILD_PAGE_BYTES
    }
}

/// What a C field is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A `u16`.
    U16,
    /// A `u32`.
    U32,
    /// A `u64`.
    U64,
    /// An `f32` stored as its bits.
    F32Bits,
    /// An `f64` stored as its bits.
    F64Bits,
    /// Raw bytes.
    Bytes,
    /// A reserved `u32`, written 0.
    Reserved32,
    /// A reserved `u64`, written 0.
    Reserved64,
}

impl Kind {
    /// The field's width in bytes.
    pub fn width(self) -> usize {
        match self {
            Kind::U16 => 2,
            Kind::U32 | Kind::F32Bits | Kind::Reserved32 => 4,
            Kind::U64 | Kind::F64Bits | Kind::Reserved64 => 8,
            Kind::Bytes => 1,
        }
    }
}

/// A struct of the C header: a name, its size, and its fields (offsets within the struct).
#[derive(Clone, Copy, Debug)]
pub struct Section {
    /// The struct's neutral tag (a generator adds its own prefix).
    pub tag: &'static str,
    /// The struct's size in bytes.
    pub size: usize,
    /// Its fields, in order.
    pub fields: &'static [Field],
    /// A one-line description.
    pub doc: &'static str,
}

#[derive(Clone, Copy, Debug)]
/// One field of a [`Section`].
pub struct Field {
    /// The field's name.
    pub name: &'static str,
    /// Its offset within the struct.
    pub offset: usize,
    /// Its kind.
    pub kind: Kind,
    /// The number of elements (1 for a scalar).
    pub count: usize,
    /// A one-line description (may be empty).
    pub doc: &'static str,
}

const fn f(
    name: &'static str,
    offset: usize,
    kind: Kind,
    count: usize,
    doc: &'static str,
) -> Field {
    Field {
        name,
        offset,
        kind,
        count,
        doc,
    }
}

use Kind::*;

/// The header line's fields.
pub const HEADER_FIELDS: &[Field] = &[
    f("magic", H_MAGIC, U32, 1, "0x43534153 \"SASC\""),
    f("abi_version", H_ABI, U32, 1, "major<<16 | minor; 3.1"),
    f(
        "region_bytes",
        H_REGION_BYTES,
        U32,
        1,
        "384 + 2 x slot_bytes",
    ),
    f("flags", H_FLAGS, U32, 1, "bit 0 render set, bit 1 ack wake"),
    f(
        "chain_key",
        H_CHAIN_KEY,
        U64,
        1,
        "the control-assigned chain key",
    ),
    f(
        "instance_id",
        H_INSTANCE_ID,
        U64,
        1,
        "a random id per region, also in chain.load",
    ),
    f("sample_rate", H_SAMPLE_RATE, F64Bits, 1, "f64 bits"),
    f(
        "max_frames",
        H_MAX_FRAMES,
        U32,
        1,
        "<= 8192, a multiple of 32",
    ),
    f("ch_in", H_CH_IN, U32, 1, "<= 8"),
    f("ch_out", H_CH_OUT, U32, 1, "<= 8"),
    f("max_midi", H_MAX_MIDI, U32, 1, "<= 1024"),
    f("max_params", H_MAX_PARAMS, U32, 1, "<= 64"),
    f(
        "tap_slot",
        H_TAP_SLOT,
        U32,
        1,
        "the slot after which the tap sits; ~0 = none",
    ),
    f(
        "slot_bytes",
        H_SLOT_BYTES,
        U32,
        1,
        "round128(256 + (in+out) x maxFrames x 4 + maxMidi x 24 + maxParams x 8)",
    ),
    f("slot_off0", H_SLOT_OFF0, U32, 1, "384"),
    f("slot_off1", H_SLOT_OFF1, U32, 1, "384 + slot_bytes"),
    f(
        "in_off",
        H_IN_OFF,
        U32,
        1,
        "plane offsets from a slot's base",
    ),
    f("out_off", H_OUT_OFF, U32, 1, ""),
    f("midi_off", H_MIDI_OFF, U32, 1, ""),
    f("param_off", H_PARAM_OFF, U32, 1, ""),
    f("reserved", H_RESERVED, Reserved32, 9, "0"),
];

/// The doorbell line's fields.
pub const DOORBELL_FIELDS: &[Field] = &[
    f(
        "seq",
        D_SEQ - DOORBELL_LINE,
        U64,
        1,
        "the last published block (callback; Release). Parked and render chains wait on it",
    ),
    f(
        "ping",
        D_PING - DOORBELL_LINE,
        U64,
        1,
        "the wedge detector's nonce (supervisor); 0 = none",
    ),
    f("reserved", D_RESERVED - DOORBELL_LINE, Reserved64, 14, "0"),
];

/// The ack line's fields.
pub const ACK_FIELDS: &[Field] = &[
    f(
        "ack",
        A_ACK - ACK_LINE,
        U64,
        1,
        "the seq of the last completed block (Release)",
    ),
    f(
        "processed_blocks",
        A_PROCESSED - ACK_LINE,
        U64,
        1,
        "cumulative",
    ),
    f(
        "pong",
        A_PONG - ACK_LINE,
        U64,
        1,
        "the last ping the CHAIN THREAD answered (Release)",
    ),
    f(
        "in_slot",
        A_IN_SLOT - ACK_LINE,
        U32,
        1,
        "slot + 1 while inside a plugin's processBlock, else 0",
    ),
    f(
        "group",
        A_GROUP - ACK_LINE,
        U32,
        1,
        "the ring group, written by the child's READER (Release; the hub switch); the host never reads it for the exchange",
    ),
    f(
        "process_hist",
        A_HIST - ACK_LINE,
        U32,
        HIST_BUCKETS,
        "block counts by processNs/period in 1/20 steps; 15 = >= 75 %",
    ),
    f("reserved", A_RESERVED - ACK_LINE, Reserved64, 4, "0"),
];

/// A slot's request and response lines' fields.
pub const REQUEST_FIELDS: &[Field] = &[
    f(
        "block_seq",
        S_BLOCK_SEQ,
        U64,
        1,
        "the seq this slot holds (host)",
    ),
    f("frames", S_FRAMES, U32, 1, "<= max_frames"),
    f(
        "block_flags",
        S_BLOCK_FLAGS,
        U32,
        1,
        "transport bits 0-4; bit 8 reset; bit 9 discontinuity",
    ),
    f("midi_count", S_MIDI_COUNT, U32, 1, "<= max_midi"),
    f("param_count", S_PARAM_COUNT, U32, 1, "<= max_params"),
    f("time_sig", S_TIME_SIG, U32, 1, "num | den << 16"),
    f("reserved0", S_RESERVED0, Reserved32, 1, "0"),
    f("ppq", S_PPQ, F64Bits, 1, ""),
    f("bpm", S_BPM, F64Bits, 1, ""),
    f("time_seconds", S_TIME_SECONDS, F64Bits, 1, ""),
    f("time_samples", S_TIME_SAMPLES, U64, 1, "i64 bits"),
    f("loop_start_ppq", S_LOOP_START_PPQ, F64Bits, 1, ""),
    f("loop_end_ppq", S_LOOP_END_PPQ, F64Bits, 1, ""),
    f("bar_start_ppq", S_BAR_START_PPQ, F64Bits, 1, ""),
    f("output_host_time", S_OUTPUT_HOST_TIME, U64, 1, "host ticks"),
    f("reserved1", S_RESERVED1, Reserved64, 4, "0"),
    f(
        "echo_seq",
        R_ECHO,
        U64,
        1,
        "child: the seq it processed into this slot",
    ),
    f(
        "status",
        R_STATUS,
        U32,
        1,
        "bit 0 processed, bit 1 skipped (busy), bit 2 a plugin threw, bit 3 a hosted slot passed through",
    ),
    f(
        "reserved_latency",
        R_RESERVED_LATENCY,
        Reserved32,
        1,
        "3.0's latencyFrames, removed; written 0, never read, never reused",
    ),
    f(
        "process_ns",
        R_PROCESS_NS,
        U32,
        1,
        "the child's time for this block",
    ),
    f(
        "tap_peak_l",
        R_TAP_PEAK_L,
        F32Bits,
        1,
        "the pre-insert block peak",
    ),
    f("tap_peak_r", R_TAP_PEAK_R, F32Bits, 1, ""),
    f(
        "tap_sumsq_l",
        R_TAP_SUMSQ_L,
        F32Bits,
        1,
        "pre-insert sum of x^2",
    ),
    f("tap_sumsq_r", R_TAP_SUMSQ_R, F32Bits, 1, ""),
    f("reserved2", R_RESERVED, Reserved32, 23, "0"),
];

/// A MIDI record's fields.
pub const MIDI_FIELDS: &[Field] = &[
    f("offset", 0, U32, 1, "frame offset in the block"),
    f("bytes", 4, U32, 1, "<= 16"),
    f("data", 8, Bytes, 16, "the MIDI bytes"),
];

/// A parameter change's fields.
pub const PARAM_FIELDS: &[Field] = &[
    f("id", 0, U32, 1, "slot << 24 | index"),
    f("value", 4, F32Bits, 1, "0..1"),
];

/// The child page's fields.
pub const CHILD_PAGE_FIELDS: &[Field] = &[
    f(
        "hub0",
        P_HUB[0],
        U64,
        1,
        "ring group 0 hub word (host; Release)",
    ),
    f(
        "ring_stamp0",
        8,
        U64,
        1,
        "DIAGNOSTIC ONLY, NOT part of the child contract: host ticks of the group's last ring, a Relaxed store by the callback (the wake-to-run measure); a child must not depend on it",
    ),
    f("reserved_hub0", 16, Reserved64, 14, ""),
    f("hub1", P_HUB[1], U64, 1, "ring group 1"),
    f(
        "ring_stamp1",
        136,
        U64,
        1,
        "DIAGNOSTIC ONLY, NOT part of the child contract: host ticks of the group's last ring, a Relaxed store by the callback (the wake-to-run measure); a child must not depend on it",
    ),
    f("reserved_hub1", 144, Reserved64, 14, ""),
    f("hub2", P_HUB[2], U64, 1, "ring group 2"),
    f(
        "ring_stamp2",
        264,
        U64,
        1,
        "DIAGNOSTIC ONLY, NOT part of the child contract: host ticks of the group's last ring, a Relaxed store by the callback (the wake-to-run measure); a child must not depend on it",
    ),
    f("reserved_hub2", 272, Reserved64, 14, ""),
    f(
        "hub3",
        P_HUB[3],
        U64,
        1,
        "ring group 3 (may span several levels; rung after the deepest)",
    ),
    f(
        "ring_stamp3",
        392,
        U64,
        1,
        "DIAGNOSTIC ONLY, NOT part of the child contract: host ticks of the group's last ring, a Relaxed store by the callback (the wake-to-run measure); a child must not depend on it",
    ),
    f("reserved_hub3", 400, Reserved64, 14, ""),
    f("reserved_core", 512, Reserved64, 64, ""),
    f(
        "op",
        P_OP,
        U32,
        1,
        "the message-thread op in progress (OP_*), 0 = none",
    ),
    f("slot", P_SLOT, U32, 1, ""),
    f("chain_key", P_CHAIN_KEY, U64, 1, ""),
    f(
        "op_start_ticks",
        P_OP_START_TICKS,
        U64,
        1,
        "host ticks; 0 when the op ends",
    ),
    f(
        "name",
        P_NAME,
        Bytes,
        P_NAME_BYTES,
        "the plugin's name, UTF-8, NUL padded",
    ),
    f("reserved_status", P_NAME + P_NAME_BYTES, Reserved64, 5, ""),
    f(
        "spurious_wakes",
        P_SPURIOUS_WAKES,
        U64,
        1,
        "chain-thread wakes that found seq unchanged",
    ),
    f(
        "workgroup_fallbacks",
        P_WG_FALLBACKS,
        U64,
        1,
        "joins that fell back to time-constraint only",
    ),
    f(
        "workgroup_joined",
        P_WG_JOINED,
        U64,
        1,
        "chain threads joined now",
    ),
];

/// The frame header's `kind` field comment: a code from the frame-kind catalogue (`codec::KINDS`).
pub const FRAME_KIND_DOC: &str = "the frame kind's code (K_*)";

/// The control frame header's fields.
pub const FRAME_FIELDS: &[Field] = &[
    f(
        "frame_len",
        F_FRAME_LEN,
        U32,
        1,
        "bytes after this field = 20 + meta_len + blob_len",
    ),
    f("kind", F_KIND, U16, 1, FRAME_KIND_DOC),
    f(
        "flags",
        F_FLAGS,
        U16,
        1,
        "bit 0 reply, bit 1 event, bit 2 error, bit 3 carries fds",
    ),
    f(
        "id",
        F_ID,
        U64,
        1,
        "request id from 1; replies echo it; events use 0",
    ),
    f("meta_len", F_META_LEN, U32, 1, "<= 64 KiB of UTF-8 JSON"),
    f("blob_len", F_BLOB_LEN, U32, 1, "<= 64 MiB of raw bytes"),
];

/// Every struct of the layout, in region order.
pub const SECTIONS: &[Section] = &[
    Section {
        tag: "header",
        size: LINE,
        fields: HEADER_FIELDS,
        doc: "Region [0, 128): written by the host once",
    },
    Section {
        tag: "doorbell",
        size: LINE,
        fields: DOORBELL_FIELDS,
        doc: "Region [128, 256): host -> child",
    },
    Section {
        tag: "ack",
        size: LINE,
        fields: ACK_FIELDS,
        doc: "Region [256, 384): child -> host",
    },
    Section {
        tag: "slot_lines",
        size: PLANES,
        fields: REQUEST_FIELDS,
        doc: "A slot's request line (host) and response line (child); planes follow at +256",
    },
    Section {
        tag: "midi",
        size: MIDI_RECORD,
        fields: MIDI_FIELDS,
        doc: "One MIDI record of the MIDI plane",
    },
    Section {
        tag: "param",
        size: PARAM_RECORD,
        fields: PARAM_FIELDS,
        doc: "One parameter change of the param plane",
    },
    Section {
        tag: "child_page",
        size: P_WG_JOINED + 8,
        fields: CHILD_PAGE_FIELDS,
        doc: "The child page (16 KiB mapping): hubs (host), status record and counters (child). ring_stamp* are diagnostic words, NOT part of the child contract",
    },
    Section {
        tag: "frame_header",
        size: FRAME_HEADER,
        fields: FRAME_FIELDS,
        doc: "The control frame header (fd-3 stream socket), little-endian",
    },
];

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;

    #[test]
    fn the_adr_sizes_hold() {
        let g = Geometry::stereo(512);
        assert_eq!(g.slot_bytes(), 33_536);
        assert_eq!(g.region_bytes(), 67_456);
        assert_eq!(g.mapped_bytes(), 80 * 1024);
        let big = Geometry {
            max_frames: 8192,
            ch_in: 8,
            ch_out: 8,
            ..Geometry::stereo(8192)
        };
        assert_eq!(big.slot_bytes(), 549_632);
        assert_eq!(big.region_bytes(), 1_099_648);
        assert_eq!(big.mapped_bytes(), 1088 * 1024);
        assert!(g.out_off().is_multiple_of(128) && g.midi_off().is_multiple_of(128));
    }

    #[test]
    fn every_section_tiles_without_gaps_or_overlaps() {
        for s in SECTIONS {
            let mut at = 0;
            for fl in s.fields {
                assert_eq!(fl.offset, at, "{}.{} offset", s.tag, fl.name);
                assert!(
                    fl.offset.is_multiple_of(fl.kind.width()),
                    "{}.{} alignment",
                    s.tag,
                    fl.name
                );
                at += fl.kind.width() * fl.count;
            }
            assert_eq!(at, s.size, "{} size", s.tag);
        }
    }

    #[test]
    fn bad_geometries_are_refused() {
        assert!(Geometry::stereo(100).validate().is_err());
        assert!(Geometry::stereo(16384).validate().is_err());
        assert!(
            Geometry {
                ch_out: 9,
                ..Geometry::stereo(512)
            }
            .validate()
            .is_err()
        );
        assert!(Geometry::stereo(64).validate().is_ok());
    }
}
