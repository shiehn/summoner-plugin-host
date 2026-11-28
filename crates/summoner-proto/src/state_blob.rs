//! State blob format v1 (magic `53 41 53 50`, four ASCII capitals), the one canonical framing of a plugin state blob
//! the bytes a `plugin-state/<sha256>.bin` holds, what `state.set {mode: sasp}` carries and what every
//! `state.get` and `editor.closed` returns.
//!
//! Little-endian, every byte fixed by the content:
//!
//! | Off | Size | Field | Value |
//! |---|---|---|---|
//! | 0 | 4 | magic | [`MAGIC`] (`53 41 53 50`) |
//! | 4 | 1 | version | 1 |
//! | 5 | 1 | format | 1 vst3, 2 au, 3 builtin |
//! | 6 | 1 | present | vst3: bit 0 component, bit 1 controller; au, builtin: 0 |
//! | 7 | 1 | reserved | 0 |
//! | 8 | 4 | len₀ | chunk₀'s length |
//! | 12 | len₀ | chunk₀ | the component chunk, the plist, or the builtin body |
//! | 12 + len₀ | 4 | len₁ | vst3 only: the controller chunk's length |
//! | 16 + len₀ | len₁ | chunk₁ | vst3 only |
//!
//! [`decode`] accepts exactly one form and refuses everything else with a named [`Refusal`]; [`encode`] is its
//! inverse (`decode(encode(x)) == x`, and `encode(decode(b)) == b` for every accepted `b`). [`check`] is the host application's
//! frame check (N9: length ≤ 64 MiB, this check, sha256, store, forward; it never reads inside a chunk, apart from an
//! au chunk's 8-byte `bplist00` magic). Fuzzed by the host application's state-blob fuzz target.
//!
//! Safe, pure, and allocation-free except [`encode`] (the child's main thread and the tests).
#![allow(clippy::disallowed_methods)] // encode builds a Vec: never on a realtime path

use crate::codec::MAX_BLOB;

/// Historical format magic for framed plugin-state blobs (`53 41 53 50`); part of the stored format, not a brand.
pub const MAGIC: [u8; 4] = *b"SASP";
/// The format version this module reads and writes.
pub const VERSION: u8 = 1;
/// The format byte of a VST3 blob (a component and a controller chunk).
pub const FORMAT_VST3: u8 = 1;
/// The format byte of an Audio Unit blob (one binary plist chunk).
pub const FORMAT_AU: u8 = 2;
/// The format byte of a built-in node's blob (one opaque body).
pub const FORMAT_BUILTIN: u8 = 3;
/// vst3 `present` bit 0: the component chunk (`IComponent::getState` returned `kResultTrue`).
pub const PRESENT_COMPONENT: u8 = 1 << 0;
/// vst3 `present` bit 1: the controller chunk (`IEditController::getState` returned `kResultTrue`).
pub const PRESENT_CONTROLLER: u8 = 1 << 1;
/// magic, version, format, present, reserved, len₀.
pub const HEADER: usize = 12;
/// An au chunk₀ is a binary plist (`kCFPropertyListBinaryFormat_v1_0`): refusal 8's magic.
pub const BPLIST: &[u8; 8] = b"bplist00";

/// The three formats of the state blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// A VST3 plugin's state: its component and controller chunks.
    Vst3,
    /// An Audio Unit's state: one binary plist.
    Au,
    /// A built-in node's state: one opaque body.
    Builtin,
}

impl Format {
    /// The format byte written in the header.
    pub fn code(self) -> u8 {
        match self {
            Format::Vst3 => FORMAT_VST3,
            Format::Au => FORMAT_AU,
            Format::Builtin => FORMAT_BUILTIN,
        }
    }

    /// The format's lowercase name (`vst3`, `au`, `builtin`).
    pub fn name(self) -> &'static str {
        match self {
            Format::Vst3 => "vst3",
            Format::Au => "au",
            Format::Builtin => "builtin",
        }
    }
}

