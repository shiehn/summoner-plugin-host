//! What the host child's integration tests share: the summoner supervisor started on this repo's child, chain specs
//! and states, scratch paths, the lab's hostile VST3 description, and the realtime driver
//! ([`drive`], the bridge's exchange alone) that runs real blocks through the bridge.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};
use summoner::supervisor::{Bridge, ChainSpec, Config, Event, SlotState};
use summoner_proto::codec::Role;
use summoner_proto::layout::Geometry;

pub const SR: f64 = 48_000.0;

#[path = "../../../../testkit/plugins/fswatch/fswatch_v2.rs"]
pub mod fswatch_v2;

#[path = "../../../../testkit/plugins/lab_plugins.rs"]
pub mod lab_plugins;

pub mod drive;
pub mod null_harness;

/// Further names of the death switch that every Rig child gets (set to `kill`, like the switch itself, unless the rig
/// takes real deaths): a test binary whose bundles read the switch by another name registers it here first
/// ([`death_switch_alias`]).
static DEATH_ALIASES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

/// Registers `name` as another name of the death switch for every Rig child this test binary starts from now on.
pub fn death_switch_alias(name: &'static str) {
    let mut a = DEATH_ALIASES.lock().unwrap_or_else(|e| e.into_inner());
    if !a.contains(&name) {
        a.push(name);
    }
}

fn death_aliases() -> Vec<&'static str> {
    DEATH_ALIASES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The fswatch recorder's log, read by the shared strict reader (an old-format log panics), with no WRITE whose
/// location cannot be resolved (a STOP).
pub fn assert_fswatch_v2(text: &str) {
    let recs = fswatch_v2::parse(text);
    let u = fswatch_v2::stops(&recs);
    assert!(
        u.is_empty(),
        "the recorder's STOPs (the shared rules): {u:?}"
    );
}

/// The state blob hash the supervisor computes (a `Config::new` argument): SHA-256 as 64 lowercase hex digits, through
/// runecraft-ipc's CommonCrypto binding, the same bytes as the engine's own hasher.
struct Sha256Hex;

