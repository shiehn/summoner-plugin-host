//! The control channel: the 24-byte little-endian frame header, the bounds (meta ≤ 64 KiB of JSON,
//! blob ≤ 64 MiB of raw bytes, checked **before** anything is allocated), the streaming [`Decoder`] the reader runs,
//! the catalogue, and the typed meta of every message. A protocol error closes the socket; there is no resync.
//! N9: everything here is fuzzed (the host application's frame fuzz target); hostile bytes give an error, never a panic.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use crate::layout::*;
pub use crate::plugin::{PluginFormat, PluginRef, ScanClass};
use runecraft_contracts::log::{Level, MAX_CORR_BYTES, corr_ok};
use serde::{Deserialize, Serialize};

pub mod form_b;
pub use form_b::{
    FormBMeta, LineFault, MAX_DESCRIPTION_LINE, MAX_SCAN_DESCRIPTIONS, OverLimit,
    SCAN_DESCRIPTIONS_BLOB, ScannedB, ScannedFault, ScannedFrame, ScannedLimit, ScannedLine,
    ScannedMalformed, ScannedMeta, decode_descriptions_blob, decode_scanned, encode_header_only,
    encode_scanned, encode_scanned_form_a, encode_scanned_form_b, parse_scanned_meta,
};

/// The largest meta of one frame: 64 KiB of UTF-8 JSON.
pub const MAX_META: usize = 64 * 1024;
/// The largest blob of one frame: 64 MiB of raw bytes.
pub const MAX_BLOB: usize = 64 * 1024 * 1024;
/// The most descriptors one frame may carry (`chain.load`: the region; `hello`: the child page).
#[cfg(not(windows))]
pub const MAX_FRAME_FDS: u32 = 2;
/// The most objects one frame may carry on Windows: the unix two, plus the child page's hub set section and a
/// waiter's event.
#[cfg(windows)]
pub const MAX_FRAME_FDS: u32 = 4;

/// Frame flag bit 0: a reply (it echoes its request's id).
pub const FF_REPLY: u16 = 1 << 0;
/// Frame flag bit 1: an event (id 0).
pub const FF_EVENT: u16 = 1 << 1;
/// Frame flag bit 2: an error reply (its meta is an [`ErrorReply`]).
pub const FF_ERROR: u16 = 1 << 2;
/// Frame flag bit 3: a handle rides with the frame.
pub const FF_FDS: u16 = 1 << 3;
const FF_KNOWN: u16 = FF_REPLY | FF_EVENT | FF_ERROR | FF_FDS;

/// The catalogue. Requests and their replies share a kind (the reply sets `FF_REPLY`); completion events have
/// their own.
pub mod kind {
    // Inline.
    /// `hello` (Inline): the opening exchange; the child page rides with it.
    pub const HELLO: u16 = 1;
    /// `ping` (Inline): a liveness check, answered by the child's reader.
    pub const PING: u16 = 2;
    /// `state.generations` (Inline): every slot's generation and editor flag ([`super::Generations`]).
    pub const STATE_GENERATIONS: u16 = 3;
    /// `shutdown` (Inline): the child ends.
    pub const SHUTDOWN: u16 = 4;
    /// Protocol 3.6 (additive): the host application's level word, applied by the
    /// child's reader at once (never behind its message thread), answered [`super::LogSetReply`] or `-32602`.
    pub const LOG_SET: u16 = 5;
    // ReceiptAck.
    /// `chain.load` (ReceiptAck): load a chain into a region; completed by `chain.loaded`.
    pub const CHAIN_LOAD: u16 = 16;
    /// `chain.unload` (ReceiptAck): completed by `chain.unloaded`.
    pub const CHAIN_UNLOAD: u16 = 17;
    /// `chain.prepare` (ReceiptAck): completed by `chain.prepared`.
    pub const CHAIN_PREPARE: u16 = 18;
    /// `chain.park` (ReceiptAck): stop a chain's thread from waiting on the doorbell; completed by `chain.parked`.
    pub const CHAIN_PARK: u16 = 19;
    /// `chain.unpark` (ReceiptAck): completed by `chain.unparked`.
    pub const CHAIN_UNPARK: u16 = 20;
    /// `state.set` (ReceiptAck): the blob is the state; completed by `state.applied`.
    pub const STATE_SET: u16 = 21;
    /// `editor.open` (ReceiptAck).
    pub const EDITOR_OPEN: u16 = 22;
    /// `editor.close` (ReceiptAck): completed by `editor.closed`.
    pub const EDITOR_CLOSE: u16 = 23;
    /// `workgroup.attach` (ReceiptAck): completed by `workgroup.attached`.
    pub const WORKGROUP_ATTACH: u16 = 24;
    /// Protocol 3.8 (additive): a render set's slots released and prepared again
    /// on their kept instances (no unload, no new instance); its completion is
    /// [`CHAIN_PREPARED_FRESH`] (never a second `chain.prepared`, whose `ChainReady` a job must not see again).
    pub const CHAIN_PREPARE_FRESH: u16 = 25;
    // MainThread.
    /// `state.get` (MainThread): answered with the slot's state as the blob.
    pub const STATE_GET: u16 = 32;
    /// `param.list` (MainThread): one page of a slot's parameters.
    pub const PARAM_LIST: u16 = 33;
    /// Protocol 3.2 (additive): enable or disable one slot of a live chain; answered once applied
    /// ([`SlotEnabledReply`](super::SlotEnabledReply)).
    pub const SLOT_SET_ENABLED: u16 = 34;
    /// Protocol 3.7 (additive): set one plugin parameter of a live slot by its
    /// own id, normalized; answered once set on the message thread ([`super::ParamSetReply`], with its read-back).
    pub const PARAM_SET: u16 = 35;
    // Events.
    /// `chain.loaded` (event).
    pub const CHAIN_LOADED: u16 = 48;
    /// `chain.unloaded` (event).
    pub const CHAIN_UNLOADED: u16 = 49;
    /// `chain.prepared` (event).
    pub const CHAIN_PREPARED: u16 = 50;
    /// `chain.parked` (event).
    pub const CHAIN_PARKED: u16 = 51;
    /// `chain.unparked` (event).
    pub const CHAIN_UNPARKED: u16 = 52;
    /// `state.applied` (event).
    pub const STATE_APPLIED: u16 = 53;
    /// `state.verified` (event).
    pub const STATE_VERIFIED: u16 = 54;
    /// `latency.changed` (event, spontaneous).
    pub const LATENCY_CHANGED: u16 = 55;
    /// `editor.closed` (event).
    pub const EDITOR_CLOSED: u16 = 56;
    /// `editor.availability` (event, spontaneous).
    pub const EDITOR_AVAILABILITY: u16 = 57;
    /// `log` (event, spontaneous): one of the child's log lines.
    pub const LOG: u16 = 58;
    /// `workgroup.attached` (event).
    pub const WORKGROUP_ATTACHED: u16 = 59;
    /// Protocol 3.1: a slot's `restartComponent(kLatencyChanged)` cycle ended.
    pub const SLOT_REACTIVATED: u16 = 60;
    /// (protocol 3.8): `chain.prepareFresh`'s completion ([`super::ChainPreparedFresh`]).
    pub const CHAIN_PREPARED_FRESH: u16 = 61;
    // Scan child only.
    /// `scan` (host to scan child): probe one bundle.
    pub const SCAN: u16 = 64;
    /// `scanned` (scan child to host, event): the probe's one answer.
    pub const SCANNED: u16 = 65;
}

/// Every frame kind's wire name and code.
pub const KINDS: &[(&str, u16)] = &[
    ("hello", kind::HELLO),
    ("ping", kind::PING),
    ("state.generations", kind::STATE_GENERATIONS),
    ("shutdown", kind::SHUTDOWN),
    ("log.set", kind::LOG_SET),
    ("chain.load", kind::CHAIN_LOAD),
    ("chain.unload", kind::CHAIN_UNLOAD),
    ("chain.prepare", kind::CHAIN_PREPARE),
    ("chain.park", kind::CHAIN_PARK),
    ("chain.unpark", kind::CHAIN_UNPARK),
    ("state.set", kind::STATE_SET),
    ("editor.open", kind::EDITOR_OPEN),
    ("editor.close", kind::EDITOR_CLOSE),
    ("workgroup.attach", kind::WORKGROUP_ATTACH),
    ("state.get", kind::STATE_GET),
    ("param.list", kind::PARAM_LIST),
    ("slot.setEnabled", kind::SLOT_SET_ENABLED),
    ("param.set", kind::PARAM_SET),
    ("chain.prepareFresh", kind::CHAIN_PREPARE_FRESH),
    ("chain.preparedFresh", kind::CHAIN_PREPARED_FRESH),
    ("chain.loaded", kind::CHAIN_LOADED),
    ("chain.unloaded", kind::CHAIN_UNLOADED),
    ("chain.prepared", kind::CHAIN_PREPARED),
    ("chain.parked", kind::CHAIN_PARKED),
    ("chain.unparked", kind::CHAIN_UNPARKED),
    ("state.applied", kind::STATE_APPLIED),
    ("state.verified", kind::STATE_VERIFIED),
    ("latency.changed", kind::LATENCY_CHANGED),
    ("editor.closed", kind::EDITOR_CLOSED),
    ("editor.availability", kind::EDITOR_AVAILABILITY),
    ("log", kind::LOG),
    ("workgroup.attached", kind::WORKGROUP_ATTACHED),
    ("slot.reactivated", kind::SLOT_REACTIVATED),
    ("scan", kind::SCAN),
    ("scanned", kind::SCANNED),
];

/// A kind's wire name (`None` for a code not in [`KINDS`]).
pub fn kind_name(k: u16) -> Option<&'static str> {
    KINDS.iter().find(|(_, c)| *c == k).map(|(n, _)| *n)
}

fn is_request_kind(k: u16) -> bool {
    matches!(k, 1..=5 | 16..=25 | 32..=35 | 64)
}

fn is_event_kind(k: u16) -> bool {
    matches!(k, 48..=61 | 65)
}

/// Why a frame was refused. Every one closes the socket (there is no resync).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtoError {
    /// The frame length does not equal `20 + meta_len + blob_len`.
    FrameLen {
        /// The header's frame length.
        frame_len: u32,
        /// The header's meta length.
        meta: u32,
        /// The header's blob length.
        blob: u32,
    },
    /// The meta is over [`MAX_META`].
    MetaTooLong(u32),
    /// The blob is over [`MAX_BLOB`].
    BlobTooLong(u32),
    /// A kind not in the catalogue, or not one this direction reads.
    UnknownKind(u16),
    /// Flags that are unknown or wrong for the kind.
    BadFlags {
        /// The frame's kind.
        kind: u16,
        /// The frame's flags.
        flags: u16,
    },
    /// An id that is wrong for the kind (an event's must be 0, a request's above 0).
    BadId {
        /// The frame's kind.
        kind: u16,
        /// The frame's id.
        id: u64,
    },
    /// The meta is not a JSON object of the expected shape (the parser's message).
    BadMeta(String),
    /// A handle count the frame may not carry.
    Fds(&'static str),
    /// A header that passed every check, refused by the caller's admission gate ([`Decoder::feed_admitting`]) before
    /// its body was allocated: the scan reader's blob budget.
    NotAdmitted {
        /// The frame's kind.
        kind: u16,
        /// The header's blob length.
        blob_len: u32,
    },
}

/// A malformed meta prints its parser's message as a sentence (`malformed meta: <message>`); every other refusal
/// prints as its `Debug` (the logs' form of before).
impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::BadMeta(m) => write!(f, "malformed meta: {m}"),
            e => write!(f, "{e:?}"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Which side decodes: the host application reads replies and events; the child reads requests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The host application decodes: replies and events.
    ToHost,
    /// The child decodes: requests.
    ToChild,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A frame's 24-byte header, decoded.
pub struct Header {
    /// The frame kind (`kind::*`).
    pub kind: u16,
    /// The flags (`FF_*`).
    pub flags: u16,
    /// The request id (0 for an event).
    pub id: u64,
    /// The meta's length in bytes.
    pub meta_len: u32,
    /// The blob's length in bytes.
    pub blob_len: u32,
}

impl Header {
    /// Whether `FF_REPLY` is set.
    pub fn is_reply(&self) -> bool {
        self.flags & FF_REPLY != 0
    }
    /// Whether `FF_EVENT` is set.
    pub fn is_event(&self) -> bool {
        self.flags & FF_EVENT != 0
    }
    /// Whether `FF_ERROR` is set.
    pub fn is_error(&self) -> bool {
        self.flags & FF_ERROR != 0
    }
    /// Whether `FF_FDS` is set.
    pub fn carries_fds(&self) -> bool {
        self.flags & FF_FDS != 0
    }

    /// How many handles ride with the frame: 0 or 1 (the `FF_FDS` bit). The wire carries a count, never a handle's
    /// value.
    pub fn handles(&self) -> u8 {
        u8::from(self.carries_fds())
    }
}

/// Decode and validate a header (caps, the length identity, the kind, the flags for `dir`, the id rule).
pub fn decode_header(b: &[u8; FRAME_HEADER], dir: Direction) -> Result<Header, ProtoError> {
    decode_header_capped(b, dir, MAX_META, MAX_BLOB)
}

/// [`decode_header`] with the caps `max_meta` and `max_blob`: [`MAX_META`] and [`MAX_BLOB`] for every frame read;
/// lifted only by [`Decoder::oversized_meta_header`] (the meta cap) and [`Decoder::oversized_header`] (both), which
/// classify a refusal and allocate nothing.
fn decode_header_capped(
    b: &[u8; FRAME_HEADER],
    dir: Direction,
    max_meta: usize,
    max_blob: usize,
) -> Result<Header, ProtoError> {
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let frame_len = u32_at(F_FRAME_LEN);
    let kind = u16::from_le_bytes([b[F_KIND], b[F_KIND + 1]]);
    let flags = u16::from_le_bytes([b[F_FLAGS], b[F_FLAGS + 1]]);
    let mut id = [0u8; 8];
    id.copy_from_slice(&b[F_ID..F_ID + 8]);
    let id = u64::from_le_bytes(id);
    let meta_len = u32_at(F_META_LEN);
    let blob_len = u32_at(F_BLOB_LEN);
    if meta_len as usize > max_meta {
        return Err(ProtoError::MetaTooLong(meta_len));
    }
    if blob_len as usize > max_blob {
        return Err(ProtoError::BlobTooLong(blob_len));
    }
    if frame_len as u64 != 20 + meta_len as u64 + blob_len as u64 {
        return Err(ProtoError::FrameLen {
            frame_len,
            meta: meta_len,
            blob: blob_len,
        });
    }
    if kind_name(kind).is_none() {
        return Err(ProtoError::UnknownKind(kind));
    }
    if flags & !FF_KNOWN != 0 {
        return Err(ProtoError::BadFlags { kind, flags });
    }
    let reply = flags & FF_REPLY != 0;
    let event = flags & FF_EVENT != 0;
    let ok = match dir {
        // The child sends replies (to request kinds) and events (event kinds); never descriptors.
        Direction::ToHost => {
            flags & FF_FDS == 0
                && ((reply && !event && is_request_kind(kind))
                    || (event && !reply && flags & FF_ERROR == 0 && is_event_kind(kind)))
        }
        // The host application sends requests only.
        Direction::ToChild => !reply && !event && flags & FF_ERROR == 0 && is_request_kind(kind),
    };
    // The mutation checks (never in a normal build): the flag rule skipped for `scanned` (65) or for `scan` (64). The
    // codec tests' negative cases (`scanned` as a reply, `scan` as an event or a reply) must fail on each.
    #[cfg(mutant_scanned_flags_ignored)]
    let ok = ok || kind == self::kind::SCANNED;
    #[cfg(mutant_scan_flags_ignored)]
    let ok = ok || kind == self::kind::SCAN;
    if !ok {
        return Err(ProtoError::BadFlags { kind, flags });
    }
    if (event && id != 0) || (!event && id == 0) {
        return Err(ProtoError::BadId { kind, id });
    }
    Ok(Header {
        kind,
        flags,
        id,
        meta_len,
        blob_len,
    })
}

/// The header flag for a frame with `handles` handles attached: none for zero, `FF_FDS` otherwise. A frame carries at
/// most one handle today (the region of a `chain.load`, the page of a `hello`); the channel refuses more.
pub fn handles_flag(handles: usize) -> u16 {
    if handles == 0 { 0 } else { FF_FDS }
}

/// Encode one frame. `meta` must be a JSON object (≤ 64 KiB) and `blob` ≤ 64 MiB; the encoder asserts both (the
/// sender's own bug).
pub fn encode(kind: u16, flags: u16, id: u64, meta: &[u8], blob: &[u8]) -> Vec<u8> {
    assert!(meta.len() <= MAX_META && blob.len() <= MAX_BLOB);
    let mut v = Vec::with_capacity(FRAME_HEADER + meta.len() + blob.len());
    v.extend_from_slice(&header_bytes(
        kind,
        flags,
        id,
        meta.len() as u32,
        blob.len() as u32,
    ));
    v.extend_from_slice(meta);
    v.extend_from_slice(blob);
    v
}

/// The 24 header bytes of a frame with these lengths: `frameLen` = 20 + `meta_len` + `blob_len`, which the
/// caller keeps within `u32`.
pub(crate) fn header_bytes(
    kind: u16,
    flags: u16,
    id: u64,
    meta_len: u32,
    blob_len: u32,
) -> [u8; FRAME_HEADER] {
    let mut b = [0u8; FRAME_HEADER];
    b[F_FRAME_LEN..F_FRAME_LEN + 4].copy_from_slice(&(20 + meta_len + blob_len).to_le_bytes());
    b[F_KIND..F_KIND + 2].copy_from_slice(&kind.to_le_bytes());
    b[F_FLAGS..F_FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
    b[F_ID..F_ID + 8].copy_from_slice(&id.to_le_bytes());
    b[F_META_LEN..F_META_LEN + 4].copy_from_slice(&meta_len.to_le_bytes());
    b[F_BLOB_LEN..F_BLOB_LEN + 4].copy_from_slice(&blob_len.to_le_bytes());
    b
}

/// Encode with a serialisable meta.
pub fn encode_msg<T: Serialize>(kind: u16, flags: u16, id: u64, meta: &T, blob: &[u8]) -> Vec<u8> {
    let m = serde_json::to_vec(meta).expect("meta serialises");
    encode(kind, flags, id, &m, blob)
}

/// One decoded frame: the header, its meta bytes (a JSON object, not yet typed) and its blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The decoded header.
    pub header: Header,
    /// The meta's bytes.
    pub meta: Vec<u8>,
    /// The blob's bytes.
    pub blob: Vec<u8>,
}

impl Frame {
    /// The meta as `T` (`BadMeta` on anything that is not a JSON object of that shape).
    pub fn meta_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, ProtoError> {
        parse_meta(&self.meta)
    }
}

/// `meta` as `T`: an empty meta reads as `{}`; anything that is not a JSON object of that shape is `BadMeta`.
pub fn parse_meta<T: for<'de> Deserialize<'de>>(meta: &[u8]) -> Result<T, ProtoError> {
    let v: serde_json::Value = if meta.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_slice(meta).map_err(|e| ProtoError::BadMeta(e.to_string()))?
    };
    if !v.is_object() {
        return Err(ProtoError::BadMeta("meta is not a JSON object".into()));
    }
    serde_json::from_value(v).map_err(|e| ProtoError::BadMeta(e.to_string()))
}

