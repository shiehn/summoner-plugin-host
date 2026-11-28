//! Discovery (raw facts, protocol 4.0's [`ScanClass`]) and validation (five steps) for one lab bundle that
//! passed the lab gate and the architecture check.
//!
//! What discovery keeps and drops (the scan's own rules, so a class it never loads is never validated):
//! - VST3: `moduleinfo.json` first (`Contents/Resources/`, then `Contents/`) when it parses as the VST3 SDK's module
//!   info (the factory vendor and each class's CID, Category, Name, Vendor, Version, SDKVersion, Class Flags and
//!   Cardinality present), else the plugin factory. Only `"Audio Module Class"` classes. From the factory, ONE class
//!   per name, compared trimmed and ASCII case-insensitively (the first in the factory's order, counted before its
//!   component is created): a later class of the same name is neither created, reported nor validated. A class whose
//!   component cannot be created is reported (`instantiated: false`) and not validated.
//! - AU: every `AudioComponents` entry, registered in-process (never the system registry); an entry whose codes fail
//!   the code check is reported refused, never instantiated.
use crate::{Reason, Step};
use std::path::Path;
use std::sync::Arc;
use summoner_format_vst3::desc::{AUDIO_MODULE_CLASS, class_id_hex, parse_class_id};
use summoner_proc::{Block, Chunks, MidiEvent, Plugin, SetOutcome, StateKind, Transport};
use summoner_proto::plugin::{AuClass, ScanClass, ScanSource, Vst3Class};
use summoner_proto::state_blob::{self as sasp, Blob};
use summoner_sys::bundle::{Bundle, Format};

const RATE: f64 = 48_000.0;
const FRAMES: usize = 512;
const BLOCKS: usize = 8;

/// Milliseconds since 1970 of a file's modification time, 0 if unknown.
pub fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64)
}

// ---- VST3 -------------------------------------------------------------------------------------------------------

/// One bundle's VST3 classes: `moduleinfo.json` (`Contents/Resources/`, then `Contents/`) when it parses and names
/// at least one Audio Module class, otherwise the factory.
pub fn discover_vst3(b: &Bundle) -> Result<Vec<ScanClass>, crate::Error> {
    discover_vst3_why(b).map_err(crate::Error::from)
}

fn discover_vst3_why(b: &Bundle) -> Result<Vec<ScanClass>, String> {
    for at in [
        "Contents/Resources/moduleinfo.json",
        "Contents/moduleinfo.json",
    ] {
        let f = b.path().join(at);
        if let Ok(text) = std::fs::read(&f)
            && let Some(d) = from_moduleinfo(b.path(), &text)
            && !d.is_empty()
        {
            return Ok(d);
        }
    }
    from_factory(b)
}

/// A `moduleinfo.json`'s Audio Module classes, raw. `None` when the file is not what the SDK's parser accepts (then
/// the factory is read instead).
fn from_moduleinfo(bundle: &Path, text: &[u8]) -> Option<Vec<ScanClass>> {
    let v: serde_json::Value = serde_json::from_slice(text).ok()?;
    let vendor = v.get("Factory Info")?.get("Vendor")?.as_str()?.to_string();
    let classes = v.get("Classes")?.as_array()?;
    let mut out = Vec::new();
    for c in classes {
        let s = |k: &str| c.get(k).and_then(|x| x.as_str());
        let (cid, category, name, version) = (s("CID")?, s("Category")?, s("Name")?, s("Version")?);
        let class_vendor = s("Vendor")?;
        let sdk_version = s("SDKVersion")?;
        c.get("Class Flags")?.as_u64()?;
        c.get("Cardinality")?.as_i64()?;
        let subs: Vec<String> = match c.get("Sub Categories") {
            Some(a) => a
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(String::from))
                .collect::<Option<_>>()?,
            None => Vec::new(),
        };
        if category != AUDIO_MODULE_CLASS {
            continue;
        }
        let Some(cid) = parse_class_id(cid) else {
            continue;
        };
        out.push(ScanClass::Vst3({
            let mut msg = Vst3Class::default();
            msg.path = bundle.display().to_string();
            msg.bundle_mtime_ms = mtime_ms(bundle);
            msg.source = Some(ScanSource::Moduleinfo);
            msg.class_id = class_id_hex(&cid);
            msg.category = category.into();
            msg.name = name.into();
            msg.categories = subs;
            msg.vendor = vendor.clone();
            msg.class_vendor = Some(class_vendor.into());
            msg.version = version.into();
            msg.sdk_version = sdk_version.into();
            msg.instantiated = None;
            msg.inputs = None;
            msg.outputs = None;
            msg
        }));
    }
    Some(out)
}