/// Why a blob is refused: the format's eight refusals, each by name (refusal 4 has three faces, 1 two).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// 1: fewer than 12 bytes.
    Short(usize),
    /// 1: a magic other than [`MAGIC`].
    Magic,
    /// 2: a version other than 1.
    Version(u8),
    /// 3: an unknown format (0, or > 3).
    Format(u8),
    /// 4: a `present` bit undefined for the format.
    Present(u8),
    /// 4: the reserved byte is not 0.
    Reserved(u8),
    /// 4: an absent chunk (its bit clear) whose length is not 0.
    AbsentLength,
    /// 5: a length that runs past the end (the sums in u64, so nothing wraps).
    Overrun,
    /// 6: bytes after the last chunk.
    Trailing(usize),
    /// 7: a total over `MAX_BLOB` (64 MiB, the frame's blob cap).
    TooLarge(usize),
    /// 8: an au chunk₀ that does not begin with `bplist00`.
    NotBplist,
}

impl Refusal {
    /// The refusal's name, as logs and an application's open warning carry it.
    pub fn name(self) -> &'static str {
        match self {
            Refusal::Short(_) => "short",
            Refusal::Magic => "magic",
            Refusal::Version(_) => "version",
            Refusal::Format(_) => "format",
            Refusal::Present(_) => "present",
            Refusal::Reserved(_) => "reserved",
            Refusal::AbsentLength => "absentLength",
            Refusal::Overrun => "overrun",
            Refusal::Trailing(_) => "trailing",
            Refusal::TooLarge(_) => "tooLarge",
            Refusal::NotBplist => "notBplist",
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state blob refused ({}): {self:?}", self.name())
    }
}

impl std::error::Error for Refusal {}

/// A decoded blob, borrowing its chunks. vst3: `None` is an absent chunk, `Some(&[])` an empty one (they differ: a
/// set calls `setState` for a present chunk, an empty one included, and skips an absent one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Blob<'a> {
    /// A VST3 blob.
    Vst3 {
        /// The component chunk (`None`: absent).
        component: Option<&'a [u8]>,
        /// The controller chunk (`None`: absent).
        controller: Option<&'a [u8]>,
    },
    /// An Audio Unit blob.
    Au {
        /// The binary plist chunk.
        plist: &'a [u8],
    },
    /// A built-in node's blob.
    Builtin {
        /// The opaque body.
        body: &'a [u8],
    },
}

impl Blob<'_> {
    /// The blob's format.
    pub fn format(&self) -> Format {
        match self {
            Blob::Vst3 { .. } => Format::Vst3,
            Blob::Au { .. } => Format::Au,
            Blob::Builtin { .. } => Format::Builtin,
        }
    }

    /// The both-absent vst3 blob: accepted, and a set of it is a counted no-op (the host counts empty sets).
    pub fn both_absent(&self) -> bool {
        matches!(
            self,
            Blob::Vst3 {
                component: None,
                controller: None
            }
        )
    }
}

/// What the host application's frame check learns: the format, and whether the blob is the both-absent vst3 no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Checked {
    /// The blob's format.
    pub format: Format,
    /// Whether the blob is the both-absent VST3 no-op.
    pub both_absent: bool,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Decode `b`, accepting exactly the format's one form. The checks run in this order, so a blob with several faults is
