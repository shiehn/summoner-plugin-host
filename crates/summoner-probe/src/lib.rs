//! `summoner-probe`: the scan child's work for ONE lab bundle. The order is fixed, and each stage runs only if the one
//! before allowed it:
//!
//! 1. **the lab gate** (`Bundle::check`): only the lab's own bundles, before anything of the bundle is opened;
//! 2. **the architecture** ([`macho`]): the executable's own Mach-O header, before any load. No arm64 slice →
//!    `failed {arch}`, with nothing loaded;
//! 3. **discovery**: each class's raw facts (protocol 4.0's `ScanClass`, nothing derived).
//!    - VST3: `moduleinfo.json` (`Contents/Resources/`, then `Contents/`) when it parses as the SDK's parser accepts
//!      it, else the factory (one class per trimmed, case-insensitive name);
//!    - AU: the bundle registered in-process (never the system registry), one class per `AudioComponents` entry;
//! 4. **validation** (native): each class instantiated, prepared at 48 kHz / 512, run for 8 blocks of silence (one
//!    note in the first for an instrument), its state round-tripped through the state blob, torn down.
//!
//! The report is the host application's `scanned` (its scan codec's types): `outcome: ok` with the classes, or
//! `outcome: failed` with a reason word and a detail.
//! The child never reports `crashed` or `timeout`: those are the supervisor's verdicts from outside (the process died;
//! its budget, or the probe's own step deadline, ended it).
#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod class_name;
pub mod macho;
mod validate;

/// The discovery of one checked bundle (the child's `--describe`).
pub use validate::{discover_au, discover_vst3, mtime_ms};

use serde::Serialize;
use std::path::Path;
use summoner_sys::bundle::{Bundle, Format};

/// The wire's types are the host application's (its codec, protocol 4.0: `scanned` carries raw `ScanClass` facts):
/// `scanned` is built from them, never from a copy. A failure's reason is the host application's `ScanFailure` (`arch`
/// among its words).
pub use summoner_proto::codec::{Scan, ScanFailure as Reason, ScanFormat, Scanned, ScannedOutcome};
pub use summoner_proto::plugin::{PluginRef, ScanClass};

/// The `PluginRef` a host loads a discovered class by (the `--describe` line, and the probe's `refs`): `None` for a
/// class that is not loaded (a refused AU entry).
pub fn plugin_ref(c: &ScanClass) -> Option<PluginRef> {
    match c {
        ScanClass::Vst3(v) => PluginRef::vst3(&v.path, v.class_id.clone(), v.name.trim()).ok(),
        ScanClass::Au(a) if a.refused.is_none() => PluginRef::au(
            &a.path,
            a.au_codes.clone(),
            summoner_format_au::desc::name_and_manufacturer(&a.component_name).0,
        )
        .ok(),
        // A refused AU entry, or a format a later codec names: nothing to load.
        _ => None,
    }
}

/// The reason word of an AU entry whose codes the child refuses.
pub const AU_CODES_WORD: &str = "auCodes";

fn au_codes_reason() -> Reason {
    Reason::Other {
        reason: AU_CODES_WORD.into(),
    }
}

/// A reason's word, as the wire writes it in `failed {reason}`.
pub fn word(r: &Reason) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.get("reason").and_then(|w| w.as_str()).map(String::from))
        .unwrap_or_default()
}

/// One class's validation (the probe's diagnostics, beside the wire's fields).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Validated {
    /// The class's display name.
    pub name: String,
    /// The class's key: its `classId` (VST3) or `auCodes` (AU).
    pub key: String,
    /// Whether the class passed validation.
    pub ok: bool,
    /// The failure's word (`state`, `process`, …), as the wire writes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// What failed, in words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Validated {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        name: String,
        key: String,
        ok: bool,
        reason: Option<String>,
        detail: Option<String>,
    ) -> Validated {
        Validated {
            name,
            key,
            ok,
            reason,
            detail,
        }
    }
}

/// The probe's extras: each validated class's `PluginRef` (what a slot takes) and each class's validation.
#[derive(Clone, Debug, Default, Serialize)]
#[non_exhaustive]
pub struct Probe {
    /// Each validated class's `PluginRef`.
    pub refs: Vec<PluginRef>,
    /// Each class's validation, in discovery order.
    pub validation: Vec<Validated>,
    /// The dyld images this scan added to the process (the probe fills it: the images after the scan, less those
    /// before it). The architecture check's proof: an x86_64-only bundle, refused by its header, adds none.
    #[serde(rename = "loadedImages")]
    pub loaded_images: Vec<String>,
    /// The scan process's activation policy after the scan (the probe fills it): 2 (Prohibited) in every
    /// clean scan, `None` where no reader ran.
    #[serde(rename = "policyAfter")]
    pub policy_after: Option<i64>,
    /// How many times the no-UI watchdog listed this process's windows (the probe fills it): at least 2 in every scan
    /// (one at the watchdog's start, one just before the report), so a scan shorter than its 250 ms tick is covered.
    #[serde(rename = "windowChecks")]
    pub window_checks: u32,
}

