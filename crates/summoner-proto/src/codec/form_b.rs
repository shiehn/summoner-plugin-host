//! `scanned` form B, the codec half. The scanned descriptions travel in the frame's blob part as JSON Lines
//! (`plugin-scan-descriptions/1`, W2); the meta names the form and the count (W3). This module is the one definition
//! both sides link: the scan child's encoder (host's W8 step 5) and the host application's strict decode (W5's checks 2 to 9).
//!
//! - **The bounds** (W3's table): [`MAX_BLOB`] (the frame's, unchanged), [`MAX_SCAN_DESCRIPTIONS`] (16,384 lines) and
//!   [`MAX_DESCRIPTION_LINE`] (64 KiB, the newline excluded), each refused before anything it bounds is allocated.
//!   The per-entry bound (8 MiB) is NOT a wire bound: it is the record's,
//!   the host application's scan record entry bound, enforced at settle, encode and decode
//!   (`scan_record/record.rs`); a form B line within these bounds may still make an entry pass it, and that is the
//!   record's no-verdict EngineFault. `scan_record/tests/bound.rs` pins the relation (`MAX_SCAN_DESCRIPTIONS ==
//!   MAX_ENTRIES`, W3's "16,384 is the record's entry bound"; `MAX_ENTRY_BYTES < MAX_BLOB`).
//! - **The decode** is two halves so the caller keeps W5's order: [`parse_scanned_meta`] is step 2's parse (form A or
//!   B, told apart by the meta key `descriptionsBlob`, never by the blob's length); the caller then checks S1's echo
//!   of `format` and `path` (step 2's second half, the scan supervisor's rule); [`ScannedMeta::with_blob`] is steps 3
//!   to 9. [`decode_scanned`] is both halves with no echo check (host's tests, the fuzz target).
//! - **The verdicts** are typed: [`ScannedFault::Limit`] is W5's engine limits L2 to L4 (an EngineFault, no strike);
//!   L1 is the header's, [`super::Decoder::oversized_header`]. [`ScannedFault::Malformed`] is everything else (a
//!   strike, `failed {protocolAfterScan}`).
//! - **The encoder** ([`encode_scanned`]) never panics: past a bound it gives the `overLimit` frame (W8), and a meta
//!   past [`MAX_META`] gives only the 24-byte header with its true lengths (the L1 path).
//!
//! N9: fuzzed (the frame fuzz target runs [`decode_scanned`] on every `scanned` frame it decodes, and
//! its form B mode builds frames with [`encode_scanned_form_b`] and mutates them).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use super::{
    FF_EVENT, FRAME_HEADER, Frame, MAX_BLOB, MAX_META, ScanClass, ScanFailure, ScanFormat,
    ScanResult, Scanned, ScannedOutcome, encode, header_bytes, kind, parse_meta,
};
use serde::{Deserialize, Serialize};

/// W1: the form B version this codec reads and writes: `scan`'s offer (`descriptionsBlob: 1`) and form B's meta key.
pub const SCAN_DESCRIPTIONS_BLOB: u32 = 1;
/// W3: the most descriptions (blob lines) one form B `scanned` carries. Past it: the sender's `overLimit: "count"`
/// (L2), or a `descriptionCount` over it (L3)..
pub const MAX_SCAN_DESCRIPTIONS: usize = 16_384;
/// W3: the most bytes of one blob line, its newline excluded (= [`MAX_META`], so form B refuses nothing form A
/// accepted). Past it: `overLimit: "line"` (L2), or L4..
pub const MAX_DESCRIPTION_LINE: usize = MAX_META;
const _: () = assert!(MAX_DESCRIPTION_LINE == 64 * 1024 && MAX_SCAN_DESCRIPTIONS == 16_384);
// W3: 16,384 lines of the largest size fit in MAX_BLOB only at 4 KiB on average; the blob cap is its own limit (L1).
const _: () = assert!(MAX_BLOB / MAX_SCAN_DESCRIPTIONS == 4096);

/// The most bytes of a decode error's text that a fault keeps (W5 step 9: "never quoting more than a bounded
/// prefix"); the cut is at a UTF-8 boundary.
pub const MAX_FAULT_TEXT: usize = 256;

fn bounded(s: impl std::fmt::Display) -> String {
    let s = s.to_string();
    if s.len() <= MAX_FAULT_TEXT {
        return s;
    }
    let mut end = MAX_FAULT_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ---------------------------------------------------------------------------------------------------------------------
// The faults (W5)
// ---------------------------------------------------------------------------------------------------------------------

/// Why a `scanned` was refused, after the header (W5 steps 2 to 9). The first check that fails decides.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScannedFault {
    /// An engine limit (W5's L2 to L4): `ScanEnd::EngineFault`, its `why` starting `OVERSIZED_SCANNED`; no verdict, no
    /// strike. (L1 is the header's: [`super::Decoder::oversized_header`].)
    Limit(ScannedLimit),
    /// Anything else: `failed {protocolAfterScan}`, a strike.
    Malformed(ScannedMalformed),
}

impl ScannedFault {
    /// True for W5's engine limits (no verdict, no strike).
    pub fn is_engine_limit(&self) -> bool {
        matches!(self, ScannedFault::Limit(_))
    }
}

impl std::fmt::Display for ScannedFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScannedFault::Limit(l) => match l {
                ScannedLimit::OverLimit {
                    word,
                    description_count,
                } => write!(
                    f,
                    "L2: the child reports overLimit {word:?} ({description_count} descriptions)"
                ),
                ScannedLimit::Count { description_count } => write!(
                    f,
                    "L3: descriptionCount {description_count} is over MAX_SCAN_DESCRIPTIONS {MAX_SCAN_DESCRIPTIONS}"
                ),
                ScannedLimit::Line { index, len } => write!(
                    f,
                    "L4: blob line {index} is {len} bytes, over MAX_DESCRIPTION_LINE {MAX_DESCRIPTION_LINE}"
                ),
            },
            ScannedFault::Malformed(m) => write!(f, "malformed scanned: {m:?}"),
        }
    }
}

impl std::error::Error for ScannedFault {}

/// W5's engine limits after the header.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScannedLimit {
    /// L2: `overLimit` in a well-formed form B meta with an empty blob: the child's own report. `word` is the child's
    /// (`count`, `line`, `blob`, or a newer word: still L2, kept for the log).
    OverLimit {
        /// The child's `overLimit` word.
        word: String,
        /// The meta's `descriptionCount`.
        description_count: u64,
    },
    /// L3: `descriptionCount` over [`MAX_SCAN_DESCRIPTIONS`].
    Count {
        /// The meta's `descriptionCount`.
        description_count: u64,
    },
    /// L4: the line at `index` (0-based) is `len` bytes, its newline excluded, over [`MAX_DESCRIPTION_LINE`].
    Line {
        /// The line's index in the blob, from 0.
        index: usize,
        /// The line's length in bytes, its newline excluded.
        len: usize,
    },
}

/// W5's strike rows, by the step that refuses them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScannedMalformed {
    /// The frame is not a `scanned` EVENT with id 0 ([`decode_scanned`] only; the reader and the step check it first).
    NotScanned {
        /// The frame's kind.
        kind: u16,
        /// The frame's flags.
        flags: u16,
        /// The frame's request id.
        id: u64,
    },
    /// Step 2: the meta is not a `scanned` of form A or B (the codec's message, cut to [`MAX_FAULT_TEXT`]).
    Meta(String),
    /// Step 2: `descriptionsBlob` is present but not the integer 1 (the value as JSON, cut).
    Version(String),
    /// Step 4 (checked with the parse): a form B meta carries `descriptions` or `discovered`.
    ForbiddenKey(&'static str),
    /// Step 3: a form A meta with a blob.
    FormABlob {
        /// The blob's length in bytes.
        blob_len: usize,
    },
    /// Step 4: form B, and this host made no offer.
    NotOffered,
    /// Step 5: `overLimit` with a blob.
    OverLimitBlob {
        /// The blob's length in bytes.
        blob_len: usize,
    },
    /// Step 7: a count above 0 with an empty blob.
    CountWithoutBlob {
        /// The meta's `descriptionCount`.
        description_count: u64,
    },
    /// Step 7: a count of 0 with a blob.
    BlobWithoutCount {
        /// The blob's length in bytes.
        blob_len: usize,
    },
    /// Step 7: the blob's last byte is not 0x0A.
    NoFinalNewline,
    /// Step 7: the number of 0x0A bytes is not `descriptionCount`.
    CountMismatch {
        /// The meta's `descriptionCount`.
        description_count: u64,
        /// The number of 0x0A bytes in the blob.
        newlines: usize,
    },
    /// Step 9: the line at `index` (0-based) is not a description.
    Line {
        /// The line's index in the blob, from 0.
        index: usize,
        /// Why it is not a description.
        why: LineFault,
    },
}

/// Why a blob line is not a description (W5 step 9).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineFault {
    /// Its first byte is not `{` (an empty line, leading whitespace, serde's sequence form `[...]`, a scalar).
    NotAnObject,
    /// Not UTF-8 from byte `at` of the line.
    NotUtf8 {
        /// The byte offset in the line where the invalid UTF-8 starts.
        at: usize,
    },
    /// It does not parse as a [`ScanClass`], its checks included (the parser's message, cut to [`MAX_FAULT_TEXT`],
    /// which never quotes more than that of the line).
    Description(String),
    /// It parses, but is not the codec's canonical bytes for what it decodes to:
    /// whitespace, a key out of the codec's order, an absent defaulted field, an explicit `null`, an unknown key, an
    /// escape the encoder does not write, `-0`. `at` is the line's byte offset where the re-encode first differs (at
    /// the start of the differing piece of the re-encode).
    NonCanonical {
        /// The byte offset in the line where the re-encode first differs.
        at: usize,
    },
}

fn malformed<T>(m: ScannedMalformed) -> Result<T, ScannedFault> {
    Err(ScannedFault::Malformed(m))
}

fn limit<T>(l: ScannedLimit) -> Result<T, ScannedFault> {
    Err(ScannedFault::Limit(l))
}

// ---------------------------------------------------------------------------------------------------------------------
// The meta (W3)
// ---------------------------------------------------------------------------------------------------------------------

/// The meta of a form B `scanned` (W3): `format`, `path` and the outcome as form A has them, plus the form
/// (`descriptionsBlob: 1`, implied), the count and the optional `overLimit`. Written with its keys sorted (the text's
/// examples, byte for byte); read as any JSON object (an unknown key is dropped, D20's additive rule).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FormBMeta {
    /// The bundle's format, as form A has it.
    pub format: ScanFormat,
    /// The scanned bundle's path, as form A has it.
    pub path: String,
    /// The scan's outcome, as form A has it.
    pub outcome: ScannedOutcome,
    /// The number of blob lines (W3): 0 to [`MAX_SCAN_DESCRIPTIONS`], or over it only with `over_limit`.
    pub description_count: u64,
    /// W5's L2: the sender's report that its line passes a bound (`count`, `line`, `blob`); the blob is then empty.
    pub over_limit: Option<String>,
}

