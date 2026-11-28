//! `summoner-format-vst3`: VST3 slots. A real slot is a bundle and a class id ([`desc`], protocol 4.0's `PluginRef`);
//! the module is loaded by its absolute path through `summoner_sys::vst3` (the shim over the pinned SDK), the class
//! picked by its id among the Audio Module classes, and the instance wrapped as a `summoner_proc` [`Plugin`] / [`Dsp`].
//!
//! - Parameters (region values): the region's index `i` is the controller's parameter `i` (all it claims, in its
//!   order); the value is the normalized value. A value set before a block becomes a point at offset 0 of that block.
//!   A plugin's count may lie: an index whose `getParameterInfo` fails, or whose id repeats an earlier index's, is
//!   reported (named by the info's title, or empty) but never driven: a value for it is dropped and counted
//!   ([`Plugin::dropped_params`]). A failed index's zeroed `ParameterInfo` (id 0) never drives the plugin's
//!   parameter 0.
//! - MIDI: note on / off become VST3 note events (velocity / 127); a controller, channel pressure (128) and pitch bend
//!   (129) become parameter points through the plugin's `IMidiMapping` when it maps them, and are dropped when it does
//!   not (every control event is consumed by the mapping). The rest goes in as: a `kPolyPressureEvent` for poly
//!   pressure; a `kLegacyMIDICCOutEvent` for a program change (130) or an MTC quarter frame (132, the low nibble).
//!   Anything else is dropped (SysEx cannot reach a child: the host application's events are three bytes). The mapping
//!   is read at
//!   load and again whenever the plugin raises `restartComponent (kMidiCCAssignmentChanged)` (on the message thread,
//!   at the next poll).
//! - The transport fills `ProcessContext`: a field is valid when the request line says so (bpm > 0, a time signature
//!   with both parts, the loop and bar flags, a host time).
//! - **State:** the product path is RAW chunks, the component chunk and the controller chunk, which the host frames
//!   together as the state blob ([`Plugin::get_state_chunks`] / [`Plugin::set_state_chunks`]). The one-chunk
//!   [`Plugin::set_state`] / [`Plugin::get_state`] carry the **component chunk only** (the controller is synced from
//!   it, `setComponentState`); both chunks are reachable through [`Vst3Plugin::chunk`] / [`Vst3Plugin::set_chunk`].
//! - A C++ exception from the plugin's `process` is caught by the shim and returned as a failed block, which the
//!   host makes THREW and silent (no unwinding on the chain thread).
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod desc;
mod midi;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use summoner_proc::{Block, Chunks, Dsp, Notice, ParamMeta, Plugin, SetOutcome, StateKind};
use summoner_proto::layout::{
    BF_BAR_START_VALID, BF_LOOP_POINTS_VALID, BF_LOOPING, BF_PLAYING, BF_RECORDING,
};
use summoner_sys::bundle::{self, Bundle, Format};
use summoner_sys::vst3::{self as sys, Buses, Instance, Module, Note, ParamInfo, Point, Processor};

/// The widest main bus hosted: a `SpeakerArrangement` is a 64-bit mask, so no VST3 bus is wider. Each bus
/// channel gets a real buffer (the block's channel, or a spare line past the block's width): never a null pointer.
const MAX_BUS: usize = 64;
/// The block width a spare output line is sized against: the host's block is never narrower (`nbuf = max(2, ...)`).
const MIN_BLOCK: u32 = 2;
/// MIDI controllers mapped per channel: 0-127, 128 channel pressure, 129 pitch bend.
const CCS: usize = 130;
/// `IComponentHandler::restartComponent`'s latency flag (`kLatencyChanged`).
const LATENCY_CHANGED: i32 = 1 << 3;
/// `IComponentHandler::restartComponent`'s MIDI-mapping flag (`kMidiCCAssignmentChanged`).
const MIDI_CC_ASSIGNMENT_CHANGED: i32 = 1 << 5;
/// `restartComponent`'s `kReloadComponent` and `kIoChanged`: both are answered with a deactivate / activate cycle,
/// through the reactivation handover, as for a latency change (the slot is taken out of the chain for the cycle, not
/// cycled synchronously under an audio lock).
const RELOAD_COMPONENT: i32 = 1 << 0;
const IO_CHANGED: i32 = 1 << 1;
/// `restartComponent`'s `kParamTitlesChanged`: the parameters' titles are read again.
const PARAM_TITLES_CHANGED: i32 = 1 << 4;
/// VST3's `kNoParamId`: an unmapped controller in [`Inner::cc`] (never a parameter's id).
const NO_PARAM: u32 = u32::MAX;
/// Room per block for notes and for parameter points (beyond one per parameter).
const MAX_NOTES: usize = 1024 + 16;
/// The most distinct parameters a block's controller edits reach (the rest wait in the shim's ring for the next).
const MAX_EDITS: usize = 256;

