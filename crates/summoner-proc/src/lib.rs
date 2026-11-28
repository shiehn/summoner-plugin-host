//! `summoner-proc`: what the child hosts in one slot of a chain.
//!
//! A slot is split by thread, as a VST3 component / processor or an AU's render and property calls are used:
//!
//! - [`Plugin`]: shared (`Send + Sync`), called on the message thread (state, parameters' names, prepare, the editor)
//!   and, for [`Plugin::set_param`] only, on the chain thread before the block;
//! - [`Dsp`]: owned by the chain thread once [`Plugin::prepare`] made it; [`Dsp::process`] runs the block in place.
//!
//! The host owns everything protocol-shaped (the region, MIDI order, the reset's all-notes-off, the transport line,
//! status and ack); an implementation sees only plain buffers. The test double (`summoner-testproc`) is the first
//! implementation; VST3 and AU come later.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

/// The request line's transport as the block's processors see it, already decoded from the slot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transport {
    /// The position in quarter notes.
    pub ppq: f64,
    /// The tempo in beats per minute.
    pub bpm: f64,
    /// The raw `S_TIME_SIG` word: numerator in the low 16 bits, denominator in the high 16.
    pub time_sig: u32,
    /// The loop's start in quarter notes (valid with `BF_LOOP_POINTS_VALID`).
    pub loop_start_ppq: f64,
    /// The loop's end in quarter notes (valid with `BF_LOOP_POINTS_VALID`).
    pub loop_end_ppq: f64,
    /// The current bar's start in quarter notes (valid with `BF_BAR_START_VALID`).
    pub bar_start_ppq: f64,
    /// `S_BLOCK_FLAGS` (`BF_*`).
    pub flags: u32,
    /// The position in seconds.
    pub time_seconds: f64,
    /// The position in samples.
    pub time_samples: u64,
    /// `S_OUTPUT_HOST_TIME` in host ticks (0: none).
    pub output_host_time: u64,
    /// The same time in nanoseconds (0: none), converted by the host (`runecraft_ipc::bridge::ticks_to_ns`).
    pub output_host_time_ns: u64,
}

/// One MIDI event of the block: `len` bytes of `data` at frame `offset` (already clamped into the block).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiEvent {
    /// The frame in the block the event falls on.
    pub offset: u32,
    /// How many bytes of `data` the event uses.
    pub len: u8,
    /// The event's bytes (the first `len` are used).
    pub data: [u8; 16],
}

// Plain values, so the chain thread's wired MIDI buffer may hold it (each field asserted).
impl runecraft_rt::arena::RtMem for MidiEvent {}
const _: () = {
    runecraft_rt::arena::assert_rt_mem::<u32>();
    runecraft_rt::arena::assert_rt_mem::<u8>();
    runecraft_rt::arena::assert_rt_mem::<[u8; 16]>();
};

impl MidiEvent {
    /// A 3-byte event (a note, a controller, ...) at frame `offset`.
    pub fn short(offset: u32, b: [u8; 3]) -> MidiEvent {
        let mut data = [0; 16];
        data[..3].copy_from_slice(&b);
        MidiEvent {
            offset,
            len: 3,
            data,
        }
    }

    /// Byte `i`, or 0 past `len`.
    #[inline]
    pub fn byte(&self, i: usize) -> u8 {
        if i < self.len as usize {
            self.data[i]
        } else {
            0
        }
    }
}

/// The block a [`Dsp`] processes in place: `frames` frames of every buffer channel (at least 2; inputs copied in,
/// the rest zero), the block's MIDI sorted by offset (stable), and the transport.
pub struct Block<'a> {
    /// The frames to process in every channel.
    pub frames: usize,
    /// Wired arena slices, `max_frames` long each.
    pub channels: &'a mut [runecraft_rt::arena::ArenaSlice<f32>],
    /// The block's MIDI, sorted by offset.
    pub midi: &'a [MidiEvent],
    /// The block's transport.
    pub transport: &'a Transport,
}

