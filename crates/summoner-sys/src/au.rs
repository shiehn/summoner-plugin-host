//! AU v2 hosting through AudioToolbox (feature `au`; lab bundles only). C APIs called directly from Rust: no C++
//! shim.
//!
//! **What this module never does:** a walk of the system's AU registry (no count, no name copy, no wildcard
//! lookup). A component is reached only through [`register`], which takes a [`Bundle`] (the lab
//! gate, already passed), loads that bundle by its absolute path, checks the loaded images ([`LoadCheck`]) and
//! registers its `AudioComponents` entries in-process with `AudioComponentRegister`. The returned `AudioComponent` is
//! the only handle ever instantiated from. Apple's registrar scan, triggered by AudioToolbox as a side effect, is
//! allowed.
//!
//! There is no registry lookup at all (no AU by identifier): no build of this child names `AudioComponentFindNext`,
//! `AudioComponentCount` or `AudioComponentCopyName` (a test of the host application's checks it).
//!
//! The types carry the threading, the same shape as for VST3:
//! - an [`Instance`] (parameters, state, latency, prepare) is `Send` and not `Sync`: the host's message thread;
//! - [`Instance::prepare`] returns the one [`Processor`] (`Send`, not `Sync`, `render(&mut self)`), which the chain
//!   thread owns. Dropping it releases the AU (`AudioUnitReset` on every
//!   bus, then global, then `AudioUnitUninitialize`). `prepare` refuses while a processor is alive;
//! - message-thread calls on an [`Instance`] (property get/set, `ClassInfo`) may run while its [`Processor`] renders:
//!   the AU concurrency every host relies on. The latency is read on the message
//!   thread only; the property listener's callback only sets an atomic ([`Instance::take_notices`]).
//!
//! **The input render callback:** the processor arms the input context immediately before `AudioUnitRender` and
//! disarms it immediately after, then waits until no pull is in flight. The callback returns
//! `kAudioUnitErr_NoConnection` (never stale memory) when the context is not armed, when the frame count is not the
//! render's, when the bus is not 0, or when a buffer is missing or too small. A second pull in one render reads the
//! same input (the AU contract). The context's buffers are written only while it is disarmed and no pull is in flight.
//!
//! **The output list:** after `AudioUnitRender` the list must still hold our count, our pointers and the render's
//! byte size; a list the AU rewrote is a failed block.
//!
//! **Images:** the load, the instantiation and `AudioUnitInitialize` are each wrapped in
//! [`LoadCheck`]: a foreign image is an `Err` and a STOP, after which this process loads no plugin code again, and the
//! path of a bundle whose load was a STOP is refused for good.
//!
//! **State:** `kAudioUnitProperty_ClassInfo` → `CFPropertyListWrite`, binary v1.0 → bytes; a set parses the bytes
//! with `CFPropertyListCreateWithStream` and sets `ClassInfo`. Both failures are reported.
//!
//! **HostCallbacks:** `kAudioUnitProperty_HostCallbacks` is set at instantiation with three procs (BeatAndTempo,
//! MusicalTimeLocation, TransportState; `transportStateProc2` NULL), each answering from the block's transport, with
//! fixed fallbacks, always `noErr`. The render thread publishes
//! each block's transport into the instance's [`HostCtx`] with a seqlock before `AudioUnitRender`
//! ([`Processor::publish_transport`]): atomics only, no allocation, no lock. A callback on the render thread reads the
//! current block's; one on another thread reads a consistent snapshot in at most four tries.
use crate::bundle::{Bundle, Format, LoadCheck};
use std::cell::{Cell, UnsafeCell};
use std::ffi::{c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex};

/// The most channels a bus carries here (the lab's chains are stereo).
pub const MAX_CH: usize = 8;

/// `kAudioUnitScope_Global`.
pub const SCOPE_GLOBAL: u32 = 0;
/// `kAudioUnitScope_Input`.
pub const SCOPE_INPUT: u32 = 1;
/// `kAudioUnitScope_Output`.
pub const SCOPE_OUTPUT: u32 = 2;
/// `kAudioUnitType_MusicDevice` (an instrument).
pub const TYPE_MUSIC_DEVICE: u32 = u32::from_be_bytes(*b"aumu");
/// `kAudioUnitType_MusicEffect` (an effect that takes MIDI).
pub const TYPE_MUSIC_EFFECT: u32 = u32::from_be_bytes(*b"aumf");
/// `kAudioUnitType_MIDIProcessor`.
pub const TYPE_MIDI_PROCESSOR: u32 = u32::from_be_bytes(*b"aumi");
/// `kAudioUnitType_Effect`.
pub const TYPE_EFFECT: u32 = u32::from_be_bytes(*b"aufx");
/// `kAudioUnitErr_NoConnection`: the input callback's answer when it has no input to give.
pub const ERR_NO_CONNECTION: i32 = -10876;

const PROP_CLASS_INFO: u32 = 0;
const PROP_SAMPLE_RATE: u32 = 2;
const PROP_PARAMETER_LIST: u32 = 3;
const PROP_PARAMETER_INFO: u32 = 4;
const PROP_STREAM_FORMAT: u32 = 8;
const PROP_ELEMENT_COUNT: u32 = 11;
const PROP_LATENCY: u32 = 12;
const PROP_MAX_FRAMES: u32 = 14;
const PROP_SET_RENDER_CALLBACK: u32 = 23;
const PROP_HOST_CALLBACKS: u32 = 27;
/// `kAudioUnitProperty_OfflineRender` (a UInt32, global). **Read only, never set:** a render set's AU slot renders
/// as live, as a bounce of the host application expects (an AU's offline mode can change its sound).
/// [`Instance::offline_render`] reads it back, for the tests that prove it is not set.
const PROP_OFFLINE_RENDER: u32 = 37;
/// `kAudioUnitParameterFlag_NonRealTime`: a parameter that is not automatable.
pub const FLAG_NON_REAL_TIME: u32 = 1 << 24;
/// `kAudioUnitParameterFlag_HasCFNameString`: the name is `cfNameString`, not the fixed `name` array.
const FLAG_HAS_CF_NAME: u32 = 1 << 27;
/// `kAudioUnitParameterFlag_CFNameRelease`: the caller releases `cfNameString` (and a custom unit's `unitName`).
const FLAG_CF_NAME_RELEASE: u32 = 1 << 4;
/// `kAudioUnitParameterFlag_HasClump`: `clumpID` is set (the parameter tree groups a clump).
const FLAG_HAS_CLUMP: u32 = 1 << 20;
/// `kAudioUnitParameterUnit_CustomUnit`: the unit's name is `unitName`.
pub const UNIT_CUSTOM: u32 = 26;

// New bindings, named after the Apple symbols (AudioToolbox/AudioUnitProperties.h, macOS 15.2 SDK).
/// kAudioUnitParameterUnit_Indexed
pub const UNIT_INDEXED: u32 = 1;
/// kAudioUnitParameterUnit_Boolean
pub const UNIT_BOOLEAN: u32 = 2;
/// kAudioUnitParameterUnit_Percent
pub const UNIT_PERCENT: u32 = 3;
/// kAudioUnitParameterUnit_Seconds
pub const UNIT_SECONDS: u32 = 4;
/// kAudioUnitParameterUnit_Hertz
pub const UNIT_HERTZ: u32 = 8;
/// kAudioUnitParameterUnit_Decibels
pub const UNIT_DECIBELS: u32 = 13;
/// kAudioUnitParameterUnit_Milliseconds
pub const UNIT_MILLISECONDS: u32 = 24;
const FORMAT_LPCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_FLOAT_PACKED_NONINTERLEAVED: u32 = 1 | 8 | 32;
const TS_SAMPLE_TIME_VALID: u32 = 1;
const TS_HOST_TIME_VALID: u32 = 2;
const CF_UTF8: u32 = 0x0800_0100;
const CF_NUMBER_SINT64: isize = 4;
const CF_PLIST_BINARY_V1: isize = 200;
const CF_PLIST_IMMUTABLE: usize = 0;

