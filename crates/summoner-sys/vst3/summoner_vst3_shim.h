// The VST3 host shim's C ABI: glue only, between Rust (summoner-sys) and the pinned
// SDK. No DSP, no policy, no state transformation; every function is noexcept and every C++ exception becomes
// SUMMONER_VST3_THREW. Modules are loaded by absolute path only (the shim has its own CFBundle loader: the SDK's
// module_mac.mm, which enumerates the system plugin folders, is not compiled).
#pragma once
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#define SUMMONER_NOEXCEPT noexcept
#else
#define SUMMONER_NOEXCEPT
#endif

enum { SUMMONER_VST3_OK = 0, SUMMONER_VST3_FAILED = 1, SUMMONER_VST3_THREW = 2, SUMMONER_VST3_REFUSED = 3 };

/* The shim's ABI number: bumped with ANY change to a signature in this header, and checked once
   by the Rust side (crates/summoner-sys/src/vst3.rs SHIM_ABI) before any module is opened, so a hand-written extern block that
   drifted from this header fails loudly instead of being undefined behaviour. 2: `prepare` gained `offline`. 4:
   `summoner_vst3_param.units` / `unit_id`, `summoner_vst3_unit_count` / `_get` and `summoner_vst3_controller_get_param`. */
#define SUMMONER_VST3_SHIM_ABI 4
int32_t summoner_vst3_shim_abi(void) SUMMONER_NOEXCEPT;

typedef struct summoner_vst3_module summoner_vst3_module;
typedef struct summoner_vst3_plugin summoner_vst3_plugin;

typedef struct {
    uint8_t cid[16];
    int32_t cardinality;
    uint32_t class_flags;
    char category[32];
    char name[64];
    char vendor[64];
    char version[64];
    char sdk_version[64];
    char sub_categories[128];
} summoner_vst3_class;

typedef struct {
    uint32_t id;
    char title[512];  // UTF-8: a String128 title (128 UTF-16 units) is at most 384 bytes
    double default_normalized;
    int32_t step_count;
    int32_t flags;
    char units[512];  // UTF-8, the String128 `ParameterInfo.units`
    int32_t unit_id;  // `ParameterInfo.unitId` (its IUnitInfo unit)
} summoner_vst3_param;

typedef struct {
    uint32_t id;
    int32_t offset;
    double value;
} summoner_vst3_point;

// The block's input events (the host's conversion of unmapped MIDI). NOTE_ON / NOTE_OFF: channel, pitch, velocity.
// POLY_PRESSURE (kPolyPressureEvent): channel, pitch, velocity = the pressure. LEGACY_CC (kLegacyMIDICCOutEvent,
// which the host sends IN for an unmapped controller, channel pressure, pitch bend, program change or MTC quarter
// frame): channel, control = its control number, pitch = value, value2.
enum { SUMMONER_VST3_NOTE_ON = 0, SUMMONER_VST3_NOTE_OFF = 1, SUMMONER_VST3_POLY_PRESSURE = 2, SUMMONER_VST3_LEGACY_CC = 3 };
typedef struct {
    int32_t offset;
    int32_t kind;
    int16_t channel;
    int16_t pitch;
    float velocity;
    int16_t control;
    int16_t value2;
} summoner_vst3_note;

// The transport, already decoded by the host (which flags are valid is the host's call).
typedef struct {
    uint8_t playing, cycle_active, recording, tempo_valid, time_sig_valid, bar_valid, cycle_valid, system_time_valid;
    double ppq, tempo, bar_ppq, cycle_start_ppq, cycle_end_ppq;
    int32_t time_sig_num, time_sig_den;
    int64_t project_samples;
    int64_t system_time_ns;
} summoner_vst3_transport;