/// The arena owner tag of this child's realtime blocks (the host's, the test double's, the formats' Rust halves):
/// `Owner::Host`.
pub const RT_OWNER: runecraft_rt::arena::Owner = runecraft_rt::arena::Owner::Host;

/// A line of `n` zeros in the wired arena (message thread only: the arena is never called on a chain thread).
pub fn rt_line(n: usize) -> runecraft_rt::arena::ArenaSlice<f32> {
    runecraft_rt::arena::ArenaSlice::filled(RT_OWNER, n, 0.0f32)
}

/// A fixed-capacity vector in wired arena memory: `push` past the capacity is refused (never a growth), `clear` keeps
/// the block. It derefs to the filled part, so the chain thread reads it as a slice.
pub struct RtVec<T: Copy> {
    buf: runecraft_rt::arena::ArenaSlice<T>,
    len: usize,
}

impl<T: Copy> RtVec<T> {
    /// `cap` slots, each `fill` (message thread: the arena is never called on a chain thread).
    pub fn new(cap: usize, fill: T) -> RtVec<T> {
        RtVec {
            buf: runecraft_rt::arena::ArenaSlice::filled(RT_OWNER, cap, fill),
            len: 0,
        }
    }

    /// Appends `v` if there is room; `false` when full (nothing grows).
    #[inline]
    pub fn push(&mut self, v: T) -> bool {
        if self.len < self.buf.len() {
            self.buf[self.len] = v;
            self.len += 1;
            true
        } else {
            false
        }
    }

    /// Empties the vector (the capacity stays).
    #[inline]
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// How many values fit.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// The whole block, filled part or not (for the wired-memory inventory check).
    pub fn storage(&self) -> &[T] {
        &self.buf
    }
}

// A wired slice and a length.
impl<T: Copy + runecraft_rt::arena::RtMem> runecraft_rt::arena::RtMem for RtVec<T> {}

impl<T: Copy> std::ops::Deref for RtVec<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.buf[..self.len]
    }
}

impl<T: Copy> std::ops::DerefMut for RtVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.buf[..self.len]
    }
}

/// Channel buffers for a caller that is not realtime (the scan's validation, the probes, tests): the arena's unwired
/// mode (`Owner::Render`: the system heap behind the same handle type), `n` channels of `frames` each, all `value`.
pub fn unwired_channels(
    n: usize,
    frames: usize,
    value: f32,
) -> Vec<runecraft_rt::arena::ArenaSlice<f32>> {
    (0..n)
        .map(|_| {
            runecraft_rt::arena::ArenaSlice::filled(
                runecraft_rt::arena::Owner::Render,
                frames,
                value,
            )
        })
        .collect()
}

std::thread_local! {
    /// [`with_channels`]' buffers, reused across calls on this thread (grown only when a call needs more).
    static POOL: std::cell::RefCell<Vec<runecraft_rt::arena::ArenaSlice<f32>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// [`with_channels`]' pool made ready for `n` channels of `frames` on this thread, so a caller that counts its
/// allocations (a probe's 64 blocks) calls this before it arms its counter.
pub fn warm_channels(n: usize, frames: usize) {
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < n || p.iter().take(n).any(|b| b.len() < frames) {
            let len = frames.max(p.first().map_or(0, |b| b.len()));
            *p = unwired_channels(n.max(p.len()), len, 0.0);
        }
    });
}

/// For a caller that is not realtime and keeps its channels in `Vec`s (the probes, tests): `chans` copied into this
/// thread's reused unwired buffers (no allocation once [`warm_channels`] made them big enough), `f` run on them, and
/// the results copied back.
pub fn with_channels<R>(
    chans: &mut [Vec<f32>],
    f: impl FnOnce(&mut [runecraft_rt::arena::ArenaSlice<f32>]) -> R,
) -> R {
    let frames = chans.iter().map(Vec::len).max().unwrap_or(0);
    warm_channels(chans.len(), frames);
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        let pool = &mut p[..chans.len()];
        for (b, c) in pool.iter_mut().zip(chans.iter()) {
            b[..c.len()].copy_from_slice(c);
        }
        let r = f(pool);
        for (c, b) in chans.iter_mut().zip(pool.iter()) {
            let n = c.len();
            c.copy_from_slice(&b[..n]);
        }
        r
    })
}