/// The factory's Audio Module classes, raw: one per trimmed, case-insensitive name (the first,
/// [`crate::class_name::first_class_of_name`]), each component created for its default-active bus channels.
fn from_factory(b: &Bundle) -> Result<Vec<ScanClass>, String> {
    use summoner_sys::vst3::Module;
    let m = Arc::new(Module::open(b).map_err(|e| format!("{e:?}"))?);
    let vendor = m.factory_vendor();
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for c in m.classes() {
        if c.category != AUDIO_MODULE_CLASS {
            continue;
        }
        if !crate::class_name::first_class_of_name(&mut seen, &c.name) {
            continue;
        }
        let (instantiated, inputs, outputs) = match Module::create(&m, &c.cid) {
            Ok(inst) => {
                let io = (inst.default_channels(true), inst.default_channels(false));
                drop(inst);
                (true, Some(io.0), Some(io.1))
            }
            Err(_) => (false, None, None),
        };
        let categories = if c.sub_categories.is_empty() {
            Vec::new()
        } else {
            c.sub_categories.split('|').map(String::from).collect()
        };
        out.push(ScanClass::Vst3({
            let mut msg = Vst3Class::default();
            msg.path = b.path().display().to_string();
            msg.bundle_mtime_ms = mtime_ms(b.path());
            msg.source = Some(ScanSource::Factory);
            msg.class_id = class_id_hex(&c.cid);
            msg.category = c.category.clone();
            msg.name = c.name.clone();
            msg.categories = categories;
            msg.vendor = vendor.clone();
            msg.class_vendor = Some(c.vendor.clone());
            msg.version = c.version.clone();
            msg.sdk_version = c.sdk_version.clone();
            msg.instantiated = Some(instantiated);
            msg.inputs = inputs;
            msg.outputs = outputs;
            msg
        }));
    }
    Ok(out)
}

// ---- AU ---------------------------------------------------------------------------------------------------------

/// One bundle's `AudioComponents` entries, raw, registered in-process (never the system registry), each instantiated
/// for its stream channels. An entry whose codes fail [`summoner_format_au::desc::check_codes`] is reported `refused`,
/// never instantiated; the others as usual.
pub fn discover_au(b: &Bundle) -> Result<Vec<ScanClass>, crate::Error> {
    discover_au_why(b).map_err(crate::Error::from)
}

fn discover_au_why(b: &Bundle) -> Result<Vec<ScanClass>, String> {
    use summoner_sys::au::{SCOPE_INPUT, SCOPE_OUTPUT};
    let reg = summoner_sys::au::register(b).map_err(|e| format!("{e:?}"))?;
    let mut out = Vec::new();
    for (comp, full) in &reg.components {
        let base = {
            let mut msg = AuClass::default();
            msg.path = b.path().display().to_string();
            msg.bundle_mtime_ms = mtime_ms(b.path());
            msg.entry = comp.entry;
            msg.au_codes = summoner_format_au::desc::codes_string(&comp.desc);
            msg.component_name = full.clone();
            msg.au_version = None;
            msg.inputs = None;
            msg.outputs = None;
            msg.refused = None;
            msg
        };
        if let Err(why) =
            summoner_format_au::desc::check_codes(summoner_format_au::desc::codes_of(&comp.desc))
        {
            out.push(ScanClass::Au({
                let mut msg = base;
                msg.refused = Some(why.into());
                msg
            }));
            continue;
        }
        let name = summoner_format_au::desc::name_and_manufacturer(full).0;
        let inst = comp.instantiate().map_err(|e| format!("{name}: {e:?}"))?;
        let (ins, outs) = (
            inst.stream_channels(SCOPE_INPUT),
            inst.stream_channels(SCOPE_OUTPUT),
        );
        drop(inst);
        out.push(ScanClass::Au({
            let mut msg = base;
            msg.au_version = Some(comp.version);
            msg.inputs = Some(ins);
            msg.outputs = Some(outs);
            msg
        }));
    }
    Ok(out)
}

// ---- What validation loads ---------------------------------------------------------------------------------------

/// One class to validate: its display name (what a failure's detail names), whether it is an instrument (no inputs,
/// one note in the first block), and what loads it.
pub struct Target {
    pub name: String,
    pub instrument: bool,
    pub key: String,
    load: Load,
}

enum Load {
    Vst3(summoner_format_vst3::desc::Description),
    Au(summoner_format_au::desc::Description),
}

