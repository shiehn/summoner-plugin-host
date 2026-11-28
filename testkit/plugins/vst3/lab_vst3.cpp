// The lab's test plugins as VST3: a thin shell over the DSP cores (testkit/plugins/dsp), built by this repo's own
// CMake from the pinned SDK's sources by path (VENDORED.md; the SDK's own CMake is never run). One bundle per core,
// chosen at compile time by SUMMONER_LAB_KIND (1 gain, 2 synth, 3 hostile). Split component / controller, so a host sees
// both state chunks (the component chunk is the DSP core's state bytes; the controller chunk is "SASC", u32 version,
// u32 kind).
//
// Lab conventions (test plugins, not products):
// - a parameter's normalized value IS the DSP core's raw value (no range mapping), ids = the DSP core's indices;
// - MIDI: note on / off arrive as VST3 events; CC 64 / 120 / 123 arrive as parameter changes mapped through
//   IMidiMapping (ids 1064 / 1120 / 1123, value × 127), turned back into MIDI at their sample offsets;
// - the hostile bundle also exports its construction variants as classes of their own (crash in the constructor,
//   crash in prepare, a 3 s prepare, a lying parameter count of 77).
//
// CTXMIDI (SUMMONER_LAB_FIXTURE_CTXMIDI): a synth shell (kind 2, an event input) whose output is a REPORT,
// written over the whole block after the DSP core ran. Its IMidiMapping maps CC 1, CC 64, kAfterTouch (128) and
// kPitchBend (129) on every channel c (0-15) of bus 0 to parameter kCtxBase + c * 256 + cc, each declared.
//   ch0[3i], ch0[3i+1], ch0[3i+2] = float(id + 1), float(sampleOffset), float(value) of the i-th parameter point the
//     host delivered, any id (queue order, then point order), as many as fit; then 0. ids < 2^24: exact. (id + 1, so
//     parameter 0's point is not a 0, the list's end.)
//   ch1[0]  = 1 when processContext is non-null, else 0 (and every other ch1 frame is 0);
//   ch1[1]  = float(state), the raw validity / transport bits (all < 2^24: exact);
//   ch1[2]  = float(sampleRate);           ch1[3]  = float(projectTimeSamples);
//   ch1[4]  = 1 if systemTime != 0, else 0 (its value is host time: not deterministic);
//   ch1[5]  = float(continousTimeSamples); ch1[6]  = float(projectTimeMusic);
//   ch1[7]  = float(barPositionMusic);     ch1[8]  = float(cycleStartMusic);  ch1[9] = float(cycleEndMusic);
//   ch1[10] = float(tempo);                ch1[11] = timeSigNumerator;        ch1[12] = timeSigDenominator;
//   ch1[13] = smpteOffsetSubframes;        ch1[14] = frameRate.framesPerSecond; ch1[15] = frameRate.flags;
//   ch1[16] = samplesToNextClock;
//   ch1[17] = the processor's setActive (true) calls so far; ch1[18] = its setupProcessing calls so far (what a
//             host's restart cycle did);
//   ch1[20 + 5i .. 20 + 5i + 4] = the i-th input event of the block: float(type + 1), its sample
//             offset, its channel, then by type: note on / off (pitch, velocity); poly pressure (pitch, pressure);
//             kLegacyMIDICCOutEvent (controlNumber, value + 1000 * value2); any other (0, 0). As many as fit (16 at
//             most); then 0. A block shorter than that carries what fits.
// float(): the double → float conversion, round to nearest even (Rust's `as f32`). No allocation, no lock.
// The component state "REMAP" (applied, the DSP core untouched): the controller swaps CC 1 and CC 64 on every channel
// (CC 1 then maps to cc 64's id, and the reverse) and raises restartComponent (kMidiCCAssignmentChanged) at once, from
// setComponentState, on the thread that set the state. "UNMAP" maps nothing from then on, raising
// the same restart.
// The component state "RESTART=<flags>" (decimal; applied, the DSP core untouched): the controller raises
// restartComponent (<flags>) at once, from setComponentState. With kParamTitlesChanged in <flags>, every "MIDI Map"
// parameter is titled "MIDI Map 2" from then on (getParameterInfo), before the restart is raised.
// The component state "EDIT=<id>:<v>[,<v>...]" (values in millionths; applied, the DSP core untouched): the controller
// edits parameter <id> to each value in turn, beginEdit / performEdit / endEdit each, from setComponentState (a
// plugin's own edit, as its editor would make one).
// The controller's chunk is "SASC", 1, kind, then parameters 0 and 1's normalized values as f64.
// The component state "OUT=<millionths>" (applied, the DSP core untouched): the processor's next block writes one
// output parameter change, parameter 1 to that value at offset 0 (a plugin moving its own parameter).
// ATEXIT (SUMMONER_LAB_FIXTURE_ATEXIT): the gain with a C++ static destructor that dies, as a plugin's singletons
// can when a host's exit runs them while its threads are still inside the plugin. Armed only by a file
// `$HOME/summoner-lab-atexit.arm` (a test's scratch home): the destructor writes `$HOME/summoner-lab-atexit.ran`, then labAbort()
// (a SIGKILL under SUMMONER_LAB_DEATH=kill, so no crash report). Unarmed it does nothing. A host that ends with `_exit`
// never runs it.
// SIDECHAIN (SUMMONER_LAB_FIXTURE_SIDECHAIN, clean-room pins): the gain (kind 1) with a mono auxiliary input that is not
// default-active and a second, default-active stereo output: default channels 2 in, 4 out.
// REFUSESETUP (SUMMONER_LAB_FIXTURE_REFUSESETUP, clean-room pins): the gain (kind 1) whose first setupProcessing per
// instance returns kResultFalse; later calls prepare as usual.
// TITLES (SUMMONER_LAB_FIXTURE_TITLES): the gain plus parameters with non-ASCII titles (UTF-16: "Größe", "µs", "→",
// an emoji as a surrogate pair, a title past 64 characters, a title of 127 three-byte characters).
// FPCR (SUMMONER_LAB_FIXTURE_FPCR): the hostile effect (kind 3: its BurnMs, Gain) whose output is a REPORT of
// the FPU mode the host's process call ran under, written over the whole block after the DSP core ran:
//   ch0[f]  = a denormal-producing tail, computed in process(): x0 = 4 * FLT_MIN, x(f+1) = x(f) * 0.5f (volatile), so
//             frames 0-2 are normal and frames 3.. are subnormal without FZ, 0 with FZ;
//   ch1[0]  = 1; ch1[1] = float((fpcr >> 16) & 0xffff), ch1[2] = float(fpcr & 0xffff) (FPCR read with `mrs` at the
//             entry of process()); ch1[3] = FZ (bit 24), ch1[4] = FZ16 (bit 19), ch1[5] = AH (bit 1), ch1[6] = FIZ
//             (bit 0), ch1[7] = RMode (bits 23:22);
//   ch1[8]  = float(bits of (FLT_MIN * 0.5f)): an output flush shows as 0, else 4194304;
//   ch1[9]  = float(bits of (the smallest subnormal * 2^20)): an input flush shows as 0, else 1048576;
//   ch1[10], ch1[11] = FPCR's high / low 16 bits at the last setupProcessing; ch1[12], ch1[13] = at the last
//             setActive (true);
//   ch1[14] = process calls since the last setActive (true); ch1[15] = 1 when processContext is non-null;
//   ch1[16] = float(processContext->state); ch1[17] = float(projectTimeSamples); ch1[18] = setActive (true) calls;
//   ch1[19] = setupProcessing calls; ch1[20] = processMode; ch1[21] = numSamples; ch1[22] = input events;
//   ch1[23] = input parameter queues; then 0. Every value < 2^24: exact. No allocation, no lock.
#include "summoner_lab_dsp.h"
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
#include "lab_view.h"
#endif
#if defined(SUMMONER_LAB_FIXTURE_ATEXIT)
#include <cstdio>
#include <cstdlib>
#include <string>
#include <unistd.h>
#endif
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
#include <cfloat>
#endif