impl FormBMeta {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        format: ScanFormat,
        path: String,
        outcome: ScannedOutcome,
        description_count: u64,
        over_limit: Option<String>,
    ) -> FormBMeta {
        FormBMeta {
            format,
            path,
            outcome,
            description_count,
            over_limit,
        }
    }
}

/// The form B meta as written: keys in sorted order, `detail` absent when empty, `overLimit` and `reason` absent when
/// none (W3's three examples).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FormBMetaOut<'a> {
    description_count: u64,
    descriptions_blob: u32,
    // Its mutant (never in a normal build): the blob format's prose label written as a meta key, which no frame
    // carries (the label tests fail).
    #[cfg(mutant_scan_form_label_on_wire)]
    descriptions_format: &'static str,
    #[serde(skip_serializing_if = "str::is_empty")]
    detail: &'a str,
    format: ScanFormat,
    outcome: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    over_limit: Option<&'a str>,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
}

/// The `reason` word of a failure, as [`ScanFailure`]'s serde writes it (pinned by a test against serde's own).
fn reason_word(f: &ScanFailure) -> &str {
    match f {
        ScanFailure::NotFound => "notFound",
        ScanFailure::Arch => "arch",
        ScanFailure::Instantiate => "instantiate",
        ScanFailure::Prepare => "prepare",
        ScanFailure::Process => "process",
        ScanFailure::State => "state",
        ScanFailure::Ui => "ui",
        ScanFailure::ProtocolAfterScan => "protocolAfterScan",
        ScanFailure::Other { reason } => reason,
    }
}

impl FormBMeta {
    /// The meta's bytes (compact JSON, keys sorted). Infallible for these types; `None` only if serde_json ever
    /// failed, which the encoder turns into the L1 header rather than a panic.
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        let (outcome, reason, detail) = match &self.outcome {
            ScannedOutcome::Ok => ("ok", None, ""),
            ScannedOutcome::Failed { failure, detail } => {
                ("failed", Some(reason_word(failure)), detail.as_str())
            }
        };
        serde_json::to_vec(&FormBMetaOut {
            description_count: self.description_count,
            descriptions_blob: SCAN_DESCRIPTIONS_BLOB,
            #[cfg(mutant_scan_form_label_on_wire)]
            descriptions_format: concat!("sas", "-scan-descriptions/1"),
            detail,
            format: self.format,
            outcome,
            over_limit: self.over_limit.as_deref(),
            path: &self.path,
            reason,
        })
        .ok()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FormBMetaUnchecked {
    format: ScanFormat,
    path: String,
    #[serde(flatten)]
    outcome: ScannedOutcome,
    description_count: u64,
    #[serde(default)]
    over_limit: Option<String>,
}

/// A `scanned` meta, parsed (W5 step 2): form A (protocol 3.1, frozen) or form B.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScannedMeta {
    /// Form A (the descriptions in the meta).
    A(Scanned),
    /// Form B (the descriptions in the blob).
    B(FormBMeta),
}

/// W5 step 2's parse. The meta is a JSON object ([`parse_meta`]'s check); the key `descriptionsBlob` decides the form:
/// absent, form A, decoded exactly as 3.1's [`Scanned`]; present, form B, and it must be the integer 1, with no
/// `descriptions` or `discovered` key (step 4's key rule), and a `descriptionCount`. A child's `protocolAfterScan` is
/// refused in either form.
pub fn parse_scanned_meta(meta: &[u8]) -> Result<ScannedMeta, ScannedFault> {
    let v: serde_json::Value = parse_meta(meta)
        .map_err(|e| ScannedFault::Malformed(ScannedMalformed::Meta(bounded(e))))?;
    let Some(obj) = v.as_object() else {
        return malformed(ScannedMalformed::Meta("meta is not a JSON object".into()));
    };
    let Some(version) = obj.get("descriptionsBlob") else {
        return serde_json::from_value::<Scanned>(v)
            .map(ScannedMeta::A)
            .map_err(|e| ScannedFault::Malformed(ScannedMalformed::Meta(bounded(e))));
    };
    if version.as_u64() != Some(u64::from(SCAN_DESCRIPTIONS_BLOB)) {
        return malformed(ScannedMalformed::Version(bounded(version)));
    }
    for key in ["descriptions", "discovered"] {
        if obj.contains_key(key) {
            return malformed(ScannedMalformed::ForbiddenKey(key));
        }
    }
    let u: FormBMetaUnchecked = serde_json::from_value(v)
        .map_err(|e| ScannedFault::Malformed(ScannedMalformed::Meta(bounded(e))))?;
    if let ScannedOutcome::Failed {
        failure: ScanFailure::ProtocolAfterScan,
        ..
    } = u.outcome
    {
        return malformed(ScannedMalformed::Meta(
            "protocolAfterScan is the supervisor's reason; a child cannot send it".into(),
        ));
    }
    Ok(ScannedMeta::B(FormBMeta {
        format: u.format,
        path: u.path,
        outcome: u.outcome,
        description_count: u.description_count,
        over_limit: u.over_limit,
    }))
}

// ---------------------------------------------------------------------------------------------------------------------
// The decoded line
// ---------------------------------------------------------------------------------------------------------------------

/// A form B `scanned`, decoded: the outcome, and the blob's descriptions in the child's discovery order. Their meaning
/// is the outcome's (W4): `ok`, the validated `descriptions`; `failed`, `discovered`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScannedB {
    /// The bundle's format.
    pub format: ScanFormat,
    /// The scanned bundle's path.
    pub path: String,
    /// The scan's outcome.
    pub outcome: ScannedOutcome,
    /// The blob's classes, in the child's discovery order.
    pub descriptions: Vec<ScanClass>,
}

impl ScannedB {
    /// The message of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        format: ScanFormat,
        path: String,
        outcome: ScannedOutcome,
        descriptions: Vec<ScanClass>,
    ) -> ScannedB {
        ScannedB {
            format,
            path,
            outcome,
            descriptions,
        }
    }
}

/// A whole `scanned`, accepted: either form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScannedLine {
    /// A form A `scanned`.
    A(Scanned),
    /// A form B `scanned`, its blob decoded.
    B(ScannedB),
}

impl ScannedLine {
    /// The verdict and the discovered classes (W4): a form B `failed` carries discovery's classes beside an unchanged
    /// `failed {reason, detail}` verdict; form A, and every `ok`, carry none.
    pub fn into_verdict(self) -> (ScanResult, Vec<ScanClass>) {
        match self {
            ScannedLine::A(s) => (ScanResult::from_scanned(s), Vec::new()),
            ScannedLine::B(b) => match b.outcome {
                ScannedOutcome::Ok => (
                    ScanResult::Ok {
                        descriptions: b.descriptions,
                    },
                    Vec::new(),
                ),
                // Its mutant (never in a normal build): the discovered classes are dropped.
                #[cfg(mutant_discovered_dropped)]
                ScannedOutcome::Failed { failure, detail } => {
                    (ScanResult::Failed { failure, detail }, Vec::new())
                }
                #[cfg(not(mutant_discovered_dropped))]
                ScannedOutcome::Failed { failure, detail } => {
                    (ScanResult::Failed { failure, detail }, b.descriptions)
                }
            },
        }
    }
}

impl ScannedMeta {
    /// The scanned bundle's format.
    pub fn format(&self) -> ScanFormat {
        match self {
            ScannedMeta::A(s) => s.format,
            ScannedMeta::B(b) => b.format,
        }
    }

    /// The scanned bundle's path.
    pub fn path(&self) -> &str {
        match self {
            ScannedMeta::A(s) => &s.path,
            ScannedMeta::B(b) => &b.path,
        }
    }

    /// W5 steps 3 to 9, after the caller's echo check (step 2). `offered`: this host sent `descriptionsBlob: 1` in the
    /// `scan` this answers. Form A: `blob` must be empty (step 3). Form B: offered (step 4); `overLimit` gives L2 with
    /// an empty blob, a strike with one (step 5); then [`decode_descriptions_blob`] (steps 6 to 9).
    pub fn with_blob(self, blob: &[u8], offered: bool) -> Result<ScannedLine, ScannedFault> {
        let b = match self {
            ScannedMeta::A(s) => {
                if !blob.is_empty() {
                    return malformed(ScannedMalformed::FormABlob {
                        blob_len: blob.len(),
                    });
                }
                return Ok(ScannedLine::A(s));
            }
            ScannedMeta::B(b) => b,
        };
        if !offered {
            return malformed(ScannedMalformed::NotOffered);
        }
        if let Some(word) = b.over_limit {
            if !blob.is_empty() {
                return malformed(ScannedMalformed::OverLimitBlob {
                    blob_len: blob.len(),
                });
            }
            return limit(ScannedLimit::OverLimit {
                word,
                description_count: b.description_count,
            });
        }
        let descriptions = decode_descriptions_blob(blob, b.description_count)?;
        Ok(ScannedLine::B(ScannedB {
            format: b.format,
            path: b.path,
            outcome: b.outcome,
            descriptions,
        }))
    }
}

/// Both halves of the decode on one frame, with no echo check: a `scanned` EVENT with id 0, its meta parsed (step 2),
/// then [`ScannedMeta::with_blob`]. The scan supervisor runs the halves itself, with S1's echo between them.
pub fn decode_scanned(f: &Frame, offered: bool) -> Result<ScannedLine, ScannedFault> {
    let h = &f.header;
    if h.kind != kind::SCANNED || h.flags != FF_EVENT || h.id != 0 {
        return malformed(ScannedMalformed::NotScanned {
            kind: h.kind,
            flags: h.flags,
            id: h.id,
        });
    }
    parse_scanned_meta(&f.meta)?.with_blob(&f.blob, offered)
}

// ---------------------------------------------------------------------------------------------------------------------
// The blob (W2)
// ---------------------------------------------------------------------------------------------------------------------

