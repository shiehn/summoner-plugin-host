//! `summoner-format-au`: AU v2 slots for the host child, loaded only from the lab's own bundles, registered
//! in-process.
//!
//! Audio Unit v2 hosting (AudioToolbox's `AudioUnitGetProperty` / `AudioUnitRender`), over `summoner-sys`'s
//! AudioToolbox handles:
//! - parameters: `ParameterList` order, ids whose info fails skipped; a value `v` → `min + range * v` through
//!   `AudioUnitSetParameter` on the chain thread before the render. A repeated id is still reported (count, name),
//!   but its values are dropped and counted;
//! - MIDI: only to music devices / effects / MIDI processors (the kinds that take MIDI): up to 3 bytes through
//!   `MusicDeviceMIDIEvent`, longer through `MusicDeviceSysEx`;
//! - latency: `round(seconds * rate)`, 0 until prepared; re-read on the message thread after prepare and state, and
//!   when the listener's notice is set. An AU that refused the listener is re-read on every periodic wake instead;
//! - state: `ClassInfo` as a binary plist; a refused state is `ok: false`;
//! - a failed render (a status, or a rewritten output list) is THREW and silence;
//! - the host's `BF_RESET` does nothing here: an AU instance has no `reset` of its own.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod desc;
mod params;

use desc::Description;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use summoner_proc::{Block, Chunks, Dsp, Notice, ParamMeta, Plugin, StateKind};
use summoner_proto::layout::{BF_BAR_START_VALID, BF_LOOP_POINTS_VALID, BF_LOOPING, BF_PLAYING};
use summoner_sys::au::{self as sys, Component, Instance, ParamInfo, Processor};
use summoner_sys::bundle::{self, Bundle, Format};

/// The lab's chains are stereo; a bus here carries at most this many channels.
const MAX_CH: usize = 2;

/// Resolves a slot (protocol 4.0): the bundle by its absolute path, through the loading policy, registered
/// in-process (never the system registry), and the ONE `AudioComponents` entry whose three codes are the slot's
/// (never the first entry, never an XOR id, which two entries can share).
fn resolve(d: &Description) -> Result<(Component, String), String> {
    desc::check_codes(d.codes).map_err(|why| format!("{}: {why}", d.file))?;
    let path = Path::new(&d.file);
    let b = Bundle::check(path, Format::Au)
        .map_err(|r| format!("{}: {}: {r}", bundle::REFUSED, d.file))?;
    let reg = sys::register(&b).map_err(|e| format!("{}: {e:?}", d.file))?;
    let codes: Vec<[u32; 3]> = reg
        .components
        .iter()
        .map(|(c, _)| desc::codes_of(&c.desc))
        .collect();
    pick_by_triple(&codes, d.codes)
        .map(|i| reg.components[i].clone())
        .map_err(|why| format!("{}: {why}", d.file))
}

/// The index of the ONE entry whose codes are `t`. None: "no AudioComponents entry …";
/// more than one: refused, since which of them the bundle meant cannot be known (never the first).
pub fn pick_by_triple(codes: &[[u32; 3]], t: [u32; 3]) -> Result<usize, crate::Error> {
    pick_by_triple_why(codes, t).map_err(crate::Error::from)
}

fn pick_by_triple_why(codes: &[[u32; 3]], t: [u32; 3]) -> Result<usize, String> {
    let triple = format!("{:08x}-{:08x}-{:08x}", t[0], t[1], t[2]);
    let mut hits = codes
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == t)
        .map(|(i, _)| i);
    match (hits.next(), hits.next()) {
        (Some(i), None) => Ok(i),
        (None, _) => Err(format!("no AudioComponents entry {triple}")),
        (Some(a), Some(b)) => Err(format!(
            "AudioComponents entries {a} and {b} (at least) both have the codes {triple}: ambiguous, refused"
        )),
    }
}

struct Param {
    info: ParamInfo,
    /// False for an id an earlier index already named.
    valid: bool,
}

struct Pending {
    value: AtomicU32,
    /// For the next render (chain thread).
    dirty: AtomicBool,
    /// For the AU's listeners (its editor's controls): the message thread's poll notifies them.
    ui_dirty: AtomicBool,
}