#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "public.sdk/source/main/pluginfactory.h"
#include "public.sdk/source/vst/vstaudioeffect.h"
#include "public.sdk/source/vst/vsteditcontroller.h"

#include <atomic>
#include <chrono>
#include <cstring>
#include <memory>
#include <thread>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace sas::lab::vst3 {

#ifndef SUMMONER_LAB_KIND
#error SUMMONER_LAB_KIND must be 1 (gain), 2 (synth) or 3 (hostile)
#endif
constexpr int kKind = SUMMONER_LAB_KIND;
constexpr bool kSynth = kKind == 2;
// The quad fixture's main buses are 4 wide (L R Ls Rs) and refuse stereo; the wide fixtures reuse it with
// SUMMONER_LAB_QUAD_IN / _OUT channels (1, 2, 4 or 6) on the input / output bus. Every other bundle is stereo.
#if defined(SUMMONER_LAB_FIXTURE_QUAD)
#ifndef SUMMONER_LAB_QUAD_IN
#define SUMMONER_LAB_QUAD_IN 4
#endif
#ifndef SUMMONER_LAB_QUAD_OUT
#define SUMMONER_LAB_QUAD_OUT 4
#endif
constexpr SpeakerArrangement arrOf(int32 n) {
    return n == 1 ? SpeakerArr::kMono : n == 2 ? SpeakerArr::kStereo : n == 4 ? SpeakerArr::k40Music : SpeakerArr::k51;
}
constexpr int32 kQuadIn = SUMMONER_LAB_QUAD_IN;
constexpr int32 kQuadOut = SUMMONER_LAB_QUAD_OUT;
constexpr SpeakerArrangement kMainIn = arrOf(kQuadIn);
constexpr SpeakerArrangement kMainArr = arrOf(kQuadOut);
#else
constexpr SpeakerArrangement kMainArr = SpeakerArr::kStereo;
constexpr SpeakerArrangement kMainIn = kMainArr;
#endif

constexpr ParamID kCcBase = 1000;
// The lying controller: real parameters, then ghosts (a colliding id), then failures (see LabController).
constexpr uint32_t kLiarReal = 5;
constexpr uint32_t kLiarGhosts = 10;
constexpr int16 kCcs[3] = {64, 120, 123};
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
// CTXMIDI's mapped controllers (every channel) and their parameter ids: kCtxBase + channel * 256 + cc. Every CTXMIDI
// line is behind this #if, so the other bundles build byte for byte as before.
constexpr ParamID kCtxBase = 2000;
constexpr int16 kCtxCcs[4] = {1, 64, kAfterTouch, kPitchBend};
#endif

// Class ids: "SASL", "LAB" + kind, role (P processor / C controller), variant.
#define SUMMONER_LAB_FUID(role, variant) FUID(0x5341534C, 0x4C414200 | kKind, role, variant)
#if defined(SUMMONER_LAB_FIXTURE_STATECTL)
// Its own controller class: this controller keeps its chunk, the gain's does not.
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0x43);
#elif defined(SUMMONER_LAB_FIXTURE_EDITOR)
// Its own controller class: this controller has an editor view.
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0x45);
#elif defined(SUMMONER_LAB_FIXTURE_ATEXIT)
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0x46);
#elif defined(SUMMONER_LAB_FIXTURE_TITLES)
// Its own controller class: this controller has the non-ASCII titles.
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0x47);
#elif defined(SUMMONER_LAB_FIXTURE_PARAMS)
// Its own controller class: this controller has units (IUnitInfo) and the PARAMS parameter shapes.
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0x49);
#else
const FUID kControllerId = SUMMONER_LAB_FUID(0x43, 0);
#endif
const FUID kLiarControllerId = SUMMONER_LAB_FUID(0x43, 77);

#if defined(SUMMONER_LAB_FIXTURE_ATEXIT)
// ATEXIT's static object (the header): its destructor runs only in a host that exits through `exit` (the C library's
// atexit list, this image's static destructors among them).
struct AtExitDeath {
    ~AtExitDeath() {
        const char* home = std::getenv("HOME");
        if (home == nullptr)
            return;
        const std::string dir(home);
        if (::access((dir + "/summoner-lab-atexit.arm").c_str(), F_OK) != 0)
            return;
        if (FILE* f = std::fopen((dir + "/summoner-lab-atexit.ran").c_str(), "w")) {
            std::fputs("the static destructor ran\n", f);
            std::fclose(f);
        }
        labAbort();
    }
};
AtExitDeath gAtExitDeath;
#endif
#if defined(SUMMONER_LAB_FIXTURE_TITLES)
// TITLES's extra parameters (ids kTitlesBase + i), after the gain's own; the last is 127 x U+2192, built at load.
constexpr ParamID kTitlesBase = 3000;
const char16* const kTitles[] = {
    u"Gr\u00f6\u00dfe",
    u"\u00b5s",
    u"\u2192",
    u"Knob \U0001F39B",
    u"A parameter title that runs past sixty-four characters: Gr\u00f6\u00dfe \u00b5s \u2192 \U0001F39B and on",
};
#endif