enum DState {
    Header {
        buf: [u8; FRAME_HEADER],
        have: usize,
    },
    Body {
        header: Header,
        buf: Vec<u8>,
        need: usize,
    },
}

/// The streaming decoder: feed it any chunking of the byte stream; it yields whole frames. After an error it stays
/// failed (the caller closes the socket).
pub struct Decoder {
    dir: Direction,
    state: DState,
    failed: Option<ProtoError>,
}

impl Decoder {
    /// A decoder for one direction of the stream.
    pub fn new(dir: Direction) -> Decoder {
        Decoder {
            dir,
            state: DState::Header {
                buf: [0; FRAME_HEADER],
                have: 0,
            },
            failed: None,
        }
    }

    /// True between frames (a clean place for EOF).
    pub fn at_boundary(&self) -> bool {
        matches!(self.state, DState::Header { have: 0, .. })
    }

    /// After a [`ProtoError::MetaTooLong`] refusal: the refused header as it reads with the meta cap lifted, when it
    /// passes every OTHER check (the blob cap, the length identity, the kind, the flags for this direction, the id
    /// rule). None when the decoder has not failed, failed for anything else, or the header fails another check too.
    /// It reads only the 24 header bytes the decoder kept (nothing of the meta was read or allocated), and the
    /// decoder stays failed. The scan supervisor uses it to tell a `scanned` line refused for its size alone, the
    /// engine's limit, from a child's protocol fault.
    pub fn oversized_meta_header(&self) -> Option<Header> {
        match (&self.failed, &self.state) {
            (Some(ProtoError::MetaTooLong(_)), DState::Header { buf, have })
                if *have == FRAME_HEADER =>
            {
                decode_header_capped(buf, self.dir, usize::MAX, MAX_BLOB).ok()
            }
            _ => None,
        }
    }

    /// After a [`ProtoError::MetaTooLong`] or a
    /// [`ProtoError::BlobTooLong`] refusal, the refused header as it reads with **both** caps lifted, when it passes
    /// every OTHER check (the length identity, the kind, the flags for this direction, the id rule). None when the
    /// decoder has not failed, failed for anything else, or the header fails another check too. Like
    /// [`Self::oversized_meta_header`] it reads only the 24 header bytes the decoder kept (nothing of the body was read
    /// or allocated), and the decoder stays failed. The header cannot show the form, so this holds for form A and B.
    pub fn oversized_header(&self) -> Option<Header> {
        match (&self.failed, &self.state) {
            (Some(ProtoError::MetaTooLong(_)), DState::Header { buf, have })
                if *have == FRAME_HEADER =>
            {
                decode_header_capped(buf, self.dir, usize::MAX, usize::MAX).ok()
            }
            // Its mutant (never in a normal build): a header refused for its blob size is left unclassified, so a
            // `scanned` past MAX_BLOB is struck instead of being the engine's limit (the oversized test's codec half).
            #[cfg(not(mutant_oversized_blob_struck))]
            (Some(ProtoError::BlobTooLong(_)), DState::Header { buf, have })
                if *have == FRAME_HEADER =>
            {
                decode_header_capped(buf, self.dir, usize::MAX, usize::MAX).ok()
            }
            _ => None,
        }
    }

    /// Feed bytes; each whole frame is passed to `out`. The first error is returned again on every later call.
    pub fn feed(&mut self, data: &[u8], out: &mut impl FnMut(Frame)) -> Result<(), ProtoError> {
        self.feed_admitting(data, &mut |_| true, out)
    }

    /// [`Self::feed`] with an admission gate. `admit` sees each header that passed every check, BEFORE its body is
    /// allocated or a byte of it is read; `false` refuses the frame: the decoder fails with
    /// [`ProtoError::NotAdmitted`], nothing of the body is allocated, and it stays failed (the 24 header bytes are all
    /// it keeps). The scan reader's blob budget is such a gate: it judges the header's declared blob length, so a blob
    /// past the budget is never allocated.
    pub fn feed_admitting(
        &mut self,
        mut data: &[u8],
        admit: &mut impl FnMut(&Header) -> bool,
        out: &mut impl FnMut(Frame),
    ) -> Result<(), ProtoError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        while !data.is_empty() {
            match &mut self.state {
                DState::Header { buf, have } => {
                    let n = (FRAME_HEADER - *have).min(data.len());
                    buf[*have..*have + n].copy_from_slice(&data[..n]);
                    *have += n;
                    data = &data[n..];
                    if *have == FRAME_HEADER {
                        let h = match decode_header(buf, self.dir) {
                            Ok(h) => h,
                            Err(e) => {
                                self.failed = Some(e.clone());
                                return Err(e);
                            }
                        };
                        if !admit(&h) {
                            let e = ProtoError::NotAdmitted {
                                kind: h.kind,
                                blob_len: h.blob_len,
                            };
                            self.failed = Some(e.clone());
                            return Err(e);
                        }
                        let need = h.meta_len as usize + h.blob_len as usize;
                        // The caps were checked by decode_header, and the gate has admitted the header: at most
                        // 64 KiB + 64 MiB is reserved here.
                        self.state = DState::Body {
                            header: h,
                            buf: Vec::with_capacity(need),
                            need,
                        };
                        if need == 0 {
                            self.finish(out)?;
                        }
                    }
                }
                DState::Body { buf, need, .. } => {
                    let n = (*need - buf.len()).min(data.len());
                    buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    if buf.len() == *need {
                        self.finish(out)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl FnMut(Frame)) -> Result<(), ProtoError> {
        let st = std::mem::replace(
            &mut self.state,
            DState::Header {
                buf: [0; FRAME_HEADER],
                have: 0,
            },
        );
        if let DState::Body {
            header, mut buf, ..
        } = st
        {
            let blob = buf.split_off(header.meta_len as usize);
            if !buf.is_empty()
                && let Err(e) = std::str::from_utf8(&buf)
            {
                let e = ProtoError::BadMeta(e.to_string());
                self.failed = Some(e.clone());
                return Err(e);
            }
            out(Frame {
                header,
                meta: buf,
                blob,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Typed meta (camelCase on the wire)
// ---------------------------------------------------------------------------------------------------------------------

/// `hello` (supervisor -> child; the child page fd rides with it).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct Hello {
    /// The child's id, chosen by the host application.
    pub child_id: String,
    /// The handles riding with the frame (the child page: 1).
    pub fds: u32,
}

impl Hello {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(child_id: String, fds: u32) -> Hello {
        Hello { child_id, fds }
    }
}

/// The child's `hello` reply.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct HelloReply {
    /// The child's process id.
    pub pid: u32,
    /// The protocol it speaks, `major << 16 | minor` ([`crate::layout::ABI_VERSION`]).
    pub protocol: u32,
    /// The child id it was given, echoed.
    pub child_id: String,
    /// The host's kind: the plugin host sends `"native"`, the test double `"fake"`.
    pub host: String,
    /// The plugin formats it can host (`vst3`, `au`, ...).
    pub formats: Vec<String>,
    /// Route B of the workgroup hand-over: slot 0 of the registered ports held the supervisor's send right.
    pub mach_slot0: bool,
    /// *(protocol 3.6; additive)* The child-wide memory it wired at `hello` (the child page today), in
    /// bytes; never written when 0, so an older line keeps its bytes. Absent (3.5 and older) reads 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub wired_bytes: u64,
    /// The child-wide memory it could not wire. Absent reads 0.
    #[serde(skip_serializing_if = "is_zero")]
    pub unwired_bytes: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// `log.set {levels, expiresInMs?}` (protocol 3.6; kind 5; Inline). `levels` is the resolved word the host application
/// applied (16 areas x 4 bits, each nibble a [`runecraft_contracts::log::Level`], absolute: never a spec, never relative to the child's launch levels);
/// `expiresInMs` the time LEFT of the host application's expiry (absent: until changed). `levels` is required.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LogSet {
    /// The level word to apply.
    pub levels: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The time left of the host application's expiry, in ms (`None`: until changed).
    pub expires_in_ms: Option<u64>,
}

impl LogSet {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(levels: u64, expires_in_ms: Option<u64>) -> LogSet {
        LogSet {
            levels,
            expires_in_ms,
        }
    }
}

/// The longest `log.set` expiry: the host application's log command's bound (86,400 s).
pub const MAX_LOG_SET_EXPIRES_MS: u64 = 86_400_000;

impl LogSet {
    /// The child's checks 2-4 (check 1 is the decode; check 5, a drainer installed, is the child's own): every nibble a
    /// known level, the expiry in `1..=`[`MAX_LOG_SET_EXPIRES_MS`], and the expiry rule (`debug` / `trace` anywhere only with an
    /// expiry). The word and the expiry to apply, or the refusal's reason (the child answers `-32602` with it and
    /// applies nothing). `--cfg mutant_log_set_no_s7` (never in a normal build) skips the expiry rule; the codec test fails.
    pub fn check(&self) -> Result<(u64, Option<std::time::Duration>), &'static str> {
        let nibble = |i: u32| (self.levels >> (i * 4)) & 0xF;
        let known = (0..16).all(|i| nibble(i) <= Level::Trace as u64);
        if !known {
            return Err("log.set: a levels nibble is not a known level");
        }
        if let Some(ms) = self.expires_in_ms
            && !(1..=MAX_LOG_SET_EXPIRES_MS).contains(&ms)
        {
            return Err("log.set: expiresInMs must be in 1..=86400000");
        }
        let above_info = (0..16).any(|i| nibble(i) > Level::Info as u64);
        if above_info && self.expires_in_ms.is_none() && !cfg!(mutant_log_set_no_s7) {
            return Err("log.set: debug or trace levels need expiresInMs (they expire)");
        }
        Ok((
            self.levels,
            self.expires_in_ms.map(std::time::Duration::from_millis),
        ))
    }
}

/// `log.set`'s reply, once applied: the word now in effect (required).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LogSetReply {
    /// The level word now in effect.
    pub levels: u64,
}

impl LogSetReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(levels: u64) -> LogSetReply {
        LogSetReply { levels }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// One slot of a `chain.load`: the plugin to load, and whether it is enabled.
#[non_exhaustive]
pub struct SlotDesc {
    /// The plugin class to load (protocol 4.0). `None` (absent): a quarantined slot, not loaded, which keeps its
    /// slot number and passes audio through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PluginRef>,
    /// Protocol 3.2: a disabled slot, kept loaded and bypassed (its latency stays in PDC; its
    /// output is its input delayed by it). Absent = `true`, and written
    /// only when `false`, so a 3.1 line keeps its bytes.
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
}

impl SlotDesc {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(plugin: Option<PluginRef>, enabled: bool) -> SlotDesc {
        SlotDesc { plugin, enabled }
    }
}

impl Default for SlotDesc {
    fn default() -> SlotDesc {
        SlotDesc {
            plugin: None,
            enabled: true,
        }
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

/// `slot.setEnabled {chainKey, slot, enabled}` (protocol 3.2, kind 34, MainThread; every field required).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SlotEnabled {
    /// The chain.
    pub chain_key: u64,
    /// The slot's index in the chain.
    pub slot: u32,
    /// Whether the slot runs (`false`: bypassed, kept loaded).
    pub enabled: bool,
}

impl SlotEnabled {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, enabled: bool) -> SlotEnabled {
        SlotEnabled {
            chain_key,
            slot,
            enabled,
        }
    }
}

/// `slot.setEnabled`'s reply, once applied: the request echoed and `ok` (`false`: no such chain or slot; no error
/// frame).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct SlotEnabledReply {
    /// The chain, echoed.
    pub chain_key: u64,
    /// The slot, echoed.
    pub slot: u32,
    /// The requested state, echoed.
    pub enabled: bool,
    /// `false`: no such chain or slot.
    pub ok: bool,
}

impl SlotEnabledReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, enabled: bool, ok: bool) -> SlotEnabledReply {
        SlotEnabledReply {
            chain_key,
            slot,
            enabled,
            ok,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// What a chain is for.
#[non_exhaustive]
pub enum Role {
    #[default]
    /// A live chain, run by the audio callback.
    Live,
    /// A render set, run block by block by a render worker.
    Render,
}

/// `chain.load` (the region fd rides with it).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ChainLoad {
    /// The control-assigned chain key.
    pub chain_key: u64,
    /// The region's random id (also in the region's header).
    pub instance_id: u64,
    /// Live or render.
    pub role: Role,
    /// The ring group the chain's thread waits on.
    pub group: u32,
    /// The sample rate.
    pub rate: f64,
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
    /// The slot after which the tap sits ([`crate::layout::NO_TAP`]: none).
    pub tap_slot: u32,
    /// The slots, in order.
    pub slots: Vec<SlotDesc>,
    /// The handles riding with the frame (the region: 1).
    pub fds: u32,
    /// Protocol 4.1, **Windows only, additive**: this chain's FanOut waiter slot
    /// in its ring group (0..64), whose event rides among the frame's handles. Never written on unix (absent), so a
    /// macOS `chain.load` keeps its bytes; a unix child ignores it.
    #[serde(skip_serializing_if = "fan_slot_absent")]
    pub fan_slot: Option<u8>,
}

/// `chain.load`'s `fanSlot` is written only when set. The mutation check (never in a normal build):
/// `--cfg mutant_fan_slot_always` writes it as `null` when absent; the 4.0 frame-bytes golden must fail.
fn fan_slot_absent(slot: &Option<u8>) -> bool {
    slot.is_none() && !cfg!(mutant_fan_slot_always)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// One slot's result in `chain.loaded`.
#[non_exhaustive]
pub struct SlotInfo {
    /// Whether the plugin loaded.
    pub ok: bool,
    /// The plugin's name.
    pub name: String,
    /// The plugin's channel count.
    pub channels: u32,
    /// The slot's own latency in frames (a quarantined slot reports 0).
    pub latency: u32,
    /// The slot's parameter count (a quarantined slot reports 0).
    pub param_count: u32,
    /// Protocol 3.4 (`editor-windows.md` gap 4; additive): whether the slot has an editor, as the child knows it
    /// at load without creating a view (VST3: true unless the plugin declares none, the C++ proxy's "probably"; AU:
    /// always, the generic view). Absent = `true` (every 3.3 child; the C++ proxy's default when it does not know), and
    /// written only when `false`, so a 3.3 line keeps its bytes. A later `editor.availability` corrects it.
    #[serde(skip_serializing_if = "is_true")]
    pub has_editor: bool,
    /// Protocol 3.9 (additive): the slot's notices, codes the child names it by (a
    /// generic carrier: the codes are values, e.g. an instrument that refused its inputs at prepare). The host application keeps
    /// at most [`MAX_SLOT_NOTICES`] valid codes ([`slot_notices_clamped`]). Absent = none (every 3.8 child), and
    /// written only when not empty, so a 3.8 line keeps its bytes; an older host application ignores it (no meta is
    /// `deny_unknown_fields`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notices: Vec<String>,
}

impl SlotInfo {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        ok: bool,
        name: String,
        channels: u32,
        latency: u32,
        param_count: u32,
        has_editor: bool,
        notices: Vec<String>,
    ) -> SlotInfo {
        SlotInfo {
            ok,
            name,
            channels,
            latency,
            param_count,
            has_editor,
            notices,
        }
    }
}

/// The most notices the host application keeps per slot.
pub const MAX_SLOT_NOTICES: usize = 8;
/// A notice code's longest length in bytes.
pub const MAX_NOTICE_BYTES: usize = 64;

/// A child's slot notices as the host application keeps them: codes of 1 to [`MAX_NOTICE_BYTES`] ASCII letters and
/// digits (anything else dropped: the child is untrusted, and a code is echoed into JSON replies), first occurrence
/// kept, at most [`MAX_SLOT_NOTICES`].
pub fn slot_notices_clamped(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in v {
        if out.len() == MAX_SLOT_NOTICES {
            break;
        }
        let valid = !n.is_empty()
            && n.len() <= MAX_NOTICE_BYTES
            && n.bytes().all(|b| b.is_ascii_alphanumeric());
        if valid && !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}

impl Default for SlotInfo {
    fn default() -> SlotInfo {
        SlotInfo {
            ok: false,
            name: String::new(),
            channels: 0,
            latency: 0,
            param_count: 0,
            has_editor: true,
            notices: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `chain.loaded`: the result of a `chain.load`.
#[non_exhaustive]
pub struct ChainLoaded {
    /// The chain.
    pub chain_key: u64,
    /// Whether the chain loaded (a quarantined slot does not make it fail).
    pub ok: bool,
    /// Each slot's result, in order.
    pub slots: Vec<SlotInfo>,
    /// The chain's latency in frames.
    pub latency: u32,
    /// The chain's parameter count.
    pub param_count: u32,
    /// *(Additive)* The chain's memory the child wired (its region, its arena blocks, the chain thread's
    /// stack top), in bytes. Absent (an older child) reads 0.
    pub wired_bytes: u64,
    /// The chain's memory the child could not wire (refused, or past its arena's soft cap). Absent reads 0.
    pub unwired_bytes: u64,
}

impl ChainLoaded {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        chain_key: u64,
        ok: bool,
        slots: Vec<SlotInfo>,
        latency: u32,
        param_count: u32,
        wired_bytes: u64,
        unwired_bytes: u64,
    ) -> ChainLoaded {
        ChainLoaded {
            chain_key,
            ok,
            slots,
            latency,
            param_count,
            wired_bytes,
            unwired_bytes,
        }
    }
}

/// `chain.unload`, `chain.prepare`, `chain.park` and their events; `editor.*` without a slot.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ChainRef {
    /// The chain.
    pub chain_key: u64,
    /// Whether the operation succeeded.
    pub ok: bool,
}

impl ChainRef {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, ok: bool) -> ChainRef {
        ChainRef { chain_key, ok }
    }
}

/// `chain.prepared` (a [`ChainRef`] on the wire up to 3.5). *(protocol 3.6; additive)* The chain's wired
/// and unwired memory after the prepare (the same sums as `chain.loaded`'s), replacing its pair. **Absent keeps the
/// pair `chain.loaded` gave** (never 0), so a 3.5 child's prepare changes nothing; never written when `None`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ChainPrepared {
    /// The chain.
    pub chain_key: u64,
    /// Whether the prepare succeeded.
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The chain's wired memory after the prepare, in bytes (`None`: keep `chain.loaded`'s).
    pub wired_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The chain's unwired memory after the prepare, in bytes (`None`: keep `chain.loaded`'s).
    pub unwired_bytes: Option<u64>,
    /// *(protocol 3.9; additive)* Each slot's notices after the prepare, by slot index (a missing index: none),
    /// **replacing** the set `chain.loaded`'s [`SlotInfo::notices`] gave: the child reports a slot's whole current set
    /// (a prepare is where an AU instrument may refuse its inputs). Absent = no slot has one (every 3.8 child), so a
    /// 3.8 line keeps its bytes. Fresh prepares carry their own optional snapshot since 3.10.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub slot_notices: Vec<Vec<String>>,
}

impl ChainPrepared {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        chain_key: u64,
        ok: bool,
        wired_bytes: Option<u64>,
        unwired_bytes: Option<u64>,
        slot_notices: Vec<Vec<String>>,
    ) -> ChainPrepared {
        ChainPrepared {
            chain_key,
            ok,
            wired_bytes,
            unwired_bytes,
            slot_notices,
        }
    }
}

/// `chain.preparedFresh` (protocol 3.8; kind 61): `chain.prepareFresh`'s completion. `ok` false: the chain is not a
/// prepared render set (a live chain is refused, as an unknown key), `error` why. `ms` is the child's own time for
/// the release and prepare of every slot, `latency` the chain's latency after it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ChainPreparedFresh {
    /// The chain.
    pub chain_key: u64,
    /// Whether the fresh prepare succeeded.
    pub ok: bool,
    /// The child's own time for it, in ms.
    pub ms: f64,
    /// The chain's latency after it, in frames.
    pub latency: u32,
    /// 3.10: complete successful-prepare snapshot. None is unknown, Some([]) explicitly empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_notices: Option<Vec<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Why it failed.
    pub error: Option<String>,
}

