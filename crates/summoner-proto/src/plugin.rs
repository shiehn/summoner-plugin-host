//! What a host says about a plugin, in protocol 4.0: [`PluginRef`] names one plugin class to load, and [`ScanClass`]
//! reports one class a scan found, as raw facts read from the bundle (nothing derived, nothing renamed).
//!
//! Both are plain serde values a user can store. The checks run on decode (`try_from`), so a value that decoded is
//! one that passed them:
//! - a VST3 class id is 32 lowercase hex digits, the class id's 16 bytes in the order `moduleinfo.json` writes its
//!   `CID` ([`check_class_id`]);
//! - an Audio Unit's codes are `<type>-<subtype>-<manufacturer>`, each code a `u32` as 8 lowercase hex digits
//!   ([`check_au_codes`]); in a [`PluginRef`] each code is also non-zero and four printable ASCII bytes, none of
//!   `,` `:` `/` ([`check_au_code_values`]);
//! - a path is absolute, and an unknown field is refused.
//!
//! Control-path values (decoded on the reader and supervisor threads, never on the audio thread), so they allocate,
//! as the rest of the codec does.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
use serde::{Deserialize, Serialize};

use crate::codec::{ProtoError, check_au_codes, check_class_id};

/// The protocol's rule for a bundle path: absolute under this OS's rule (`Path::is_absolute`: on unix
/// exactly a leading `/`; on Windows a drive or `\\?\` path, never a drive-relative one). The text itself is the
/// wire's UTF-8 (a path that is not Unicode never reaches it: [`bundle_text`] refuses it).
pub(crate) fn absolute_bundle_path(p: &str) -> bool {
    // The mutation check (never in a normal build): a relative path passes.
    // `a_bundle_path_is_absolute_under_the_os_rule_and_split_lexically` must fail.
    std::path::Path::new(p).is_absolute() || cfg!(mutant_path_relative_ok)
}

/// The `.` / `..` / empty-segment test of a bundle path, lexical on its text, split on this OS's separators (never
/// `Path::components`, which drops an inner `.`).
pub(crate) fn has_dot_segment(p: &str) -> bool {
    p.split(std::path::is_separator)
        .any(|seg| seg == "." || seg == "..")
}

/// A caller's bundle path as the wire's text: refused when it is not Unicode (a Windows path with an unpaired
/// surrogate cannot ride the UTF-8 wire, so it is refused).
pub fn bundle_text(p: &std::path::Path) -> Result<&str, ProtoError> {
    p.to_str().ok_or_else(|| {
        ProtoError::BadMeta(format!(
            "bundle path {} is not Unicode: it cannot ride the protocol's UTF-8",
            p.display()
        ))
    })
}

/// A [`PluginRef`]'s bound, as its compact JSON: a ref longer than this is refused.
pub const MAX_PLUGIN_REF_BYTES: usize = 4096;
/// A [`PluginRef`]'s bound on the knobs of a test double.
pub const MAX_FAKE_KNOBS: usize = 32;
/// A test double's ref bound: its knobs can describe many parameters (a page test's ~45 KiB), so it is bounded by the
/// frame's meta (64 KiB) less room for the rest of `chain.load`, never by [`MAX_PLUGIN_REF_BYTES`].
pub const MAX_FAKE_REF_BYTES: usize = 60 << 10;

/// The plugin format a [`PluginRef`] or a [`ScanClass`] names.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PluginFormat {
    /// A VST3 bundle (`.vst3`).
    Vst3,
    /// An Audio Unit bundle (`.component`).
    Au,
}

/// One plugin class to load: the bundle and the class in it.
///
/// ```json
/// {"format":"vst3","path":"/abs/Foo.vst3","classId":"<32 lowercase hex>","name":"Foo"}
/// {"format":"au","path":"/abs/Bar.component","auCodes":"61756678-534c6761-5361534c","name":"Bar"}
/// ```
///
/// `name` is for display only (status lines, crash reports, before the plugin is loaded); it is never used to find
/// the class. A host picks the VST3 class whose class id is `classId` and whose category is `"Audio Module Class"`,
/// and the Audio Unit whose bundle entry has the codes `auCodes`.
///
/// `{"format":"fake","fake":{…}}` is a test double's configuration (an object of string, number or boolean knobs).
/// Only a host built for tests loads one; any other host refuses it.
///
/// Its fields are private: a ref is made only through its checks, by decoding or by [`PluginRef::vst3`] /
/// [`PluginRef::au`] (and the test double's [`PluginRef::fake`]), and read through its accessors.
///
/// ```compile_fail
/// let _ = summoner_proto::plugin::PluginRef::Vst3 { path: "x".into(), class_id: "y".into(), name: "z".into() };
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PluginRefUnchecked", into = "PluginRefUnchecked")]
pub struct PluginRef(RefKind);

/// The `Debug` of the ref's kind, exactly as the enum it was printed (`Vst3 { path: .., class_id: .., name: .. }`),
/// so every log line that names a ref keeps its bytes.
impl std::fmt::Debug for PluginRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

/// What a [`PluginRef`] names (crate-private: the ref's fields).
#[derive(Clone, Debug, PartialEq, Eq)]
enum RefKind {
    /// A VST3 class in a `.vst3` bundle.
    Vst3 {
        /// The bundle's absolute path.
        path: String,
        /// The class id: 32 lowercase hex digits.
        class_id: String,
        /// The display name (never used to find the class).
        name: String,
    },
    /// An Audio Unit entry of a `.component` bundle.
    Au {
        /// The bundle's absolute path.
        path: String,
        /// The entry's type, subtype and manufacturer codes: three 8-hex-digit words joined by `-`.
        au_codes: String,
        /// The display name (never used to find the entry).
        name: String,
    },
    /// A test double's configuration; only a host built for tests loads it.
    Fake {
        /// The test double's knobs.
        fake: FakeRef,
    },
}