#if defined(SUMMONER_LAB_FIXTURE_PARAMS)
// PARAMS: the gain plus parameters that exercise the host's parameter-tree
// derivations: units 1 ("A", the root's), 2 ("B", the root's) and 3 ("C", B's), so the flattened tree differs from the
// index order; `kCanAutomate` with and without `kIsHidden`, a read-only meter without it, a list of 4 states, a
// duplicate title and an empty one. Ids kParamsBase + i, after the gain's own (index 0, the root unit).
constexpr ParamID kParamsBase = 4000;
struct LabParam {
    const char16* title;
    const char16* units;
    int32 steps;
    int32 flags;
    UnitID unit;
};
const LabParam kParams[] = {
    {u"Cutoff", u"Hz", 0, ParameterInfo::kCanAutomate, 2},
    {u"Mode", u"", 3, ParameterInfo::kCanAutomate | ParameterInfo::kIsList, kRootUnitId},
    {u"Hidden", u"", 0, ParameterInfo::kCanAutomate | ParameterInfo::kIsHidden, 1},
    {u"Meter", u"dB", 0, ParameterInfo::kIsReadOnly, kRootUnitId},
    {u"Cutoff", u"Hz", 0, ParameterInfo::kCanAutomate, 2},
    {u"", u"", 0, ParameterInfo::kCanAutomate, kRootUnitId},
    {u"Res", u"%", 0, ParameterInfo::kCanAutomate, 3},
};
using LabControllerBase = EditControllerEx1;
#else
using LabControllerBase = EditController;
#endif

std::unique_ptr<Processor> makeCore(const HostileConfig& cfg) {
    switch (kKind) {
        case 1: return makeGain();
#if defined(SUMMONER_LAB_FIXTURE_NEEDSNOTE)
        case 2: return makeSynth(true);
#else
        case 2: return makeSynth();
#endif
        default: return makeHostile(cfg);
    }
}

bool readAll(IBStream* s, std::vector<uint8_t>& out) {
    out.clear();
    uint8_t buf[4096];
    for (;;) {
        int32 got = 0;
        if (s->read(buf, sizeof(buf), &got) != kResultOk)
            return false;
        if (got <= 0)
            return true;
        out.insert(out.end(), buf, buf + got);
    }
}

bool writeAll(IBStream* s, const std::vector<uint8_t>& bytes) {
    int32 done = 0;
    return s->write(const_cast<uint8_t*>(bytes.data()), int32(bytes.size()), &done) == kResultOk
           && done == int32(bytes.size());
}

class LabProcessor : public AudioEffect {
public:
    LabProcessor(const HostileConfig& cfg, const FUID& controller) : cfg_(cfg), core_(makeCore(cfg)) {
        setControllerClass(controller);
    }

    tresult PLUGIN_API initialize(FUnknown* context) SMTG_OVERRIDE {
        const tresult r = AudioEffect::initialize(context);
        if (r != kResultOk)
            return r;
        if (kSynth)
            addEventInput(STR16("MIDI In"), 16);
        else
            addAudioInput(STR16("In"), kMainIn);
        addAudioOutput(STR16("Out"), kMainArr);
#if defined(SUMMONER_LAB_FIXTURE_SIDECHAIN)
        // SIDECHAIN: a mono auxiliary input that is NOT default-active, and a second, default-active stereo output.
        addAudioInput(STR16("Side"), SpeakerArr::kMono, kAux, 0);
        addAudioOutput(STR16("Out 2"), SpeakerArr::kStereo, kAux, BusInfo::kDefaultActive);
#endif
        return kResultOk;
    }

    tresult PLUGIN_API setBusArrangements(SpeakerArrangement* ins, int32 numIns, SpeakerArrangement* outs,
                                          int32 numOuts) SMTG_OVERRIDE {
        // Its own arrangement only (stereo, the quad fixture's 4 wide): anything else is refused, as a strict plugin
        // would.
        if (numOuts != 1 || outs[0] != kMainArr)
            return kResultFalse;
        if (!kSynth && (numIns != 1 || ins[0] != kMainIn))
            return kResultFalse;
        return AudioEffect::setBusArrangements(ins, numIns, outs, numOuts);
    }

    tresult PLUGIN_API canProcessSampleSize(int32 size) SMTG_OVERRIDE {
        return size == kSample32 ? kResultTrue : kResultFalse;
    }

    tresult PLUGIN_API setupProcessing(ProcessSetup& setup) SMTG_OVERRIDE {
#if defined(SUMMONER_LAB_FIXTURE_REFUSESETUP)
        // REFUSESETUP: the first setupProcessing of an instance is refused (kResultFalse), nothing prepared.
        if (!setupRefused_) {
            setupRefused_ = true;
            return kResultFalse;
        }
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        ++setups_;
#endif
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
        fpSetupFpcr_ = labFpcr();
        ++fpSetups_;
#endif
        core_->prepare(setup.sampleRate, uint32_t(std::max<int32>(setup.maxSamplesPerBlock, 1)));
        midi_.reserve(1024 + 3 * 64);
        return AudioEffect::setupProcessing(setup);
    }

    tresult PLUGIN_API setActive(TBool state) SMTG_OVERRIDE {
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        if (state)
            ++activations_;
#endif
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
        if (state) {
            fpActiveFpcr_ = labFpcr();
            ++fpActivations_;
            fpProcs_ = 0;
        }
#endif
        if (state)
            core_->reset();
        return AudioEffect::setActive(state);
    }

    uint32 PLUGIN_API getLatencySamples() SMTG_OVERRIDE { return core_->latency(); }