/// `AudioComponentDescription`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    /// `componentType`.
    pub kind: u32,
    /// `componentSubType`.
    pub sub: u32,
    /// `componentManufacturer`.
    pub manu: u32,
    /// `componentFlags`.
    pub flags: u32,
    /// `componentFlagsMask`.
    pub mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Asbd {
    rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

/// `AudioBufferList` with room for [`MAX_CH`] buffers (the same layout: the count, then the array at offset 8).
#[repr(C)]
struct BufferList {
    n: u32,
    bufs: [AudioBuffer; MAX_CH],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Smpte {
    subframes: i16,
    divisor: i16,
    counter: u32,
    kind: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock: u64,
    smpte: Smpte,
    flags: u32,
    reserved: u32,
}

type InputProc =
    unsafe extern "C" fn(*mut c_void, *mut u32, *const TimeStamp, u32, u32, *mut BufferList) -> i32;
type ListenerProc = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u32);
type FactoryFn = unsafe extern "C" fn(*const Desc) -> *mut c_void;
type BeatProc = unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> i32;
type MusicalProc = unsafe extern "C" fn(*mut c_void, *mut u32, *mut f32, *mut u32, *mut f64) -> i32;
type TransportProc = unsafe extern "C" fn(
    *mut c_void,
    *mut u8,
    *mut u8,
    *mut f64,
    *mut u8,
    *mut f64,
    *mut f64,
) -> i32;
type TransportProc2 = unsafe extern "C" fn(
    *mut c_void,
    *mut u8,
    *mut u8,
    *mut u8,
    *mut f64,
    *mut u8,
    *mut f64,
    *mut f64,
) -> i32;

/// `HostCallbackInfo` (AudioUnitProperties.h): five pointers, 40 bytes. `Boolean` is `unsigned char`.
#[repr(C)]
struct HostCallbackInfo {
    host_user_data: *mut c_void,
    beat_and_tempo: Option<BeatProc>,
    musical_time_location: Option<MusicalProc>,
    transport_state: Option<TransportProc>,
    /// NULL (it adds isRecording; revisit only if the app ever records through hosted AUs).
    transport_state2: Option<TransportProc2>,
}

const _: () = assert!(size_of::<HostCallbackInfo>() == 40);

#[repr(C)]
struct RenderCallback {
    proc_: Option<InputProc>,
    refcon: *mut c_void,
}

// The layouts, as the SDK's headers have them (checked against a C compile of <AudioToolbox/AudioToolbox.h>).
const _: () = assert!(size_of::<Desc>() == 20);
const _: () = assert!(size_of::<Asbd>() == 40);
const _: () = assert!(size_of::<AudioBuffer>() == 16);
const _: () = assert!(std::mem::offset_of!(BufferList, bufs) == 8);
const _: () = assert!(size_of::<TimeStamp>() == 64);
const _: () = assert!(std::mem::offset_of!(TimeStamp, flags) == 56);
const _: () = assert!(size_of::<RenderCallback>() == 16);

/// `AudioUnitParameter` / `AudioUnitProperty` (the same layout): the event's argument (AudioUnitUtilities.h).
#[repr(C)]
struct AuEventArg {
    unit: *mut c_void,
    id: u32,
    scope: u32,
    element: u32,
}

/// `AudioUnitEvent`: its type, then the union (both members have `AuEventArg`'s layout).
#[repr(C)]
struct AuEvent {
    event_type: u32,
    arg: AuEventArg,
}

/// `kAudioUnitEvent_ParameterValueChange`.
const AU_EVENT_PARAMETER_VALUE_CHANGE: u32 = 0;

type AuEventProc = unsafe extern "C" fn(*mut c_void, *mut c_void, *const AuEvent, u64, f32);

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AUEventListenerCreate(
        proc_: AuEventProc,
        user: *mut c_void,
        run_loop: *mut c_void,
        mode: *const c_void,
        interval: f32,
        granularity: f32,
        out: *mut *mut c_void,
    ) -> i32;
    fn AUEventListenerAddEventType(
        listener: *mut c_void,
        object: *mut c_void,
        event: *const AuEvent,
    ) -> i32;
    fn AUListenerDispose(listener: *mut c_void) -> i32;
    fn AUEventListenerNotify(
        sending_listener: *mut c_void,
        sending_object: *mut c_void,
        event: *const AuEvent,
    ) -> i32;
    fn AudioComponentRegister(
        desc: *const Desc,
        name: *const c_void,
        version: u32,
        f: FactoryFn,
    ) -> *mut c_void;
    fn AudioComponentInstanceNew(comp: *mut c_void, out: *mut *mut c_void) -> i32;
    fn AudioComponentInstanceDispose(unit: *mut c_void) -> i32;
    fn AudioUnitInitialize(unit: *mut c_void) -> i32;
    fn AudioUnitUninitialize(unit: *mut c_void) -> i32;
    fn AudioUnitGetPropertyInfo(
        unit: *mut c_void,
        id: u32,
        scope: u32,
        el: u32,
        size: *mut u32,
        w: *mut u8,
    ) -> i32;
    fn AudioUnitGetProperty(
        unit: *mut c_void,
        id: u32,
        scope: u32,
        el: u32,
        out: *mut c_void,
        size: *mut u32,
    ) -> i32;
    fn AudioUnitSetProperty(
        unit: *mut c_void,
        id: u32,
        scope: u32,
        el: u32,
        data: *const c_void,
        size: u32,
    ) -> i32;
    fn AudioUnitAddPropertyListener(
        unit: *mut c_void,
        id: u32,
        f: ListenerProc,
        refcon: *mut c_void,
    ) -> i32;
    fn AudioUnitRemovePropertyListenerWithUserData(
        unit: *mut c_void,
        id: u32,
        f: ListenerProc,
        refcon: *mut c_void,
    ) -> i32;
    fn AudioUnitSetParameter(
        unit: *mut c_void,
        id: u32,
        scope: u32,
        el: u32,
        v: f32,
        offset: u32,
    ) -> i32;
    fn AudioUnitGetParameter(unit: *mut c_void, id: u32, scope: u32, el: u32, v: *mut f32) -> i32;
    fn AudioUnitReset(unit: *mut c_void, scope: u32, el: u32) -> i32;
    fn AudioUnitRender(
        unit: *mut c_void,
        flags: *mut u32,
        ts: *const TimeStamp,
        bus: u32,
        frames: u32,
        io: *mut BufferList,
    ) -> i32;
    fn MusicDeviceMIDIEvent(unit: *mut c_void, status: u32, d1: u32, d2: u32, offset: u32) -> i32;
    fn MusicDeviceSysEx(unit: *mut c_void, data: *const u8, len: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    fn CFRunLoopGetMain() -> *mut c_void;
    static kCFStreamPropertyDataWritten: *const c_void;
    static kCFAllocatorNull: *const c_void;
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
    fn CFStringGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFDataGetTypeID() -> usize;
    fn CFURLCreateFromFileSystemRepresentation(
        a: *const c_void,
        p: *const u8,
        n: isize,
        dir: u8,
    ) -> *const c_void;
    fn CFBundleCreate(a: *const c_void, url: *const c_void) -> *const c_void;
    fn CFBundleGetValueForInfoDictionaryKey(b: *const c_void, key: *const c_void) -> *const c_void;
    fn CFBundleLoadExecutable(b: *const c_void) -> u8;
    fn CFBundleGetFunctionPointerForName(b: *const c_void, name: *const c_void) -> *mut c_void;
    fn CFStringCreateWithBytes(
        a: *const c_void,
        p: *const u8,
        n: isize,
        enc: u32,
        ext: u8,
    ) -> *const c_void;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(n: isize, enc: u32) -> isize;
    fn CFStringGetCString(s: *const c_void, buf: *mut c_char, n: isize, enc: u32) -> u8;
    fn CFArrayGetCount(a: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(a: *const c_void, i: isize) -> *const c_void;
    fn CFDictionaryGetValue(d: *const c_void, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(n: *const c_void, kind: isize, out: *mut c_void) -> u8;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFWriteStreamCreateWithAllocatedBuffers(a: *const c_void, b: *const c_void)
    -> *const c_void;
    fn CFWriteStreamOpen(s: *const c_void) -> u8;
    fn CFWriteStreamClose(s: *const c_void);
    fn CFWriteStreamCopyProperty(s: *const c_void, key: *const c_void) -> *const c_void;
    fn CFPropertyListWrite(
        p: *const c_void,
        s: *const c_void,
        fmt: isize,
        opt: usize,
        err: *mut c_void,
    ) -> isize;
    fn CFReadStreamCreateWithBytesNoCopy(
        a: *const c_void,
        p: *const u8,
        n: isize,
        d: *const c_void,
    ) -> *const c_void;
    fn CFReadStreamOpen(s: *const c_void) -> u8;
    fn CFReadStreamClose(s: *const c_void);
    fn CFPropertyListCreateWithStream(
        a: *const c_void,
        s: *const c_void,
        n: isize,
        opt: usize,
        fmt: *mut isize,
        err: *mut c_void,
    ) -> *const c_void;
}

/// An owned CoreFoundation reference (+1), released on drop.
struct Cf(NonNull<c_void>);

impl Cf {
    fn new(p: *const c_void) -> Option<Cf> {
        NonNull::new(p.cast_mut()).map(Cf)
    }
    fn get(&self) -> *const c_void {
        self.0.as_ptr()
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: `Cf` holds exactly one reference (made by a Create / Copy call), released once, here.
        unsafe { CFRelease(self.0.as_ptr()) };
    }
}

/// A CFString from Rust text (+1).
fn cf_str(s: &str) -> Cf {
    // SAFETY: `s` is valid UTF-8 for `s.len()` bytes for the duration of the call; the default allocator.
    let p = unsafe {
        CFStringCreateWithBytes(std::ptr::null(), s.as_ptr(), s.len() as isize, CF_UTF8, 0)
    };
    Cf::new(p).expect("CFStringCreateWithBytes of valid UTF-8")
}

/// The text of a CFString that is not ours to release (`None` for null or not a string).
fn string_of(s: *const c_void) -> Option<String> {
    // SAFETY: `s` is null-checked; CFGetTypeID accepts any CF object.
    if s.is_null() || unsafe { CFGetTypeID(s) != CFStringGetTypeID() } {
        return None;
    }
    // SAFETY: `s` is a live CFString (checked above).
    let n = unsafe { CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), CF_UTF8) } + 1;
    let mut buf = vec![0 as c_char; n.max(1) as usize];
    // SAFETY: `buf` has `n` bytes of capacity, as passed; CFStringGetCString NUL-terminates within it on success.
    if unsafe { CFStringGetCString(s, buf.as_mut_ptr(), n, CF_UTF8) } == 0 {
        return None;
    }
    // SAFETY: on success `buf` holds a NUL-terminated string.
    Some(
        unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// The value of `key` in a CFDictionary that is not ours to release.
fn dict_get(d: *const c_void, key: &str) -> *const c_void {
    let k = cf_str(key);
    // SAFETY: `d` is a live CFDictionary (the callers check the type) and `k` a live CFString; the result is not
    // retained (the Get rule).
    unsafe { CFDictionaryGetValue(d, k.get()) }
}

/// A four-character code: four characters, big-endian; shorter strings are padded with spaces.
fn os_type(s: &str) -> u32 {
    let mut b = [b' '; 4];
    for (d, c) in b.iter_mut().zip(s.bytes()) {
        *d = c;
    }
    u32::from_be_bytes(b)
}

/// Why an AudioToolbox call or the work around it failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Refused before AudioToolbox was called (not a lab bundle, a processor alive, a bad argument).
    Refused(String),
    /// An `OSStatus` other than `noErr`.
    Status(i32),
    /// A CoreFoundation step around AudioToolbox failed (no URL, no bundle, a bad plist, ...): what failed.
    Failed(String),
}

fn check(rc: i32) -> Result<(), Error> {
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::Status(rc))
    }
}

/// Where a component came from: the image checks around its instantiation and initialization depend on it.
#[derive(Clone, Debug)]
enum Origin {
    /// Registered in-process from this bundle ([`register`]): judged against the bundle ([`LoadCheck::finish`]).
    Bundle(Arc<Bundle>),
}