impl runecraft_contracts::digest::StateHasher for Sha256Hex {
    fn sha256_hex(&self, bytes: &[u8]) -> String {
        runecraft_ipc::digest::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// The hasher's known answer: FIPS 180-2's SHA-256("abc") (one block), so a wrong digest or hex form cannot pass.
#[test]
fn the_state_hasher_gives_sha256_abc() {
    assert_eq!(
        runecraft_contracts::digest::StateHasher::sha256_hex(&Sha256Hex, b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

/// The tests' state hasher, for `Config::new`.
pub fn state_hasher() -> Arc<dyn runecraft_contracts::digest::StateHasher> {
    Arc::new(Sha256Hex)
}

/// The child these tests run: the test-hooks build (the `<fake .../>` test double and the test flags). The feature is
/// opt-in, so a run without it fails here, loudly, instead of in a confusing refusal later.
pub fn child_bin() -> PathBuf {
    if !cfg!(feature = "test-hooks") {
        panic!("built without test-hooks: run cargo test --features summoner-host/test-hooks");
    }
    PathBuf::from(env!("CARGO_BIN_EXE_summoner-host"))
}

/// A scratch directory under cargo's per-target tmp (never `/tmp`), removed by the caller.
pub fn scratch(name: &str) -> PathBuf {
    // A number per call: tests run in parallel in one process, and each scratch directory must be its own.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let d =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn wait_for(
    rx: &Receiver<Event>,
    seen: &mut Vec<Event>,
    mut pred: impl FnMut(&Event) -> bool,
) -> Event {
    let until = Instant::now() + Duration::from_secs(10);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(e) if pred(&e) => return e,
            Ok(e) => seen.push(e),
            Err(_) => break,
        }
    }
    panic!("no matching event in 10 s; saw {seen:?}");
}

pub fn start(cfg: Config) -> (Bridge, Receiver<Event>) {
    let (tx, rx) = sync_channel(4096);
    let b = Bridge::start(cfg, tx).expect("the supervisor starts");
    let mut seen = Vec::new();
    wait_for(&rx, &mut seen, |e| matches!(e, Event::ChildUp { .. }));
    (b, rx)
}

/// A test slot as JSON: `""` is an empty (quarantined) slot, anything else a protocol-4.0 `PluginRef`
/// (the test double's `{"format":"fake","fake":{…}}`, a VST3 class by id, an AU entry by codes).
pub fn slot(json: &str) -> Option<summoner_proto::plugin::PluginRef> {
    (!json.is_empty()).then(|| {
        serde_json::from_str(json)
            .unwrap_or_else(|e| panic!("a test slot is a plugin ref: {e}: {json}"))
    })
}

/// A VST3 slot: the bundle at `path`, the class `cid`, the display name.
pub fn vst3_ref(path: &std::path::Path, cid: &[u8; 16], name: &str) -> String {
    serde_json::json!({
        "format": "vst3",
        "path": path.display().to_string(),
        "classId": summoner_format_vst3::desc::class_id_hex(cid),
        "name": name,
    })
    .to_string()
}

/// An AU slot: the bundle at `path`, the entry's codes (`type`, `subtype`, `manufacturer` as four-character codes),
/// the display name.
pub fn au_ref(path: &str, kind: &[u8; 4], sub: &[u8; 4], manu: &[u8; 4], name: &str) -> String {
    serde_json::json!({
        "format": "au",
        "path": path,
        "auCodes": format!(
            "{:08x}-{:08x}-{:08x}",
            u32::from_be_bytes(*kind),
            u32::from_be_bytes(*sub),
            u32::from_be_bytes(*manu)
        ),
        "name": name,
    })
    .to_string()
}

/// The lab's VST3 class id FUID (0x5341534C, 0x4C414200 | kind, role, variant).
pub fn lab_cid(kind: u32, role: u32, variant: u32) -> [u8; 16] {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C41_4200 | kind, role, variant]
        .iter()
        .enumerate()
    {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    cid
}

/// A scan's class (protocol 4.0's `ScanClass` JSON) as what the real-plugin harnesses load and show: the
/// slot (its `PluginRef` JSON), its display name, vendor and version, and whether it is an instrument (a VST3's
/// sub-categories or category say `instrument`; an AU of type `aumu`).
pub struct ClassSlot {
    pub slot: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub instrument: bool,
    pub au: bool,
}

pub fn class_slot(v: &serde_json::Value) -> Option<ClassSlot> {
    let c: summoner_probe::ScanClass = serde_json::from_value(v.clone()).ok()?;
    let r = summoner_probe::plugin_ref(&c)?;
    let slot = serde_json::to_string(&r).ok()?;
    Some(match &c {
        summoner_probe::ScanClass::Vst3(x) => ClassSlot {
            slot,
            name: x.name.trim().to_string(),
            vendor: x.vendor.trim().to_string(),
            version: x.version.trim().to_string(),
            instrument: {
                let cat = x.categories.join("|");
                let cat = if cat.trim().is_empty() {
                    x.category.clone()
                } else {
                    cat
                };
                cat.to_lowercase().contains("instrument")
            },
            au: false,
        },
        summoner_probe::ScanClass::Au(a) => ClassSlot {
            slot,
            name: r.name().to_string(),
            vendor: a
                .component_name
                .split_once(':')
                .map(|(m, _)| m.trim().to_string())
                .unwrap_or_default(),
            version: a
                .au_version
                .map(|v| format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff))
                .unwrap_or_default(),
            instrument: a.au_codes.starts_with("61756d75-"),
            au: true,
        },
        _ => return None,
    })
}

pub fn spec(slots: &[&str]) -> ChainSpec {
    {
        let mut msg = ChainSpec::new(
            Geometry::stereo(64),
            Role::Live,
            slots.iter().map(|s| slot(s)).collect(),
        );
        msg.group = 0;
        msg.depth = 1;
        msg.monitor = false;
        msg.enabled = Vec::new();
        msg.param_ids = Arc::from(Vec::<u32>::new());
        msg
    }
}

/// A `<fake>` slot's state (its knob text) as the child frames it: a state blob {builtin}.
pub fn state(slot: u32, generation: u64, text: &str) -> SlotState {
    SlotState::new(
        slot,
        generation,
        Arc::from(summoner_proto::state_blob::builtin(text.as_bytes())),
    )
}

/// A VST3 slot's state as the child frames it: a state blob {vst3} with these chunks (`None` absent).
pub fn vst3_blob(component: Option<&[u8]>, controller: Option<&[u8]>) -> Vec<u8> {
    summoner_proto::state_blob::encode(&summoner_proto::state_blob::Blob::Vst3 {
        component,
        controller,
    })
    .expect("a vst3 state blob")
}

/// A VST3 slot's state: the component chunk alone, a state blob {vst3}.
pub fn vst3_state(slot: u32, generation: u64, component: &[u8]) -> SlotState {
    SlotState::new(
        slot,
        generation,
        Arc::from(vst3_blob(Some(component), None)),
    )
}

/// The lab's hostile VST3 processor class id: FUID (0x5341534C, 0x4C414203, 0x50, 0), as the bundle defines it.
pub fn hostile_cid() -> [u8; 16] {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414203, 0x50, 0].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    cid
}

/// The xorshift noise in [-0.5, 0.5), `frames` long (the host application's bridge tests' source).
pub fn noise_data(frames: usize) -> Arc<[f32]> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..frames)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        })
        .collect()
}