    tresult PLUGIN_API process(ProcessData& data) SMTG_OVERRIDE {
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
        const uint64_t fpcr = labFpcr();  // first: the mode the host called process() under
        ++fpProcs_;
#endif
        midi_.clear();
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        ctxCount_ = 0;
#endif
        if (auto* changes = data.inputParameterChanges) {
            for (int32 i = 0; i < changes->getParameterCount(); ++i) {
                IParamValueQueue* q = changes->getParameterData(i);
                if (q == nullptr || q->getPointCount() <= 0)
                    continue;
                const ParamID id = q->getParameterId();
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
                for (int32 p = 0; p < q->getPointCount() && ctxCount_ < kCtxMax; ++p) {
                    int32 offset = 0;
                    ParamValue v = 0;
                    if (q->getPoint(p, offset, v) == kResultOk)
                        ctx_[ctxCount_++] = CtxPoint{id, offset, v};
                }
                if (id >= kCtxBase)
                    continue;
#endif
                if (kSynth && id >= kCcBase) {
                    for (int32 p = 0; p < q->getPointCount(); ++p) {
                        int32 offset = 0;
                        ParamValue v = 0;
                        if (q->getPoint(p, offset, v) == kResultOk)
                            pushMidi(uint32_t(std::max<int32>(offset, 0)), 0xb0, uint8_t(id - kCcBase),
                                     uint8_t(std::min(127.0, std::max(0.0, v * 127.0 + 0.5))));
                    }
                    continue;
                }
                int32 offset = 0;
                ParamValue v = 0;
                if (q->getPoint(q->getPointCount() - 1, offset, v) == kResultOk)
                    core_->setParam(uint32_t(id), float(v));
            }
        }
        if (auto* events = data.inputEvents) {
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
            evCount_ = 0;
#endif
            for (int32 i = 0; i < events->getEventCount(); ++i) {
                Event e{};
                if (events->getEvent(i, e) != kResultOk)
                    continue;
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
                if (evCount_ < kEvMax) {
                    float ch = 0, x = 0, y = 0;
                    switch (e.type) {
                        case Event::kNoteOnEvent:
                            ch = e.noteOn.channel, x = e.noteOn.pitch, y = e.noteOn.velocity;
                            break;
                        case Event::kNoteOffEvent:
                            ch = e.noteOff.channel, x = e.noteOff.pitch, y = e.noteOff.velocity;
                            break;
                        case Event::kPolyPressureEvent:
                            ch = e.polyPressure.channel, x = e.polyPressure.pitch, y = e.polyPressure.pressure;
                            break;
                        case Event::kLegacyMIDICCOutEvent:
                            ch = e.midiCCOut.channel, x = e.midiCCOut.controlNumber,
                            y = float(e.midiCCOut.value) + 1000.0f * float(e.midiCCOut.value2);
                            break;
                        default:
                            break;
                    }
                    ev_[evCount_++] = EvRec{float(e.type) + 1.0f, float(e.sampleOffset), ch, x, y};
                }
#endif
                const uint32_t at = uint32_t(std::max<int32>(e.sampleOffset, 0));
                if (e.type == Event::kNoteOnEvent)
                    pushMidi(at, uint8_t(0x90 | (e.noteOn.channel & 0x0f)), uint8_t(e.noteOn.pitch),
                             uint8_t(std::min(127.0f, std::max(0.0f, e.noteOn.velocity * 127.0f + 0.5f))));
                else if (e.type == Event::kNoteOffEvent)
                    pushMidi(at, uint8_t(0x80 | (e.noteOff.channel & 0x0f)), uint8_t(e.noteOff.pitch), 0);
            }
        }
        // In offset order, stable: insertion, no allocation (reserved at setup).
        for (size_t i = 1; i < midi_.size(); ++i)
            for (size_t j = i; j > 0 && midi_[j - 1].offset > midi_[j].offset; --j)
                std::swap(midi_[j - 1], midi_[j]);
        if (data.numOutputs < 1 || data.numSamples <= 0)
            return kResultOk;
        AudioBusBuffers& out = data.outputs[0];
        const uint32_t frames = uint32_t(data.numSamples);
#if defined(SUMMONER_LAB_FIXTURE_QUAD)
        // Trusts its arrangement, as a plugin may (VST3: numChannels is the bus's): every channel of each bus
        // is dereferenced, whatever the host says. Out c (c < 2) = in c (0 past the inputs) plus every input from 2
        // on (a host must give zeros past its block); outs from 2 on are 0.75 (a host must drop them).
        if (data.numInputs < 1)
            return kResultFalse;
        {
            const AudioBusBuffers& in = data.inputs[0];
            for (int32 c = 0; c < kQuadOut; ++c) {
                float* o = out.channelBuffers32[c];
                for (uint32_t i = 0; i < frames; ++i) {
                    float v = 0.75f;
                    if (c < 2) {
                        v = c < kQuadIn ? in.channelBuffers32[c][i] : 0.0f;
                        for (int32 k = 2; k < kQuadIn; ++k)
                            v += in.channelBuffers32[k][i];
                    }
                    o[i] = v;
                }
            }
            out.silenceFlags = 0;
            return in.numChannels == kQuadIn && out.numChannels == kQuadOut ? kResultOk : kResultFalse;
        }
#endif
        if (!kSynth && data.numInputs > 0) {
            const AudioBusBuffers& in = data.inputs[0];
            for (int32 c = 0; c < out.numChannels; ++c) {
                float* o = out.channelBuffers32[c];
                const float* x = c < in.numChannels ? in.channelBuffers32[c] : nullptr;
                if (x == nullptr)
                    std::memset(o, 0, frames * sizeof(float));
                else if (x != o)
                    std::memcpy(o, x, frames * sizeof(float));
            }
        } else {
            for (int32 c = 0; c < out.numChannels; ++c)
                std::memset(out.channelBuffers32[c], 0, frames * sizeof(float));
        }
        Block b;
        b.channels = out.channelBuffers32;
        b.numChannels = uint32_t(out.numChannels);
        b.frames = frames;
        b.midi = midi_.data();
        b.midiCount = uint32_t(midi_.size());
        // The transport as the host gave it (the hostile core's TRANSPORT reporter reads it; null without one).
        Transport t;
        if (const ProcessContext* pc = data.processContext) {
            t.playing = (pc->state & ProcessContext::kPlaying) != 0;
            t.recording = (pc->state & ProcessContext::kRecording) != 0;
            t.cycleActive = (pc->state & ProcessContext::kCycleActive) != 0;
            t.tempoValid = (pc->state & ProcessContext::kTempoValid) != 0;
            t.musicValid = (pc->state & ProcessContext::kProjectTimeMusicValid) != 0;
            t.tempo = pc->tempo;
            t.projectTimeMusic = pc->projectTimeMusic;
            t.projectTimeSamples = pc->projectTimeSamples;
            b.transport = &t;
        }
        core_->process(b);  // the hostile core may throw from here, as a misbehaving plugin does
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        if (data.outputParameterChanges != nullptr && outPending_.exchange(false)) {
            int32 index = 0;
            if (IParamValueQueue* q = data.outputParameterChanges->addParameterData(ParamID(1), index))
                q->addPoint(0, outValue_.load(), index);
        }
        if (out.numChannels >= 2)
            report(data.processContext, out.channelBuffers32[0], out.channelBuffers32[1], frames);
#endif
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
        if (out.numChannels >= 2)
            fpReport(data, fpcr, out.channelBuffers32[0], out.channelBuffers32[1], frames);
#endif
        out.silenceFlags = 0;
        return core_->blockFailed() ? kResultFalse : kResultOk;
    }

    tresult PLUGIN_API getState(IBStream* state) SMTG_OVERRIDE {
        if (core_->getStateFails())
            return kResultFalse;  // FAIL_GETSTATE: nothing written; the controller's chunk is unaffected
        return state != nullptr && writeAll(state, core_->getState()) ? kResultOk : kResultFalse;
    }