/// A registered component: the description, the name (`AudioComponents`' `name`), the version. The handle stays
/// valid for the process (see the `Send` / `Sync` reasoning below), so it is `Send` and `Sync`.
#[derive(Clone, Debug)]
pub struct Component {
    /// The component's description (its three codes and flags).
    pub desc: Desc,
    raw: NonNull<c_void>,
    /// The entry's `version` from the bundle's `Info.plist`.
    pub version: u32,
    /// The index of this component's entry in its bundle's `Info.plist` `AudioComponents` array (a scan verdict
    /// names it).
    pub entry: Option<u32>,
    origin: Origin,
}

// SAFETY: an AudioComponent handle is a process-global registry entry that is not removed during the process: every
// one here is one this process registered itself (`register`), which is never unregistered, and AudioToolbox does not
// remove v2 registry entries while a process runs. AudioToolbox's AudioComponent functions may be called from any
// thread; the bundle is plain data.
unsafe impl Send for Component {}
// SAFETY: as Send: no method mutates through it.
unsafe impl Sync for Component {}

impl Origin {
    /// The post-load check of an instantiation or an initialize, by origin.
    fn finish(&self, load: LoadCheck) -> Result<(), String> {
        match self {
            Origin::Bundle(b) => load.finish(b).map_err(String::from),
        }
    }
}

/// One registered bundle: its components with their names. Kept for the life of the process (bundles are never
/// unloaded), so registering a path twice returns the first registration.
#[derive(Clone, Debug)]
pub struct Registered {
    /// Each registered entry with its `name`, in `AudioComponents` order.
    pub components: Vec<(Component, String)>,
}

struct BundleHold {
    path: std::path::PathBuf,
    _bundle: Cf,
    reg: Registered,
}

// SAFETY: a CFBundle reference may be held and released from any thread (CF's bundle API is thread-safe), and the
// held bundle is only ever kept alive here, never called through.
unsafe impl Send for BundleHold {}

static BUNDLES: Mutex<Vec<BundleHold>> = Mutex::new(Vec::new());
/// Paths whose load was a STOP: never registered, refused for good.
static POISONED: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

/// Loads a lab bundle (the lab gate passed) and registers each of its `AudioComponents` entries in-process.
pub fn register(b: &Bundle) -> Result<Registered, Error> {
    if b.format() != Format::Au {
        return Err(Error::Refused("not an AU bundle".into()));
    }
    if POISONED.lock().unwrap().iter().any(|p| p == b.path()) {
        return Err(Error::Refused(format!("{} was a STOP", b.path().display())));
    }
    let mut all = BUNDLES.lock().unwrap();
    if let Some(h) = all.iter().find(|h| h.path == b.path()) {
        return Ok(h.reg.clone());
    }
    let p = b.path().as_os_str().as_encoded_bytes();
    // SAFETY: `p` is valid for `p.len()` bytes for the call; the default allocator; `1`: a directory.
    let url = Cf::new(unsafe {
        CFURLCreateFromFileSystemRepresentation(std::ptr::null(), p.as_ptr(), p.len() as isize, 1)
    })
    .ok_or_else(|| Error::Failed("no URL".into()))?;
    // Refused after any STOP in this process, before the bundle is created.
    let load = LoadCheck::begin().map_err(|e| Error::Refused(e.into()))?;
    // SAFETY: `url` is a live CFURL; the result is +1 (Create rule), owned by `bundle`.
    let bundle = Cf::new(unsafe { CFBundleCreate(std::ptr::null(), url.get()) })
        .ok_or_else(|| Error::Failed(format!("no bundle at {}", b.path().display())))?;
    // SAFETY: `bundle` is live.
    if unsafe { CFBundleLoadExecutable(bundle.get()) } == 0 {
        return Err(Error::Failed("CFBundleLoadExecutable failed".into()));
    }
    if let Err(e) = load.finish(b) {
        // The executable stays loaded (bundles never unload): the path is poisoned, and the STOP refuses every later
        // load in this process.
        POISONED.lock().unwrap().push(b.path().to_path_buf());
        return Err(Error::Refused(e.into()));
    }
    let shared = Arc::new(b.clone());
    let key = cf_str("AudioComponents");
    // SAFETY: `bundle` and `key` are live; the value is not retained (Get rule) and lives as long as the bundle.
    let comps = unsafe { CFBundleGetValueForInfoDictionaryKey(bundle.get(), key.get()) };
    // SAFETY: null-checked before CFGetTypeID.
    if comps.is_null() || unsafe { CFGetTypeID(comps) != CFArrayGetTypeID() } {
        return Err(Error::Failed("no AudioComponents array".into()));
    }
    let mut out = Vec::new();
    // SAFETY: `comps` is a live CFArray (checked).
    let n = unsafe { CFArrayGetCount(comps) };
    for i in 0..n {
        // SAFETY: `i` is within the array's count; the element is not retained (Get rule).
        let d = unsafe { CFArrayGetValueAtIndex(comps, i) };
        // SAFETY: null-checked before CFGetTypeID.
        if d.is_null() || unsafe { CFGetTypeID(d) != CFDictionaryGetTypeID() } {
            continue;
        }
        let text = |k: &str| string_of(dict_get(d, k));
        let (Some(kind), Some(sub), Some(manu), Some(factory)) = (
            text("type"),
            text("subtype"),
            text("manufacturer"),
            text("factoryFunction"),
        ) else {
            continue;
        };
        let name_ref = dict_get(d, "name");
        let name = string_of(name_ref).unwrap_or_default();
        // Read as SInt64: a version above i32::MAX does not fit kCFNumberSInt32Type.
        let mut version: i64 = 0;
        let v = dict_get(d, "version");
        // SAFETY: null-checked before CFGetTypeID, and CFNumberGetValue only on a CFNumber; `version` is a live i64
        // out-parameter for kCFNumberSInt64Type.
        unsafe {
            if !v.is_null() && CFGetTypeID(v) == CFNumberGetTypeID() {
                CFNumberGetValue(v, CF_NUMBER_SINT64, (&raw mut version).cast());
            }
        }
        let f = cf_str(&factory);
        // SAFETY: `bundle` is loaded and `f` a live CFString; the result is a code address in the bundle or null.
        let fp = unsafe { CFBundleGetFunctionPointerForName(bundle.get(), f.get()) };
        if fp.is_null() {
            continue;
        }
        // SAFETY: the bundle's Info.plist names this symbol as its AudioComponentFactoryFunction, whose C signature is
        // `void* (const AudioComponentDescription*)`; the bundle stays loaded for the life of the process.
        let factory_fn: FactoryFn = unsafe { std::mem::transmute::<*mut c_void, FactoryFn>(fp) };
        let desc = Desc {
            kind: os_type(&kind),
            sub: os_type(&sub),
            manu: os_type(&manu),
            flags: 0,
            mask: 0,
        };
        // SAFETY: `desc` is live for the call; `name_ref` is the plist's CFString (or null), owned by the bundle,
        // which is never released; `factory_fn` is the bundle's factory (above).
        let raw = unsafe { AudioComponentRegister(&desc, name_ref, version as u32, factory_fn) };
        if let Some(raw) = NonNull::new(raw) {
            out.push((
                Component {
                    desc,
                    raw,
                    version: version as u32,
                    entry: Some(i as u32),
                    origin: Origin::Bundle(shared.clone()),
                },
                name,
            ));
        }
    }
    let reg = Registered { components: out };
    all.push(BundleHold {
        path: b.path().to_path_buf(),
        _bundle: bundle,
        reg: reg.clone(),
    });
    Ok(reg)
}

/// The input render callback's context, owned by the instance, at a fixed address for its whole life.
struct InputCtx {
    armed: AtomicBool,
    in_flight: AtomicU32,
    frames: AtomicU32,
    channels: AtomicU32,
    /// `channels` × `max_frames` floats, written only while disarmed with no pull in flight.
    buf: UnsafeCell<Vec<f32>>,
    max_frames: AtomicU32,
}

// SAFETY: `buf` is written only by the processor while `armed` is false and `in_flight` is 0, and read only by the
// callback after it has counted itself in `in_flight` and seen `armed` true (SeqCst on both sides): the accesses
// never overlap. Everything else is atomic.
unsafe impl Sync for InputCtx {}