impl ChainPreparedFresh {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        chain_key: u64,
        ok: bool,
        ms: f64,
        latency: u32,
        slot_notices: Option<Vec<Vec<String>>>,
        error: Option<String>,
    ) -> ChainPreparedFresh {
        ChainPreparedFresh {
            chain_key,
            ok,
            ms,
            latency,
            slot_notices,
            error,
        }
    }
}

// ---- `corr` on control frames (protocol 3.6) ------------------------------------------

/// Why [`with_corr`] added no key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorrRefused {
    /// Not the correlation id grammar ([`corr_ok`]).
    Invalid,
    /// A zero-length meta (a meta-less frame): never given a key.
    MetaLess,
    /// Not a JSON object's bytes (no codec meta is; a guard, not a path).
    NotObject,
    /// The key would take the meta past [`MAX_META`]: the encoder asserts the cap, so the key is dropped instead.
    TooLong,
}

/// The bytes `,"corr":"…"` add at most: 9 + [`MAX_CORR_BYTES`] + 1.
pub const CORR_KEY_BYTES: usize = 10 + MAX_CORR_BYTES;

/// `meta` with a top-level `"corr"` key added, the one splice every sender uses. Total over every meta
/// form: `{}` (and `{ }`) becomes `{"corr":"…"}`, any other object gets `,"corr":"…"` before its closing brace, and
/// a zero-length meta, a non-object or a meta that would pass [`MAX_META`] is left as it was (the reason returned).
/// `corr` is in the correlation id grammar, so nothing needs escaping. No codec struct names `corr` (a unit test per kind), so the key
/// is never duplicated. `--cfg mutant_corr_no_cap` (never in a normal build) skips the cap check; the near-cap
/// test then panics in [`encode`].
pub fn with_corr(meta: &mut Vec<u8>, corr: &str) -> Result<(), CorrRefused> {
    if !corr_ok(corr) {
        return Err(CorrRefused::Invalid);
    }
    if meta.is_empty() {
        return Err(CorrRefused::MetaLess);
    }
    let ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let (Some(open), Some(close)) = (
        meta.iter().position(|b| !ws(b)),
        meta.iter().rposition(|b| !ws(b)),
    ) else {
        return Err(CorrRefused::NotObject);
    };
    if meta[open] != b'{' || meta[close] != b'}' || close == open {
        return Err(CorrRefused::NotObject);
    }
    let empty = meta[open + 1..close].iter().all(ws);
    let add = corr.len() + if empty { 9 } else { 10 };
    if meta.len() + add > MAX_META && !cfg!(mutant_corr_no_cap) {
        return Err(CorrRefused::TooLong);
    }
    let mut key = Vec::with_capacity(add);
    if !empty {
        key.push(b',');
    }
    key.extend_from_slice(b"\"corr\":\"");
    key.extend_from_slice(corr.as_bytes());
    key.push(b'"');
    if empty {
        meta.truncate(open + 1);
        meta.extend_from_slice(&key);
        meta.push(b'}');
    } else {
        meta.splice(close..close, key);
    }
    Ok(())
}

/// echo set: whether a child -> supervisor frame answers a request, so it may carry that request's `corr`: a reply
/// (to an Inline or MainThread request; a ReceiptAck's receipt carries none, but is a reply all the same), or the
/// completion event of a ReceiptAck request. Spontaneous events (`latency.changed`, `slot.reactivated`,
/// `editor.availability`, `log`) never do. `editor.closed` is in the set because it completes `editor.close`; the
/// child echoes only on that one (never on a close the user made).
pub fn answers_a_request(h: &Header) -> bool {
    use kind::*;
    h.is_reply()
        || matches!(
            h.kind,
            CHAIN_LOADED
                | CHAIN_UNLOADED
                | CHAIN_PREPARED
                | CHAIN_PREPARED_FRESH
                | CHAIN_PARKED
                | CHAIN_UNPARKED
                | STATE_APPLIED
                | EDITOR_CLOSED
                | WORKGROUP_ATTACHED
        )
}

/// What a frame's top-level `corr` says ([`corr_of_meta`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CorrRead {
    /// No `corr` key.
    Absent,
    /// A `corr` in the correlation id grammar.
    Valid(String),
    /// Present but not in the grammar (not a string, empty, too long, a byte outside it, null): ignored and counted by
    /// the reader, never an error (the key is advisory).
    Invalid,
}

/// The lenient reader of a meta's top-level `corr`, the same on both sides: it never fails the frame. A
/// meta without the bytes `"corr"` is not parsed again.
pub fn corr_of_meta(meta: &[u8]) -> CorrRead {
    if !meta.windows(6).any(|w| w == b"\"corr\"") {
        return CorrRead::Absent;
    }
    match serde_json::from_slice::<serde_json::Value>(meta) {
        Ok(serde_json::Value::Object(o)) => match o.get("corr") {
            None => CorrRead::Absent,
            Some(serde_json::Value::String(c)) if corr_ok(c) => CorrRead::Valid(c.clone()),
            Some(_) => CorrRead::Invalid,
        },
        // Not an object: the typed decode refuses the frame on its own; no corr.
        _ => CorrRead::Absent,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `chain.unpark`: a parked chain's thread waits again, on `group`.
#[non_exhaustive]
pub struct ChainUnpark {
    /// The chain.
    pub chain_key: u64,
    /// The ring group to wait on.
    pub group: u32,
}

impl ChainUnpark {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, group: u32) -> ChainUnpark {
        ChainUnpark { chain_key, group }
    }
}

/// The `state.set` mode (protocol 3.1).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum StateMode {
    /// The one production mode: the blob is state blob format v1 ([`crate::state_blob`], magic `53 41 53 50`). Every
    /// package blob is one, and the re-apply path sends it. Its wire word is `sasp`.
    #[default]
    #[serde(rename = "sasp")]
    V1,
    /// Any other word, 3.0's struck `raw` included: a 3.1 child answers `state.applied {ok: false}`, and nothing
    /// reaches the plugin.
    #[serde(other)]
    Unknown,
}

/// `state.set` (the blob is the state).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct StateSet {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    #[serde(rename = "gen")]
    /// The host application's generation of this state (`gen` on the wire).
    pub generation: u64,
    /// The blob's mode.
    pub mode: StateMode,
}

impl StateSet {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, generation: u64, mode: StateMode) -> StateSet {
        StateSet {
            chain_key,
            slot,
            generation,
            mode,
        }
    }
}

/// `state.applied` / `state.verified`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct StateApplied {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    #[serde(rename = "gen")]
    /// The generation the set carried (`gen` on the wire).
    pub generation: u64,
    /// Whether the whole set applied.
    pub ok: bool,
    /// Protocol 3.1, additive: sent only on a **partial** vst3 set (with `ok:
    /// false`): which chunks reached the plugin before the set failed (the component's `setState` applied, then the
    /// controller's failed). The plugin has changed and cannot be rolled back. Absent: today's meaning (`ok: false`
    /// = nothing applied).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied: Option<Applied>,
    /// Protocol 3.3, additive: the child's own generation of the slot at the moment the
    /// applied state took effect (the host application's capture baseline). Absent from a 3.2 child, and never sent
    /// by the host application.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_generation: Option<u64>,
}

impl StateApplied {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        chain_key: u64,
        slot: u32,
        generation: u64,
        ok: bool,
        applied: Option<Applied>,
        child_generation: Option<u64>,
    ) -> StateApplied {
        StateApplied {
            chain_key,
            slot,
            generation,
            ok,
            applied,
            child_generation,
        }
    }
}

/// Which chunks of a vst3 set reached the plugin ([`StateApplied::applied`]).
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct Applied {
    /// The component chunk reached the plugin.
    pub component: bool,
    /// The controller chunk reached the plugin.
    pub controller: bool,
}

impl Applied {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(component: bool, controller: bool) -> Applied {
        Applied {
            component,
            controller,
        }
    }
}

/// What a `state.applied` says about the plugin ([`StateApplied::classify`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AppliedKind {
    /// No `applied`: `ok: true`, the set applied whole; `ok: false`, nothing reached the plugin.
    Whole,
    /// `ok: false` with exactly one chunk applied: the plugin changed and cannot be rolled back.
    Partial,
    /// Any other combination (`applied` with `ok: true`, `{true, true}`, `{false, false}`): a protocol error,
    /// counted (`stateAppliedInvalid`) and logged; the slot is still reported moved.
    Invalid,
}

impl StateApplied {
    /// `applied` appears only with `ok: false` and with exactly one of the two true (`{true, true}`
    /// is `ok: true` with no `applied`; `{false, false}` is no `applied`).
    pub fn classify(&self) -> AppliedKind {
        match (self.ok, self.applied) {
            (_, None) => AppliedKind::Whole,
            (false, Some(a)) if a.component != a.controller => AppliedKind::Partial,
            _ => AppliedKind::Invalid,
        }
    }

    /// A set that failed after changing the plugin: never "nothing changed".
    pub fn partial(&self) -> bool {
        self.classify() == AppliedKind::Partial
    }
}

/// `state.get` (request) and its reply (`childGen` + the blob).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct StateGet {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// Whether the child may answer from its cached copy.
    pub cached: bool,
}

impl StateGet {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, cached: bool) -> StateGet {
        StateGet {
            chain_key,
            slot,
            cached,
        }
    }
}

/// The reply to `state.get`. Protocol 3.3 (additive): a child answers a state over
/// [`MAX_BLOB`] with `{ok: false, reason: "tooLarge", bytes}` and no blob, keeping `chainKey`, `slot` and `childGen`.
/// A 3.2 reply has none of the three and decodes as `ok: true`; an `ok: true` reply encodes byte for byte as 3.2's.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct StateGetReply {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The child's generation of the slot when it read the state.
    pub child_gen: u64,
    /// Absent: `true` (every 3.2 reply). Encoded only when false.
    #[serde(skip_serializing_if = "is_true")]
    pub ok: bool,
    /// Why `ok` is false: `"tooLarge"` (the one reason 3.3 defines; any other word is still a failed read).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// With `tooLarge`: the framed state blob's size the child measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl StateGetReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        chain_key: u64,
        slot: u32,
        child_gen: u64,
        ok: bool,
        reason: Option<String>,
        bytes: Option<u64>,
    ) -> StateGetReply {
        StateGetReply {
            chain_key,
            slot,
            child_gen,
            ok,
            reason,
            bytes,
        }
    }
}

impl Default for StateGetReply {
    fn default() -> Self {
        Self {
            chain_key: 0,
            slot: 0,
            child_gen: 0,
            ok: true,
            reason: None,
            bytes: None,
        }
    }
}

impl StateGetReply {
    /// The one reason protocol 3.3 names for `ok: false`.
    pub const TOO_LARGE: &'static str = "tooLarge";

    /// A 3.3 `tooLarge` answer: the state did not fit a frame, and the child measured `bytes`.
    pub fn too_large(&self) -> Option<u64> {
        (!self.ok && self.reason.as_deref() == Some(Self::TOO_LARGE))
            .then_some(self.bytes.unwrap_or(0))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
/// One slot's entry in a [`Generations`] reply.
#[non_exhaustive]
pub struct GenEntry {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The child's generation of the slot (it moves when the plugin's state may have changed).
    pub child_gen: u64,
    /// Whether the slot's editor is open.
    pub editor_open: bool,
}

impl GenEntry {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, child_gen: u64, editor_open: bool) -> GenEntry {
        GenEntry {
            chain_key,
            slot,
            child_gen,
            editor_open,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// The reply to `state.generations`.
#[non_exhaustive]
pub struct Generations {
    /// Every hosted slot's entry.
    pub slots: Vec<GenEntry>,
}

impl Generations {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(slots: Vec<GenEntry>) -> Generations {
        Generations { slots }
    }
}

/// `editor.open` / `editor.close` / `editor.availability`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct EditorRef {
    /// The chain.
    pub chain_key: u64,
    /// The slot (`None`: the chain's every editor).
    pub slot: Option<u32>,
    /// Protocol 3.4 (`editor-windows.md` gap 5; additive), `editor.open` only: arm the editor-reset guard (the
    /// state is read before the view opens and restored if the open resets it). Absent = `false`,
    /// and written only when `true`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub guard_reset: bool,
}

impl EditorRef {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: Option<u32>, guard_reset: bool) -> EditorRef {
        EditorRef {
            chain_key,
            slot,
            guard_reset,
        }
    }
}

/// The editor window a host-embedding child opened, for the host to parent: on Windows the child's own
/// container window (a sandboxed child cannot parent itself into the host's window), which the host makes its child
/// window. A macOS child keeps its window in-child and never sends one. Tagged by platform: `{"hwnd": n}`; an unknown
/// tag is refused.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum EditorWindow {
    /// A Windows `HWND`, as an integer.
    Hwnd(u64),
}

/// The receipt of `editor.open` (protocol 4.1, Windows only, additive): `{}` as ever, or with `editorWindow` when a
/// host-embedding child opened a container for the host to parent.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct EditorOpened {
    /// The container for the host to parent (never on unix: absent, so the receipt keeps its `{}`).
    #[serde(skip_serializing_if = "editor_window_absent")]
    pub editor_window: Option<EditorWindow>,
}

impl EditorOpened {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(editor_window: Option<EditorWindow>) -> EditorOpened {
        EditorOpened { editor_window }
    }
}

/// `editorWindow` is written only when set. The mutation check (never in a normal build):
/// `--cfg mutant_editor_window_always` writes it as `null` when absent; `the_editor_open_receipt_keeps_its_bytes`
/// must fail.
fn editor_window_absent(w: &Option<EditorWindow>) -> bool {
    w.is_none() && !cfg!(mutant_editor_window_always)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `editor.closed`: an editor window closed.
#[non_exhaustive]
pub struct EditorClosed {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The child's generation of the slot after the editor closed.
    pub child_gen: u64,
}

impl EditorClosed {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, child_gen: u64) -> EditorClosed {
        EditorClosed {
            chain_key,
            slot,
            child_gen,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `latency.changed`: a chain's latency moved.
#[non_exhaustive]
pub struct LatencyChanged {
    /// The chain.
    pub chain_key: u64,
    /// The chain's new latency in frames.
    pub latency: u32,
}

impl LatencyChanged {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, latency: u32) -> LatencyChanged {
        LatencyChanged { chain_key, latency }
    }
}

/// `slot.reactivated`: sent by the child at the end of each `restartComponent(kLatencyChanged)` cycle.
/// Untrusted: the supervisor clamps every count and saturates its own.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct SlotReactivated {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The `restartComponent` flags the cycle answered (`kLatencyChanged` = 1 << 3 in the VST3 SDK).
    pub flags: u32,
    /// Blocks this slot passed through (with `ST_BYPASSED`) since its last `slot.reactivated`.
    pub bypassed_blocks: u32,
    /// The cycle's length, the processor out of its cell (`setProcessing(false)` .. `setProcessing(true)`).
    pub cycle_ns: u64,
    /// The slot's latency after the cycle (`getLatencySamples`).
    pub latency: u32,
    /// The main thread took the processor past its bound (the chain thread had not drained): a stall.
    pub forced_take: bool,
    /// Bypassed blocks whose delay line could not hold the slot's latency (a delay of C, not L: `bypassMisaligned`).
    pub misaligned: u32,
    /// The take was given up at `handover::TAKE_GIVE_UP` and completed on a later main-loop turn (a stall).
    pub take_given_up: bool,
    /// The take did not complete within `handover::TAKE_HORIZON`: the cycle never ran, the processor is still out of
    /// its cell, and the slot passes through. The host application quarantines the slot (with its notice) and reloads the chain.
    pub abandoned: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `param.list`: one page of a slot's parameters.
#[non_exhaustive]
pub struct ParamList {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The first entry of the page.
    pub offset: u32,
    /// The most entries of the page.
    pub count: u32,
}

impl ParamList {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, offset: u32, count: u32) -> ParamList {
        ParamList {
            chain_key,
            slot,
            offset,
            count,
        }
    }
}

/// One parameter of a `param.list` page. `id` and `name` are 3.0's. The rest are protocol 3.7's, read by the host
/// application only from a child whose `hello` claims 3.7:
/// an older child's entries default them, and `plugin_id` 0 is a valid id, so presence is never the test.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ParamEntry {
    /// The region id, `slot << 24 | index` (the child's raw index).
    pub id: u32,
    /// The parameter's name (cut at `crate::params::NAME_CHARS` characters).
    pub name: String,
    /// The plugin's own id: VST3 `ParameterInfo.id`, AU `AudioUnitParameterID`.
    pub plugin_id: u32,
    /// VST3 `ParameterInfo::kCanAutomate`; AU not `kAudioUnitParameterFlag_NonRealTime`.
    pub automatable: bool,
    /// Normalized: VST3 `defaultNormalizedValue`; AU `(default - min) / (max - min)` (0 for an empty range).
    pub default: f32,
    /// Normalized, at the reply: an unapplied host set's value, else the plugin's own.
    pub value: f32,
    /// VST3 `stepCount`; AU `max - min` for the `Indexed` / `Boolean` units, else 0.
    pub step_count: u32,
    /// VST3 `ParameterInfo::units`; AU the custom unit's name (`kAudioUnitParameterUnit_CustomUnit`), `%`, `s`, `Hz`,
    /// `dB`, `ms`, else empty.
    pub label: String,
    /// The position in the flattened parameter tree (`summoner_plugin::params`), `u32::MAX` when not in it.
    pub order: u32,
}