/// A test double's configuration: an object of string, number or boolean knobs, read only ([`FakeRef::knobs`],
/// [`PluginRef::fake_knob`]).
///
/// Decoding a `{"format":"fake",…}` ref builds one in every build, so a host that is not built for tests still decodes
/// the frame and refuses only that slot. Rust code builds one only with the `test-double` feature
/// ([`PluginRef::fake`]); the field is private, so no other path exists.
///
/// ```compile_fail
/// let _ = summoner_proto::plugin::FakeRef { knobs: Default::default() };
/// ```
#[cfg_attr(
    not(feature = "test-double"),
    doc = "Without the feature there is no constructor:\n\n```compile_fail\nlet _ = summoner_proto::plugin::PluginRef::fake(Default::default());\n```"
)]
#[cfg_attr(
    feature = "test-double",
    doc = "With the feature:\n\n```\nlet r = summoner_proto::plugin::PluginRef::fake(Default::default()).unwrap();\nassert!(r.fake_knob(\"gain\").is_none());\n```"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeRef {
    knobs: serde_json::Map<String, serde_json::Value>,
}

impl FakeRef {
    /// The knobs, as decoded.
    pub fn knobs(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.knobs
    }
}

/// In a build with the `test-double` feature only, so a shipped binary can be checked for it.
#[cfg(feature = "test-double")]
#[used]
pub static TEST_DOUBLE_MARKER: [u8; 34] = *b"summoner-proto feature test-double";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum RefFormat {
    Vst3,
    Au,
    Fake,
}

/// [`PluginRef`] as it is on the wire, before its checks. `--cfg mutant_plugin_ref_unknown_accepted` (never in
/// a normal build) drops `deny_unknown_fields`; `bad_refs_are_refused` fails.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(mutant_plugin_ref_unknown_accepted), serde(deny_unknown_fields))]
struct PluginRefUnchecked {
    format: RefFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    au_codes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fake: Option<serde_json::Map<String, serde_json::Value>>,
}

impl From<PluginRef> for PluginRefUnchecked {
    fn from(r: PluginRef) -> PluginRefUnchecked {
        let none = PluginRefUnchecked {
            format: RefFormat::Fake,
            path: None,
            class_id: None,
            au_codes: None,
            name: None,
            fake: None,
        };
        match r.0 {
            RefKind::Vst3 {
                path,
                class_id,
                name,
            } => PluginRefUnchecked {
                format: RefFormat::Vst3,
                path: Some(path),
                class_id: Some(class_id),
                name: Some(name),
                ..none
            },
            RefKind::Au {
                path,
                au_codes,
                name,
            } => PluginRefUnchecked {
                format: RefFormat::Au,
                path: Some(path),
                au_codes: Some(au_codes),
                name: Some(name),
                ..none
            },
            RefKind::Fake { fake } => PluginRefUnchecked {
                fake: Some(fake.knobs),
                ..none
            },
        }
    }
}

fn bad(m: impl std::fmt::Display) -> String {
    format!("plugin: {m}")
}

/// Each Audio Unit code non-zero, and each of its four bytes printable ASCII (`0x20..=0x7e`) and none of `,` `:`
/// `/`. The error names the code and the byte, never the value.
pub fn check_au_code_values(codes: [u32; 3]) -> Result<(), ProtoError> {
    au_code_values(codes).map_err(ProtoError::BadMeta)
}

fn au_code_values(codes: [u32; 3]) -> Result<(), String> {
    const NAMES: [&str; 3] = ["type", "subtype", "manufacturer"];
    for (i, v) in codes.iter().enumerate() {
        if *v == 0 {
            return Err(format!("auCodes: the {} code is zero", NAMES[i]));
        }
        for (k, &b) in v.to_be_bytes().iter().enumerate() {
            if !(0x20..=0x7e).contains(&b) || matches!(b, b',' | b':' | b'/') {
                return Err(format!(
                    "auCodes: the {} code's byte {k} is outside printable ASCII or is one of ',' ':' '/'",
                    NAMES[i]
                ));
            }
        }
    }
    Ok(())
}

fn check_path(p: &str, ext: &str) -> Result<(), String> {
    if !absolute_bundle_path(p) || p.contains('\0') {
        return Err(bad("path is not absolute"));
    }
    if !p.trim_end_matches('/').ends_with(ext) {
        return Err(bad(format!("path is not a {ext} bundle")));
    }
    Ok(())
}