/// A mono one-shot of [`noise_data`], played from frame 0 into both channels.
pub fn noise(frames: usize) -> drive::Input {
    drive::Input::Once(noise_data(frames))
}

/// [`noise`] again every `frames` for `total` frames (a soak's input for its whole length; [`noise`] alone plays its
/// `frames` once and then silence).
pub fn noise_repeating(frames: usize, total: usize) -> drive::Input {
    drive::Input::Repeat(noise_data(frames), total)
}

/// The hostile bundle's classes (testkit/plugins/vst3/lab_vst3.cpp): processor variant and its name.
pub const HOSTILE: u32 = 0;
pub const HOSTILE_CRASH_IN_CTOR: u32 = 1;
pub const HOSTILE_CRASH_IN_PREPARE: u32 = 2;
pub const HOSTILE_SLOW_PREPARE: u32 = 3;
pub const HOSTILE_77_PARAMS: u32 = 77;

/// The slot of a hostile class (protocol 4.0's ref: the class by id).
pub fn hostile_xml(variant: u32) -> String {
    let name = match variant {
        HOSTILE => "Summoner Lab Hostile",
        HOSTILE_CRASH_IN_CTOR => "Summoner Lab Hostile (crash in constructor)",
        HOSTILE_CRASH_IN_PREPARE => "Summoner Lab Hostile (crash in prepare)",
        HOSTILE_SLOW_PREPARE => "Summoner Lab Hostile (3 s prepare)",
        _ => "Summoner Lab Hostile (77 parameters)",
    };
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414203, 0x50, variant]
        .iter()
        .enumerate()
    {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-hostile.vst3"),
        &cid,
        name,
    )
}

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The supervisor on a manual clock, its child under the file-access recorder (every VST3 test) and the
/// service-lookup recorder (does hosting VST3 reach the AU registrar?), with `HOME`
/// a scratch directory (for the crash log).
pub struct Rig {
    pub bridge: Bridge,
    pub rx: Receiver<Event>,
    pub clock: Arc<summoner::clock::ManualClock>,
    pub seen: Vec<Event>,
    pub dir: PathBuf,
    pub log: PathBuf,
    pub lookups: PathBuf,
    /// The first child's pid, from its `ChildUp`.
    pub child_pid: u32,
}

impl Rig {
    pub fn start(name: &str) -> Rig {
        Rig::start_with(name, Vec::new())
    }

    /// As [`Rig::start`], with the child's command-line `args` (`--counters <path>`, say).
    pub fn start_with(name: &str, args: Vec<std::ffi::OsString>) -> Rig {
        Rig::start_child(name, &child_bin(), args)
    }

    /// As [`Rig::start_with`], with another child binary.
    pub fn start_child(name: &str, bin: &Path, args: Vec<std::ffi::OsString>) -> Rig {
        Rig::start_inserting(name, bin, args, &[])
    }

    /// As [`Rig::start_child`], the supervisor (so every chain) at sample rate `rate`.
    pub fn start_child_at(name: &str, bin: &Path, args: Vec<std::ffi::OsString>, rate: f64) -> Rig {
        Rig::start_full_at(name, bin, args, &[], false, rate)
    }

