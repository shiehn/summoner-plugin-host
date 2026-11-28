//! `summoner-testproc`: the lab's in-process test double for a chain slot, the slot the host application's bridge e2e
//! tests drive. Bit-identical to the host application's test double, with these rules fixed where a choice was needed:
//! the play head's fields, strict number parsing, and note state kept across a re-prepare:
//!
//! ```text
//! {"format":"fake","fake":{"name":"...","gain":"g","delay":"n","synth":"1","ignore-cc123":"1","sustain":"1",
//!  "probe":"1","crash-load":"1","echo":"e","feedback":"f"}}
//! ```
//!
//! (protocol 4.0's test-double `PluginRef`; a knob's value is a string, or a number or boolean read as its JSON text;
//! a flag knob counts when present.)
//!
//! - gain, then (`echo`) a decaying delay of e frames with feedback f (`y = fma(f, line[pos], x)`, `line[pos] = y`, a
//!   value under 1e-30 in magnitude flushed to 0: a long tail), then a fixed delay of n frames (its
//!   latency), on channels 0 and 1;
//! - `synth`: an instrument gate, output 0.25 (× gain) while any note is held (a count of note-ons less note-offs,
//!   reset by CC 123 / CC 120 unless `ignore-cc123`; with `sustain`, a note let go while its channel's CC 64 is ≥ 64
//!   stays held until that CC 64 drops below 64);
//! - `probe`: channel 0's first 11 frames are the transport as a play head reports it (0 ppq, 1 bpm, 2 / 3 the time
//!   signature, 4 / 5 loop start / end ppq, 6 bar start ppq, 7 the transport flag bits, 8 time in seconds, 9 time in
//!   samples, 10 host time in ns), the rest 0;
//! - `crash-load`: aborts while the plugin is made (a crash inside chain.load);
//! - parameters (region values, set before the block): 0 gain (raw value), 1 crash inside the block (≥ 0.5), 2 wedge
//!   inside the block (≥ 0.5, never returns), 3 CPU burn (value × 100 ms per block);
//! - states: `gain=<f>` sets the gain; `CRASH` aborts inside set_state; `SLOWGET=<ms>` makes get_state sleep (on the
//!   message thread); `SLOWSET=<ms>` makes set_state sleep first (its receipt comes that much later); `FAIL` is
//!   applied and refused (`ok: false`); `BAREGET` makes the host answer this slot's state.get / editor.closed with
//!   the bare body instead of a state blob {builtin} (the bare-state refusal test, as the host application's test
//!   double).
//!
//! **This repo's extensions** (not in the host application's fake, and unused by the host application's tests; the
//! grammar above is unchanged by them):
//! - `latency="n"`: the latency the plugin reports, when it differs from its delay (default: `delay`);
//! - state `latency=<n>`: the reported latency changes at run time (what a plugin's latency-changed notice is), so
//!   the host's `latency.changed` is testable; the delay line itself does not change;
//! - state `SEGV`: a SIGSEGV inside set_state, for the crash handler's test;
//! - state `RESTART=<n>;CYCLEMS=<ms>` (the host application's reactivation knob, as its test double has it): the
//!   next poll asks the host to re-activate the slot (`restartComponent(kLatencyChanged)`); the cycle
//!   brings it back fresh (lines and voices) at a delay, and a reported latency, of n frames, and takes ms (default 0);
//! - parameters 4 (skip) and 5 (throw), past the 4 the double reports (as the host application's test double): >= 0.5
//!   acks the block SKIPPED and silent (the host's, `Plugin::test_skip`), or makes it THREW and silent;
//!   `exit-load="n"` exits n while the plugin is made (inside chain.load, after the hello); state `BLOCKMS=<ms>`: the next block takes that long
//!   (all three the host application's fake's);
//! - state `BIGGET=<n>`: get_state answers the stored state padded with zeros to `n` bytes (at most 128 MiB), so a
//!   state over the codec's 64 MiB cap is testable (protocol 3.3's `state.get` tooLarge) without sending one;
//! - state `REACTIVATE[=<ms>]`: the next poll asks the host to re-activate the slot (as a VST3 `kLatencyChanged`
//!   does), and the re-activation takes `<ms>`, so the host's cycle and its stall count are testable without a
//!   plugin;
//! - `inputs="n"` and `outputs="n"`, the buses [`Dsp::io`] reports (default 2 and 2, so every other test is
//!   unchanged; at most 64), and `write="all|ch0|add|none"`, how the processor writes its block: every channel (the
//!   default, as above), channel 0 only (channel 1 is left as the host handed it), accumulating (a `synth` adds its
//!   0.25 to what the channel holds instead of replacing it), or nothing at all (the block returns untouched).
//!
//! **Protocol 3.7's parameter descriptions, as the host application's test double:** plugin ids 10-13 for gain, crash,
//! wedge, burn (only gain is automatable and settable by `param.set`);
//! `xparams="name|auto|clump|steps|label|default;..."` adds parameters at raw indices 4.. (so on such a slot the hidden
//! region knobs 4 and 5 are those parameters), plugin ids 100.., ordered by the AU clump rule
//! (`summoner_plugin::params`); a host set applies at once and writes `gain=<v>` / `p<id>=<v>` into the state (which
//! `set_state` reads back).
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use summoner_proc::{Block, Dsp, Notice, Plugin};
use summoner_proto::layout::{
    BF_BAR_START_VALID, BF_LOOP_POINTS_VALID, BF_LOOPING, BF_PLAYING, BF_RECORDING,
};