typedef struct {
    const float* const* inputs;  // num_inputs channels of the main input bus (may be 0)
    int32_t num_inputs;
    float* const* outputs;  // num_outputs channels of the main output bus
    int32_t num_outputs;
    int32_t frames;
    const summoner_vst3_point* points;  // sorted by id, stably (each id's points in offset order); at most max_points
    int32_t num_points;
    const summoner_vst3_note* notes;
    int32_t num_notes;
    const summoner_vst3_transport* transport;
} summoner_vst3_block;

// err: a NUL-terminated message (truncated to cap) on failure.
int summoner_vst3_module_open(const char* abs_path, summoner_vst3_module** out, char* err, size_t cap) SUMMONER_NOEXCEPT;
void summoner_vst3_module_close(summoner_vst3_module* m) SUMMONER_NOEXCEPT;
int32_t summoner_vst3_class_count(const summoner_vst3_module* m) SUMMONER_NOEXCEPT;
int summoner_vst3_class_get(const summoner_vst3_module* m, int32_t index, summoner_vst3_class* out) SUMMONER_NOEXCEPT;
// For the scan: the factory's PFactoryInfo::vendor, "" if the factory refuses.
int summoner_vst3_factory_vendor(const summoner_vst3_module* m, char* out, size_t cap) SUMMONER_NOEXCEPT;
// The channels of the default-active audio buses in `dir` (0 input, 1 output), as the freshly created (initialised,
// not yet arranged) component reports them (IComponent::getBusCount / getBusInfo).
int32_t summoner_vst3_plugin_default_channels(const summoner_vst3_plugin* p, int32_t dir) SUMMONER_NOEXCEPT;

// The component and its controller (separate or single), initialized and connected, controller synced to the
// component's state.
int summoner_vst3_plugin_create(summoner_vst3_module* m, const uint8_t cid[16], summoner_vst3_plugin** out, char* err,
                           size_t cap) SUMMONER_NOEXCEPT;