    /// As [`Rig::start_child`], with `extra` images injected after the two recorders (the allocation counter, say).
    pub fn start_inserting(
        name: &str,
        bin: &Path,
        args: Vec<std::ffi::OsString>,
        extra: &[PathBuf],
    ) -> Rig {
        Rig::start_full(name, bin, args, extra, false)
    }

    /// As [`Rig::start`], but the lab's deaths are real signals (no `SUMMONER_LAB_DEATH=kill`): only for the one test per
    /// signal kind where the signal itself is under test (every other death is a SIGKILL, which
    /// leaves no crash report in the user's real DiagnosticReports).
    pub fn start_with_real_deaths(name: &str) -> Rig {
        Rig::start_full(name, &child_bin(), Vec::new(), &[], true)
    }

    fn start_full(
        name: &str,
        bin: &Path,
        args: Vec<std::ffi::OsString>,
        extra: &[PathBuf],
        real_deaths: bool,
    ) -> Rig {
        Rig::start_full_at(name, bin, args, extra, real_deaths, SR)
    }

    fn start_full_at(
        name: &str,
        bin: &Path,
        args: Vec<std::ffi::OsString>,
        extra: &[PathBuf],
        real_deaths: bool,
        rate: f64,
    ) -> Rig {
        // This child activates itself when an editor window opens (the plugin takes the
        // keyboard). The lab's tests open windows on a screen someone is typing on, so this repo's child never does
        // here (another child binary may take no such flag).
        let mut args = args;
        if bin == child_bin() {
            args.push("--test-no-activate".into());
        }
        let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
        let lookwatch = lab_plugins::dir().join("libsummoner_lab_lookwatch.dylib");
        assert!(
            fswatch.is_file() && lookwatch.is_file(),
            "tools/build-plugins.sh"
        );
        let dir = scratch(name);
        let log = dir.join("fswatch.log");
        let lookups = dir.join("lookwatch.log");
        std::fs::create_dir_all(dir.join("home/Library/Logs")).unwrap();
        let mut cfg = Config::new(bin.to_path_buf(), rate, state_hasher());
        cfg.args = args;
        let clock = summoner::clock::ManualClock::new();
        cfg.clock = clock.clone();
        for (k, v) in cfg.env.iter_mut() {
            if k == "HOME" || k == "CFFIXED_USER_HOME" {
                *v = dir.join("home").into();
            }
        }
        // Replaced, never appended: with two entries of one name, which one the child sees is not ours to guess.
        cfg.env.retain(|(k, _)| {
            k != "DYLD_INSERT_LIBRARIES"
                && k != "SUMMONER_FSWATCH_LOG"
                && k != "SUMMONER_LOOKWATCH_LOG"
                && k != "SUMMONER_LAB_DEATH"
        });
        let aliases = death_aliases();
        cfg.env.retain(|(k, _)| !aliases.iter().any(|a| k == a));
        if !real_deaths {
            cfg.env.push(("SUMMONER_LAB_DEATH".into(), "kill".into()));
            for a in aliases {
                cfg.env.push((a.into(), "kill".into()));
            }
        }
        cfg.env.push((
            "DYLD_INSERT_LIBRARIES".into(),
            std::iter::once(fswatch.display().to_string())
                .chain(std::iter::once(lookwatch.display().to_string()))
                .chain(extra.iter().map(|p| p.display().to_string()))
                .collect::<Vec<_>>()
                .join(":")
                .into(),
        ));
        cfg.env
            .push(("SUMMONER_FSWATCH_LOG".into(), log.clone().into()));
        cfg.env
            .push(("SUMMONER_LOOKWATCH_LOG".into(), lookups.clone().into()));
        let (tx, rx) = sync_channel(4096);
        let bridge = Bridge::start(cfg, tx).expect("the supervisor starts");
        let mut seen = Vec::new();
        let Event::ChildUp { pid, .. } =
            wait_for(&rx, &mut seen, |e| matches!(e, Event::ChildUp { .. }))
        else {
            unreachable!()
        };
        Rig {
            bridge,
            rx,
            clock,
            seen: Vec::new(),
            dir,
            log,
            lookups,
            child_pid: pid,
        }
    }