/// What a class is to validation, or `None` when it is not validated (not an Audio Module class, not instantiated,
/// a refused AU entry: the caller reports the refusal).
pub fn target(c: &ScanClass) -> Option<Target> {
    match c {
        ScanClass::Vst3(v) => {
            if v.category != AUDIO_MODULE_CLASS || v.instantiated == Some(false) {
                return None;
            }
            let (name, instrument) = match v.source {
                // moduleinfo: the name as written; an instrument when a sub-category is exactly `Instrument`.
                Some(ScanSource::Moduleinfo) => (
                    v.name.clone(),
                    v.categories.iter().any(|x| x == "Instrument"),
                ),
                // The factory: the trimmed name; an instrument when the sub-categories (else the category) say
                // `instrument`, in any case.
                _ => {
                    let mut cat = v.categories.join("|").trim().to_string();
                    if cat.is_empty() {
                        cat = v.category.trim().to_string();
                    }
                    (
                        v.name.trim().to_string(),
                        cat.to_lowercase().contains("instrument"),
                    )
                }
            };
            Some(Target {
                key: v.class_id.clone(),
                load: Load::Vst3(summoner_format_vst3::desc::Description::new(
                    name.clone(),
                    v.path.clone(),
                    parse_class_id(&v.class_id)?,
                )),
                name,
                instrument,
            })
        }
        ScanClass::Au(a) => {
            if a.refused.is_some() {
                return None;
            }
            let codes = summoner_proto::codec::check_au_codes(&a.au_codes).ok()?;
            let name = summoner_format_au::desc::name_and_manufacturer(&a.component_name).0;
            Some(Target {
                key: a.au_codes.clone(),
                instrument: codes[0] == summoner_sys::au::TYPE_MUSIC_DEVICE,
                load: Load::Au(summoner_format_au::desc::Description::new(
                    name.clone(),
                    a.path.clone(),
                    codes,
                )),
                name,
            })
        }
        // A format a later codec names: this host does not validate it.
        _ => None,
    }
}

// ---- Validation ---------------------------------------------------------------------------------------------------

/// The five validation steps for one class: instantiate and initialise; prepare (48 kHz / 512) and activate; 8 blocks
/// of silence (an instrument gets one note in the first); the state through the state blob and back; tear down (on
/// this thread).
pub fn validate(t: &Target, _format: Format, steps: &dyn Step) -> Result<(), (Reason, String)> {
    steps.step("instantiate");
    let ch_in = if t.instrument { 0 } else { 2 };
    let plugin: Arc<dyn Plugin> = match &t.load {
        Load::Vst3(d) => Arc::new(
            summoner_format_vst3::load(d, ch_in, 2).map_err(|e| (Reason::Instantiate, e.into()))?,
        ),
        // By the bundle's path and the entry's codes (two entries can share an XOR id).
        Load::Au(d) => Arc::new(
            summoner_format_au::load(d, ch_in, 2).map_err(|e| (Reason::Instantiate, e.into()))?,
        ),
    };
    steps.step("prepare");
    let mut dsp = plugin.prepare(RATE, FRAMES);
    steps.step("process");
    // Not realtime (the scan child): the arena's unwired mode.
    let mut chans = summoner_proc::unwired_channels(2, FRAMES, 0.0);
    let note = [MidiEvent::short(0, [0x90, 60, 100])];
    let tr = Transport::default();
    for k in 0..BLOCKS {
        for c in &mut chans {
            c.fill(0.0);
        }
        let midi: &[MidiEvent] = if t.instrument && k == 0 { &note } else { &[] };
        let ok = dsp.process(&mut Block {
            frames: FRAMES,
            channels: &mut chans,
            midi,
            transport: &tr,
        });
        if !ok {
            return Err((
                Reason::Process,
                format!("block {k} failed (or the prepare left no processor)"),
            ));
        }
    }
    steps.step("state");
    state_round_trip(plugin.as_ref()).map_err(|e| (Reason::State, e))?;
    steps.step("teardown");
    drop(dsp);
    drop(plugin);
    Ok(())
}