/// (protocol 3.7): the leading entries of `entries` whose page stays within `limit` bytes of meta
/// (`crate::params::PAGE_BYTES`), at least one when there is one. The host application pages by the entries it receives, so a short
/// page is never a loss.
pub fn page_within(entries: Vec<ParamEntry>, limit: usize) -> Vec<ParamEntry> {
    // The reply's own fields (chainKey, slot, total, the brackets) and one comma per entry, with room to spare.
    let mut used = 128usize;
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let n = serde_json::to_vec(&e).map_or(usize::MAX / 2, |v| v.len()) + 1;
        if !out.is_empty() && used + n > limit {
            break;
        }
        used += n;
        out.push(e);
    }
    out
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// The reply to `param.list`.
#[non_exhaustive]
pub struct ParamListReply {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The slot's parameter count.
    pub total: u32,
    /// The page's entries.
    pub params: Vec<ParamEntry>,
}

impl ParamListReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, total: u32, params: Vec<ParamEntry>) -> ParamListReply {
        ParamListReply {
            chain_key,
            slot,
            total,
            params,
        }
    }
}

/// `param.set {chainKey, slot, pluginId, value}` (protocol 3.7, kind 35, MainThread; every field required): the
/// value is normalized and clamped to [0, 1] by the child too.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ParamSet {
    /// The chain.
    pub chain_key: u64,
    /// The slot.
    pub slot: u32,
    /// The plugin's own parameter id.
    pub plugin_id: u32,
    /// The normalized value to set.
    pub value: f32,
}

impl ParamSet {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, plugin_id: u32, value: f32) -> ParamSet {
        ParamSet {
            chain_key,
            slot,
            plugin_id,
            value,
        }
    }
}

/// `param.set`'s reply, once set: the request echoed, and `value` the read-back by `ParamEntry.value`'s rule. A
/// refused set (no such chain, slot or parameter, a non-finite value) is an error reply.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ParamSetReply {
    /// The chain, echoed.
    pub chain_key: u64,
    /// The slot, echoed.
    pub slot: u32,
    /// The parameter id, echoed.
    pub plugin_id: u32,
    /// The value read back after the set.
    pub value: f32,
}

impl ParamSetReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(chain_key: u64, slot: u32, plugin_id: u32, value: f32) -> ParamSetReply {
        ParamSetReply {
            chain_key,
            slot,
            plugin_id,
            value,
        }
    }
}

/// `workgroup.attach {token}`: the workgroup port follows as a mach message with the same token. With
/// `bootstrapName` set (route A), the child first looks that name up and sends its channel hello there.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct WorkgroupAttach {
    /// The token the mach message carries too.
    pub token: u64,
    /// Route A: the bootstrap name the child looks up first.
    pub bootstrap_name: Option<String>,
}

impl WorkgroupAttach {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(token: u64, bootstrap_name: Option<String>) -> WorkgroupAttach {
        WorkgroupAttach {
            token,
            bootstrap_name,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `workgroup.attached`: the attach's completion.
#[non_exhaustive]
pub struct WorkgroupAttached {
    /// The token, echoed.
    pub token: u64,
    /// Whether the port arrived and was accepted.
    pub ok: bool,
    /// Whether `os_workgroup_create_with_port` succeeded and the chain threads joined (false: time-constraint only,
    /// counted by the child as a workgroup fallback).
    pub joined: bool,
}

impl WorkgroupAttached {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(token: u64, ok: bool, joined: bool) -> WorkgroupAttached {
        WorkgroupAttached { token, ok, joined }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
/// `log`: one of the child's log lines.
#[non_exhaustive]
pub struct Log {
    /// Its level word.
    pub level: String,
    /// Its text.
    pub message: String,
}

impl Log {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(level: String, message: String) -> Log {
        Log { level, message }
    }
}

/// The meta of a reply with `FF_ERROR`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
#[non_exhaustive]
pub struct ErrorReply {
    /// The error code (JSON-RPC style: `-32601` unknown, `-32602` invalid params, ...).
    pub code: i32,
    /// The error's text.
    pub message: String,
}

impl ErrorReply {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(code: i32, message: String) -> ErrorReply {
        ErrorReply { code, message }
    }
}

// ---- The scan child (protocol 3.1, additive) ------------------------------------------------------------------------
//
// `scan {format, path}` -> `scanned {format, path, outcome, descriptions}`, one plugin per process. The CHILD can
// only ever report `ok` or `failed {reason}`: a probe that crashes or passes its budget sends nothing, so `crashed`
// and `timeout` are the scan supervisor's verdicts, recorded in [`ScanResult`] (an application's scan record entry). The
// outcome is a tagged enum (`"outcome"`) at every step, so `timeout` stays distinct from `crashed` and `failed` from
// the wire into the cache.

/// A plugin format the scan child probes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ScanFormat {
    /// A VST3 bundle.
    Vst3,
    /// An Audio Unit bundle.
    Au,
}

/// `scan` (host -> scan child): probe ONE plugin bundle. `path` is ALWAYS an absolute bundle path, for both formats
/// (`.vst3`; `.component`): an AU identifier could only be resolved through the system
/// registry (`AudioComponentFindNext`), which the child never does. A `.component` scan registers that bundle
/// in-process and reports every `AudioComponents` entry. The identifier form is a LOAD input only. Built only through
/// [`Scan::new`], and decoded only through the same check (`try_from`), so a bad `scan` is refused on both sides.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", try_from = "ScanUnchecked")]
pub struct Scan {
    /// The bundle's format.
    format: ScanFormat,
    /// The bundle's absolute path.
    path: String,
    /// The host application's offer, `descriptionsBlob: 1`, "this host reads form B,
    /// version 1" ([`SCAN_DESCRIPTIONS_BLOB`]). `None` is no offer: never written, so a `scan` without it keeps protocol 3.1's
    /// bytes; absent (or `null`) decodes as `None`. Any integer value decodes; only [`Scan::offers_descriptions_blob`]
    /// says whether it is the version this codec knows. [`Scan::new`] makes no offer (
    /// 2026-11-01: the scan supervisor turns it on in its own branch, with [`Scan::offer_descriptions_blob`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    descriptions_blob: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanUnchecked {
    format: ScanFormat,
    path: String,
    #[serde(default)]
    descriptions_blob: Option<u32>,
}

impl TryFrom<ScanUnchecked> for Scan {
    type Error = String;
    fn try_from(u: ScanUnchecked) -> Result<Scan, String> {
        let s = Scan::new(u.format, u.path).map_err(|e| e.to_string())?;
        Ok(Scan {
            descriptions_blob: u.descriptions_blob,
            ..s
        })
    }
}

impl Scan {
    /// The bundle's path as a path.
    pub fn bundle_path(&self) -> &std::path::Path {
        std::path::Path::new(&self.path)
    }

    /// The bundle's format.
    pub fn format(&self) -> ScanFormat {
        self.format
    }

    /// The bundle's absolute path, as the wire's text.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The host application's form B offer, as it arrived (`None`: no offer). [`Scan::offers_descriptions_blob`] says whether it
    /// is the version this codec writes.
    pub fn descriptions_blob(&self) -> Option<u32> {
        self.descriptions_blob
    }

    /// A scan of one bundle: `path` absolute, with no `.` or `..` segment and no NUL, naming a `.vst3` (VST3) or a
    /// `.component` (AU) bundle, with no trailing `/`. Anything else (an AU identifier string, a relative path, a
    /// directory to search) is refused. "Absolute" is this OS's rule, the segments are split on this OS's separators
    /// (on unix both exactly as before), and a path that is not Unicode is refused.
    pub fn new(format: ScanFormat, path: impl AsRef<std::path::Path>) -> Result<Scan, ProtoError> {
        let path = crate::plugin::bundle_text(path.as_ref())?.to_string();
        let ext = match format {
            ScanFormat::Vst3 => ".vst3",
            ScanFormat::Au => ".component",
        };
        let bad = |why: &str| ProtoError::BadMeta(format!("scan.path {path:?}: {why}"));
        if !crate::plugin::absolute_bundle_path(&path) {
            return Err(bad("not an absolute bundle path"));
        }
        if path.contains('\0') {
            return Err(bad("contains NUL"));
        }
        if crate::plugin::has_dot_segment(&path) {
            return Err(bad("has a . or .. segment"));
        }
        let name = path.rsplit(std::path::is_separator).next().unwrap_or("");
        if name.len() <= ext.len() || !name.ends_with(ext) {
            return Err(bad(&format!("does not name a {ext} bundle")));
        }
        Ok(Scan {
            format,
            path,
            descriptions_blob: None,
        })
    }

    /// The same scan with the form B offer (`descriptionsBlob: 1`). The scan supervisor's branch sends it on every
    /// `scan`; nothing here does yet. (The final name; before host pins it.)
    pub fn offer_descriptions_blob(self) -> Scan {
        Scan {
            descriptions_blob: Some(SCAN_DESCRIPTIONS_BLOB),
            ..self
        }
    }

    /// The child's side of the offer: true when the offer names the form B version this codec writes
    /// ([`SCAN_DESCRIPTIONS_BLOB`]); then the child answers in form B, always, whatever the size. Any other value, or no
    /// offer, is form A (a child never answers with a version it was not offered).
    pub fn offers_descriptions_blob(&self) -> bool {
        self.descriptions_blob == Some(SCAN_DESCRIPTIONS_BLOB)
    }
}

/// The length of an AU's codes in the class grammar (`^[0-9a-f]{8}-[0-9a-f]{8}-[0-9a-f]{8}$`).
pub const AU_CODES_LEN: usize = 26;

/// The three codes in the grammar's AU form.
pub fn au_codes_string(kind: u32, sub: u32, manu: u32) -> String {
    format!("{kind:08x}-{sub:08x}-{manu:08x}")
}

/// The child's rendering of an AU component version (`major.minor.bugfix`: the high 16 bits, then a byte, then a
/// byte), which `version` must equal when `auVersion` is present.
pub fn au_version_string(v: u32) -> String {
    format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff)
}

/// The grammar for an AU's codes, the SHAPE the child formats and nothing else: exactly
/// [`AU_CODES_LEN`] ASCII bytes, three groups of 8 lowercase hex digits joined by `-`. Returns the three codes. The error
/// names the byte's offset, never echoes the value.
///
/// What a code may be (non-zero; printable ASCII, none of `,` `:` `/`) is plugin-data
/// policy, not the wire's: it is the AU index's check (the host application's index of scanned plugins), so a
/// healthy AU whose plist code holds, say, `©` is left out of the index and counted, never refused here as a protocol
/// fault (which would strike it and lose the rest of its bundle).
pub fn check_au_codes(codes: &str) -> Result<[u32; 3], ProtoError> {
    let bad = |m: String| ProtoError::BadMeta(format!("auCodes: {m}"));
    if codes.len() != AU_CODES_LEN {
        return Err(bad(format!(
            "{} bytes, not {AU_CODES_LEN} (three groups of 8 lowercase hex digits joined by '-')",
            codes.len()
        )));
    }
    for (at, &c) in codes.as_bytes().iter().enumerate() {
        let dash = at == 8 || at == 17;
        let ok = if dash {
            c == b'-'
        } else {
            matches!(c, b'0'..=b'9' | b'a'..=b'f')
        };
        if !ok {
            return Err(bad(format!(
                "byte {at} is not {}",
                if dash { "'-'" } else { "a lowercase hex digit" }
            )));
        }
    }
    let mut out = [0u32; 3];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u32::from_str_radix(&codes[i * 9..i * 9 + 8], 16).expect("checked: 8 hex digits");
    }
    Ok(out)
}

/// The digits of a VST3 class id in the class grammar (`^[0-9a-f]{32}$`).
pub const CLASS_ID_DIGITS: usize = 32;

/// The grammar for a VST3 class id: exactly [`CLASS_ID_DIGITS`] ASCII bytes, each `0-9` or `a-f`. An uppercase
/// digit, a short or long value, a `-` (the COM/GUID spelling), any other byte, and the empty string are refused. The
/// error names the length or the first bad byte's offset, never echoes the value.
pub fn check_class_id(id: &str) -> Result<(), ProtoError> {
    if cfg!(mutant_class_id_unchecked) {
        return Ok(());
    }
    if id.len() != CLASS_ID_DIGITS {
        return Err(ProtoError::BadMeta(format!(
            "classId: {} bytes, not {CLASS_ID_DIGITS} lowercase hex digits",
            id.len()
        )));
    }
    match id
        .bytes()
        .position(|b| !matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        None => Ok(()),
        Some(at) => Err(ProtoError::BadMeta(format!(
            "classId: byte {at} is not a lowercase hex digit"
        ))),
    }
}

/// Why a probe failed validation. Known reasons have names; any other word a child sends is
/// [`ScanFailure::Other`], keeping the word, so a newer child never makes an older host application reject the line.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "reason"
)]
#[non_exhaustive]
pub enum ScanFailure {
    /// The bundle or description could not be opened, or no class was found.
    NotFound,
    /// The bundle has no slice for this machine's architecture: refused before any load.
    Arch,
    /// A class did not instantiate or initialise (validation step 1).
    Instantiate,
    /// prepare (48 kHz / 512) or activate failed (step 2).
    Prepare,
    /// The 8 silent blocks (plus one note for an instrument) failed (step 3).
    Process,
    /// The `getState` -> `setState` round trip failed, or its bytes are not a framed state blob (step 4).
    State,
    /// The probe saw a window owned by its pid (a probe shows no UI).
    Ui,
    /// The SUPERVISOR's reason, never the child's: the conversation broke after
    /// the scan's receipt, when plugin code could have been running (a `scanned` that echoes another path, a frame the
    /// decoder refused, a frame out of order). A strike, under its own reason, so a systematic child bug shows as every
    /// plugin striking for the same reason. A `scanned` that names it is refused.
    ProtocolAfterScan,
    /// Any reason this host does not know: `reason` keeps the child's word.
    #[serde(untagged)]
    Other {
        /// The child's reason word.
        reason: String,
    },
}

/// What the scan CHILD reports in `scanned`: only these two (it cannot report its own crash or timeout).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "outcome"
)]
#[non_exhaustive]
pub enum ScannedOutcome {
    /// The bundle validated.
    Ok,
    /// The bundle failed validation.
    Failed {
        #[serde(flatten)]
        /// Which step failed.
        failure: ScanFailure,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        /// The child's detail (absent when empty).
        detail: String,
    },
}

/// `scanned` (scan child -> supervisor). Decoded only through its unchecked twin's check: a line naming the supervisor's
/// own reason (`protocolAfterScan`) is refused, as a child's `crashed` or `timeout` is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", try_from = "ScannedUnchecked")]
#[non_exhaustive]
pub struct Scanned {
    /// The bundle's format.
    pub format: ScanFormat,
    /// The bundle's path, as the `scan` gave it.
    pub path: String,
    #[serde(flatten)]
    /// The outcome.
    pub outcome: ScannedOutcome,
    /// Form A: the classes found, as raw facts (protocol 4.0, [`ScanClass`]); empty unless `outcome` is `ok`. As the
    /// input of [`encode_scanned`]: the classes in discovery order (`ok`: the validated ones; `failed`: discovery's),
    /// which form B carries in its blob and form A writes only on `ok` (`[]` on `failed`).
    #[serde(default)]
    pub classes: Vec<ScanClass>,
}

impl Scanned {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        format: ScanFormat,
        path: String,
        outcome: ScannedOutcome,
        classes: Vec<ScanClass>,
    ) -> Scanned {
        Scanned {
            format,
            path,
            outcome,
            classes,
        }
    }
}

/// `scanned` as it arrives, before [`Scanned`]'s check.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScannedUnchecked {
    format: ScanFormat,
    path: String,
    #[serde(flatten)]
    outcome: ScannedOutcome,
    #[serde(default)]
    classes: Vec<ScanClass>,
}

impl TryFrom<ScannedUnchecked> for Scanned {
    type Error = String;
    fn try_from(u: ScannedUnchecked) -> Result<Scanned, String> {
        if let ScannedOutcome::Failed {
            failure: ScanFailure::ProtocolAfterScan,
            ..
        } = u.outcome
        {
            return Err(
                "protocolAfterScan is the supervisor's reason; a child cannot send it".into(),
            );
        }
        Ok(Scanned {
            format: u.format,
            path: u.path,
            outcome: u.outcome,
            classes: u.classes,
        })
    }
}