    pub fn send(&self, c: summoner::supervisor::Command) {
        self.bridge.send(c).map_err(|_| "busy").unwrap();
    }

    /// The first event matching `pred`, from the backlog first, then as it arrives (others kept).
    pub fn wait(&mut self, mut pred: impl FnMut(&Event) -> bool) -> Event {
        if let Some(i) = self.seen.iter().position(&mut pred) {
            return self.seen.remove(i);
        }
        wait_for(&self.rx, &mut self.seen, pred)
    }

    /// Advance the manual clock past respawn number `n` (1-based) of this rig's child, without racing its arming.
    ///
    /// The supervisor emits `ChainDown` inside `death()` and only then (`after_death`) reads the clock and arms
    /// `respawn_at = now + backoff[n - 1]`. A test that advances on seeing `ChainDown` can land between the two, and
    /// the respawn is then armed past the advanced clock and never fires (a hostile.rs flake once did).
    /// So first wait (real time) for the observable fact `after_death` stores LAST, the `attempts` counter reaching
    /// `n`; then advance by the host application's own backoff for that attempt (`Config::new`'s schedule, which the
    /// rig keeps) plus EPS. A failure here is a setup / timing failure, never an attribution one.
    pub fn advance_to_respawn(&self, n: u32) {
        const EPS: Duration = Duration::from_millis(100);
        const ARMING: Duration = Duration::from_secs(10);
        assert!(n >= 1);
        let backoff = summoner::supervisor::Config::new(PathBuf::new(), SR, state_hasher()).backoff;
        let step = backoff[(n - 1) as usize] + EPS;
        let attempts = || self.bridge.counters().attempts();
        let until = Instant::now() + ARMING;
        while attempts() < n {
            assert!(
                Instant::now() < until,
                "SETUP/TIMING (not an attribution failure): respawn #{n} was never armed in {ARMING:?} of real time \
                 (attempts = {})",
                attempts()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        self.advance(step);
    }

    /// As [`Rig::wait`], but a timeout names `what` (a timing failure: the event never came), apart from a wrong
    /// event's assertion (a real failure).
    pub fn wait_timing(&mut self, what: &str, pred: impl FnMut(&Event) -> bool) -> Event {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.wait(pred))) {
            Ok(e) => e,
            Err(p) => {
                let why = p
                    .downcast_ref::<String>()
                    .cloned()
                    .unwrap_or_else(|| "?".into());
                panic!("TIMING (not an attribution failure): {what}: {why}")
            }
        }
    }

    /// Advance the supervisor's clock and let it tick.
    pub fn advance(&self, d: Duration) {
        self.clock.advance(d);
        let (tx, rx) = sync_channel(1);
        self.send(summoner::supervisor::Command::Barrier(tx));
        rx.recv_timeout(Duration::from_secs(10)).expect("barrier");
    }

    /// The recorder saw the hostile bundle and nothing forbidden.
    pub fn recorder_clean(&self) {
        self.recorder_clean_for("summoner-lab-hostile.vst3");
    }

    /// The lookup recorder ran in the child (its positive control: lines under the first child's pid) and no process
    /// under it asked for the AU registrar.
    pub fn lookups_clean(&self) {
        let text = std::fs::read_to_string(&self.lookups).unwrap_or_default();
        let pid = format!("{} ", self.child_pid);
        assert!(
            text.lines().any(|l| l.starts_with(&pid)),
            "the lookup recorder logged nothing for child {}:\n{text}",
            self.child_pid
        );
        for bad in [
            "com.apple.audio.AudioComponentRegistrar",
            "com.apple.audio.AudioComponentPrefs",
        ] {
            assert!(!text.contains(bad), "{bad} looked up:\n{text}");
        }
    }