/// W5 steps 6 to 9 on a form B blob of `description_count` lines (`plugin-scan-descriptions/1`, W2), the first failing
/// check deciding:
/// 6. the count is at most [`MAX_SCAN_DESCRIPTIONS`] (L3), before anything is allocated;
/// 7. the framing, one pass with no allocation: a count of 0 with an empty blob, above 0 with a non-empty one; the last
///    byte 0x0A; exactly `description_count` 0x0A bytes (a strike);
/// 8. every line's length, its newline excluded, at most [`MAX_DESCRIPTION_LINE`] (L4), before any line is parsed: a
///    long line is the limit even if it would not parse;
/// 9. every line's content: first byte `{`, UTF-8, a [`ScanClass`] (its decode checks it), and its canonical bytes (a
///    strike naming the line).
///
/// The lines are parsed in place (slices of `blob`); the descriptions are the only allocation, bounded by the count.
/// **Canonical lines only** (replacing W2's "the receiver does not check canonicality"):
/// each parsed line re-encodes, through the encoder's own serialisation, to exactly its bytes, compared as it is
/// written with no allocation, or it is [`LineFault::NonCanonical`]. So an unknown key (a newer child's additive
/// field) is refused in form B: a new description field needs a new form B version (the offer), never a silent
/// addition. Form A keeps D20's additive rule.
pub fn decode_descriptions_blob(
    blob: &[u8],
    description_count: u64,
) -> Result<Vec<ScanClass>, ScannedFault> {
    // 6. L3. Its mutant (never in a normal build): no count cap.
    #[cfg(not(mutant_scan_descriptions_uncapped))]
    if description_count > MAX_SCAN_DESCRIPTIONS as u64 {
        return limit(ScannedLimit::Count { description_count });
    }
    // 7. The framing.
    if description_count == 0 {
        if !blob.is_empty() {
            return malformed(ScannedMalformed::BlobWithoutCount {
                blob_len: blob.len(),
            });
        }
        return Ok(Vec::new());
    }
    if blob.is_empty() {
        return malformed(ScannedMalformed::CountWithoutBlob { description_count });
    }
    if blob.last() != Some(&b'\n') {
        return malformed(ScannedMalformed::NoFinalNewline);
    }
    let newlines = blob.iter().filter(|b| **b == b'\n').count();
    // Its mutant (never in a normal build): the count is not compared with the newlines.
    #[cfg(not(mutant_scan_blob_count_unchecked))]
    if newlines as u64 != description_count {
        return malformed(ScannedMalformed::CountMismatch {
            description_count,
            newlines,
        });
    }
    // The lines: the pieces before each 0x0A (the last byte is one, so the piece after it is empty and dropped).
    let lines = || blob[..blob.len() - 1].split(|b| *b == b'\n');
    // 8 then 9; their mutant (never in a normal build) swaps them: each line parsed before its size is checked.
    #[cfg(not(mutant_scan_line_size_after_parse))]
    for (index, line) in lines().enumerate() {
        check_line_len(index, line)?;
    }
    let mut out = Vec::with_capacity(newlines.min(MAX_SCAN_DESCRIPTIONS));
    for (index, line) in lines().enumerate() {
        out.push(parse_line(index, line)?);
        #[cfg(mutant_scan_line_size_after_parse)]
        check_line_len(index, line)?;
    }
    Ok(out)
}

fn check_line_len(index: usize, line: &[u8]) -> Result<(), ScannedFault> {
    // L4. Its mutant (never in a normal build): no line cap.
    #[cfg(not(mutant_scan_line_uncapped))]
    if line.len() > MAX_DESCRIPTION_LINE {
        return limit(ScannedLimit::Line {
            index,
            len: line.len(),
        });
    }
    let _ = (index, line);
    Ok(())
}

fn parse_line(index: usize, line: &[u8]) -> Result<ScanClass, ScannedFault> {
    let bad = |why| ScannedFault::Malformed(ScannedMalformed::Line { index, why });
    // Its mutant (never in a normal build): the `{` check skipped, so serde's sequence form is accepted.
    #[cfg(not(mutant_scan_line_seq_accepted))]
    if line.first() != Some(&b'{') {
        return Err(bad(LineFault::NotAnObject));
    }
    if let Err(e) = std::str::from_utf8(line) {
        return Err(bad(LineFault::NotUtf8 {
            at: e.valid_up_to(),
        }));
    }
    let (d, unknown) = crate::plugin::decode_scan_class(line)
        .map_err(|e| bad(LineFault::Description(bounded(e))))?;
    // Canonical bytes only. Its mutant (never in a normal build): the compare skipped.
    // A line with fields this codec does not know (a newer child's additive facts, dropped and
    // noted) cannot re-encode to its own bytes: the compare is for lines of known fields only.
    #[cfg(not(mutant_scan_line_canon_unchecked))]
    if !unknown {
        let mut same = SameAs {
            rest: line,
            at: 0,
            differs: None,
        };
        if serde_json::to_writer(&mut same, &d).is_err() {
            return Err(bad(LineFault::NonCanonical { at: same.at }));
        }
        if let Some(at) = same.differs {
            return Err(bad(LineFault::NonCanonical { at }));
        }
        if !same.rest.is_empty() {
            return Err(bad(LineFault::NonCanonical { at: same.at }));
        }
    }
    Ok(d)
}

/// A writer that compares what is written with `rest` and keeps nothing: the canonical check with no allocation.
struct SameAs<'a> {
    rest: &'a [u8],
    at: usize,
    differs: Option<usize>,
}

impl std::io::Write for SameAs<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.differs.is_none() {
            if self.rest.starts_with(b) {
                self.rest = &self.rest[b.len()..];
                self.at += b.len();
            } else {
                self.differs = Some(self.at);
            }
        }
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// The encoder (W8 step 5): the scan child's
// ---------------------------------------------------------------------------------------------------------------------

/// The bound a form B sender passed (W8), named by `overLimit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OverLimit {
    /// More than [`MAX_SCAN_DESCRIPTIONS`] descriptions.
    Count,
    /// A line over [`MAX_DESCRIPTION_LINE`].
    Line,
    /// The blob over [`MAX_BLOB`].
    Blob,
}

impl OverLimit {
    /// The `overLimit` word on the wire.
    pub fn word(self) -> &'static str {
        match self {
            OverLimit::Count => "count",
            OverLimit::Line => "line",
            OverLimit::Blob => "blob",
        }
    }
}

/// What the scan child writes as its one `scanned` (kind 65, `FF_EVENT`, id 0). Every variant is bytes to send
/// ([`ScannedFrame::bytes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScannedFrame {
    /// The whole frame: form A, or form B with its blob. Then `shutdown` as usual.
    Whole(Vec<u8>),
    /// Form B past a bound (W8): the form B meta with the true `descriptionCount`, `overLimit` naming the first bound
    /// passed, and an empty blob; the host application's L2. Then `shutdown` as usual.
    OverLimit {
        /// The first bound passed.
        bound: OverLimit,
        /// The whole frame: the meta with `overLimit`, and an empty blob.
        frame: Vec<u8>,
    },
    /// The meta itself passes [`MAX_META`] (a form A shell bundle's line; or a form B meta whose `path` or `detail` is
    /// past it): only the 24-byte header with its true lengths, which a host application at or after `8e05b95`
    /// makes L1's EngineFault. Then wait for the host application's EOF. A form B header-only names the meta's length and a blob of
    /// 0 (the blob is never built).
    HeaderOnly {
        /// The frame header's bytes, with the true lengths.
        header: [u8; FRAME_HEADER],
        /// The meta's true length in bytes.
        meta_len: usize,
        /// The blob's length in bytes (0: never built).
        blob_len: usize,
    },
}

impl ScannedFrame {
    /// The bytes to send: the whole frame, or the 24 header bytes alone.
    pub fn bytes(&self) -> &[u8] {
        match self {
            ScannedFrame::Whole(v) | ScannedFrame::OverLimit { frame: v, .. } => v,
            ScannedFrame::HeaderOnly { header, .. } => header,
        }
    }
}

/// The header-only helper: the 24 header bytes of a frame whose meta and blob are never sent, with their true lengths, so the
/// header passes every check but the caps (the length identity included). Lengths that cannot be written together in
/// `u32` are clamped (the meta first, the blob to what is left) so the identity still holds: still past the caps.
pub fn encode_header_only(
    kind: u16,
    flags: u16,
    id: u64,
    meta_len: usize,
    blob_len: usize,
) -> [u8; FRAME_HEADER] {
    let room = u64::from(u32::MAX - 20);
    let m = (meta_len as u64).min(room);
    let b = (blob_len as u64).min(room - m);
    header_bytes(kind, flags, id, m as u32, b as u32)
}

fn header_only(meta_len: usize, blob_len: usize) -> ScannedFrame {
    ScannedFrame::HeaderOnly {
        header: encode_header_only(kind::SCANNED, FF_EVENT, 0, meta_len, blob_len),
        meta_len,
        blob_len,
    }
}

/// The scan child's one call (W8 step 5): form B when the `scan` offered it ([`super::Scan::offers_descriptions_blob`]),
/// form A otherwise. Never panics.
pub fn encode_scanned(s: &Scanned, offered: bool) -> ScannedFrame {
    if offered {
        encode_scanned_form_b(s)
    } else {
        encode_scanned_form_a(s)
    }
}

/// Form A, byte-identical to protocol 3.1's `Scanned` at `1488c63` (the frozen form): `descriptions` only on `ok`,
/// `[]` on `failed`, no `discovered`. A meta past [`MAX_META`] is [`ScannedFrame::HeaderOnly`], never a panic.
pub fn encode_scanned_form_a(s: &Scanned) -> ScannedFrame {
    let failed_copy;
    let s = match s.outcome {
        ScannedOutcome::Failed { .. } if !s.classes.is_empty() => {
            failed_copy = Scanned {
                format: s.format,
                path: s.path.clone(),
                outcome: s.outcome.clone(),
                classes: Vec::new(),
            };
            &failed_copy
        }
        _ => s,
    };
    let Ok(meta) = serde_json::to_vec(s) else {
        return header_only(MAX_META + 1, 0);
    };
    if meta.len() > MAX_META {
        return header_only(meta.len(), 0);
    }
    ScannedFrame::Whole(encode(kind::SCANNED, FF_EVENT, 0, &meta, &[]))
}

/// Form B (W2, W3): the meta with `descriptionsBlob: 1` and `descriptionCount`, then every description of `s` in its
/// order, each `serde_json`'s compact encoding of [`ScanClass`] (its field order, absent fields
/// left out) and one 0x0A, in the same frame. The first bound passed (the count, checked first; then, line by line
/// in order, a line over [`MAX_DESCRIPTION_LINE`] or the blob over [`MAX_BLOB`]) gives [`ScannedFrame::OverLimit`];
/// a meta past [`MAX_META`] gives [`ScannedFrame::HeaderOnly`]. Built in one buffer, header first; never panics. The
/// sender passes each description through `check` before (W8 step 4); this does not.
pub fn encode_scanned_form_b(s: &Scanned) -> ScannedFrame {
    let n = s.classes.len();
    let meta = FormBMeta {
        format: s.format,
        path: s.path.clone(),
        outcome: s.outcome.clone(),
        description_count: n as u64,
        over_limit: None,
    };
    let over = |bound: OverLimit| {
        let m = FormBMeta {
            over_limit: Some(bound.word().to_string()),
            ..meta.clone()
        };
        match m.to_bytes() {
            Some(b) if b.len() <= MAX_META => ScannedFrame::OverLimit {
                bound,
                frame: encode(kind::SCANNED, FF_EVENT, 0, &b, &[]),
            },
            Some(b) => header_only(b.len(), 0),
            None => header_only(MAX_META + 1, 0),
        }
    };
    // Its mutant (never in a normal build): the encoder's count bound skipped.
    #[cfg(not(mutant_scan_encode_count_unchecked))]
    if n > MAX_SCAN_DESCRIPTIONS {
        return over(OverLimit::Count);
    }
    let Some(m) = meta.to_bytes() else {
        return header_only(MAX_META + 1, 0);
    };
    if m.len() > MAX_META {
        return header_only(m.len(), 0);
    }
    let mut buf = Vec::with_capacity(FRAME_HEADER + m.len() + n.saturating_mul(512).min(MAX_BLOB));
    buf.extend_from_slice(&[0u8; FRAME_HEADER]);
    buf.extend_from_slice(&m);
    let blob_at = buf.len();
    for d in &s.classes {
        let mark = buf.len();
        if serde_json::to_writer(&mut buf, d).is_err() {
            return header_only(MAX_META + 1, 0);
        }
        // Its mutants (never in a normal build): the encoder's line bound, or its blob bound, skipped.
        #[cfg(not(mutant_scan_encode_line_unchecked))]
        if buf.len() - mark > MAX_DESCRIPTION_LINE {
            return over(OverLimit::Line);
        }
        buf.push(b'\n');
        #[cfg(not(mutant_scan_encode_blob_unchecked))]
        if buf.len() - blob_at > MAX_BLOB {
            return over(OverLimit::Blob);
        }
    }
    let blob_len = buf.len() - blob_at;
    // The bounds hold here (the meta within MAX_META, the blob within MAX_BLOB), so both lengths and the frame length
    // 20 + meta + blob fit in u32.
    buf[..FRAME_HEADER].copy_from_slice(&header_bytes(
        kind::SCANNED,
        FF_EVENT,
        0,
        m.len() as u32,
        blob_len as u32,
    ));
    ScannedFrame::Whole(buf)
}