/// A test double's configuration, read from its ref's knobs.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// `name`: the display name.
    pub name: String,
    /// `gain`: the gain (raw value).
    pub gain: f32,
    /// `delay`: the fixed delay in frames.
    pub delay: usize,
    /// `synth`: an instrument gate.
    pub synth: bool,
    /// `ignore-cc123`: CC 123 / CC 120 do not reset the held notes.
    pub ignore_cc123: bool,
    /// `sustain`: CC 64 holds released notes.
    pub sustain: bool,
    /// `probe`: channel 0 reports the transport.
    pub probe: bool,
    /// `crash-load`: aborts while the plugin is made.
    pub crash_load: bool,
    /// `exit-load="n"`: exits n while the plugin is made.
    pub exit_load: Option<i32>,
    /// `echo`: the decaying delay's length in frames (0: none).
    pub echo: usize,
    /// `feedback`: the decaying delay's feedback.
    pub feedback: f32,
    /// The reported latency (`latency="n"`; default `delay`).
    pub latency: u32,
    /// `xparams`: the extra parameters.
    pub xparams: Vec<XParam>,
    /// The buses [`Dsp::io`] reports (`inputs` / `outputs`, default 2 / 2).
    pub io: summoner_proc::Io,
    /// How the processor writes its block (`write`, default [`Write::All`]).
    pub write: Write,
}

/// `write="all|ch0|add|none"` (anything else: `all`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Write {
    /// Every channel of the block (channels 0 and 1).
    #[default]
    All,
    /// Channel 0 only; channel 1 is left as the host handed it (a mono-out plugin that writes one buffer).
    Ch0,
    /// As `All`, but a `synth` adds its gate to the channel instead of replacing it (a plugin that accumulates).
    Add,
    /// Nothing: the block is returned untouched (a plugin that relies on silence flags, say).
    None,
}

/// The most bus channels `inputs` / `outputs` take.
const MAX_IO: u64 = 64;

/// One `xparams` entry (`name|auto|clump|steps|label|default`).
#[derive(Clone, Debug, PartialEq)]
pub struct XParam {
    /// The parameter's name.
    pub name: String,
    /// `auto`: automatable unless `0`.
    pub automatable: bool,
    /// `clump`: its clump id (`0` or absent: none).
    pub clump: Option<u32>,
    /// `steps`: the step count (0: continuous).
    pub steps: u32,
    /// `label`: the unit label.
    pub label: String,
    /// `default`: the normalized default.
    pub default: f32,
}

fn xparams(spec: &str) -> Vec<XParam> {
    spec.split(';')
        .filter(|e| !e.is_empty())
        .map(|e| {
            let f: Vec<&str> = e.split('|').collect();
            let get = |i: usize| f.get(i).copied().unwrap_or("");
            let clump: u32 = get(2).parse().unwrap_or(0);
            XParam {
                name: get(0).into(),
                automatable: get(1) != "0",
                clump: (clump != 0).then_some(clump),
                steps: get(3).parse().unwrap_or(0),
                label: get(4).into(),
                default: strict_f32(get(5).as_bytes()).unwrap_or(0.0),
            }
        })
        .collect()
}

/// The plugin ids of gain, crash, wedge, burn, and the first `xparams` entry's.
const BASE_IDS: [u32; 4] = [10, 11, 12, 13];
const XPARAM_ID0: u32 = 100;

/// The longest delay or echo line (frames).
const MAX_LINE: u64 = 1 << 20;

/// `strictFloat`, by the C library's `strtof` (`summoner_sys::num`).
fn strict_f32(s: &[u8]) -> Option<f32> {
    summoner_sys::num::strtof_strict(s)
}

/// `strictU64`: non-empty, ASCII digits only, no overflow.
fn strict_u64(s: &[u8]) -> Option<u64> {
    (!s.is_empty() && s.iter().all(u8::is_ascii_digit))
        .then(|| std::str::from_utf8(s).ok()?.parse().ok())
        .flatten()
}