impl TryFrom<PluginRefUnchecked> for PluginRef {
    type Error = String;
    fn try_from(u: PluginRefUnchecked) -> Result<PluginRef, String> {
        let r = match u.format {
            RefFormat::Vst3 | RefFormat::Au => {
                if u.fake.is_some() {
                    return Err(bad("fake on a vst3 or au ref"));
                }
                let (Some(path), Some(name)) = (u.path, u.name) else {
                    return Err(bad("path and name are required"));
                };
                if u.format == RefFormat::Vst3 {
                    if u.au_codes.is_some() {
                        return Err(bad("auCodes on a vst3 ref"));
                    }
                    let Some(class_id) = u.class_id else {
                        return Err(bad("classId is required"));
                    };
                    check_path(&path, ".vst3")?;
                    check_class_id(&class_id).map_err(|e| bad(proto(e)))?;
                    RefKind::Vst3 {
                        path,
                        class_id,
                        name,
                    }
                } else {
                    if u.class_id.is_some() {
                        return Err(bad("classId on an au ref"));
                    }
                    let Some(au_codes) = u.au_codes else {
                        return Err(bad("auCodes is required"));
                    };
                    check_path(&path, ".component")?;
                    let c = check_au_codes(&au_codes).map_err(|e| bad(proto(e)))?;
                    if !cfg!(mutant_plugin_ref_au_values_unchecked) {
                        au_code_values(c).map_err(bad)?;
                    }
                    RefKind::Au {
                        path,
                        au_codes,
                        name,
                    }
                }
            }
            RefFormat::Fake => {
                if u.path.is_some()
                    || u.class_id.is_some()
                    || u.au_codes.is_some()
                    || u.name.is_some()
                {
                    return Err(bad("a fake ref carries only fake"));
                }
                let Some(fake) = u.fake else {
                    return Err(bad("fake is required"));
                };
                if fake.len() > MAX_FAKE_KNOBS {
                    return Err(bad(format!("over {MAX_FAKE_KNOBS} fake knobs")));
                }
                if fake
                    .values()
                    .any(|v| v.is_array() || v.is_object() || v.is_null())
                {
                    return Err(bad("a fake knob is not a string, number or boolean"));
                }
                RefKind::Fake {
                    fake: FakeRef { knobs: fake },
                }
            }
        };
        let r = PluginRef(r);
        let n = r.json_len();
        let cap = if matches!(r.0, RefKind::Fake { .. }) {
            MAX_FAKE_REF_BYTES
        } else {
            MAX_PLUGIN_REF_BYTES
        };
        if n > cap && !cfg!(mutant_plugin_ref_uncapped) {
            return Err(bad(format!("{n} bytes, over {cap}")));
        }
        Ok(r)
    }
}

fn proto(e: ProtoError) -> String {
    match e {
        ProtoError::BadMeta(m) => m,
        e => e.to_string(),
    }
}

impl PluginRef {
    /// The ref's length as compact JSON (its bound is [`MAX_PLUGIN_REF_BYTES`]).
    pub fn json_len(&self) -> usize {
        serde_json::to_vec(&PluginRefUnchecked::from(self.clone())).map_or(usize::MAX, |v| v.len())
    }