/// The scan supervisor's verdict on one path, over the classes `D`: the wire's [`ScanClass`] here ([`ScanResult`]);
/// the host application records it as one scan record entry's outcome, over its own description type. `crashed` and `timeout` exist
/// only here (the child cannot send them), and each is its own tag, never folded into another.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "outcome"
)]
#[non_exhaustive]
pub enum ScanVerdict<D> {
    /// Validated; the classes found.
    Ok {
        /// The classes found.
        descriptions: Vec<D>,
    },
    /// The child reported a failed validation.
    Failed {
        #[serde(flatten)]
        /// Which step failed.
        failure: ScanFailure,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        /// The child's detail (absent when empty).
        detail: String,
    },
    /// The probe died before `scanned` (an OS crash code, or an exit without the line): the dead-man's pedal marks the
    /// path. The OS's code names no OS type (a unix signal or a Windows exception, [`OsCode`]); its name is
    /// [`ScanVerdict::crash_name`], derived and never stored. The stored keys are as ever: `signal` / `exitCode`, and on
    /// Windows `exception`.
    ///
    /// The valid combinations, the only ones the constructors make:
    /// - `os_code: Some(code), exit_code: None` ([`ScanVerdict::crashed_by`]): the probe died by the OS's code (a
    ///   signal, or an exception);
    /// - `os_code: None, exit_code: Some(n)` ([`ScanVerdict::exited_early`]): it exited with status `n` and no
    ///   `scanned` line;
    /// - both `None` ([`ScanVerdict::crashed_unknown`]): it died and no end was read (a pending mark found at load:
    ///   the dead-man's pedal).
    ///
    /// Both `Some` is never made; a stored record is decoded as it is.
    #[non_exhaustive]
    Crashed {
        #[serde(flatten)]
        /// The OS's code of the crash, if the probe died by one.
        os_code: Option<OsCode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        /// The probe's exit code, if it exited.
        exit_code: Option<i32>,
    },
    /// The probe ran out of time: `kind` only. `step`: the child's
    /// own step deadline ended it (exit 75, no `scanned`); the step's name is in the stderr attached to the record.
    /// `budget`: the supervisor's budget (30 s by default) ran out and it SIGKILLed the group.
    Timeout {
        /// Which deadline ended it.
        kind: TimeoutKind,
    },
}

/// The OS's code of a probe's crash: a unix signal, or a Windows unhandled exception (an
/// NTSTATUS). Stored as `{"signal": n}` / `{"exception": n}` beside the verdict's other keys, so a record written
/// before keeps its bytes.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum OsCode {
    /// A unix signal number (never produced on Windows).
    Signal(i32),
    /// A Windows exception code (never produced on unix).
    Exception(u32),
}

impl OsCode {
    /// The code's conventional name, when this table knows it (`SIGSEGV`, `STATUS_ACCESS_VIOLATION`); signal numbers
    /// are this OS's.
    pub fn name(&self) -> Option<&'static str> {
        match *self {
            OsCode::Signal(s) => signal_name(s),
            OsCode::Exception(c) => match c {
                0xC000_0005 => Some("STATUS_ACCESS_VIOLATION"),
                0xC000_001D => Some("STATUS_ILLEGAL_INSTRUCTION"),
                0xC000_0094 => Some("STATUS_INTEGER_DIVIDE_BY_ZERO"),
                0xC000_00FD => Some("STATUS_STACK_OVERFLOW"),
                0xC000_0374 => Some("STATUS_HEAP_CORRUPTION"),
                0xC000_0409 => Some("STATUS_STACK_BUFFER_OVERRUN"),
                0x8000_0003 => Some("STATUS_BREAKPOINT"),
                _ => None,
            },
        }
    }
}

/// The signals a probe dies of, by this OS's numbers.
fn signal_name(s: i32) -> Option<&'static str> {
    Some(match s {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        14 => "SIGALRM",
        15 => "SIGTERM",
        #[cfg(target_os = "macos")]
        10 => "SIGBUS",
        #[cfg(target_os = "macos")]
        12 => "SIGSYS",
        #[cfg(target_os = "linux")]
        7 => "SIGBUS",
        #[cfg(target_os = "linux")]
        31 => "SIGSYS",
        _ => return None,
    })
}

/// The scan supervisor's verdict as the bridge reports it: over the raw classes.
pub type ScanResult = ScanVerdict<ScanClass>;

/// Which deadline a [`ScanResult::Timeout`] was: two kinds of one outcome, each kept distinct from `crashed` and
/// `failed` into the cache.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TimeoutKind {
    /// The child's own step deadline (`--step-ms`, host's default 10 000 ms): exit 75.
    Step,
    /// The supervisor's budget, from the spawn to the leader's exit: its own SIGKILL of the group.
    Budget,
}

impl ScanResult {
    /// The supervisor's record of a `scanned` line the child did send.
    pub fn from_scanned(s: Scanned) -> ScanResult {
        match s.outcome {
            ScannedOutcome::Ok => ScanResult::Ok {
                descriptions: s.classes,
            },
            ScannedOutcome::Failed { failure, detail } => ScanResult::Failed { failure, detail },
        }
    }
}

impl<D> ScanVerdict<D> {
    /// The same verdict over other classes (the host application's description of each): `f` maps an `ok` verdict's list.
    pub fn map_classes<E>(self, f: impl FnOnce(Vec<D>) -> Vec<E>) -> ScanVerdict<E> {
        match self {
            ScanVerdict::Ok { descriptions } => ScanVerdict::Ok {
                descriptions: f(descriptions),
            },
            ScanVerdict::Failed { failure, detail } => ScanVerdict::Failed { failure, detail },
            ScanVerdict::Crashed { os_code, exit_code } => {
                ScanVerdict::Crashed { os_code, exit_code }
            }
            ScanVerdict::Timeout { kind } => ScanVerdict::Timeout { kind },
        }
    }

    /// The probe died by the OS's code (`Crashed { os_code: Some(code), exit_code: None }`).
    pub fn crashed_by(code: OsCode) -> ScanVerdict<D> {
        ScanVerdict::Crashed {
            os_code: Some(code),
            exit_code: None,
        }
    }

    /// The probe exited with status `n` and no `scanned` line (`Crashed { os_code: None, exit_code: Some(n) }`).
    pub fn exited_early(n: i32) -> ScanVerdict<D> {
        ScanVerdict::Crashed {
            os_code: None,
            exit_code: Some(n),
        }
    }

    /// The probe died and no end was read (`Crashed { os_code: None, exit_code: None }`): a pending mark found at
    /// load, the dead-man's pedal.
    pub fn crashed_unknown() -> ScanVerdict<D> {
        ScanVerdict::Crashed {
            os_code: None,
            exit_code: None,
        }
    }

    /// A crash's name: the name of its OS code, derived, never stored. `None` for
    /// another verdict, an exit-code crash, or a code the table does not know.
    pub fn crash_name(&self) -> Option<&'static str> {
        match self {
            ScanVerdict::Crashed {
                os_code: Some(c), ..
            } => c.name(),
            _ => None,
        }
    }

    /// The wire and cache tag: `ok`, `failed`, `crashed` or `timeout`.
    pub fn tag(&self) -> &'static str {
        match self {
            ScanVerdict::Ok { .. } => "ok",
            ScanVerdict::Failed { .. } => "failed",
            ScanVerdict::Crashed { .. } => "crashed",
            ScanVerdict::Timeout { .. } => "timeout",
        }
    }
}

