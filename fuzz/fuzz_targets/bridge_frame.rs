//! The control-frame codec on hostile bytes (`bridge_frame`): bytes from a misbehaving plugin child never crash the
//! host. The first byte picks the chunking and the direction; the rest is fed to the streaming decoder in that
//! chunking. Every frame it yields respects the caps (meta <= 64 KiB, blob <= 64 MiB), re-encodes to the same bytes,
//! and goes through the typed meta validation (`check_to_host`, or the request metas the child parses). An error
//! stops the stream, as a protocol error closes the socket. The same stream through the admission gate (the scan
//! reader's blob budget) yields a prefix of those frames within the budget.
//!
//! `scanned` form B: every `scanned` frame decoded also goes through the form B decode (`decode_scanned`, offered and
//! not), with two invariants: no panic, and an accepted form B line's classes re-encode (the codec's own encoder) to
//! exactly `descriptionCount` lines that decode back equal, and (canonical lines only) to the very bytes of the
//! accepted blob when no line carried a field the codec does not know (such fields are dropped, so that line cannot
//! re-encode to its own bytes). A second mode seeds itself: it builds a valid form B frame from the bytes (0 to 100 classes, both
//! class shapes), checks it decodes back whole, then corrupts one byte of it and decodes again under the same
//! invariants. A 64 MiB input is past libFuzzer's practical `-max_len`, so the size boundaries are unit tests.
//!
//! `log.set` metas go through the child's decode and `LogSet::check`, and every meta through `corr_of_meta` and
//! `with_corr` (the cap kept; an added key reads back on a meta that held none).
#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
use summoner_proto::codec::{
    self, Decoder, Direction, Frame, MAX_BLOB, MAX_META, ScanClass, ScanFailure, ScanFormat,
    Scanned, ScannedFrame, ScannedLine, ScannedOutcome, kind,
};
use summoner_proto::plugin::{AuClass, Vst3Class, unknown_scan_class_fields};

/// The form B check on one decoded `scanned` frame: no panic; an accepted form B line re-encodes to exactly
/// `descriptionCount` lines that decode back equal.
fn form_b(f: &Frame) {
    for offered in [true, false] {
        // A class line with fields this codec does not know (a newer child's additive facts) is accepted with those
        // fields dropped, so only a blob of known fields is canonical: the process-wide count of dropped fields says
        // whether this decode dropped any.
        let unknown_before = unknown_scan_class_fields();
        let Ok(line) = codec::decode_scanned(f, offered) else {
            continue;
        };
        let had_unknown = unknown_scan_class_fields() != unknown_before;
        let ScannedLine::B(b) = line else {
            continue;
        };
        assert!(offered, "form B is never accepted without the offer");
        let Ok(codec::ScannedMeta::B(m)) = codec::parse_scanned_meta(&f.meta) else {
            panic!("an accepted form B line has a form B meta");
        };
        assert_eq!(m.description_count, b.descriptions.len() as u64);
        let again = Scanned::new(
            b.format,
            b.path.clone(),
            b.outcome.clone(),
            b.descriptions.clone(),
        );
        let ScannedFrame::Whole(bytes) = codec::encode_scanned_form_b(&again) else {
            // Only the meta can re-encode differently (it keeps the protocol's additive rule); never past its cap in
            // practice, and if it were, that is the encoder's header-only frame, never a panic.
            continue;
        };
        let mut d = Decoder::new(Direction::ToHost);
        let mut back = Vec::new();
        d.feed(&bytes, &mut |g| back.push(g))
            .expect("the encoder's frame decodes");
        assert_eq!(back.len(), 1);
        assert_eq!(
            back[0].blob.iter().filter(|x| **x == b'\n').count() as u64,
            m.description_count
        );
        // An accepted blob of known fields is canonical, so its re-encode is the same bytes.
        if !had_unknown {
            assert_eq!(
                back[0].blob, f.blob,
                "an accepted blob is its own re-encode"
            );
        }
        match codec::decode_scanned(&back[0], true) {
            Ok(ScannedLine::B(b2)) => assert_eq!(b2.descriptions, b.descriptions),
            other => panic!("the re-encoded line decodes back: {other:?}"),
        }
    }
}