// ---------------------------------------------------------------------------------------------------------------------
// Tests: the scanned wire's codec half, its rows, the failed verdict's classes, the invariants. Which test each mutant fails (`cargo rustc -p summoner-proto --lib --profile test -- --cfg <mutant>`):
// - `mutant_scan_descriptions_uncapped`: `the_count_bound_is_16384_and_one_more_is_l3`;
// - `mutant_scan_line_uncapped`: `a_line_of_64_kib_is_accepted_and_one_byte_more_is_l4`;
// - `mutant_oversized_blob_struck`: `the_header_is_l1_for_either_cap_alone_and_never_with_another_fault`;
// - `mutant_scan_blob_count_unchecked`: `every_strike_row_is_malformed_and_names_its_step`;
// - `mutant_scan_line_seq_accepted`: `every_strike_row_is_malformed_and_names_its_step` (the sequence row);
// - `mutant_scan_line_size_after_parse`: `size_comes_before_content_and_framing_before_size`;
// - `mutant_discovered_dropped`: `a_failed_form_b_carries_its_discovered_classes_and_the_same_verdict`;
// - `mutant_scan_encode_count_unchecked`: `the_encoder_reports_count_past_16384`;
// - `mutant_scan_encode_line_unchecked`: `the_encoder_reports_a_line_past_64_kib`;
// - `mutant_scan_encode_blob_unchecked`: `the_encoder_reports_a_blob_past_64_mib`;
// - `mutant_scan_line_canon_unchecked`: `a_non_canonical_line_is_refused`;
// - `mutant_scan_form_label_on_wire`: `the_scan_wire_names_no_format_label` and
//   `the_scan_frames_are_protocol_4_0s_bytes`.
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Decoder, Direction, FF_REPLY, Header, ProtoError, Scan, decode_header};
    use crate::layout::{F_BLOB_LEN, F_META_LEN};

    /// W3's three examples, byte for byte (the text's keys are sorted, which is how the encoder writes them).
    const EX1: &str = r#"{"descriptionCount":1000,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"<abs bundle path>"}"#;
    const EX2: &str = r#"{"descriptionCount":3,"descriptionsBlob":1,"detail":"…","format":"vst3","outcome":"failed","path":"…","reason":"process"}"#;
    const EX3: &str = r#"{"descriptionCount":20000,"descriptionsBlob":1,"format":"vst3","outcome":"ok","overLimit":"count","path":"…"}"#;

    /// The fake's synthetic class `fake-scan-class-<i>` (How tested 1), as a 4.0 scan reports it (raw facts).
    fn class(i: usize) -> ScanClass {
        ScanClass::Vst3(crate::plugin::Vst3Class {
            path: "/x/shell.vst3".into(),
            bundle_mtime_ms: 0,
            source: Some(crate::plugin::ScanSource::Factory),
            class_id: format!("{i:032x}"),
            category: "Audio Module Class".into(),
            name: format!("fake-scan-class-{i}"),
            categories: vec!["Fx".into()],
            vendor: "Example Lab".into(),
            class_vendor: None,
            version: "0.1.0".into(),
            sdk_version: "VST 3.8.1".into(),
            instantiated: Some(true),
            inputs: Some(2),
            outputs: Some(2),
        })
    }

    /// The smallest class (every optional fact absent).
    fn default_class() -> ScanClass {
        ScanClass::Vst3(crate::plugin::Vst3Class {
            path: "/x/a.vst3".into(),
            class_id: "0".repeat(32),
            ..Default::default()
        })
    }

    fn vst3(c: &mut ScanClass) -> &mut crate::plugin::Vst3Class {
        match c {
            ScanClass::Vst3(v) => v,
            ScanClass::Au(_) => unreachable!(),
        }
    }

    fn classes(n: usize) -> Vec<ScanClass> {
        (0..n).map(class).collect()
    }

    fn scanned(path: &str, outcome: ScannedOutcome, d: Vec<ScanClass>) -> Scanned {
        Scanned {
            format: ScanFormat::Vst3,
            path: path.into(),
            outcome,
            classes: d,
        }
    }

    fn failed(f: ScanFailure, detail: &str) -> ScannedOutcome {
        ScannedOutcome::Failed {
            failure: f,
            detail: detail.into(),
        }
    }

    /// The one frame in `bytes`, through the host application's streaming decoder.
    fn frame(bytes: &[u8]) -> Frame {
        let mut d = Decoder::new(Direction::ToCore);
        let mut out = Vec::new();
        for c in bytes.chunks(4093) {
            d.feed(c, &mut |f| out.push(f)).unwrap();
        }
        assert_eq!(out.len(), 1);
        out.pop().unwrap()
    }

    fn whole(f: ScannedFrame) -> Vec<u8> {
        match f {
            ScannedFrame::Whole(v) => v,
            other => panic!("not whole: {:?}", other_kind(&other)),
        }
    }

    fn other_kind(f: &ScannedFrame) -> String {
        match f {
            ScannedFrame::Whole(v) => format!("Whole({} bytes)", v.len()),
            ScannedFrame::OverLimit { bound, .. } => format!("OverLimit({bound:?})"),
            ScannedFrame::HeaderOnly {
                meta_len, blob_len, ..
            } => format!("HeaderOnly({meta_len}, {blob_len})"),
        }
    }

    /// A raw `scanned` frame: any meta, any blob (the caps hold).
    fn raw(meta: &str, blob: &[u8]) -> Frame {
        frame(&encode(kind::SCANNED, FF_EVENT, 0, meta.as_bytes(), blob))
    }

    fn b_meta(count: u64) -> String {
        format!(
            r#"{{"descriptionCount":{count},"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3"}}"#
        )
    }

    fn jsonl(d: &[ScanClass]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in d {
            v.extend_from_slice(&serde_json::to_vec(x).unwrap());
            v.push(b'\n');
        }
        v
    }

    /// The default description's canonical line (every field present, `classId` absent).
    fn default_line() -> String {
        serde_json::to_string(&default_class()).unwrap()
    }

    fn mal(r: Result<ScannedLine, ScannedFault>) -> ScannedMalformed {
        match r {
            Err(ScannedFault::Malformed(m)) => m,
            other => panic!("not malformed: {other:?}"),
        }
    }

    fn lim(r: Result<ScannedLine, ScannedFault>) -> ScannedLimit {
        match r {
            Err(ScannedFault::Limit(l)) => l,
            Err(e) => panic!("not a limit: {e:?}"),
            Ok(_) => panic!("accepted"),
        }
    }

    /// W3's three metas are the encoder's bytes, and each decodes and re-encodes to the same bytes (How tested 1's
    /// shape at the codec: 1,000 classes in form B, in order; a `failed` with 3 discovered; 20,000 is `overLimit`).
    #[test]
    fn the_texts_three_metas_are_byte_exact_both_ways() {
        // Example 1: 1,000 lines in the blob, the meta under 1 KiB.
        let s1 = scanned("<abs bundle path>", ScannedOutcome::Ok, classes(1000));
        let bytes = whole(encode_scanned(&s1, true));
        let f = frame(&bytes);
        assert_eq!(std::str::from_utf8(&f.meta).unwrap(), EX1);
        assert!(f.meta.len() < 1024);
        assert_eq!(f.blob, jsonl(&s1.classes));
        assert_eq!(f.blob.iter().filter(|b| **b == b'\n').count(), 1000);
        let back = decode_scanned(&f, true).unwrap();
        let ScannedLine::B(b) = &back else {
            panic!("form B")
        };
        assert_eq!(b.descriptions, s1.classes, "in order, equal");
        assert_eq!(
            whole(encode_scanned_form_b(&scanned(
                &b.path,
                b.outcome.clone(),
                b.descriptions.clone()
            ))),
            bytes,
            "a re-encode equals the bytes"
        );
        let ScannedMeta::B(m) = parse_scanned_meta(EX1.as_bytes()).unwrap() else {
            panic!("form B")
        };
        assert_eq!((m.description_count, m.over_limit.as_deref()), (1000, None));
        assert_eq!(m.to_bytes().unwrap(), EX1.as_bytes());

        // Example 2: a failed validation with 3 discovered classes.
        let s2 = scanned("…", failed(ScanFailure::Process, "…"), classes(3));
        let bytes = whole(encode_scanned(&s2, true));
        let f = frame(&bytes);
        assert_eq!(std::str::from_utf8(&f.meta).unwrap(), EX2);
        let ScannedMeta::B(m) = parse_scanned_meta(EX2.as_bytes()).unwrap() else {
            panic!("form B")
        };
        assert_eq!(m.outcome, failed(ScanFailure::Process, "…"));
        assert_eq!(m.to_bytes().unwrap(), EX2.as_bytes());
        let ScannedLine::B(b) = decode_scanned(&f, true).unwrap() else {
            panic!("form B")
        };
        assert_eq!(b.descriptions, classes(3));
        assert_eq!(
            whole(encode_scanned_form_b(&scanned(
                &b.path,
                b.outcome,
                b.descriptions
            ))),
            bytes
        );

        // Example 3: 20,000 classes pass the count; the line carries no blob and is L2.
        let s3 = scanned("…", ScannedOutcome::Ok, classes(20_000));
        let ScannedFrame::OverLimit { bound, frame: fb } = encode_scanned(&s3, true) else {
            panic!("over the count")
        };
        assert_eq!(bound, OverLimit::Count);
        let f = frame(&fb);
        assert_eq!(std::str::from_utf8(&f.meta).unwrap(), EX3);
        assert!(f.blob.is_empty());
        assert_eq!(
            lim(decode_scanned(&f, true)),
            ScannedLimit::OverLimit {
                word: "count".into(),
                description_count: 20_000
            }
        );
        let ScannedMeta::B(m) = parse_scanned_meta(EX3.as_bytes()).unwrap() else {
            panic!("form B")
        };
        assert_eq!(m.to_bytes().unwrap(), EX3.as_bytes());
    }

    /// W2's bytes: each line is serde_json's compact [`ScanClass`] (protocol 4.0: its field order, absent facts left
    /// out) and one 0x0A; 0, 1 and many lines; the classes round-trip.
    #[test]
    fn the_blob_is_json_lines_of_the_codecs_compact_descriptions() {
        let gain = class(0);
        let line0 = r#"{"format":"vst3","path":"/x/shell.vst3","bundleMtimeMs":0,"source":"factory","classId":"00000000000000000000000000000000","category":"Audio Module Class","name":"fake-scan-class-0","categories":["Fx"],"vendor":"Example Lab","version":"0.1.0","sdkVersion":"VST 3.8.1","instantiated":true,"inputs":2,"outputs":2}"#;
        let line1 = r#"{"format":"vst3","path":"/x/shell.vst3","bundleMtimeMs":0,"source":"factory","classId":"00000000000000000000000000000001","category":"Audio Module Class","name":"fake-scan-class-1","categories":["Fx"],"vendor":"Example Lab","version":"0.1.0","sdkVersion":"VST 3.8.1","instantiated":true,"inputs":2,"outputs":2}"#;
        let s = scanned(
            "/x/shell.vst3",
            ScannedOutcome::Ok,
            vec![gain.clone(), class(1)],
        );
        let f = frame(&whole(encode_scanned(&s, true)));
        assert_eq!(f.blob, format!("{line0}\n{line1}\n").as_bytes());
        assert_eq!(
            decode_descriptions_blob(&f.blob, 2).unwrap(),
            vec![gain, class(1)]
        );
        // 0 lines: an empty blob (and an `ok` with no classes is accepted, as form A's `[]` is).
        let s0 = scanned("/x/shell.vst3", ScannedOutcome::Ok, vec![]);
        let bytes = whole(encode_scanned(&s0, true));
        let f = frame(&bytes);
        assert!(f.blob.is_empty() && f.header.blob_len == 0);
        assert!(
            std::str::from_utf8(&f.meta)
                .unwrap()
                .starts_with(r#"{"descriptionCount":0,"#)
        );
        let ScannedLine::B(b) = decode_scanned(&f, true).unwrap() else {
            panic!("form B")
        };
        assert!(b.descriptions.is_empty());
        // 1 and 100 lines.
        for n in [1, 100] {
            let s = scanned("/x/shell.vst3", ScannedOutcome::Ok, classes(n));
            let f = frame(&whole(encode_scanned(&s, true)));
            let (r, disc) = decode_scanned(&f, true).unwrap().into_verdict();
            assert_eq!(
                r,
                ScanResult::Ok {
                    descriptions: classes(n)
                }
            );
            assert!(disc.is_empty());
        }
        // The default description in its canonical bytes is a line; an unknown META key is dropped (the meta keeps
        // D20's additive rule; the lines are canonical only).
        let blob = format!("{}\n", default_line());
        let d = decode_descriptions_blob(blob.as_bytes(), 1).unwrap();
        assert_eq!(d[0], default_class());
        let f = raw(
            r#"{"descriptionCount":1,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3","newer":{"x":1}}"#,
            blob.as_bytes(),
        );
        assert!(decode_scanned(&f, true).is_ok());
    }

    /// The `reason` word the form B meta writes is serde's own for every failure (so form A and B agree).
    #[test]
    fn the_form_b_reason_word_is_serdes() {
        for f in [
            ScanFailure::NotFound,
            ScanFailure::Arch,
            ScanFailure::Instantiate,
            ScanFailure::Prepare,
            ScanFailure::Process,
            ScanFailure::State,
            ScanFailure::Ui,
            ScanFailure::ProtocolAfterScan,
            ScanFailure::Other {
                reason: "newerReason".into(),
            },
        ] {
            let v = serde_json::to_value(&f).unwrap();
            assert_eq!(v["reason"], reason_word(&f), "{f:?}");
        }
        // And a newer word round-trips through form B as ScanFailure::Other.
        let s = scanned(
            "/x/a.vst3",
            failed(
                ScanFailure::Other {
                    reason: "newerReason".into(),
                },
                "",
            ),
            vec![],
        );
        let f = frame(&whole(encode_scanned(&s, true)));
        let ScannedLine::B(b) = decode_scanned(&f, true).unwrap() else {
            panic!("form B")
        };
        assert_eq!(b.outcome, s.outcome);
    }

    /// How tested 2: with no offer the encoder writes form A byte-identical to protocol 3.1's `Scanned` at `1488c63`
    /// (the fixture below is that commit's serde shape: `git diff 1488c63 aa300aa` shows no change to `ScanFormat` ..
    /// `ScanResult`, nor to the lock's serde crates): descriptions only on `ok`, `[]` on `failed`, no `discovered`.
    /// Form A decodes as today; form A's frame with no blob is accepted whether or not the host application offered form B (an
    /// old child ignores the offer).
    #[test]
    fn with_no_offer_the_encoder_writes_form_a_as_1488c63_did() {
        // 1488c63's shape with protocol 4.0's payload: `classes` (raw facts) where 3.x had
        // `descriptions`; every other byte of the frame as before.
        const DESC: &str = r#"{"format":"vst3","path":"/x/gain.vst3","bundleMtimeMs":0,"classId":"00000000000000000000000000001234","category":"","name":"gain","categories":[],"vendor":"","version":"","sdkVersion":"","inputs":2,"outputs":2}"#;
        let gain = ScanClass::Vst3(crate::plugin::Vst3Class {
            path: "/x/gain.vst3".into(),
            class_id: format!("{:032x}", 0x1234),
            name: "gain".into(),
            inputs: Some(2),
            outputs: Some(2),
            ..Default::default()
        });
        let ok = scanned("/x/gain.vst3", ScannedOutcome::Ok, vec![gain.clone()]);
        let golden_ok = format!(
            r#"{{"format":"vst3","path":"/x/gain.vst3","outcome":"ok","classes":[{DESC}]}}"#
        );
        let bytes = whole(encode_scanned(&ok, false));
        assert_eq!(
            bytes,
            encode(kind::SCANNED, FF_EVENT, 0, golden_ok.as_bytes(), &[])
        );
        assert_eq!(
            bytes,
            crate::codec::encode_msg(kind::SCANNED, FF_EVENT, 0, &ok, &[]),
            "what host's child sends today"
        );
        // A failed line keeps 3.1's `[]`, even when the report holds discovered classes.
        let fl = scanned(
            "/x/h.vst3",
            failed(ScanFailure::Process, "8 blocks"),
            vec![gain.clone()],
        );
        let golden_failed = r#"{"format":"vst3","path":"/x/h.vst3","outcome":"failed","reason":"process","detail":"8 blocks","classes":[]}"#;
        let bytes = whole(encode_scanned(&fl, false));
        assert_eq!(
            bytes,
            encode(kind::SCANNED, FF_EVENT, 0, golden_failed.as_bytes(), &[])
        );
        // Decoded: form A, either offer; its verdict as today, with no discovered classes.
        for offered in [false, true] {
            let line = decode_scanned(&frame(&bytes), offered).unwrap();
            assert!(matches!(line, ScannedLine::A(_)));
            let (r, disc) = line.into_verdict();
            assert_eq!(
                r,
                ScanResult::Failed {
                    failure: ScanFailure::Process,
                    detail: "8 blocks".into()
                }
            );
            assert!(disc.is_empty());
            let ScannedLine::A(a) =
                decode_scanned(&frame(&whole(encode_scanned(&ok, false))), offered).unwrap()
            else {
                panic!("form A")
            };
            assert_eq!(a, ok);
        }
    }

    /// 1488c63's `ScanUnchecked`, frozen: the old child's view of a `scan`.
    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct ScanUnchecked1488c63 {
        format: ScanFormat,
        path: String,
    }

    /// W1's offer: `Scan::new` makes none (bytes as 3.1's), the builder adds `descriptionsBlob: 1`, an old child's
    /// frozen decode ignores it, the path rules still hold, and only version 1 is form B for this codec.
    #[test]
    fn the_offer_is_one_optional_key_an_old_child_ignores() {
        let plain = Scan::new(ScanFormat::Vst3, "/x/a.vst3").unwrap();
        assert_eq!(plain.descriptions_blob, None);
        assert!(!plain.offers_descriptions_blob());
        assert_eq!(
            serde_json::to_string(&plain).unwrap(),
            r#"{"format":"vst3","path":"/x/a.vst3"}"#
        );
        let offer = plain.clone().offer_descriptions_blob();
        let bytes = serde_json::to_string(&offer).unwrap();
        assert_eq!(
            bytes,
            r#"{"format":"vst3","path":"/x/a.vst3","descriptionsBlob":1}"#
        );
        assert!(offer.offers_descriptions_blob());
        assert_eq!(serde_json::from_str::<Scan>(&bytes).unwrap(), offer);
        // The old child's view: it decodes, and the offer is dropped.
        let old: ScanUnchecked1488c63 = serde_json::from_str(&bytes).unwrap();
        assert_eq!(
            (old.format, old.path.as_str()),
            (ScanFormat::Vst3, "/x/a.vst3")
        );
        // Through a real `scan` frame, as the child reads it.
        let f = crate::codec::encode_msg(kind::SCAN, 0, 5, &offer, &[]);
        let mut d = Decoder::new(Direction::ToChild);
        let mut got = Vec::new();
        d.feed(&f, &mut |x| got.push(x)).unwrap();
        assert_eq!(got[0].meta_as::<Scan>().unwrap(), offer);
        // Path rules unchanged with an offer.
        for bad in [
            r#"{"format":"vst3","path":"a.vst3","descriptionsBlob":1}"#,
            r#"{"format":"au","path":"/x/a.vst3","descriptionsBlob":1}"#,
        ] {
            assert!(serde_json::from_str::<Scan>(bad).is_err(), "{bad}");
        }
        // Another version, or null: not this codec's form B.
        let two: Scan =
            serde_json::from_str(r#"{"format":"vst3","path":"/x/a.vst3","descriptionsBlob":2}"#)
                .unwrap();
        assert!(!two.offers_descriptions_blob());
        let null: Scan =
            serde_json::from_str(r#"{"format":"vst3","path":"/x/a.vst3","descriptionsBlob":null}"#)
                .unwrap();
        assert_eq!(null, plain);
        assert!(
            serde_json::from_str::<Scan>(
                r#"{"format":"vst3","path":"/x/a.vst3","descriptionsBlob":"1"}"#
            )
            .is_err()
        );
    }

    /// W5's strike rows (How tested 4's codec half), each `Malformed` and named by its step.
    #[test]
    fn every_strike_row_is_malformed_and_names_its_step() {
        let one = b"{}\n".to_vec();
        let ok_a = r#"{"format":"vst3","path":"/x/a.vst3","outcome":"ok","descriptions":[]}"#;
        // 3: form A with a blob.
        assert_eq!(
            mal(decode_scanned(&raw(ok_a, b"{}\n"), true)),
            ScannedMalformed::FormABlob { blob_len: 3 }
        );
        // 2/4: form B with `descriptions`, with `discovered`.
        for key in ["descriptions", "discovered"] {
            let m = format!(
                r#"{{"descriptionCount":1,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3","{key}":[]}}"#
            );
            assert_eq!(
                mal(decode_scanned(&raw(&m, &one), true)),
                ScannedMalformed::ForbiddenKey(key)
            );
        }
        // 2: `descriptionsBlob` 2, "1", null, 1.0, -1.
        for v in ["2", "\"1\"", "null", "1.0", "-1", "0"] {
            let m = format!(
                r#"{{"descriptionCount":1,"descriptionsBlob":{v},"format":"vst3","outcome":"ok","path":"/x/a.vst3"}}"#
            );
            assert!(
                matches!(
                    mal(decode_scanned(&raw(&m, &one), true)),
                    ScannedMalformed::Version(_)
                ),
                "{v}"
            );
        }
        // 2: no `descriptionCount`, a negative one, a fraction, a string; the supervisor's own reason; no format.
        for m in [
            r#"{"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":-1,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":1.5,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":"1","descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":0,"descriptionsBlob":1,"format":"vst3","outcome":"failed","reason":"protocolAfterScan","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":0,"descriptionsBlob":1,"outcome":"ok","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":0,"descriptionsBlob":1,"format":"vst3","outcome":"crashed","path":"/x/a.vst3"}"#,
            r#"{"descriptionCount":0,"descriptionsBlob":1,"format":"vst3","outcome":"ok","path":"/x/a.vst3","overLimit":3}"#,
            r#"[1]"#,
        ] {
            assert!(
                matches!(
                    mal(decode_scanned(&raw(m, b""), true)),
                    ScannedMalformed::Meta(_)
                ),
                "{m}"
            );
        }
        // 4: form B the host application did not offer.
        assert_eq!(
            mal(decode_scanned(&raw(&b_meta(1), &one), false)),
            ScannedMalformed::NotOffered
        );
        // 5: overLimit with a blob.
        let over = r#"{"descriptionCount":1,"descriptionsBlob":1,"format":"vst3","outcome":"ok","overLimit":"line","path":"/x/a.vst3"}"#;
        assert_eq!(
            mal(decode_scanned(&raw(over, &one), true)),
            ScannedMalformed::OverLimitBlob { blob_len: 3 }
        );
        // 7: the framing.
        let rows: [(u64, &[u8], ScannedMalformed); 6] = [
            (
                2,
                b"{}\n",
                ScannedMalformed::CountMismatch {
                    description_count: 2,
                    newlines: 1,
                },
            ),
            (
                1,
                b"{}\n{}\n",
                ScannedMalformed::CountMismatch {
                    description_count: 1,
                    newlines: 2,
                },
            ),
            (1, b"{}", ScannedMalformed::NoFinalNewline),
            (2, b"{}\n{}", ScannedMalformed::NoFinalNewline),
            (
                0,
                b"{}\n",
                ScannedMalformed::BlobWithoutCount { blob_len: 3 },
            ),
            (
                1,
                b"",
                ScannedMalformed::CountWithoutBlob {
                    description_count: 1,
                },
            ),
        ];
        for (count, blob, want) in rows {
            assert_eq!(
                mal(decode_scanned(&raw(&b_meta(count), blob), true)),
                want,
                "{count} {blob:?}"
            );
        }
        // 9: each line's content, naming its index.
        let good = serde_json::to_string(&class(0)).unwrap();
        // serde's sequence form of a whole class: the `{` check refuses it first, by name. (Since the S2 amend's
        // tolerated unknown fields, a flattened map, serde refuses the sequence form too; the check stays the first.)
        let seq = r#"["vst3","/x/a.vst3",0,"factory","00000000000000000000000000000000","Audio Module Class","n",[],"V",null,"1","s"]"#;
        let upper = serde_json::to_string(&class(0)).unwrap().replace(
            "00000000000000000000000000000000",
            "5341534C4C4142010000005000000000",
        );
        let line_rows: [(Vec<u8>, LineFault); 5] = [
            (b"".to_vec(), LineFault::NotAnObject),
            (seq.as_bytes().to_vec(), LineFault::NotAnObject),
            (b" {}".to_vec(), LineFault::NotAnObject),
            (
                b"{\"name\":\"\xff\"}".to_vec(),
                LineFault::NotUtf8 { at: 9 },
            ),
            (b"{} x".to_vec(), LineFault::Description(String::new())),
        ];
        for (bad, want) in line_rows {
            let mut blob = format!("{good}\n").into_bytes();
            blob.extend_from_slice(&bad);
            blob.push(b'\n');
            let got = mal(decode_scanned(&raw(&b_meta(2), &blob), true));
            match (&got, &want) {
                (
                    ScannedMalformed::Line {
                        index: 1,
                        why: LineFault::Description(_),
                    },
                    LineFault::Description(_),
                ) => {}
                _ => assert_eq!(
                    got,
                    ScannedMalformed::Line {
                        index: 1,
                        why: want
                    },
                    "{bad:?}"
                ),
            }
        }
        // An uppercase classId: the line's `check` refuses it, the error names classId and never the value.
        let blob = format!("{upper}\n");
        let ScannedMalformed::Line {
            index: 0,
            why: LineFault::Description(why),
        } = mal(decode_scanned(&raw(&b_meta(1), blob.as_bytes()), true))
        else {
            panic!("a line fault")
        };
        assert!(
            why.contains("classId") && !why.contains("5341534C"),
            "{why}"
        );
        // The quoted text is bounded: a 60 KiB string of the wrong type is cut to MAX_FAULT_TEXT.
        let blob = format!("{{\"uniqueId\":\"{}\"}}\n", "z".repeat(60_000));
        let ScannedMalformed::Line {
            why: LineFault::Description(why),
            ..
        } = mal(decode_scanned(&raw(&b_meta(1), blob.as_bytes()), true))
        else {
            panic!("a line fault")
        };
        assert!(
            why.len() <= MAX_FAULT_TEXT + '…'.len_utf8(),
            "{}",
            why.len()
        );
        // Not a `scanned` event at all.
        let f = frame(&encode(kind::SHUTDOWN, FF_REPLY, 9, b"{}", &[]));
        assert!(matches!(
            mal(decode_scanned(&f, true)),
            ScannedMalformed::NotScanned { .. }
        ));
    }

    /// W5's order: the framing (7) before the line sizes (8), the sizes of every line before any content (9). A long
    /// line is L4 even when it would not parse, and even when an earlier line is malformed; a child that drops its
    /// newlines is struck at 7, never taken for one long line.
    #[test]
    fn size_comes_before_content_and_framing_before_size() {
        let long_bad = vec![b'x'; MAX_DESCRIPTION_LINE + 1];
        let mut blob = long_bad.clone();
        blob.push(b'\n');
        assert_eq!(
            lim(decode_scanned(&raw(&b_meta(1), &blob), true)),
            ScannedLimit::Line {
                index: 0,
                len: MAX_DESCRIPTION_LINE + 1
            }
        );
        // Line 0 malformed, line 1 too long: still L4 (every size before any content).
        let mut blob = b"[]\n".to_vec();
        blob.extend_from_slice(&long_bad);
        blob.push(b'\n');
        assert_eq!(
            lim(decode_scanned(&raw(&b_meta(2), &blob), true)),
            ScannedLimit::Line {
                index: 1,
                len: MAX_DESCRIPTION_LINE + 1
            }
        );
        // Newlines dropped: 3 descriptions in one 0x0A-terminated run is a count mismatch, not a long line.
        let mut blob = Vec::new();
        for _ in 0..3 {
            blob.extend_from_slice(&serde_json::to_vec(&class(7)).unwrap());
            blob.extend(std::iter::repeat_n(b' ', MAX_DESCRIPTION_LINE / 2));
        }
        blob.push(b'\n');
        assert!(matches!(
            mal(decode_scanned(&raw(&b_meta(3), &blob), true)),
            ScannedMalformed::CountMismatch { .. }
        ));
    }

    /// A line that parses but is not the codec's canonical bytes is refused, a strike
    /// naming the line (`LineFault::NonCanonical`), in every shape; the canonical line itself is accepted. No
    /// non-canonical integer spelling parses (`02` is not JSON; `-0`, `2e0`, `2.0` are floats to serde), so those are
    /// refused one step earlier, by the parse (`Description`). The mutant
    /// `--cfg mutant_scan_line_canon_unchecked` (the compare skipped) must fail here.
    #[test]
    fn a_non_canonical_line_is_refused() {
        let mut d = class(1);
        vst3(&mut d).vendor = "A".into();
        let good = serde_json::to_string(&d).unwrap();
        let first = serde_json::to_string(&class(0)).unwrap();
        let swapped = good.replacen(
            r#""category":"Audio Module Class","name":"fake-scan-class-1""#,
            r#""name":"fake-scan-class-1","category":"Audio Module Class""#,
            1,
        );
        let shapes: [(&str, String); 9] = [
            ("trailing space", format!("{good} ")),
            ("trailing tab", format!("{good}\t")),
            ("a space after a colon", good.replacen("\":", "\": ", 1)),
            ("a space after a comma", good.replacen(",", ", ", 1)),
            ("key order", swapped),
            (
                "an escape the encoder does not write",
                good.replacen(r#""vendor":"A""#, r#""vendor":"\u0041""#, 1),
            ),
            (
                "an explicit null classVendor",
                good.replacen('}', r#","classVendor":null}"#, 1),
            ),
            (
                "an absent defaulted field",
                good.replacen(r#""bundleMtimeMs":0,"#, "", 1),
            ),
            (
                "an explicit null source",
                good.replacen(r#""source":"factory""#, r#""source":null"#, 1),
            ),
        ];
        let accepted = format!("{first}\n{good}\n").into_bytes();
        let f = raw(&b_meta(2), &accepted);
        assert!(decode_scanned(&f, true).is_ok(), "the canonical line");
        for (what, line) in shapes {
            assert!(
                serde_json::from_str::<ScanClass>(&line).is_ok(),
                "{what}: serde itself takes it: {line}"
            );
            let blob = format!("{first}\n{line}\n");
            let got = mal(decode_scanned(&raw(&b_meta(2), blob.as_bytes()), true));
            assert!(
                matches!(
                    got,
                    ScannedMalformed::Line {
                        index: 1,
                        why: LineFault::NonCanonical { .. }
                    }
                ),
                "{what}: {got:?}"
            );
        }
        // The offset names where the re-encode first differs.
        let blob = format!("{good} \n");
        assert_eq!(
            mal(decode_scanned(&raw(&b_meta(1), blob.as_bytes()), true)),
            ScannedMalformed::Line {
                index: 0,
                why: LineFault::NonCanonical { at: good.len() }
            }
        );
        // No non-canonical spelling of an integer parses: a leading zero is not JSON, and `-0`, `1e0` and `1.0` are
        // floats to serde, refused for the integer fields. So the parse refuses them first (`Description`).
        for (from, to) in [
            (r#""inputs":2"#, r#""inputs":02"#),
            (r#""bundleMtimeMs":0"#, r#""bundleMtimeMs":-0"#),
            (r#""inputs":2"#, r#""inputs":2e0"#),
            (r#""inputs":2"#, r#""inputs":2.0"#),
        ] {
            let line = good.replacen(from, to, 1);
            assert_ne!(line, good, "{to}");
            let blob = format!("{line}\n");
            assert!(
                matches!(
                    mal(decode_scanned(&raw(&b_meta(1), blob.as_bytes()), true)),
                    ScannedMalformed::Line {
                        index: 0,
                        why: LineFault::Description(_)
                    }
                ),
                "{to}"
            );
        }
    }

    fn line_of(len: usize) -> ScanClass {
        // The smallest class, its name padded to exactly `len` bytes of compact JSON.
        let base = serde_json::to_vec(&default_class()).unwrap().len();
        let mut c = default_class();
        vst3(&mut c).name = "n".repeat(len - base);
        c
    }

    /// W3: a line of exactly 64 KiB is accepted, one byte more is L4, on the decode; the encoder writes the first
    /// whole and reports the second as `overLimit: "line"` (How tested 4's boundary).
    #[test]
    fn a_line_of_64_kib_is_accepted_and_one_byte_more_is_l4() {
        let at = line_of(MAX_DESCRIPTION_LINE);
        assert_eq!(serde_json::to_vec(&at).unwrap().len(), MAX_DESCRIPTION_LINE);
        let s = scanned("/x/a.vst3", ScannedOutcome::Ok, vec![class(0), at.clone()]);
        let f = frame(&whole(encode_scanned(&s, true)));
        let ScannedLine::B(b) = decode_scanned(&f, true).unwrap() else {
            panic!("form B")
        };
        assert_eq!(b.descriptions[1], at);
        let past = line_of(MAX_DESCRIPTION_LINE + 1);
        let blob = jsonl(&[class(0), past]);
        assert_eq!(
            lim(decode_scanned(&raw(&b_meta(2), &blob), true)),
            ScannedLimit::Line {
                index: 1,
                len: MAX_DESCRIPTION_LINE + 1
            }
        );
    }

    /// W3: 16,384 descriptions are accepted; 16,385 is L3 from the count alone (before the framing, before anything is
    /// allocated): with the lines, and with none.
    #[test]
    fn the_count_bound_is_16384_and_one_more_is_l3() {
        let blob = format!("{}\n", default_line()).repeat(MAX_SCAN_DESCRIPTIONS);
        let d = decode_descriptions_blob(blob.as_bytes(), MAX_SCAN_DESCRIPTIONS as u64).unwrap();
        assert_eq!(d.len(), MAX_SCAN_DESCRIPTIONS);
        let past = (MAX_SCAN_DESCRIPTIONS + 1) as u64;
        let blob = b"{}\n".repeat(MAX_SCAN_DESCRIPTIONS + 1);
        for b in [&blob[..], b""] {
            assert_eq!(
                lim(decode_scanned(&raw(&b_meta(past), b), true)),
                ScannedLimit::Count {
                    description_count: past
                }
            );
        }
        assert_eq!(
            lim(decode_scanned(&raw(&b_meta(u64::MAX), b""), true)),
            ScannedLimit::Count {
                description_count: u64::MAX
            }
        );
        // overLimit comes first (step 5): a well-formed overLimit meta of any count is L2, its word kept.
        let m = r#"{"descriptionCount":7,"descriptionsBlob":1,"format":"vst3","outcome":"ok","overLimit":"newerBound","path":"/x/a.vst3"}"#;
        assert_eq!(
            lim(decode_scanned(&raw(m, b""), true)),
            ScannedLimit::OverLimit {
                word: "newerBound".into(),
                description_count: 7
            }
        );
    }

    /// W3: a blob of exactly 64 MiB (16,384 lines of 4,095 bytes and a newline) is accepted through the host application's
    /// decoder; the encoder writes it whole.
    #[test]
    fn a_blob_of_exactly_64_mib_is_accepted() {
        let d = line_of(4095);
        let s = scanned(
            "/x/a.vst3",
            ScannedOutcome::Ok,
            vec![d.clone(); MAX_SCAN_DESCRIPTIONS],
        );
        let f = frame(&whole(encode_scanned(&s, true)));
        assert_eq!(f.blob.len(), MAX_BLOB);
        let ScannedLine::B(b) = decode_scanned(&f, true).unwrap() else {
            panic!("form B")
        };
        assert_eq!(b.descriptions.len(), MAX_SCAN_DESCRIPTIONS);
        assert!(b.descriptions.iter().all(|x| *x == d));
    }

    fn over_frame(f: ScannedFrame) -> (OverLimit, Frame) {
        match f {
            ScannedFrame::OverLimit { bound, frame: b } => (bound, frame(&b)),
            other => panic!("not over: {}", other_kind(&other)),
        }
    }

    /// W8: past the count the encoder sends the form B meta with the true count, `overLimit: "count"` and no blob.
    #[test]
    fn the_encoder_reports_count_past_16384() {
        let at = scanned(
            "/x/a.vst3",
            ScannedOutcome::Ok,
            classes(MAX_SCAN_DESCRIPTIONS),
        );
        assert!(matches!(encode_scanned(&at, true), ScannedFrame::Whole(_)));
        let s = scanned(
            "/x/a.vst3",
            ScannedOutcome::Ok,
            classes(MAX_SCAN_DESCRIPTIONS + 1),
        );
        let (bound, f) = over_frame(encode_scanned(&s, true));
        assert_eq!(bound, OverLimit::Count);
        assert!(f.blob.is_empty());
        assert_eq!(
            lim(decode_scanned(&f, true)),
            ScannedLimit::OverLimit {
                word: "count".into(),
                description_count: MAX_SCAN_DESCRIPTIONS as u64 + 1
            }
        );
    }

    /// W8: a line past 64 KiB is `overLimit: "line"`, with the true count (and a failed verdict's outcome kept).
    #[test]
    fn the_encoder_reports_a_line_past_64_kib() {
        let s = scanned(
            "/x/a.vst3",
            failed(ScanFailure::Instantiate, "x"),
            vec![class(0), line_of(MAX_DESCRIPTION_LINE + 1), class(2)],
        );
        let (bound, f) = over_frame(encode_scanned(&s, true));
        assert_eq!(bound, OverLimit::Line);
        assert!(f.blob.is_empty());
        let ScannedMeta::B(m) = parse_scanned_meta(&f.meta).unwrap() else {
            panic!("form B")
        };
        assert_eq!(
            (m.description_count, m.over_limit.as_deref(), &m.outcome),
            (3, Some("line"), &s.outcome)
        );
        assert!(
            lim(decode_scanned(&f, true))
                == ScannedLimit::OverLimit {
                    word: "line".into(),
                    description_count: 3
                }
        );
    }

    /// W8: a blob past 64 MiB (16,384 lines of 4,096 bytes each with its newline is one byte per line over) is
    /// `overLimit: "blob"`.
    #[test]
    fn the_encoder_reports_a_blob_past_64_mib() {
        let s = scanned(
            "/x/a.vst3",
            ScannedOutcome::Ok,
            vec![line_of(4096); MAX_SCAN_DESCRIPTIONS],
        );
        let (bound, f) = over_frame(encode_scanned(&s, true));
        assert_eq!(bound, OverLimit::Blob);
        assert_eq!(
            lim(decode_scanned(&f, true)),
            ScannedLimit::OverLimit {
                word: "blob".into(),
                description_count: MAX_SCAN_DESCRIPTIONS as u64
            }
        );
    }

    /// W5's L1: a meta past MAX_META is never a panic; the encoder sends only the 24-byte header with the
    /// true lengths (How tested 7 for host: 2,000 classes not offered), which the host application's decoder refuses for its size
    /// alone and names with `oversized_header`. A form B meta past it (a huge `detail`) is the same path.
    #[test]
    fn a_meta_past_64_kib_is_the_header_only_never_a_panic() {
        let s = scanned("/x/a.vst3", ScannedOutcome::Ok, classes(2000));
        let true_len = serde_json::to_vec(&s).unwrap().len();
        assert!(true_len > MAX_META);
        let ScannedFrame::HeaderOnly {
            header,
            meta_len,
            blob_len,
        } = encode_scanned(&s, false)
        else {
            panic!("header only")
        };
        assert_eq!((meta_len, blob_len), (true_len, 0));
        assert_eq!(encode_scanned(&s, false).bytes(), &header[..]);
        let mut d = Decoder::new(Direction::ToCore);
        assert_eq!(
            d.feed(&header, &mut |_| panic!("no frame")),
            Err(ProtoError::MetaTooLong(true_len as u32))
        );
        let h = d.oversized_header().expect("L1");
        assert_eq!(
            (h.kind, h.flags, h.id, h.meta_len, h.blob_len),
            (kind::SCANNED, FF_EVENT, 0, true_len as u32, 0)
        );
        // The same report, offered: form B, whole, under 1 KiB of meta.
        let f = frame(&whole(encode_scanned(&s, true)));
        assert!(f.meta.len() < 1024);
        // A form B meta past MAX_META.
        let big = scanned(
            "/x/a.vst3",
            failed(ScanFailure::Process, &"d".repeat(MAX_META)),
            classes(1),
        );
        let ScannedFrame::HeaderOnly { header, .. } = encode_scanned(&big, true) else {
            panic!("header only")
        };
        let mut d = Decoder::new(Direction::ToCore);
        assert!(d.feed(&header, &mut |_| {}).is_err());
        assert!(d.oversized_header().is_some());
        // And its overLimit form too (count passed AND a meta past the cap).
        let big = scanned(
            "/x/a.vst3",
            failed(ScanFailure::Process, &"d".repeat(MAX_META)),
            classes(MAX_SCAN_DESCRIPTIONS + 1),
        );
        assert!(matches!(
            encode_scanned(&big, true),
            ScannedFrame::HeaderOnly { .. }
        ));
    }

    /// A form B meta past MAX_META (here a `failed` whose `detail` is past it) is the
    /// 24-byte header only, naming the meta's true length and a blob of 0, even with classes to carry: the blob part is
    /// never built. The host application's decoder refuses it for its meta size alone, and `oversized_header` names exactly that.
    #[test]
    fn a_form_b_meta_past_64_kib_is_the_header_only_with_blob_0() {
        let s = scanned(
            "/x/a.vst3",
            failed(ScanFailure::Process, &"d".repeat(MAX_META)),
            classes(3),
        );
        let true_len = FormBMeta {
            format: s.format,
            path: s.path.clone(),
            outcome: s.outcome.clone(),
            description_count: 3,
            over_limit: None,
        }
        .to_bytes()
        .unwrap()
        .len();
        assert!(true_len > MAX_META);
        let ScannedFrame::HeaderOnly {
            header,
            meta_len,
            blob_len,
        } = encode_scanned(&s, true)
        else {
            panic!("header only")
        };
        assert_eq!((meta_len, blob_len), (true_len, 0));
        assert_eq!(
            header,
            encode_header_only(kind::SCANNED, FF_EVENT, 0, true_len, 0)
        );
        let mut d = Decoder::new(Direction::ToCore);
        assert_eq!(
            d.feed(&header, &mut |_| panic!("no frame")),
            Err(ProtoError::MetaTooLong(true_len as u32))
        );
        let h = d.oversized_header().expect("L1");
        assert_eq!(
            (h.kind, h.flags, h.id, h.meta_len, h.blob_len),
            (kind::SCANNED, FF_EVENT, 0, true_len as u32, 0)
        );
    }

    fn head(
        kind: u16,
        flags: u16,
        id: u64,
        meta: u32,
        blob: u32,
        frame: u32,
    ) -> [u8; FRAME_HEADER] {
        let mut b = header_bytes(kind, flags, id, 0, 0);
        b[..4].copy_from_slice(&frame.to_le_bytes());
        b[F_META_LEN..F_META_LEN + 4].copy_from_slice(&meta.to_le_bytes());
        b[F_BLOB_LEN..F_BLOB_LEN + 4].copy_from_slice(&blob.to_le_bytes());
        b
    }

    /// W5 step 1 (How tested 3's codec unit test): `oversized_header` names the header for the meta cap alone, the blob
    /// cap alone, and both; None when any other check fails too, for any other refusal, and before a whole header.
    #[test]
    fn the_header_is_l1_for_either_cap_alone_and_never_with_another_fault() {
        let (bm, bb) = (MAX_META as u32 + 1, MAX_BLOB as u32 + 1);
        let read = |b: &[u8]| {
            let mut d = Decoder::new(Direction::ToCore);
            let r = d.feed(b, &mut |_| panic!("no frame"));
            (r, d.oversized_header(), d.oversized_meta_header())
        };
        for (what, meta, blob, err) in [
            ("the meta cap", bm, 0, ProtoError::MetaTooLong(bm)),
            ("the blob cap", 2, bb, ProtoError::BlobTooLong(bb)),
            ("both caps", bm, bb, ProtoError::MetaTooLong(bm)),
        ] {
            let b = encode_header_only(kind::SCANNED, FF_EVENT, 0, meta as usize, blob as usize);
            let (r, h, _) = read(&b);
            assert_eq!(r, Err(err), "{what}");
            let h: Header = h.unwrap_or_else(|| panic!("{what}: named"));
            assert_eq!(
                (h.kind, h.flags, h.id, h.meta_len, h.blob_len),
                (kind::SCANNED, FF_EVENT, 0, meta, blob),
                "{what}"
            );
        }
        // The meta-only classifier (as built) still names only the meta cap.
        let (_, _, m) = read(&encode_header_only(
            kind::SCANNED,
            FF_EVENT,
            0,
            2,
            bb as usize,
        ));
        assert_eq!(m, None);
        // Oversized and a fault besides: not named.
        for (what, b) in [
            ("a reply", head(kind::SCANNED, FF_REPLY, 2, 2, bb, 22 + bb)),
            (
                "an event id",
                head(kind::SCANNED, FF_EVENT, 7, 2, bb, 22 + bb),
            ),
            (
                "the length identity",
                head(kind::SCANNED, FF_EVENT, 0, 2, bb, 22),
            ),
            ("an unknown kind", head(999, FF_EVENT, 0, 2, bb, 22 + bb)),
            (
                "unknown flags",
                head(kind::SCANNED, FF_EVENT | 0x100, 0, 2, bb, 22 + bb),
            ),
            (
                "both, and a reply",
                head(kind::SCANNED, FF_REPLY, 2, bm, bb, 20 + bm + bb),
            ),
        ] {
            let (r, h, _) = read(&b);
            assert!(
                matches!(
                    r,
                    Err(ProtoError::BlobTooLong(_) | ProtoError::MetaTooLong(_))
                ),
                "{what}: {r:?}"
            );
            assert_eq!(h, None, "{what}");
        }
        // Another refusal, a refused admission, or none: not named.
        let (r, h, _) = read(&head(kind::SCANNED, FF_REPLY, 2, 2, 0, 22));
        assert!(matches!(r, Err(ProtoError::BadFlags { .. })));
        assert_eq!(h, None);
        let mut d = Decoder::new(Direction::ToCore);
        let r = d.feed_admitting(
            &encode_header_only(kind::SCANNED, FF_EVENT, 0, 2, 4096),
            &mut |_| false,
            &mut |_| {},
        );
        assert!(matches!(r, Err(ProtoError::NotAdmitted { .. })));
        assert_eq!(d.oversized_header(), None);
        let mut d = Decoder::new(Direction::ToCore);
        assert_eq!(d.oversized_header(), None);
        d.feed(
            &encode_header_only(kind::SCANNED, FF_EVENT, 0, 2, bb as usize)[..10],
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(d.oversized_header(), None, "a header not yet whole");
        // Towards the child the same rule holds, with that direction's flags.
        let mut d = Decoder::new(Direction::ToChild);
        let _ = d.feed(
            &encode_header_only(kind::SCAN, 0, 3, 2, bb as usize),
            &mut |_| {},
        );
        assert!(d.oversized_header().is_some());
    }

    /// The header-only helper keeps the length identity, clamping lengths `u32` cannot hold together (still past the caps).
    #[test]
    fn the_header_only_helper_keeps_the_identity() {
        for (m, b) in [
            (MAX_META + 1, 0),
            (0, MAX_BLOB + 1),
            (usize::MAX, usize::MAX),
            (u32::MAX as usize, 5),
        ] {
            let h = encode_header_only(kind::SCANNED, FF_EVENT, 0, m, b);
            let r = |o: usize| u32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]) as u64;
            assert_eq!(r(0), 20 + r(F_META_LEN) + r(F_BLOB_LEN), "{m} {b}");
            assert!(r(F_META_LEN) as usize > MAX_META || r(F_BLOB_LEN) as usize > MAX_BLOB);
            assert!(matches!(
                decode_header(&h, Direction::ToCore),
                Err(ProtoError::MetaTooLong(_) | ProtoError::BlobTooLong(_))
            ));
        }
    }

    /// W4 (How tested 5's codec half): a form B `failed` carries its discovered classes beside the same verdict a line
    /// without them gives; form A, and a count of 0, carry none (fail-closed).
    #[test]
    fn a_failed_form_b_carries_its_discovered_classes_and_the_same_verdict() {
        let out = failed(ScanFailure::State, "round trip");
        let verdict = ScanResult::Failed {
            failure: ScanFailure::State,
            detail: "round trip".into(),
        };
        let b = frame(&whole(encode_scanned(
            &scanned("/x/a.vst3", out.clone(), classes(3)),
            true,
        )));
        let (r, disc) = decode_scanned(&b, true).unwrap().into_verdict();
        assert_eq!((r, disc), (verdict.clone(), classes(3)));
        let none = frame(&whole(encode_scanned(
            &scanned("/x/a.vst3", out.clone(), vec![]),
            true,
        )));
        assert_eq!(
            decode_scanned(&none, true).unwrap().into_verdict(),
            (verdict.clone(), vec![])
        );
        let a = frame(&whole(encode_scanned(
            &scanned("/x/a.vst3", out, classes(3)),
            false,
        )));
        assert_eq!(
            decode_scanned(&a, true).unwrap().into_verdict(),
            (verdict, vec![])
        );
    }

    // ---- The blob format's label is prose, never on the wire -----------------------------------------------------

    /// The frames T1 and T2 read: the offered `scan`, form B `scanned` with 0, 1 and 3 classes, a form B `failed` with
    /// its discovered classes, a form B past the count bound, the L1 header-only frame, and a form A `scanned`.
    fn label_frames() -> Vec<(&'static str, Vec<u8>)> {
        let scan = Scan::new(ScanFormat::Vst3, "/x/shell.vst3")
            .unwrap()
            .offer_descriptions_blob();
        let mut out = vec![(
            "scan_offer",
            crate::codec::encode_msg(kind::SCAN, 0, 7, &scan, &[]),
        )];
        for (name, n) in [("form_b_0", 0), ("form_b_1", 1), ("form_b_3", 3)] {
            let s = scanned("/x/shell.vst3", ScannedOutcome::Ok, classes(n));
            out.push((name, encode_scanned_form_b(&s).bytes().to_vec()));
        }
        let f = scanned(
            "/x/shell.vst3",
            failed(ScanFailure::Process, "threw"),
            classes(2),
        );
        out.push(("form_b_failed", encode_scanned_form_b(&f).bytes().to_vec()));
        let over = scanned(
            "/x/shell.vst3",
            ScannedOutcome::Ok,
            vec![default_class(); MAX_SCAN_DESCRIPTIONS + 1],
        );
        let over = encode_scanned_form_b(&over);
        assert!(matches!(
            over,
            ScannedFrame::OverLimit {
                bound: OverLimit::Count,
                ..
            }
        ));
        out.push(("form_b_over_count", over.bytes().to_vec()));
        let long = format!("/x/{}.vst3", "a".repeat(MAX_META));
        let l1 = encode_scanned_form_b(&scanned(&long, ScannedOutcome::Ok, classes(1)));
        assert!(matches!(l1, ScannedFrame::HeaderOnly { .. }));
        out.push(("form_b_header_only", l1.bytes().to_vec()));
        let a = scanned("/x/shell.vst3", ScannedOutcome::Ok, classes(2));
        out.push(("form_a", encode_scanned_form_a(&a).bytes().to_vec()));
        out
    }

    /// T1: no frame carries the blob format's label or any product prefix, and form B's meta keys are exactly W3's.
    /// Mutant `mutant_scan_form_label_on_wire` (a `descriptionsFormat` key) fails here.
    #[test]
    fn the_scan_wire_names_no_format_label() {
        let product = format!("{}-", "sas");
        let w3 = [
            "descriptionCount",
            "descriptionsBlob",
            "format",
            "outcome",
            "path",
        ];
        let w3_optional = ["detail", "overLimit", "reason"];
        for (name, bytes) in label_frames() {
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.contains("scan-descriptions"), "{name}: {text}");
            assert!(!text.contains(&product), "{name}: {text}");
            if !name.starts_with("form_b") || name == "form_b_header_only" {
                continue;
            }
            let f = frame(&bytes);
            let meta: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&f.meta).unwrap();
            for k in meta.keys() {
                assert!(
                    w3.contains(&k.as_str()) || w3_optional.contains(&k.as_str()),
                    "{name}: the key {k} is not W3's"
                );
            }
            for k in w3 {
                assert!(meta.contains_key(k), "{name}: W3's key {k} is missing");
            }
        }
    }

    /// T2: protocol 4.0's bytes. The frames above, written by the codec at `a9e87fc1` (before the label's rename) and
    /// committed beside this file, one `<name> <hex>` line each: today's encoder reproduces them byte for byte, so a
    /// core and a host on either side of the rename read each other's frames as their own. Mutant
    /// `mutant_scan_form_label_on_wire` fails here.
    #[test]
    fn the_scan_frames_are_protocol_4_0s_bytes() {
        let golden = include_str!("form_b_4_0_frames.txt");
        let want: Vec<(&str, &str)> = golden
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split_once(' ').expect("<name> <hex>"))
            .collect();
        let got = label_frames();
        assert_eq!(want.len(), got.len(), "the golden lists every frame");
        for ((wn, wh), (gn, gb)) in want.iter().zip(&got) {
            assert_eq!(wn, gn);
            let gh: String = gb.iter().map(|b| format!("{b:02x}")).collect();
            assert!(*wh == gh, "{gn}: the bytes differ from protocol 4.0's");
        }
    }
}