/// One parameter as `param.list` (protocol 3.7) reports it beyond its id and name. Computed at load (message thread)
/// by each format.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ParamMeta {
    /// The plugin's own id: VST3 `ParameterInfo.id`, AU `AudioUnitParameterID`.
    pub plugin_id: u32,
    /// Whether the host may automate it: VST3 `kCanAutomate`; AU not `kAudioUnitParameterFlag_NonRealTime`.
    pub automatable: bool,
    /// Normalized, in [0, 1].
    pub default: f32,
    /// VST3 `stepCount`; AU `max - min` for the `Indexed` / `Boolean` units, else 0.
    pub step_count: u32,
    /// The parameter's unit label (VST3 `ParameterInfo::units`; AU the unit's name).
    pub label: String,
    /// The position in the flattened parameter tree (`summoner_plugin::params`), `u32::MAX` when not in it.
    pub order: u32,
}

impl ParamMeta {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        plugin_id: u32,
        automatable: bool,
        default: f32,
        step_count: u32,
        label: String,
        order: u32,
    ) -> ParamMeta {
        ParamMeta {
            plugin_id,
            automatable,
            default,
            step_count,
            label,
            order,
        }
    }
}

/// A loaded slot, message-thread side.
pub trait Plugin: Send + Sync {
    /// The plugin's display name.
    fn name(&self) -> &str;
    /// The slot's latency in frames (its PDC share).
    fn latency(&self) -> u32;
    /// How many parameters the slot reports.
    fn param_count(&self) -> u32;
    /// *(bridge protocol 3.9)* The slot's notices now, codes the host application names it by (`SlotInfo.notices` at
    /// load, `chain.prepared`'s `slotNotices` after a prepare: the whole current set). Message thread. None by default.
    fn notices(&self) -> Vec<String> {
        Vec::new()
    }
    /// Parameter `index`'s name (empty past the count).
    fn param_name(&self, index: u32) -> String;
    /// A parameter value from the region, on the chain thread before the block: must not block or allocate.
    fn set_param(&self, index: u32, value: f32);
    /// Region values this plugin dropped: an index it reports but cannot be driven (a lying count). Counted on the
    /// chain thread.
    fn dropped_params(&self) -> u64 {
        0
    }
    /// The test double's hooks ([`TestHooks`]), in a build with this crate's `test-hooks` feature only (a test host);
    /// a shipped build has neither the method nor the hooks. `None` for every real plugin.
    #[cfg(feature = "test-hooks")]
    fn test_hooks(&self) -> Option<&dyn TestHooks> {
        None
    }
    /// One chunk of state (the builtin body, the vst3 component chunk, the au plist): `Ok` applied, `Err` refused.
    fn set_state(&self, blob: &[u8]) -> Result<(), Error>;
    /// The slot's state as one chunk (the builtin body, the vst3 component chunk, the au plist).
    fn get_state(&self) -> Vec<u8>;
    /// The state blob's format of this slot's state. The host frames and unframes the state blob; a slot sees chunks.
    fn state_kind(&self) -> StateKind {
        StateKind::Builtin
    }
    /// `state.set`, as chunks: `first` the builtin body / the vst3 component chunk / the au plist, `second` the vst3
    /// controller chunk; `None` an absent chunk (skipped), `Some(&[])` an empty one (applied). The default takes one
    /// chunk; a second is refused.
    fn set_state_chunks(&self, first: Option<&[u8]>, second: Option<&[u8]>) -> SetOutcome {
        if second.is_some() {
            return SetOutcome::Refused("this format has no second chunk".into());
        }
        match first {
            Some(b) => match self.set_state(b) {
                Ok(()) => SetOutcome::Applied,
                Err(e) => SetOutcome::Refused(e.into()),
            },
            None => SetOutcome::Applied,
        }
    }
    /// `state.get` / `editor.closed`, as chunks (`Err`: no state to answer). The default is one chunk.
    fn get_state_chunks(&self) -> Result<Chunks, Error> {
        Ok(Chunks {
            first: Some(self.get_state()),
            second: None,
        })
    }
    /// `editor.open`. Returns true when the slot now counts as having an editor open (the test double's "editor" is a
    /// user edit that moves the state).
    fn open_editor(&self) -> bool {
        false
    }
    /// The editor of a real plugin, message thread. Step 1: make the plugin's view, if it has one
    /// the host can show: its content size and whether it resizes. `None`: no editor (`editor.availability` false).
    fn editor_create(&self) -> Option<(u32, u32, bool)> {
        None
    }
    /// Step 3: the view into the window ([`EditorAttach`]).
    fn editor_attach(&self, _a: EditorAttach) -> Result<(), Error> {
        Err("no editor".into())
    }
    /// The window's user-resize hook for this plugin's view, and its context (valid while the plugin lives).
    fn editor_resize_hook(&self) -> Option<(ResizeHookFn, usize)> {
        None
    }
    /// Step 5: the view out of the window and released (before the window closes).
    fn editor_close(&self) {}
    /// A view the host builds itself, for a plugin with no view of its own to attach ([`EditorSource`]): an AU v2's
    /// unit, for its Cocoa UI or the generic view. [`EditorSource::None`] by default.
    fn editor_source(&self) -> EditorSource {
        EditorSource::None
    }
    /// Parameter `index`'s description for a 3.7 `param.list` (message thread). The default: an id equal to the
    /// index, not automatable, in tree order.
    fn param_meta(&self, index: u32) -> ParamMeta {
        ParamMeta {
            plugin_id: index,
            order: index,
            ..ParamMeta::default()
        }
    }
    /// Parameter `index`'s normalized value for `ParamEntry.value` (message thread): an unapplied host set's
    /// value, else the plugin's own.
    fn param_value(&self, _index: u32) -> f32 {
        0.0
    }
    /// `param.set` (message thread): parameter `plugin_id` set to the normalized `value` (already clamped and
    /// finite), through the processor's queue and the controller (VST3) or the unit (AU). The
    /// read-back by [`Plugin::param_value`]'s rule, or why it was refused (no such parameter).
    fn host_set(&self, plugin_id: u32, _value: f32) -> Result<f32, Error> {
        Err(format!("no parameter {plugin_id}").into())
    }
    /// A host set still waits for the processor to take it (the host moves the slot's `childGen` again
    /// once none does). Message thread.
    fn host_set_pending(&self) -> bool {
        false
    }
    /// The message thread's periodic look at what the plugin raised on its own (a VST3 `restartComponent`, say).
    /// Never called on the chain thread.
    fn poll(&self) -> Notice {
        Notice::None
    }
    /// How many edits the plugin's own controller has made so far (a VST3 `performEdit`: its editor, or the plugin on
    /// its own). The host moves the slot's `childGen` when it moves. Message thread.
    fn edits(&self) -> u64 {
        0
    }
    /// `chain.prepare`, on the message thread: a fresh chain-thread side for `rate` and `max_frames`.
    fn prepare(&self, rate: f64, max_frames: usize) -> Box<dyn Dsp>;
    /// [`Plugin::prepare`] for a chain of `role: render` when `offline` (render sets are created
    /// non-realtime): VST3 `kOffline`, AU `kAudioUnitProperty_OfflineRender`. The default (the test double) ignores
    /// the mode.
    fn prepare_mode(&self, rate: f64, max_frames: usize, offline: bool) -> Box<dyn Dsp> {
        let _ = offline;
        self.prepare(rate, max_frames)
    }
}