impl Config {
    /// The configuration of slot `slot`'s ref; `None` when the ref is not the test double.
    pub fn from_ref(r: &summoner_proto::plugin::PluginRef, slot: usize) -> Option<Config> {
        if !r.is_fake() {
            return None;
        }
        let knob = |n: &str| r.fake_knob(n);
        let num = |n: &str| knob(n).map(String::into_bytes);
        let has = |n: &str| knob(n).is_some();
        let delay = num("delay")
            .as_deref()
            .and_then(strict_u64)
            .map_or(0, |d| d.min(MAX_LINE) as usize);
        Some(Config {
            name: knob("name").unwrap_or_else(|| format!("fake{slot}")),
            gain: num("gain").as_deref().and_then(strict_f32).unwrap_or(1.0),
            delay,
            synth: has("synth"),
            ignore_cc123: has("ignore-cc123"),
            sustain: has("sustain"),
            probe: has("probe"),
            crash_load: has("crash-load"),
            exit_load: knob("exit-load").and_then(|v| v.parse().ok()),
            echo: num("echo")
                .as_deref()
                .and_then(strict_u64)
                .map_or(0, |e| e.min(MAX_LINE) as usize),
            feedback: num("feedback")
                .as_deref()
                .and_then(strict_f32)
                .unwrap_or(0.0),
            latency: num("latency")
                .as_deref()
                .and_then(strict_u64)
                .map_or(delay as u32, |l| l.min(MAX_LINE) as u32),
            xparams: knob("xparams").map(|x| xparams(&x)).unwrap_or_default(),
            io: summoner_proc::Io::new(
                num("inputs")
                    .as_deref()
                    .and_then(strict_u64)
                    .map_or(2, |n| n.min(MAX_IO) as u32),
                num("outputs")
                    .as_deref()
                    .and_then(strict_u64)
                    .map_or(2, |n| n.min(MAX_IO) as u32),
            ),
            write: match knob("write").as_deref() {
                Some("ch0") => Write::Ch0,
                Some("add") => Write::Add,
                Some("none") => Write::None,
                _ => Write::All,
            },
        })
    }
}

const PARAM_NAMES: [&str; 4] = ["gain", "crash", "wedge", "burn"];

/// The note state, kept on the plugin (not the prepared side) so a re-prepare keeps it, as a hosted
/// processor's members do. Chain-thread only: loaded at the block's start, stored at its end.
#[derive(Default)]
struct Notes {
    held: AtomicU32,
    pedal: AtomicU32,
    sustained: [AtomicU32; 16],
}

/// One test plugin: what its message-thread side and its prepared chain-thread side share.
struct Inner {
    cfg: Config,
    /// The four raw parameters (f32 bits): gain, crash, wedge, burn.
    params: [AtomicU32; 4],
    notes: Notes,
    state: Mutex<Vec<u8>>,
    slow_get_ms: AtomicU64,
    /// `BIGGET=<n>`: get_state's size (0: the stored state as it is).
    big_get: AtomicU64,
    /// `BLOCKMS=<ms>`: the next processed block's extra time.
    block_once_ms: AtomicU64,
    /// Parameters 4 and 5 (>= 0.5): skip or throw the block.
    skip: AtomicBool,
    throw: AtomicBool,
    bare_get: AtomicBool,
    latency: AtomicU32,
    /// `REACTIVATE`: a re-activation is asked for (taken by poll), and how long it takes.
    reactivate_asked: AtomicBool,
    reactivate_ms: AtomicU64,
    /// `RESTART=<n>`: the delay (and latency) the next cycle brings the slot back at, plus one (0: none).
    restart_to: AtomicU64,
    /// The `xparams` values (f32 bits) and every parameter's parameter-tree order.
    xvals: Vec<AtomicU32>,
    orders: Vec<u32>,
}

impl Inner {
    fn param(&self, i: usize) -> f32 {
        f32::from_bits(self.params[i].load(Ordering::Relaxed))
    }
}

/// The test double, message-thread side (its [`Dsp`] is made by [`Plugin::prepare`]).
pub struct TestPlugin(Arc<Inner>);

impl TestPlugin {
    /// Makes the plugin; `crash-load` aborts here (a crash inside chain.load, attributed by the status record).
    pub fn new(cfg: Config) -> TestPlugin {
        if let Some(n) = cfg.exit_load {
            std::process::exit(n);
        }
        if cfg.crash_load {
            summoner_sys::crash::lab_abort();
        }
        let clumps: Vec<Option<u32>> = [None; 4]
            .into_iter()
            .chain(cfg.xparams.iter().map(|x| x.clump))
            .collect();
        let orders = summoner_plugin::params::au_clump_order(&clumps);
        let xvals = cfg
            .xparams
            .iter()
            .map(|x| AtomicU32::new(x.default.to_bits()))
            .collect();
        TestPlugin(Arc::new(Inner {
            xvals,
            orders,
            params: [
                AtomicU32::new(cfg.gain.to_bits()),
                AtomicU32::new(0),
                AtomicU32::new(0),
                AtomicU32::new(0),
            ],
            latency: AtomicU32::new(cfg.latency),
            reactivate_asked: AtomicBool::new(false),
            reactivate_ms: AtomicU64::new(0),
            restart_to: AtomicU64::new(0),
            cfg,
            notes: Notes::default(),
            state: Mutex::new(Vec::new()),
            slow_get_ms: AtomicU64::new(0),
            big_get: AtomicU64::new(0),
            block_once_ms: AtomicU64::new(0),
            skip: AtomicBool::new(false),
            throw: AtomicBool::new(false),
            bare_get: AtomicBool::new(false),
        }))
    }
}