    /// A VST3 class to load: `path` an absolute `.vst3` bundle path (Unicode), `class_id` 32 lowercase hex digits,
    /// `name` for display. Refused (and nothing built) on any check a decoded ref fails.
    pub fn vst3(
        path: impl AsRef<std::path::Path>,
        class_id: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<PluginRef, ProtoError> {
        let path = bundle_text(path.as_ref())?.to_string();
        // The mutation check (never in a normal build): the constructor skips the checks.
        // `a_constructed_ref_passes_the_decoders_checks` must fail.
        if cfg!(mutant_ref_ctor_unchecked) {
            return Ok(PluginRef(RefKind::Vst3 {
                path,
                class_id: class_id.into(),
                name: name.into(),
            }));
        }
        PluginRef::try_from(PluginRefUnchecked {
            format: RefFormat::Vst3,
            path: Some(path),
            class_id: Some(class_id.into()),
            au_codes: None,
            name: Some(name.into()),
            fake: None,
        })
        .map_err(ProtoError::BadMeta)
    }

    /// An Audio Unit entry to load: `path` an absolute `.component` bundle path (Unicode), `au_codes` the
    /// `<type>-<subtype>-<manufacturer>` codes (each 8 lowercase hex digits, non-zero, printable ASCII), `name` for
    /// display. Refused (and nothing built) on any check a decoded ref fails.
    pub fn au(
        path: impl AsRef<std::path::Path>,
        au_codes: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<PluginRef, ProtoError> {
        let path = bundle_text(path.as_ref())?.to_string();
        if cfg!(mutant_ref_ctor_unchecked) {
            return Ok(PluginRef(RefKind::Au {
                path,
                au_codes: au_codes.into(),
                name: name.into(),
            }));
        }
        PluginRef::try_from(PluginRefUnchecked {
            format: RefFormat::Au,
            path: Some(path),
            class_id: None,
            au_codes: Some(au_codes.into()),
            name: Some(name.into()),
            fake: None,
        })
        .map_err(ProtoError::BadMeta)
    }

    /// The display name (`""` for a test double).
    pub fn name(&self) -> &str {
        match &self.0 {
            RefKind::Vst3 { name, .. } | RefKind::Au { name, .. } => name,
            RefKind::Fake { .. } => "",
        }
    }

    /// The bundle's path (`None` for a test double).
    pub fn path(&self) -> Option<&str> {
        match &self.0 {
            RefKind::Vst3 { path, .. } | RefKind::Au { path, .. } => Some(path),
            RefKind::Fake { .. } => None,
        }
    }

    /// A VST3 ref's class id (`None` for another format).
    pub fn class_id(&self) -> Option<&str> {
        match &self.0 {
            RefKind::Vst3 { class_id, .. } => Some(class_id),
            _ => None,
        }
    }

    /// An Audio Unit ref's codes, as the wire's text (`None` for another format).
    pub fn au_codes(&self) -> Option<&str> {
        match &self.0 {
            RefKind::Au { au_codes, .. } => Some(au_codes),
            _ => None,
        }
    }

    /// Whether this is a test double's ref.
    pub fn is_fake(&self) -> bool {
        matches!(self.0, RefKind::Fake { .. })
    }

    /// The bundle's path as a path (`None` for a test double).
    pub fn bundle_path(&self) -> Option<&std::path::Path> {
        self.path().map(std::path::Path::new)
    }

    /// The format (`None` for a test double).
    pub fn format(&self) -> Option<PluginFormat> {
        match self.0 {
            RefKind::Vst3 { .. } => Some(PluginFormat::Vst3),
            RefKind::Au { .. } => Some(PluginFormat::Au),
            RefKind::Fake { .. } => None,
        }
    }

    /// A test double's ref built from Rust code, through the same checks a decoded one passes. Only with the
    /// `test-double` feature (a test host or a test harness); decoding needs no feature.
    #[cfg(any(feature = "test-double", mutant_fake_ctor_ungated))]
    pub fn fake(
        knobs: serde_json::Map<String, serde_json::Value>,
    ) -> Result<PluginRef, ProtoError> {
        PluginRef::try_from(PluginRefUnchecked {
            format: RefFormat::Fake,
            path: None,
            class_id: None,
            au_codes: None,
            name: None,
            fake: Some(knobs),
        })
        .map_err(ProtoError::BadMeta)
    }

    /// A test double's knob as text (a string as it is, a number or boolean as JSON writes it).
    pub fn fake_knob(&self, key: &str) -> Option<String> {
        let RefKind::Fake { fake } = &self.0 else {
            return None;
        };
        fake.knobs.get(key).map(|v| match v {
            serde_json::Value::String(s) => s.clone(),
            v => v.to_string(),
        })
    }
}

/// Where a scan read a VST3 class's facts: the bundle's `moduleinfo.json` (nothing loaded), or the plugin factory
/// (the bundle's code loaded and each class's component created).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ScanSource {
    /// From the bundle's `moduleinfo.json`, with nothing loaded.
    Moduleinfo,
    /// From the plugin factory, with the bundle's code loaded.
    Factory,
}

/// One class a scan found in a bundle, as raw facts: strings exactly as the bundle has them (untrimmed), lists
/// unjoined. A scan reports every `"Audio Module Class"` VST3 class (its first class per name, compared trimmed and
/// case-insensitively: a later class with the same name is never loaded or reported) and every `AudioComponents`
/// entry of an Audio Unit bundle.
///
/// ```json
/// {"format":"vst3","path":"/abs/Foo.vst3","bundleMtimeMs":1763500000000,"source":"factory",
///  "classId":"<32 lowercase hex>","category":"Audio Module Class","name":"Foo","categories":["Fx","Delay"],
///  "vendor":"Foo Inc","classVendor":"Foo Inc","version":"1.0.0","sdkVersion":"VST 3.8.0",
///  "instantiated":true,"inputs":2,"outputs":2}
/// {"format":"au","path":"/abs/Bar.component","bundleMtimeMs":1763500000000,"entry":0,
///  "auCodes":"61756678-534c6761-5361534c","componentName":"Bar Inc: Bar","auVersion":65536,"inputs":2,"outputs":2}
/// {"format":"au","path":"/abs/Bar.component","bundleMtimeMs":1763500000000,"entry":1,
///  "auCodes":"61756678-534c3a61-5361534c","componentName":"Bar Inc: Bad","refused":"<why, the value never echoed>"}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ScanClassUnchecked", into = "ScanClassUnchecked")]
#[non_exhaustive]
pub enum ScanClass {
    /// A VST3 class.
    Vst3(Vst3Class),
    /// An Audio Unit bundle entry.
    Au(AuClass),
}

/// A VST3 class's facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Vst3Class {
    /// The bundle's absolute path.
    pub path: String,
    /// The bundle's modification time, ms since 1970 (0 when unknown).
    pub bundle_mtime_ms: i64,
    /// Where the facts were read (`None` when the scan did not say).
    pub source: Option<ScanSource>,
    /// The class id: 32 lowercase hex digits.
    pub class_id: String,
    /// The class's category (`"Audio Module Class"` for a plugin).
    pub category: String,
    /// The class's name, as the bundle has it.
    pub name: String,
    /// The class's sub-categories, in order (`moduleinfo.json`'s list, or the factory's `|`-separated string split).
    pub categories: Vec<String>,
    /// The factory's vendor.
    pub vendor: String,
    /// The class's own vendor, when the bundle gives one.
    pub class_vendor: Option<String>,
    /// The class's version string.
    pub version: String,
    /// The VST3 SDK version the class was built with.
    pub sdk_version: String,
    /// Whether the class's component was created during the scan (`None`: not tried, the facts came from
    /// `moduleinfo.json`).
    pub instantiated: Option<bool>,
    /// The default-active bus channels, when the component was created.
    pub inputs: Option<u32>,
    /// The default-active bus output channels, when the component was created.
    pub outputs: Option<u32>,
}

/// An Audio Unit bundle entry's facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct AuClass {
    /// The bundle's absolute path.
    pub path: String,
    /// The bundle's modification time, ms since 1970 (0 when unknown).
    pub bundle_mtime_ms: i64,
    /// The entry's index in the bundle's `AudioComponents` list (`None` when the bundle did not give one).
    pub entry: Option<u32>,
    /// The entry's type, subtype and manufacturer codes: three 8-hex-digit words joined by `-`.
    pub au_codes: String,
    /// The entry's `name`, as the bundle has it (by convention `"Manufacturer: Name"`).
    pub component_name: String,
    /// The entry's `version`; `None` on a refused entry.
    pub au_version: Option<u32>,
    /// The default input channels, when the scan read them.
    pub inputs: Option<u32>,
    /// The default output channels, when the scan read them.
    pub outputs: Option<u32>,
    /// Why the scan refused the entry before creating it (its codes); the value is never echoed.
    pub refused: Option<String>,
}

impl ScanClass {
    /// The bundle's path as a path.
    pub fn bundle_path(&self) -> &std::path::Path {
        std::path::Path::new(self.path())
    }

