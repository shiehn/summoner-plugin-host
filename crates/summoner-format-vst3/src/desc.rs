//! A real VST3 slot: the bundle and the class to load (protocol 4.0's `PluginRef`). The class is the one whose class id
//! is the ref's and whose category is `"Audio Module Class"` (the VST3 SDK's `kVstAudioEffectClass`); the name is for
//! display only and never picks a class.

/// The category of a VST3 class that processes audio (`kVstAudioEffectClass`).
pub const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

/// What a slot names: the bundle, the class id and the display name.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Description {
    /// The display name (never picks a class).
    pub name: String,
    /// The `.vst3` bundle's absolute path.
    pub file: String,
    /// The class id, 16 bytes in the order `moduleinfo.json` writes a `CID`.
    pub class_id: [u8; 16],
}

impl Description {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(name: String, file: String, class_id: [u8; 16]) -> Description {
        Description {
            name,
            file,
            class_id,
        }
    }
}

/// A class id in its 32-digit hex form (the order `moduleinfo.json` writes a `CID`), as 16 bytes.
pub fn parse_class_id(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// A class id's 16 bytes as 32 lowercase hex digits.
pub fn class_id_hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02x}")).collect()
}

/// The slot a `PluginRef` names, when it is a VST3 one.
pub fn of_ref(r: &summoner_proto::plugin::PluginRef) -> Option<Description> {
    let class_id = r.class_id()?;
    Some(Description {
        name: r.name().to_string(),
        file: r.path()?.to_string(),
        class_id: parse_class_id(class_id)?,
    })
}

/// The class a slot loads: the index of the first Audio Module class with the slot's class id, or `None`.
pub fn match_class(classes: &[summoner_sys::vst3::ClassInfo], d: &Description) -> Option<usize> {
    classes
        .iter()
        .position(|c| c.category == AUDIO_MODULE_CLASS && c.cid == d.class_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_id_parses_and_prints_back() {
        let s = "5341534c4c4142010000005000000000";
        let b = parse_class_id(s).unwrap();
        assert_eq!(&b[..4], &[0x53, 0x41, 0x53, 0x4c]);
        assert_eq!(class_id_hex(&b), s);
        assert_eq!(
            parse_class_id("5341534C4C4142010000005000000000")
                .map(|b| class_id_hex(&b))
                .as_deref(),
            Some(s)
        );
        for bad in ["", "53", &"0".repeat(33), &format!("{}g", &s[..31])] {
            assert!(parse_class_id(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_vst3_ref_is_a_slot_and_others_are_not() {
        let r: summoner_proto::plugin::PluginRef = serde_json::from_str(
            r#"{"format":"vst3","path":"/x/g.vst3","classId":"5341534c4c4142010000005000000000","name":"Summoner Lab Gain"}"#,
        )
        .unwrap();
        let d = of_ref(&r).unwrap();
        assert_eq!(
            (d.name.as_str(), d.file.as_str()),
            ("Summoner Lab Gain", "/x/g.vst3")
        );
        let au: summoner_proto::plugin::PluginRef = serde_json::from_str(
            r#"{"format":"au","path":"/x/g.component","auCodes":"61756678-534c676e-5361534c","name":"x"}"#,
        )
        .unwrap();
        assert!(of_ref(&au).is_none());
    }

    /// The class is the class id's, Audio Module only; the name never matters.
    #[test]
    fn the_class_is_picked_by_id_and_category() {
        let cid = parse_class_id("5341534c4c4142010000005000000000").unwrap();
        let class = |category: &str, name: &str, cid: [u8; 16]| summoner_sys::vst3::ClassInfo {
            cid,
            cardinality: 0x7fff_ffff,
            class_flags: 0,
            category: category.into(),
            name: name.into(),
            vendor: String::new(),
            version: String::new(),
            sdk_version: String::new(),
            sub_categories: String::new(),
        };
        let d = Description {
            name: "anything".into(),
            file: "/x".into(),
            class_id: cid,
        };
        let mut other = cid;
        other[15] = 1;
        let classes = [
            class("Component Controller Class", "Summoner Lab Gain", cid),
            class(AUDIO_MODULE_CLASS, "Summoner Lab Gain", other),
            class(AUDIO_MODULE_CLASS, "Another Name", cid),
        ];
        assert_eq!(match_class(&classes, &d), Some(2));
        assert_eq!(match_class(&classes[..2], &d), None);
    }
}