    tresult PLUGIN_API setState(IBStream* state) SMTG_OVERRIDE {
        std::vector<uint8_t> bytes;
        if (state == nullptr || !readAll(state, bytes))
            return kResultFalse;
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
        if (bytes.size() > 2 && std::memcmp(bytes.data(), "UI", 2) == 0)
            return kResultOk;  // the controller's (setComponentState); the core keeps its state
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        if (bytes.size() == 5 && (std::memcmp(bytes.data(), "REMAP", 5) == 0 || std::memcmp(bytes.data(), "UNMAP", 5) == 0))
            return kResultOk;  // the controller's (setComponentState); the core keeps its state
        if (bytes.size() > 8 && std::memcmp(bytes.data(), "RESTART=", 8) == 0)
            return kResultOk;  // likewise
        if (bytes.size() > 5 && std::memcmp(bytes.data(), "EDIT=", 5) == 0)
            return kResultOk;  // likewise
        if (bytes.size() > 4 && std::memcmp(bytes.data(), "OUT=", 4) == 0) {
            uint32_t m = 0;
            for (size_t i = 4; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && m < 1u << 24; ++i)
                m = m * 10 + uint32_t(bytes[i] - '0');
            outValue_.store(double(m) / 1e6);
            outPending_.store(true);
            return kResultOk;
        }
#endif
        return core_->setState(bytes.data(), bytes.size()) ? kResultOk : kResultFalse;
    }

private:
#if defined(SUMMONER_LAB_FIXTURE_FPCR)
    // FPCR's report (the header's layout), over the whole block. The arithmetic goes through volatiles, so the
    // compiler cannot fold it: it runs here, under the FPCR the host set.
    void fpReport(const ProcessData& data, uint64_t fpcr, float* l, float* r, uint32_t frames) {
        volatile float x = 4.0f * FLT_MIN;
        volatile float half = 0.5f;
        for (uint32_t f = 0; f < frames; ++f) {
            l[f] = x;
            x = x * half;
        }
        volatile float m = FLT_MIN;
        const float outFlush = m * half;
        uint32_t one = 1;
        float tiny = 0.0f;
        std::memcpy(&tiny, &one, 4);
        volatile float sub = tiny;
        volatile float scale = 1048576.0f;  // 2^20
        const float inFlush = sub * scale;
        uint32_t ob = 0, ib = 0;
        std::memcpy(&ob, &outFlush, 4);
        std::memcpy(&ib, &inFlush, 4);
        const ProcessContext* pc = data.processContext;
        const float v[24] = {1.0f,
                             float((fpcr >> 16) & 0xffff),
                             float(fpcr & 0xffff),
                             float((fpcr >> 24) & 1),
                             float((fpcr >> 19) & 1),
                             float((fpcr >> 1) & 1),
                             float(fpcr & 1),
                             float((fpcr >> 22) & 3),
                             float(ob),
                             float(ib),
                             float((fpSetupFpcr_ >> 16) & 0xffff),
                             float(fpSetupFpcr_ & 0xffff),
                             float((fpActiveFpcr_ >> 16) & 0xffff),
                             float(fpActiveFpcr_ & 0xffff),
                             float(fpProcs_),
                             pc != nullptr ? 1.0f : 0.0f,
                             pc != nullptr ? float(pc->state) : 0.0f,
                             pc != nullptr ? float(pc->projectTimeSamples) : 0.0f,
                             float(fpActivations_),
                             float(fpSetups_),
                             float(data.processMode),
                             float(data.numSamples),
                             data.inputEvents != nullptr ? float(data.inputEvents->getEventCount()) : 0.0f,
                             data.inputParameterChanges != nullptr
                                 ? float(data.inputParameterChanges->getParameterCount())
                                 : 0.0f};
        std::memset(r, 0, frames * sizeof(float));
        for (uint32_t f = 0; f < 24 && f < frames; ++f)
            r[f] = v[f];
    }
    static uint64_t labFpcr() {
        uint64_t v = 0;
#if defined(__aarch64__)
        asm volatile("mrs %0, fpcr" : "=r"(v));
#endif
        return v;
    }
    uint64_t fpSetupFpcr_ = 0;   // FPCR at the last setupProcessing
    uint64_t fpActiveFpcr_ = 0;  // FPCR at the last setActive (true)
    uint32_t fpProcs_ = 0;       // process calls since the last setActive (true)
    uint32_t fpActivations_ = 0;
    uint32_t fpSetups_ = 0;
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
    // CTXMIDI's report (the header's layout), over the whole block.
    void report(const ProcessContext* pc, float* l, float* r, uint32_t frames) {
        std::memset(l, 0, frames * sizeof(float));
        std::memset(r, 0, frames * sizeof(float));
        for (uint32_t i = 0; i < ctxCount_ && 3 * i + 2 < frames; ++i) {
            l[3 * i] = float(ctx_[i].id + 1);
            l[3 * i + 1] = float(ctx_[i].offset);
            l[3 * i + 2] = float(ctx_[i].value);
        }
        if (pc == nullptr)
            return;
        const float v[17] = {1.0f,
                             float(pc->state),
                             float(pc->sampleRate),
                             float(pc->projectTimeSamples),
                             pc->systemTime != 0 ? 1.0f : 0.0f,
                             float(pc->continousTimeSamples),
                             float(pc->projectTimeMusic),
                             float(pc->barPositionMusic),
                             float(pc->cycleStartMusic),
                             float(pc->cycleEndMusic),
                             float(pc->tempo),
                             float(pc->timeSigNumerator),
                             float(pc->timeSigDenominator),
                             float(pc->smpteOffsetSubframes),
                             float(pc->frameRate.framesPerSecond),
                             float(pc->frameRate.flags),
                             float(pc->samplesToNextClock)};
        for (uint32_t f = 0; f < 17 && f < frames; ++f)
            r[f] = v[f];
        if (frames > 18) {
            r[17] = float(activations_);
            r[18] = float(setups_);
        }
        for (uint32_t i = 0; i < evCount_ && 20 + 5 * i + 4 < frames; ++i) {
            const float* src = &ev_[i].type;
            for (uint32_t k = 0; k < 5; ++k)
                r[20 + 5 * i + k] = src[k];
        }
    }
    struct EvRec {
        float type, offset, channel, x, y;
    };
    static constexpr uint32_t kEvMax = 16;
    EvRec ev_[kEvMax] = {};
    uint32_t evCount_ = 0;
    std::atomic<double> outValue_{0.0};  // "OUT=": the value, and that the next block owes it
    std::atomic<bool> outPending_{false};
    uint32_t activations_ = 0;  // setActive (true) calls (message thread; read by process between them)
    uint32_t setups_ = 0;       // setupProcessing calls

    struct CtxPoint {
        ParamID id;
        int32 offset;
        ParamValue value;
    };
    static constexpr uint32_t kCtxMax = 64;
    CtxPoint ctx_[kCtxMax] = {};
    uint32_t ctxCount_ = 0;
#endif

    void pushMidi(uint32_t at, uint8_t s, uint8_t d1, uint8_t d2) {
        if (midi_.size() == midi_.capacity())
            return;  // never grow on the audio thread
        MidiEvent m;
        m.offset = at;
        m.size = 3;
        m.data[0] = s;
        m.data[1] = d1;
        m.data[2] = d2;
        midi_.push_back(m);
    }

#if defined(SUMMONER_LAB_FIXTURE_REFUSESETUP)
    bool setupRefused_ = false;  // the first setupProcessing has been refused
#endif
    HostileConfig cfg_;
    std::unique_ptr<Processor> core_;
    std::vector<MidiEvent> midi_;
};