    /// The bundle's path.
    pub fn path(&self) -> &str {
        match self {
            ScanClass::Vst3(v) => &v.path,
            ScanClass::Au(a) => &a.path,
        }
    }

    /// The class's name as the bundle has it (an Audio Unit's `componentName`).
    pub fn name(&self) -> &str {
        match self {
            ScanClass::Vst3(v) => &v.name,
            ScanClass::Au(a) => &a.component_name,
        }
    }

    /// An Audio Unit's codes (`None` for a VST3).
    pub fn au_codes(&self) -> Option<&str> {
        match self {
            ScanClass::Vst3(_) => None,
            ScanClass::Au(a) => Some(&a.au_codes),
        }
    }
}

/// [`ScanClass`] as it is on the wire, before its checks. Unknown fields are TOLERATED (a newer child's additive
/// fact, protocol 4.x minors being additive): collected here, dropped, and noted once per process
/// ([`unknown_scan_class_fields`]). A [`PluginRef`] stays strict.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanClassUnchecked {
    format: PluginFormat,
    path: String,
    #[serde(default)]
    bundle_mtime_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<ScanSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    categories: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    class_vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sdk_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instantiated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    au_codes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    component_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    au_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inputs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outputs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
    /// Fields this codec does not know (never written).
    #[serde(flatten, skip_serializing)]
    unknown: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

impl From<ScanClass> for ScanClassUnchecked {
    fn from(c: ScanClass) -> ScanClassUnchecked {
        match c {
            ScanClass::Vst3(v) => ScanClassUnchecked {
                format: PluginFormat::Vst3,
                path: v.path,
                bundle_mtime_ms: v.bundle_mtime_ms,
                source: v.source,
                class_id: Some(v.class_id),
                category: Some(v.category),
                name: Some(v.name),
                categories: Some(v.categories),
                vendor: Some(v.vendor),
                class_vendor: v.class_vendor,
                version: Some(v.version),
                sdk_version: Some(v.sdk_version),
                instantiated: v.instantiated,
                entry: None,
                au_codes: None,
                component_name: None,
                au_version: None,
                inputs: v.inputs,
                outputs: v.outputs,
                refused: None,
                unknown: Default::default(),
            },
            ScanClass::Au(a) => ScanClassUnchecked {
                format: PluginFormat::Au,
                path: a.path,
                bundle_mtime_ms: a.bundle_mtime_ms,
                source: None,
                class_id: None,
                category: None,
                name: None,
                categories: None,
                vendor: None,
                class_vendor: None,
                version: None,
                sdk_version: None,
                instantiated: None,
                entry: a.entry,
                au_codes: Some(a.au_codes),
                component_name: Some(a.component_name),
                au_version: a.au_version,
                inputs: a.inputs,
                outputs: a.outputs,
                refused: a.refused,
                unknown: Default::default(),
            },
        }
    }
}

/// How many unknown ScanClass fields this process has dropped (a newer child's additive facts).
static UNKNOWN_FIELDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The ScanClass fields this process has dropped as unknown, so far.
pub fn unknown_scan_class_fields() -> u64 {
    UNKNOWN_FIELDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Counts `n` dropped fields; the first time, says so once on stderr (bounded: one line per process).
fn note_unknown_fields(n: usize) {
    if UNKNOWN_FIELDS.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed) == 0 {
        // One write of the whole line to the locked stderr, its error ignored (a diagnostic is never worth a panic).
        use std::io::Write as _;
        let _ = std::io::stderr().lock().write_all(
            b"summoner-proto: a scan class carried fields this host does not know (a newer plugin child): ignored; \
              counted, said once\n",
        );
    }
}

/// Decodes one scan class and says whether it carried unknown fields (form B's canonical check cannot re-encode
/// those, so it is skipped for such a line).
pub(crate) fn decode_scan_class(line: &[u8]) -> Result<(ScanClass, bool), serde_json::Error> {
    let u: ScanClassUnchecked = serde_json::from_slice(line)?;
    let unknown = !u.unknown.is_empty();
    let c = ScanClass::try_from(u).map_err(serde::de::Error::custom)?;
    Ok((c, unknown))
}

