//! The VST3 host shim (`vst3/summoner_vst3_shim.h`), behind safe owned handles. The shim is glue over the pinned SDK:
//! every call is `noexcept`, and a C++ exception comes back as [`Error::Threw`]. A module is loaded by absolute path
//! only; the shim has no path-enumerating code (the SDK's `module_mac.mm` is not compiled, and a test pins that its
//! folder walk is not in the library).
//!
//! The types carry the VST3 threading contract:
//! - an [`Instance`] (create, buses, state, parameters, latency) is `Send` and not `Sync`: one thread at a time, the
//!   host's message thread;
//! - [`Instance::prepare`] (`&mut`) returns the one [`Processor`] (`Send`, not `Sync`, `process(&mut self)`) that the
//!   chain thread owns; dropping it releases the plugin (`setProcessing (false)`, `setActive (false)`). `prepare`
//!   refuses while a processor is alive, so prepare and release can never run concurrently with `process`;
//! - the instance and its processor share the raw plugin, which holds its [`Module`] (`Arc`), so a module outlives
//!   every plugin made from it whatever the caller does;
//! - message-thread calls on an [`Instance`] may run while its [`Processor`] processes, as the VST3 spec allows (a
//!   plugin must be safe for it).
use crate::bundle::{Bundle, Format, LoadCheck};
use std::cell::Cell;
use std::ffi::{CString, c_char, c_void};
use std::marker::PhantomData;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

#[repr(C)]
struct RawModule {
    _p: [u8; 0],
}
#[repr(C)]
struct RawPlugin {
    _p: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawClass {
    cid: [u8; 16],
    cardinality: i32,
    class_flags: u32,
    category: [c_char; 32],
    name: [c_char; 64],
    vendor: [c_char; 64],
    version: [c_char; 64],
    sdk_version: [c_char; 64],
    sub_categories: [c_char; 128],
}

#[repr(C)]
struct RawParam {
    id: u32,
    /// UTF-8 (the shim converts the UTF-16 String128: at most 384 bytes and the terminator).
    title: [c_char; 512],
    default_normalized: f64,
    step_count: i32,
    flags: i32,
    /// UTF-8, the String128 `units`.
    units: [c_char; 512],
    /// `ParameterInfo.unitId`.
    unit_id: i32,
}

/// One parameter change for the block (`IParamValueQueue` point).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// The parameter id (`ParamID`).
    pub id: u32,
    /// The sample offset of the point in the block.
    pub offset: i32,
    /// The normalized value (0 to 1).
    pub value: f64,
}

// Plain values, so the chain thread's wired buffers may hold them.
impl runecraft_rt::arena::RtMem for Point {}

/// `kNoteOnEvent`: `channel`, `pitch`, `velocity`.
pub const NOTE_ON: i32 = 0;
/// `kNoteOffEvent`: `channel`, `pitch`, `velocity`.
pub const NOTE_OFF: i32 = 1;
/// `kPolyPressureEvent`: `pitch`, `velocity` = the pressure.
pub const POLY_PRESSURE: i32 = 2;
/// `kLegacyMIDICCOutEvent`: `control`, `pitch` = value, `value2`.
pub const LEGACY_CC: i32 = 3;

/// One input event for the block (summoner_vst3_shim.h's `summoner_vst3_note`, field for field).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Note {
    /// The sample offset of the event in the block.
    pub offset: i32,
    /// [`NOTE_ON`], [`NOTE_OFF`], [`POLY_PRESSURE`] or [`LEGACY_CC`].
    pub kind: i32,
    /// The MIDI channel (0-15).
    pub channel: i16,
    /// The note number, or the value of a [`LEGACY_CC`].
    pub pitch: i16,
    /// The velocity (0 to 1), or the pressure of a [`POLY_PRESSURE`].
    pub velocity: f32,
    /// The control number of a [`LEGACY_CC`].
    pub control: i16,
    /// The second value of a [`LEGACY_CC`].
    pub value2: i16,
}

// Plain values.
impl runecraft_rt::arena::RtMem for Note {}