impl Probe {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        refs: Vec<PluginRef>,
        validation: Vec<Validated>,
        loaded_images: Vec<String>,
        policy_after: Option<i64>,
        window_checks: u32,
    ) -> Probe {
        Probe {
            refs,
            validation,
            loaded_images,
            policy_after,
            window_checks,
        }
    }
}

/// `NSApplicationActivationPolicyProhibited`: no Dock tile, no focus (the policy a scan must leave).
pub const POLICY_PROHIBITED: i64 = 2;

/// `NSApplicationActivationPolicyRegular`: the one policy with a Dock tile and focus.
pub const POLICY_REGULAR: i64 = 0;

/// Reads the scan process's activation policy: `None` when the process has
/// NO NSApplication (the scan child makes none; only a plugin can), else the existing application's policy. The
/// reader must never create one. The tests' readers are fakes, so no test ever sets a real policy.
pub trait PolicyReader {
    /// The policy, or `None` when the process has no NSApplication.
    fn policy(&self) -> Option<i64>;
}

/// The activation-policy check, run on every report before it is printed (an ok one or a failed one):
/// the scan child makes no NSApplication, but a plugin may make one while it is instantiated or prepared, and make it
/// Regular (a Dock tile, focus). The policy is read, recorded in `probe.policyAfter`, and a Regular application fails
/// the scan as `ui`, ahead of whatever the report said. No application, Accessory or Prohibited: the report as it was.
pub fn check_policy(r: &mut Report, reader: &dyn PolicyReader) {
    let p = reader.policy();
    r.probe.policy_after = p;
    if p == Some(POLICY_REGULAR) {
        set_failed(
            r,
            Reason::Ui,
            "the plugin made an NSApplication with the Regular activation policy (a Dock tile)",
        );
    }
}

/// A report of `scan` that failed outside the scan's own stages. Shaped as every other report.
pub fn failed_report(scan: &Scan, reason: Reason, detail: impl Into<String>) -> Report {
    Report {
        scanned: Scanned::new(
            scan.format(),
            scan.path().to_string().clone(),
            failed(reason, detail),
            Vec::new(),
        ),
        probe: Probe::default(),
        discovered: Vec::new(),
    }
}

/// The scan of one bundle: the host application's `scanned` (format, path, outcome, classes), plus the probe's extras.
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct Report {
    /// The host application's `scanned` line (format, path, outcome, classes).
    #[serde(flatten)]
    pub scanned: Scanned,
    /// The probe's extras.
    pub probe: Probe,
    /// When VALIDATION fails, the classes discovery found, in discovery order. Empty for an
    /// `ok` scan and for a failure before or in discovery. Form A carries `classes` only on `ok` (`[]` on `failed`), so
    /// the wire carries these only in form B: the scan child gives them to summoner-proto's encoder as a `failed`
    /// line's classes.
    pub discovered: Vec<ScanClass>,
}

impl Report {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(scanned: Scanned, probe: Probe, discovered: Vec<ScanClass>) -> Report {
        Report {
            scanned,
            probe,
            discovered,
        }
    }
}

/// The step the probe is in, for its per-step deadline (the supervisor's 30 s budget is the backstop).
pub trait Step {
    /// The probe enters step `name`.
    fn step(&self, name: &str);
}

/// No deadline (the library's own tests).
pub struct NoSteps;

impl Step for NoSteps {
    fn step(&self, _: &str) {}
}

fn failed(reason: Reason, detail: impl Into<String>) -> ScannedOutcome {
    ScannedOutcome::Failed {
        failure: reason,
        detail: detail.into(),
    }
}

/// `r` failed: its outcome, and no classes (the wire's "empty unless ok").
fn set_failed(r: &mut Report, reason: Reason, detail: impl Into<String>) {
    r.scanned.outcome = failed(reason, detail);
    r.scanned.classes.clear();
}