/// The self-seeding mode: a valid form B frame built from the bytes, then one byte of it corrupted.
fn form_b_built(first: u8, rest: &[u8]) {
    let Some((&n, body)) = rest.split_first() else {
        return;
    };
    let n = n as usize % 101;
    let piece = |i: usize| {
        let c = body.chunks(7).nth(i).unwrap_or(&[]);
        String::from_utf8_lossy(c).into_owned()
    };
    // Both class shapes (the structs are non_exhaustive: built from their defaults, then their fields set), with the
    // optional facts present on some and absent on others.
    let path = "/x/fuzz.vst3";
    let classes: Vec<ScanClass> = (0..n)
        .map(|i| {
            if i % 4 == 3 {
                let mut a = AuClass::default();
                a.path = path.into();
                a.au_codes = format!("{:08x}-{:08x}-{:08x}", i, i * 31, i * 7919);
                a.component_name = piece(i);
                a.entry = (i % 2 == 0).then_some(i as u32);
                a.au_version = Some(i as u32);
                a.outputs = (i % 3 == 0).then_some(2);
                ScanClass::Au(a)
            } else {
                let mut v = Vst3Class::default();
                v.path = path.into();
                v.bundle_mtime_ms = i as i64;
                v.class_id = format!("{:032x}", i * 7919);
                v.category = "Audio Module Class".into();
                v.name = piece(i);
                v.categories = vec![piece(i + 1)];
                v.vendor = piece(i + 2);
                v.class_vendor = (i % 2 == 0).then(|| piece(i + 3));
                v.version = "1.0.0".into();
                v.sdk_version = "VST 3.7".into();
                v.instantiated = (i % 3 == 0).then_some(true);
                v.inputs = (i % 3 == 0).then_some(2);
                ScanClass::Vst3(v)
            }
        })
        .collect();
    let outcome = if first & 1 == 0 {
        ScannedOutcome::Ok
    } else {
        ScannedOutcome::Failed {
            failure: ScanFailure::Process,
            detail: piece(0),
        }
    };
    let s = Scanned::new(ScanFormat::Vst3, path.into(), outcome, classes);
    let ScannedFrame::Whole(mut bytes) = codec::encode_scanned_form_b(&s) else {
        panic!("100 short classes are within every bound");
    };
    let mut d = Decoder::new(Direction::ToHost);
    let mut fs = Vec::new();
    d.feed(&bytes, &mut |g| fs.push(g))
        .expect("the encoder's frame decodes");
    match codec::decode_scanned(&fs[0], true) {
        Ok(ScannedLine::B(b)) => assert_eq!(b.descriptions, s.classes),
        other => panic!("the encoder's line decodes back: {other:?}"),
    }
    form_b(&fs[0]);
    // One byte corrupted past the header (so the frame still frames), at a place and with a value from the bytes.
    let tail = body.len().saturating_sub(4);
    let (at, v) = match body.get(tail..) {
        Some([a, b, c, v]) => (u32::from_le_bytes([*a, *b, *c, 0]) as usize, *v),
        _ => return,
    };
    let span = bytes.len() - 24;
    if span == 0 {
        return;
    }
    bytes[24 + at % span] ^= v | 1;
    let mut d = Decoder::new(Direction::ToHost);
    let mut fs = Vec::new();
    if d.feed(&bytes, &mut |g| fs.push(g)).is_ok() {
        for f in &fs {
            form_b(f);
        }
    }
}