/// refused by the first: 7 (the size, before anything is read), 1, 2, 3, 4 (the `present` bits and the reserved
/// byte), 5, 4 (an absent chunk's length, once both lengths are in bounds), 6, 8.
pub fn decode(b: &[u8]) -> Result<Blob<'_>, Refusal> {
    if b.len() > MAX_BLOB {
        return Err(Refusal::TooLarge(b.len()));
    }
    if b.len() < HEADER {
        return Err(Refusal::Short(b.len()));
    }
    if b[0..4] != MAGIC {
        return Err(Refusal::Magic);
    }
    if b[4] != VERSION {
        return Err(Refusal::Version(b[4]));
    }
    let (format, present, reserved) = (b[5], b[6], b[7]);
    let defined = match format {
        FORMAT_VST3 => PRESENT_COMPONENT | PRESENT_CONTROLLER,
        FORMAT_AU | FORMAT_BUILTIN => 0,
        f => return Err(Refusal::Format(f)),
    };
    if present & !defined != 0 {
        return Err(Refusal::Present(present));
    }
    if reserved != 0 {
        return Err(Refusal::Reserved(reserved));
    }
    let len = b.len() as u64;
    let len0 = u64::from(u32_at(b, 8));
    let end0 = HEADER as u64 + len0;
    if end0 > len {
        return Err(Refusal::Overrun);
    }
    let end0 = end0 as usize;
    let chunk0 = &b[HEADER..end0];
    let end = match format {
        FORMAT_VST3 => {
            if end0 as u64 + 4 > len {
                return Err(Refusal::Overrun);
            }
            let len1 = u64::from(u32_at(b, end0));
            let end1 = end0 as u64 + 4 + len1;
            if end1 > len {
                return Err(Refusal::Overrun);
            }
            if (present & PRESENT_COMPONENT == 0 && len0 != 0)
                || (present & PRESENT_CONTROLLER == 0 && len1 != 0)
            {
                return Err(Refusal::AbsentLength);
            }
            end1 as usize
        }
        _ => end0,
    };
    if b.len() > end {
        return Err(Refusal::Trailing(b.len() - end));
    }
    Ok(match format {
        FORMAT_VST3 => Blob::Vst3 {
            component: (present & PRESENT_COMPONENT != 0).then_some(chunk0),
            controller: (present & PRESENT_CONTROLLER != 0).then_some(&b[end0 + 4..end]),
        },
        FORMAT_AU => {
            if !chunk0.starts_with(BPLIST) {
                return Err(Refusal::NotBplist);
            }
            Blob::Au { plist: chunk0 }
        }
        _ => Blob::Builtin { body: chunk0 },
    })
}

/// The host application's frame check (where the format runs): [`decode`]'s verdict, without keeping the chunks.
pub fn check(b: &[u8]) -> Result<Checked, Refusal> {
    decode(b).map(|x| Checked {
        format: x.format(),
        both_absent: x.both_absent(),
    })
}

/// Encode `x`: the inverse of [`decode`]. Refuses (with the refusal [`decode`] would give the result) a blob over
/// `MAX_BLOB` and an au chunk that is not a binary plist, so every `Ok` decodes back to `x`.
pub fn encode(x: &Blob<'_>) -> Result<Vec<u8>, Refusal> {
    let (format, present, c0, c1): (u8, u8, &[u8], Option<&[u8]>) = match *x {
        Blob::Vst3 {
            component,
            controller,
        } => (
            FORMAT_VST3,
            if component.is_some() {
                PRESENT_COMPONENT
            } else {
                0
            } | if controller.is_some() {
                PRESENT_CONTROLLER
            } else {
                0
            },
            component.unwrap_or(&[]),
            Some(controller.unwrap_or(&[])),
        ),
        Blob::Au { plist } => {
            if !plist.starts_with(BPLIST) {
                return Err(Refusal::NotBplist);
            }
            (FORMAT_AU, 0, plist, None)
        }
        Blob::Builtin { body } => (FORMAT_BUILTIN, 0, body, None),
    };
    let total = HEADER as u64 + c0.len() as u64 + c1.map_or(0, |c| 4 + c.len() as u64);
    if total > MAX_BLOB as u64 {
        return Err(Refusal::TooLarge(total.min(usize::MAX as u64) as usize));
    }
    let mut v = Vec::with_capacity(total as usize);
    v.extend_from_slice(&MAGIC);
    v.extend_from_slice(&[VERSION, format, present, 0]);
    v.extend_from_slice(&(c0.len() as u32).to_le_bytes());
    v.extend_from_slice(c0);
    if let Some(c1) = c1 {
        v.extend_from_slice(&(c1.len() as u32).to_le_bytes());
        v.extend_from_slice(c1);
    }
    Ok(v)
}