class LabController : public LabControllerBase, public IMidiMapping {
public:
    explicit LabController(const HostileConfig& cfg) : cfg_(cfg) {}

    tresult PLUGIN_API initialize(FUnknown* context) SMTG_OVERRIDE {
        const tresult r = LabControllerBase::initialize(context);
        if (r != kResultOk)
            return r;
        const auto core = makeCore(HostileConfig{false, false, 0, cfg_.lieParamCount});
        // The liar registers only its first kLiarReal parameters, and lies about the rest (getParameterCount,
        // getParameterInfo below).
        const uint32_t registered = cfg_.lieParamCount != 0 ? kLiarReal : core->paramCount();
        for (uint32_t i = 0; i < registered; ++i) {
            const ParamInfo info = core->paramInfo(i);
            String128 title;
            for (size_t k = 0; k < 127 && info.name[k] != 0; ++k)
                title[k] = char16(info.name[k]), title[k + 1] = 0;
            if (info.name[0] == 0)
                title[0] = 0;
            parameters.addParameter(title, nullptr, 0, info.defaultValue, ParameterInfo::kCanAutomate, ParamID(i));
        }
        if (kSynth)
            for (int16 cc : kCcs)
                parameters.addParameter(STR16("MIDI CC"), nullptr, 127, 0.0, ParameterInfo::kNoFlags,
                                        kCcBase + ParamID(cc));
#if defined(SUMMONER_LAB_FIXTURE_TITLES)
        ParamID tid = kTitlesBase;
        for (const char16* t : kTitles)
            parameters.addParameter(t, nullptr, 0, 0.0, ParameterInfo::kNoFlags, tid++);
        String128 arrows;
        for (int k = 0; k < 127; ++k)
            arrows[k] = char16(0x2192);
        arrows[127] = 0;
        parameters.addParameter(arrows, nullptr, 0, 0.0, ParameterInfo::kNoFlags, tid);
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        for (int16 ch = 0; ch < 16; ++ch)
            for (int16 cc : kCtxCcs)
                parameters.addParameter(STR16("MIDI Map"), nullptr, 0, 0.0, ParameterInfo::kNoFlags,
                                        kCtxBase + ParamID(ch) * 256 + ParamID(cc));
#endif
#if defined(SUMMONER_LAB_FIXTURE_PARAMS)
        addUnit(new Unit(STR16("Root"), kRootUnitId, kNoParentUnitId));
        addUnit(new Unit(STR16("A"), 1, kRootUnitId));
        addUnit(new Unit(STR16("B"), 2, kRootUnitId));
        addUnit(new Unit(STR16("C"), 3, 2));
        ParamID pid = kParamsBase;
        for (const LabParam& lp : kParams)
            parameters.addParameter(lp.title, lp.units, lp.steps, 0.25, lp.flags, int32(pid++), lp.unit);
#endif
        return kResultOk;
    }

    // The component's chunk, as the controller learns it: the DSP core's state bytes; its parameters follow.
    tresult PLUGIN_API setComponentState(IBStream* state) SMTG_OVERRIDE {
        std::vector<uint8_t> bytes;
        if (state == nullptr || !readAll(state, bytes))
            return kResultFalse;
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
        auto num = [&](size_t& i) {
            uint32_t n = 0;
            for (; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && n < 1u << 24; ++i)
                n = n * 10 + uint32_t(bytes[i] - '0');
            return n;
        };
        // The guard's knobs (lab_view.h) UIQUIET / UIRESETONVIEW, anywhere in a "UI" component state (the processor keeps
        // its own state for those), separated by ';'.
        {
            const std::string text(bytes.begin(), bytes.end());
            if (text.find("UIQUIET") != std::string::npos)
                ed_.quiet = true;
            if (text.find("UIRESETONVIEW") != std::string::npos)
                ed_.resetOnView = true;
            const size_t r2 = text.find("UIRESET2=");
            if (r2 != std::string::npos) {
                size_t i = r2 + 9;
                ed_.reset2Ms = num(i);
            }
        }
        if (bytes.size() == 7 && std::memcmp(bytes.data(), "UICRASH", 7) == 0) {
            ed_.crashNext = true;
            return kResultOk;
        }
        if (bytes.size() > 7 && std::memcmp(bytes.data(), "UIHOLD=", 7) == 0) {
            size_t i = 7;
            ed_.holdMs = num(i);
            return kResultOk;
        }
        if (bytes.size() > 9 && std::memcmp(bytes.data(), "UIRESIZE=", 9) == 0) {
            size_t i = 9;
            const uint32_t w = num(i);
            ++i;
            const uint32_t h = num(i);
            sasLabResize(&ed_, int32(w), int32(h));
            return kResultOk;
        }
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        if (bytes.size() == 5 && std::memcmp(bytes.data(), "UNMAP", 5) == 0) {
            unmapped_ = true;  // no controller mapped any more, so the host sends them as events
            if (componentHandler)
                componentHandler->restartComponent(kMidiCCAssignmentChanged);
            return kResultOk;
        }
        if (bytes.size() == 5 && std::memcmp(bytes.data(), "REMAP", 5) == 0) {
            remapped_ = true;
            if (componentHandler)
                componentHandler->restartComponent(kMidiCCAssignmentChanged);
            return kResultOk;
        }
        if (bytes.size() > 5 && std::memcmp(bytes.data(), "EDIT=", 5) == 0) {
            size_t i = 5;
            uint32_t id = 0;
            for (; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && id < 1u << 24; ++i)
                id = id * 10 + uint32_t(bytes[i] - '0');
            while (i < bytes.size() && (bytes[i] == ':' || bytes[i] == ',')) {
                ++i;
                uint32_t m = 0;
                for (; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && m < 1u << 24; ++i)
                    m = m * 10 + uint32_t(bytes[i] - '0');
                if (componentHandler) {
                    componentHandler->beginEdit(ParamID(id));
                    componentHandler->performEdit(ParamID(id), ParamValue(m) / 1e6);
                    componentHandler->endEdit(ParamID(id));
                }
            }
            return kResultOk;
        }
        if (bytes.size() > 8 && std::memcmp(bytes.data(), "RESTART=", 8) == 0) {
            int32 flags = 0;
            for (size_t i = 8; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && flags < 1 << 20; ++i)
                flags = flags * 10 + int32(bytes[i] - '0');
            if (flags & kParamTitlesChanged)
                retitled_ = true;
            if (componentHandler)
                componentHandler->restartComponent(flags);
            return kResultOk;
        }
#endif
        if (kKind == 3 && bytes.size() > 6 && std::memcmp(bytes.data(), "LATER=", 6) == 0) {
            // The processor changes its latency <ms> later; this controller tells the host 20 ms after that, from a
            // thread of its own, as a plugin that notices its own latency change does.
            uint64_t ms = 0;
            for (size_t i = 6; i < bytes.size() && bytes[i] >= '0' && bytes[i] <= '9' && ms < 60000; ++i)
                ms = ms * 10 + uint64_t(bytes[i] - '0');
            IPtr<IComponentHandler> handler = componentHandler;
            std::thread([handler, ms] {
                std::this_thread::sleep_for(std::chrono::milliseconds(ms + 20));
                if (handler)
                    handler->restartComponent(kLatencyChanged);
            }).detach();
            return kResultOk;
        }
        auto core = makeCore(HostileConfig{false, false, 0, cfg_.lieParamCount});
        if (!core->setState(bytes.data(), bytes.size()))
            return kResultOk;  // a special (hostile) state carries no parameters
        for (uint32_t i = 0; i < core->paramCount(); ++i)
            setParamNormalized(ParamID(i), core->param(i));
        return kResultOk;
    }

#if defined(SUMMONER_LAB_FIXTURE_STATECTL)
    // A controller that KEEPS its chunk, so a state read shows whether a controller chunk
    // was applied (the gain's controller checks it and stores nothing). Accepts what the gain's accepts.
    tresult PLUGIN_API getState(IBStream* state) SMTG_OVERRIDE {
        return state != nullptr && writeAll(state, kept_) ? kResultOk : kResultFalse;
    }