/// The test hooks' `--test-latency-no-cycle`: a `kLatencyChanged` is answered with the latency re-read and reported
/// and no re-activation (so a test can see the latency move without a cycle). Never set in a product child.
static LATENCY_NO_CYCLE: AtomicBool = AtomicBool::new(false);

/// Sets [`LATENCY_NO_CYCLE`] (the host's test hooks, once, at start).
pub fn set_latency_no_cycle(on: bool) {
    LATENCY_NO_CYCLE.store(on, Ordering::Relaxed);
}

/// Modules by path, shared by the slots that load the same bundle (each instance holds its module itself).
static MODULES: Mutex<Vec<(PathBuf, Weak<Module>)>> = Mutex::new(Vec::new());

fn module(path: &Path) -> Result<Arc<Module>, String> {
    let mut all = MODULES.lock().unwrap();
    all.retain(|(_, w)| w.strong_count() > 0);
    if let Some(m) = all
        .iter()
        .find(|(p, _)| p == path)
        .and_then(|(_, w)| w.upgrade())
    {
        return Ok(m);
    }
    // The lab gate: only the lab's own bundles, checked before anything of the bundle is opened.
    let b = Bundle::check(path, Format::Vst3)
        .map_err(|r| format!("{}: {}: {r}", bundle::REFUSED, path.display()))?;
    let m = Arc::new(Module::open(&b).map_err(|e| format!("{}: {e:?}", path.display()))?);
    all.push((path.to_path_buf(), Arc::downgrade(&m)));
    Ok(m)
}

struct Param {
    info: Option<ParamInfo>,
    /// The info succeeded and its id is not an earlier index's.
    valid: bool,
}

struct Pending {
    value: AtomicU64,
    /// For the processor's next block (chain thread).
    dirty: AtomicBool,
    /// For the controller (`setParamNormalized`, message thread: [`Vst3Plugin::flush_controller`]): a value
    /// set off the message thread is cached until then.
    ctl_dirty: AtomicBool,
}

struct Inner {
    /// The message-thread side (the chain thread owns its own [`Processor`]).
    inst: Mutex<Instance>,
    /// The latency as last read on the message thread (at load, prepare and every state), so the chain thread never
    /// calls into the plugin for it.
    latency: AtomicU32,
    /// The latency the ACTIVE processor was set up with: read only at its prepare and at each re-activation (never at a
    /// state set, which may report a change the processor applies only when re-activated). A kLatencyChanged at this
    /// latency needs no cycle.
    active_latency: AtomicU32,
    name: String,
    /// Every claimed index: its info (when `getParameterInfo` succeeded) and whether it may be driven.
    params: Vec<Param>,
    /// The ids an edit may drive (every valid index's), sorted: a `performEdit` of any other id is dropped (no
    /// parameter has it).
    edit_ids: Vec<u32>,
    /// Every claimed index's title as the host reports it (`param_name`): the info's at load, read again at a
    /// `kParamTitlesChanged` (message thread only).
    titles: Mutex<Vec<String>>,
    /// Region values for indices that may not be driven, dropped (chain thread).
    dropped: AtomicU64,
    pending: Box<[Pending]>,
    /// Every claimed index's `param.list` description (protocol 3.7), made at load.
    metas: Vec<ParamMeta>,
    /// `IMidiMapping`, per channel × controller: the parameter id, or [`NO_PARAM`]. Written on the message thread (at
    /// load and at a `kMidiCCAssignmentChanged`), read on the chain thread, entry by entry: a block that meets a
    /// re-read in flight may take some controllers' new mapping and others' old one, never anything else.
    cc: Box<[AtomicU32]>,
    buses: Buses,
}