/// The block's transport as the shim fills `ProcessContext` (which fields are valid is the caller's decision).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transport {
    /// Nonzero: the transport is playing.
    pub playing: u8,
    /// Nonzero: the cycle (loop) is active.
    pub cycle_active: u8,
    /// Nonzero: the transport is recording.
    pub recording: u8,
    /// Nonzero: `tempo` is valid.
    pub tempo_valid: u8,
    /// Nonzero: `time_sig_num` / `time_sig_den` are valid.
    pub time_sig_valid: u8,
    /// Nonzero: `bar_ppq` is valid.
    pub bar_valid: u8,
    /// Nonzero: `cycle_start_ppq` / `cycle_end_ppq` are valid.
    pub cycle_valid: u8,
    /// Nonzero: `system_time_ns` is valid.
    pub system_time_valid: u8,
    /// The block's musical position in quarter notes.
    pub ppq: f64,
    /// The tempo in beats per minute.
    pub tempo: f64,
    /// The position of the last bar start in quarter notes.
    pub bar_ppq: f64,
    /// The cycle start in quarter notes.
    pub cycle_start_ppq: f64,
    /// The cycle end in quarter notes.
    pub cycle_end_ppq: f64,
    /// The time signature's numerator.
    pub time_sig_num: i32,
    /// The time signature's denominator.
    pub time_sig_den: i32,
    /// The block's position in samples from the project start.
    pub project_samples: i64,
    /// The system time in nanoseconds.
    pub system_time_ns: i64,
}

#[repr(C)]
struct RawBlock {
    inputs: *const *const f32,
    num_inputs: i32,
    outputs: *const *mut f32,
    num_outputs: i32,
    frames: i32,
    points: *const Point,
    num_points: i32,
    notes: *const Note,
    num_notes: i32,
    transport: *const Transport,
}

const OK: i32 = 0;
const THREW: i32 = 2;
const REFUSED: i32 = 3;

/// The shim's ABI number this extern block was written for: `SUMMONER_VST3_SHIM_ABI` in summoner_vst3_shim.h.
const SHIM_ABI: i32 = 4;