/// The state blob's format of a slot's state (`format` 3 builtin, 1 vst3, 2 au).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StateKind {
    /// The test double's builtin body (`format` 3).
    Builtin,
    /// A VST3 component chunk and controller chunk (`format` 1).
    Vst3,
    /// An AU ClassInfo plist (`format` 2).
    Au,
}

/// A slot's state as chunks: `None` absent. `second` is the vst3 controller chunk only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chunks {
    /// The builtin body, the vst3 component chunk or the au plist.
    pub first: Option<Vec<u8>>,
    /// The vst3 controller chunk.
    pub second: Option<Vec<u8>>,
}

impl Chunks {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(first: Option<Vec<u8>>, second: Option<Vec<u8>>) -> Chunks {
        Chunks { first, second }
    }
}

/// What a [`Plugin::set_state_chunks`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetOutcome {
    /// Every present chunk applied.
    Applied,
    /// Nothing reached the plugin, or the one chunk it was given was refused: why.
    Refused(String),
    /// The first chunk applied and the second was then refused (a partial vst3 set): the plugin changed and cannot
    /// be rolled back. Why the second was refused.
    FirstOnly(String),
}

/// The C signature of an editor window's user-resize hook (summoner_sys::editor's `ResizeHook`): phase 0 asks, and may
/// change `*w` / `*h` (nonzero allows); phase 1 tells. Only passed along here, never called.
pub type ResizeHookFn = unsafe extern "C" fn(*mut std::ffi::c_void, i32, *mut f64, *mut f64) -> i32;

