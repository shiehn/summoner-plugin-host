//! The state blob's one parser on hostile bytes.
//! - Any bytes: `check` and `decode` never panic and agree (the same verdict, the same named refusal); an accepted
//!   blob re-encodes to exactly its own bytes (`encode(decode(b)) == b`: one form).
//! - Structured: the first byte picks a shape (vst3 with each present-bit pattern, au with and without `bplist00`,
//!   builtin) over the rest; whatever `encode` accepts decodes back to the same blob (`decode(encode(x)) == x`), and
//!   what it refuses (an au chunk that is not a binary plist) is what `decode` would refuse.
#![no_main]
#![forbid(unsafe_code)]
use libfuzzer_sys::fuzz_target;
use summoner_proto::state_blob::{self, Blob, Refusal};

fuzz_target!(|data: &[u8]| {
    match state_blob::decode(data) {
        Ok(x) => {
            let c = state_blob::check(data).expect("check agrees with decode");
            assert_eq!(c.format, x.format());
            assert_eq!(c.both_absent, x.both_absent());
            assert_eq!(
                state_blob::encode(&x).expect("an accepted blob re-encodes"),
                data
            );
        }
        Err(e) => assert_eq!(state_blob::check(data), Err(e)),
    }
    let Some((&sel, rest)) = data.split_first() else {
        return;
    };
    let (a, b) = rest.split_at(rest.len() / 2);
    let mut plist = state_blob::BPLIST.to_vec();
    plist.extend_from_slice(rest);
    let x = match sel % 7 {
        0 => Blob::Vst3 {
            component: Some(a),
            controller: Some(b),
        },
        1 => Blob::Vst3 {
            component: None,
            controller: Some(b),
        },
        2 => Blob::Vst3 {
            component: Some(a),
            controller: None,
        },
        3 => Blob::Vst3 {
            component: None,
            controller: None,
        },
        4 => Blob::Au { plist: &plist },
        5 => Blob::Au { plist: rest },
        _ => Blob::Builtin { body: rest },
    };
    match state_blob::encode(&x) {
        Ok(v) => {
            assert_eq!(state_blob::decode(&v).expect("an encoded blob decodes"), x);
            let want = match x {
                Blob::Vst3 {
                    component,
                    controller,
                } => 16 + component.map_or(0, <[u8]>::len) + controller.map_or(0, <[u8]>::len),
                Blob::Au { plist } => 12 + plist.len(),
                Blob::Builtin { body } => 12 + body.len(),
                // Blob is non_exhaustive; `x` is one of the three shapes built above.
                _ => unreachable!("x is built as vst3, au or builtin"),
            };
            assert_eq!(
                v.len(),
                want,
                "a vst3 blob is 16 + len0 + len1, the others 12 + len0"
            );
        }
        Err(e) => {
            assert_eq!(e, Refusal::NotBplist);
            assert!(!rest.starts_with(state_blob::BPLIST));
        }
    }
});