impl TryFrom<ScanClassUnchecked> for ScanClass {
    type Error = String;
    fn try_from(u: ScanClassUnchecked) -> Result<ScanClass, String> {
        let bad = |m: &str| format!("scan class: {m}");
        if !u.unknown.is_empty() {
            // The mutant (never in a normal build): an unknown field refused again, as before the amend; tests
            // plugin's `a_scan_class_with_an_unknown_field_is_accepted` and compat's describe test fail.
            if cfg!(mutant_scan_class_unknown_refused) {
                return Err(bad("an unknown field"));
            }
            note_unknown_fields(u.unknown.len());
        }
        if !absolute_bundle_path(&u.path) {
            return Err(bad("path is not absolute"));
        }
        match u.format {
            PluginFormat::Vst3 => {
                if u.entry.is_some()
                    || u.au_codes.is_some()
                    || u.component_name.is_some()
                    || u.au_version.is_some()
                    || u.refused.is_some()
                {
                    return Err(bad("an Audio Unit field on a vst3 class"));
                }
                let (
                    Some(class_id),
                    Some(category),
                    Some(name),
                    Some(categories),
                    Some(vendor),
                    Some(version),
                    Some(sdk_version),
                ) = (
                    u.class_id,
                    u.category,
                    u.name,
                    u.categories,
                    u.vendor,
                    u.version,
                    u.sdk_version,
                )
                else {
                    return Err(bad(
                        "classId, category, name, categories, vendor, version and sdkVersion are required",
                    ));
                };
                check_class_id(&class_id).map_err(proto)?;
                Ok(ScanClass::Vst3(Vst3Class {
                    path: u.path,
                    bundle_mtime_ms: u.bundle_mtime_ms,
                    source: u.source,
                    class_id,
                    category,
                    name,
                    categories,
                    vendor,
                    class_vendor: u.class_vendor,
                    version,
                    sdk_version,
                    instantiated: u.instantiated,
                    inputs: u.inputs,
                    outputs: u.outputs,
                }))
            }
            PluginFormat::Au => {
                if u.source.is_some()
                    || u.class_id.is_some()
                    || u.category.is_some()
                    || u.name.is_some()
                    || u.categories.is_some()
                    || u.vendor.is_some()
                    || u.class_vendor.is_some()
                    || u.version.is_some()
                    || u.sdk_version.is_some()
                    || u.instantiated.is_some()
                {
                    return Err(bad("a vst3 field on an au class"));
                }
                let (Some(au_codes), Some(component_name)) = (u.au_codes, u.component_name) else {
                    return Err(bad("auCodes and componentName are required"));
                };
                check_au_codes(&au_codes).map_err(proto)?;
                if u.refused.is_none() && u.au_version.is_none() {
                    return Err(bad("auVersion is required unless refused"));
                }
                Ok(ScanClass::Au(AuClass {
                    path: u.path,
                    bundle_mtime_ms: u.bundle_mtime_ms,
                    entry: u.entry,
                    au_codes,
                    component_name,
                    au_version: u.au_version,
                    inputs: u.inputs,
                    outputs: u.outputs,
                    refused: u.refused,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle-path rule's verdicts are today's on unix (a leading `/`, no NUL, no `.` / `..`
    /// segment, split lexically: an inner `.` is refused, which `Path::components` would have dropped), and a caller's
    /// path that is not Unicode is refused. `--cfg mutant_path_relative_ok` must fail this.
    #[cfg(unix)]
    #[test]
    fn a_bundle_path_is_absolute_under_the_os_rule_and_split_lexically() {
        use crate::codec::{Scan, ScanFormat};
        for (p, ok) in [
            ("/lab/x.vst3", true),
            ("/lab/sub/x.vst3", true),
            ("lab/x.vst3", false),
            ("./x.vst3", false),
            ("/lab/./x.vst3", false),
            ("/lab/../x.vst3", false),
            ("/lab/x.vst3/", false),
            ("/lab/x\0.vst3", false),
        ] {
            assert_eq!(Scan::new(ScanFormat::Vst3, p).is_ok(), ok, "{p}");
        }
        assert!(absolute_bundle_path("/a"));
        assert!(!absolute_bundle_path("a/b"));
        assert!(
            has_dot_segment("/a/./b") && has_dot_segment("/a/../b") && !has_dot_segment("/a/b.c/d")
        );
        let s = Scan::new(ScanFormat::Vst3, std::path::Path::new("/lab/x.vst3")).unwrap();
        assert_eq!(s.bundle_path(), std::path::Path::new("/lab/x.vst3"));
        use std::os::unix::ffi::OsStrExt;
        let not_utf8 = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/lab/\xff.vst3"));
        assert!(bundle_text(not_utf8).is_err());
        assert!(Scan::new(ScanFormat::Vst3, not_utf8).is_err());
    }

    const CID: &str = "5341534c4c4142010000005000000000";

    fn de(s: &str) -> Result<PluginRef, String> {
        serde_json::from_str::<PluginRef>(s).map_err(|e| e.to_string())
    }

    #[test]
    fn refs_round_trip_byte_for_byte() {
        for s in [
            format!(r#"{{"format":"vst3","path":"/x/a.vst3","classId":"{CID}","name":"A"}}"#),
            r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c6761-5361534c","name":"A"}"#
                .to_string(),
            r#"{"format":"fake","fake":{"gain":"0.5","name":"f","synth":1}}"#.to_string(),
        ] {
            let r = de(&s).unwrap();
            assert_eq!(serde_json::to_string(&r).unwrap(), s);
        }
        let r = de(r#"{"format":"fake","fake":{"delay":64,"synth":true}}"#).unwrap();
        assert_eq!(r.fake_knob("delay").as_deref(), Some("64"));
        assert_eq!(r.fake_knob("synth").as_deref(), Some("true"));
    }

    /// A fake ref decodes in every build (with or without `test-double`), keeps its knobs readable and its bytes, and
    /// still matches `Fake { .. }`; with the feature, the constructor builds the same ref and refuses what decode does.
    #[test]
    fn a_fake_ref_decodes_with_or_without_the_feature() {
        let s = r#"{"format":"fake","fake":{"gain":"0.5","synth":true}}"#;
        let r = de(s).unwrap();
        assert!(r.is_fake());
        let RefKind::Fake { fake } = &r.0 else {
            unreachable!()
        };
        assert_eq!(fake.knobs().len(), 2);
        assert_eq!(r.fake_knob("gain").as_deref(), Some("0.5"));
        assert_eq!(serde_json::to_string(&r).unwrap(), s);
        #[cfg(feature = "test-double")]
        {
            let RefKind::Fake { fake } = r.0.clone() else {
                unreachable!()
            };
            assert_eq!(PluginRef::fake(fake.knobs().clone()), Ok(r));
            let mut nested = serde_json::Map::new();
            nested.insert("a".into(), serde_json::json!([1]));
            assert!(PluginRef::fake(nested).is_err());
            assert!(TEST_DOUBLE_MARKER.starts_with(b"summoner-proto"));
        }
    }

    /// Every refusal; the mutants `mutant_plugin_ref_uncapped` and
    /// `mutant_plugin_ref_au_values_unchecked` fail here.
    #[test]
    fn bad_refs_are_refused() {
        let long = "a".repeat(MAX_PLUGIN_REF_BYTES);
        for (why, s) in [
            ("unknown field", format!(r#"{{"format":"vst3","path":"/x/a.vst3","classId":"{CID}","name":"A","uid":1}}"#)),
            ("uppercase class id", format!(r#"{{"format":"vst3","path":"/x/a.vst3","classId":"{}","name":"A"}}"#, CID.to_uppercase())),
            ("short class id", r#"{"format":"vst3","path":"/x/a.vst3","classId":"5341","name":"A"}"#.into()),
            ("codes on vst3", format!(r#"{{"format":"vst3","path":"/x/a.vst3","classId":"{CID}","auCodes":"61756678-534c6761-5361534c","name":"A"}}"#)),
            ("no class id", r#"{"format":"vst3","path":"/x/a.vst3","name":"A"}"#.into()),
            ("relative path", format!(r#"{{"format":"vst3","path":"x/a.vst3","classId":"{CID}","name":"A"}}"#)),
            ("wrong bundle", format!(r#"{{"format":"vst3","path":"/x/a.component","classId":"{CID}","name":"A"}}"#)),
            ("bad codes", r#"{"format":"au","path":"/x/a.component","auCodes":"61756678534c67615361534c","name":"A"}"#.into()),
            ("zero code", r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-00000000-5361534c","name":"A"}"#.into()),
            ("colon in a code", r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c3a61-5361534c","name":"A"}"#.into()),
            ("no name", r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c6761-5361534c"}"#.into()),
            ("path on fake", r#"{"format":"fake","path":"/x/a.vst3","fake":{}}"#.into()),
            ("nested fake knob", r#"{"format":"fake","fake":{"a":[1]}}"#.into()),
            ("other format", r#"{"format":"clap","path":"/x/a.clap","name":"A"}"#.into()),
            ("over the cap", format!(r#"{{"format":"vst3","path":"/x/{long}.vst3","classId":"{CID}","name":"A"}}"#)),
            ("a fake over its cap", format!(r#"{{"format":"fake","fake":{{"x":"{}"}}}}"#, "a".repeat(MAX_FAKE_REF_BYTES))),
        ] {
            assert!(de(&s).is_err(), "{why}: {s}");
        }
    }

    /// The constructors run the decoder's checks: a good ref encodes to the bytes a decoded one has, and each bad
    /// input a decoded ref refuses is refused by `vst3` / `au` too.
    #[test]
    fn a_constructed_ref_passes_the_decoders_checks() {
        let v = PluginRef::vst3("/x/a.vst3", CID, "A").unwrap();
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            format!(r#"{{"format":"vst3","path":"/x/a.vst3","classId":"{CID}","name":"A"}}"#)
        );
        assert_eq!(v, de(&serde_json::to_string(&v).unwrap()).unwrap());
        assert_eq!(
            (v.class_id(), v.au_codes(), v.name()),
            (Some(CID), None, "A")
        );
        let a = PluginRef::au("/x/a.component", "61756678-534c6761-5361534c", "B").unwrap();
        assert_eq!(a.au_codes(), Some("61756678-534c6761-5361534c"));
        assert_eq!(a.format(), Some(PluginFormat::Au));
        for (why, r) in [
            (
                "uppercase class id",
                PluginRef::vst3("/x/a.vst3", CID.to_uppercase(), "A"),
            ),
            ("short class id", PluginRef::vst3("/x/a.vst3", "5341", "A")),
            ("relative path", PluginRef::vst3("x/a.vst3", CID, "A")),
            ("wrong bundle", PluginRef::vst3("/x/a.component", CID, "A")),
            (
                "over the cap",
                PluginRef::vst3(
                    format!("/x/{}.vst3", "a".repeat(MAX_PLUGIN_REF_BYTES)),
                    CID,
                    "A",
                ),
            ),
            (
                "bad codes",
                PluginRef::au("/x/a.component", "61756678534c67615361534c", "A"),
            ),
            (
                "zero code",
                PluginRef::au("/x/a.component", "61756678-00000000-5361534c", "A"),
            ),
            (
                "colon in a code",
                PluginRef::au("/x/a.component", "61756678-534c3a61-5361534c", "A"),
            ),
            (
                "wrong au bundle",
                PluginRef::au("/x/a.vst3", "61756678-534c6761-5361534c", "A"),
            ),
        ] {
            assert!(matches!(r, Err(ProtoError::BadMeta(_))), "{why}: {r:?}");
        }
    }

    /// A form B line with an unknown field skips the canonical-bytes check, so its known fields may
    /// come reordered or spaced: it is accepted, and what is kept (the decoded class, re-encoded: the bytes the record's
    /// description is made from) is exactly the canonical encoding of the known class. Duplicate KNOWN keys are still
    /// refused, with the flattened map of unknown fields in the struct: on a canonical line, a reordered one and one
    /// with an unknown field (serde's own "duplicate field" error with `flatten` too, measured: no code of ours to mutate).
    #[test]
    fn form_b_keeps_the_canonical_class_and_refuses_duplicate_keys() {
        let canonical = format!(
            r#"{{"format":"vst3","path":"/x/a.vst3","bundleMtimeMs":0,"source":"factory","classId":"{CID}","category":"Audio Module Class","name":"A","categories":["Fx"],"vendor":"V","version":"1","sdkVersion":"s","instantiated":true,"inputs":2,"outputs":2}}"#
        );
        let known: ScanClass = serde_json::from_str(&canonical).unwrap();
        // Reordered keys, spaces, and an unknown field.
        let messy = format!(
            r#"{{ "name" : "A", "format":"vst3", "newer": [1], "path":"/x/a.vst3","classId":"{CID}","category":"Audio Module Class","categories":["Fx"],"vendor":"V","version":"1","sdkVersion":"s","source":"factory","instantiated":true,"outputs":2,"inputs":2,"bundleMtimeMs":0 }}"#
        );
        let blob = format!("{messy}\n");
        let kept = crate::codec::decode_descriptions_blob(blob.as_bytes(), 1).expect("accepted");
        assert_eq!(kept, vec![known.clone()]);
        assert_eq!(
            serde_json::to_string(&kept[0]).unwrap(),
            canonical,
            "kept as the canonical encoding"
        );
        // The same reordering WITHOUT an unknown field is still non-canonical, refused.
        let reordered = messy.replace(r#" "newer": [1],"#, "");
        assert!(
            crate::codec::decode_descriptions_blob(format!("{reordered}\n").as_bytes(), 1).is_err()
        );
        // Duplicate known keys: refused everywhere.
        for (why, line) in [
            (
                "canonical",
                canonical.replacen(r#""name":"A""#, r#""name":"A","name":"B""#, 1),
            ),
            (
                "with an unknown field",
                messy.replacen(r#""name" : "A""#, r#""name" : "A", "name":"B""#, 1),
            ),
            (
                "a duplicate numeric key",
                canonical.replacen(r#""inputs":2"#, r#""inputs":2,"inputs":0"#, 1),
            ),
        ] {
            assert!(
                serde_json::from_str::<ScanClass>(&line).is_err(),
                "{why}: {line}"
            );
            assert!(
                crate::codec::decode_descriptions_blob(format!("{line}\n").as_bytes(), 1).is_err(),
                "{why}: the blob line"
            );
        }
    }

    /// 4.x minors are additive: a class with a field this codec does not know is accepted, the field
    /// dropped (never written back) and counted; the same class without it decodes equal. Through form A's `scanned`
    /// and a form B line too. The mutant `mutant_scan_class_unknown_refused` fails here.
    #[test]
    fn a_scan_class_with_an_unknown_field_is_accepted() {
        let known = format!(
            r#"{{"format":"vst3","path":"/x/a.vst3","bundleMtimeMs":0,"source":"factory","classId":"{CID}","category":"Audio Module Class","name":"A","categories":["Fx"],"vendor":"V","version":"1","sdkVersion":"s","instantiated":true,"inputs":2,"outputs":2}}"#
        );
        let newer = known.replacen('}', r#","newerFact":{"x":[1,2]},"also":"y"}"#, 1);
        let before = unknown_scan_class_fields();
        let a: ScanClass = serde_json::from_str(&known).unwrap();
        let b: ScanClass = serde_json::from_str(&newer).expect("an unknown field is tolerated");
        assert_eq!(a, b, "the same class");
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            known,
            "never written back"
        );
        assert!(unknown_scan_class_fields() >= before + 2, "counted");
        let au = r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c6761-5361534c","componentName":"M: N","auVersion":1,"extra":1}"#;
        assert!(serde_json::from_str::<ScanClass>(au).is_ok());
        // Form A: a whole `scanned` line.
        let line =
            format!(r#"{{"format":"vst3","path":"/x/a.vst3","outcome":"ok","classes":[{newer}]}}"#);
        let s: crate::codec::Scanned = serde_json::from_str(&line).unwrap();
        assert_eq!(s.classes, vec![a.clone()]);
        // Form B: a blob line.
        let (c, unknown) = decode_scan_class(newer.as_bytes()).unwrap();
        assert!(unknown && c == a);
        let blob = format!("{newer}\n");
        assert_eq!(
            crate::codec::decode_descriptions_blob(blob.as_bytes(), 1).unwrap(),
            vec![a]
        );
    }

    #[test]
    fn scan_classes_round_trip_and_refuse() {
        for s in [
            format!(
                r#"{{"format":"vst3","path":"/x/a.vst3","bundleMtimeMs":5,"source":"factory","classId":"{CID}","category":"Audio Module Class","name":" A ","categories":["Fx","Delay"],"vendor":"V","classVendor":"W","version":"1","sdkVersion":"VST 3.8.0","instantiated":true,"inputs":2,"outputs":2}}"#
            ),
            format!(
                r#"{{"format":"vst3","path":"/x/a.vst3","bundleMtimeMs":0,"source":"moduleinfo","classId":"{CID}","category":"Audio Module Class","name":"A","categories":[],"vendor":"V","version":"1","sdkVersion":"s"}}"#
            ),
            r#"{"format":"au","path":"/x/a.component","bundleMtimeMs":0,"entry":0,"auCodes":"61756678-534c6761-5361534c","componentName":"M: N","auVersion":65536,"inputs":2,"outputs":2}"#.into(),
            r#"{"format":"au","path":"/x/a.component","bundleMtimeMs":0,"entry":1,"auCodes":"61756678-534c3a61-5361534c","componentName":"M: N","refused":"why"}"#.into(),
        ] {
            let c: ScanClass = serde_json::from_str(&s).unwrap();
            assert_eq!(serde_json::to_string(&c).unwrap(), s);
        }
        for s in [
            r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c6761-5361534c","componentName":"M: N"}"#,
            r#"{"format":"au","path":"/x/a.component","auCodes":"61756678-534c6761-5361534c","componentName":"M: N","auVersion":1,"name":"x"}"#,
            r#"{"format":"vst3","path":"/x/a.vst3","classId":"zz","category":"c","name":"A","categories":[],"vendor":"V","version":"1","sdkVersion":"s"}"#,
        ] {
            assert!(serde_json::from_str::<ScanClass>(s).is_err(), "{s}");
        }
    }
}