/// Validate a child -> supervisor frame's meta for its kind (the supervisor's dispatch does the same with the typed value).
/// What the fuzz target drives: every frame the decoder yields goes through here.
pub fn check_to_host(f: &Frame) -> Result<(), ProtoError> {
    use kind::*;
    let h = &f.header;
    if h.is_error() {
        return f.meta_as::<ErrorReply>().map(|_| ());
    }
    match h.kind {
        HELLO => f.meta_as::<HelloReply>().map(|_| ()),
        STATE_GENERATIONS => f.meta_as::<Generations>().map(|_| ()),
        STATE_GET => f.meta_as::<StateGetReply>().map(|_| ()),
        PARAM_LIST => f.meta_as::<ParamListReply>().map(|_| ()),
        SLOT_SET_ENABLED => f.meta_as::<SlotEnabledReply>().map(|_| ()),
        PARAM_SET => f.meta_as::<ParamSetReply>().map(|_| ()),
        CHAIN_LOADED => f.meta_as::<ChainLoaded>().map(|_| ()),
        CHAIN_UNLOADED | CHAIN_PARKED => f.meta_as::<ChainRef>().map(|_| ()),
        CHAIN_PREPARED => f.meta_as::<ChainPrepared>().map(|_| ()),
        CHAIN_PREPARED_FRESH => f.meta_as::<ChainPreparedFresh>().map(|_| ()),
        LOG_SET => f.meta_as::<LogSetReply>().map(|_| ()),
        CHAIN_UNPARKED => f.meta_as::<ChainUnpark>().map(|_| ()),
        STATE_APPLIED | STATE_VERIFIED => f.meta_as::<StateApplied>().map(|_| ()),
        LATENCY_CHANGED => f.meta_as::<LatencyChanged>().map(|_| ()),
        EDITOR_CLOSED => f.meta_as::<EditorClosed>().map(|_| ()),
        EDITOR_AVAILABILITY => f.meta_as::<EditorRef>().map(|_| ()),
        LOG => f.meta_as::<Log>().map(|_| ()),
        WORKGROUP_ATTACHED => f.meta_as::<WorkgroupAttached>().map(|_| ()),
        SLOT_REACTIVATED => f.meta_as::<SlotReactivated>().map(|_| ()),
        SCANNED => f.meta_as::<Scanned>().map(|_| ()),
        _ => parse_meta::<serde_json::Value>(&f.meta).map(|_| ()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two frames that carry a handle (a descriptor) to the child keep their bytes: `hello` with the child page and
    /// `chain.load` with its region, header and meta, as the supervisor encodes them (FF_FDS set, `fds: 1`).
    #[test]
    /// The crash verdict's stored bytes are as ever (`signal`, `exitCode`, both, neither); a Windows
    /// exception is the new `exception` key; the name is derived (`SIGSEGV`), never stored.
    /// `--cfg mutant_crash_name_stored` (the name written beside the code) must fail this.
    fn the_crash_verdict_keeps_its_stored_bytes_and_derives_its_name() {
        let rows = [
            (
                Some(OsCode::Signal(11)),
                None,
                r#"{"outcome":"crashed","signal":11}"#,
            ),
            (None, Some(3), r#"{"outcome":"crashed","exitCode":3}"#),
            (
                Some(OsCode::Signal(9)),
                Some(3),
                r#"{"outcome":"crashed","signal":9,"exitCode":3}"#,
            ),
            (None, None, r#"{"outcome":"crashed"}"#),
            (
                Some(OsCode::Exception(0xC000_0005)),
                None,
                r#"{"outcome":"crashed","exception":3221225477}"#,
            ),
        ];
        for (os_code, exit_code, want) in rows {
            let v: ScanResult = ScanVerdict::Crashed { os_code, exit_code };
            let got = serde_json::to_string(&v).unwrap();
            #[cfg(mutant_crash_name_stored)]
            let got = match v.crash_name() {
                Some(n) => got.replacen('}', &format!(",\"name\":\"{n}\"}}"), 1),
                None => got,
            };
            assert_eq!(got, want);
            assert_eq!(
                serde_json::from_str::<ScanResult>(want).unwrap(),
                v,
                "{want}"
            );
        }
        let segv: ScanResult =
            serde_json::from_str(r#"{"outcome":"crashed","signal":11}"#).unwrap();
        assert_eq!(segv.crash_name(), Some("SIGSEGV"));
        let av = ScanResult::Crashed {
            os_code: Some(OsCode::Exception(0xC000_0005)),
            exit_code: None,
        };
        assert_eq!(av.crash_name(), Some("STATUS_ACCESS_VIOLATION"));
        assert_eq!(
            ScanResult::Crashed {
                os_code: None,
                exit_code: Some(3)
            }
            .crash_name(),
            None
        );
    }

    #[test]
    /// The `editor.open` receipt with no window is exactly `{}` (a unix child's, byte for byte), a Windows
    /// container round-trips as `{"editorWindow":{"hwnd":n}}`, and an unknown platform tag is refused.
    fn the_editor_open_receipt_keeps_its_bytes() {
        assert_eq!(
            serde_json::to_string(&EditorOpened::default()).unwrap(),
            "{}"
        );
        let w = EditorOpened {
            editor_window: Some(EditorWindow::Hwnd(0x1234)),
        };
        let j = serde_json::to_string(&w).unwrap();
        assert_eq!(j, r#"{"editorWindow":{"hwnd":4660}}"#);
        assert_eq!(serde_json::from_str::<EditorOpened>(&j).unwrap(), w);
        assert_eq!(
            serde_json::from_str::<EditorOpened>("{}").unwrap(),
            EditorOpened::default()
        );
        assert!(serde_json::from_str::<EditorOpened>(r#"{"editorWindow":{"nsView":1}}"#).is_err());
    }

    #[test]
    /// `fanSlot` is the Windows-only 4.1 addition to `chain.load`. Absent, the
    /// meta has no such key (a unix frame keeps its bytes: the golden below); set, it round-trips; a 4.0 decoder
    /// reads a frame with it (it is lenient about unknown keys) and a frame without it decodes as `None`.
    fn fan_slot_is_written_only_when_set_and_round_trips() {
        let mut m = ChainLoad {
            chain_key: 7,
            fds: 1,
            ..Default::default()
        };
        let absent = serde_json::to_string(&m).unwrap();
        assert!(!absent.contains("fanSlot"), "{absent}");
        m.fan_slot = Some(3);
        let set = serde_json::to_string(&m).unwrap();
        assert!(set.contains("\"fanSlot\":3"), "{set}");
        let back: ChainLoad = serde_json::from_str(&set).unwrap();
        assert_eq!(back.fan_slot, Some(3));
        let old: ChainLoad = serde_json::from_str(&absent).unwrap();
        assert_eq!(old.fan_slot, None);
    }

    #[test]
    fn the_frames_that_carry_a_handle_keep_their_bytes() {
        let hex = |v: &[u8]| v.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let hello = serde_json::to_vec(&Hello {
            child_id: "live".into(),
            fds: 1,
        })
        .unwrap();
        let load = serde_json::to_vec(&ChainLoad {
            chain_key: 7,
            instance_id: 42,
            role: Role::Live,
            group: 1,
            rate: 48_000.0,
            max_frames: 512,
            ch_in: 2,
            ch_out: 2,
            max_midi: 256,
            max_params: 64,
            tap_slot: 0,
            slots: Vec::new(),
            fds: 1,
            fan_slot: None,
        })
        .unwrap();
        let got = [
            hex(&encode(kind::HELLO, FF_FDS, 1, &hello, &[])),
            hex(&encode(kind::CHAIN_LOAD, FF_FDS, 2, &load, &[])),
        ];
        assert_eq!(
            got,
            [
                "2e0000000100080001000000000000001a000000000000007b226368696c644964223a226c697665222c22666473223a317d",
                concat!(
                    "b8000000100008000200000000000000a4000000000000007b22636861696e4b6579223a372c22696e7374616e6365",
                    "4964223a34322c22726f6c65223a226c697665222c2267726f7570223a312c2272617465223a34383030302e302c22",
                    "6d61784672616d6573223a3531322c226368496e223a322c2263684f7574223a322c226d61784d696469223a323536",
                    "2c226d6178506172616d73223a36342c22746170536c6f74223a302c22736c6f7473223a5b5d2c22666473223a317d"
                ),
            ]
        );
    }

    /// The lab gain's class as a 4.0 scan reports it.
    fn desc() -> ScanClass {
        ScanClass::Vst3(crate::plugin::Vst3Class {
            path: "/x/gain.vst3".into(),
            source: Some(crate::plugin::ScanSource::Factory),
            class_id: "5341534c4c4142010000005000000000".into(),
            category: "Audio Module Class".into(),
            name: "gain".into(),
            instantiated: Some(true),
            inputs: Some(2),
            outputs: Some(2),
            ..Default::default()
        })
    }

    fn decode_all(bytes: &[u8], dir: Direction, chunk: usize) -> Result<Vec<Frame>, ProtoError> {
        let mut d = Decoder::new(dir);
        let mut v = Vec::new();
        for c in bytes.chunks(chunk.max(1)) {
            d.feed(c, &mut |f| v.push(f))?;
        }
        Ok(v)
    }

    #[test]
    fn frames_round_trip_in_any_chunking() {
        let mut s = encode_msg(
            kind::STATE_APPLIED,
            FF_EVENT,
            0,
            &StateApplied {
                chain_key: 7,
                slot: 1,
                generation: 9,
                ok: true,
                applied: None,
                child_generation: None,
            },
            &[],
        );
        s.extend(encode(kind::STATE_GET, FF_REPLY, 3, b"{}", &[1, 2, 3]));
        for chunk in [1, 5, 24, 1000] {
            let fs = decode_all(&s, Direction::ToHost, chunk).unwrap();
            assert_eq!(fs.len(), 2);
            let a: StateApplied = fs[0].meta_as().unwrap();
            assert_eq!((a.chain_key, a.slot, a.generation, a.ok), (7, 1, 9, true));
            assert_eq!(fs[1].blob, vec![1, 2, 3]);
            assert_eq!(fs[1].header.id, 3);
        }
    }

    #[test]
    fn caps_and_identities_are_checked_before_any_allocation() {
        let mut h = encode(kind::LOG, FF_EVENT, 0, b"{}", &[]);
        h[F_BLOB_LEN..F_BLOB_LEN + 4].copy_from_slice(&(u32::MAX).to_le_bytes());
        assert!(matches!(
            decode_all(&h, Direction::ToHost, 64),
            Err(ProtoError::BlobTooLong(_))
        ));
        let mut h = encode(kind::LOG, FF_EVENT, 0, b"{}", &[]);
        h[0] ^= 1;
        assert!(matches!(
            decode_all(&h, Direction::ToHost, 64),
            Err(ProtoError::FrameLen { .. })
        ));
        // An event with an id, a reply without one, a request towards the host application, fds towards the host application.
        assert!(
            decode_all(
                &encode(kind::LOG, FF_EVENT, 5, b"{}", &[]),
                Direction::ToHost,
                64
            )
            .is_err()
        );
        assert!(
            decode_all(
                &encode(kind::PING, FF_REPLY, 0, b"{}", &[]),
                Direction::ToHost,
                64
            )
            .is_err()
        );
        assert!(decode_all(&encode(kind::PING, 0, 1, b"{}", &[]), Direction::ToHost, 64).is_err());
        assert!(
            decode_all(
                &encode(kind::PING, FF_REPLY | FF_FDS, 1, b"{}", &[]),
                Direction::ToHost,
                64
            )
            .is_err()
        );
        assert!(decode_all(&encode(999, FF_EVENT, 0, b"{}", &[]), Direction::ToHost, 64).is_err());
        // Non-UTF-8 meta, a meta that is not an object.
        assert!(
            decode_all(
                &encode(kind::LOG, FF_EVENT, 0, &[0xff], &[]),
                Direction::ToHost,
                64
            )
            .is_err()
        );
        let f = &decode_all(
            &encode(kind::LOG, FF_EVENT, 0, b"[1]", &[]),
            Direction::ToHost,
            64,
        )
        .unwrap()[0];
        assert!(check_to_host(f).is_err());
    }

    #[test]
    fn the_state_mode_is_sasp_by_default_and_raw_is_struck() {
        // `sasp` is the one production mode (and the default); any other word (3.x's test token and 3.0's `raw`
        // included) parses as Unknown so a child answers it with `state.applied {ok: false}`.
        let m: StateSet = parse_meta(br#"{"chainKey":1,"slot":2,"gen":3}"#).unwrap();
        assert_eq!(m.mode, StateMode::V1);
        assert_eq!(
            serde_json::to_value(StateSet::default()).unwrap()["mode"],
            "sasp"
        );
        for (word, want) in [
            ("sasp", StateMode::V1),
            ("raw", StateMode::Unknown),
            ("Sasp", StateMode::Unknown),
        ] {
            let m: StateSet = parse_meta(
                format!(r#"{{"chainKey":1,"slot":2,"gen":3,"mode":"{word}"}}"#).as_bytes(),
            )
            .unwrap();
            assert_eq!(m.mode, want, "{word}");
            assert_eq!((m.chain_key, m.slot, m.generation), (1, 2, 3), "{word}");
        }
    }

    #[test]
    fn slot_reactivated_is_an_event_the_core_reads() {
        let f = encode_msg(
            kind::SLOT_REACTIVATED,
            FF_EVENT,
            0,
            &SlotReactivated {
                chain_key: 9,
                slot: 1,
                flags: 8,
                bypassed_blocks: 5,
                cycle_ns: 1_000_000,
                latency: 64,
                forced_take: true,
                misaligned: 0,
                take_given_up: false,
                abandoned: false,
            },
            &[],
        );
        let fs = decode_all(&f, Direction::ToHost, 7).unwrap();
        assert!(check_to_host(&fs[0]).is_ok());
        let m: SlotReactivated = fs[0].meta_as().unwrap();
        // `applied` on state.applied is additive: absent from a whole set's meta, parsed when a child
        // reports a partial vst3 set.
        let whole = StateApplied {
            ok: true,
            ..Default::default()
        };
        assert!(!serde_json::to_string(&whole).unwrap().contains("applied\""));
        let p: StateApplied = parse_meta(
            br#"{"chainKey":1,"slot":0,"gen":2,"ok":false,"applied":{"component":true,"controller":false}}"#,
        )
        .unwrap();
        assert!(p.partial());
        assert_eq!(
            p.applied,
            Some(Applied {
                component: true,
                controller: false
            })
        );
        let old: StateApplied =
            parse_meta(br#"{"chainKey":1,"slot":0,"gen":2,"ok":false}"#).unwrap();
        assert!(!old.partial() && old.applied.is_none());
        assert_eq!((m.bypassed_blocks, m.latency, m.forced_take), (5, 64, true));
        let s = String::from_utf8(fs[0].meta.clone()).unwrap();
        for k in [
            "chainKey",
            "slot",
            "flags",
            "bypassedBlocks",
            "cycleNs",
            "latency",
            "forcedTake",
            "misaligned",
        ] {
            assert!(s.contains(&format!("\"{k}\"")), "{k} in {s}");
        }
        // A request kind cannot carry it, and the child never receives it.
        assert!(
            decode_all(
                &encode(kind::SLOT_REACTIVATED, 0, 1, b"{}", &[]),
                Direction::ToChild,
                64
            )
            .is_err()
        );
    }

    /// The rule on `applied`, every combination.
    #[test]
    fn applied_is_valid_only_with_ok_false_and_exactly_one_chunk() {
        let a = |component, controller| {
            Some(Applied {
                component,
                controller,
            })
        };
        let cases = [
            (true, None, AppliedKind::Whole),
            (false, None, AppliedKind::Whole),
            (false, a(true, false), AppliedKind::Partial),
            (false, a(false, true), AppliedKind::Partial),
            (false, a(true, true), AppliedKind::Invalid),
            (false, a(false, false), AppliedKind::Invalid),
            (true, a(true, false), AppliedKind::Invalid),
            (true, a(false, true), AppliedKind::Invalid),
            (true, a(true, true), AppliedKind::Invalid),
            (true, a(false, false), AppliedKind::Invalid),
        ];
        for (ok, applied, want) in cases {
            let m = StateApplied {
                ok,
                applied,
                ..Default::default()
            };
            assert_eq!(m.classify(), want, "ok {ok}, applied {applied:?}");
            assert_eq!(m.partial(), want == AppliedKind::Partial);
        }
    }

    #[test]
    fn a_failed_decoder_stays_failed() {
        let mut d = Decoder::new(Direction::ToHost);
        let bad = [0xffu8; 24];
        assert!(d.feed(&bad, &mut |_| {}).is_err());
        let good = encode(kind::LOG, FF_EVENT, 0, b"{}", &[]);
        assert!(
            d.feed(&good, &mut |_| panic!("no frame after an error"))
                .is_err()
        );
    }
    // ---- The scan child's metas ----

    #[test]
    fn scan_and_scanned_have_their_wire_shapes() {
        let scan = Scan::new(ScanFormat::Vst3, "/x/gain.vst3").unwrap();
        assert_eq!(
            serde_json::to_value(&scan).unwrap(),
            serde_json::json!({"format": "vst3", "path": "/x/gain.vst3"})
        );
        let ok = Scanned {
            format: ScanFormat::Au,
            path: "/x/gain.component".into(),
            outcome: ScannedOutcome::Ok,
            classes: vec![desc()],
        };
        let v = serde_json::to_value(&ok).unwrap();
        assert_eq!(v["outcome"], "ok");
        assert_eq!(v["format"], "au");
        assert_eq!(
            v["classes"][0]["classId"],
            "5341534c4c4142010000005000000000"
        );
        assert_eq!(v["classes"][0]["path"], "/x/gain.vst3");
        assert_eq!(serde_json::from_value::<Scanned>(v).unwrap(), ok);
        let failed: Scanned = serde_json::from_value(serde_json::json!({
            "format": "vst3", "path": "/x/h.vst3", "outcome": "failed", "reason": "ui", "detail": "a licence dialog"
        }))
        .unwrap();
        assert_eq!(
            failed.outcome,
            ScannedOutcome::Failed {
                failure: ScanFailure::Ui,
                detail: "a licence dialog".into()
            }
        );
        assert!(failed.classes.is_empty());
    }

    #[test]
    fn a_child_cannot_report_crashed_or_timeout() {
        for outcome in ["crashed", "timeout", "bogus"] {
            let r = serde_json::from_value::<Scanned>(serde_json::json!({
                "format": "vst3", "path": "/x/h.vst3", "outcome": outcome
            }));
            assert!(r.is_err(), "{outcome}: only the supervisor decides it");
        }
    }

    #[test]
    fn an_unknown_failure_reason_keeps_the_childs_word() {
        let s: Scanned = serde_json::from_value(serde_json::json!({
            "format": "au", "path": "aumu x y", "outcome": "failed", "reason": "licenseServerDown"
        }))
        .unwrap();
        assert_eq!(
            s.outcome,
            ScannedOutcome::Failed {
                failure: ScanFailure::Other {
                    reason: "licenseServerDown".into()
                },
                detail: String::new()
            }
        );
        let back = serde_json::to_value(&s).unwrap();
        assert_eq!(back["reason"], "licenseServerDown");
        assert_eq!(back["outcome"], "failed");
    }

    /// `timeout` stays distinct from `crashed` and `failed` from the wire into the cache entry.
    #[test]
    fn every_scan_result_keeps_its_own_tag_through_the_cache() {
        let results = [
            ScanResult::from_scanned(Scanned {
                format: ScanFormat::Vst3,
                path: "/x/g.vst3".into(),
                outcome: ScannedOutcome::Ok,
                classes: vec![desc()],
            }),
            ScanResult::from_scanned(Scanned {
                format: ScanFormat::Vst3,
                path: "/x/h.vst3".into(),
                outcome: ScannedOutcome::Failed {
                    failure: ScanFailure::State,
                    detail: "not a framed state blob".into(),
                },
                classes: vec![],
            }),
            ScanResult::Crashed {
                os_code: Some(OsCode::Signal(11)),
                exit_code: None,
            },
            ScanResult::Timeout {
                kind: TimeoutKind::Budget,
            },
            ScanResult::Timeout {
                kind: TimeoutKind::Step,
            },
            ScanResult::Crashed {
                os_code: None,
                exit_code: Some(3),
            },
        ];
        let tags: Vec<&str> = results.iter().map(ScanResult::tag).collect();
        assert_eq!(
            tags,
            ["ok", "failed", "crashed", "timeout", "timeout", "crashed"]
        );
        for r in &results {
            let v = serde_json::to_value(r).unwrap();
            assert_eq!(v["outcome"], r.tag(), "{v}");
            let back: ScanResult = serde_json::from_value(v.clone()).unwrap();
            assert_eq!(&back, r, "{v}");
        }
        // The goldens (`kind` only).
        let t = serde_json::to_value(&results[3]).unwrap();
        assert_eq!(
            t,
            serde_json::json!({"outcome": "timeout", "kind": "budget"})
        );
        assert_eq!(
            serde_json::to_value(&results[4]).unwrap(),
            serde_json::json!({"outcome": "timeout", "kind": "step"})
        );
        let c = serde_json::to_value(&results[2]).unwrap();
        assert_eq!(c, serde_json::json!({"outcome": "crashed", "signal": 11}));
        assert_eq!(
            serde_json::to_value(&results[5]).unwrap(),
            serde_json::json!({"outcome": "crashed", "exitCode": 3})
        );
        // A timeout never reads back as a crash or a failure, nor the reverse.
        assert_ne!(
            serde_json::from_value::<ScanResult>(t).unwrap().tag(),
            "crashed"
        );
    }

    /// The scan codec's change: the old `{outcome: timeout, budgetMs}` shape (no `kind`) is refused, as is a `kind`
    /// word that is neither `step` nor `budget`; nothing is ever read as a timeout without saying which.
    #[test]
    fn a_timeout_must_say_its_kind() {
        for v in [
            serde_json::json!({"outcome": "timeout", "budgetMs": 30000}),
            serde_json::json!({"outcome": "timeout"}),
            serde_json::json!({"outcome": "timeout", "kind": "wall"}),
            serde_json::json!({"outcome": "timeout", "kind": 1}),
        ] {
            assert!(
                serde_json::from_value::<ScanResult>(v.clone()).is_err(),
                "{v}"
            );
        }
        let both: Vec<ScanResult> = ["step", "budget"]
            .iter()
            .map(|k| {
                serde_json::from_value(serde_json::json!({"outcome": "timeout", "kind": k}))
                    .unwrap()
            })
            .collect();
        assert_ne!(both[0], both[1], "two kinds, never folded");
    }

    /// `scanned` (65) is an EVENT (id 0), as host's scan mode sends it: through the host application's real `Decoder`, then
    /// `check_to_host` for its meta. (The test before this one built the frame by hand with `FF_REPLY` and checked only
    /// `check_to_host`, which ignores flags, so it never saw the decoder refuse a `scanned` reply.)
    #[test]
    fn a_scanned_event_decodes_and_is_validated_by_its_kind() {
        let good = Scanned {
            format: ScanFormat::Vst3,
            path: "/x/g.vst3".into(),
            outcome: ScannedOutcome::Ok,
            classes: vec![desc()],
        };
        let bytes = encode_msg(kind::SCANNED, FF_EVENT, 0, &good, &[]);
        for chunk in [1, 7, 24, 4096] {
            let fs = decode_all(&bytes, Direction::ToHost, chunk).unwrap();
            assert_eq!(fs.len(), 1);
            let f = &fs[0];
            assert_eq!(
                (f.header.kind, f.header.flags, f.header.id),
                (65, FF_EVENT, 0)
            );
            assert!(check_to_host(f).is_ok());
            assert_eq!(f.meta_as::<Scanned>().unwrap(), good);
        }
        // A well-framed event whose meta is a child's `timeout`: the frame decodes, its meta is refused.
        let timeout = encode(
            kind::SCANNED,
            FF_EVENT,
            0,
            br#"{"format":"vst3","path":"/x","outcome":"timeout"}"#,
            &[],
        );
        let fs = decode_all(&timeout, Direction::ToHost, 64).unwrap();
        assert!(
            check_to_host(&fs[0]).is_err(),
            "a child's timeout is refused"
        );
    }

    /// The negative cases: `scanned` framed as a reply is refused by the decoder itself with `BadFlags {65, 1}` (the
    /// mutant `--cfg mutant_scanned_flags_ignored` must fail here), and so is an error-flagged event; an event
    /// with an id is `BadId`; the host application never sends `scanned`.
    #[test]
    fn a_scanned_reply_is_refused_by_the_decoder() {
        let meta = serde_json::to_vec(&Scanned {
            format: ScanFormat::Vst3,
            path: "/x/g.vst3".into(),
            outcome: ScannedOutcome::Ok,
            classes: vec![],
        })
        .unwrap();
        let reply = encode(kind::SCANNED, FF_REPLY, 1, &meta, &[]);
        let mut d = Decoder::new(Direction::ToHost);
        assert_eq!(
            d.feed(&reply, &mut |_| panic!("a scanned reply yields no frame")),
            Err(ProtoError::BadFlags { kind: 65, flags: 1 })
        );
        assert_eq!(
            decode_all(
                &encode(kind::SCANNED, FF_EVENT | FF_ERROR, 0, &meta, &[]),
                Direction::ToHost,
                64
            )
            .unwrap_err(),
            ProtoError::BadFlags {
                kind: 65,
                flags: FF_EVENT | FF_ERROR
            }
        );
        assert_eq!(
            decode_all(
                &encode(kind::SCANNED, FF_EVENT, 1, &meta, &[]),
                Direction::ToHost,
                64
            )
            .unwrap_err(),
            ProtoError::BadId { kind: 65, id: 1 }
        );
        assert_eq!(
            decode_all(
                &encode(kind::SCANNED, 0, 1, &meta, &[]),
                Direction::ToChild,
                64
            )
            .unwrap_err(),
            ProtoError::BadFlags { kind: 65, flags: 0 }
        );
    }

    /// `scan` (64) is a REQUEST (flags 0, id >= 1) towards the child, through the child's real `Decoder` and
    /// `Scan`'s own check; its receipt is a REPLY carrying the request's id, through the host application's.
    #[test]
    fn a_scan_request_decodes_and_its_receipt_is_a_reply() {
        let scan = Scan::new(ScanFormat::Au, "/x/plugins/au/gain.component").unwrap();
        let bytes = encode_msg(kind::SCAN, 0, 2, &scan, &[]);
        for chunk in [1, 7, 24, 4096] {
            let fs = decode_all(&bytes, Direction::ToChild, chunk).unwrap();
            assert_eq!(fs.len(), 1);
            assert_eq!(
                (fs[0].header.kind, fs[0].header.flags, fs[0].header.id),
                (64, 0, 2)
            );
            assert_eq!(fs[0].meta_as::<Scan>().unwrap(), scan);
        }
        // A well-framed request with a path `Scan` refuses: the frame decodes, its meta does not.
        let bad = encode(
            kind::SCAN,
            0,
            2,
            br#"{"format":"au","path":"aufx SLgn SaSL"}"#,
            &[],
        );
        let fs = decode_all(&bad, Direction::ToChild, 64).unwrap();
        assert!(fs[0].meta_as::<Scan>().is_err());
        // The receipt (host: `{}` with the scan's id, before the probe runs).
        let receipt = encode(kind::SCAN, FF_REPLY, 2, b"{}", &[]);
        let fs = decode_all(&receipt, Direction::ToHost, 64).unwrap();
        assert_eq!(
            (fs[0].header.kind, fs[0].header.flags, fs[0].header.id),
            (64, FF_REPLY, 2)
        );
        assert!(check_to_host(&fs[0]).is_ok());
    }

    /// The negative cases: towards the child, `scan` framed as an event or as a reply is `BadFlags` (the mutant
    /// `--cfg mutant_scan_flags_ignored` must fail here); a request with id 0 is `BadId`; towards the host application, a
    /// `scan` request or event is `BadFlags`.
    #[test]
    fn a_scan_not_framed_as_a_request_is_refused_by_the_decoder() {
        let meta =
            serde_json::to_vec(&Scan::new(ScanFormat::Vst3, "/x/gain.vst3").unwrap()).unwrap();
        let refused = |flags: u16, id: u64, dir: Direction| {
            decode_all(&encode(kind::SCAN, flags, id, &meta, &[]), dir, 64).unwrap_err()
        };
        assert_eq!(
            refused(FF_EVENT, 0, Direction::ToChild),
            ProtoError::BadFlags { kind: 64, flags: 2 }
        );
        assert_eq!(
            refused(FF_REPLY, 2, Direction::ToChild),
            ProtoError::BadFlags { kind: 64, flags: 1 }
        );
        assert_eq!(
            refused(0, 0, Direction::ToChild),
            ProtoError::BadId { kind: 64, id: 0 }
        );
        assert_eq!(
            refused(0, 2, Direction::ToHost),
            ProtoError::BadFlags { kind: 64, flags: 0 }
        );
        assert_eq!(
            refused(FF_EVENT, 0, Direction::ToHost),
            ProtoError::BadFlags { kind: 64, flags: 2 }
        );
    }
    /// `scan.path` is always an absolute bundle path; an AU identifier, a relative path, a
    /// search directory or the wrong bundle type is refused, both when built and when decoded.
    #[test]
    fn a_scan_is_only_ever_an_absolute_bundle_path() {
        assert!(Scan::new(ScanFormat::Vst3, "/x/plugins/vst3/gain.vst3").is_ok());
        assert!(Scan::new(ScanFormat::Au, "/x/plugins/au/gain.component").is_ok());
        for (format, path) in [
            (ScanFormat::Au, "aufx SLgn SaSL"),
            (ScanFormat::Au, "aufx:SLgn:SaSL"),
            (ScanFormat::Au, "/x/plugins/au/gain.vst3"),
            (ScanFormat::Vst3, "/x/plugins/au/gain.component"),
            (ScanFormat::Vst3, "gain.vst3"),
            (ScanFormat::Vst3, "/x/plugins/vst3"),
            (ScanFormat::Vst3, "/x/plugins/vst3/"),
            (ScanFormat::Vst3, "/x/plugins/vst3/gain.vst3/"),
            (ScanFormat::Vst3, "/x/plugins/../vst3/gain.vst3"),
            (ScanFormat::Vst3, "/x/./gain.vst3"),
            (ScanFormat::Vst3, "/.vst3"),
            (ScanFormat::Au, ""),
        ] {
            assert!(
                Scan::new(format, path).is_err(),
                "{format:?} {path:?}: built"
            );
            let fmt = serde_json::to_value(format).unwrap();
            let r =
                serde_json::from_value::<Scan>(serde_json::json!({"format": fmt, "path": path}));
            assert!(r.is_err(), "{format:?} {path:?}: decoded");
        }
        let nul = format!("/x/a{}b.vst3", '\0');
        assert!(Scan::new(ScanFormat::Vst3, nul).is_err());
    }
    /// `protocolAfterScan` is a reason of the supervisor's record, round-trips
    /// there, and is refused in a child's `scanned` (decoded and through `check_to_host`).
    #[test]
    fn protocol_after_scan_is_the_supervisors_reason_only() {
        let r = ScanResult::Failed {
            failure: ScanFailure::ProtocolAfterScan,
            detail: "scanned echoes another path".into(),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"outcome": "failed", "reason": "protocolAfterScan", "detail": "scanned echoes another path"})
        );
        assert_eq!(serde_json::from_value::<ScanResult>(v).unwrap(), r);
        let line = serde_json::json!({
            "format": "vst3", "path": "/x/h.vst3", "outcome": "failed", "reason": "protocolAfterScan"
        });
        assert!(serde_json::from_value::<Scanned>(line.clone()).is_err());
        let f = &decode_all(
            &encode(kind::SCANNED, FF_EVENT, 0, line.to_string().as_bytes(), &[]),
            Direction::ToHost,
            64,
        )
        .unwrap()[0];
        assert!(check_to_host(f).is_err(), "refused by kind");
    }

    /// After a `MetaTooLong` refusal the decoder names the refused header
    /// only when the size is its one fault; any other refusal, or a header that fails another check too, is None.
    #[test]
    fn an_oversized_meta_header_is_named_only_when_the_size_is_its_one_fault() {
        let head = |kind: u16, flags: u16, id: u64, meta: u32, blob: u32, frame: u32| {
            let mut b = Vec::with_capacity(FRAME_HEADER);
            b.extend_from_slice(&frame.to_le_bytes());
            b.extend_from_slice(&kind.to_le_bytes());
            b.extend_from_slice(&flags.to_le_bytes());
            b.extend_from_slice(&id.to_le_bytes());
            b.extend_from_slice(&meta.to_le_bytes());
            b.extend_from_slice(&blob.to_le_bytes());
            b
        };
        let big = MAX_META as u32 + 1;
        let read = |b: &[u8]| {
            let mut d = Decoder::new(Direction::ToHost);
            let r = d.feed(b, &mut |_| panic!("no frame"));
            (r, d.oversized_meta_header())
        };
        // A `scanned` event refused for its size alone: named, and the decoder stays failed.
        let (r, h) = read(&head(kind::SCANNED, FF_EVENT, 0, big, 0, 20 + big));
        assert_eq!(r, Err(ProtoError::MetaTooLong(big)));
        let h = h.expect("named");
        assert_eq!(
            (h.kind, h.flags, h.id, h.meta_len),
            (kind::SCANNED, FF_EVENT, 0, big)
        );
        // Oversized, and a fault besides: not named.
        for (what, b) in [
            (
                "a reply",
                head(kind::SCANNED, FF_REPLY, 2, big, 0, 20 + big),
            ),
            (
                "an event id",
                head(kind::SCANNED, FF_EVENT, 7, big, 0, 20 + big),
            ),
            (
                "the length identity",
                head(kind::SCANNED, FF_EVENT, 0, big, 0, 20),
            ),
            ("an unknown kind", head(999, FF_EVENT, 0, big, 0, 20 + big)),
            (
                "the blob cap",
                head(kind::SCANNED, FF_EVENT, 0, big, MAX_BLOB as u32 + 1, 0),
            ),
        ] {
            let (r, h) = read(&b);
            assert_eq!(r, Err(ProtoError::MetaTooLong(big)), "{what}");
            assert_eq!(h, None, "{what}");
        }
        // Another refusal, or none: not named.
        let (r, h) = read(&head(kind::SCANNED, FF_REPLY, 2, 2, 0, 22));
        assert!(matches!(r, Err(ProtoError::BadFlags { .. })), "{r:?}");
        assert_eq!(h, None);
        let mut d = Decoder::new(Direction::ToHost);
        assert_eq!(d.oversized_meta_header(), None);
        d.feed(
            &head(kind::SCANNED, FF_EVENT, 0, big, 0, 20 + big)[..10],
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(d.oversized_meta_header(), None, "a header not yet whole");
    }

    /// The admission gate: it sees each checked header before a byte of the
    /// body is read; a refusal fails the decoder with `NotAdmitted` and leaves no body allocated (the decoder still
    /// holds only its 24 header bytes), and it stays failed. An admitting gate decodes exactly what `feed` does.
    #[test]
    fn the_admission_gate_judges_the_header_before_the_body_is_allocated() {
        let blob = vec![7u8; 4096];
        let a = encode(kind::SCANNED, FF_EVENT, 0, br#"{"x":1}"#, &blob);
        let b = encode(kind::SCANNED, FF_EVENT, 0, br#"{"x":2}"#, &[]);
        let both: Vec<u8> = a.iter().chain(&b).copied().collect();
        // Admitting: the same frames as `feed`, in any chunking; the gate saw each header once.
        for chunk in [1, 23, 24, 25, 4096, both.len()] {
            let mut d = Decoder::new(Direction::ToHost);
            let (mut seen, mut got) = (Vec::new(), Vec::new());
            for c in both.chunks(chunk) {
                d.feed_admitting(
                    c,
                    &mut |h| {
                        seen.push(h.blob_len);
                        true
                    },
                    &mut |f| got.push(f),
                )
                .unwrap();
            }
            assert_eq!(seen, vec![4096, 0], "{chunk}");
            assert_eq!(got, decode_all(&both, Direction::ToHost, chunk).unwrap());
        }
        // Refusing: the header alone is fed, so the refusal comes before any body byte; no body is allocated.
        let mut d = Decoder::new(Direction::ToHost);
        let r = d.feed_admitting(&a[..FRAME_HEADER], &mut |h| h.blob_len == 0, &mut |_| {
            panic!("no frame")
        });
        let refused = ProtoError::NotAdmitted {
            kind: kind::SCANNED,
            blob_len: 4096,
        };
        assert_eq!(r, Err(refused.clone()));
        assert!(
            matches!(d.state, DState::Header { have, .. } if have == FRAME_HEADER),
            "only the header is held: no body was allocated"
        );
        assert_eq!(d.oversized_meta_header(), None, "not a size refusal");
        assert_eq!(
            d.feed(&a[FRAME_HEADER..], &mut |_| panic!("no frame")),
            Err(refused),
            "it stays failed"
        );
        // A header that fails its own checks is refused by them, never shown to the gate.
        let mut bad = a.clone();
        bad[F_FLAGS..F_FLAGS + 2].copy_from_slice(&FF_REPLY.to_le_bytes());
        let mut d = Decoder::new(Direction::ToHost);
        let r = d.feed_admitting(&bad, &mut |_| panic!("not shown"), &mut |_| {});
        assert!(matches!(r, Err(ProtoError::BadFlags { .. })), "{r:?}");
    }

    #[test]
    fn arch_is_a_known_failure_reason() {
        let s: Scanned = serde_json::from_value(serde_json::json!({
            "format": "vst3", "path": "/x/old.vst3", "outcome": "failed", "reason": "arch", "detail": "x86_64 only"
        }))
        .unwrap();
        assert_eq!(
            s.outcome,
            ScannedOutcome::Failed {
                failure: ScanFailure::Arch,
                detail: "x86_64 only".into()
            }
        );
        let r = ScanResult::from_scanned(s);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"], "failed");
        assert_eq!(v["reason"], "arch");
        assert_eq!(serde_json::from_value::<ScanResult>(v).unwrap(), r);
    }

    /// Protocol 3.3: every field is additive. A 3.2 `state.get` reply decodes as
    /// `ok: true`, and a reply or a `state.applied` with no 3.3 field encodes byte for byte as 3.2's did.
    #[test]
    fn protocol_3_3_fields_are_additive_and_3_2_frames_are_unchanged() {
        // 3.2 bytes, as a 3.2 child (and this codec before 3.3) wrote them.
        let reply_3_2: &[u8] = br#"{"chainKey":7,"slot":1,"childGen":4}"#;
        let applied_3_2: &[u8] = br#"{"chainKey":7,"slot":1,"gen":9,"ok":true}"#;
        let r: StateGetReply = parse_meta(reply_3_2).unwrap();
        assert!(r.ok, "a 3.2 reply has no `ok`: it is a read that answered");
        assert_eq!((r.chain_key, r.slot, r.child_gen), (7, 1, 4));
        assert_eq!(
            (r.reason.as_deref(), r.bytes, r.too_large()),
            (None, None, None)
        );
        assert_eq!(
            serde_json::to_vec(&r).unwrap(),
            reply_3_2,
            "re-encoded byte for byte"
        );
        let d = StateGetReply {
            chain_key: 7,
            slot: 1,
            child_gen: 4,
            ..Default::default()
        };
        assert!(d.ok, "the default is a read that answered");
        assert_eq!(
            encode_msg(kind::STATE_GET, FF_REPLY, 3, &d, &[1, 2]),
            encode(kind::STATE_GET, FF_REPLY, 3, reply_3_2, &[1, 2]),
            "an ok reply's whole frame is 3.2's"
        );
        let a: StateApplied = parse_meta(applied_3_2).unwrap();
        assert_eq!(a.child_generation, None);
        assert_eq!(serde_json::to_vec(&a).unwrap(), applied_3_2);

        // 3.3: tooLarge keeps chainKey / slot / childGen; childGeneration rides on state.applied.
        let t: StateGetReply = parse_meta(
            br#"{"chainKey":7,"slot":1,"childGen":5,"ok":false,"reason":"tooLarge","bytes":70000000}"#,
        )
        .unwrap();
        assert_eq!((t.chain_key, t.slot, t.child_gen), (7, 1, 5));
        assert_eq!(t.too_large(), Some(70_000_000));
        let rt: StateGetReply = parse_meta(&serde_json::to_vec(&t).unwrap()).unwrap();
        assert_eq!(rt, t, "a tooLarge reply round-trips");
        let other: StateGetReply =
            parse_meta(br#"{"chainKey":7,"slot":1,"ok":false,"reason":"x"}"#).unwrap();
        assert!(
            !other.ok && other.too_large().is_none(),
            "another reason is a failed read, not tooLarge"
        );
        let g: StateApplied =
            parse_meta(br#"{"chainKey":7,"slot":1,"gen":9,"ok":true,"childGeneration":12}"#)
                .unwrap();
        assert_eq!(g.child_generation, Some(12));
        let f = encode_msg(kind::STATE_APPLIED, FF_EVENT, 0, &g, &[]);
        let fs = decode_all(&f, Direction::ToHost, 7).unwrap();
        assert!(check_to_host(&fs[0]).is_ok());
        assert_eq!(fs[0].meta_as::<StateApplied>().unwrap(), g);
        let tf = encode_msg(kind::STATE_GET, FF_REPLY, 3, &t, &[]);
        let ts = decode_all(&tf, Direction::ToHost, 5).unwrap();
        assert!(
            check_to_host(&ts[0]).is_ok(),
            "a tooLarge reply with no blob is a valid frame"
        );
    }

    /// Protocol 3.4 (`editor-windows.md` gaps 4 and 5): `SlotInfo.hasEditor` (absent = true) and
    /// `EditorRef.guardReset` (absent = false) are additive, and every 3.3 frame keeps its bytes.
    #[test]
    fn protocol_3_4_fields_are_additive_and_3_3_frames_are_unchanged() {
        // 3.3 bytes, as a 3.3 child (and this codec before 3.4) wrote them.
        let info_3_3: &[u8] =
            br#"{"ok":true,"name":"gain","channels":2,"latency":0,"paramCount":4}"#;
        let open_3_3: &[u8] = br#"{"chainKey":7,"slot":1}"#;
        let close_all_3_3: &[u8] = br#"{"chainKey":7,"slot":null}"#;
        let i: SlotInfo = parse_meta(info_3_3).unwrap();
        assert!(
            i.has_editor,
            "a 3.3 slot has no `hasEditor`: true (the C++ default)"
        );
        assert_eq!(
            serde_json::to_vec(&i).unwrap(),
            info_3_3,
            "re-encoded byte for byte"
        );
        assert!(
            SlotInfo::default().has_editor,
            "the Rust default is the wire's"
        );
        for b in [open_3_3, close_all_3_3] {
            let e: EditorRef = parse_meta(b).unwrap();
            assert!(
                !e.guard_reset,
                "a 3.3 editor.open has no `guardReset`: false"
            );
            assert_eq!(serde_json::to_vec(&e).unwrap(), b);
        }
        let e = EditorRef {
            chain_key: 7,
            slot: Some(1),
            guard_reset: false,
        };
        assert_eq!(
            encode_msg(kind::EDITOR_OPEN, 0, 3, &e, &[]),
            encode(kind::EDITOR_OPEN, 0, 3, open_3_3, &[]),
            "an unguarded open's whole frame is 3.3's"
        );
        // 3.4: the two fields on the wire, camelCase, only when they differ from the default.
        let g = EditorRef {
            guard_reset: true,
            ..e.clone()
        };
        assert_eq!(
            serde_json::to_vec(&g).unwrap(),
            br#"{"chainKey":7,"slot":1,"guardReset":true}"#
        );
        assert_eq!(
            parse_meta::<EditorRef>(&serde_json::to_vec(&g).unwrap()).unwrap(),
            g
        );
        let n = SlotInfo {
            has_editor: false,
            ..i.clone()
        };
        let v = serde_json::to_value(&n).unwrap();
        assert_eq!(v["hasEditor"], false);
        assert_eq!(
            parse_meta::<SlotInfo>(&serde_json::to_vec(&n).unwrap()).unwrap(),
            n
        );
        let l = ChainLoaded {
            chain_key: 7,
            ok: true,
            slots: vec![n],
            latency: 0,
            param_count: 4,
            ..Default::default()
        };
        let f = encode_msg(kind::CHAIN_LOADED, FF_EVENT, 0, &l, &[]);
        let fs = decode_all(&f, Direction::ToHost, 7).unwrap();
        assert!(check_to_host(&fs[0]).is_ok());
        assert!(!fs[0].meta_as::<ChainLoaded>().unwrap().slots[0].has_editor);
    }

    /// *(protocol 3.9)* `SlotInfo.notices` and `chain.prepared`'s per-slot notices are additive: empty writes a 3.8
    /// line's exact bytes, a 3.8 line (no key) decodes as none, and a newer line round-trips.
    #[test]
    fn slot_notices_are_additive() {
        let old_slot = br#"{"ok":true,"name":"x","channels":2,"latency":0,"paramCount":4}"#;
        let s: SlotInfo = parse_meta(old_slot).unwrap();
        assert!(s.notices.is_empty());
        assert_eq!(
            serde_json::to_vec(&s).unwrap(),
            old_slot.to_vec(),
            "a 3.8 SlotInfo's bytes"
        );
        let n = SlotInfo {
            notices: vec!["inputsRefused".into()],
            ..s.clone()
        };
        let j = serde_json::to_string(&n).unwrap();
        assert!(j.ends_with(r#","notices":["inputsRefused"]}"#), "{j}");
        assert_eq!(parse_meta::<SlotInfo>(j.as_bytes()).unwrap(), n);
        let old_prep = br#"{"chainKey":7,"ok":true,"wiredBytes":5,"unwiredBytes":0}"#;
        let p: ChainPrepared = parse_meta(old_prep).unwrap();
        assert!(p.slot_notices.is_empty());
        assert_eq!(
            serde_json::to_vec(&p).unwrap(),
            old_prep.to_vec(),
            "a 3.8 chain.prepared's bytes"
        );
        let p = ChainPrepared {
            slot_notices: vec![vec![], vec!["inputsRefused".into()]],
            ..p
        };
        let j = serde_json::to_vec(&p).unwrap();
        assert_eq!(parse_meta::<ChainPrepared>(&j).unwrap(), p);
        // An older host's view of a newer line (the field unknown to it): every other field still decodes, as no
        // codec meta denies unknown fields.
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct OldSlot {
            ok: bool,
            param_count: u32,
        }
        let o: OldSlot = parse_meta(serde_json::to_string(&n).unwrap().as_bytes()).unwrap();
        assert!(o.ok && o.param_count == 4);
        let f = encode_msg(kind::CHAIN_PREPARED, FF_EVENT, 0, &p, &[]);
        let fs = decode_all(&f, Direction::ToHost, 7).unwrap();
        assert!(check_to_host(&fs[0]).is_ok());
    }

    /// The host application keeps at most 8 codes of 1-64 ASCII letters and digits, the first of each.
    #[test]
    fn slot_notices_are_clamped() {
        let v: Vec<String> = [
            "inputsRefused",
            "",
            "a b",
            "x\"",
            "inputsRefused",
            &"z".repeat(65),
            &"y".repeat(64),
        ]
        .iter()
        .map(|x| x.to_string())
        .collect();
        assert_eq!(
            slot_notices_clamped(&v),
            vec!["inputsRefused".to_string(), "y".repeat(64)]
        );
        let many: Vec<String> = (0..20).map(|i| format!("n{i}")).collect();
        assert_eq!(slot_notices_clamped(&many).len(), MAX_SLOT_NOTICES);
    }

    /// `chain.loaded`'s `wiredBytes` / `unwiredBytes` are additive: a frame from an older child (neither key)
    /// decodes and checks, both reading 0; a newer child's round-trip.
    #[test]
    fn chain_loaded_wired_bytes_are_additive() {
        let old = br#"{"chainKey":7,"ok":true,"slots":[],"latency":0,"paramCount":0}"#;
        let m = parse_meta::<ChainLoaded>(old).unwrap();
        assert_eq!((m.wired_bytes, m.unwired_bytes), (0, 0));
        let f = encode_msg(
            kind::CHAIN_LOADED,
            FF_EVENT,
            0,
            &serde_json::from_slice::<serde_json::Value>(old).unwrap(),
            &[],
        );
        let fs = decode_all(&f, Direction::ToHost, 7).unwrap();
        assert!(
            check_to_host(&fs[0]).is_ok(),
            "an older child's chain.loaded is still valid"
        );
        let new = ChainLoaded {
            chain_key: 7,
            ok: true,
            wired_bytes: 3 << 20,
            unwired_bytes: 16_384,
            ..Default::default()
        };
        let j = serde_json::to_vec(&new).unwrap();
        let text = String::from_utf8(j.clone()).unwrap();
        assert!(
            text.contains("\"wiredBytes\":3145728") && text.contains("\"unwiredBytes\":16384"),
            "{text}"
        );
        assert_eq!(parse_meta::<ChainLoaded>(&j).unwrap(), new);
    }

    // ---- (protocol 3.6): log.set, corr, the 3.6 fields ---------------------------------------------------------

    #[test]
    fn log_set_is_an_inline_request_answered_by_a_reply() {
        assert_eq!(kind::LOG_SET, 5);
        assert_eq!(kind_name(5), Some("log.set"));
        let m = LogSet {
            levels: 0x2222_2222_2222_2232,
            expires_in_ms: Some(1500),
        };
        let req = encode_msg(kind::LOG_SET, 0, 9, &m, &[]);
        let f = decode_all(&req, Direction::ToChild, 7).unwrap();
        assert_eq!(f[0].meta_as::<LogSet>().unwrap(), m);
        assert!(
            decode_all(&req, Direction::ToHost, 64).is_err(),
            "never a request to the host"
        );
        let rep = encode_msg(
            kind::LOG_SET,
            FF_REPLY,
            9,
            &LogSetReply { levels: m.levels },
            &[],
        );
        let f = decode_all(&rep, Direction::ToHost, 64).unwrap();
        check_to_host(&f[0]).unwrap();
        assert!(
            decode_all(
                &encode(kind::LOG_SET, FF_EVENT, 0, b"{}", &[]),
                Direction::ToHost,
                64
            )
            .is_err()
        );
        // The reply's word is required: `{}` is refused (a typed reply that does not decode).
        let empty = decode_all(
            &encode(kind::LOG_SET, FF_REPLY, 9, b"{}", &[]),
            Direction::ToHost,
            64,
        )
        .unwrap();
        assert!(check_to_host(&empty[0]).is_err());
        // An error reply is the refusal channel.
        let err = encode_msg(
            kind::LOG_SET,
            FF_REPLY | FF_ERROR,
            9,
            &ErrorReply {
                code: -32602,
                message: "no".into(),
            },
            &[],
        );
        check_to_host(&decode_all(&err, Direction::ToHost, 64).unwrap()[0]).unwrap();
        // Absent expiry: never written.
        let text = serde_json::to_string(&LogSet {
            levels: 1,
            expires_in_ms: None,
        })
        .unwrap();
        assert_eq!(text, r#"{"levels":1}"#);
        assert!(parse_meta::<LogSet>(b"{}").is_err(), "levels is required");
        assert!(parse_meta::<LogSet>(br#"{"levels":"x"}"#).is_err());
    }

    /// The child's checks 2-4 (mutation `mutant_log_set_no_s7` fails the expiry rule's row).
    #[test]
    fn log_set_check_refuses_unknown_levels_bad_expiries_and_unbounded_debug() {
        // Every area at warn, at info; warn with area 11 at trace.
        let warn = 0x2222_2222_2222_2222;
        let info = 0x3333_3333_3333_3333;
        let trace_host = 0x2222_5222_2222_2222;
        let ok = |levels, e: Option<u64>| {
            LogSet {
                levels,
                expires_in_ms: e,
            }
            .check()
        };
        assert_eq!(ok(info, None), Ok((info, None)));
        assert_eq!(ok(0, None), Ok((0, None)), "every area off");
        assert_eq!(
            ok(trace_host, Some(300)),
            Ok((trace_host, Some(std::time::Duration::from_millis(300))))
        );
        assert_eq!(
            ok(trace_host, Some(MAX_LOG_SET_EXPIRES_MS)).map(|v| v.0),
            Ok(trace_host)
        );
        for (levels, e, why) in [
            (warn | 6, None, "nibble"),
            (
                0xF000_0000_0000_0000 | warn & 0x0FFF_FFFF_FFFF_FFFF,
                None,
                "nibble",
            ),
            (info, Some(0), "expiresInMs"),
            (info, Some(MAX_LOG_SET_EXPIRES_MS + 1), "expiresInMs"),
            (trace_host, None, "they expire"),
            (0x4444_4444_4444_4444, None, "they expire"),
        ] {
            let r = ok(levels, e);
            assert!(
                r.as_ref().is_err_and(|m| m.contains(why)),
                "{levels:#x} {e:?}: {r:?}"
            );
        }
    }

    #[test]
    fn the_3_6_fields_are_additive() {
        // A 3.5 hello decodes 0 and a 0 pair is never written, so a 3.5 line keeps its bytes.
        let h: HelloReply = parse_meta(br#"{"pid":1,"protocol":196613}"#).unwrap();
        assert_eq!((h.wired_bytes, h.unwired_bytes), (0, 0));
        assert!(!serde_json::to_string(&h).unwrap().contains("wiredBytes"));
        let h = HelloReply {
            wired_bytes: 16_384,
            ..h
        };
        let back: HelloReply = parse_meta(&serde_json::to_vec(&h).unwrap()).unwrap();
        assert_eq!(back.wired_bytes, 16_384);
        // `chain.prepared`: absent is None (keep), and a None pair writes a 3.5 `ChainRef`'s bytes.
        let p: ChainPrepared = parse_meta(br#"{"chainKey":7,"ok":true}"#).unwrap();
        assert_eq!((p.wired_bytes, p.unwired_bytes), (None, None));
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            serde_json::to_string(&ChainRef {
                chain_key: 7,
                ok: true
            })
            .unwrap()
        );
        let p: ChainPrepared = parse_meta(br#"{"chainKey":7,"ok":true,"wiredBytes":0}"#).unwrap();
        assert_eq!(
            p.wired_bytes,
            Some(0),
            "an explicit 0 is a value, not absent"
        );
    }

    /// A meta-less request's typed form: `{}` (ping, shutdown, state.generations; the reply to a ReceiptAck).
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Empty {}

    fn corr_round_trip<T>(v: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let mut m = serde_json::to_vec(v).unwrap();
        let before: serde_json::Value = serde_json::from_slice(&m).unwrap();
        assert!(
            before.get("corr").is_none(),
            "a codec struct names `corr`: {before}"
        );
        with_corr(&mut m, "r:7").unwrap();
        let mut after: serde_json::Value = serde_json::from_slice(&m).unwrap();
        assert_eq!(after["corr"], "r:7");
        after.as_object_mut().unwrap().remove("corr");
        assert_eq!(after, before, "the same object plus corr");
        assert_eq!(
            &parse_meta::<T>(&m).unwrap(),
            v,
            "the decoder takes the key"
        );
        assert_eq!(corr_of_meta(&m), CorrRead::Valid("r:7".into()));
    }

    /// `with_corr` is total over every meta form, and every kind's decoders (the child's for a
    /// request, the host application's for an answer) take the key. A new kind without a row here fails.
    #[test]
    fn with_corr_is_total_and_every_kind_s_decoder_takes_the_key() {
        for (name, k) in KINDS {
            match *k {
                kind::HELLO => {
                    corr_round_trip(&Hello::default());
                    corr_round_trip(&HelloReply::default());
                }
                kind::PING | kind::SHUTDOWN => corr_round_trip(&Empty {}),
                kind::STATE_GENERATIONS => {
                    corr_round_trip(&Empty {});
                    corr_round_trip(&Generations::default());
                }
                kind::LOG_SET => {
                    corr_round_trip(&LogSet {
                        levels: 3,
                        expires_in_ms: Some(5),
                    });
                    corr_round_trip(&LogSetReply { levels: 3 });
                }
                kind::CHAIN_LOAD => corr_round_trip(&ChainLoad::default()),
                kind::CHAIN_UNLOAD
                | kind::CHAIN_PREPARE
                | kind::CHAIN_PREPARE_FRESH
                | kind::CHAIN_UNLOADED
                | kind::CHAIN_PARKED => corr_round_trip(&ChainRef::default()),
                kind::CHAIN_PREPARED_FRESH => {
                    corr_round_trip(&ChainPreparedFresh {
                        chain_key: 1,
                        ok: true,
                        ms: 1.5,
                        latency: 64,
                        slot_notices: Some(vec![]),
                        error: None,
                    });
                    corr_round_trip(&ChainPreparedFresh {
                        error: Some("not a render set".into()),
                        ..ChainPreparedFresh::default()
                    });
                }
                kind::CHAIN_PARK | kind::CHAIN_UNPARK | kind::CHAIN_UNPARKED => {
                    corr_round_trip(&ChainUnpark::default())
                }
                kind::STATE_SET => corr_round_trip(&StateSet::default()),
                kind::EDITOR_OPEN | kind::EDITOR_CLOSE | kind::EDITOR_AVAILABILITY => {
                    corr_round_trip(&EditorRef::default())
                }
                kind::WORKGROUP_ATTACH => corr_round_trip(&WorkgroupAttach::default()),
                kind::STATE_GET => {
                    corr_round_trip(&StateGet::default());
                    corr_round_trip(&StateGetReply {
                        chain_key: 1,
                        slot: 0,
                        child_gen: 2,
                        ok: true,
                        reason: None,
                        bytes: None,
                    });
                }
                kind::PARAM_LIST => {
                    corr_round_trip(&ParamList::default());
                    corr_round_trip(&ParamListReply::default());
                }
                kind::SLOT_SET_ENABLED => {
                    corr_round_trip(&SlotEnabled {
                        chain_key: 1,
                        slot: 0,
                        enabled: false,
                    });
                    corr_round_trip(&SlotEnabledReply::default());
                }
                kind::PARAM_SET => {
                    corr_round_trip(&ParamSet {
                        chain_key: 1,
                        slot: 0,
                        plugin_id: 7,
                        value: 0.5,
                    });
                    corr_round_trip(&ParamSetReply::default());
                }
                kind::CHAIN_LOADED => corr_round_trip(&ChainLoaded::default()),
                kind::CHAIN_PREPARED => corr_round_trip(&ChainPrepared {
                    chain_key: 1,
                    ok: true,
                    wired_bytes: Some(4096),
                    unwired_bytes: Some(0),
                    slot_notices: vec![vec!["inputsRefused".into()]],
                }),
                kind::STATE_APPLIED | kind::STATE_VERIFIED => {
                    corr_round_trip(&StateApplied::default())
                }
                kind::LATENCY_CHANGED => corr_round_trip(&LatencyChanged::default()),
                kind::EDITOR_CLOSED => corr_round_trip(&EditorClosed::default()),
                kind::LOG => corr_round_trip(&Log::default()),
                kind::WORKGROUP_ATTACHED => corr_round_trip(&WorkgroupAttached::default()),
                kind::SLOT_REACTIVATED => corr_round_trip(&SlotReactivated::default()),
                kind::SCAN => corr_round_trip(&Scan::new(ScanFormat::Vst3, "/lab/a.vst3").unwrap()),
                kind::SCANNED => {
                    // Decoded through its check (`ScannedUnchecked`): the key is ignored there too.
                    let line = br#"{"format":"vst3","path":"/lab/a.vst3","outcome":"ok","descriptions":[]}"#;
                    let plain: Scanned = parse_meta(line).unwrap();
                    let mut m = line.to_vec();
                    with_corr(&mut m, "r:7").unwrap();
                    assert_eq!(parse_meta::<Scanned>(&m).unwrap(), plain);
                }
                _ => panic!("kind {name} ({k}) has no corr row"),
            }
        }
        // The forms: `{}` and `{ }`, any object, and what is left alone.
        let add = |m: &[u8]| {
            let mut v = m.to_vec();
            with_corr(&mut v, "a").map(|()| String::from_utf8(v).unwrap())
        };
        assert_eq!(add(b"{}").unwrap(), r#"{"corr":"a"}"#);
        assert_eq!(add(b" { } ").unwrap(), r#" {"corr":"a"}"#);
        assert_eq!(add(br#"{"x":1}"#).unwrap(), r#"{"x":1,"corr":"a"}"#);
        assert_eq!(add(b"{\"x\":1}\n").unwrap(), "{\"x\":1,\"corr\":\"a\"}\n");
        assert_eq!(
            add(b""),
            Err(CorrRefused::MetaLess),
            "a meta-less frame gets no key"
        );
        assert_eq!(add(b"   "), Err(CorrRefused::NotObject));
        assert_eq!(add(b"[1]"), Err(CorrRefused::NotObject));
        assert_eq!(add(b"{"), Err(CorrRefused::NotObject));
        for bad in ["", "a b", "a\"b", "x".repeat(65).as_str(), "é"] {
            let mut v = b"{}".to_vec();
            assert_eq!(with_corr(&mut v, bad), Err(CorrRefused::Invalid), "{bad:?}");
            assert_eq!(v, b"{}");
        }
        let max = "x".repeat(64);
        let mut v = b"{}".to_vec();
        with_corr(&mut v, &max).unwrap();
        assert_eq!(corr_of_meta(&v), CorrRead::Valid(max));
    }

    /// The cap: a meta within the key's length of [`MAX_META`] gets no key, and the frame still encodes. Mutation
    /// `mutant_corr_no_cap` adds it anyway, and `encode` panics on the cap.
    #[test]
    fn a_meta_at_its_cap_gets_no_corr_and_still_encodes() {
        let pad = |n: usize| {
            let mut m = br#"{"p":""#.to_vec();
            m.resize(n - 2, b'x');
            m.extend_from_slice(b"\"}");
            assert_eq!(m.len(), n);
            m
        };
        let corr = "c".repeat(64);
        let mut fits = pad(MAX_META - CORR_KEY_BYTES);
        with_corr(&mut fits, &corr).unwrap();
        assert_eq!(
            fits.len(),
            MAX_META,
            "the longest meta that takes the key ends at the cap"
        );
        let mut over = pad(MAX_META - CORR_KEY_BYTES + 1);
        let r = with_corr(&mut over, &corr);
        assert_eq!(r, Err(CorrRefused::TooLong));
        assert_eq!(over.len(), MAX_META - CORR_KEY_BYTES + 1, "unchanged");
        let f = encode(kind::CHAIN_LOAD, 0, 1, &over, &[]);
        assert_eq!(f.len(), FRAME_HEADER + over.len());
        let mut full = pad(MAX_META);
        assert_eq!(with_corr(&mut full, "a"), Err(CorrRefused::TooLong));
        let _ = encode(kind::CHAIN_LOAD, 0, 1, &full, &[]);
    }

    #[test]
    fn the_corr_reader_is_lenient_and_never_fails() {
        assert_eq!(corr_of_meta(b""), CorrRead::Absent);
        assert_eq!(corr_of_meta(b"{}"), CorrRead::Absent);
        assert_eq!(
            corr_of_meta(br#"{"x":"corr"}"#),
            CorrRead::Absent,
            "the word, not the key"
        );
        assert_eq!(
            corr_of_meta(br#"{"x":{"corr":"a"}}"#),
            CorrRead::Absent,
            "top level only"
        );
        assert_eq!(corr_of_meta(b"not json \"corr\""), CorrRead::Absent);
        assert_eq!(
            corr_of_meta(br#"{"corr":"i-1.a:B_2"}"#),
            CorrRead::Valid("i-1.a:B_2".into())
        );
        for bad in [
            br#"{"corr":null}"#.as_slice(),
            br#"{"corr":7}"#,
            br#"{"corr":""}"#,
            br#"{"corr":"bad corr!"}"#,
            br#"{"corr":"a\"b"}"#,
            br#"{"corr":["a"]}"#,
        ] {
            assert_eq!(
                corr_of_meta(bad),
                CorrRead::Invalid,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let long = format!(r#"{{"corr":"{}"}}"#, "x".repeat(65));
        assert_eq!(corr_of_meta(long.as_bytes()), CorrRead::Invalid);
    }

    #[test]
    fn the_echo_set_is_the_answers() {
        let h = |k: u16, flags: u16| Header {
            kind: k,
            flags,
            id: u64::from(flags & FF_EVENT == 0),
            meta_len: 0,
            blob_len: 0,
        };
        for k in [
            kind::CHAIN_LOADED,
            kind::CHAIN_UNLOADED,
            kind::CHAIN_PREPARED,
            kind::CHAIN_PREPARED_FRESH,
            kind::CHAIN_PARKED,
            kind::CHAIN_UNPARKED,
            kind::STATE_APPLIED,
            kind::EDITOR_CLOSED,
            kind::WORKGROUP_ATTACHED,
        ] {
            assert!(answers_a_request(&h(k, FF_EVENT)), "{k}");
        }
        for k in [
            kind::LOG_SET,
            kind::STATE_GET,
            kind::PARAM_LIST,
            kind::SLOT_SET_ENABLED,
            kind::PARAM_SET,
        ] {
            assert!(answers_a_request(&h(k, FF_REPLY)), "{k}");
        }
        for k in [
            kind::LATENCY_CHANGED,
            kind::SLOT_REACTIVATED,
            kind::EDITOR_AVAILABILITY,
            kind::LOG,
            kind::STATE_VERIFIED,
        ] {
            assert!(!answers_a_request(&h(k, FF_EVENT)), "{k}: spontaneous");
        }
    }
}