/// The state through the state blob and back: the chunks framed as the slot's format, decoded, and applied whole.
fn state_round_trip(p: &dyn Plugin) -> Result<(), String> {
    let c: Chunks = p.get_state_chunks()?;
    let blob = match p.state_kind() {
        StateKind::Builtin => sasp::encode(&Blob::Builtin {
            body: c.first.as_deref().unwrap_or_default(),
        }),
        StateKind::Vst3 => sasp::encode(&Blob::Vst3 {
            component: c.first.as_deref(),
            controller: c.second.as_deref(),
        }),
        StateKind::Au => sasp::encode(&Blob::Au {
            plist: c.first.as_deref().unwrap_or_default(),
        }),
        // A state kind a later host names: this check does not frame it.
        _ => return Err("a state kind this check does not frame".into()),
    }
    .map_err(|r| format!("the state does not frame as a state blob: {r}"))?;
    let (first, second) = match sasp::decode(&blob).map_err(|r| r.to_string())? {
        Blob::Builtin { body } => (Some(body), None),
        Blob::Vst3 {
            component,
            controller,
        } => (component, controller),
        Blob::Au { plist } => (Some(plist), None),
        // A blob form a later codec names: not one this host applies.
        _ => return Err("the state decodes as a blob form this host does not apply".into()),
    };
    match p.set_state_chunks(first, second) {
        SetOutcome::Applied => Ok(()),
        SetOutcome::Refused(why) => Err(format!("its own state was refused: {why}")),
        SetOutcome::FirstOnly(why) => Err(format!("its own state applied in part: {why}")),
        _ => Err("its own state set ended in an outcome this check does not know".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The helper's old name in these tests' bodies.
    use crate::class_name::first_class_of_name as first_of_its_name;

    fn vst3(source: ScanSource, name: &str, cats: &[&str]) -> ScanClass {
        ScanClass::Vst3({
            let mut msg = Vst3Class::default();
            msg.path = "/x/a.vst3".into();
            msg.source = Some(source);
            msg.class_id = "5341534c4c4142010000005000000000".into();
            msg.category = AUDIO_MODULE_CLASS.into();
            msg.name = name.into();
            msg.categories = cats.iter().map(|c| c.to_string()).collect();
            msg.instantiated = (source == ScanSource::Factory).then_some(true);
            msg
        })
    }

    #[test]
    fn one_class_per_trimmed_case_insensitive_name() {
        let mut seen = Vec::new();
        assert!(first_of_its_name(&mut seen, "Summoner Lab Gain"));
        assert!(
            !first_of_its_name(&mut seen, "  summoner lab GAIN "),
            "trimmed, any ASCII case"
        );
        assert!(first_of_its_name(&mut seen, "Summoner Lab Gain 2"));
        assert!(first_of_its_name(&mut seen, "Ünïcode"));
        assert!(
            first_of_its_name(&mut seen, "ünïcode"),
            "case-insensitive in ASCII only"
        );
    }

    /// The trim is Unicode White_Space (NBSP, tab,
    /// U+3000) but not U+200B; the empty name is a name; case folding is ASCII only (no `ß` / `SS`).
    #[test]
    fn trimmed_by_unicode_white_space() {
        let seq = |names: &[&str]| -> Vec<bool> {
            let mut seen = Vec::new();
            names
                .iter()
                .map(|n| first_of_its_name(&mut seen, n))
                .collect()
        };
        assert_eq!(
            seq(&["\u{a0}Pad\t", "pad"]),
            [true, false],
            "NBSP and tab trimmed"
        );
        assert_eq!(
            seq(&["\u{3000}Pad\u{3000}", "PAD"]),
            [true, false],
            "U+3000 trimmed"
        );
        assert_eq!(
            seq(&["\u{200b}Pad", "Pad"]),
            [true, true],
            "U+200B is not white space"
        );
        assert_eq!(seq(&["", "  "]), [true, false], "the empty name is a name");
        assert_eq!(
            seq(&["STRASSE", "straße"]),
            [true, true],
            "no Unicode folding"
        );
        assert_eq!(seq(&["É", "é"]), [true, true], "non-ASCII case is exact");
        assert_eq!(
            seq(&["a  b", "A B"]),
            [true, true],
            "interior white space kept"
        );
    }

    /// What validation names and treats as an instrument, per source (the old descriptions' own rules).
    #[test]
    fn targets_name_and_classify_as_their_source_says() {
        let t = target(&vst3(
            ScanSource::Factory,
            "  Pad  ",
            &["Synth", "INSTRUMENTs"],
        ))
        .unwrap();
        assert_eq!((t.name.as_str(), t.instrument), ("Pad", true));
        let t = target(&vst3(ScanSource::Moduleinfo, "  Raw  ", &["instrument"])).unwrap();
        assert_eq!(
            (t.name.as_str(), t.instrument),
            ("  Raw  ", false),
            "exactly `Instrument` in moduleinfo"
        );
        let t = target(&vst3(ScanSource::Moduleinfo, "R", &["Instrument", "Synth"])).unwrap();
        assert!(t.instrument);
        let mut dead = vst3(ScanSource::Factory, "D", &[]);
        if let ScanClass::Vst3(v) = &mut dead {
            v.instantiated = Some(false);
        }
        assert!(
            target(&dead).is_none(),
            "a class not created is not validated"
        );
        let au = ScanClass::Au({
            let mut msg = AuClass::default();
            msg.path = "/x/a.component".into();
            msg.au_codes = "61756d75-534c7379-5361534c".into();
            msg.component_name = "Summoner Lab: Summoner Lab Synth".into();
            msg.au_version = Some(65536);
            msg
        });
        let t = target(&au).unwrap();
        assert_eq!(
            (t.name.as_str(), t.instrument),
            ("Summoner Lab Synth", true)
        );
        let refused = ScanClass::Au({
            let mut msg = match au {
                ScanClass::Au(a) => a,
                _ => unreachable!(),
            };
            msg.refused = Some("why".into());
            msg
        });
        assert!(target(&refused).is_none());
    }
}