struct Inner {
    inst: Mutex<Instance>,
    name: String,
    desc: sys::Desc,
    params: Vec<Param>,
    /// The names `ParamList` answers with, by index: the loaded ones, re-read for the valid ids after each
    /// `ParameterList` notice (names and labels of the ids it has, never a re-index).
    names: Mutex<Vec<String>>,
    pending: Box<[Pending]>,
    /// Every index's `param.list` description (protocol 3.7), made at load.
    metas: Vec<ParamMeta>,
    dropped: AtomicU64,
    latency: AtomicU32,
    /// The prepared rate's bits (0: not prepared).
    rate: AtomicU64,
    ch_in: u32,
    /// A music device (`aumu` that wants MIDI): the one kind retried with no inputs when it refuses them.
    music_device: bool,
    /// `inputsRefused`: the last prepare refused the inputs offered and was retried with none.
    inputs_refused: AtomicBool,
    /// The input lines the last prepare wired (the processor's effective inputs; the shim's input buffer is sized
    /// alike): 0 for a unit without an input bus.
    input_lines: AtomicU32,
    ch_out: u32,
}

/// A loaded AU slot.
pub struct AuPlugin(Arc<Inner>);

/// Loads a slot for a chain of `ch_in` / `ch_out` channels (only from the lab's own bundles).
pub fn load(d: &Description, ch_in: u32, ch_out: u32) -> Result<AuPlugin, crate::Error> {
    load_why(d, ch_in, ch_out).map_err(crate::Error::from)
}

fn load_why(d: &Description, ch_in: u32, ch_out: u32) -> Result<AuPlugin, String> {
    let (comp, full_name) = resolve(d)?;
    let inst = comp
        .instantiate()
        .map_err(|e| format!("{}: {e:?}", d.file))?;
    if !inst.listening() {
        eprintln!(
            "[summoner-au] {full_name}: the AU refused the property listener; its latency is re-read every wake"
        );
    }
    if !inst.host_callbacks() {
        eprintln!(
            "[summoner-au] {full_name}: the AU refused kAudioUnitProperty_HostCallbacks; it gets no beat, tempo or \
         transport"
        );
    }
    // Validity is the first occurrence of each id (`params::first_occurrences`).
    let infos = inst.params();
    let valid = params::first_occurrences(&infos);
    let params: Vec<Param> = infos
        .into_iter()
        .zip(valid)
        .map(|(info, valid)| Param { info, valid })
        .collect();
    // The AU's own edits (its editor's AUParameterSet) move childGen, so the host application sees a plugin-side edit:
    // listened for on the main run loop, every valid parameter.
    let ids: Vec<u32> = params
        .iter()
        .filter(|p| p.valid)
        .map(|p| p.info.id)
        .collect();
    if !inst.listen_for_edits(&ids) {
        eprintln!(
            "[summoner-au] {}: no edit listener (its editor's edits will not move childGen)",
            d.file
        );
    }
    let pending = params
        .iter()
        .map(|_| Pending {
            value: AtomicU32::new(0),
            dirty: AtomicBool::new(false),
            ui_dirty: AtomicBool::new(false),
        })
        .collect();
    let metas = param_metas(&params);
    // A music device is offered the chain's inputs like any AU (it was once forced to 0): the prepare reports what
    // it really takes (`Processor::channels`: 0 without an input bus). The unit's input elements are counted whatever
    // its type, so an aumu with an input bus hears the clips. The mutant restores the old forcing.
    let music = cfg!(mutant_aumu_no_input)
        && desc::wants_midi(&comp.desc)
        && comp.desc.kind == sys::TYPE_MUSIC_DEVICE;
    Ok(AuPlugin(Arc::new(Inner {
        name: desc::name_and_manufacturer(&full_name).0,
        desc: comp.desc,
        inst: Mutex::new(inst),
        names: Mutex::new(params.iter().map(|p| p.info.name.clone()).collect()),
        params,
        pending,
        metas,
        dropped: AtomicU64::new(0),
        latency: AtomicU32::new(0),
        rate: AtomicU64::new(0),
        music_device: desc::wants_midi(&comp.desc) && comp.desc.kind == sys::TYPE_MUSIC_DEVICE,
        inputs_refused: AtomicBool::new(false),
        input_lines: AtomicU32::new(0),
        ch_in: if music {
            0
        } else {
            ch_in.clamp(1, MAX_CH as u32)
        },
        ch_out: ch_out.clamp(1, MAX_CH as u32),
    })))
}