unsafe extern "C" {
    fn summoner_vst3_module_open(
        path: *const c_char,
        out: *mut *mut RawModule,
        err: *mut c_char,
        cap: usize,
    ) -> i32;
    fn summoner_vst3_module_close(m: *mut RawModule);
    fn summoner_vst3_class_count(m: *const RawModule) -> i32;
    fn summoner_vst3_class_get(m: *const RawModule, index: i32, out: *mut RawClass) -> i32;
    fn summoner_vst3_factory_vendor(m: *const RawModule, out: *mut c_char, cap: usize) -> i32;
    fn summoner_vst3_plugin_default_channels(p: *const RawPlugin, dir: i32) -> i32;
    fn summoner_vst3_plugin_create(
        m: *mut RawModule,
        cid: *const u8,
        out: *mut *mut RawPlugin,
        err: *mut c_char,
        cap: usize,
    ) -> i32;
    fn summoner_vst3_plugin_destroy(p: *mut RawPlugin);
    fn summoner_vst3_plugin_buses(
        p: *mut RawPlugin,
        in_ch: i32,
        out_ch: i32,
        got_in: *mut i32,
        got_out: *mut i32,
        has_events: *mut i32,
    ) -> i32;
    fn summoner_vst3_shim_abi() -> i32;
    fn summoner_vst3_rt_bytes(max_points: i32, max_notes: i32) -> usize;
    fn summoner_vst3_plugin_prepare(
        p: *mut RawPlugin,
        rate: f64,
        max_frames: i32,
        max_points: i32,
        max_notes: i32,
        offline: i32,
        rt: *mut u8,
        rt_len: usize,
    ) -> i32;
    fn summoner_vst3_plugin_release(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_plugin_setup_only(p: *mut RawPlugin, rate: f64, max_frames: i32) -> i32;
    fn summoner_vst3_plugin_reactivate(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_plugin_process(p: *mut RawPlugin, b: *const RawBlock) -> i32;
    fn summoner_vst3_plugin_latency(p: *mut RawPlugin) -> u32;
    fn summoner_vst3_plugin_take_restart(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_plugin_edit_count(p: *mut RawPlugin) -> u64;
    fn summoner_vst3_controller_set_param(p: *mut RawPlugin, id: u32, value: f64) -> i32;
    fn summoner_vst3_plugin_flush_outputs(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_editor_create(
        p: *mut RawPlugin,
        w: *mut i32,
        h: *mut i32,
        resizable: *mut i32,
    ) -> i32;
    fn summoner_vst3_editor_attach(
        p: *mut RawPlugin,
        nsview: *mut c_void,
        set_size: *mut c_void,
        get_size: *mut c_void,
        window: *mut c_void,
    ) -> i32;
    fn summoner_vst3_editor_user_resize(
        ctx: *mut c_void,
        phase: i32,
        w: *mut f64,
        h: *mut f64,
    ) -> i32;
    fn summoner_vst3_editor_close(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_plugin_take_edit(p: *mut RawPlugin, id: *mut u32, value: *mut f64) -> i32;
    fn summoner_vst3_param_count(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_param_get(p: *mut RawPlugin, index: i32, out: *mut RawParam) -> i32;
    fn summoner_vst3_unit_count(p: *mut RawPlugin) -> i32;
    fn summoner_vst3_unit_get(p: *mut RawPlugin, index: i32, id: *mut i32, parent: *mut i32)
    -> i32;
    fn summoner_vst3_controller_get_param(p: *mut RawPlugin, id: u32, value: *mut f64) -> i32;
    fn summoner_vst3_midi_mapping(p: *mut RawPlugin, channel: i16, cc: i16, id: *mut u32) -> i32;
    fn summoner_vst3_state_get(
        p: *mut RawPlugin,
        which: i32,
        data: *mut *mut u8,
        len: *mut usize,
    ) -> i32;
    fn summoner_vst3_state_set(p: *mut RawPlugin, which: i32, data: *const u8, len: usize) -> i32;
    fn summoner_vst3_free(data: *mut u8);
}

/// A shim call that did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The plugin (or the SDK) said no, or the call failed; the message when there is one.
    Failed(String),
    /// The plugin refused the input (a state it would not take).
    Refused,
    /// A C++ exception escaped the plugin and was caught at the boundary.
    Threw,
}

fn check(rc: i32) -> Result<(), Error> {
    match rc {
        OK => Ok(()),
        THREW => Err(Error::Threw),
        REFUSED => Err(Error::Refused),
        _ => Err(Error::Failed(String::new())),
    }
}

fn check_msg(rc: i32, err: &[c_char]) -> Result<(), Error> {
    check(rc).map_err(|e| match e {
        Error::Failed(_) => Error::Failed(cstr(err)),
        e => e,
    })
}

fn cstr(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A factory class (`PClassInfo` / `PClassInfo2`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassInfo {
    /// The class id.
    pub cid: [u8; 16],
    /// The class cardinality (`kManyInstances` or a count).
    pub cardinality: i32,
    /// `PClassInfo2::classFlags`.
    pub class_flags: u32,
    /// The category (`kVstAudioEffectClass` and the like).
    pub category: String,
    /// The class name.
    pub name: String,
    /// `PClassInfo2::vendor`.
    pub vendor: String,
    /// `PClassInfo2::version`.
    pub version: String,
    /// `PClassInfo2::sdkVersion`.
    pub sdk_version: String,
    /// `PClassInfo2::subCategories` (`|`-separated).
    pub sub_categories: String,
}

/// A parameter (`ParameterInfo`).
#[derive(Clone, Debug, PartialEq)]
pub struct ParamInfo {
    /// `ParameterInfo::id`.
    pub id: u32,
    /// `ParameterInfo::title`, as UTF-8.
    pub title: String,
    /// `ParameterInfo::defaultNormalizedValue`.
    pub default_normalized: f64,
    /// `ParameterInfo::stepCount` (0: continuous).
    pub step_count: i32,
    /// `ParameterInfo::flags` (see [`PARAM_CAN_AUTOMATE`]).
    pub flags: i32,
    /// `ParameterInfo::units`.
    pub units: String,
    /// `ParameterInfo::unitId` (its `IUnitInfo` unit).
    pub unit_id: i32,
}

/// `ParameterInfo::kCanAutomate`: the parameter is automatable.
pub const PARAM_CAN_AUTOMATE: i32 = 1;

/// A loaded VST3 module (bundle).
pub struct Module {
    raw: NonNull<RawModule>,
}

// SAFETY: the module is only touched through `&self` / `&mut self` from the host's message thread or, for its
// plugins, as the module docs say; the shim's module holds no thread-affine state of its own (CFBundle references are
// thread-safe), and a VST3 factory may be used from the thread that loaded it.
unsafe impl Send for Module {}
// SAFETY: as Send; shared access only reads the factory (class queries), which VST3 factories allow.
unsafe impl Sync for Module {}

impl Module {
    /// Loads a lab bundle: `bundleEntry`, then `GetPluginFactory`, then the
    /// post-load image check ([`LoadCheck`]): an image outside the bundle and the system's own is a STOP and an `Err`.
    pub fn open(b: &Bundle) -> Result<Module, Error> {
        // SAFETY: a function of no arguments that returns a constant.
        let abi = unsafe { summoner_vst3_shim_abi() };
        if abi != SHIM_ABI {
            return Err(Error::Failed(format!(
                "the VST3 shim's ABI is {abi}, this build expects {SHIM_ABI} (summoner_vst3_shim.h)"
            )));
        }
        if b.format() != Format::Vst3 {
            return Err(Error::Refused);
        }
        let c =
            CString::new(b.path().as_os_str().as_encoded_bytes()).map_err(|_| Error::Refused)?;
        let mut out = std::ptr::null_mut();
        let mut err = [0 as c_char; 512];
        let load = LoadCheck::begin().map_err(|e| Error::Failed(e.into()))?;
        // SAFETY: `c` is a NUL-terminated path that outlives the call; `out` and `err` are live out-parameters and
        // `err.len()` is its capacity.
        let rc =
            unsafe { summoner_vst3_module_open(c.as_ptr(), &mut out, err.as_mut_ptr(), err.len()) };
        check_msg(rc, &err)?;
        let m = NonNull::new(out)
            .map(|raw| Module { raw })
            .ok_or_else(|| Error::Failed("no module".into()))?;
        load.finish(b).map_err(|e| Error::Failed(e.into()))?;
        Ok(m)
    }

    /// The factory's classes, in factory order (a class whose info the factory refuses is left out).
    pub fn classes(&self) -> Vec<ClassInfo> {
        // SAFETY: `raw` is a live module from `open`.
        let n = unsafe { summoner_vst3_class_count(self.raw.as_ptr()) };
        (0..n)
            .filter_map(|i| {
                // SAFETY: an all-zero RawClass is valid plain data, filled in by the shim.
                let mut c: RawClass = unsafe { std::mem::zeroed() };
                // SAFETY: `raw` is live, `i` is in 0..count, `c` is a live out-parameter.
                (unsafe { summoner_vst3_class_get(self.raw.as_ptr(), i, &mut c) } == OK).then(
                    || ClassInfo {
                        cid: c.cid,
                        cardinality: c.cardinality,
                        class_flags: c.class_flags,
                        category: cstr(&c.category),
                        name: cstr(&c.name),
                        vendor: cstr(&c.vendor),
                        version: cstr(&c.version),
                        sdk_version: cstr(&c.sdk_version),
                        sub_categories: cstr(&c.sub_categories),
                    },
                )
            })
            .collect()
    }

    /// The factory's `PFactoryInfo::vendor` (for the scan); empty if the factory refuses.
    pub fn factory_vendor(&self) -> String {
        let mut buf = [0 as c_char; 64];
        // SAFETY: `raw` is a live module from `open`; `buf` is a live out-buffer of the capacity passed.
        unsafe { summoner_vst3_factory_vendor(self.raw.as_ptr(), buf.as_mut_ptr(), buf.len()) };
        cstr(&buf)
    }

    /// Creates the class `cid`: its component and controller, initialized, connected, synced. The instance holds
    /// the module.
    pub fn create(module: &Arc<Module>, cid: &[u8; 16]) -> Result<Instance, Error> {
        let mut out = std::ptr::null_mut();
        let mut err = [0 as c_char; 512];
        // SAFETY: `raw` is a live module (the Arc keeps it so, and the new plugin's Raw holds a clone of that Arc);
        // `cid` is 16 bytes; `out` and `err` are live out-parameters.
        let rc = unsafe {
            summoner_vst3_plugin_create(
                module.raw.as_ptr(),
                cid.as_ptr(),
                &mut out,
                err.as_mut_ptr(),
                err.len(),
            )
        };
        check_msg(rc, &err)?;
        let ptr = NonNull::new(out).ok_or_else(|| Error::Failed("no instance".into()))?;
        Ok(Instance {
            raw: Arc::new(Raw {
                ptr,
                rt: std::sync::Mutex::new(None),
                _module: module.clone(),
            }),
            _not_sync: PhantomData,
        })
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `open` and is closed once, here.
        unsafe { summoner_vst3_module_close(self.raw.as_ptr()) };
    }
}

/// The raw plugin, shared by its [`Instance`] and its [`Processor`]; destroyed when both are gone, and then its
/// module reference is dropped (field order).
struct Raw {
    ptr: NonNull<RawPlugin>,
    /// Shim ABI 3: the wired memory the shim placed the process call's state in (its parameter queues, events and
    /// output ring) at the last prepare. Dropped after the record (`Drop` destroys the record first, then the fields),
    /// and replaced only by a prepare, while no processor exists. The chain thread never takes this lock.
    rt: std::sync::Mutex<Option<runecraft_rt::os::LockedRegion>>,
    _module: Arc<Module>,
}

// SAFETY: Raw has no methods; it is reached only through an Instance (one thread at a time, not Sync) and its one
// Processor (one thread, not Sync, `&mut` process), and the shim allows the instance's message-thread calls while the
// processor processes (the VST3 contract, module docs). Moving it between threads moves no thread-affine state.
unsafe impl Send for Raw {}
// SAFETY: as Send: the Arc that shares Raw is only ever held by the two handles above.
unsafe impl Sync for Raw {}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from `create` and is destroyed once, here, after the processor (if any) has released it.
        unsafe { summoner_vst3_plugin_destroy(self.ptr.as_ptr()) };
    }
}

/// A created plugin: its component, processor and controller, message-thread side.
pub struct Instance {
    raw: Arc<Raw>,
    _not_sync: PhantomData<Cell<()>>,
}

/// A prepared plugin, chain-thread side: [`Processor::process`]. Dropping it releases the plugin.
pub struct Processor {
    raw: Arc<Raw>,
    _not_sync: PhantomData<Cell<()>>,
}

impl Drop for Processor {
    fn drop(&mut self) {
        // SAFETY: the plugin is live (Raw outlives this handle) and was prepared by `Instance::prepare`.
        unsafe { summoner_vst3_plugin_release(self.raw.ptr.as_ptr()) };
    }
}

/// The main buses as arranged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Buses {
    /// The main input bus's channels (0: no input bus).
    pub inputs: u32,
    /// The main output bus's channels (0: no output bus).
    pub outputs: u32,
    /// The plugin has an event input bus.
    pub events: bool,
}

/// The block for [`Processor::process`]: the main buses' channels, the parameter points, the notes, the transport.
pub struct Block<'a> {
    /// The main input bus's channels (may be empty).
    pub inputs: &'a [*const f32],
    /// The main output bus's channels.
    pub outputs: &'a [*mut f32],
    /// The frames in every channel.
    pub frames: usize,
    /// The parameter points, sorted by id, stably (each id's points in offset order).
    pub points: &'a [Point],
    /// The input events.
    pub notes: &'a [Note],
    /// The block's transport.
    pub transport: &'a Transport,
}

impl Instance {
    /// Arranges the main buses for `ch_in` / `ch_out` channels and returns them as the plugin took them.
    pub fn buses(&self, ch_in: u32, ch_out: u32) -> Result<Buses, Error> {
        let (mut i, mut o, mut e) = (0, 0, 0);
        // SAFETY: `raw` is live; the three are live out-parameters.
        check(unsafe {
            summoner_vst3_plugin_buses(
                self.raw.ptr.as_ptr(),
                ch_in as i32,
                ch_out as i32,
                &mut i,
                &mut o,
                &mut e,
            )
        })?;
        Ok(Buses {
            inputs: i.max(0) as u32,
            outputs: o.max(0) as u32,
            events: e != 0,
        })
    }

    /// setupProcessing, setActive, setProcessing: the one [`Processor`]. Refused while a processor is alive.
    /// `offline`: `kOffline` (a render set's slot), else `kRealtime`; kept for reactivate and every
    /// process call.
    pub fn prepare(
        &mut self,
        rate: f64,
        max_frames: u32,
        max_points: u32,
        max_notes: u32,
        offline: bool,
    ) -> Result<Processor, Error> {
        if Arc::strong_count(&self.raw) > 1 {
            return Err(Error::Failed("a processor is still alive".into()));
        }
        // The process call's state in wired memory the shim lays out (a refused wire leaves it usable, counted by
        // the region's own accounting).
        // SAFETY: a pure size computation in the shim; plain values.
        let bytes = unsafe { summoner_vst3_rt_bytes(max_points as i32, max_notes as i32) };
        let mut region = runecraft_rt::os::LockedRegion::map(bytes.max(1))
            .map_err(|e| Error::Failed(format!("the shim's realtime memory: {e}")))?;
        let _ = region.lock();
        let mut held = self.raw.rt.lock().unwrap();
        let mem = region.as_bytes_mut();
        // SAFETY: the plugin is live; no processor exists, so nothing processes during this call. `mem` is `bytes` of
        // zeroed memory owned by `region`, which is stored in `Raw::rt` below and outlives every use the shim makes of
        // it (the record, destroyed before the region in `Raw::drop`; a later prepare replaces it only after the shim
        // has moved to the new region and flushed the old ring, within this call).
        let r = check(unsafe {
            summoner_vst3_plugin_prepare(
                self.raw.ptr.as_ptr(),
                rate,
                max_frames as i32,
                max_points as i32,
                max_notes as i32,
                offline as i32,
                mem.as_mut_ptr(),
                mem.len(),
            )
        });
        // Which memory the record now points at: the new region, unless the shim refused it before placing anything
        // (a size or null refusal: then it still uses the previous one, which stays).
        if !matches!(r, Err(Error::Refused)) || held.is_none() {
            *held = Some(region);
        }
        drop(held);
        r?;
        Ok(Processor {
            raw: self.raw.clone(),
            _not_sync: PhantomData,
        })
    }

    /// `setupProcessing` alone on the inactive plugin. Refused while a Processor of this instance is alive; else
    /// forwards to the shim.
    pub fn setup_only(&mut self, rate: f64, max_frames: u32) -> Result<(), Error> {
        if self.processor_alive() {
            return Err(Error::Failed(
                "setup_only refused: a Processor of this instance is alive".into(),
            ));
        }
        // Two's-complement truncation on purpose: values above i32::MAX are passed as negative.
        let frames = max_frames as i32;
        // SAFETY: the plugin is live; no processor exists, so nothing processes during this call; plain values.
        let rc = unsafe { summoner_vst3_plugin_setup_only(self.raw.ptr.as_ptr(), rate, frames) };
        if rc == OK {
            Ok(())
        } else {
            Err(Error::Failed(format!(
                "setupProcessing at load failed (shim code {rc})"
            )))
        }
    }

    /// A Processor holds the only other clone of the shared handle.
    fn processor_alive(&self) -> bool {
        std::sync::Arc::strong_count(&self.raw) > 1
    }

    /// The plugin's latency in samples (`IAudioProcessor::getLatencySamples`).
    pub fn latency(&self) -> u32 {
        // SAFETY: `raw` is live.
        unsafe { summoner_vst3_plugin_latency(self.raw.ptr.as_ptr()) }
    }

    /// The channels of the default-active audio buses (input or output), as the shim counts them; negative values as 0.
    pub fn default_channels(&self, input: bool) -> u32 {
        let dir: i32 = if input { 0 } else { 1 };
        // SAFETY: `raw` is live; the shim only reads bus info.
        let n = unsafe { summoner_vst3_plugin_default_channels(self.raw.ptr.as_ptr(), dir) };
        if n < 0 { 0 } else { n as u32 }
    }

    /// The `restartComponent` flags since the last call.
    pub fn take_restart(&self) -> i32 {
        // SAFETY: `raw` is live.
        unsafe { summoner_vst3_plugin_take_restart(self.raw.ptr.as_ptr()) }
    }

    /// `IEditController::setParamNormalized`: the host's value of parameter `id`, told to the controller.
    pub fn controller_set_param(&self, id: u32, value: f64) -> Result<(), Error> {
        // SAFETY: `raw` is live; `Instance` is the message-thread side (`Send`, not `Sync`), the controller's thread.
        check(unsafe { summoner_vst3_controller_set_param(self.raw.ptr.as_ptr(), id, value) })
    }

    /// The processor's output parameter changes since the last call, handed to the controller: how many, or `None` if
    /// the plugin threw.
    pub fn flush_outputs(&self) -> Option<u32> {
        // SAFETY: `raw` is live; the message-thread side, the ring's one consumer (`Instance` is not `Sync`).
        let n = unsafe { summoner_vst3_plugin_flush_outputs(self.raw.ptr.as_ptr()) };
        u32::try_from(n).ok()
    }

    /// The editor, step 1: the controller's view, if it has one that takes an `NSView`:
    /// its size and whether it resizes. `Err(Refused)` when it has none (`editor.availability {available: false}`).
    pub fn editor_create(&self) -> Result<(u32, u32, bool), Error> {
        let (mut w, mut h, mut r) = (0i32, 0i32, 0i32);
        // SAFETY: `raw` is live; message thread (`Instance` is not `Sync`); the out-pointers are live locals.
        check(unsafe {
            summoner_vst3_editor_create(self.raw.ptr.as_ptr(), &mut w, &mut h, &mut r)
        })?;
        Ok((w.max(1) as u32, h.max(1) as u32, r != 0))
    }

    /// Step 3: `setFrame`, then `attached` to `nsview` (the editor window's content view). `set_size` / `get_size` are
    /// the editor dylib's window-size functions (`summoner_ed_window_set_size` / `_get_size`) and `window` their context,
    /// for the plugin's `IPlugFrame::resizeView`.
    ///
    /// The caller guarantees `nsview` and `window` are a live editor window's, and stay so until
    /// [`Instance::editor_close`]: the host closes the view before the window.
    pub fn editor_attach(
        &self,
        nsview: usize,
        set_size: usize,
        get_size: usize,
        window: usize,
    ) -> Result<(), Error> {
        // SAFETY: `raw` is live, with a view from `editor_create`; message thread. The four pointers are, by the
        // contract above, a live window's content view, the dylib's two size functions (their C signatures match
        // shim.cpp's SetSizeFn / GetSizeFn) and that window's handle; the shim stores them until `editor_close`.
        check(unsafe {
            summoner_vst3_editor_attach(
                self.raw.ptr.as_ptr(),
                nsview as *mut c_void,
                set_size as *mut c_void,
                get_size as *mut c_void,
                window as *mut c_void,
            )
        })
    }

    /// The window's user-resize hook for this plugin (`checkSizeConstraint`, then `onSize`) and its context: the
    /// editor dylib's `ResizeHook` signature exactly. The context is valid while this instance lives.
    pub fn editor_resize_hook(
        &self,
    ) -> (
        unsafe extern "C" fn(*mut c_void, i32, *mut f64, *mut f64) -> i32,
        usize,
    ) {
        (
            summoner_vst3_editor_user_resize,
            self.raw.ptr.as_ptr() as usize,
        )
    }

    /// Step 5: `removed`, `setFrame (null)`, the view released (before the window is closed).
    pub fn editor_close(&self) -> Result<(), Error> {
        // SAFETY: `raw` is live; message thread.
        check(unsafe { summoner_vst3_editor_close(self.raw.ptr.as_ptr()) })
    }

    /// How many `performEdit`s the controller has made so far (the shim's counter).
    pub fn edit_count(&self) -> u64 {
        // SAFETY: `raw` is live; the shim reads an atomic.
        unsafe { summoner_vst3_plugin_edit_count(self.raw.ptr.as_ptr()) }
    }

    /// One entry per index the controller claims (`getParameterCount`), `None` where its `getParameterInfo` failed:
    /// a plugin's count may lie, and the caller decides what an index it cannot describe means.
    pub fn params(&self) -> Vec<Option<ParamInfo>> {
        // SAFETY: `raw` is live.
        let n = unsafe { summoner_vst3_param_count(self.raw.ptr.as_ptr()) };
        (0..n.max(0))
            .map(|i| {
                // SAFETY: an all-zero RawParam is valid plain data, filled in by the shim.
                let mut p: RawParam = unsafe { std::mem::zeroed() };
                // SAFETY: `raw` is live, `p` a live out-parameter.
                (unsafe { summoner_vst3_param_get(self.raw.ptr.as_ptr(), i, &mut p) } == OK).then(
                    || ParamInfo {
                        id: p.id,
                        title: cstr(&p.title),
                        default_normalized: p.default_normalized,
                        step_count: p.step_count,
                        flags: p.flags,
                        units: cstr(&p.units),
                        unit_id: p.unit_id,
                    },
                )
            })
            .collect()
    }

    /// `IUnitInfo`'s units 1 to `getUnitCount() - 1` as `(id, parentUnitId)` (a failed `getUnitInfo` is a zeroed
    /// unit, `(0, 0)`); empty without `IUnitInfo`.
    pub fn units(&self) -> Vec<(i32, i32)> {
        // SAFETY: `raw` is live.
        let n = unsafe { summoner_vst3_unit_count(self.raw.ptr.as_ptr()) };
        (1..n.max(0))
            .map(|i| {
                let (mut id, mut parent) = (0i32, 0i32);
                // SAFETY: `raw` is live; `id` and `parent` are live out-parameters (zeroed by the shim on a failure).
                let _ = unsafe {
                    summoner_vst3_unit_get(self.raw.ptr.as_ptr(), i, &mut id, &mut parent)
                };
                (id, parent)
            })
            .collect()
    }

    /// `IEditController::getParamNormalized` of parameter `id` (`None`: no controller, or it threw).
    pub fn controller_get_param(&self, id: u32) -> Option<f64> {
        let mut v = 0f64;
        // SAFETY: `raw` is live; `v` is a live out-parameter; the message-thread side, where the controller lives.
        (unsafe { summoner_vst3_controller_get_param(self.raw.ptr.as_ptr(), id, &mut v) } == OK)
            .then_some(v)
    }

    /// `IMidiMapping`: the parameter MIDI controller `cc` on `channel` maps to.
    pub fn midi_mapping(&self, channel: i16, cc: i16) -> Option<u32> {
        let mut id = 0;
        // SAFETY: `raw` is live, `id` a live out-parameter.
        (unsafe { summoner_vst3_midi_mapping(self.raw.ptr.as_ptr(), channel, cc, &mut id) } == OK)
            .then_some(id)
    }

    /// The component chunk (`which` 0) or the controller chunk (1).
    pub fn state(&self, which: i32) -> Result<Vec<u8>, Error> {
        let mut data = std::ptr::null_mut();
        let mut len = 0usize;
        // SAFETY: `raw` is live; the out-parameters are live.
        check(unsafe {
            summoner_vst3_state_get(self.raw.ptr.as_ptr(), which, &mut data, &mut len)
        })?;
        if data.is_null() {
            return Ok(Vec::new());
        }
        // SAFETY: the shim returned `len` initialised bytes at `data` (malloc'd), freed once, here, after the copy.
        let v = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
        // SAFETY: as above.
        unsafe { summoner_vst3_free(data) };
        Ok(v)
    }

    /// Sets the component chunk (`which` 0, then the controller's component state) or the controller chunk (1).
    pub fn set_state(&self, which: i32, bytes: &[u8]) -> Result<(), Error> {
        // SAFETY: `raw` is live; `bytes` outlives the call and the shim only reads it.
        check(unsafe {
            summoner_vst3_state_set(self.raw.ptr.as_ptr(), which, bytes.as_ptr(), bytes.len())
        })
    }
}

impl Processor {
    /// The oldest `performEdit` not yet taken (its id, and its value rounded to `f32`), or `None`. The chain thread's
    /// lock-free pop of the shim's ring; never blocks, never allocates.
    pub fn take_edit(&mut self) -> Option<(u32, f64)> {
        let (mut id, mut value) = (0u32, 0f64);
        // SAFETY: the plugin is live (this handle exists); the ring has one consumer, this processor, and `&mut self`
        // keeps it to one thread at a time; both out-pointers are to live locals.
        let got =
            unsafe { summoner_vst3_plugin_take_edit(self.raw.ptr.as_ptr(), &mut id, &mut value) };
        (got == 1).then_some((id, value))
    }

    /// The spec's re-activation after `restartComponent (kLatencyChanged)` (setProcessing / setActive off,
    /// setupProcessing as prepared, setActive / setProcessing on). `&mut self`: the chain thread does not hold this
    /// processor while it runs (the host takes it out first).
    pub fn reactivate(&mut self) -> Result<(), Error> {
        // SAFETY: the plugin is live and prepared (this handle exists); `&mut self` means no process call runs.
        check(unsafe { summoner_vst3_plugin_reactivate(self.raw.ptr.as_ptr()) })
    }

    /// One block. The caller guarantees every input pointer has `frames` readable floats and every output pointer
    /// `frames` writable ones (crates/summoner-format-vst3 builds them from its own buffers); the slices only carry
    /// them.
    pub fn process(&mut self, b: &Block<'_>) -> Result<(), Error> {
        let raw = RawBlock {
            inputs: b.inputs.as_ptr(),
            num_inputs: b.inputs.len() as i32,
            outputs: b.outputs.as_ptr(),
            num_outputs: b.outputs.len() as i32,
            frames: b.frames as i32,
            points: b.points.as_ptr(),
            num_points: b.points.len() as i32,
            notes: b.notes.as_ptr(),
            num_notes: b.notes.len() as i32,
            transport: b.transport,
        };
        // SAFETY: the plugin is live and prepared (this handle exists); `&mut self` makes this the only process call;
        // every pointer in the block points into slices that outlive the call, with the lengths given; the channel
        // pointers' extent is the caller's guarantee above.
        check(unsafe { summoner_vst3_plugin_process(self.raw.ptr.as_ptr(), &raw) })
    }
}

/// The shim library's path, for the symbol test (and nothing else).
#[doc(hidden)]
pub fn shim_library() -> std::path::PathBuf {
    Path::new(env!("OUT_DIR")).join("libsummoner_vst3_shim.a")
}

#[cfg(test)]
mod tests {
    /// The SDK's folder walk (`Module::getModulePaths` and friends, in `module_mac.mm`) is not in
    /// the shim library, defined or referenced, nor is any API that enumerates the system's plugin folders.
    #[test]
    fn the_shim_has_no_path_enumerating_code() {
        let lib = super::shim_library();
        let out = std::process::Command::new("nm")
            .arg("-gj")
            .arg(&lib)
            .output()
            .expect("nm");
        assert!(out.status.success(), "nm {}", lib.display());
        let syms = String::from_utf8_lossy(&out.stdout);
        assert!(
            syms.contains("_summoner_vst3_module_open"),
            "nm read the shim: {}",
            lib.display()
        );
        for bad in [
            "getModulePaths",
            "getSnapshots",
            "getModuleInfoPath",
            "NSSearchPathForDirectoriesInDomains",
            "CFBundleCreateBundlesFromDirectory",
            "opendir",
            "readdir",
            "fts_open",
            "glob",
        ] {
            assert!(
                !syms.lines().any(|l| l.contains(bad)),
                "{bad} is in the shim:\n{syms}"
            );
        }
    }
}