impl summoner_proc::TestHooks for TestPlugin {
    fn test_param(&self, index: u32, value: f32) -> bool {
        match index {
            4 => self.0.skip.store(value >= 0.5, Ordering::Relaxed),
            5 => self.0.throw.store(value >= 0.5, Ordering::Relaxed),
            _ => return false,
        }
        true
    }

    fn test_skip(&self) -> bool {
        self.0.skip.load(Ordering::Relaxed)
    }

    fn answers_bare(&self) -> bool {
        self.0.bare_get.load(Ordering::Relaxed)
    }
}

impl Plugin for TestPlugin {
    fn name(&self) -> &str {
        &self.0.cfg.name
    }

    fn latency(&self) -> u32 {
        self.0.latency.load(Ordering::Relaxed)
    }

    fn param_count(&self) -> u32 {
        (PARAM_NAMES.len() + self.0.cfg.xparams.len()) as u32
    }

    fn param_name(&self, index: u32) -> String {
        let i = index as usize;
        match PARAM_NAMES.get(i) {
            Some(n) => (*n).into(),
            None => self
                .0
                .cfg
                .xparams
                .get(i - PARAM_NAMES.len())
                .map(|x| x.name.clone())
                .unwrap_or_default(),
        }
    }

    fn set_param(&self, index: u32, value: f32) {
        let i = index as usize;
        if let Some(p) = self.0.params.get(i) {
            p.store(value.to_bits(), Ordering::Relaxed);
        } else if let Some(x) = self.0.xvals.get(i - PARAM_NAMES.len()) {
            x.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    fn param_meta(&self, index: u32) -> summoner_proc::ParamMeta {
        let i = index as usize;
        let order = self.0.orders.get(i).copied().unwrap_or(u32::MAX);
        if i < PARAM_NAMES.len() {
            return {
                let mut msg = summoner_proc::ParamMeta::default();
                msg.plugin_id = BASE_IDS[i];
                msg.automatable = i == 0;
                msg.default = if i == 0 {
                    self.0.cfg.gain.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                msg.order = order;
                msg
            };
        }
        let Some(x) = self.0.cfg.xparams.get(i - PARAM_NAMES.len()) else {
            return summoner_proc::ParamMeta::default();
        };
        summoner_proc::ParamMeta::new(
            XPARAM_ID0 + (i - PARAM_NAMES.len()) as u32,
            x.automatable,
            x.default,
            x.steps,
            x.label.clone(),
            order,
        )
    }

    fn param_value(&self, index: u32) -> f32 {
        let i = index as usize;
        let v = if i < PARAM_NAMES.len() {
            self.0.param(i)
        } else {
            self.0
                .xvals
                .get(i - PARAM_NAMES.len())
                .map_or(0.0, |x| f32::from_bits(x.load(Ordering::Relaxed)))
        };
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Gain and the `xparams` only (crash, wedge and burn are hostile region knobs); applied at once, and
    /// written into the state as a typical plugin's controller does.
    fn host_set(&self, plugin_id: u32, value: f32) -> Result<f32, summoner_proc::Error> {
        let why = || -> Result<f32, String> {
            let (index, knob) = if plugin_id == BASE_IDS[0] {
                (0usize, "gain".to_string())
            } else if plugin_id >= XPARAM_ID0
                && ((plugin_id - XPARAM_ID0) as usize) < self.0.cfg.xparams.len()
            {
                (
                    PARAM_NAMES.len() + (plugin_id - XPARAM_ID0) as usize,
                    format!("p{plugin_id}"),
                )
            } else {
                return Err(format!("no settable parameter {plugin_id}"));
            };
            self.set_param(index as u32, value);
            set_knob(&mut self.0.state.lock().unwrap(), &knob, value);
            Ok(self.param_value(index as u32))
        };
        why().map_err(summoner_proc::Error::from)
    }

    fn test_hooks(&self) -> Option<&dyn summoner_proc::TestHooks> {
        Some(self)
    }

    /// The state set, on the bytes (not UTF-8 checked).
    fn set_state(&self, blob: &[u8]) -> Result<(), summoner_proc::Error> {
        let why = || -> Result<(), String> {
            if blob.starts_with(b"CRASH") {
                summoner_sys::crash::lab_abort(); // a hostile state crashes inside set_state, attributed by the status record
            }
            if blob.starts_with(b"SEGV") {
                summoner_sys::crash::lab_segv();
            }
            if let Some(rest) = blob.strip_prefix(b"REACTIVATE") {
                let ms = rest.strip_prefix(b"=").and_then(strict_u64).unwrap_or(0);
                self.0
                    .reactivate_ms
                    .store(ms.min(60_000), Ordering::Relaxed);
                self.0.reactivate_asked.store(true, Ordering::Release);
            }
            // `RESTART=<n>` and `CYCLEMS=<ms>`, as parts (the host application's reactivation knob).
            let mut restart = None;
            let mut cycle_ms = 0;
            for part in blob.split(|b| *b == b';') {
                if let Some(n) = part.strip_prefix(b"RESTART=").and_then(strict_u64) {
                    restart = Some(n.min(MAX_LINE));
                }
                if let Some(ms) = part.strip_prefix(b"CYCLEMS=").and_then(strict_u64) {
                    cycle_ms = ms.min(60_000);
                }
            }
            if let Some(n) = restart {
                self.0.reactivate_ms.store(cycle_ms, Ordering::Relaxed);
                self.0.restart_to.store(n + 1, Ordering::Relaxed);
                self.0.reactivate_asked.store(true, Ordering::Release);
            }
            // `SLOWSET=<ms>` at the start or right after a ';' sleeps first (each one, in order).
            for at in (0..blob.len()).filter(|&i| blob[i..].starts_with(b"SLOWSET=")) {
                if at != 0 && blob[at - 1] != b';' {
                    continue;
                }
                let v = &blob[at + 8..];
                let v = &v[..v.iter().position(|b| *b == b';').unwrap_or(v.len())];
                if let Some(ms) = strict_u64(v) {
                    std::thread::sleep(Duration::from_millis(ms.min(60_000)));
                }
            }
            for part in blob.split(|b| *b == b';') {
                if let Some(g) = part.strip_prefix(b"gain=").and_then(strict_f32) {
                    self.0.params[0].store(g.to_bits(), Ordering::Relaxed);
                }
                // `p<id>=<v>`, an `xparams` value (what a host set writes).
                if let Some((id, v)) = part.strip_prefix(b"p").and_then(|r| {
                    let eq = r.iter().position(|b| *b == b'=')?;
                    Some((strict_u64(&r[..eq])?, strict_f32(&r[eq + 1..])?))
                }) && let Some(x) = id
                    .checked_sub(u64::from(XPARAM_ID0))
                    .and_then(|j| self.0.xvals.get(j as usize))
                {
                    x.store(v.to_bits(), Ordering::Relaxed);
                }
                if let Some(ms) = part.strip_prefix(b"SLOWGET=").and_then(strict_u64) {
                    self.0.slow_get_ms.store(ms, Ordering::Relaxed);
                }
                if let Some(ms) = part.strip_prefix(b"BLOCKMS=").and_then(strict_u64) {
                    self.0
                        .block_once_ms
                        .store(ms.min(60_000), Ordering::Relaxed);
                }
                if let Some(n) = part.strip_prefix(b"BIGGET=").and_then(strict_u64) {
                    self.0.big_get.store(n.min(128 << 20), Ordering::Relaxed);
                }
                if part == b"BAREGET" {
                    self.0.bare_get.store(true, Ordering::Relaxed);
                }
                if let Some(l) = part.strip_prefix(b"latency=").and_then(strict_u64) {
                    self.0
                        .latency
                        .store(l.min(MAX_LINE) as u32, Ordering::Relaxed);
                }
            }
            *self.0.state.lock().unwrap() = blob.to_vec();
            if blob.starts_with(b"FAIL") {
                return Err("the test double refuses this state".into());
            }
            Ok(())
        };
        why().map_err(summoner_proc::Error::from)
    }

    fn get_state(&self) -> Vec<u8> {
        let ms = self.0.slow_get_ms.load(Ordering::Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        let mut s = self.0.state.lock().unwrap().clone();
        let big = self.0.big_get.load(Ordering::Relaxed) as usize;
        if big > s.len() {
            s.resize(big, 0);
        }
        s
    }

    fn poll(&self) -> Notice {
        if self.0.reactivate_asked.swap(false, Ordering::AcqRel) {
            Notice::Reactivate
        } else {
            Notice::None
        }
    }

    /// No window in the test double: opening its "editor" is a user edit that moves the state.
    fn open_editor(&self) -> bool {
        self.0.state.lock().unwrap().extend_from_slice(b";edited");
        true
    }

    /// Fresh lines at each prepare; the note state stays on the plugin.
    fn prepare(&self, _rate: f64, _max_frames: usize) -> Box<dyn Dsp> {
        let n = self.0.cfg.delay.max(1);
        let m = self.0.cfg.echo.max(1);
        Box::new(TestDsp {
            plugin: self.0.clone(),
            delay: self.0.cfg.delay,
            line: [summoner_proc::rt_line(n), summoner_proc::rt_line(n)],
            pos: 0,
            echo: [summoner_proc::rt_line(m), summoner_proc::rt_line(m)],
            epos: 0,
        })
    }
}

/// The chain-thread side: the lines, and the plugin's shared half for its parameters and note state.
struct TestDsp {
    plugin: Arc<Inner>,
    /// The delay this processor applies (frames): the description's, or a `RESTART=<n>` cycle's.
    delay: usize,
    /// The lines in the wired arena (made on the message thread, at prepare or a RESTART cycle).
    line: [runecraft_rt::arena::ArenaSlice<f32>; 2],
    pos: usize,
    echo: [runecraft_rt::arena::ArenaSlice<f32>; 2],
    epos: usize,
}

/// `knob=<v>` in a `;`-separated state body: replaced in place, or appended (the host application's test double's
/// rule).
fn set_knob(body: &mut Vec<u8>, knob: &str, v: f32) {
    let text = String::from_utf8_lossy(body).into_owned();
    let pre = format!("{knob}=");
    let mut parts: Vec<String> = text
        .split(';')
        .filter(|s| !s.is_empty() && !s.starts_with(&pre))
        .map(str::to_string)
        .collect();
    parts.push(format!("{pre}{v}"));
    *body = parts.join(";").into_bytes();
}

/// Makes the host's slot for a parsed description.
pub fn make(cfg: Config) -> Arc<dyn Plugin> {
    Arc::new(TestPlugin::new(cfg))
}

fn spin_for(d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until {
        std::hint::spin_loop();
    }
}

impl Dsp for TestDsp {
    /// The description's `inputs` / `outputs`.
    fn io(&self) -> summoner_proc::Io {
        self.plugin.cfg.io
    }

    /// `REACTIVATE=<ms>`: the re-activation takes that long (on the message thread, the slot out of the chain).
    fn reactivate(&mut self) -> Result<(), summoner_proc::Error> {
        // `RESTART=<n>`: the cycle brings the plugin back fresh (lines and voices) at its new delay and latency. This
        // runs on the message thread with the processor out of its cell (the reactivation handover), so allocating here
        // is the cycle's.
        let to = self.plugin.restart_to.swap(0, Ordering::Relaxed);
        if to > 0 {
            let n = (to - 1) as usize;
            self.delay = n;
            self.line = [
                summoner_proc::rt_line(n.max(1)),
                summoner_proc::rt_line(n.max(1)),
            ];
            self.pos = 0;
            let m = self.plugin.cfg.echo.max(1);
            self.echo = [summoner_proc::rt_line(m), summoner_proc::rt_line(m)];
            self.epos = 0;
            self.reset();
            self.plugin.latency.store(n as u32, Ordering::Relaxed);
        }
        let ms = self.plugin.reactivate_ms.load(Ordering::Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
        Ok(())
    }

    /// The host's `BF_RESET`: all notes off and every pedal up (the lines keep their tails).
    fn reset(&mut self) {
        let n = &self.plugin.notes;
        n.held.store(0, Ordering::Relaxed);
        n.pedal.store(0, Ordering::Relaxed);
        for s in &n.sustained {
            s.store(0, Ordering::Relaxed);
        }
    }

    fn process(&mut self, b: &mut Block<'_>) -> bool {
        let p = &*self.plugin;
        let cfg = &p.cfg;
        if p.param(1) >= 0.5 {
            summoner_sys::crash::lab_abort();
        }
        if p.param(2) >= 0.5 {
            // Hung inside the block: never returns, so the chain never acks and never answers the ping.
            loop {
                std::hint::spin_loop();
            }
        }
        let once = p.block_once_ms.swap(0, Ordering::Relaxed);
        if once > 0 {
            // A slow block that holds the cell (the host application's fake's BLOCKMS).
            std::thread::sleep(Duration::from_millis(once));
        }
        if p.throw.load(Ordering::Relaxed) {
            return false; // the host makes the block THREW and silent
        }
        let burn_us = (p.param(3).clamp(0.0, 1.0) * 100_000.0) as u32;
        if burn_us > 0 {
            spin_for(Duration::from_micros(burn_us as u64));
        }
        let n = b.frames;
        // `write` says which channels the block touches.
        let ch = match cfg.write {
            Write::None => return true,
            Write::Ch0 => b.channels.len().min(1),
            Write::All | Write::Add => b.channels.len().min(2),
        };
        let gain = p.param(0);
        let delay = self.delay;
        let notes = &p.notes;
        let mut held = notes.held.load(Ordering::Relaxed);
        let mut pedal = notes.pedal.load(Ordering::Relaxed);
        let mut sustained = [0u32; 16];
        for (s, a) in sustained.iter_mut().zip(&notes.sustained) {
            *s = a.load(Ordering::Relaxed);
        }
        let mut mi = 0;
        for i in 0..n {
            if cfg.synth {
                while mi < b.midi.len() && b.midi[mi].offset as usize <= i {
                    let e = &b.midi[mi];
                    let (s, d1, d2) = (e.byte(0), e.byte(1), e.byte(2));
                    let chn = (s & 0x0f) as usize;
                    match s & 0xf0 {
                        0x90 if d2 > 0 => held += 1,
                        0x80 | 0x90 if cfg.sustain && (pedal >> chn) & 1 != 0 => {
                            sustained[chn] += 1
                        }
                        0x80 | 0x90 => held = held.saturating_sub(1),
                        0xb0 if (d1 == 123 || d1 == 120) && !cfg.ignore_cc123 => {
                            held = 0;
                            sustained = [0; 16];
                        }
                        0xb0 if d1 == 64 && cfg.sustain => {
                            if d2 >= 64 {
                                pedal |= 1 << chn;
                            } else {
                                pedal &= !(1 << chn);
                                held = held.saturating_sub(sustained[chn]);
                                sustained[chn] = 0;
                            }
                        }
                        _ => {}
                    }
                    mi += 1;
                }
                let v = if held > 0 { 0.25 } else { 0.0 };
                for c in b.channels.iter_mut().take(ch) {
                    if cfg.write == Write::Add {
                        c[i] += v;
                    } else {
                        c[i] = v;
                    }
                }
            }
            for (c, buf) in b.channels.iter_mut().take(ch).enumerate() {
                let mut x = buf[i] * gain;
                if cfg.echo > 0 {
                    // The decaying delay: an explicit fma, as the host application's test double does it.
                    let mut y = cfg.feedback.mul_add(self.echo[c][self.epos], x);
                    if y.abs() < 1e-30 {
                        y = 0.0;
                    }
                    self.echo[c][self.epos] = y;
                    x = y;
                }
                buf[i] = if delay > 0 {
                    let y = self.line[c][self.pos];
                    self.line[c][self.pos] = x;
                    y
                } else {
                    x
                };
            }
            if delay > 0 {
                self.pos = (self.pos + 1) % delay;
            }
            if cfg.echo > 0 {
                self.epos = (self.epos + 1) % cfg.echo;
            }
        }
        notes.held.store(held, Ordering::Relaxed);
        notes.pedal.store(pedal, Ordering::Relaxed);
        for (a, s) in notes.sustained.iter().zip(sustained) {
            a.store(s, Ordering::Relaxed);
        }
        if cfg.probe {
            let t = b.transport;
            let f = t.flags;
            let (num, den) = (t.time_sig & 0xffff, t.time_sig >> 16);
            let loop_valid = f & BF_LOOP_POINTS_VALID != 0;
            let bar_valid = f & BF_BAR_START_VALID != 0;
            // What a play head built from the request line reports: fields it
            // does not carry read as 0; the flags are rebuilt from what it carries.
            let flags = (f & (BF_PLAYING | BF_LOOPING | BF_RECORDING))
                | if loop_valid { BF_LOOP_POINTS_VALID } else { 0 }
                | if bar_valid { BF_BAR_START_VALID } else { 0 };
            let probe: [f64; 11] = [
                t.ppq,
                if t.bpm > 0.0 { t.bpm } else { 0.0 },
                if num != 0 && den != 0 {
                    num as f64
                } else {
                    0.0
                },
                if num != 0 && den != 0 {
                    den as f64
                } else {
                    0.0
                },
                if loop_valid { t.loop_start_ppq } else { 0.0 },
                if loop_valid { t.loop_end_ppq } else { 0.0 },
                if bar_valid { t.bar_start_ppq } else { 0.0 },
                flags as f64,
                t.time_seconds,
                t.time_samples as i64 as f64,
                t.output_host_time_ns as i64 as f64,
            ];
            for (c, buf) in b.channels.iter_mut().take(ch).enumerate() {
                for (i, x) in buf.iter_mut().enumerate().take(n) {
                    *x = if c == 0 && i < probe.len() {
                        probe[i] as f32
                    } else {
                        0.0
                    };
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(knobs: &str, slot: usize) -> Option<Config> {
        let r: summoner_proto::plugin::PluginRef =
            serde_json::from_str(&format!(r#"{{"format":"fake","fake":{knobs}}}"#)).unwrap();
        Config::from_ref(&r, slot)
    }

    #[test]
    fn the_knobs_configure_the_double() {
        let c = cfg(r#"{"name":"half","gain":"0.5","delay":"3"}"#, 2).unwrap();
        assert_eq!((c.name.as_str(), c.gain, c.delay), ("half", 0.5, 3));
        let c = cfg(r#"{"synth":"1","ignore-cc123":"1","sustain":"1"}"#, 4).unwrap();
        assert_eq!(c.name, "fake4");
        assert!(c.synth && c.ignore_cc123 && c.sustain && !c.probe);
        let c = cfg(r#"{"echo":"4800","feedback":"0.97"}"#, 0).unwrap();
        assert_eq!((c.echo, c.feedback), (4800, 0.97));
        // Strict numbers: a malformed value is the default, not a prefix.
        let c = cfg(r#"{"gain":"0.5x","delay":"+3","echo":"9999999999"}"#, 0).unwrap();
        assert_eq!((c.gain, c.delay, c.echo), (1.0, 0, 1 << 20));
        assert_eq!(cfg(r#"{"name":"a & b"}"#, 0).unwrap().name, "a & b");
    }

    /// `inputs` / `outputs` are what `io()` reports (default 2 / 2, strict numbers, at most 64) and `write`
    /// picks the processor's writes.
    #[test]
    fn the_io_and_write_knobs_parse() {
        let io = |x: &str| cfg(x, 0).unwrap().io;
        assert_eq!(io("{}"), summoner_proc::Io::STEREO);
        assert_eq!(
            io(r#"{"inputs":"0","outputs":"1"}"#),
            summoner_proc::Io::new(0, 1)
        );
        assert_eq!(
            io(r#"{"inputs":"1x","outputs":"999"}"#),
            summoner_proc::Io::new(2, 64)
        );
        let w = |x: &str| cfg(x, 0).unwrap().write;
        assert_eq!(w("{}"), Write::All);
        assert_eq!(w(r#"{"write":"ch0"}"#), Write::Ch0);
        assert_eq!(w(r#"{"write":"add"}"#), Write::Add);
        assert_eq!(w(r#"{"write":"none"}"#), Write::None);
        assert_eq!(w(r#"{"write":"bogus"}"#), Write::All);
        let p = TestPlugin::new(cfg(r#"{"inputs":"0","outputs":"1"}"#, 0).unwrap());
        assert_eq!(p.prepare(48_000.0, 8).io(), summoner_proc::Io::new(0, 1));
    }

    /// One block of 4 frames over `[0.1, 0.2]` with a note on at frame 0 through `knobs`' processor: its two channels.
    fn one_block(knobs: &str) -> [Vec<f32>; 2] {
        let p = TestPlugin::new(cfg(knobs, 0).unwrap());
        let mut d = p.prepare(48_000.0, 4);
        let mut ch = [
            runecraft_rt::arena::ArenaSlice::filled(summoner_proc::RT_OWNER, 4, 0.1f32),
            runecraft_rt::arena::ArenaSlice::filled(summoner_proc::RT_OWNER, 4, 0.2f32),
        ];
        let midi = [summoner_proc::MidiEvent::short(0, [0x90, 60, 100])];
        assert!(d.process(&mut Block {
            frames: 4,
            channels: &mut ch,
            midi: &midi,
            transport: &summoner_proc::Transport::default(),
        }));
        [ch[0].to_vec(), ch[1].to_vec()]
    }

    #[test]
    fn write_picks_what_the_synth_writes() {
        assert_eq!(
            one_block(r#"{"synth":"1"}"#),
            [vec![0.25; 4], vec![0.25; 4]]
        );
        assert_eq!(
            one_block(r#"{"synth":"1","write":"ch0"}"#),
            [vec![0.25; 4], vec![0.2; 4]],
            "channel 1 as handed in"
        );
        assert_eq!(
            one_block(r#"{"synth":"1","write":"add"}"#),
            [vec![0.1 + 0.25; 4], vec![0.2 + 0.25; 4]],
            "accumulated"
        );
        assert_eq!(
            one_block(r#"{"synth":"1","write":"none"}"#),
            [vec![0.1; 4], vec![0.2; 4]],
            "untouched"
        );
        assert_eq!(
            one_block(r#"{"gain":"0.5","write":"ch0"}"#),
            [vec![0.05; 4], vec![0.2; 4]],
            "an effect writes channel 0 only"
        );
    }

    #[test]
    fn states_set_the_gain_and_are_returned_byte_for_byte() {
        let p = TestPlugin::new(cfg("{}", 0).unwrap());
        assert!(p.set_state(b"gain=0.25;SLOWGET=0").is_ok());
        assert_eq!(p.0.param(0), 0.25);
        assert_eq!(p.get_state(), b"gain=0.25;SLOWGET=0");
        assert!(p.set_state(b"FAIL;gain=2").is_err());
        assert_eq!(p.0.param(0), 2.0, "a refused state is still applied");
        assert!(p.open_editor());
        assert_eq!(p.get_state(), b"FAIL;gain=2;edited");
    }
}
