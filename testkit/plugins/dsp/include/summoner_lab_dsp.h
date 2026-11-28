// The lab's test plugins, as plain DSP cores: no plugin SDK, no allocation or lock in process(), no I/O. The VST3 and
// AU wrappers are thin shells over these; every host under test sees the same bytes from either wrapper. Built by
// tools/build-plugins.sh (cmake + ninja, nice 15) into target/plugins.
//
//   Gain     a stereo (any channel count) gain: y = x * gain, gain stored raw (no smoothing, no range), latency 0.
//   Synth    a deterministic 16-voice sine synth: note on / off, CC 123 / 120 all notes off, CC 64 sustain, a linear
//            release; output depends only on the events and the sample rate (phase restarts at prepare()).
//   Hostile  every misbehaviour the host must survive (crashes, exceptions, hangs, realtime abuse), each behind its own
//            switch:
//            crash (abort / SIGSEGV / an exception) and wedge in process(); a crash or a stall in construction,
//            prepare() and setState(); CPU burn; NaN / inf / denormal / huge output; a reported latency that lies or
//            moves; a parameter count that lies; a state that fails, or one over 64 MiB.
//
// State is versioned bytes: "SASL", u32 kind, u32 version, then the parameters as little-endian f32 (raw, so a state
// round trip is bit-exact). getState() / setState() are byte-for-byte inverse for every state getState() produced.
#pragma once

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <vector>