/// A loaded VST3 slot.
pub struct Vst3Plugin(Arc<Inner>);

/// The plugin's `IMidiMapping`, every channel × controller, into `cc` (message thread).
fn read_mapping(inst: &Instance, cc: &[AtomicU32]) {
    for ch in 0..16 {
        for n in 0..CCS {
            let id = inst
                .midi_mapping(ch as i16, n as i16)
                .filter(|&id| id != NO_PARAM)
                .unwrap_or(NO_PARAM);
            cc[ch * CCS + n].store(id, Ordering::Relaxed);
        }
    }
}

/// Loads a slot for a chain of `ch_in` / `ch_out` channels: the module by its absolute path, the Audio Module class
/// with the slot's class id, the instance with its main buses arranged.
pub fn load(d: &desc::Description, ch_in: u32, ch_out: u32) -> Result<Vst3Plugin, crate::Error> {
    load_why(d, ch_in, ch_out).map_err(crate::Error::from)
}

fn load_why(d: &desc::Description, ch_in: u32, ch_out: u32) -> Result<Vst3Plugin, String> {
    let path = Path::new(&d.file);
    if !path.is_absolute() {
        return Err(format!("not an absolute path: {}", d.file));
    }
    let m = module(path)?;
    let classes = m.classes();
    let i = desc::match_class(&classes, d).ok_or_else(|| {
        format!(
            "no Audio Module class of {} has the class id {} ({:?})",
            d.file,
            desc::class_id_hex(&d.class_id),
            d.name
        )
    })?;
    let mut inst =
        Module::create(&m, &classes[i].cid).map_err(|e| format!("{}: {e:?}", classes[i].name))?;
    let buses = inst
        .buses(ch_in.min(2), ch_out.min(2))
        .map_err(|e| format!("bus arrangement: {e:?}"))?;
    // A plugin that refuses the stereo arrangement keeps its own (`getBusArrangement`), possibly wider than the chain's
    // block. Hosted in its default layout, every channel a real buffer (the scratch is `max(in, out, 2)` wide), up to
    // MAX_BUS; anything wider (no conforming shim reports it) is a clean load error, never a block with a channel the
    // plugin cannot write.
    if buses.inputs as usize > MAX_BUS || buses.outputs as usize > MAX_BUS {
        return Err(format!(
            "main bus of {} in / {} out channels: wider than the {MAX_BUS} hosted",
            buses.inputs, buses.outputs
        ));
    }
    // Some plugins render differently unless setupProcessing is called once at load.
    // Compatibility values; the result is ignored on purpose (a failure is not a load error).
    let _ = inst.setup_only(44100.0, 1024);
    let mut seen = std::collections::BTreeSet::new();
    let params: Vec<Param> = inst
        .params()
        .into_iter()
        .map(|info| {
            let valid = info.as_ref().is_some_and(|p| seen.insert(p.id));
            Param { info, valid }
        })
        .collect();
    let cc: Box<[AtomicU32]> = (0..16 * CCS).map(|_| AtomicU32::new(NO_PARAM)).collect();
    read_mapping(&inst, &cc);
    let mut edit_ids: Vec<u32> = params
        .iter()
        .filter(|p| p.valid)
        .filter_map(|p| p.info.as_ref().map(|i| i.id))
        .collect();
    edit_ids.sort_unstable();
    let titles = Mutex::new(
        params
            .iter()
            .map(|p| p.info.as_ref().map(|i| i.title.clone()).unwrap_or_default())
            .collect(),
    );
    let metas = param_metas(&params, &inst.units());
    let pending = params
        .iter()
        .map(|_| Pending {
            value: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            ctl_dirty: AtomicBool::new(false),
        })
        .collect();
    Ok(Vst3Plugin(Arc::new(Inner {
        latency: AtomicU32::new(inst.latency()),
        active_latency: AtomicU32::new(u32::MAX),
        inst: Mutex::new(inst),
        name: classes[i].name.clone(),
        params,
        edit_ids,
        titles,
        dropped: AtomicU64::new(0),
        pending,
        metas,
        cc,
        buses,
    })))
}