fn to_child(f: &Frame) {
    let _ = match f.header.kind {
        kind::HELLO => f.meta_as::<codec::Hello>().map(|_| ()),
        kind::CHAIN_LOAD => f.meta_as::<codec::ChainLoad>().map(|_| ()),
        kind::CHAIN_UNPARK => f.meta_as::<codec::ChainUnpark>().map(|_| ()),
        kind::STATE_SET => f.meta_as::<codec::StateSet>().map(|_| ()),
        kind::STATE_GET => f.meta_as::<codec::StateGet>().map(|_| ()),
        kind::PARAM_LIST => f.meta_as::<codec::ParamList>().map(|_| ()),
        kind::WORKGROUP_ATTACH => f.meta_as::<codec::WorkgroupAttach>().map(|_| ()),
        // The child's decode, then its checks (never a panic).
        kind::LOG_SET => f.meta_as::<codec::LogSet>().map(|m| {
            let _ = m.check();
        }),
        _ => f.meta_as::<codec::ChainRef>().map(|_| ()),
    };
}

/// The correlation helpers on every decoded meta: the lenient reader never panics, and a key the splice adds stays
/// within the cap and reads back.
fn corr(f: &Frame) {
    let _ = codec::corr_of_meta(&f.meta);
    let mut m = f.meta.clone();
    if codec::with_corr(&mut m, "r:7").is_ok() {
        assert!(m.len() <= MAX_META, "the splice keeps the cap");
        if serde_json::from_slice::<serde_json::Value>(&f.meta)
            .is_ok_and(|v| v.get("corr").is_none())
        {
            assert_eq!(
                codec::corr_of_meta(&m),
                codec::CorrRead::Valid("r:7".into())
            );
        }
    } else {
        assert_eq!(m, f.meta, "a refusal leaves the bytes");
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let dir = if first & 0x80 != 0 {
        Direction::ToChild
    } else {
        Direction::ToHost
    };
    let chunk = 1 + (first & 0x7f) as usize * 3;
    let mut d = Decoder::new(dir);
    let mut frames = Vec::new();
    for c in rest.chunks(chunk) {
        if d.feed(c, &mut |f| frames.push(f)).is_err() {
            break;
        }
    }
    // The admission gate (the scan reader's blob budget), with a budget taken from the first byte: the same stream in the same chunking yields a prefix of the ungated frames, their blobs sum to
    // at most the budget, and a refusal is `NotAdmitted` for a header whose blob would pass it.
    let budget = u64::from(first & 0x7f) * 64;
    let mut g = Decoder::new(dir);
    let (mut gated, mut admitted, mut refused) = (Vec::new(), 0u64, None);
    for c in rest.chunks(chunk) {
        let r = g.feed_admitting(
            c,
            &mut |h| {
                let after = admitted + u64::from(h.blob_len);
                if after > budget && h.blob_len > 0 {
                    refused = Some(*h);
                    return false;
                }
                admitted = after;
                true
            },
            &mut |f| gated.push(f),
        );
        if let Err(e) = r {
            if let Some(h) = refused {
                assert_eq!(
                    e,
                    codec::ProtoError::NotAdmitted {
                        kind: h.kind,
                        blob_len: h.blob_len
                    }
                );
            }
            break;
        }
    }
    assert!(gated.len() <= frames.len() && gated[..] == frames[..gated.len()]);
    assert!(gated.iter().map(|f| f.blob.len() as u64).sum::<u64>() <= budget);
    for f in frames {
        assert!(f.meta.len() <= MAX_META && f.blob.len() <= MAX_BLOB);
        assert_eq!(f.meta.len(), f.header.meta_len as usize);
        assert_eq!(f.blob.len(), f.header.blob_len as usize);
        let again = codec::encode(f.header.kind, f.header.flags, f.header.id, &f.meta, &f.blob);
        let mut d2 = Decoder::new(dir);
        let mut back = Vec::new();
        d2.feed(&again, &mut |g| back.push(g))
            .expect("a decoded frame re-encodes to a valid frame");
        assert_eq!(back, vec![f.clone()]);
        corr(&f);
        match dir {
            Direction::ToHost => {
                let _ = codec::check_to_host(&f);
                if f.header.kind == kind::SCANNED {
                    form_b(&f);
                }
            }
            Direction::ToChild => to_child(&f),
        }
    }
    form_b_built(first, rest);
});