/// A `{builtin}` blob of `body` (the test doubles' knob strings). Panics only past 64 MiB (the caller's bug).
pub fn builtin(body: &[u8]) -> Vec<u8> {
    encode(&Blob::Builtin { body }).expect("a builtin body under 64 MiB")
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;

    fn vst3(c: Option<&[u8]>, e: Option<&[u8]>) -> Vec<u8> {
        encode(&Blob::Vst3 {
            component: c,
            controller: e,
        })
        .unwrap()
    }

    /// The format kept its bytes through the module's rename: a v1 blob written as bytes (the same bytes
    /// `the_adr_sizes_and_bytes_hold` pins) decodes to the same blob, and re-encodes to itself.
    #[test]
    fn a_v1_blob_from_before_the_rename_decodes_to_the_same_blob() {
        let bytes: &[u8] = b"\x53\x41\x53\x50\x01\x01\x03\x00\x03\x00\x00\x00abc\x02\x00\x00\x00de";
        assert_eq!(MAGIC, [0x53, 0x41, 0x53, 0x50]);
        assert_eq!(VERSION, 1);
        let b = decode(bytes).unwrap();
        assert_eq!(
            b,
            Blob::Vst3 {
                component: Some(b"abc"),
                controller: Some(b"de"),
            }
        );
        assert_eq!(encode(&b).unwrap(), bytes);
    }

    #[test]
    fn the_adr_sizes_and_bytes_hold() {
        // vst3 is exactly 16 + len0 + len1; au and builtin exactly 12 + len0.
        let v = vst3(Some(b"abc"), Some(b"de"));
        assert_eq!(v.len(), 16 + 3 + 2);
        assert_eq!(
            v,
            [
                MAGIC.as_slice(),
                &[1, 1, 3, 0],
                &3u32.to_le_bytes(),
                b"abc",
                &2u32.to_le_bytes(),
                b"de"
            ]
            .concat()
        );
        let b = builtin(b"gain=0.5");
        assert_eq!(b.len(), 12 + 8);
        assert_eq!(&b[..8], &[b'S', b'A', b'S', b'P', 1, 3, 0, 0]);
        let a = encode(&Blob::Au {
            plist: b"bplist00xyz",
        })
        .unwrap();
        assert_eq!(a.len(), 12 + 11);
        assert_eq!(a[5], FORMAT_AU);
    }

    #[test]
    fn both_absent_is_accepted_as_sixteen_bytes() {
        let v = vst3(None, None);
        assert_eq!(v.len(), 16);
        assert_eq!(v[6], 0);
        let c = check(&v).unwrap();
        assert!(c.both_absent && c.format == Format::Vst3);
        assert!(!check(&vst3(Some(b""), None)).unwrap().both_absent);
    }

    #[test]
    fn absent_and_empty_differ() {
        let empty = vst3(Some(b""), Some(b""));
        let absent = vst3(None, None);
        assert_ne!(empty, absent);
        assert_eq!(
            decode(&empty).unwrap(),
            Blob::Vst3 {
                component: Some(b""),
                controller: Some(b"")
            }
        );
        assert_eq!(
            decode(&vst3(None, Some(b"x"))).unwrap(),
            Blob::Vst3 {
                component: None,
                controller: Some(b"x")
            }
        );
    }

    #[test]
    fn round_trips_both_ways() {
        let cases: Vec<Vec<u8>> = vec![
            vst3(Some(b"component"), Some(b"controller")),
            vst3(Some(b""), None),
            vst3(None, Some(&[0u8; 300])),
            vst3(None, None),
            builtin(b""),
            builtin(b"CRASH"),
            encode(&Blob::Au { plist: b"bplist00" }).unwrap(),
        ];
        for b in cases {
            let x = decode(&b).unwrap();
            assert_eq!(encode(&x).unwrap(), b, "encode(decode(b)) == b");
            assert_eq!(decode(&encode(&x).unwrap()).unwrap(), x);
        }
    }

    #[test]
    fn every_refusal_is_named() {
        let good = vst3(Some(b"ab"), Some(b"c"));
        let with = |at: usize, v: u8| {
            let mut b = good.clone();
            b[at] = v;
            b
        };
        assert_eq!(
            decode(b"\x53\x41\x53\x50\x01\x03\x00"),
            Err(Refusal::Short(7))
        );
        assert_eq!(decode(&with(0, b'X')), Err(Refusal::Magic));
        assert_eq!(decode(&with(4, 2)), Err(Refusal::Version(2)));
        assert_eq!(decode(&with(5, 0)), Err(Refusal::Format(0)));
        assert_eq!(decode(&with(5, 4)), Err(Refusal::Format(4)));
        assert_eq!(decode(&with(6, 7)), Err(Refusal::Present(7)));
        let mut au = builtin(b"x");
        au[6] = 1;
        assert_eq!(decode(&au), Err(Refusal::Present(1)));
        assert_eq!(decode(&with(7, 1)), Err(Refusal::Reserved(1)));
        // An absent chunk with a length.
        assert_eq!(
            decode(&with(6, PRESENT_CONTROLLER)),
            Err(Refusal::AbsentLength)
        );
        assert_eq!(
            decode(&with(6, PRESENT_COMPONENT)),
            Err(Refusal::AbsentLength)
        );
        // Lengths past the end, including one that would wrap a u32 sum.
        let mut b = good.clone();
        b[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&b), Err(Refusal::Overrun));
        let mut b = good.clone();
        let at = 12 + 2;
        b[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(&b), Err(Refusal::Overrun));
        assert_eq!(decode(&good[..good.len() - 1]), Err(Refusal::Overrun));
        // A vst3 blob cut before len1.
        assert_eq!(decode(&good[..12 + 2 + 3]), Err(Refusal::Overrun));
        let mut b = good.clone();
        b.push(0);
        assert_eq!(decode(&b), Err(Refusal::Trailing(1)));
        let mut b = builtin(b"x");
        b.extend_from_slice(b"yz");
        assert_eq!(decode(&b), Err(Refusal::Trailing(2)));
        let big = vec![0u8; MAX_BLOB + 1];
        assert_eq!(decode(&big), Err(Refusal::TooLarge(MAX_BLOB + 1)));
        // au: the one content check, a magic.
        let mut a = builtin(b"<?xml version");
        a[5] = FORMAT_AU;
        assert_eq!(decode(&a), Err(Refusal::NotBplist));
        assert_eq!(
            encode(&Blob::Au { plist: b"<?xml" }),
            Err(Refusal::NotBplist)
        );
        // A bare blob (no framing), as the oldest fixtures were.
        assert_eq!(decode(b"gain=0.5;x=1"), Err(Refusal::Magic));
        assert_eq!(decode(b"gain"), Err(Refusal::Short(4)));
    }

    #[test]
    fn the_largest_blob_fits_and_one_more_byte_does_not() {
        let body = vec![7u8; MAX_BLOB - HEADER];
        let b = builtin(&body);
        assert_eq!(b.len(), MAX_BLOB);
        assert!(check(&b).is_ok());
        let over = vec![7u8; MAX_BLOB - HEADER + 1];
        assert_eq!(
            encode(&Blob::Builtin { body: &over }),
            Err(Refusal::TooLarge(MAX_BLOB + 1))
        );
    }
}