/// Each claimed index's 3.7 description (from `IEditController::getParameterInfo` and `IUnitInfo`): automatable is
/// `kCanAutomate` alone (no hidden or read-only filter); a failed `getParameterInfo` is a zeroed info (root unit, flags
/// 0, id 0); the order is the flattened unit tree. A duplicate id keeps its flags (it is listed twice); only driving it
/// is refused.
fn param_metas(params: &[Param], units: &[(i32, i32)]) -> Vec<ParamMeta> {
    let unit_of: Vec<i32> = params
        .iter()
        .map(|p| p.info.as_ref().map_or(0, |i| i.unit_id))
        .collect();
    let order = summoner_plugin::params::vst3_unit_order(units, &unit_of);
    params
        .iter()
        .zip(order)
        .map(|(p, order)| match &p.info {
            Some(i) => ParamMeta::new(
                i.id,
                // `--cfg mutant_param_automatable_always` (never in a normal build): every parameter automatable;
                // host/tests/param_set.rs's VST3 tree test fails.
                i.flags & sys::PARAM_CAN_AUTOMATE != 0 || cfg!(mutant_param_automatable_always),
                (i.default_normalized as f32).clamp(0.0, 1.0),
                i.step_count.max(0) as u32,
                summoner_plugin::params::cut_chars(
                    i.units.clone(),
                    summoner_plugin::params::LABEL_CHARS,
                ),
                order,
            ),
            None => {
                let mut msg = ParamMeta::default();
                msg.order = order;
                msg
            }
        })
        .collect()
}

impl Vst3Plugin {
    /// The host's values set since the last flush, then the processor's output parameter changes, told to the
    /// controller (`setParamNormalized`): at every poll (the 50 ms poll) and before every state get or set. Message
    /// thread, with the instance locked.
    fn flush_controller(&self, inst: &Instance) {
        for (p, param) in self.0.pending.iter().zip(&self.0.params) {
            if let Some(info) = param.info.as_ref().filter(|_| param.valid)
                && p.ctl_dirty.swap(false, Ordering::Acquire)
            {
                let _ = inst
                    .controller_set_param(info.id, f64::from_bits(p.value.load(Ordering::Relaxed)));
            }
        }
        // Then the processor's own output changes (after the host's, so the plugin's last word wins: one value per
        // parameter).
        let _ = inst.flush_outputs();
    }

    /// The component chunk (`which` 0) or the controller chunk (1), raw.
    pub fn chunk(&self, which: i32) -> Result<Vec<u8>, sys::Error> {
        let inst = self.0.inst.lock().unwrap();
        self.flush_controller(&inst);
        inst.state(which)
    }

    /// Sets the component chunk (`which` 0) or the controller chunk (1), raw; the latency is read again after it.
    pub fn set_chunk(&self, which: i32, bytes: &[u8]) -> Result<(), sys::Error> {
        let inst = self.0.inst.lock().unwrap();
        self.flush_controller(&inst);
        let r = inst.set_state(which, bytes);
        self.0.latency.store(inst.latency(), Ordering::Relaxed);
        r
    }

    /// The main buses as arranged at load.
    pub fn buses(&self) -> Buses {
        self.0.buses
    }

    /// The `restartComponent` flags the plugin raised since the last call.
    pub fn take_restart(&self) -> i32 {
        self.0.inst.lock().unwrap().take_restart()
    }
}

impl Plugin for Vst3Plugin {
    fn name(&self) -> &str {
        &self.0.name
    }

    fn latency(&self) -> u32 {
        self.0.latency.load(Ordering::Relaxed)
    }

    fn param_count(&self) -> u32 {
        self.0.params.len() as u32
    }