/// The edit listener's callback (main run loop): a parameter value changed on the AU's own side. Only counts.
unsafe extern "C" fn au_edit_event(
    user: *mut c_void,
    _object: *mut c_void,
    event: *const AuEvent,
    _t: u64,
    _v: f32,
) {
    // SAFETY: AudioToolbox passes the event it delivers (live for the call) and our `user`: Raw's `edits`, alive until
    // the listener is disposed (Raw's drop does that first).
    unsafe {
        if !event.is_null() && (*event).event_type == AU_EVENT_PARAMETER_VALUE_CHANGE {
            (*user.cast::<AtomicU64>()).fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// What the property listener saw: it only sets these.
#[derive(Default)]
struct Notices {
    latency: AtomicBool,
    params: AtomicBool,
}

/// The block's transport as an AU's HostCallbacks answer it: the request line's, as the host decoded it for the block
/// (the same values VST3's `ProcessContext` gets). The `*_valid` / state booleans are the block flags'.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HostTransport {
    /// The position in quarter notes (the callbacks' beat).
    pub ppq: f64,
    /// 0 (or less): none; the callbacks answer the fallback, 120.
    pub bpm: f64,
    /// 0 / 0 (either 0): none; the callbacks answer the default, 4/4.
    pub time_sig_num: u32,
    /// The time signature's denominator (see `time_sig_num`).
    pub time_sig_den: u32,
    /// The current bar's start in quarter notes (valid with `bar_valid`).
    pub bar_start_ppq: f64,
    /// Whether `bar_start_ppq` is set.
    pub bar_valid: bool,
    /// The loop's start in quarter notes (valid with `loop_valid`).
    pub loop_start_ppq: f64,
    /// The loop's end in quarter notes (valid with `loop_valid`).
    pub loop_end_ppq: f64,
    /// Whether the loop points are set.
    pub loop_valid: bool,
    /// Whether the transport is playing.
    pub playing: bool,
    /// Whether the transport is looping.
    pub looping: bool,
    /// The position in samples.
    pub time_samples: u64,
}

// HostCtx's flag bits (this module's own, not the wire's).
const HT_PLAYING: u32 = 1;
const HT_LOOPING: u32 = 2;
const HT_LOOP_VALID: u32 = 4;
const HT_BAR_VALID: u32 = 8;

/// The block's transport for the HostCallbacks, behind a seqlock: one writer (the render thread, in
/// [`Processor::publish_transport`]), any number of readers (the callbacks, on any thread). Atomics only. The
/// orderings, exactly:
/// - writer: `seq.fetch_add(1, Relaxed)` (now odd); `fence(Release)`; every field stored `Relaxed`;
///   `seq.fetch_add(1, Release)` (even again);
/// - reader: `s1 = seq.load(Acquire)`, retried if odd; every field loaded `Relaxed`; `fence(Acquire)`;
///   `s2 = seq.load(Relaxed)`; accepted iff `s1 == s2`.
///
/// A reader that finds no consistent snapshot in [`HostCtx::TRIES`] tries answers from its last read (each field is
/// an atomic, so never UB; at worst fields of adjacent blocks) and counts it in `torn`.
pub(crate) struct HostCtx {
    seq: AtomicU32,
    ppq: AtomicU64,
    bpm: AtomicU64,
    bar: AtomicU64,
    loop_start: AtomicU64,
    loop_end: AtomicU64,
    samples: AtomicU64,
    /// numerator | denominator << 16
    sig: AtomicU32,
    flags: AtomicU32,
    /// The playing state the last TransportState call saw: its `transportStateChanged` is per CALL, not per block.
    was_playing: AtomicBool,
    torn: AtomicU32,
}

impl HostCtx {
    const TRIES: u32 = 4;

    fn new() -> HostCtx {
        HostCtx {
            seq: AtomicU32::new(0),
            ppq: AtomicU64::new(0),
            bpm: AtomicU64::new(0),
            bar: AtomicU64::new(0),
            loop_start: AtomicU64::new(0),
            loop_end: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            sig: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            was_playing: AtomicBool::new(false),
            torn: AtomicU32::new(0),
        }
    }

    /// The writer (the render thread only).
    fn publish(&self, t: &HostTransport) {
        self.seq.fetch_add(1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.ppq.store(t.ppq.to_bits(), Ordering::Relaxed);
        self.bpm.store(t.bpm.to_bits(), Ordering::Relaxed);
        self.bar.store(t.bar_start_ppq.to_bits(), Ordering::Relaxed);
        self.loop_start
            .store(t.loop_start_ppq.to_bits(), Ordering::Relaxed);
        self.loop_end
            .store(t.loop_end_ppq.to_bits(), Ordering::Relaxed);
        self.samples.store(t.time_samples, Ordering::Relaxed);
        self.sig.store(
            (t.time_sig_num & 0xffff) | (t.time_sig_den & 0xffff) << 16,
            Ordering::Relaxed,
        );
        let mut f = 0;
        for (on, bit) in [
            (t.playing, HT_PLAYING),
            (t.looping, HT_LOOPING),
            (t.loop_valid, HT_LOOP_VALID),
            (t.bar_valid, HT_BAR_VALID),
        ] {
            if on {
                f |= bit;
            }
        }
        self.flags.store(f, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// A reader: the snapshot, and whether it is consistent (one block's).
    fn read(&self) -> (HostTransport, bool) {
        let mut last = HostTransport::default();
        for _ in 0..Self::TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            let sig = self.sig.load(Ordering::Relaxed);
            let f = self.flags.load(Ordering::Relaxed);
            last = HostTransport {
                ppq: f64::from_bits(self.ppq.load(Ordering::Relaxed)),
                bpm: f64::from_bits(self.bpm.load(Ordering::Relaxed)),
                time_sig_num: sig & 0xffff,
                time_sig_den: sig >> 16,
                bar_start_ppq: f64::from_bits(self.bar.load(Ordering::Relaxed)),
                bar_valid: f & HT_BAR_VALID != 0,
                loop_start_ppq: f64::from_bits(self.loop_start.load(Ordering::Relaxed)),
                loop_end_ppq: f64::from_bits(self.loop_end.load(Ordering::Relaxed)),
                loop_valid: f & HT_LOOP_VALID != 0,
                playing: f & HT_PLAYING != 0,
                looping: f & HT_LOOPING != 0,
                time_samples: self.samples.load(Ordering::Relaxed),
            };
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return (last, true);
            }
        }
        self.torn.fetch_add(1, Ordering::Relaxed);
        (last, false)
    }
}

/// A NULL-checked write through an AU's out-pointer.
///
/// # Safety
/// `p` is NULL or valid, aligned and writable for one `T` (the AU API's contract for each out-parameter).
unsafe fn put<T>(p: *mut T, v: T) {
    if !p.is_null() {
        // SAFETY: not NULL, so valid, aligned and writable for one T (the caller's contract).
        unsafe { p.write(v) };
    }
}

/// HostCallbacks' BeatAndTempo: beat = ppq; tempo = bpm if > 0, else 120. Always noErr.
unsafe extern "C" fn host_beat_and_tempo(
    refcon: *mut c_void,
    beat: *mut f64,
    tempo: *mut f64,
) -> i32 {
    // SAFETY: `refcon` is the instance's HostCtx (a Box), which outlives the registration: Raw's drop NULLs the
    // HostCallbacks, then disposes the unit, before the Box is freed.
    let t = unsafe { &*refcon.cast::<HostCtx>() }.read().0;
    // SAFETY: each out-pointer is NULL or a valid, aligned, writable Float64 (the AU API's contract).
    unsafe {
        put(beat, t.ppq);
        put(tempo, if t.bpm > 0.0 { t.bpm } else { 120.0 });
    }
    0
}

/// HostCallbacks' MusicalTimeLocation: the offset to the next beat 0; the
/// time signature if both parts are set, else 4/4; the measure's downbeat the bar start if valid, else 0. Always noErr.
unsafe extern "C" fn host_musical_time_location(
    refcon: *mut c_void,
    to_next_beat: *mut u32,
    num: *mut f32,
    den: *mut u32,
    downbeat: *mut f64,
) -> i32 {
    // SAFETY: as host_beat_and_tempo: `refcon` is the live HostCtx.
    let t = unsafe { &*refcon.cast::<HostCtx>() }.read().0;
    let (n, d) = if t.time_sig_num != 0 && t.time_sig_den != 0 {
        (t.time_sig_num, t.time_sig_den)
    } else {
        (4, 4)
    };
    // SAFETY: each out-pointer is NULL or valid, aligned and writable for its type (UInt32, Float32, UInt32, Float64).
    unsafe {
        put(to_next_beat, 0);
        put(num, n as f32);
        put(den, d);
        put(downbeat, if t.bar_valid { t.bar_start_ppq } else { 0.0 });
    }
    0
}

/// HostCallbacks' TransportState: playing and cycling from the block flags; changed is per call
/// (`exchange (was_playing, now_playing) != now_playing`); the sample in the timeline; the cycle's beats if the
/// loop points are valid, else 0 / 0. Always noErr.
unsafe extern "C" fn host_transport_state(
    refcon: *mut c_void,
    playing: *mut u8,
    changed: *mut u8,
    sample: *mut f64,
    cycling: *mut u8,
    cycle_start: *mut f64,
    cycle_end: *mut f64,
) -> i32 {
    // SAFETY: as host_beat_and_tempo: `refcon` is the live HostCtx.
    let h = unsafe { &*refcon.cast::<HostCtx>() };
    let t = h.read().0;
    let was = h.was_playing.swap(t.playing, Ordering::AcqRel);
    let (cs, ce) = if t.loop_valid {
        (t.loop_start_ppq, t.loop_end_ppq)
    } else {
        (0.0, 0.0)
    };
    // SAFETY: each out-pointer is NULL or valid, aligned and writable for its type (Boolean = u8, Float64).
    unsafe {
        put(playing, u8::from(t.playing));
        put(changed, u8::from(was != t.playing));
        put(sample, t.time_samples as f64);
        put(cycling, u8::from(t.looping));
        put(cycle_start, cs);
        put(cycle_end, ce);
    }
    0
}

/// HostCallbacks registrations that could not be cleared at an instance's drop (it was disposed anyway: no callback
/// can follow a dispose).
static HOST_CALLBACKS_CLEAR_FAILURES: AtomicU32 = AtomicU32::new(0);

/// See [`HOST_CALLBACKS_CLEAR_FAILURES`].
pub fn host_callbacks_clear_failures() -> u32 {
    HOST_CALLBACKS_CLEAR_FAILURES.load(Ordering::Relaxed)
}

struct Raw {
    unit: NonNull<c_void>,
    desc: Desc,
    origin: Origin,
    input: Box<InputCtx>,
    notices: Box<Notices>,
    /// Parameter-value-change events from the AU's own side (its editor's `AUParameterSet`): counted
    /// by the event listener (its refcon). The host's own `AudioUnitSetParameter` sends none.
    edits: Box<AtomicU64>,
    /// The event listener ([`Instance::listen_for_edits`]), or null; disposed first in drop.
    edit_listener: std::sync::atomic::AtomicPtr<c_void>,
    /// The HostCallbacks' transport (its address is their refcon).
    host: Box<HostCtx>,
    /// `kAudioUnitProperty_HostCallbacks` was accepted; if not, the AU sees no callbacks (the SDK's -1), as before.
    host_callbacks: bool,
    /// Both property listeners registered; if not, the host re-reads this instance's latency itself.
    listening: bool,
}

// SAFETY: Raw is reached only through an Instance (one thread at a time, not Sync) and its one Processor (one thread,
// not Sync, `&mut` render); an AudioUnit has no thread affinity, and its property calls may run on another thread
// while it renders (module docs). Moving it between threads moves no thread-affine state.
unsafe impl Send for Raw {}
// SAFETY: as Send: the Arc that shares Raw is only ever held by the two handles above.
unsafe impl Sync for Raw {}

impl Drop for Raw {
    fn drop(&mut self) {
        // The edit listener goes first: no event may reach `edits` once it is freed (AUListenerDispose removes every
        // event type it was given).
        let l = self
            .edit_listener
            .swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !l.is_null() {
            // SAFETY: `l` is the listener `listen_for_edits` created, disposed exactly once (swapped out above).
            unsafe { AUListenerDispose(l) };
        }
        if self.host_callbacks {
            // First: no callback may reach `host` once it is freed. A refusal is counted, and the unit disposed
            // anyway: no callback can follow a dispose.
            let none = HostCallbackInfo {
                host_user_data: std::ptr::null_mut(),
                beat_and_tempo: None,
                musical_time_location: None,
                transport_state: None,
                transport_state2: None,
            };
            // SAFETY: the unit is live; `none` is a live HostCallbackInfo of its declared size for the call.
            let rc = unsafe {
                AudioUnitSetProperty(
                    self.unit.as_ptr(),
                    PROP_HOST_CALLBACKS,
                    SCOPE_GLOBAL,
                    0,
                    (&raw const none).cast(),
                    size_of::<HostCallbackInfo>() as u32,
                )
            };
            if rc != 0 {
                HOST_CALLBACKS_CLEAR_FAILURES.fetch_add(1, Ordering::Relaxed);
            }
        }
        let refcon = (&raw const *self.notices).cast_mut().cast();
        for id in [PROP_LATENCY, PROP_PARAMETER_LIST] {
            // SAFETY: the unit is live; the listener and refcon are those added in `instantiate`.
            unsafe {
                AudioUnitRemovePropertyListenerWithUserData(
                    self.unit.as_ptr(),
                    id,
                    listener,
                    refcon,
                )
            };
        }
        // SAFETY: the unit came from AudioComponentInstanceNew and is disposed once, here, after its processor (if
        // any) released it; `notices`, `input` and `host` are freed after this (fields drop after `drop`).
        unsafe { AudioComponentInstanceDispose(self.unit.as_ptr()) };
    }
}

unsafe extern "C" fn listener(
    refcon: *mut c_void,
    _unit: *mut c_void,
    id: u32,
    _scope: u32,
    _el: u32,
) {
    // SAFETY: `refcon` is the instance's `Notices`, which outlives the listener registration (removed in Raw's drop
    // before it is freed).
    let n = unsafe { &*refcon.cast::<Notices>() };
    match id {
        PROP_LATENCY => n.latency.store(true, Ordering::Release),
        PROP_PARAMETER_LIST => n.params.store(true, Ordering::Release),
        _ => {}
    }
}

unsafe extern "C" fn input_proc(
    refcon: *mut c_void,
    _flags: *mut u32,
    _ts: *const TimeStamp,
    bus: u32,
    frames: u32,
    io: *mut BufferList,
) -> i32 {
    // SAFETY: `refcon` is the instance's InputCtx, which lives as long as the AudioUnit that calls this.
    let ctx = unsafe { &*refcon.cast::<InputCtx>() };
    ctx.in_flight.fetch_add(1, Ordering::SeqCst);
    let r = (|| {
        if !ctx.armed.load(Ordering::SeqCst)
            || bus != 0
            || io.is_null()
            || frames != ctx.frames.load(Ordering::SeqCst)
        {
            return ERR_NO_CONNECTION;
        }
        // SAFETY: `io` is the AU's AudioBufferList for this pull (non-null), at least its count readable.
        let n = unsafe { (*io).n } as usize;
        let ch = ctx.channels.load(Ordering::SeqCst) as usize;
        let max = ctx.max_frames.load(Ordering::SeqCst) as usize;
        let need = frames as usize * size_of::<f32>();
        // SAFETY: armed and counted in flight: `buf` is not being written (InputCtx's Sync reasoning).
        let buf = unsafe { &*ctx.buf.get() };
        for c in 0..n {
            // SAFETY: `c` < the list's own count; its buffers follow the count at offset 8 (BufferList's layout),
            // however many there are.
            let b = unsafe { &*(&raw const (*io).bufs).cast::<AudioBuffer>().add(c) };
            if b.data.is_null() || (b.bytes as usize) < need {
                return ERR_NO_CONNECTION;
            }
            let dst = b.data.cast::<f32>();
            if c < ch {
                // SAFETY: `dst` holds at least `frames` floats (checked); the source row `c` of `buf` holds
                // `max` >= `frames` floats; the two never overlap (the AU's buffer vs ours).
                unsafe {
                    std::ptr::copy_nonoverlapping(buf.as_ptr().add(c * max), dst, frames as usize)
                };
            } else {
                // SAFETY: as above, `dst` holds at least `frames` floats.
                unsafe { std::ptr::write_bytes(dst, 0, frames as usize) };
            }
        }
        0
    })();
    ctx.in_flight.fetch_sub(1, Ordering::SeqCst);
    r
}

fn bus_count(raw: &Raw, scope: u32) -> u32 {
    let (mut v, mut n) = (0u32, 4u32);
    // SAFETY: the unit is live; `v` is a live u32 of the size passed.
    let rc = unsafe {
        AudioUnitGetProperty(
            raw.unit.as_ptr(),
            PROP_ELEMENT_COUNT,
            scope,
            0,
            (&raw mut v).cast(),
            &mut n,
        )
    };
    if rc == 0 { v } else { 0 }
}

impl Component {
    /// `AudioComponentInstanceNew`, then the listener on `Latency` and `ParameterList`, wrapped in the post-load
    /// image diff: plugin code that loads a foreign image is an `Err` and a STOP.
    pub fn instantiate(&self) -> Result<Instance, Error> {
        let load = LoadCheck::begin().map_err(|e| Error::Refused(e.into()))?;
        let mut unit = std::ptr::null_mut();
        // SAFETY: `raw` is a registered component; `unit` is a live out-parameter.
        check(unsafe { AudioComponentInstanceNew(self.raw.as_ptr(), &mut unit) })?;
        let unit = NonNull::new(unit).ok_or_else(|| Error::Failed("no instance".into()))?;
        let mut raw = Raw {
            unit,
            desc: self.desc,
            origin: self.origin.clone(),
            listening: false,
            input: Box::new(InputCtx {
                armed: AtomicBool::new(false),
                in_flight: AtomicU32::new(0),
                frames: AtomicU32::new(0),
                channels: AtomicU32::new(0),
                buf: UnsafeCell::new(Vec::new()),
                max_frames: AtomicU32::new(0),
            }),
            notices: Box::default(),
            edits: Box::default(),
            edit_listener: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
            host: Box::new(HostCtx::new()),
            host_callbacks: false,
        };
        let refcon = (&raw const *raw.notices).cast_mut().cast();
        let mut listening = true;
        for id in [PROP_LATENCY, PROP_PARAMETER_LIST] {
            // SAFETY: the unit is live; `refcon` points at `notices`, which outlives the registration (Raw's drop).
            listening &=
                unsafe { AudioUnitAddPropertyListener(unit.as_ptr(), id, listener, refcon) } == 0;
        }
        raw.listening = listening;
        // HostCallbacks: the three procs, their refcon the instance's HostCtx (its Box never moves).
        let info = HostCallbackInfo {
            host_user_data: (&raw const *raw.host).cast_mut().cast(),
            beat_and_tempo: Some(host_beat_and_tempo),
            musical_time_location: Some(host_musical_time_location),
            transport_state: Some(host_transport_state),
            transport_state2: None,
        };
        // SAFETY: the unit is live; `info` is a live HostCallbackInfo of its declared size for the call (the AU copies
        // it); its refcon points at `raw.host`, which outlives the registration (Raw's drop clears it first).
        raw.host_callbacks = unsafe {
            AudioUnitSetProperty(
                unit.as_ptr(),
                PROP_HOST_CALLBACKS,
                SCOPE_GLOBAL,
                0,
                (&raw const info).cast(),
                size_of::<HostCallbackInfo>() as u32,
            )
        } == 0;
        // A foreign image loaded by the AU's construction: `raw` is dropped (the unit disposed) and the STOP stands.
        raw.origin.finish(load).map_err(Error::Refused)?;
        Ok(Instance {
            raw: Arc::new(raw),
            _not_sync: PhantomData,
        })
    }
}

// The parameter list and per-id info.
mod params;
pub use params::ParamInfo;

/// A created AU, message-thread side.
pub struct Instance {
    raw: Arc<Raw>,
    _not_sync: PhantomData<Cell<()>>,
}

/// A prepared AU, chain-thread side: [`Processor::render`]. Dropping it releases the AU.
pub struct Processor {
    raw: Arc<Raw>,
    out: Box<BufferList>,
    ts: TimeStamp,
    ch_in: usize,
    ch_out: usize,
    max_frames: u32,
    _not_sync: PhantomData<Cell<()>>,
}

impl Instance {
    fn unit(&self) -> *mut c_void {
        self.raw.unit.as_ptr()
    }

    /// The component description this instance was made from.
    pub fn desc(&self) -> Desc {
        self.raw.desc
    }

    /// The `AudioUnit` handle as an address, for the editor dylib's AU view: valid while this
    /// instance lives; the host closes the AU's editor window before it releases the plugin.
    pub fn unit_handle(&self) -> usize {
        self.unit() as usize
    }

    /// Whether the AU accepted `kAudioUnitProperty_HostCallbacks` (if not, it gets no beat, tempo or transport).
    pub fn host_callbacks(&self) -> bool {
        self.raw.host_callbacks
    }

    /// How many HostCallbacks reads on other threads gave up after four tries and answered from their last read.
    pub fn host_torn_reads(&self) -> u32 {
        self.raw.host.torn.load(Ordering::Relaxed)
    }

    /// Whether both property listeners were accepted. If not, the AU's own latency notice cannot arrive: the
    /// host re-reads this instance's latency on its periodic wake.
    pub fn listening(&self) -> bool {
        self.raw.listening
    }

    /// The bus count of `scope` (`kAudioUnitProperty_ElementCount`), 0 when the AU does not say.
    pub fn bus_count(&self, scope: u32) -> u32 {
        bus_count(&self.raw, scope)
    }

    /// `kAudioUnitProperty_OfflineRender` as the AU reports it: `None` when the AU does not support the property. The
    /// child never sets it, so an AU that stores it reads back `Some(false)` after any prepare, a render set's
    /// included. **A check for tests and the control thread only:** never call it on a chain thread; a property get
    /// from the control thread while the AU renders is allowed by the AU API and benign for this property.
    pub fn offline_render(&self) -> Option<bool> {
        let mut v: u32 = 0;
        let mut n = 4u32;
        // SAFETY: the unit is live; `v` is a live u32 of the size passed.
        let rc = unsafe {
            AudioUnitGetProperty(
                self.unit(),
                PROP_OFFLINE_RENDER,
                SCOPE_GLOBAL,
                0,
                (&raw mut v).cast(),
                &mut n,
            )
        };
        (rc == 0).then_some(v != 0)
    }

    /// The scan's view: the channels of `scope`'s element 0 as the AU's current stream format says (0 with no bus, or
    /// when the AU does not say): the AU's default layout before a prepare.
    pub fn stream_channels(&self, scope: u32) -> u32 {
        if bus_count(&self.raw, scope) == 0 {
            return 0;
        }
        let mut f = Asbd::default();
        let mut n = size_of::<Asbd>() as u32;
        // SAFETY: the unit is live; `f` is a live AudioStreamBasicDescription of the size passed.
        let rc = unsafe {
            AudioUnitGetProperty(
                self.unit(),
                PROP_STREAM_FORMAT,
                scope,
                0,
                (&raw mut f).cast(),
                &mut n,
            )
        };
        if rc == 0 { f.channels } else { 0 }
    }

    /// `AudioUnitGetParameter` (global scope).
    pub fn param(&self, id: u32) -> Result<f32, Error> {
        let mut v = 0f32;
        // SAFETY: the unit is live; `v` is a live out-parameter.
        check(unsafe { AudioUnitGetParameter(self.unit(), id, SCOPE_GLOBAL, 0, &mut v) })?;
        Ok(v)
    }

    /// `AudioUnitSetParameter` (global scope, offset 0) from the message thread.
    pub fn set_param(&self, id: u32, v: f32) -> Result<(), Error> {
        // SAFETY: the unit is live.
        check(unsafe { AudioUnitSetParameter(self.unit(), id, SCOPE_GLOBAL, 0, v, 0) })
    }

    /// `kAudioUnitProperty_Latency` in seconds (0 when the AU does not say: the read is zero-initialised).
    pub fn latency_seconds(&self) -> f64 {
        let (mut v, mut n) = (0f64, 8u32);
        // SAFETY: the unit is live; `v` is a live f64 of the size passed.
        let rc = unsafe {
            AudioUnitGetProperty(
                self.unit(),
                PROP_LATENCY,
                SCOPE_GLOBAL,
                0,
                (&raw mut v).cast(),
                &mut n,
            )
        };
        if rc == 0 { v } else { 0.0 }
    }

    /// The AU's own parameter edits (they move the slot's `childGen`): an `AUEventListener` on the MAIN run loop (where
    /// the editor's events arrive; the message thread pumps it while an editor is open), one `ParameterValueChange` per
    /// parameter of `ids` (global scope), its callback only a counter bump. Main thread, once. True when it is
    /// listening.
    pub fn listen_for_edits(&self, ids: &[u32]) -> bool {
        if !self.raw.edit_listener.load(Ordering::Acquire).is_null() {
            return true;
        }
        let mut l: *mut c_void = std::ptr::null_mut();
        let user = (&raw const *self.raw.edits).cast_mut().cast();
        // SAFETY: `au_edit_event` has the AUEventListenerProc signature; `user` points at `edits`, which outlives the
        // listener (Raw's drop disposes it first); the main run loop and its default mode are CoreFoundation's own; `l`
        // is a live out-parameter.
        let rc = unsafe {
            AUEventListenerCreate(
                au_edit_event,
                user,
                CFRunLoopGetMain(),
                kCFRunLoopDefaultMode,
                0.01,
                0.0,
                &mut l,
            )
        };
        if rc != 0 || l.is_null() {
            return false;
        }
        for &id in ids {
            let e = AuEvent {
                event_type: AU_EVENT_PARAMETER_VALUE_CHANGE,
                arg: AuEventArg {
                    unit: self.unit(),
                    id,
                    scope: SCOPE_GLOBAL,
                    element: 0,
                },
            };
            // SAFETY: `l` is the live listener just created; `e` is a live AudioUnitEvent naming this live unit.
            unsafe { AUEventListenerAddEventType(l, std::ptr::null_mut(), &e) };
        }
        self.raw.edit_listener.store(l, Ordering::Release);
        true
    }

    /// Tells the AU's listeners (its editor's controls) that the host changed parameter `id` (a parameter-change
    /// event after the host's `AudioUnitSetParameter`), with this instance's own edit listener as
    /// the sender, which AudioToolbox does not call back: the host's values are never counted as the AU's own edits.
    /// Message thread (never the chain thread: it signals no other thread).
    pub fn notify_param_changed(&self, id: u32) {
        let e = AuEvent {
            event_type: AU_EVENT_PARAMETER_VALUE_CHANGE,
            arg: AuEventArg {
                unit: self.unit(),
                id,
                scope: SCOPE_GLOBAL,
                element: 0,
            },
        };
        let sender = self.raw.edit_listener.load(Ordering::Acquire);
        // SAFETY: `e` is a live AudioUnitEvent naming this live unit; `sender` is this instance's live listener or null
        // (AudioToolbox then notifies every listener); a null sending object is allowed.
        unsafe { AUEventListenerNotify(sender, std::ptr::null_mut(), &e) };
    }

    /// The AU's own parameter edits counted so far ([`Instance::listen_for_edits`]).
    pub fn edit_count(&self) -> u64 {
        self.raw.edits.load(Ordering::Acquire)
    }

    /// What the listener saw since the last call: (latency, parameter list). Makes no AudioToolbox call.
    pub fn take_notices(&self) -> (bool, bool) {
        let n = &self.raw.notices;
        (
            n.latency.swap(false, Ordering::AcqRel),
            n.params.swap(false, Ordering::AcqRel),
        )
    }

    /// The state: `ClassInfo` as a binary v1.0 plist.
    pub fn class_info(&self) -> Result<Vec<u8>, Error> {
        let mut plist: *const c_void = std::ptr::null();
        let mut n = size_of::<*const c_void>() as u32;
        // SAFETY: the unit is live; `plist` is a live pointer-sized out-parameter; on success it is +1 (the property's
        // contract), owned below.
        check(unsafe {
            AudioUnitGetProperty(
                self.unit(),
                PROP_CLASS_INFO,
                SCOPE_GLOBAL,
                0,
                (&raw mut plist).cast(),
                &mut n,
            )
        })?;
        let plist = Cf::new(plist).ok_or_else(|| Error::Failed("no ClassInfo".into()))?;
        plist_bytes(plist.get())
    }

    /// Applies a state from [`Instance::class_info`]'s bytes: parsed as a plist and set as `ClassInfo`. A parse failure
    /// or a refused `ClassInfo` is an `Err`.
    pub fn set_class_info(&self, bytes: &[u8]) -> Result<(), Error> {
        let plist =
            parse_plist(bytes).ok_or_else(|| Error::Failed("not a property list".into()))?;
        let p = plist.get();
        // SAFETY: the unit is live; the data is a pointer to the CFPropertyListRef, of pointer size (the property's
        // contract); the AU retains what it keeps.
        check(unsafe {
            AudioUnitSetProperty(
                self.unit(),
                PROP_CLASS_INFO,
                SCOPE_GLOBAL,
                0,
                (&raw const p).cast(),
                size_of::<*const c_void>() as u32,
            )
        })
    }

    /// The prepare for one input and one output bus: the stream formats (rate, channels,
    /// non-interleaved f32), the input callback, `MaximumFramesPerSlice`, then `AudioUnitInitialize`. A music device
    /// gets no input. Refused while a processor lives.
    pub fn prepare(
        &mut self,
        rate: f64,
        max_frames: u32,
        ch_in: u32,
        ch_out: u32,
    ) -> Result<Processor, Error> {
        self.prepare_mode(rate, max_frames, ch_in, ch_out, false)
    }

    /// [`Instance::prepare`], for a live slot or a render set's. `offline` changes nothing in the AU:
    /// `kAudioUnitProperty_OfflineRender` is never set, so a bounce renders an AU as live, as the host application
    /// expects. A render set's offline meaning stays the host's (its chain
    /// thread's pacing and the VST3 process mode), not the AU's.
    pub fn prepare_mode(
        &mut self,
        rate: f64,
        max_frames: u32,
        ch_in: u32,
        ch_out: u32,
        offline: bool,
    ) -> Result<Processor, Error> {
        if Arc::strong_count(&self.raw) > 1 {
            return Err(Error::Refused("a processor is alive".into()));
        }
        if ch_in as usize > MAX_CH || ch_out as usize > MAX_CH || ch_out == 0 || max_frames == 0 {
            return Err(Error::Refused("channels or frames".into()));
        }
        let u = self.unit();
        let fmt = |ch: u32| Asbd {
            rate,
            format_id: FORMAT_LPCM,
            format_flags: FORMAT_FLOAT_PACKED_NONINTERLEAVED,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels: ch,
            bits: 32,
            reserved: 0,
        };
        let has_input = ch_in > 0 && self.bus_count(SCOPE_INPUT) > 0;
        // The input context holds the channels the unit really pulls, so a unit without
        // an input bus (most instruments) gets no input buffer.
        let eff_in = if has_input { ch_in } else { 0 };
        let ctx = &self.raw.input;
        // No processor lives (checked), so no render runs and the context is disarmed: the buffer may be resized.
        // Resident before the render thread's first pull writes it (as the host's chain scratch).
        let buf =
            runecraft_rt::os::prefaulted(0.0f32, eff_in as usize * max_frames as usize).into_vec();
        // SAFETY: disarmed and not in flight (InputCtx's Sync reasoning); nothing else holds a reference.
        unsafe { *ctx.buf.get() = buf };
        ctx.channels.store(eff_in, Ordering::SeqCst);
        ctx.max_frames.store(max_frames, Ordering::SeqCst);
        for (scope, ch, on) in [
            (SCOPE_INPUT, ch_in, has_input),
            (SCOPE_OUTPUT, ch_out, true),
        ] {
            if !on {
                continue;
            }
            let (mut sr, mut n) = (0f64, 8u32);
            // SAFETY: the unit is live; `sr` is a live f64 of the size passed.
            unsafe {
                AudioUnitGetProperty(u, PROP_SAMPLE_RATE, scope, 0, (&raw mut sr).cast(), &mut n)
            };
            if sr != rate {
                // SAFETY: the unit is live; the data is an f64 of the size passed.
                unsafe {
                    AudioUnitSetProperty(u, PROP_SAMPLE_RATE, scope, 0, (&raw const rate).cast(), 8)
                };
            }
            let f = fmt(ch);
            // SAFETY: the unit is live; `f` is an AudioStreamBasicDescription of the size passed.
            check(unsafe {
                AudioUnitSetProperty(u, PROP_STREAM_FORMAT, scope, 0, (&raw const f).cast(), 40)
            })?;
        }
        if has_input {
            let cb = RenderCallback {
                proc_: Some(input_proc),
                refcon: (&raw const **ctx).cast_mut().cast(),
            };
            // SAFETY: the unit is live; `cb` is an AURenderCallbackStruct of the size passed; its refcon is the
            // instance's InputCtx, which lives as long as the unit.
            check(unsafe {
                AudioUnitSetProperty(
                    u,
                    PROP_SET_RENDER_CALLBACK,
                    SCOPE_INPUT,
                    0,
                    (&raw const cb).cast(),
                    16,
                )
            })?;
        }
        // SAFETY: the unit is live; a u32 of the size passed.
        check(unsafe {
            AudioUnitSetProperty(
                u,
                PROP_MAX_FRAMES,
                SCOPE_GLOBAL,
                0,
                (&raw const max_frames).cast(),
                4,
            )
        })?;
        // Never kAudioUnitProperty_OfflineRender, live or offline (a bounce sounds as live);
        // `offline` is the host's meaning only. au_sys.rs's offline scenario (an AU that stores the property) proves
        // it.
        let _ = offline;
        // Wrapped in the post-load image diff.
        let load = LoadCheck::begin().map_err(|e| Error::Refused(e.into()))?;
        // SAFETY: the unit is live and not initialized (no processor lives; a processor's drop uninitializes).
        check(unsafe { AudioUnitInitialize(u) })?;
        if let Err(e) = self.raw.origin.finish(load) {
            // SAFETY: the unit is live and was initialized just above; uninitialized once, here.
            unsafe { AudioUnitUninitialize(u) };
            return Err(Error::Refused(e));
        }
        Ok(Processor {
            raw: self.raw.clone(),
            out: Box::new(BufferList {
                n: ch_out,
                bufs: [AudioBuffer {
                    channels: 1,
                    bytes: 0,
                    data: std::ptr::null_mut(),
                }; MAX_CH],
            }),
            ts: TimeStamp {
                flags: TS_SAMPLE_TIME_VALID,
                ..TimeStamp::default()
            },
            ch_in: if has_input { ch_in as usize } else { 0 },
            ch_out: ch_out as usize,
            max_frames,
            _not_sync: PhantomData,
        })
    }
}

// SAFETY: the output list's pointers are null between renders and are set only inside `render(&mut self)`, on the
// thread that owns the processor; the rest is Raw (Send) and plain data. Not Sync (PhantomData<Cell>).
unsafe impl Send for Processor {}

impl Processor {
    fn unit(&self) -> *mut c_void {
        self.raw.unit.as_ptr()
    }

    /// The channels this processor renders: `(inputs, outputs)` as prepared. The input count is the effective one,
    /// 0 when the unit has no input bus (or was prepared with none), whatever the host asked for.
    pub fn channels(&self) -> (u32, u32) {
        (self.ch_in as u32, self.ch_out as u32)
    }

    /// `AudioUnitSetParameter` (global, offset 0), on the chain thread before the render.
    pub fn set_param(&mut self, id: u32, v: f32) {
        // SAFETY: the unit is live and initialized.
        unsafe { AudioUnitSetParameter(self.unit(), id, SCOPE_GLOBAL, 0, v, 0) };
    }

    /// `MusicDeviceMIDIEvent` (the path for messages of at most 3 bytes).
    pub fn midi(&mut self, status: u8, d1: u8, d2: u8, offset: u32) {
        // SAFETY: the unit is live and initialized.
        unsafe { MusicDeviceMIDIEvent(self.unit(), status.into(), d1.into(), d2.into(), offset) };
    }

    /// `MusicDeviceSysEx` (the path for longer messages).
    pub fn sysex(&mut self, bytes: &[u8]) {
        // SAFETY: the unit is live; `bytes` is valid for its length during the call.
        unsafe { MusicDeviceSysEx(self.unit(), bytes.as_ptr(), bytes.len() as u32) };
    }

    /// The block's transport for the HostCallbacks, published before [`Processor::render`] (the render thread, the
    /// one writer: `&mut self`). Atomics only, no allocation, no lock ([`HostCtx`]'s seqlock).
    pub fn publish_transport(&mut self, t: &HostTransport) {
        self.raw.host.publish(t);
    }

    /// One render of `frames`: `inputs` copied into the input context (armed only for this render), the output
    /// list pointing at `outputs`, `AudioUnitRender` on bus 0, then `mSampleTime += frames`. `host_time`: the block's
    /// host time in ticks, if any (the time stamp's host-time flag is set only then).
    pub fn render(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        frames: usize,
        host_time: Option<u64>,
    ) -> Result<(), Error> {
        if frames == 0 || frames > self.max_frames as usize || outputs.len() < self.ch_out {
            return Err(Error::Refused("frames or outputs".into()));
        }
        if outputs.iter().take(self.ch_out).any(|o| o.len() < frames)
            || inputs.iter().any(|i| i.len() < frames)
        {
            return Err(Error::Refused("short buffers".into()));
        }
        let ctx = &self.raw.input;
        let max = self.max_frames as usize;
        {
            // SAFETY: disarmed and no pull in flight (the previous render waited for it): no reader (InputCtx).
            let buf = unsafe { &mut *ctx.buf.get() };
            for c in 0..self.ch_in {
                let row = &mut buf[c * max..c * max + frames];
                match inputs.get(c) {
                    Some(x) => row.copy_from_slice(&x[..frames]),
                    None => row.fill(0.0),
                }
            }
        }
        ctx.frames.store(frames as u32, Ordering::SeqCst);
        let bytes = (frames * size_of::<f32>()) as u32;
        let mut ours = [std::ptr::null_mut::<c_void>(); MAX_CH];
        for ((b, o), p) in self
            .out
            .bufs
            .iter_mut()
            .zip(outputs.iter_mut())
            .zip(ours.iter_mut())
            .take(self.ch_out)
        {
            b.channels = 1;
            b.bytes = bytes;
            b.data = o.as_mut_ptr().cast();
            *p = b.data;
        }
        self.out.n = self.ch_out as u32;
        match host_time {
            Some(t) => {
                self.ts.host_time = t;
                self.ts.flags |= TS_HOST_TIME_VALID;
            }
            None => {
                self.ts.host_time = 0;
                self.ts.flags &= !TS_HOST_TIME_VALID;
            }
        }
        let mut flags = 0u32;
        ctx.armed.store(true, Ordering::SeqCst);
        // SAFETY: the unit is live and initialized; `ts` and the output list are live for the call, the list's
        // buffers point at `outputs` (each at least `frames` floats, checked); the input callback reads only the armed
        // context.
        let rc = unsafe {
            AudioUnitRender(
                self.unit(),
                &mut flags,
                &self.ts,
                0,
                frames as u32,
                &mut *self.out,
            )
        };
        ctx.armed.store(false, Ordering::SeqCst);
        while ctx.in_flight.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
        // The AU must render into our buffers. A list it rewrote (the count, a pointer or a size) means the block's
        // audio is not in `outputs`: a failed block (THREW and silence), never the stale buffers.
        let swapped = self.out.n != self.ch_out as u32
            || self
                .out
                .bufs
                .iter()
                .zip(ours.iter())
                .take(self.ch_out)
                .any(|(b, p)| b.data != *p || b.bytes != bytes);
        for b in self.out.bufs.iter_mut() {
            b.data = std::ptr::null_mut();
        }
        self.ts.sample_time += frames as f64;
        check(rc)?;
        if swapped {
            return Err(Error::Failed(
                "the AU rewrote the output buffer list".into(),
            ));
        }
        Ok(())
    }
}

/// The prefix of the host's chain-thread names (`crates/summoner-host/src/chain.rs`: `hc-chain-<key>`).
pub const CHAIN_THREAD_PREFIX: &str = "hc-chain-";

static WRONG_THREAD_DROPS: AtomicU32 = AtomicU32::new(0);

/// How many processors were dropped on a chain thread (the release allocates and locks, so it belongs
/// on the message thread). The tests assert 0.
pub fn wrong_thread_drops() -> u32 {
    WRONG_THREAD_DROPS.load(Ordering::SeqCst)
}

/// The release-thread check, run by every processor's drop: on a chain thread it is counted and logged (never a panic
/// in a drop).
#[doc(hidden)]
pub fn note_drop_thread() {
    if std::thread::current()
        .name()
        .is_some_and(|n| n.starts_with(CHAIN_THREAD_PREFIX))
    {
        WRONG_THREAD_DROPS.fetch_add(1, Ordering::SeqCst);
        eprintln!("[summoner-sys] an AU processor was released on a chain thread");
    }
}

impl Drop for Processor {
    /// The release: reset every input and output bus, then global, then uninitialize. On the message thread only
    /// (counted otherwise).
    fn drop(&mut self) {
        note_drop_thread();
        let u = self.unit();
        for scope in [SCOPE_INPUT, SCOPE_OUTPUT] {
            for el in 0..bus_count(&self.raw, scope) {
                // SAFETY: the unit is live.
                unsafe { AudioUnitReset(u, scope, el) };
            }
        }
        // SAFETY: the unit is live and initialized by `prepare`; uninitialized once, here.
        unsafe {
            AudioUnitReset(u, SCOPE_GLOBAL, 0);
            AudioUnitUninitialize(u);
        }
    }
}

/// A property list as binary v1.0 bytes (`CFPropertyListWrite` to an allocated-buffers stream).
fn plist_bytes(plist: *const c_void) -> Result<Vec<u8>, Error> {
    // SAFETY: default allocators; the result is +1, owned by `s`.
    let s = Cf::new(unsafe {
        CFWriteStreamCreateWithAllocatedBuffers(std::ptr::null(), std::ptr::null())
    })
    .ok_or_else(|| Error::Failed("no stream".into()))?;
    // SAFETY: `s` is a live write stream; `plist` a live property list; the error out-parameter may be null.
    let written = unsafe {
        CFWriteStreamOpen(s.get());
        let w = CFPropertyListWrite(plist, s.get(), CF_PLIST_BINARY_V1, 0, std::ptr::null_mut());
        CFWriteStreamClose(s.get());
        w
    };
    // SAFETY: `s` is live; the key is CF's own constant; the result is +1 (Copy rule), owned by `data`.
    let data = Cf::new(unsafe { CFWriteStreamCopyProperty(s.get(), kCFStreamPropertyDataWritten) })
        .ok_or_else(|| Error::Failed("no data written".into()))?;
    // SAFETY: `data` is a live CFData: its byte pointer is valid for its length while it lives; copied out at once.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            CFDataGetBytePtr(data.get()),
            CFDataGetLength(data.get()) as usize,
        )
    };
    // The buffer is sized by CFPropertyListWrite's count, and that many bytes are copied.
    let n = (written.max(0) as usize).min(bytes.len());
    if n == 0 {
        return Err(Error::Failed("CFPropertyListWrite wrote nothing".into()));
    }
    Ok(bytes[..n].to_vec())
}

/// Bytes parsed as a property list (`CFPropertyListCreateWithStream`, immutable), or `None`.
fn parse_plist(bytes: &[u8]) -> Option<Cf> {
    // SAFETY: `bytes` outlives the stream (both end in this function); kCFAllocatorNull: the stream does not free it.
    let s = Cf::new(unsafe {
        CFReadStreamCreateWithBytesNoCopy(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as isize,
            kCFAllocatorNull,
        )
    })?;
    let mut fmt = CF_PLIST_BINARY_V1;
    // SAFETY: `s` is a live read stream over `bytes`; `fmt` a live out-parameter; the error out-parameter may be null;
    // the result is +1 (Create rule).
    let p = unsafe {
        CFReadStreamOpen(s.get());
        let p = CFPropertyListCreateWithStream(
            std::ptr::null(),
            s.get(),
            0,
            CF_PLIST_IMMUTABLE,
            &mut fmt,
            std::ptr::null_mut(),
        );
        CFReadStreamClose(s.get());
        p
    };
    Cf::new(p)
}

/// The bytes of a CFData value under `key` in a dictionary property list (the lab AUs' `summoner-lab-state`), or `None`.
pub fn plist_data(bytes: &[u8], key: &str) -> Option<Vec<u8>> {
    let p = parse_plist(bytes)?;
    // SAFETY: `p` is a live property list.
    if unsafe { CFGetTypeID(p.get()) != CFDictionaryGetTypeID() } {
        return None;
    }
    let v = dict_get(p.get(), key);
    // SAFETY: null- and type-checked; the byte pointer is valid for its length while `p` lives; copied out at once.
    unsafe {
        if v.is_null() || CFGetTypeID(v) != CFDataGetTypeID() {
            return None;
        }
        Some(std::slice::from_raw_parts(CFDataGetBytePtr(v), CFDataGetLength(v) as usize).to_vec())
    }
}

/// Whether two byte strings parse to equal property lists (`CFEqual`): the fallback comparison when the binary plists
/// differ. `None` if either does not parse.
pub fn plist_equal(a: &[u8], b: &[u8]) -> Option<bool> {
    let (a, b) = (parse_plist(a)?, parse_plist(b)?);
    // SAFETY: both are live property lists.
    Some(unsafe { CFEqual(a.get(), b.get()) } != 0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_drop_on_a_chain_thread_is_counted() {
        let before = super::wrong_thread_drops();
        super::note_drop_thread(); // the test thread: not a chain thread
        assert_eq!(super::wrong_thread_drops(), before);
        std::thread::Builder::new()
            .name("hc-chain-7".into())
            .spawn(super::note_drop_thread)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(super::wrong_thread_drops(), before + 1);
    }

    #[test]
    fn os_types_read_big_endian_space_padded() {
        assert_eq!(super::os_type("aufx"), super::TYPE_EFFECT);
        assert_eq!(super::os_type("ab"), u32::from_be_bytes(*b"ab  "));
    }

    /// The three HostCallbacks answer from the transport, with their fallbacks, NULL out-pointers skipped, always
    /// noErr.
    #[test]
    fn the_host_callbacks_answer_as_the_oracle_childs_with_its_fallbacks() {
        use super::{HostCtx, HostTransport};
        let h = HostCtx::new();
        let r = (&raw const h).cast_mut().cast();
        let beat = |h: *mut std::ffi::c_void| {
            let (mut b, mut t) = (-9.0, -9.0);
            // SAFETY: `h` is a live HostCtx; both out-pointers are live f64s.
            let rc = unsafe { super::host_beat_and_tempo(h, &mut b, &mut t) };
            (rc, b, t)
        };
        let musical = |h: *mut std::ffi::c_void| {
            let (mut n, mut num, mut den, mut d) = (9u32, -9.0f32, 9u32, -9.0f64);
            // SAFETY: as above; the out-pointers are live values of their types.
            let rc =
                unsafe { super::host_musical_time_location(h, &mut n, &mut num, &mut den, &mut d) };
            (rc, n, num, den, d)
        };
        let transport = |h: *mut std::ffi::c_void| {
            let (mut p, mut c, mut s, mut cy, mut a, mut b) = (9u8, 9u8, -9.0, 9u8, -9.0, -9.0);
            // SAFETY: as above.
            let rc = unsafe {
                super::host_transport_state(h, &mut p, &mut c, &mut s, &mut cy, &mut a, &mut b)
            };
            (rc, p, c, s, cy, a, b)
        };
        // Nothing published: the default answers (beat 0, tempo 120, 4/4, stopped, sample
        // 0).
        assert_eq!(beat(r), (0, 0.0, 120.0));
        assert_eq!(musical(r), (0, 0, 4.0, 4, 0.0));
        assert_eq!(transport(r), (0, 0, 0, 0.0, 0, 0.0, 0.0));
        // A full transport.
        h.publish(&HostTransport {
            ppq: 7.5,
            bpm: 96.0,
            time_sig_num: 3,
            time_sig_den: 8,
            bar_start_ppq: 6.0,
            bar_valid: true,
            loop_start_ppq: 4.0,
            loop_end_ppq: 8.0,
            loop_valid: true,
            playing: true,
            looping: true,
            time_samples: 48_000,
        });
        assert_eq!(beat(r), (0, 7.5, 96.0));
        assert_eq!(musical(r), (0, 0, 3.0, 8, 6.0));
        assert_eq!(
            transport(r),
            (0, 1, 1, 48_000.0, 1, 4.0, 8.0),
            "changed: stopped → playing"
        );
        assert_eq!(transport(r).2, 0, "changed is per call: not again");
        // The fallbacks: no bpm, half a time signature, no bar start, no loop points; and a stop.
        h.publish(&HostTransport {
            ppq: 2.0,
            time_sig_num: 3,
            bar_start_ppq: 6.0,
            loop_start_ppq: 4.0,
            loop_end_ppq: 8.0,
            looping: true,
            time_samples: 96_000,
            ..HostTransport::default()
        });
        assert_eq!(beat(r), (0, 2.0, 120.0));
        assert_eq!(musical(r), (0, 0, 4.0, 4, 0.0));
        assert_eq!(
            transport(r),
            (0, 0, 1, 96_000.0, 1, 0.0, 0.0),
            "changed: playing → stopped"
        );
        // NULL out-pointers are skipped.
        // SAFETY: `r` is a live HostCtx; every out-pointer NULL.
        let rc = unsafe {
            super::host_beat_and_tempo(r, std::ptr::null_mut(), std::ptr::null_mut())
                | super::host_musical_time_location(
                    r,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
                | super::host_transport_state(
                    r,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
        };
        assert_eq!(rc, 0);
        assert_eq!(h.torn.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    /// The seqlock under stress (no loom here): one writer publishing snapshots whose every field encodes the
    /// same k, one reader on another thread; every snapshot the reader ACCEPTS is one k's, and it accepts many.
    #[test]
    fn a_cross_thread_reader_never_accepts_a_torn_snapshot() {
        use super::{HostCtx, HostTransport};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let h = Arc::new(HostCtx::new());
        let stop = Arc::new(AtomicBool::new(false));
        let at = |k: u64| HostTransport {
            ppq: k as f64,
            bpm: k as f64 + 0.5,
            time_sig_num: (k % 60_000) as u32 + 1,
            time_sig_den: (k % 60_000) as u32 + 2,
            bar_start_ppq: k as f64 + 0.25,
            bar_valid: k.is_multiple_of(2),
            loop_start_ppq: k as f64 + 0.125,
            loop_end_ppq: k as f64 + 0.0625,
            loop_valid: k % 2 == 1,
            playing: k.is_multiple_of(2),
            looping: k % 2 == 1,
            time_samples: k,
        };
        let writer = {
            let (h, stop) = (h.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut k = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    h.publish(&at(k));
                    k += 1;
                }
                k
            })
        };
        let (mut accepted, mut torn) = (0u64, 0u64);
        for _ in 0..2_000_000 {
            let (t, ok) = h.read();
            if ok {
                accepted += 1;
                // One block's, or the state before the writer's first publish (all zero): never a mix.
                assert!(
                    t == at(t.time_samples) || t == HostTransport::default(),
                    "an accepted snapshot is one block's: {t:?}"
                );
            } else {
                torn += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let written = writer.join().unwrap();
        assert!(written > 1000, "the writer ran: {written}");
        assert!(
            accepted > 1000,
            "consistent reads happen: {accepted} accepted, {torn} gave up"
        );
    }
}