    tresult PLUGIN_API setState(IBStream* state) SMTG_OVERRIDE {
        std::vector<uint8_t> bytes;
        if (state == nullptr || !readAll(state, bytes))
            return kResultFalse;
        if (bytes.size() != 12 || std::memcmp(bytes.data(), "SASC", 4) != 0)
            return kResultFalse;
        kept_ = bytes;
        return kResultOk;
    }
#else
    tresult PLUGIN_API getState(IBStream* state) SMTG_OVERRIDE {
        std::vector<uint8_t> v = {'S', 'A', 'S', 'C', 1, 0, 0, 0, uint8_t(kKind), 0, 0, 0};
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
        // Then the editor's w, h, attaches, removes, onSizes (u32 each), little-endian; with UIQUIET
        // only `wasReset` (u32), so an open is no state change (the guard's tests).
        if (ed_.quiet) {
            uint8_t b[4];
            const uint32_t r = ed_.wasReset.load();
            std::memcpy(b, &r, 4);
            v.insert(v.end(), b, b + 4);
        } else {
            for (uint32_t x : {uint32_t(ed_.w), uint32_t(ed_.h), ed_.attaches, ed_.removes, ed_.onSizes}) {
                uint8_t b[4];
                std::memcpy(b, &x, 4);
                v.insert(v.end(), b, b + 4);
            }
        }
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        // Then parameters 0 and 1's values as this controller holds them (f64, little-endian), so a host's
        // setParamNormalized is observable in the state read.
        for (ParamID id : {ParamID(0), ParamID(1)}) {
            const double x = getParamNormalized(id);
            uint8_t b[8];
            std::memcpy(b, &x, 8);
            v.insert(v.end(), b, b + 8);
        }
#endif
        return state != nullptr && writeAll(state, v) ? kResultOk : kResultFalse;
    }

    tresult PLUGIN_API setState(IBStream* state) SMTG_OVERRIDE {
        std::vector<uint8_t> bytes;
        if (state == nullptr || !readAll(state, bytes))
            return kResultFalse;
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
        // UIQUIET: a chunk's `wasReset` is applied (the guard's restore sets it back to the snapshot's).
        if (ed_.quiet && bytes.size() >= 16 && std::memcmp(bytes.data(), "SASC", 4) == 0) {
            uint32_t r = 0;
            std::memcpy(&r, bytes.data() + 12, 4);
            ed_.wasReset.store(r);
        }
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI) || defined(SUMMONER_LAB_FIXTURE_EDITOR)
        return bytes.size() >= 12 && std::memcmp(bytes.data(), "SASC", 4) == 0 ? kResultOk : kResultFalse;
#endif
        return bytes.size() == 12 && std::memcmp(bytes.data(), "SASC", 4) == 0 ? kResultOk : kResultFalse;
    }
#endif

    // The lying variant: it claims lieParamCount parameters; getParameterInfo succeeds for the
    // first kLiarReal, returns a colliding id (0, the gain's) with a "ghost" title up to kLiarGhosts, and fails
    // beyond that.
    int32 PLUGIN_API getParameterCount() SMTG_OVERRIDE {
        return cfg_.lieParamCount != 0 ? int32(cfg_.lieParamCount) : LabControllerBase::getParameterCount();
    }

    tresult PLUGIN_API getParameterInfo(int32 index, ParameterInfo& info) SMTG_OVERRIDE {
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        const tresult r = LabControllerBase::getParameterInfo(index, info);
        if (r == kResultOk && retitled_ && info.id >= kCtxBase) {
            const char* t = "MIDI Map 2";
            int k = 0;
            for (; t[k] != 0; ++k)
                info.title[k] = char16(t[k]);
            info.title[k] = 0;
        }
        return r;
#endif
        if (cfg_.lieParamCount == 0 || index < int32(kLiarReal))
            return LabControllerBase::getParameterInfo(index, info);
        if (index < int32(kLiarGhosts)) {
            info = ParameterInfo{};
            info.id = 0;
            const char* t = "ghost";
            for (int k = 0; t[k] != 0; ++k)
                info.title[k] = char16(t[k]);
            return kResultOk;
        }
        return kResultFalse;
    }

    tresult PLUGIN_API getMidiControllerAssignment(int32 busIndex, int16 channel, CtrlNumber cc,
                                                   ParamID& id) SMTG_OVERRIDE {
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
        if (unmapped_ || busIndex != 0 || channel < 0 || channel > 15)
            return kResultFalse;
        for (int16 c : kCtxCcs)
            if (c == cc) {
                const int16 m = !remapped_ ? c : c == 1 ? int16(64) : c == 64 ? int16(1) : c;
                id = kCtxBase + ParamID(channel) * 256 + ParamID(m);
                return kResultTrue;
            }
        return kResultFalse;
#endif
        if (!kSynth || busIndex != 0 || channel != 0)
            return kResultFalse;
        for (int16 c : kCcs)
            if (c == cc) {
                id = kCcBase + ParamID(c);
                return kResultTrue;
            }
        return kResultFalse;
    }

#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
    IPlugView* PLUGIN_API createView(FIDString name) SMTG_OVERRIDE {
        if (name == nullptr || std::strcmp(name, ViewType::kEditor) != 0 || ed_.view != nullptr)
            return nullptr;
        return sasLabMakeView(&ed_);
    }
#endif

    OBJ_METHODS(LabController, LabControllerBase)
    DEFINE_INTERFACES
        DEF_INTERFACE(IMidiMapping)
    END_DEFINE_INTERFACES(LabControllerBase)
    REFCOUNT_METHODS(LabControllerBase)

private:
    HostileConfig cfg_;