/// Each index's 3.7 description: `params::au_param_meta`, its `order` from the clump order of
/// every listed index (valid or not).
fn param_metas(params: &[Param]) -> Vec<ParamMeta> {
    let clumps: Vec<Option<u32>> = params.iter().map(|p| p.info.clump).collect();
    let order = summoner_plugin::params::au_clump_order(&clumps);
    params
        .iter()
        .zip(order)
        .map(|(p, order)| params::au_param_meta(&p.info, order))
        .collect()
}

/// A plain AU value normalized by the parameter's range (0 for an empty range), clamped.
fn normalized(info: &ParamInfo, v: f32) -> f32 {
    let range = info.max - info.min;
    if range > 0.0 && range.is_finite() {
        let n = (v - info.min) / range;
        if n.is_finite() {
            n.clamp(0.0, 1.0)
        } else {
            0.0
        }
    } else {
        0.0
    }
}

impl Inner {
    /// The latency in frames: `round(seconds * rate)`, 0 while unprepared. Returns whether it
    /// moved.
    fn reread_latency(&self, inst: &Instance) -> bool {
        let rate = f64::from_bits(self.rate.load(Ordering::Relaxed));
        let frames = if rate > 0.0 {
            (inst.latency_seconds() * rate)
                .round()
                .clamp(0.0, f64::from(u32::MAX)) as u32
        } else {
            0
        };
        self.latency.swap(frames, Ordering::Relaxed) != frames
    }
}

impl AuPlugin {
    /// The loaded component's codes in their wire form (`auCodes`).
    pub fn codes(&self) -> String {
        desc::codes_string(&self.0.desc)
    }

    /// `AudioUnitGetParameter` of the raw id (the probe reads the hostile AU's report parameters this way).
    pub fn raw_param(&self, id: u32) -> Option<f32> {
        self.0.inst.lock().unwrap().param(id).ok()
    }

    /// `inputsRefused`: whether the last prepare of this music device refused the inputs offered and was
    /// prepared with none (it plays, deaf to the clips). The bridge carries it ([`Plugin::notices`]).
    pub fn inputs_refused(&self) -> bool {
        self.0.inputs_refused.load(Ordering::Relaxed)
    }

    /// The input lines the last prepare wired for the chain thread (0 for a unit without an input bus).
    pub fn input_lines(&self) -> u32 {
        self.0.input_lines.load(Ordering::Relaxed)
    }

    /// Whether the AU accepted the property listener (one that refused it is re-read on every wake).
    pub fn listening(&self) -> bool {
        self.0.inst.lock().unwrap().listening()
    }
}

impl Plugin for AuPlugin {
    fn edits(&self) -> u64 {
        self.0.inst.lock().unwrap().edit_count()
    }

    fn editor_source(&self) -> summoner_proc::EditorSource {
        summoner_proc::EditorSource::AuUnit(summoner_proc::AuUnit::from_raw(
            self.0.inst.lock().unwrap().unit_handle(),
        ))
    }

    fn name(&self) -> &str {
        &self.0.name
    }

    fn latency(&self) -> u32 {
        self.0.latency.load(Ordering::Relaxed)
    }

    fn param_count(&self) -> u32 {
        self.0.params.len() as u32
    }

    /// `inputsRefused` after a prepare that refused the inputs offered (and was retried with none). `--cfg
    /// mutant_child_no_notice` (never in a normal build) names none; the au_io and host notice tests fail.
    fn notices(&self) -> Vec<String> {
        if self.inputs_refused() && !cfg!(mutant_child_no_notice) {
            vec!["inputsRefused".to_string()]
        } else {
            Vec::new()
        }
    }

    fn param_name(&self, index: u32) -> String {
        self.0
            .names
            .lock()
            .unwrap()
            .get(index as usize)
            .cloned()
            .unwrap_or_default()
    }

