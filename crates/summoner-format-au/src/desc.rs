//! A real Audio Unit slot (protocol 4.0's `PluginRef`): a `.component` bundle by absolute path, registered in-process
//! (never the system registry), and the `AudioComponents` entry whose three codes are the ref's `auCodes`. The
//! name is for display only.
use summoner_sys::au::{Desc, TYPE_MIDI_PROCESSOR, TYPE_MUSIC_DEVICE, TYPE_MUSIC_EFFECT};

/// What a slot names: the bundle, the entry's `[type, subtype, manufacturer]` and the display name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Description {
    /// The display name (never part of the identity).
    pub name: String,
    /// The `.component` bundle's absolute path.
    pub file: String,
    /// The `AudioComponents` entry's `[type, subtype, manufacturer]` codes.
    pub codes: [u32; 3],
}

impl Description {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(name: String, file: String, codes: [u32; 3]) -> Description {
        Description { name, file, codes }
    }
}

/// The names of the three codes, in their order.
pub const CODE_NAMES: [&str; 3] = ["type", "subtype", "manufacturer"];

/// The codes' values (mirrored from the wire's own check): each code non-zero, and each of its four bytes
/// printable ASCII (`0x20`..=`0x7E`) and none of `,` `:` `/`. The error names the code and the byte's offset, never
/// echoes the value.
pub fn check_codes(codes: [u32; 3]) -> Result<(), crate::Error> {
    check_codes_why(codes).map_err(crate::Error::from)
}

fn check_codes_why(codes: [u32; 3]) -> Result<(), String> {
    for (i, v) in codes.iter().enumerate() {
        if *v == 0 {
            return Err(format!(
                "the {} code is zero (a wildcard is never an identity) (F4)",
                CODE_NAMES[i]
            ));
        }
        for (k, b) in v.to_be_bytes().iter().enumerate() {
            if !(0x20..=0x7e).contains(b) || matches!(b, b',' | b':' | b'/') {
                return Err(format!(
                    "the {} code's byte {k} is outside printable ASCII or is one of ',' ':' '/' (F4)",
                    CODE_NAMES[i]
                ));
            }
        }
    }
    Ok(())
}

/// The three codes of a component's description.
pub fn codes_of(d: &Desc) -> [u32; 3] {
    [d.kind, d.sub, d.manu]
}

/// The codes in their wire form, `auCodes`: `<type>-<subtype>-<manufacturer>`, each 8 lowercase hex digits.
pub fn codes_string(d: &Desc) -> String {
    format!("{:08x}-{:08x}-{:08x}", d.kind, d.sub, d.manu)
}

/// The slot a `PluginRef` names, when it is an Audio Unit one (the ref's decode checked the codes).
pub fn of_ref(r: &summoner_proto::plugin::PluginRef) -> Option<Description> {
    let au_codes = r.au_codes()?;
    Some(Description {
        name: r.name().to_string(),
        file: r.path()?.to_string(),
        codes: summoner_proto::codec::check_au_codes(au_codes).ok()?,
    })
}

/// An `AudioComponents` entry's display name: Apple's `"Manufacturer: Name"` convention split at the first `:`, the
/// name part trimmed (`<Unknown>` when empty), and the manufacturer part.
pub fn name_and_manufacturer(full: &str) -> (String, String) {
    let (manu, name) = match full.split_once(':') {
        Some((m, n)) => (m.trim().to_string(), n.trim().to_string()),
        None => (String::new(), full.to_string()),
    };
    (
        if name.is_empty() {
            "<Unknown>".into()
        } else {
            name
        },
        manu,
    )
}

/// Whether the component takes MIDI (music devices, music effects, MIDI processors).
pub fn wants_midi(d: &Desc) -> bool {
    matches!(
        d.kind,
        TYPE_MUSIC_DEVICE | TYPE_MUSIC_EFFECT | TYPE_MIDI_PROCESSOR
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use summoner_sys::au::TYPE_EFFECT;

    #[test]
    fn names_split_at_the_first_colon() {
        assert_eq!(
            name_and_manufacturer("Summoner Lab: Summoner Lab Gain"),
            ("Summoner Lab Gain".into(), "Summoner Lab".into())
        );
        assert_eq!(
            name_and_manufacturer("Plain"),
            ("Plain".into(), String::new())
        );
        assert_eq!(
            name_and_manufacturer("M:"),
            ("<Unknown>".into(), "M".into())
        );
    }

    #[test]
    fn an_au_ref_is_a_slot_and_others_are_not() {
        let r: summoner_proto::plugin::PluginRef = serde_json::from_str(
            r#"{"format":"au","path":"/x/g.component","auCodes":"61756678-534c676e-5361534c","name":"Summoner Lab Gain"}"#,
        )
        .unwrap();
        let d = of_ref(&r).unwrap();
        assert_eq!(
            d.codes,
            [
                TYPE_EFFECT,
                u32::from_be_bytes(*b"SLgn"),
                u32::from_be_bytes(*b"SaSL")
            ]
        );
        assert_eq!(
            (d.name.as_str(), d.file.as_str()),
            ("Summoner Lab Gain", "/x/g.component")
        );
        let v: summoner_proto::plugin::PluginRef = serde_json::from_str(
            r#"{"format":"vst3","path":"/x/g.vst3","classId":"5341534c4c4142010000005000000000","name":"x"}"#,
        )
        .unwrap();
        assert!(of_ref(&v).is_none());
    }

    /// Zero codes, bytes outside 0x20..=0x7E, and the three separators are refused, naming the code
    /// and the byte, never the value.
    #[test]
    fn f4_refuses_zero_codes_unprintable_bytes_and_the_separators() {
        let good = [
            TYPE_EFFECT,
            u32::from_be_bytes(*b"SLgn"),
            u32::from_be_bytes(*b"SaSL"),
        ];
        assert!(check_codes(good).is_ok());
        assert!(
            check_codes([
                TYPE_EFFECT,
                u32::from_be_bytes(*b"SLx`"),
                u32::from_be_bytes(*b"S ~ ")
            ])
            .is_ok()
        );
        for (pos, byte) in [
            (0, 0u8),
            (1, 0x09),
            (2, 0x7f),
            (0, 0x80),
            (1, 0xe9),
            (2, b','),
            (0, b':'),
            (1, b'/'),
        ] {
            let mut c = good;
            c[pos] = (c[pos] & 0xffff_00ff) | (u32::from(byte) << 8);
            let e = check_codes(c).expect_err("refused");
            assert!(
                e.as_str().contains(CODE_NAMES[pos]) && e.as_str().contains("byte 2"),
                "{e}"
            );
        }
        for pos in 0..3 {
            let mut c = good;
            c[pos] = 0;
            assert!(check_codes(c).expect_err("zero").as_str().contains("zero"));
        }
        let d = Desc {
            kind: good[0],
            sub: good[1],
            manu: good[2],
            flags: 0,
            mask: 0,
        };
        assert_eq!(codes_string(&d), "61756678-534c676e-5361534c");
    }
}