#if defined(SUMMONER_LAB_FIXTURE_STATECTL)
    std::vector<uint8_t> kept_ = {'S', 'A', 'S', 'C', 1, 0, 0, 0, uint8_t(kKind), 0, 0, 0};  // the gain's, until set
#endif
#if defined(SUMMONER_LAB_FIXTURE_EDITOR)
    LabEditorState ed_;  // what the editor view did
#endif
#if defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
    bool remapped_ = false;  // "REMAP" swapped CC 1 and CC 64 (message thread only)
    bool unmapped_ = false;  // "UNMAP": no controller mapped
    bool retitled_ = false;  // "RESTART=" with kParamTitlesChanged renamed the MIDI Map parameters
#endif
};

template <bool CtorCrash, bool PrepareCrash, uint32_t PrepareMs, uint32_t Lie, bool CtorHang = false,
          bool BadState = false>
FUnknown* createProcessor(void*) {
    const HostileConfig cfg{CtorCrash, PrepareCrash, PrepareMs, Lie, CtorHang, BadState};
    return static_cast<IAudioProcessor*>(new LabProcessor(cfg, Lie != 0 ? kLiarControllerId : kControllerId));
}

template <uint32_t Lie>
FUnknown* createController(void*) {
    return static_cast<IEditController*>(new LabController(HostileConfig{false, false, 0, Lie}));
}

}  // namespace sas::lab::vst3

using namespace sas::lab::vst3;

// The scan fixtures: one class per bundle, its name and its construction config (createProcessor's arguments).
#if defined(SUMMONER_LAB_FIXTURE_HANG)
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Hang"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME " (hangs in constructor)"
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, true, false>)
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_NEEDSNOTE)
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Needs Note"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME " (fails without a note)"
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_SUB "Instrument|Synth"
#elif defined(SUMMONER_LAB_FIXTURE_BADSTATE)
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Bad State"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME " (refuses its own state)"
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, true>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x41
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_KNOBS)
// The hostile core's knob class ALONE (SLOW / BIG / FAIL_GETSTATE, TRANSPORT, LATER=), with no constructor- or
// prepare-crash class, so a scan can read the bundle.
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Knobs"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x42
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_STATECTL)
// The gain (kind 1), its controller keeping its chunk (getState returns the last one set).
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab State Controller"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x43
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_EDITOR)
// The gain (kind 1) with an editor view (lab_view.mm): the host's editor side.
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Editor"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x45
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_ATEXIT)
// The gain (kind 1) with a static destructor that dies (the header's ATEXIT).
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Atexit"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x46
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_TITLES)
// The gain (kind 1) with non-ASCII parameter titles (the header's TITLES).
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Titles"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x47
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_PARAMS)
// The gain (kind 1) with units and the parameter-tree shapes (the header's PARAMS).
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Params"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x49
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_FPCR)
// The hostile effect (kind 3) reporting the FPU mode of its process call (the header's FPCR layout).
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab FPCR"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x48
#define SUMMONER_LAB_SUB "Fx"
#elif defined(SUMMONER_LAB_FIXTURE_CTXMIDI)
// The ProcessContext and MIDI-mapping reporter (the header's CTXMIDI layout), a synth shell.
#define SUMMONER_LAB_FIXTURE 1
#define SUMMONER_LAB_NAME "Summoner Lab Context MIDI"
#define SUMMONER_LAB_FIXTURE_CLASS SUMMONER_LAB_NAME
#define SUMMONER_LAB_FIXTURE_CREATE (createProcessor<false, false, 0, 0, false, false>)
#define SUMMONER_LAB_FIXTURE_VARIANT 0x44
#define SUMMONER_LAB_SUB "Instrument|Synth"
#elif SUMMONER_LAB_KIND == 1
#define SUMMONER_LAB_NAME "Summoner Lab Gain"
#define SUMMONER_LAB_SUB "Fx"
#elif SUMMONER_LAB_KIND == 2
#define SUMMONER_LAB_NAME "Summoner Lab Synth"
#define SUMMONER_LAB_SUB "Instrument|Synth"
#else
#define SUMMONER_LAB_NAME "Summoner Lab Hostile"
#define SUMMONER_LAB_SUB "Fx"
#endif

BEGIN_FACTORY_DEF("Summoner Lab", "https://github.com/shiehn/summoner-plugin-host", "mailto:none")
#if defined(SUMMONER_LAB_FIXTURE)
    // A scan fixture (a bundle of its own, so the hostile bundle's constructor crash cannot pre-empt it): one
    // processor, as SUMMONER_LAB_FIXTURE_CREATE builds it.
#ifndef SUMMONER_LAB_FIXTURE_VARIANT
#define SUMMONER_LAB_FIXTURE_VARIANT 0x40
#endif
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, SUMMONER_LAB_FIXTURE_VARIANT)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               SUMMONER_LAB_FIXTURE_CLASS, Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0", kVstVersionString,
               SUMMONER_LAB_FIXTURE_CREATE)
    DEF_CLASS2(INLINE_UID_FROM_FUID(kControllerId), PClassInfo::kManyInstances, kVstComponentControllerClass,
               SUMMONER_LAB_NAME " Controller", 0, "", "1.0.0", kVstVersionString, (createController<0>))
#else
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, 0)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               SUMMONER_LAB_NAME, Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0", kVstVersionString,
               (createProcessor<false, false, 0, 0>))
    DEF_CLASS2(INLINE_UID_FROM_FUID(kControllerId), PClassInfo::kManyInstances, kVstComponentControllerClass,
               SUMMONER_LAB_NAME " Controller", 0, "", "1.0.0", kVstVersionString, (createController<0>))
#if SUMMONER_LAB_KIND == 3
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, 1)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               "Summoner Lab Hostile (crash in constructor)", Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0",
               kVstVersionString, (createProcessor<true, false, 0, 0>))
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, 2)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               "Summoner Lab Hostile (crash in prepare)", Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0", kVstVersionString,
               (createProcessor<false, true, 0, 0>))
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, 3)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               "Summoner Lab Hostile (3 s prepare)", Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0", kVstVersionString,
               (createProcessor<false, false, 3000, 0>))
    DEF_CLASS2(INLINE_UID_FROM_FUID(SUMMONER_LAB_FUID(0x50, 77)), PClassInfo::kManyInstances, kVstAudioEffectClass,
               "Summoner Lab Hostile (77 parameters)", Vst::kDistributable, SUMMONER_LAB_SUB, "1.0.0", kVstVersionString,
               (createProcessor<false, false, 0, 77>))
    DEF_CLASS2(INLINE_UID_FROM_FUID(kLiarControllerId), PClassInfo::kManyInstances, kVstComponentControllerClass,
               "Summoner Lab Hostile (77 parameters) Controller", 0, "", "1.0.0", kVstVersionString,
               (createController<77>))
#endif
#endif  // SUMMONER_LAB_FIXTURE
END_FACTORY