/// The window a plugin's editor view is attached to, tagged by platform (VST3's platform types): never a
/// bare pointer whose meaning depends on the OS. On Windows it is the child's own container window, which the host
/// then parents into its window (host-embedding).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentWindow {
    /// macOS: an `NSView*` (VST3's `kPlatformTypeNSView`).
    NsView(usize),
    /// Windows: an `HWND` (`kPlatformTypeHWND`).
    Hwnd(usize),
    /// X11: an embed window id (`kPlatformTypeX11EmbedWindowID`; an XID is a C `unsigned long`).
    X11(u64),
}

impl ParentWindow {
    /// The `NSView*` of a macOS parent; any other platform's parent is refused here, before any call into a plugin
    /// (this host's macOS editors attach to an `NSView` only). The mutation check (never in a normal build): the
    /// platform is not checked. `another_platforms_parent_is_refused_before_any_call` must fail.
    pub fn ns_view(self) -> Result<usize, Error> {
        match self {
            ParentWindow::NsView(v) => Ok(v),
            #[cfg(mutant_parent_kind_unchecked)]
            ParentWindow::Hwnd(v) => Ok(v),
            other => Err(Error(format!(
                "an editor parent of another platform ({other:?}): this host attaches to an NSView"
            ))),
        }
    }
}

/// Where a plugin's editor view goes (step 3): the window ([`ParentWindow`]) and the host window's resize functions
/// ([`HostWindow`]).
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct EditorAttach {
    /// The window the view attaches to.
    pub parent: ParentWindow,
    /// The host window the plugin's resize requests go to.
    pub host: HostWindow,
}

impl EditorAttach {
    /// The view of `parent`, resized through `host`.
    pub fn new(parent: ParentWindow, host: HostWindow) -> EditorAttach {
        EditorAttach { parent, host }
    }
}

/// The host window's resize functions, as the editor dylib exports them, and their context: an opaque handle a
/// format's attach passes through to the plugin's resize callbacks, never a value a caller reads for anything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostWindow {
    set_size: usize,
    get_size: usize,
    context: usize,
}

impl HostWindow {
    /// The editor dylib's set-size and get-size functions' addresses, and the window handle they take.
    pub fn new(set_size: usize, get_size: usize, context: usize) -> HostWindow {
        HostWindow {
            set_size,
            get_size,
            context,
        }
    }

    /// The set-size function's address.
    pub fn set_size(self) -> usize {
        self.set_size
    }

    /// The get-size function's address.
    pub fn get_size(self) -> usize {
        self.get_size
    }

    /// The window handle (the functions' context).
    pub fn context(self) -> usize {
        self.context
    }
}

/// A view the host builds itself ([`Plugin::editor_source`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditorSource {
    /// None: the plugin attaches its own view, or has none.
    None,
    /// An AU v2's `AudioUnit` handle, valid while the plugin lives: the host builds its Cocoa UI (or CoreAudioKit's
    /// generic view) from it.
    AuUnit(AuUnit),
}