/// Scans one bundle, as the host application's `scan` names it (see the module docs for the stages and their order).
/// `Scan` is built only through its own check (an absolute `.vst3` / `.component` bundle path), so every report has a
/// format.
pub fn scan(scan: &Scan, steps: &dyn Step) -> Report {
    let path = Path::new(scan.path());
    let format = match scan.format() {
        ScanFormat::Vst3 => Some(Format::Vst3),
        ScanFormat::Au => Some(Format::Au),
        // A format a later codec names: this probe has no loader for it.
        _ => None,
    };
    let mut r = Report {
        scanned: Scanned::new(
            scan.format(),
            scan.path().to_string().clone(),
            ScannedOutcome::Ok,
            Vec::new(),
        ),
        probe: Probe::default(),
        discovered: Vec::new(),
    };
    let Some(format) = format else {
        set_failed(
            &mut r,
            Reason::NotFound,
            format!("{:?} is a format this probe does not scan", scan.format()),
        );
        return r;
    };
    steps.step("gate");
    let b = match Bundle::check(path, format) {
        Ok(b) => b,
        Err(why) => {
            set_failed(
                &mut r,
                Reason::NotFound,
                format!("{}: {why}", summoner_sys::bundle::REFUSED),
            );
            return r;
        }
    };
    steps.step("arch");
    match macho::executable(b.path())
        .map_err(String::from)
        .and_then(|e| macho::arch_of(&e).map_err(|x| x.to_string()))
    {
        Ok(macho::Arch::Arm64) => {}
        Ok(macho::Arch::Other(types)) => {
            let names: Vec<String> = types.into_iter().map(macho::cpu_name).collect();
            set_failed(
                &mut r,
                Reason::Arch,
                format!("{} only, no arm64 slice; nothing loaded", names.join(", ")),
            );
            return r;
        }
        Ok(macho::Arch::NotMachO) => {
            set_failed(
                &mut r,
                Reason::Arch,
                "not a Mach-O executable; nothing loaded",
            );
            return r;
        }
        Err(e) => {
            set_failed(&mut r, Reason::NotFound, format!("no executable: {e}"));
            return r;
        }
    }
    steps.step("discover");
    let found = match format {
        Format::Vst3 => validate::discover_vst3(&b),
        Format::Au => validate::discover_au(&b),
        // Never reached: the format was mapped above, from the two this probe loads.
        _ => Err(crate::Error::from("a format this probe does not discover")),
    };
    let classes: Vec<ScanClass> = match found {
        Ok(d) if d.is_empty() => {
            set_failed(&mut r, Reason::NotFound, "no plugin classes");
            return r;
        }
        Ok(d) => d,
        Err(e) => {
            set_failed(&mut r, Reason::Instantiate, format!("discovery: {e}"));
            return r;
        }
    };
    // The bundle's verdict names one failure: the first VALIDATION failure outranks the first `auCodes`
    // refusal, wherever each stands. The host application reads `failed {auCodes}` as "every `discovered` class passed
    // validation" and indexes them as good (its scan index), so an earlier refusal must never
    // hide a sibling's failed validation; the refusal still shows in `probe.validation`, by its own word.
    let mut first_invalid = None;
    let mut first_refused = None;
    // The classes the report carries: every one discovery found, a refused AU entry included (with its `refused`; the
    // host application describes none for it, as before 4.0 none was).
    let mut kept = Vec::with_capacity(classes.len());
    for c in classes {
        if let ScanClass::Au(a) = &c
            && let Some(why) = &a.refused
        {
            // An entry whose codes the child refuses is a failed verdict, never instantiated; its
            // siblings are still validated and reported (`discovered`).
            let at = a.entry.map_or_else(|| "?".to_string(), |i| i.to_string());
            let name = summoner_format_au::desc::name_and_manufacturer(&a.component_name).0;
            let detail = format!("{name}: AudioComponents entry {at}: {why}; not instantiated");
            // Its own word, `auCodes` (the host application's ScanFailure::Other, untagged: wire-safe), so the
            // verdict is not read as a validation failure (`instantiate`) of the siblings beside it.
            if first_refused.is_none() {
                first_refused = Some((au_codes_reason(), detail.clone()));
            }
            r.probe.validation.push(Validated {
                name,
                key: a.au_codes.clone(),
                ok: false,
                reason: Some(word(&au_codes_reason())),
                detail: Some(detail),
            });
            kept.push(c);
            continue;
        }
        let t = validate::target(&c);
        kept.push(c);
        let Some(t) = t else { continue };
        let v = validate::validate(&t, format, steps);
        let (reason, detail) = match &v {
            Ok(()) => (None, None),
            Err((reason, detail)) => (Some(reason.clone()), Some(format!("{}: {detail}", t.name))),
        };
        if first_invalid.is_none()
            && let (Some(reason), Some(detail)) = (reason.clone(), detail.clone())
        {
            first_invalid = Some((reason, detail));
        }
        if let Some(pr) = kept.last().and_then(plugin_ref) {
            r.probe.refs.push(pr);
        }
        r.probe.validation.push(Validated {
            name: t.name.clone(),
            key: t.key.clone(),
            ok: v.is_ok(),
            reason: reason.as_ref().map(word),
            detail,
        });
    }
    // The order-swap mutant: the refusal first again; it must fail
    // a_validation_failure_outranks_an_earlier_au_codes_refusal (crates/summoner-probe-bin/tests/scan.rs).
    #[cfg(mutant_refused_first)]
    let verdict = first_refused.or(first_invalid);
    #[cfg(not(mutant_refused_first))]
    let verdict = first_invalid.or(first_refused);
    match verdict {
        None => r.scanned.classes = kept,
        Some((reason, detail)) => {
            // A validation failure keeps what discovery found.
            set_failed(&mut r, reason, detail);
            r.discovered = kept;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Option<i64>);
    impl PolicyReader for Fake {
        fn policy(&self) -> Option<i64> {
            self.0
        }
    }

    fn ok_report() -> Report {
        Report {
            scanned: Scanned::new(ScanFormat::Vst3, "/x.vst3".into(), ScannedOutcome::Ok, vec![
                    serde_json::from_str(
                        r#"{"format":"vst3","path":"/x.vst3","classId":"5341534c4c4142010000005000000000","category":"Audio Module Class","name":"g","categories":[],"vendor":"","version":"","sdkVersion":""}"#,
                    )
                    .unwrap(),
                ]),
            probe: Probe::default(),
            discovered: Vec::new(),
        }
    }

    /// A plugin that made a Regular (0) NSApplication fails the
    /// scan as `ui`, whatever the report said (and its classes go: the wire's "empty unless ok"); no application
    /// (None), Accessory (1) or Prohibited (2) leave it as it was. Fake readers only: no test sets a real activation
    /// policy (a scan must never show a Dock tile).
    #[test]
    fn a_regular_application_fails_the_scan_as_ui() {
        let mut r = ok_report();
        check_policy(&mut r, &Fake(Some(POLICY_REGULAR)));
        assert_eq!(r.probe.policy_after, Some(0));
        match &r.scanned.outcome {
            ScannedOutcome::Failed {
                failure: Reason::Ui,
                detail,
            } => assert!(detail.contains("Regular"), "{detail}"),
            o => panic!("{o:?}"),
        }
        assert!(r.scanned.classes.is_empty());
        for p in [None, Some(1), Some(POLICY_PROHIBITED)] {
            let mut r = ok_report();
            check_policy(&mut r, &Fake(p));
            assert_eq!(r.scanned.outcome, ScannedOutcome::Ok, "{p:?}: unchanged");
            assert_eq!(r.scanned.classes.len(), 1);
            assert_eq!(r.probe.policy_after, p);
        }
        // A failed report is checked too: a changed policy takes precedence over its own reason.
        let sc = Scan::new(ScanFormat::Au, "/y.component").unwrap();
        let mut f = failed_report(&sc, Reason::State, "x");
        check_policy(&mut f, &Fake(Some(0)));
        assert!(matches!(
            f.scanned.outcome,
            ScannedOutcome::Failed {
                failure: Reason::Ui,
                ..
            }
        ));
        assert_eq!(f.scanned.format, ScanFormat::Au);
    }

    /// The report's JSON is the host application's `scanned` plus `probe`: decoding it back as the codec's `Scanned`
    /// gives the same value (the wire is the host application's, not a look-alike).
    #[test]
    fn a_report_is_the_cores_scanned_on_the_wire() {
        let mut r = ok_report();
        let v = serde_json::to_value(&r).unwrap();
        let back: Scanned = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back, r.scanned);
        assert_eq!(v["outcome"], "ok");
        set_failed(&mut r, Reason::Arch, "x86_64 only");
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(
            (&v["outcome"], &v["reason"], &v["detail"]),
            (&"failed".into(), &"arch".into(), &"x86_64 only".into())
        );
        assert_eq!(v["classes"], serde_json::json!([]));
        let back: Scanned = serde_json::from_value(v).unwrap();
        assert_eq!(back, r.scanned);
        assert_eq!(word(&Reason::State), "state");
    }
}

/// The crate's refusal with a reason: its `Display` is the reason as the host's lines and replies word it, its `Debug`
/// the reason's as a string's.
#[derive(Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// The reason's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(why: String) -> Error {
        Error(why)
    }
}

impl From<&str> for Error {
    fn from(why: &str) -> Error {
        Error(why.to_string())
    }
}

/// The reason's text, for a caller that reports it in a message of its own.
impl From<Error> for String {
    fn from(e: Error) -> String {
        e.0
    }
}