    fn param_name(&self, index: u32) -> String {
        self.0
            .titles
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
                p.value.store(f64::from(value).to_bits(), Ordering::Relaxed);
                p.dirty.store(true, Ordering::Release);
                p.ctl_dirty.store(true, Ordering::Release);
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

    /// An unapplied host value (the processor's or the controller's), else the controller's own
    /// (`getParamNormalized`): the value follows both.
    fn param_value(&self, index: u32) -> f32 {
        let i = index as usize;
        let (Some(p), Some(param)) = (self.0.pending.get(i), self.0.params.get(i)) else {
            return 0.0;
        };
        if p.dirty.load(Ordering::Acquire) || p.ctl_dirty.load(Ordering::Acquire) {
            return (f64::from_bits(p.value.load(Ordering::Relaxed)) as f32).clamp(0.0, 1.0);
        }
        let Some(info) = &param.info else { return 0.0 };
        let inst = self.0.inst.lock().unwrap();
        inst.controller_get_param(info.id)
            .map_or(0.0, |v| (v as f32).clamp(0.0, 1.0))
    }

    /// A value from the host: queued for the processor's next block (the region path's atomics) and told to the
    /// controller now (the dispatcher's flush, here at once).
    fn host_set(&self, plugin_id: u32, value: f32) -> Result<f32, summoner_proc::Error> {
        let why = || -> Result<f32, String> {
            let index = self
                .0
                .params
                .iter()
                .position(|p| p.valid && p.info.as_ref().is_some_and(|i| i.id == plugin_id))
                // `--cfg mutant_param_set_any_id` (never in a normal build): an unknown id sets index 0;
                // host/tests/param_set.rs's refusal test fails.
                .or(cfg!(mutant_param_set_any_id).then_some(0))
                .ok_or_else(|| format!("no parameter {plugin_id}"))?;
            self.set_param(index as u32, value);
            {
                let inst = self.0.inst.lock().unwrap();
                self.flush_controller(&inst);
            }
            Ok(self.param_value(index as u32))
        };
        why().map_err(summoner_proc::Error::from)
    }

    fn host_set_pending(&self) -> bool {
        self.0
            .pending
            .iter()
            .any(|p| p.dirty.load(Ordering::Acquire))
    }

    fn set_state(&self, blob: &[u8]) -> Result<(), summoner_proc::Error> {
        {
            let why =
                || -> Result<(), String> { self.set_chunk(0, blob).map_err(|e| format!("{e:?}")) };
            why()
        }
        .map_err(summoner_proc::Error::from)
    }

    /// What the plugin raised through `restartComponent`, from any thread, since the last poll:
    /// - `kMidiCCAssignmentChanged`: the MIDI mapping is read again, here;
    /// - `kParamTitlesChanged`: the titles are read again, here, index by index; an index whose info now fails keeps
    ///   its title, and the ids never change;
    /// - `kLatencyChanged`, `kReloadComponent`, `kIoChanged`: it is re-activated (the reactivation handover); the
    ///   cached latency is read again after that, in [`Dsp::reactivate`]. A `kLatencyChanged` alone
    ///   whose latency equals the active processor's (read at its prepare or last re-activation) is nothing.
    /// - `kParamValuesChanged`: nothing (no cached value here needs a refresh, and nothing in the bridge reads one).
    fn poll(&self) -> Notice {
        let inst = self.0.inst.lock().unwrap();
        self.flush_controller(&inst);
        let flags = inst.take_restart();
        if flags & MIDI_CC_ASSIGNMENT_CHANGED != 0 {
            read_mapping(&inst, &self.0.cc);
        }
        if flags & PARAM_TITLES_CHANGED != 0 {
            let fresh = inst.params();
            let mut titles = self.0.titles.lock().unwrap();
            for (t, info) in titles.iter_mut().zip(fresh) {
                if let Some(info) = info {
                    *t = info.title;
                }
            }
        }
        if flags & (LATENCY_CHANGED | RELOAD_COMPONENT | IO_CHANGED) == 0 {
            Notice::None
        } else if flags & (RELOAD_COMPONENT | IO_CHANGED) == 0
            && inst.latency() == self.0.active_latency.load(Ordering::Relaxed)
        {
            // kLatencyChanged alone, at the latency the active processor already has (a plugin may raise it from
            // inside its own activation, delivered at the next poll): nothing to align, so no cycle and no report.
            Notice::None
        } else if flags & (RELOAD_COMPONENT | IO_CHANGED) == 0
            && LATENCY_NO_CYCLE.load(Ordering::Relaxed)
        {
            // The test hooks' no-cycle way (`--test-latency-no-cycle`): the latency is read again and reported, with
            // no cycle, so a real plugin's null can be taken without a re-activation in it.
            let _ = self.0.latency.swap(inst.latency(), Ordering::Relaxed);
            Notice::Latency
        } else {
            Notice::Reactivate
        }
    }

    fn edits(&self) -> u64 {
        self.0.inst.lock().unwrap().edit_count()
    }

    fn editor_create(&self) -> Option<(u32, u32, bool)> {
        self.0.inst.lock().unwrap().editor_create().ok()
    }

    fn editor_attach(&self, a: summoner_proc::EditorAttach) -> Result<(), summoner_proc::Error> {
        self.0
            .inst
            .lock()
            .unwrap()
            .editor_attach(
                a.parent.ns_view()?,
                a.host.set_size(),
                a.host.get_size(),
                a.host.context(),
            )
            .map_err(|e| format!("{e:?}").into())
    }

    fn editor_resize_hook(&self) -> Option<(summoner_proc::ResizeHookFn, usize)> {
        Some(self.0.inst.lock().unwrap().editor_resize_hook())
    }

    fn editor_close(&self) {
        let _ = self.0.inst.lock().unwrap().editor_close();
    }

    fn get_state(&self) -> Vec<u8> {
        self.chunk(0).unwrap_or_default()
    }

    fn state_kind(&self) -> StateKind {
        StateKind::Vst3
    }

    /// The state blob's two chunks: the component chunk (the controller synced from it), then the controller chunk; an
    /// absent chunk is skipped, an empty one applied. The component applied and the controller then refused is
    /// [`SetOutcome::FirstOnly`] (the plugin changed, it cannot be rolled back).
    fn set_state_chunks(&self, component: Option<&[u8]>, controller: Option<&[u8]>) -> SetOutcome {
        if let Some(c) = component
            && let Err(e) = self.set_chunk(0, c)
        {
            return SetOutcome::Refused(format!("component: {e:?}"));
        }
        if let Some(e) = controller
            && let Err(why) = self.set_chunk(1, e)
        {
            let why = format!("controller: {why:?}");
            return if component.is_some() {
                SetOutcome::FirstOnly(why)
            } else {
                SetOutcome::Refused(why)
            };
        }
        SetOutcome::Applied
    }

    /// Each chunk present when its `getState` succeeded (the state blob's `present` bits).
    fn get_state_chunks(&self) -> Result<Chunks, summoner_proc::Error> {
        Ok(Chunks::new(self.chunk(0).ok(), self.chunk(1).ok()))
    }

    fn prepare(&self, rate: f64, max_frames: usize) -> Box<dyn Dsp> {
        self.prepare_mode(rate, max_frames, false)
    }

    fn prepare_mode(&self, rate: f64, max_frames: usize, offline: bool) -> Box<dyn Dsp> {
        let i = &self.0;
        let max_points = i.params.len() + MAX_NOTES;
        let mut inst = i.inst.lock().unwrap();
        // A failed prepare leaves no processor; that slot's blocks are then THREW (the host's report).
        let processor = inst
            .prepare(
                rate,
                max_frames as u32,
                max_points as u32,
                MAX_NOTES as u32,
                offline,
            )
            .map_err(|e| eprintln!("[summoner-vst3] {}: prepare: {e:?}", i.name))
            .ok();
        i.latency.store(inst.latency(), Ordering::Relaxed);
        i.active_latency.store(inst.latency(), Ordering::Relaxed);
        drop(inst);
        // Every buffer the chain thread writes is in runecraft-rt's wired arena (the arena's own prefault).
        let dsp = Vst3Dsp {
            processor,
            inner: i.clone(),
            inputs: runecraft_rt::arena::ArenaSlice::from_fn(
                summoner_proc::RT_OWNER,
                i.buses.inputs as usize,
                |_| summoner_proc::rt_line(max_frames),
            ),
            spare: runecraft_rt::arena::ArenaSlice::from_fn(
                summoner_proc::RT_OWNER,
                i.buses.outputs.saturating_sub(MIN_BLOCK) as usize,
                |_| summoner_proc::rt_line(max_frames),
            ),
            points: summoner_proc::RtVec::new(max_points, Point::default()),
            notes: summoner_proc::RtVec::new(MAX_NOTES, Note::default()),
            edits: summoner_proc::RtVec::new(MAX_EDITS, (0, 0.0)),
        };
        Box::new(dsp)
    }
}

struct Vst3Dsp {
    processor: Option<Processor>,
    inner: Arc<Inner>,
    /// The input bus's copy of the block, one line per bus channel: the block's channel, or zeros past the block's
    /// width (the outputs are written in place into the block's channels).
    /// All five in the wired arena (made at prepare, on the message thread).
    inputs: runecraft_rt::arena::ArenaSlice<runecraft_rt::arena::ArenaSlice<f32>>,
    /// The output bus's channels past a 2-wide block (a main bus the plugin kept wider than stereo): written by the
    /// plugin and dropped (only the block's channels are copied back).
    spare: runecraft_rt::arena::ArenaSlice<runecraft_rt::arena::ArenaSlice<f32>>,
    points: summoner_proc::RtVec<Point>,
    notes: summoner_proc::RtVec<Note>,
    /// This block's controller edits, the last value per id (fixed capacity: never grows).
    edits: summoner_proc::RtVec<(u32, f64)>,
}

impl Vst3Dsp {
    fn push_point(&mut self, p: Point) {
        let _ = self.points.push(p); // never grows: a full buffer refuses
    }