/// An Audio Unit instance's handle (an `AudioUnit`), opaque: only passed back to the AU editor calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuUnit(usize);

impl AuUnit {
    /// The handle of the unit at `raw`.
    pub fn from_raw(raw: usize) -> AuUnit {
        AuUnit(raw)
    }

    /// The raw handle, for the AU editor calls.
    pub fn raw(self) -> usize {
        self.0
    }
}

/// The test double's hooks (`summoner-testproc`, as the host application's test double), reached through
/// [`Plugin::test_hooks`] in a `test-hooks` build only.
#[cfg(feature = "test-hooks")]
pub trait TestHooks: Send + Sync {
    /// A region value past [`Plugin::param_count`], on the chain thread before the block. `false`: not taken.
    fn test_param(&self, _index: u32, _value: f32) -> bool {
        false
    }
    /// This block is acked SKIPPED and silent, as a chain that prepare holds (for the host application's render tests).
    fn test_skip(&self) -> bool {
        false
    }
    /// The `BAREGET` knob (the bare-state refusal test): answer `state.get` / `editor.closed` with the bare first
    /// chunk, not a state blob.
    fn answers_bare(&self) -> bool {
        false
    }
}

/// The crate's refusal with a reason (a state refused, an editor not attached, a parameter set refused): its
/// `Display` is the reason, as the host's lines and error replies word it; its `Debug` the reason's as a string's.
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

/// What [`Plugin::poll`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Notice {
    /// Nothing changed.
    None,
    /// Something the host reports (the latency) may have changed: read it again.
    Latency,
    /// The plugin asks to be re-activated (VST3 `kLatencyChanged`) before what it reports applies: the host takes
    /// the slot's [`Dsp`] out of the chain, calls [`Dsp::reactivate`], puts it back, then reads the latency.
    Reactivate,
}

/// A prepared processor's audio buses: the input and output channels it takes and writes, as the host application
/// expects them reported for a hosted plugin. Fixed at prepare: the host reads it once per prepare and caches it per
/// slot, never per block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Io {
    /// The input channels it takes.
    pub inputs: u32,
    /// The output channels it writes.
    pub outputs: u32,
}

impl Io {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(inputs: u32, outputs: u32) -> Io {
        Io { inputs, outputs }
    }
}

impl Io {
    /// Two in, two out: the block's own width, where the host's channel rule changes nothing.
    pub const STEREO: Io = Io {
        inputs: 2,
        outputs: 2,
    };
}

impl Default for Io {
    fn default() -> Io {
        Io::STEREO
    }
}

// Plain values, so the chain's wired per-slot table may hold it (each field asserted).
impl runecraft_rt::arena::RtMem for Io {}

/// A prepared slot, chain-thread side.
pub trait Dsp: Send {
    /// The processor's buses ([`Io`]), fixed from its prepare on: the host applies its channel rule around
    /// [`Dsp::process`] with it. Read on the message thread at prepare, never in a block.
    fn io(&self) -> Io;
    /// The host's `BF_RESET`, before the block (all notes off, pedals up; tails may stay).
    fn reset(&mut self);
    /// One block in place. `false`: the plugin failed it (a C++ exception caught at a format's boundary, say): the
    /// host makes the block THREW and silent, with no unwinding on the chain thread.
    fn process(&mut self, block: &mut Block<'_>) -> bool;
    /// On the message thread, while the host holds this `Dsp` out of the chain (the slot passes audio through):
    /// re-activate it so a changed latency is applied. `Err` with why; the host puts it back either way.
    fn reactivate(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A macOS parent is its `NSView*`; another platform's parent is refused before any call into a
    /// plugin. `--cfg mutant_parent_kind_unchecked` must fail this.
    #[test]
    fn another_platforms_parent_is_refused_before_any_call() {
        assert_eq!(ParentWindow::NsView(0x1000).ns_view(), Ok(0x1000));
        assert!(ParentWindow::Hwnd(0x1000).ns_view().is_err());
        assert!(ParentWindow::X11(7).ns_view().is_err());
    }
}