    /// For an AU child: the registrar lookup is EXPECTED (AudioToolbox's own, as a side effect of hosting),
    /// and every lookup is either it or one of the start-up services every lab process makes (the VST3 run's
    /// baseline, transcripts/vst3-lookwatch). Anything else is new and fails.
    pub fn au_lookups_clean(&self) {
        const ALLOWED: [&str; 6] = [
            "com.apple.audio.AudioComponentRegistrar",
            "com.apple.system.notification_center",
            "com.apple.logd",
            "com.apple.system.opendirectoryd.libinfo",
            "com.apple.cfprefsd.agent",
            "com.apple.cfprefsd.daemon",
        ];
        let text = std::fs::read_to_string(&self.lookups).unwrap_or_default();
        assert!(
            text.contains("com.apple.audio.AudioComponentRegistrar"),
            "the AU child never reached the registrar (the positive record):\n{text}"
        );
        for l in text.lines() {
            let service = l.rsplit(' ').next().unwrap_or("");
            assert!(ALLOWED.contains(&service), "a new lookup: {l}");
        }
    }

    /// For an AU child: the recorder saw `bundle`, nothing under a plugin folder, and the lookups are as
    /// [`Rig::au_lookups_clean`] allows.
    pub fn au_recorder_clean_for(&self, bundle: &str) {
        self.au_lookups_clean();
        let text = fswatch_v2::read_log(std::path::Path::new(&self.log));
        assert_fswatch_v2(&text);
        assert!(text.contains(bundle), "the recorder saw the load:\n{text}");
        for bad in [
            "/Library/Audio/Plug-Ins",
            "/System/Library/Components",
            "Library/Audio/Plug-Ins",
        ] {
            assert!(
                !text.lines().any(|l| l.contains(bad)),
                "{bad} touched:\n{text}"
            );
        }
    }

    /// The recorder saw `bundle` and nothing forbidden; and the lookups are clean.
    pub fn recorder_clean_for(&self, bundle: &str) {
        self.lookups_clean();
        let text = fswatch_v2::read_log(std::path::Path::new(&self.log));
        assert_fswatch_v2(&text);
        assert!(text.contains(bundle), "the recorder saw the load:\n{text}");
        for bad in [
            "/Library/Audio/Plug-Ins",
            "/System/Library/Components",
            "Library/Audio/Plug-Ins",
        ] {
            assert!(
                !text.lines().any(|l| l.contains(bad)),
                "{bad} touched:\n{text}"
            );
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // A recorded run keeps every Rig's lookup log.
        if let Some(keep) = std::env::var_os("SUMMONER_LOOKWATCH_KEEP") {
            let name = self.dir.file_name().unwrap_or_default().to_owned();
            let _ = std::fs::copy(
                &self.lookups,
                Path::new(&keep).join(name).with_extension("log"),
            );
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The slot of the lab's gain VST3.
pub fn gain_xml() -> String {
    let mut cid = [0u8; 16];
    for (i, l) in [0x5341534Cu32, 0x4C414201, 0x50, 0].iter().enumerate() {
        cid[i * 4..i * 4 + 4].copy_from_slice(&l.to_be_bytes());
    }
    vst3_ref(
        &lab_plugins::dir().join("vst3/summoner-lab-gain.vst3"),
        &cid,
        "Summoner Lab Gain",
    )
}

/// The lab gain's component chunk for `gain` (testkit/plugins/dsp: "SASL", u32 kind 1, u32 version 1, u32 count 1,
/// f32).
pub fn gain_chunk(gain: f32) -> Vec<u8> {
    let mut v = b"SASL".to_vec();
    for x in [1u32, 1, 1] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.extend_from_slice(&gain.to_le_bytes());
    v
}

/// The child's resident set (KiB) and open file descriptors (read-only tools on the child this test started).
pub fn rss_fds(pid: u32) -> (u64, usize) {
    let ps = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let rss = String::from_utf8_lossy(&ps.stdout)
        .trim()
        .parse()
        .expect("an rss");
    let lsof = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-p", &pid.to_string()])
        .output()
        .expect("lsof");
    let fds = String::from_utf8_lossy(&lsof.stdout)
        .lines()
        .skip(1)
        .filter(|l| {
            l.split_whitespace()
                .nth(3)
                .is_some_and(|fd| fd.starts_with(|c: char| c.is_ascii_digit()))
        })
        .count();
    (rss, fds)
}

/// SIGKILL a process this test spawned, by its raw pid (the floor keeps its raw signal crate-private, so a test
/// that plays the host application with a raw pid kills through `/bin/kill -9`). True when the signal was sent.
#[allow(dead_code)]
pub fn sigkill(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-9", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