    fn push_note(&mut self, n: Note) {
        if self.notes.len() < self.notes.capacity() {
            let _ = self.notes.push(n);
        }
    }

    /// One MIDI event as the plugin takes it: the conversion (`midi::convert_midi`) over this
    /// instance's `IMidiMapping` table, pushed into the block's bounded buffers.
    fn midi(&mut self, offset: i32, s: u8, d1: u8, d2: u8) {
        let cc = &self.inner.cc;
        let out = midi::convert_midi(offset, s, d1, d2, |ch, n| {
            let id = cc[ch * CCS + n].load(Ordering::Relaxed);
            (id != NO_PARAM).then_some(id)
        });
        match out {
            midi::Vst3Midi::Event(n) => self.push_note(n),
            midi::Vst3Midi::Point { id, offset, value } => {
                self.push_point(Point { id, offset, value })
            }
            midi::Vst3Midi::None => {}
        }
    }
}

impl Dsp for Vst3Dsp {
    /// The main buses as arranged at load (`Instance::buses`), fixed for the instance's life: stereo each
    /// way when the plugin takes the stereo request, else its own arrangement (possibly wider than the block;
    /// every channel of it is a real buffer in [`Dsp::process`]).
    fn io(&self) -> summoner_proc::Io {
        summoner_proc::Io::new(self.inner.buses.inputs, self.inner.buses.outputs)
    }