namespace sas::lab {

struct MidiEvent {
    uint32_t offset = 0;  // frame within the block, already clamped by the host
    uint8_t size = 0;     // bytes in data (0 = none)
    uint8_t data[3] = {0, 0, 0};
};

// What the host told the plugin about the transport for this block (VST3: ProcessContext; null when the host gave
// none). Only the hostile core's TRANSPORT reporter reads it.
struct Transport {
    bool playing = false;
    bool recording = false;
    bool cycleActive = false;
    bool tempoValid = false;
    bool musicValid = false;  // projectTimeMusic
    double tempo = 0.0;
    double projectTimeMusic = 0.0;  // ppq at the block's first frame
    int64_t projectTimeSamples = 0;
};

// One block, processed in place: `channels` pointers to `frames` floats each, and the block's MIDI in offset order.
struct Block {
    float* const* channels = nullptr;
    uint32_t numChannels = 0;
    uint32_t frames = 0;
    const MidiEvent* midi = nullptr;
    uint32_t midiCount = 0;
    const Transport* transport = nullptr;
};

struct ParamInfo {
    const char* id;    // stable, ASCII
    const char* name;
    float defaultValue;
};

enum class Kind : uint32_t { Gain = 1, Synth = 2, Hostile = 3 };

// The interface every core implements; the wrappers drive only this.
class Processor {
public:
    virtual ~Processor() = default;
    virtual Kind kind() const = 0;
    virtual uint32_t paramCount() const = 0;
    virtual ParamInfo paramInfo(uint32_t index) const = 0;
    virtual float param(uint32_t index) const = 0;
    virtual void setParam(uint32_t index, float value) = 0;  // any thread; realtime-safe
    virtual uint32_t latency() const { return 0; }
    virtual void prepare(double sampleRate, uint32_t maxFrames) = 0;
    virtual void reset() = 0;                // all notes off, tails kept
    virtual void process(const Block& b) = 0;  // realtime: no allocation, no lock, no syscall
    virtual std::vector<uint8_t> getState() const;
    // True: the wrapper's getState fails (VST3: the component's getState returns kResultFalse) instead of returning
    // getState()'s bytes. Only the hostile core's FAIL_GETSTATE says so.
    virtual bool getStateFails() const { return false; }
    // True: the block process() just ran failed (the wrapper reports it as its format's failure: VST3 kResultFalse,
    // AU an error status), with no exception. Only the needs-the-note synth (the scan fixture) says so.
    virtual bool blockFailed() const { return false; }
    // False: refused (applied as far as it got). The wrapper reports it as the format's failure code.
    virtual bool setState(const uint8_t* data, size_t size);
};

// Construction-time behaviour of the hostile core (the wrappers expose these as separate plugin classes).
struct HostileConfig {
    bool crashInConstructor = false;
    bool crashInPrepare = false;
    uint32_t prepareSleepMs = 0;
    uint32_t lieParamCount = 0;  // 0: the truth
    bool hangInConstructor = false;  // The scan fixture: never returns from construction (sleeps, 1 s at a time)
    bool refusesOwnState = false;    // The scan fixture: getState() returns bytes its own setState() refuses
};

// The lab's death switch: with SUMMONER_LAB_DEATH=kill in the environment, every hostile death is a SIGKILL, which
// leaves no crash report in the user's real ~/Library/Logs/DiagnosticReports. Otherwise they are what they
// say (abort(), a write through a null pointer). Read once per process.
[[noreturn]] void labAbort();
[[noreturn]] void labSegv();

std::unique_ptr<Processor> makeGain();
// needsNote (the scan fixture): from the 8th block after prepare() on, every block fails until a note-on has
// arrived (validation sends one in its first block to an instrument, and runs 8).
std::unique_ptr<Processor> makeSynth(bool needsNote = false);
std::unique_ptr<Processor> makeHostile(const HostileConfig& cfg = {});

// The hostile core's parameter indices (each a switch at >= 0.5 unless said otherwise).
namespace hostile {
enum Param : uint32_t {
    Gain = 0,         // raw gain, as Gain's
    Abort,            // std::abort() in process()
    Segv,             // a write through a null pointer in process()
    Throw,            // throws std::runtime_error from process()
    Wedge,            // never returns from process()
    BurnMs,           // value * 100 ms of spinning per block
    Nan,              // output NaN
    Inf,              // output +inf
    Denormal,         // output the smallest subnormal
    Huge,             // output 1e30
    LatencyFrames,    // reported latency = round(value * 8192) frames (moves when the value does)
    Count
};
}  // namespace hostile

// The special states the hostile core reacts to (the bytes, as setState receives them): "CRASH" aborts, "SEGV"
// faults, "SLOW=<ms>" sleeps first, "FAIL" is refused; "BIG" makes getState() return 64 MiB + 1 bytes;
// "LATER=<ms>:<frames>" changes the latency to <frames> (at most 8192), <ms> later, on a thread of its own (its VST3
// controller raises restartComponent (kLatencyChanged) 20 ms after that). The instance must outlive the delay.
//
// The getState knobs and the transport reporter (for the host application's state and transport tests). Each is applied
// (true) and is a MODE: it lasts until the next setState, and any other state (a normal one included) ends it.
//   "SLOW_GETSTATE=<ms>"  getState() sleeps <ms> (at most 60000) on the calling thread, then returns these same bytes
//                         ("SLOW_GETSTATE=<ms>", as set), so a round trip keeps the mode.
//   "BIG_GETSTATE"        getState() returns kBig2State: exactly 2 MiB (2,097,152 bytes), "SASBIG2M" then byte i =
//                         uint8_t((i * 2654435761) >> 24) for i in 8.., deterministic; setState of exactly those bytes
//                         is applied and keeps the mode (a round trip), any other "SASBIG2M" blob is refused.
//   "FAIL_GETSTATE"       getStateFails(): the VST3 component's getState returns kResultFalse. Its controller's chunk
//                         is unaffected, so a host's get is PARTIAL (component absent, controller present).
//   "TRANSPORT"           the transport reporter: process() writes what the host passed, as samples (TRANSPORT_ENCODING
//                         below); getState() returns "TRANSPORT" (constant: the values never go through the state).
//
// TRANSPORT_ENCODING. After the block's normal processing (the input, the gain, any delay), the output is OVERWRITTEN:
//   ch0[f]  = float(projectTimeMusic) for every frame f (the block-start ppq, as the host gave it, not advanced), or
//             0 when it is not valid. float(): the C++ double → float conversion, round to nearest even, the same as
//             Rust's `ppq as f32`. f32 holds 24 significant bits: in [0, 1024) beats, the range the tests use, the
//             step is at most 2^-14 beat (6.1e-5); beyond it the precision halves per octave.
//   ch1[0]  = float(tempo) in bpm, or 0 when it is not valid (exact for the tests' tempos, which are multiples of
//             1/64 bpm below 1024).
//   ch1[1]  = the flags, an exact small integer: bit 0 playing, 1 recording, 2 cycle active, 3 tempo valid,
//             4 projectTimeMusic valid, 5 a transport was given at all (VST3: processContext non-null).
//   ch1[2]  = float(projectTimeSamples): exact below 2^24 samples (349.5 s at 48 kHz), the range the tests use.
//   ch1[f]  = 0 for f >= 3; channels past 1 = 0. A block of fewer than 3 frames carries what fits.
// No allocation, no lock: the report is written in place in process().
}  // namespace sas::lab