void summoner_vst3_plugin_destroy(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
// The main buses after arranging them for (in_ch, out_ch): 0 when that bus does not exist; event input 0 / 1.
int summoner_vst3_plugin_buses(summoner_vst3_plugin* p, int32_t in_ch, int32_t out_ch, int32_t* got_in, int32_t* got_out,
                          int32_t* has_events) SUMMONER_NOEXCEPT;
// ABI 3: the bytes prepare needs at `rt` for max_points / max_notes (the parameter queues in and out, the events,
// the output-value ring), laid out by the shim. The caller owns that memory (wired) for as long as the plugin.
size_t summoner_vst3_rt_bytes(int32_t max_points, int32_t max_notes) SUMMONER_NOEXCEPT;
// setupProcessing (32-bit; kOffline when offline != 0, a render set's slot; else kRealtime),
// setActive (true), setProcessing (true); with room for max_points / max_notes, placed at `rt` (at least
// summoner_vst3_rt_bytes, else SUMMONER_VST3_REFUSED with nothing placed; once placed, the record uses `rt` whatever the result;
// the previous ring's values are flushed to the controller first). The mode is
// kept for reactivate and every process call.
int summoner_vst3_plugin_prepare(summoner_vst3_plugin* p, double rate, int32_t max_frames, int32_t max_points,
                            int32_t max_notes, int32_t offline, void* rt, size_t rt_len) SUMMONER_NOEXCEPT;
int summoner_vst3_plugin_release(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
/* setupProcessing(kRealtime, 32-bit, max_frames, rate) on an INACTIVE instance, nothing else: the host's
   placeholder setup at instantiation (44.1 kHz, 1024), kept because a real plugin's audio depends on it. */
int summoner_vst3_plugin_setup_only(summoner_vst3_plugin* p, double rate, int32_t max_frames) SUMMONER_NOEXCEPT;
// The spec's answer to restartComponent (kLatencyChanged): setProcessing (false), setActive (false), setupProcessing
// with the setup of the last prepare, setActive (true), setProcessing (true). Fails if never prepared.
int summoner_vst3_plugin_reactivate(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
int summoner_vst3_plugin_process(summoner_vst3_plugin* p, const summoner_vst3_block* b) SUMMONER_NOEXCEPT;
uint32_t summoner_vst3_plugin_latency(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
// IComponentHandler::restartComponent flags since the last call (exchanged with 0).
int32_t summoner_vst3_plugin_take_restart(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
// performEdit's edits: how many the controller made so far (any thread; the message thread's poll), and
// the chain thread's lock-free pop of the oldest one not yet taken (1 and *id, *value; 0 when there is none).
uint64_t summoner_vst3_plugin_edit_count(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
// IEditController::setParamNormalized (message thread: the host's values reach the controller).
int summoner_vst3_controller_set_param(summoner_vst3_plugin* p, uint32_t id, double value) SUMMONER_NOEXCEPT;
// The processor's output parameter changes since the last call, each id's last value per block, handed to the
// controller (setParamNormalized), oldest first (message thread). Returns how many, or -1 if it threw.
int32_t summoner_vst3_plugin_flush_outputs(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
// The editor, main thread only. create: the controller's view, if it has one that takes
// an NSView (else REFUSED: editor.availability false), its size and whether it resizes. attach: setFrame, then
// attached (nsview: the window's content view; set_size / get_size: the editor dylib's window size functions, window
// their context). user_resize: the window's resize hook (ctx = the plugin; phase 0 checkSizeConstraint, may change
// *w / *h, 1 = allowed; phase 1 onSize). close: removed, setFrame (null), the view released.
int summoner_vst3_editor_create(summoner_vst3_plugin* p, int32_t* w, int32_t* h, int32_t* resizable) SUMMONER_NOEXCEPT;
int summoner_vst3_editor_attach(summoner_vst3_plugin* p, void* nsview, void* set_size, void* get_size, void* window) SUMMONER_NOEXCEPT;
int summoner_vst3_editor_user_resize(void* ctx, int phase, double* w, double* h) SUMMONER_NOEXCEPT;
int summoner_vst3_editor_close(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
int summoner_vst3_plugin_take_edit(summoner_vst3_plugin* p, uint32_t* id, double* value) SUMMONER_NOEXCEPT;
int32_t summoner_vst3_param_count(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
int summoner_vst3_param_get(summoner_vst3_plugin* p, int32_t index, summoner_vst3_param* out) SUMMONER_NOEXCEPT;
// The parameter tree: IUnitInfo's unit count (0: no IUnitInfo), and unit `index`'s id and parent (FAILED: getUnitInfo
// failed; the caller then keeps a zeroed unit). get_param:
// IEditController::getParamNormalized.
int32_t summoner_vst3_unit_count(summoner_vst3_plugin* p) SUMMONER_NOEXCEPT;
int summoner_vst3_unit_get(summoner_vst3_plugin* p, int32_t index, int32_t* id, int32_t* parent) SUMMONER_NOEXCEPT;
int summoner_vst3_controller_get_param(summoner_vst3_plugin* p, uint32_t id, double* value) SUMMONER_NOEXCEPT;
// IMidiMapping: the parameter a MIDI controller (0-127, 128 aftertouch, 129 pitch bend) maps to.
int summoner_vst3_midi_mapping(summoner_vst3_plugin* p, int16_t channel, int16_t cc, uint32_t* id) SUMMONER_NOEXCEPT;
// Chunks: which 0 = component (IComponent::getState / setState, then IEditController::setComponentState),
// which 1 = controller (IEditController::getState / setState). get: *data is malloc'd, freed by summoner_vst3_free.
int summoner_vst3_state_get(summoner_vst3_plugin* p, int32_t which, uint8_t** data, size_t* len) SUMMONER_NOEXCEPT;
int summoner_vst3_state_set(summoner_vst3_plugin* p, int32_t which, const uint8_t* data, size_t len) SUMMONER_NOEXCEPT;
void summoner_vst3_free(void* data) SUMMONER_NOEXCEPT;

#ifdef __cplusplus
}
#endif