    fn set_param(&self, index: u32, value: f32) {
        let i = index as usize;
        match (self.0.params.get(i), self.0.pending.get(i)) {
            (Some(Param { valid: true, .. }), Some(p)) => {
                p.value.store(value.to_bits(), Ordering::Relaxed);
                p.dirty.store(true, Ordering::Release);
                p.ui_dirty.store(true, Ordering::Release);
            }
            _ => {
                self.0.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn dropped_params(&self) -> u64 {
        self.0.dropped.load(Ordering::Relaxed)
    }

    fn param_meta(&self, index: u32) -> ParamMeta {
        self.0
            .metas
            .get(index as usize)
            .cloned()
            .unwrap_or_default()
    }

    /// An unapplied region value, else the unit's own (`AudioUnitGetParameter`), normalized.
    fn param_value(&self, index: u32) -> f32 {
        let i = index as usize;
        let (Some(param), Some(p)) = (self.0.params.get(i), self.0.pending.get(i)) else {
            return 0.0;
        };
        if p.dirty.load(Ordering::Acquire) {
            return f32::from_bits(p.value.load(Ordering::Relaxed)).clamp(0.0, 1.0);
        }
        let v = self.0.inst.lock().unwrap().param(param.info.id);
        v.map_or(0.0, |v| normalized(&param.info, v))
    }

    /// A value from the host: `AudioUnitSetParameter` of
    /// `min + range × v` on the calling (message) thread, and the AU's listeners told at the next poll. The unit holds
    /// it at once, so its ClassInfo (the state a capture reads) does too.
    fn host_set(&self, plugin_id: u32, value: f32) -> Result<f32, summoner_proc::Error> {
        let why = || -> Result<f32, String> {
            let index = self
                .0
                .params
                .iter()
                .position(|p| p.valid && p.info.id == plugin_id)
                .ok_or_else(|| format!("no parameter {plugin_id}"))?;
            let info = &self.0.params[index].info;
            let plain = info.min + (info.max - info.min) * value;
            self.0
                .inst
                .lock()
                .unwrap()
                .set_param(plugin_id, plain)
                .map_err(|e| format!("AudioUnitSetParameter: {e:?}"))?;
            if let Some(p) = self.0.pending.get(index) {
                p.ui_dirty.store(true, Ordering::Release);
            }
            Ok(self.param_value(index as u32))
        };
        why().map_err(summoner_proc::Error::from)
    }

    fn set_state(&self, blob: &[u8]) -> Result<(), summoner_proc::Error> {
        let why = || -> Result<(), String> {
            let inst = self.0.inst.lock().unwrap();
            let r = inst.set_class_info(blob).map_err(|e| format!("{e:?}"));
            self.0.reread_latency(&inst);
            r
        };
        why().map_err(summoner_proc::Error::from)
    }

    fn get_state(&self) -> Vec<u8> {
        self.0.inst.lock().unwrap().class_info().unwrap_or_default()
    }

    fn state_kind(&self) -> StateKind {
        StateKind::Au
    }

    /// The state blob's `au` format: the ClassInfo binary plist; no plist, no state (the host answers an error).
    fn get_state_chunks(&self) -> Result<Chunks, summoner_proc::Error> {
        let why = || -> Result<Chunks, String> {
            let plist = self
                .0
                .inst
                .lock()
                .unwrap()
                .class_info()
                .map_err(|e| format!("{e:?}"))?;
            Ok(Chunks::new(Some(plist), None))
        };
        why().map_err(summoner_proc::Error::from)
    }

    /// The listener's notice (no AU call), or every wake for an AU that refused the listener; then the
    /// latency is re-read on this (the message) thread.
    fn poll(&self) -> Notice {
        let inst = self.0.inst.lock().unwrap();
        // The host's values since the last poll, to the AU's listeners (its editor's controls follow automation): one
        // notice per AudioUnitSetParameter, sent at the poll, never on the chain thread.
        for (p, param) in self.0.pending.iter().zip(&self.0.params) {
            if param.valid && p.ui_dirty.swap(false, Ordering::Acquire) {
                inst.notify_param_changed(param.info.id);
            }
        }
        let (latency, list) = inst.take_notices();
        if list {
            // A `ParameterList` notice: the ids loaded keep their indices (nothing is added, removed or moved); each
            // valid id's name is read again, one whose info now fails keeps its name. Repeated ids keep theirs (only
            // an id's first occurrence is valid).
            let mut names = self.0.names.lock().unwrap();
            let loaded: Vec<(u32, bool)> =
                self.0.params.iter().map(|p| (p.info.id, p.valid)).collect();
            params::refresh_names(&mut names, &loaded, |id| inst.param_info(id));
        }
        if (latency || !inst.listening()) && self.0.reread_latency(&inst) {
            Notice::Latency
        } else {
            Notice::None
        }
    }

    fn prepare(&self, rate: f64, max_frames: usize) -> Box<dyn Dsp> {
        self.prepare_mode(rate, max_frames, false)
    }

    fn prepare_mode(&self, rate: f64, max_frames: usize, offline: bool) -> Box<dyn Dsp> {
        let i = &self.0;
        let mut inst = i.inst.lock().unwrap();
        i.inputs_refused.store(false, Ordering::Relaxed);
        let mut first = inst.prepare_mode(rate, max_frames as u32, i.ch_in, i.ch_out, offline);
        // A music device that refuses the inputs offered (an OSStatus from AudioToolbox: an input format it does not
        // take, say) is prepared once more with none, so an instrument never goes silent for its input bus. Named
        // `inputsRefused` (the log, `inputs_refused` and the slot's notices). Only an OSStatus: a refusal before
        // AudioToolbox or of the image checks is final.
        // Effects are unchanged.
        if i.music_device
            && i.ch_in > 0
            && matches!(first, Err(sys::Error::Status(_)))
            && !cfg!(mutant_no_input_fallback)
        {
            let why = first
                .as_ref()
                .err()
                .map(|e| format!("{e:?}"))
                .unwrap_or_default();
            first = inst.prepare_mode(rate, max_frames as u32, 0, i.ch_out, offline);
            if first.is_ok() {
                i.inputs_refused.store(true, Ordering::Relaxed);
                eprintln!(
                    "[summoner-au] {}: inputsRefused: the music device refused {} inputs ({why}); prepared with none",
                    i.name, i.ch_in
                );
                runecraft_log::log!(
                    summoner_proto::log_vocab::PLUGIN,
                    runecraft_log::Level::Warn,
                    "plugin.inputsRefused",
                    inputs = u64::from(i.ch_in),
                );
            }
        }
        let processor = first
            .map_err(|e| eprintln!("[summoner-au] {}: prepare: {e:?}", i.name))
            .ok();
        i.rate.store(
            if processor.is_some() {
                rate.to_bits()
            } else {
                0
            },
            Ordering::Relaxed,
        );
        i.reread_latency(&inst);
        drop(inst);
        // A failed prepare (no processor) keeps the requested lines: its blocks are THREW before they are read.
        let n_in = processor.as_ref().map_or(i.ch_in, |p| p.channels().0) as usize;
        i.input_lines.store(n_in as u32, Ordering::Relaxed);
        Box::new(AuDsp {
            processor,
            inner: i.clone(),
            // In runecraft-rt's wired arena (the arena's own prefault).
            // The processor's effective inputs (0 after an inputsRefused retry); the requested ones without one.
            inputs: runecraft_rt::arena::ArenaSlice::from_fn(summoner_proc::RT_OWNER, n_in, |_| {
                summoner_proc::rt_line(max_frames)
            }),
            wants_midi: desc::wants_midi(&i.desc),
        })
    }
}

struct AuDsp {
    processor: Option<Processor>,
    inner: Arc<Inner>,
    /// The block's input, copied before the render (the outputs are written in place into the block's channels). In
    /// the wired arena (made at prepare, on the message thread).
    inputs: runecraft_rt::arena::ArenaSlice<runecraft_rt::arena::ArenaSlice<f32>>,
    wants_midi: bool,
}

impl Dsp for AuDsp {
    /// The prepared processor's channels: its effective input count, 0 when the unit has no input bus
    /// (`summoner_sys::au::Processor::channels`), never the requested `ch_in`. A failed prepare (no processor) reports
    /// none in and the requested outputs: its blocks are THREW and silent anyway.
    fn io(&self) -> summoner_proc::Io {
        let (inputs, outputs) = self
            .processor
            .as_ref()
            .map_or((0, self.inner.ch_out), Processor::channels);
        summoner_proc::Io::new(inputs, outputs)
    }

    /// An AU instance has no `reset`: the host's all-notes-off reaches a music device as MIDI.
    fn reset(&mut self) {}

    fn process(&mut self, b: &mut Block<'_>) -> bool {
        let Some(p) = self.processor.as_mut() else {
            return false; // not prepared: THREW
        };
        let inner = &self.inner;
        for (pend, param) in inner.pending.iter().zip(&inner.params) {
            if param.valid && pend.dirty.swap(false, Ordering::Acquire) {
                let v = f32::from_bits(pend.value.load(Ordering::Relaxed));
                let info = &param.info;
                p.set_param(info.id, info.min + (info.max - info.min) * v);
            }
        }
        if self.wants_midi {
            for m in b.midi {
                if m.len <= 3 {
                    p.midi(m.byte(0), m.byte(1), m.byte(2), m.offset);
                } else {
                    p.sysex(&m.data[..usize::from(m.len).min(m.data.len())]);
                }
            }
        }
        let frames = b.frames;
        for (dst, src) in self.inputs.iter_mut().zip(b.channels.iter()) {
            dst[..frames].copy_from_slice(&src[..frames]);
        }
        let ins: [&[f32]; MAX_CH] =
            std::array::from_fn(|c| self.inputs.get(c).map_or(&[][..], |v| &v[..frames]));
        let n_in = self.inputs.len();
        let n_out = (inner.ch_out as usize).min(b.channels.len());
        let mut outs = b.channels.iter_mut().take(n_out).map(|v| &mut v[..frames]);
        let mut o: [&mut [f32]; MAX_CH] = std::array::from_fn(|_| outs.next().unwrap_or(&mut []));
        let host_time = (b.transport.output_host_time != 0).then_some(b.transport.output_host_time);
        // The HostCallbacks' answers for this block: the request line's transport, the values VST3's
        // ProcessContext gets, published before the render (atomics only: no allocation, no lock).
        let t = b.transport;
        p.publish_transport(&sys::HostTransport {
            ppq: t.ppq,
            bpm: t.bpm,
            time_sig_num: t.time_sig & 0xffff,
            time_sig_den: t.time_sig >> 16,
            bar_start_ppq: t.bar_start_ppq,
            bar_valid: t.flags & BF_BAR_START_VALID != 0,
            loop_start_ppq: t.loop_start_ppq,
            loop_end_ppq: t.loop_end_ppq,
            loop_valid: t.flags & BF_LOOP_POINTS_VALID != 0,
            playing: t.flags & BF_PLAYING != 0,
            looping: t.flags & BF_LOOPING != 0,
            time_samples: t.time_samples,
        });
        p.render(&ins[..n_in], &mut o[..n_out], frames, host_time)
            .is_ok()
    }
}

/// Pins of the description derivation over hand-built parameter infos, one per
/// label row and edge, black box over `param_metas` (no AU needed).
#[cfg(test)]
mod cleanroom_c04_tests {
    use super::{Param, ParamInfo, param_metas};

    fn info(id: u32, unit: u32, min: f32, max: f32, default: f32, unit_name: &str) -> ParamInfo {
        ParamInfo {
            id,
            name: format!("p{id}"),
            min,
            max,
            default,
            flags: 0,
            unit,
            clump: None,
            unit_name: unit_name.into(),
        }
    }

    fn metas(infos: Vec<ParamInfo>) -> Vec<summoner_proc::ParamMeta> {
        let params: Vec<Param> = infos
            .into_iter()
            .map(|info| Param { info, valid: true })
            .collect();
        param_metas(&params)
    }

    #[test]
    fn every_label_row_and_no_other_unit_has_a_label() {
        let rows = [
            (3, "%"),
            (4, "s"),
            (8, "Hz"),
            (13, "dB"),
            (24, "ms"),
            (26, "drv"),
        ];
        for (unit, want) in rows {
            let m = metas(vec![info(0, unit, 0.0, 1.0, 0.5, "drv")]);
            assert_eq!(m[0].label, want, "unit {unit}");
        }
        for unit in (0..40).filter(|u| ![3, 4, 8, 13, 24, 26].contains(u)) {
            let m = metas(vec![info(0, unit, 0.0, 1.0, 0.5, "drv")]);
            assert_eq!(m[0].label, "", "unit {unit} has no label");
        }
        // The custom unit's name is cut at 64 characters (not bytes).
        let long = "é".repeat(70);
        let m = metas(vec![info(0, 26, 0.0, 1.0, 0.5, &long)]);
        assert_eq!(m[0].label.chars().count(), 64);
        assert_eq!(m[0].label, "é".repeat(64));
    }

    #[test]
    fn steps_for_indexed_and_boolean_only_rounded_half_away() {
        let m = metas(vec![
            info(0, 2, 0.0, 1.0, 0.0, ""),           // Boolean 0..1: 1 step
            info(1, 1, -0.5, 2.0, 0.0, ""),          // Indexed, range 2.5: round half away -> 3
            info(2, 1, 0.0, 3.0, 1.0, ""),           // Indexed 0..3: 3
            info(3, 0, 0.0, 3.0, 1.0, ""),           // Generic: 0
            info(4, 1, 0.0, f32::INFINITY, 0.0, ""), // infinite range: 0
            info(5, 1, 3.0, 1.0, 0.0, ""),           // negative range: 0
            info(6, 1, 0.0, 5e9, 0.0, ""),           // saturates at u32::MAX
        ]);
        let steps: Vec<u32> = m.iter().map(|p| p.step_count).collect();
        assert_eq!(steps, [1, 3, 3, 0, 0, 0, u32::MAX]);
    }

    #[test]
    fn defaults_normalize_and_non_finite_ones_are_zero() {
        let m = metas(vec![
            info(0, 0, 20.0, 20000.0, 1000.0, ""),
            info(1, 0, 0.0, 1.0, f32::NAN, ""),
            info(2, 0, 0.0, f32::INFINITY, 1.0, ""),
            info(3, 0, 1.0, 1.0, 1.0, ""), // empty range
            info(4, 0, 0.0, 1.0, 7.0, ""), // clamped
            info(5, 0, 0.0, 1.0, -3.0, ""),
            info(6, 0, f32::NAN, 1.0, 0.5, ""),
        ]);
        let d: Vec<u32> = m.iter().map(|p| p.default.to_bits()).collect();
        let want: Vec<u32> = [((1000.0f32 - 20.0) / 19980.0), 0.0, 0.0, 0.0, 1.0, 0.0, 0.0]
            .iter()
            .map(|x| x.to_bits())
            .collect();
        assert_eq!(d, want);
    }

    #[test]
    fn automatable_is_not_non_realtime_and_the_order_covers_every_index() {
        let mut a = info(0, 0, 0.0, 1.0, 0.0, "");
        a.flags = super::sys::FLAG_NON_REAL_TIME;
        let mut b = info(0, 0, 0.0, 1.0, 0.0, ""); // a repeated id, invalid
        b.clump = Some(7);
        let c = info(1, 0, 0.0, 1.0, 0.0, "");
        let mut d = info(2, 0, 0.0, 1.0, 0.0, "");
        d.clump = Some(7);
        let params = vec![
            Param {
                info: a,
                valid: true,
            },
            Param {
                info: b,
                valid: false,
            },
            Param {
                info: c,
                valid: true,
            },
            Param {
                info: d,
                valid: true,
            },
        ];
        let m = param_metas(&params);
        assert_eq!(
            m.iter().map(|p| p.automatable).collect::<Vec<_>>(),
            [false, true, true, true]
        );
        // The invalid repeated index opens clump 7 at its own place: root [p0, clump 7 (p1, p3), p2].
        assert_eq!(m.iter().map(|p| p.order).collect::<Vec<_>>(), [0, 1, 3, 2]);
        assert_eq!(
            m.iter().map(|p| p.plugin_id).collect::<Vec<_>>(),
            [0, 0, 1, 2]
        );
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