    /// The spec's cycle on the message thread, then the latency read again.
    fn reactivate(&mut self) -> Result<(), summoner_proc::Error> {
        let r = match self.processor.as_mut() {
            Some(p) => p.reactivate().map_err(|e| format!("{e:?}").into()),
            None => Err("not prepared".into()),
        };
        let inst = self.inner.inst.lock().unwrap();
        self.inner.latency.store(inst.latency(), Ordering::Relaxed);
        self.inner
            .active_latency
            .store(inst.latency(), Ordering::Relaxed);
        r
    }

    /// VST3 has no reset call: the host's all-notes-off (CC 123 on every channel, in the block's MIDI) reaches the
    /// plugin through its MIDI mapping, if it has one.
    fn reset(&mut self) {}

    fn process(&mut self, b: &mut Block<'_>) -> bool {
        self.points.clear();
        self.notes.clear();
        let inner = self.inner.clone();
        for (p, param) in inner.pending.iter().zip(&inner.params) {
            if let Some(info) = param.info.as_ref().filter(|_| param.valid)
                && p.dirty.swap(false, Ordering::Acquire)
            {
                self.push_point(Point {
                    id: info.id,
                    offset: 0,
                    value: f64::from_bits(p.value.load(Ordering::Relaxed)),
                });
            }
        }
        for m in b.midi {
            self.midi(m.offset as i32, m.byte(0), m.byte(1), m.byte(2));
        }
        // The controller's edits since the last block (GUI -> audio by value): the last value per id at offset 0; an
        // id that is no parameter is dropped.
        self.edits.clear();
        if let Some(processor) = self.processor.as_mut() {
            while self.edits.len() < self.edits.capacity() {
                let Some((id, value)) = processor.take_edit() else {
                    break;
                };
                if inner.edit_ids.binary_search(&id).is_err() {
                    continue;
                }
                match self.edits.iter_mut().find(|(i, _)| *i == id) {
                    Some(e) => e.1 = value,
                    None => {
                        let _ = self.edits.push((id, value));
                    }
                }
            }
        }
        for i in 0..self.edits.len() {
            let (id, value) = self.edits[i];
            self.push_point(Point {
                id,
                offset: 0,
                value,
            });
        }
        // The shim takes the points sorted by id, stably (each id's points stay in offset order): insertion, in place.
        for i in 1..self.points.len() {
            let mut j = i;
            while j > 0 && self.points[j - 1].id > self.points[j].id {
                self.points.swap(j - 1, j);
                j -= 1;
            }
        }
        let frames = b.frames;
        // The plugin gets exactly its buses' channel counts, each a real buffer of `frames`. Inputs past the block's
        // width are zero (the scratch is cleared past the block's inputs); outputs past it go to the spare lines.
        let width = b.channels.len();
        let (n_in, n_out) = if cfg!(mutant_bus_width) {
            // The mutant: the old counts (the bus clamped to the block), its channels past the block null.
            (
                self.inputs.len().min(width),
                (inner.buses.outputs as usize).min(width),
            )
        } else {
            (self.inputs.len(), inner.buses.outputs as usize)
        };
        if n_out > width + self.spare.len() {
            return false; // a block narrower than the host's 2 (never sent): THREW, never a missing channel
        }
        for (c, dst) in self.inputs.iter_mut().enumerate() {
            match b.channels.get(c) {
                Some(src) => dst[..frames].copy_from_slice(&src[..frames]),
                None => dst[..frames].fill(0.0),
            }
        }
        let mut ins = [std::ptr::null::<f32>(); MAX_BUS];
        for (p, v) in ins.iter_mut().zip(self.inputs.iter()).take(n_in) {
            *p = v.as_ptr();
        }
        let mut outs = [std::ptr::null_mut::<f32>(); MAX_BUS];
        let spare_needed = n_out.saturating_sub(width);
        for s in self.spare.iter_mut().take(spare_needed) {
            s[..frames].fill(0.0);
        }
        let lines = b
            .channels
            .iter_mut()
            .take(n_out)
            .chain(self.spare.iter_mut().take(spare_needed));
        for (p, v) in outs.iter_mut().zip(lines) {
            *p = v.as_mut_ptr();
        }
        let t = b.transport;
        let (num, den) = ((t.time_sig & 0xffff) as i32, (t.time_sig >> 16) as i32);
        let transport = sys::Transport {
            playing: u8::from(t.flags & BF_PLAYING != 0),
            cycle_active: u8::from(t.flags & BF_LOOPING != 0),
            recording: u8::from(t.flags & BF_RECORDING != 0),
            tempo_valid: u8::from(t.bpm > 0.0),
            time_sig_valid: u8::from(num != 0 && den != 0),
            bar_valid: u8::from(t.flags & BF_BAR_START_VALID != 0),
            cycle_valid: u8::from(t.flags & BF_LOOP_POINTS_VALID != 0),
            system_time_valid: u8::from(t.output_host_time_ns != 0),
            ppq: t.ppq,
            tempo: t.bpm,
            bar_ppq: t.bar_start_ppq,
            cycle_start_ppq: t.loop_start_ppq,
            cycle_end_ppq: t.loop_end_ppq,
            time_sig_num: num,
            time_sig_den: den,
            project_samples: t.time_samples as i64,
            system_time_ns: t.output_host_time_ns as i64,
        };
        let Some(processor) = self.processor.as_mut() else {
            return false; // not prepared: THREW
        };
        let r = processor.process(&sys::Block {
            inputs: &ins[..n_in],
            outputs: &outs[..n_out],
            frames,
            points: &self.points,
            notes: &self.notes,
            transport: &transport,
        });
        // A failed block (a C++ exception caught by the shim, or the plugin's own failure): THREW and silent.
        r.is_ok()
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
