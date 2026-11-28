// The lab's test plugins as AU v2: a thin shell over the DSP cores (testkit/plugins/dsp), built by this repo's own
// CMake from the pinned AudioUnitSDK's include/ and src/ by path (VENDORED.md; nothing of the SDK's own build is run).
// One bundle per core, chosen at compile time by SUMMONER_LAB_KIND (1 gain, 2 synth, 3 hostile). Never installed: the hosts
// register them in-process from target/plugins/au by absolute path (a lab build never asks the system AU registry).
//
// Lab conventions (test plugins, not products):
// - parameter ids = the DSP core's indices, range 0..1, so the host's normalized value IS the DSP core's raw value;
// - MIDI reaches the synth through MusicDeviceMIDIEvent, bytes as given (no mapping: AU has none);
// - state: AUBase's ClassInfo dictionary plus the DSP core's bytes under "summoner-lab-state" (authoritative on
//   restore);
// - latency: the DSP core's frames / the sample rate, announced with PropertyChanged(kAudioUnitProperty_Latency);
// - the hostile bundle's construction variants are AudioComponents entries of their own (Info.plist), and it has
//   AU-only switches after the DSP core's parameters: a render error status, the three input-pull misbehaviours
//   (each reporting what it saw through a read-only parameter), and SWAP_OUT; and AU-only variants: one that refuses
//   property listeners, and two that load a lab dylib from outside every bundle, in the constructor or in Initialize
//   (the post-load image check's STOP fixtures).
#include "summoner_lab_dsp.h"
#if defined(SUMMONER_LAB_FIXTURE_COCOAUI)
extern "C" unsigned sasLabViewNotices(void);  // cocoa_view.mm
#endif

#include <AudioUnitSDK/AUEffectBase.h>
#include <AudioUnitSDK/MusicDeviceBase.h>

#include <dlfcn.h>

#include <string>

#include <atomic>
#include <chrono>
#include <cmath>
#include <cstring>
#include <memory>
#include <thread>
#include <vector>

#ifndef SUMMONER_LAB_KIND
#error SUMMONER_LAB_KIND must be 1 (gain), 2 (synth) or 3 (hostile)
#endif

namespace sas::lab::au {

constexpr int kKind = SUMMONER_LAB_KIND;
constexpr UInt32 kMaxChannels = 8;
constexpr UInt32 kMaxMidi = 1024;
const CFStringRef kStateKey = CFSTR("summoner-lab-state");

// The hostile AU's own switches, after the DSP core's parameters (ids = indices, as the DSP core's).
namespace hx {
constexpr UInt32 RenderError = hostile::Count;  // >= 0.5: Render returns kRenderErrorStatus
constexpr UInt32 PullOutside = RenderError + 1;  // >= 0.5: a thread of the AU's own pulls input between renders
constexpr UInt32 PullTwice = PullOutside + 1;    // >= 0.5: every render pulls a second time and compares
constexpr UInt32 PullWrongN = PullTwice + 1;     // >= 0.5: every render also pulls with one frame fewer
constexpr UInt32 SwapOut = PullWrongN + 1;        // >= 0.5: after each render the caller's list points at our buffer
// Read-only: what the misbehaviours saw.
constexpr UInt32 OutsideStatus = SwapOut + 1;  // the OSStatus the last outside pull got (0 before any)
constexpr UInt32 OutsideSawData = OutsideStatus + 1;  // 1 if an outside pull ever changed its sentinel buffer
constexpr UInt32 TwiceEqual = OutsideSawData + 1;     // 1 while every second pull matched the first, -1 on a mismatch
constexpr UInt32 WrongNStatus = TwiceEqual + 1;       // the OSStatus the last short pull got
constexpr UInt32 End = WrongNStatus + 1;
}  // namespace hx
constexpr OSStatus kRenderErrorStatus = -10875;  // kAudioUnitErr_FailedInitialization's neighbour: a lab marker
constexpr float kSentinel = -12345.0f;
constexpr float kSwapValue = 0.25f;  // what SWAP_OUT's own buffer holds: a host that follows the pointer plays it

// PARAMLIST / HOSTCB: two fixtures, a bundle each, both on the gain core (kind 1).
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
// PARAMLIST: for its first kParamListAt rendered blocks its ParameterList is the DSP core's ids [0, N); at that block
// it gains one more parameter, id N ("Late parameter"), INSERTED at index 0 ([N, 0, 1, ..., N-1]), and announces
// PropertyChanged(kAudioUnitProperty_ParameterList). Its right channel is id N's value, every frame, so the audio shows
// whether a host's region index ever drives id N (a host that re-indexes after the notice does; one that keeps the
// indices it loaded with never does). From the same block, id 0's ParameterInfo name is its core name + " (renamed)"
// (a host may answer the notice with a name / label re-read of the ids it already has).
constexpr UInt32 kParamListAt = 8;  // the block (counted from Initialize) at which the list changes: a block count
#endif
#if defined(SUMMONER_LAB_FIXTURE_HOSTCB)
// HOSTCB: every render asks the host (kAudioUnitProperty_HostCallbacks) and writes what it got, replacing the audio:
//   left, every frame: the beat (CallHostBeatAndTempo);
//   right[0] tempo, [1] a flags word (bit 0 BeatAndTempo, bit 1 MusicalTimeLocation, bit 2 TransportState: each set
//   when that call returned noErr), [2] isPlaying, [3] currentSampleInTimeLine, [4] the time signature's numerator,
//   [5] its denominator, [6] currentMeasureDownBeat, [7] deltaSampleOffsetToNextBeat; the rest 0.
// A value whose call the host did not answer (the SDK's -1: no callback supplied) is written -1 (kNoHost), so a host
// that sets no HostCallbacks gives left all -1, flags 0, and -1 in [0] and [2..7].
constexpr float kNoHost = -1.0f;
#endif

// AU-only construction quirks of the hostile bundle's variants.
enum class Quirk { None, RefuseListeners, DlopenInConstructor, DlopenInInitialize };

#ifdef SUMMONER_LAB_DRAG_NAME
// A lab dylib outside every bundle, loaded at run time: the post-load image checks must catch it. Found beside this
// bundle's format dir (<base>/au/X.component/Contents/MacOS/X → <base>/lib: the build's target.noindex/plugins/lib, a
// pin's <sha8>/lib), never by a compiled-in path, so a pinned copy loads the pin's dylib.
void dragIn() {
    Dl_info info{};
    if (dladdr(reinterpret_cast<const void*>(&dragIn), &info) == 0 || info.dli_fname == nullptr)
        return;
    std::string p(info.dli_fname);  // <base>/au/X.component/Contents/MacOS/X
    for (int up = 0; up < 5; ++up)
        p = p.substr(0, p.rfind('/'));
    p += "/lib/" SUMMONER_LAB_DRAG_NAME;
    (void)dlopen(p.c_str(), RTLD_NOW | RTLD_LOCAL);
}
#else
void dragIn() {}
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

#if defined(SUMMONER_LAB_FIXTURE_PARAMS)
// PARAMS: after the gain (id 0), ids 1.. that exercise the host's AU parameter
// derivations: two clumps (5 and 9) whose members are not adjacent in the list, a NonRealTime parameter (not
// automatable), an indexed one (4 states), Hz / % / a custom unit ("drv"), a duplicate name and an empty one.
struct LabParam {
    const char* name;
    AudioUnitParameterUnit unit;
    float min, max, def;
    UInt32 clump;  // 0: none
    bool nonRealTime;
};
const LabParam kLabParams[] = {
    {"Cutoff", kAudioUnitParameterUnit_Hertz, 20.0f, 20000.0f, 1000.0f, 5, false},
    {"Mode", kAudioUnitParameterUnit_Indexed, 0.0f, 3.0f, 1.0f, 0, false},
    {"Offline", kAudioUnitParameterUnit_Generic, 0.0f, 1.0f, 0.0f, 0, true},
    {"Cutoff", kAudioUnitParameterUnit_Hertz, 20.0f, 20000.0f, 1000.0f, 5, false},
    {"", kAudioUnitParameterUnit_Generic, 0.0f, 1.0f, 0.5f, 0, false},
    {"Drive", kAudioUnitParameterUnit_CustomUnit, 0.0f, 10.0f, 2.5f, 9, false},
    {"Res", kAudioUnitParameterUnit_Percent, 0.0f, 100.0f, 25.0f, 9, false},
};
constexpr UInt32 kLabParamCount = sizeof(kLabParams) / sizeof(kLabParams[0]);
#endif

bool isAuOnlyParam(UInt32 id) { return kKind == 3 && id >= hx::RenderError && id < hx::End; }

// A buffer list of up to kMaxChannels mono buffers, allocated once.
struct OwnedList {
    std::vector<float> data;
    std::vector<std::byte> raw;
    AudioBufferList* list = nullptr;
    UInt32 channels = 0, frames = 0;

    void allocate(UInt32 ch, UInt32 n) {
        channels = ch;
        frames = n;
        data.assign(size_t(ch) * n, 0.0f);
        raw.assign(offsetof(AudioBufferList, mBuffers) + sizeof(AudioBuffer) * std::max<UInt32>(ch, 1), std::byte{});
        list = reinterpret_cast<AudioBufferList*>(raw.data());
    }
    // Points the list at its own storage for `n` frames (n <= frames).
    AudioBufferList& prepare(UInt32 n) {
        list->mNumberBuffers = channels;
        for (UInt32 c = 0; c < channels; ++c) {
            list->mBuffers[c].mNumberChannels = 1;
            list->mBuffers[c].mDataByteSize = n * sizeof(float);
            list->mBuffers[c].mData = data.data() + size_t(c) * frames;
        }
        return *list;
    }
    void fill(float v) { std::fill(data.begin(), data.end(), v); }
};

// What both AU shells share: the DSP core, its parameters mirrored into the AU's global element, state, latency.
template <class Base>
class Shell : public Base {
public:
    template <class... A>
    Shell(const HostileConfig& cfg, AudioComponentInstance inst, A... a) : Base(inst, a...), cfg_(cfg) {
        core_ = makeCore(cfg);
        this->CreateElements();
        const UInt32 n = paramTotal();
        this->Globals()->UseIndexedParameters(n);
        for (UInt32 i = 0; i < core_->paramCount(); ++i)
            this->Globals()->SetParameter(i, core_->param(i));
        for (UInt32 i = core_->paramCount(); i < n; ++i)
#if defined(SUMMONER_LAB_FIXTURE_PARAMS)
            this->Globals()->SetParameter(i, kLabParams[i - core_->paramCount()].def);
#else
            this->Globals()->SetParameter(i, 0.0f);
#endif
    }

    ~Shell() override { stopOutside(); }

    OSStatus Initialize() override {
        const OSStatus r = Base::Initialize();
        if (r != noErr)
            return r;
        const UInt32 maxFrames = this->GetMaxFramesPerSlice();
        core_->prepare(this->Output(0).GetStreamFormat().mSampleRate, std::max<UInt32>(maxFrames, 1));
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
        blocks_ = 0;
        listChanged_ = false;
#endif
        midi_.reserve(kMaxMidi);
        if (kKind == 3) {
            const UInt32 ch = std::min(this->Output(0).GetStreamFormat().mChannelsPerFrame, kMaxChannels);
            first_.allocate(ch, maxFrames);
            second_.allocate(ch, maxFrames);
            outside_.allocate(ch, maxFrames);
            startOutside();
        }
        return noErr;
    }

    void Cleanup() override {
        stopOutside();
        Base::Cleanup();
    }

    OSStatus Reset(AudioUnitScope scope, AudioUnitElement element) override {
        if (scope == kAudioUnitScope_Global)
            core_->reset();
        return Base::Reset(scope, element);
    }

    OSStatus GetParameterList(AudioUnitScope scope, AudioUnitParameterID* out, UInt32& count) override {
        if (scope != kAudioUnitScope_Global) {
            count = 0;
            return noErr;
        }
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
        {
            // PARAMLIST: [0, N) before the change, [N, 0, ..., N-1] after it.
            const UInt32 n = core_->paramCount();
            const bool late = listChanged_.load();
            count = late ? n + 1 : n;
            if (out != nullptr)
                for (UInt32 i = 0; i < count; ++i)
                    out[i] = late ? (i == 0 ? n : i - 1) : i;
            return noErr;
        }
#endif
        if (cfg_.lieParamCount != 0) {
            // The liar: claims lieParamCount ids: the first kLiarReal real, then ghosts that
            // repeat id 0 (the gain's) up to kLiarGhosts, then ids whose info fails.
            count = cfg_.lieParamCount;
            if (out != nullptr)
                for (UInt32 i = 0; i < count; ++i)
                    out[i] = i < kLiarReal ? i : i < kLiarGhosts ? 0 : 1000 + i;
            return noErr;
        }
        count = paramTotal();
        if (out != nullptr)
            for (UInt32 i = 0; i < count; ++i)
                out[i] = i;
        return noErr;
    }

    OSStatus GetParameterInfo(AudioUnitScope scope, AudioUnitParameterID id, AudioUnitParameterInfo& info) override {
        if (scope != kAudioUnitScope_Global || id >= paramTotal())
            return kAudioUnitErr_InvalidParameter;
        if (cfg_.lieParamCount != 0 && id >= kLiarReal)
            return kAudioUnitErr_InvalidParameter;
        const char* name = nullptr;
        bool readOnly = false;
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
        std::string renamed;
#endif
#if defined(SUMMONER_LAB_FIXTURE_PARAMS)
        if (id >= core_->paramCount()) {
            const LabParam& lp = kLabParams[id - core_->paramCount()];
            info.flags = kAudioUnitParameterFlag_IsReadable | kAudioUnitParameterFlag_IsWritable |
                         (lp.nonRealTime ? kAudioUnitParameterFlag_NonRealTime : 0);
            if (lp.clump != 0) {
                info.flags |= kAudioUnitParameterFlag_HasClump;
                info.clumpID = lp.clump;
            }
#if defined(SUMMONER_LAB_FIXTURE_PARAMSFIXED)
            // PARAMSFIXED (clean-room pins): no CF strings at all. The name goes in the fixed `name` array (no
            // HasCFNameString, no CFNameRelease): "Res" as the bytes 'R' 0xFF 's' (invalid UTF-8), "Drive" as 52
            // 'D' bytes with no NUL; the custom unit's name is a constant CFString the AU keeps.
            std::memset(info.name, 0, sizeof(info.name));
            if (std::strcmp(lp.name, "Res") == 0) {
                info.name[0] = 'R';
                info.name[1] = char(0xFF);
                info.name[2] = 's';
            } else if (std::strcmp(lp.name, "Drive") == 0) {
                std::memset(info.name, 'D', sizeof(info.name));
            } else {
                std::strncpy(info.name, lp.name, sizeof(info.name) - 1);
            }
            info.unit = lp.unit;
            if (lp.unit == kAudioUnitParameterUnit_CustomUnit)
                info.unitName = CFSTR("drv");
#else
            CFStringRef pcf = CFStringCreateWithCString(nullptr, lp.name, kCFStringEncodingUTF8);
            ausdk::AUBase::FillInParameterName(info, pcf, true);
            info.unit = lp.unit;
            if (lp.unit == kAudioUnitParameterUnit_CustomUnit)
                info.unitName = CFStringCreateWithCString(nullptr, "drv", kCFStringEncodingUTF8);  // +1: released by
                                                                                                  // the host (CFNameRelease)
#endif
            info.minValue = lp.min;
            info.maxValue = lp.max;
            info.defaultValue = lp.def;
            return noErr;
        }
#endif
        if (id < core_->paramCount()) {
            name = core_->paramInfo(id).name;
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
            if (id == 0 && listChanged_.load()) {
                renamed = std::string(name) + " (renamed)";
                name = renamed.c_str();
            }
        } else if (true) {
            name = "Late parameter";  // PARAMLIST's id N
#endif
        } else {
            static const char* names[] = {"Render error", "Pull outside", "Pull twice", "Pull wrong n", "Swap out",
                                          "Outside status", "Outside saw data", "Twice equal", "Wrong n status"};
            name = names[id - hx::RenderError];
            readOnly = id >= hx::OutsideStatus;
        }
        info.flags = kAudioUnitParameterFlag_IsReadable | (readOnly ? 0 : kAudioUnitParameterFlag_IsWritable);
        CFStringRef cf = CFStringCreateWithCString(nullptr, name, kCFStringEncodingUTF8);
        ausdk::AUBase::FillInParameterName(info, cf, true);
        info.unit = kAudioUnitParameterUnit_Generic;
        info.minValue = readOnly ? -1.0e9f : 0.0f;
        info.maxValue = readOnly ? 1.0e9f : 1.0f;
        info.defaultValue = id < core_->paramCount() ? core_->paramInfo(id).defaultValue : 0.0f;
        return noErr;
    }

    OSStatus SetParameter(AudioUnitParameterID id, AudioUnitScope scope, AudioUnitElement element,
                          AudioUnitParameterValue value, UInt32 offset) override {
        if (scope == kAudioUnitScope_Global && id >= hx::OutsideStatus && isAuOnlyParam(id))
            return kAudioUnitErr_InvalidPropertyValue;  // read-only
        const OSStatus r = Base::SetParameter(id, scope, element, value, offset);
        if (r != noErr || scope != kAudioUnitScope_Global)
            return r;
        if (id < core_->paramCount()) {
            const UInt32 before = core_->latency();
            core_->setParam(id, value);
            if (core_->latency() != before)
                this->PropertyChanged(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
        }
        return noErr;
    }

    Float64 GetLatency() override {
        const Float64 rate = this->Output(0).GetStreamFormat().mSampleRate;
        return rate > 0 ? Float64(core_->latency()) / rate : 0.0;
    }

    OSStatus SaveState(CFPropertyListRef* out) override {
        const OSStatus r = Base::SaveState(out);
        if (r != noErr)
            return r;
        const std::vector<uint8_t> bytes = core_->getState();
        CFDataRef data = CFDataCreate(nullptr, bytes.data(), CFIndex(bytes.size()));
        CFDictionarySetValue((CFMutableDictionaryRef)*out, kStateKey, data);
        CFRelease(data);
        return noErr;
    }

    OSStatus RestoreState(CFPropertyListRef plist) override {
        const OSStatus r = Base::RestoreState(plist);
        if (r != noErr)
            return r;
        const auto data = (CFDataRef)CFDictionaryGetValue((CFDictionaryRef)plist, kStateKey);
        if (data == nullptr || CFGetTypeID(data) != CFDataGetTypeID())
            return kAudioUnitErr_InvalidPropertyValue;
        const UInt8* p = CFDataGetBytePtr(data);
        const size_t n = size_t(CFDataGetLength(data));
        const UInt32 before = core_->latency();
        if (kKind == 3 && n > 6 && std::memcmp(p, "LATER=", 6) == 0) {
            core_->setState(p, n);  // the DSP core moves its latency <ms> later, on a thread of its own
            uint64_t ms = 0;
            for (size_t i = 6; i < n && p[i] >= '0' && p[i] <= '9' && ms < 60000; ++i)
                ms = ms * 10 + uint64_t(p[i] - '0');
            // ... and the AU tells its host 20 ms after that, from a thread of its own (the instance must outlive it).
            std::thread([this, ms] {
                std::this_thread::sleep_for(std::chrono::milliseconds(ms + 20));
                this->PropertyChanged(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
            }).detach();
            return noErr;
        }
        if (!core_->setState(p, n))
            return kAudioUnitErr_InvalidPropertyValue;
        for (UInt32 i = 0; i < core_->paramCount(); ++i)
            this->Globals()->SetParameter(i, core_->param(i));
        if (core_->latency() != before)
            this->PropertyChanged(kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0);
        return noErr;
    }

    bool CanScheduleParameters() const override { return false; }

protected:
    static constexpr UInt32 kLiarReal = 5;
    static constexpr UInt32 kLiarGhosts = 10;

#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
    UInt32 paramTotal() const { return core_->paramCount() + 1; }  // + PARAMLIST's id N
#elif defined(SUMMONER_LAB_FIXTURE_PARAMS)
    UInt32 paramTotal() const { return core_->paramCount() + kLabParamCount; }  // + PARAMS's ids
#else
    UInt32 paramTotal() const { return core_->paramCount() + (kKind == 3 ? hx::End - hx::RenderError : 0); }
#endif
    bool on(UInt32 id) { return kKind == 3 && this->Globals()->GetParameter(id) >= 0.5f; }
    void report(UInt32 id, float v) { this->Globals()->SetParameter(id, v); }

    // Runs the DSP core over `channels` buffers of `frames`, with the MIDI collected since the last render.
    void runCore(float* const* channels, UInt32 numChannels, UInt32 frames) {
        for (size_t i = 1; i < midi_.size(); ++i)
            for (size_t j = i; j > 0 && midi_[j - 1].offset > midi_[j].offset; --j)
                std::swap(midi_[j - 1], midi_[j]);
        for (auto& m : midi_)
            m.offset = std::min(m.offset, frames > 0 ? frames - 1 : 0);
        Block b;
        b.channels = channels;
        b.numChannels = numChannels;
        b.frames = frames;
        b.midi = midi_.data();
        b.midiCount = UInt32(midi_.size());
        core_->process(b);  // the hostile core may abort, fault, wedge or throw here (a throw: std::terminate)
        midi_.clear();
    }

    // The input-pull misbehaviours PULL_TWICE / PULL_WRONG_N, inside a render that already pulled into `first`.
    void hostilePulls(AudioUnitRenderActionFlags& flags, const AudioTimeStamp& ts, const AudioBufferList& in,
                      UInt32 frames) {
        if (kKind != 3)
            return;
        if (on(hx::PullTwice)) {
            AudioBufferList& again = second_.prepare(frames);
            OSStatus r = this->Input(0).PullInputWithBufferList(flags, ts, 0, frames, again);
            bool same = r == noErr && again.mNumberBuffers <= in.mNumberBuffers;
            for (UInt32 c = 0; same && c < again.mNumberBuffers; ++c)
                same = std::memcmp(again.mBuffers[c].mData, in.mBuffers[c].mData, frames * sizeof(float)) == 0;
            if (!same)
                twiceMismatch_ = true;
            report(hx::TwiceEqual, twiceMismatch_ ? -1.0f : 1.0f);
        }
        if (on(hx::PullWrongN) && frames > 1) {
            AudioBufferList& shorter = second_.prepare(frames - 1);
            report(hx::WrongNStatus,
                   float(this->Input(0).PullInputWithBufferList(flags, ts, 0, frames - 1, shorter)));
        }
    }

    HostileConfig cfg_;
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
    std::atomic<bool> listChanged_{false};  // PARAMLIST: the list has gained id N
    UInt32 blocks_ = 0;                     // PARAMLIST: rendered blocks since Initialize (the render thread's)
#endif
    std::unique_ptr<Processor> core_;
    std::vector<MidiEvent> midi_;
    std::atomic<bool> inRender_{false};

private:
    // PULL_OUTSIDE: a thread of the AU's own that pulls input while no render is running.
    void startOutside() {
        stopOutside();
        stop_ = false;
        outsideThread_ = std::thread([this] {
            while (!stop_.load()) {
                std::this_thread::sleep_for(std::chrono::milliseconds(5));
                if (!on(hx::PullOutside) || inRender_.load() || lastFrames_.load() == 0)
                    continue;
                const UInt32 n = lastFrames_.load();
                outside_.fill(kSentinel);
                AudioBufferList& l = outside_.prepare(n);
                AudioUnitRenderActionFlags f = 0;
                AudioTimeStamp ts{};
                ts.mFlags = kAudioTimeStampSampleTimeValid;
                const OSStatus r = this->Input(0).PullInputWithBufferList(f, ts, 0, n, l);
                bool changed = false;
                for (float v : outside_.data)
                    changed = changed || v != kSentinel;
                report(hx::OutsideStatus, float(r));
                if (changed)
                    report(hx::OutsideSawData, 1.0f);
            }
        });
    }
    void stopOutside() {
        stop_ = true;
        if (outsideThread_.joinable())
            outsideThread_.join();
    }

protected:
    OwnedList first_, second_, outside_;
    std::atomic<UInt32> lastFrames_{0};
    bool twiceMismatch_ = false;

private:
    std::atomic<bool> stop_{true};
    std::thread outsideThread_;
};

// --- the effect (gain, hostile) ------------------------------------------------------------------------------------

class LabEffect : public Shell<ausdk::AUEffectBase> {
public:
    LabEffect(AudioComponentInstance inst, const HostileConfig& cfg, Quirk quirk = Quirk::None)
        : Shell(cfg, inst, true), quirk_(quirk) {
        if (cfg.crashInConstructor)
            labAbort();
        if (quirk_ == Quirk::DlopenInConstructor)
            dragIn();
    }

    OSStatus Initialize() override {
        if (cfg_.crashInPrepare)
            labAbort();
        if (cfg_.prepareSleepMs != 0)
            std::this_thread::sleep_for(std::chrono::milliseconds(cfg_.prepareSleepMs));
        if (quirk_ == Quirk::DlopenInInitialize)
            dragIn();
        const OSStatus r = Shell::Initialize();
        if (r == noErr && kKind == 3) {
            swap_.allocate(std::min(Output(0).GetStreamFormat().mChannelsPerFrame, kMaxChannels), GetMaxFramesPerSlice());
            swap_.fill(kSwapValue);
            SetRenderNotification(swapNotify, this);
        }
        return r;
    }

    void Cleanup() override {
        if (kKind == 3)
            RemoveRenderNotification(swapNotify, this);
        Shell::Cleanup();
    }

#if defined(SUMMONER_LAB_FIXTURE_COCOAUI)
    // kAudioUnitProperty_CocoaUI names this bundle (found from this executable, dladdr: no lookup by
    // identifier) and its view factory class, SummonerLabCocoaViewFactory (cocoa_view.mm).
    OSStatus GetPropertyInfo(AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el, UInt32& size,
                             bool& writable) override {
        if (id == kAudioUnitProperty_CocoaUI && scope == kAudioUnitScope_Global) {
            size = sizeof(AudioUnitCocoaViewInfo);
            writable = false;
            return noErr;
        }
        return Shell::GetPropertyInfo(id, scope, el, size, writable);
    }

    OSStatus GetProperty(AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el, void* data) override {
        if (id == kAudioUnitProperty_CocoaUI && scope == kAudioUnitScope_Global) {
            Dl_info dl{};
            if (dladdr(reinterpret_cast<const void*>(&cocoaAnchor), &dl) == 0 || dl.dli_fname == nullptr)
                return kAudioUnitErr_InvalidProperty;
            // <bundle>/Contents/MacOS/<exe>: three levels up.
            std::string path = dl.dli_fname;
            for (int i = 0; i < 3; ++i)
                path = path.substr(0, path.rfind('/'));
            CFURLRef url = CFURLCreateFromFileSystemRepresentation(
                nullptr, reinterpret_cast<const UInt8*>(path.c_str()), CFIndex(path.size()), true);
            if (url == nullptr)
                return kAudioUnitErr_InvalidProperty;
            auto* info = static_cast<AudioUnitCocoaViewInfo*>(data);
            info->mCocoaAUViewBundleLocation = url;  // the caller releases both (the property's contract)
            info->mCocoaAUViewClass[0] = CFStringCreateCopy(nullptr, CFSTR("SummonerLabCocoaViewFactory"));
            return noErr;
        }
        return Shell::GetProperty(id, scope, el, data);
    }
    static void cocoaAnchor() {}
#endif

#if defined(SUMMONER_LAB_FIXTURE_OFFLINE)
    // An AU that supports and stores kAudioUnitProperty_OfflineRender (the AudioUnitSDK's AUBase does not), so a host
    // that sets it is seen: the child must never set it.
    OSStatus GetPropertyInfo(AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el, UInt32& size,
                             bool& writable) override {
        if (id == kAudioUnitProperty_OfflineRender && scope == kAudioUnitScope_Global) {
            size = sizeof(UInt32);
            writable = true;
            return noErr;
        }
        return Shell::GetPropertyInfo(id, scope, el, size, writable);
    }

    OSStatus GetProperty(AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el, void* data) override {
        if (id == kAudioUnitProperty_OfflineRender && scope == kAudioUnitScope_Global) {
            *static_cast<UInt32*>(data) = offline_;
            return noErr;
        }
        return Shell::GetProperty(id, scope, el, data);
    }

    OSStatus SetProperty(AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el, const void* data,
                         UInt32 size) override {
        if (id == kAudioUnitProperty_OfflineRender && scope == kAudioUnitScope_Global) {
            if (size != sizeof(UInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            offline_ = *static_cast<const UInt32*>(data);
            return noErr;
        }
        return Shell::SetProperty(id, scope, el, data, size);
    }
    UInt32 offline_ = 0;
#endif

    OSStatus AddPropertyListener(AudioUnitPropertyID id, AudioUnitPropertyListenerProc proc, void* refcon) override {
        if (quirk_ == Quirk::RefuseListeners)
            return kAudioUnitErr_InvalidProperty;
        return Shell::AddPropertyListener(id, proc, refcon);
    }

    UInt32 SupportedNumChannels(const AUChannelInfo** out) override {
        static const AUChannelInfo info[] = {{-1, -1}};  // any N to the same N
        if (out != nullptr)
            *out = info;
        return 1;
    }

    OSStatus Render(AudioUnitRenderActionFlags& flags, const AudioTimeStamp& ts, UInt32 frames) override {
        inRender_ = true;
        if (on(hx::RenderError)) {
            inRender_ = false;
            return kRenderErrorStatus;
        }
        ts_ = &ts;
        const OSStatus r = AUEffectBase::Render(flags, ts, frames);
        ts_ = nullptr;
        lastFrames_ = frames;
        inRender_ = false;
        return r;
    }

    OSStatus ProcessBufferLists(AudioUnitRenderActionFlags& flags, const AudioBufferList& in, AudioBufferList& out,
                                UInt32 frames) override {
        if (ts_ != nullptr)
            hostilePulls(flags, *ts_, in, frames);
        const UInt32 n = std::min(out.mNumberBuffers, kMaxChannels);
        float* ch[kMaxChannels] = {};
        for (UInt32 c = 0; c < n; ++c) {
            ch[c] = static_cast<float*>(out.mBuffers[c].mData);
            const float* x = c < in.mNumberBuffers ? static_cast<const float*>(in.mBuffers[c].mData) : nullptr;
            if (x == nullptr)
                std::memset(ch[c], 0, frames * sizeof(float));
            else if (x != ch[c])
                std::memcpy(ch[c], x, frames * sizeof(float));
        }
        runCore(ch, n, frames);
#if defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
        if (++blocks_ == kParamListAt && !listChanged_.exchange(true))
            // From the render thread, at a block count (deterministic): AUBase only walks its listener list.
            PropertyChanged(kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0);
        if (n >= 2)
            std::fill(ch[1], ch[1] + frames, Globals()->GetParameter(core_->paramCount()));
#endif
#if defined(SUMMONER_LAB_FIXTURE_HOSTCB)
        reportHost(ch, n, frames);
#endif
#if defined(SUMMONER_LAB_FIXTURE_COCOAUI)
        // The host's parameter notifications its view received, in the right channel's first frame.
        if (n >= 2 && frames > 0)
            ch[1][0] = float(sasLabViewNotices());
#endif
        flags &= ~kAudioUnitRenderAction_OutputIsSilence;
        return noErr;
    }

    bool SupportsTail() override { return true; }

private:
#if defined(SUMMONER_LAB_FIXTURE_HOSTCB)
    // HOSTCB: what the host's callbacks answer, into the audio (the layout above, at SUMMONER_LAB_FIXTURE_HOSTCB).
    void reportHost(float* const* ch, UInt32 n, UInt32 frames) {
        Float64 beat = kNoHost, tempo = kNoHost, sample = kNoHost, downBeat = kNoHost, c0 = 0, c1 = 0;
        UInt32 toNext = 0, den = 0;
        Float32 num = kNoHost;
        Boolean playing = false, changed = false, cycling = false;
        UInt32 flagsWord = 0;
        if (CallHostBeatAndTempo(&beat, &tempo) == noErr)
            flagsWord |= 1;
        else
            beat = tempo = kNoHost;
        const bool mt = CallHostMusicalTimeLocation(&toNext, &num, &den, &downBeat) == noErr;
        if (mt)
            flagsWord |= 2;
        const bool tr = CallHostTransportState(&playing, &changed, &sample, &cycling, &c0, &c1) == noErr;
        if (tr)
            flagsWord |= 4;
        if (n >= 1)
            std::fill(ch[0], ch[0] + frames, float(beat));
        if (n >= 2 && frames >= 8) {
            float* r = ch[1];
            std::fill(r, r + frames, 0.0f);
            r[0] = float(tempo);
            r[1] = float(flagsWord);
            r[2] = tr ? (playing ? 1.0f : 0.0f) : kNoHost;
            r[3] = tr ? float(sample) : kNoHost;
            r[4] = mt ? num : kNoHost;
            r[5] = mt ? float(den) : kNoHost;
            r[6] = mt ? float(downBeat) : kNoHost;
            r[7] = mt ? float(toNext) : kNoHost;
        }
    }
#endif

    // SWAP_OUT: after the SDK has copied the rendered audio into the caller's buffers, point the caller's
    // list at a buffer of our own, as a misbehaving AU may.
    static OSStatus swapNotify(void* self, AudioUnitRenderActionFlags* flags, const AudioTimeStamp*, UInt32,
                               UInt32 frames, AudioBufferList* io) {
        auto* me = static_cast<LabEffect*>(self);
        if (io == nullptr || (*flags & kAudioUnitRenderAction_PostRender) == 0 || !me->on(hx::SwapOut))
            return noErr;
        AudioBufferList& own = me->swap_.prepare(std::min(frames, me->swap_.frames));
        for (UInt32 c = 0; c < io->mNumberBuffers && c < own.mNumberBuffers; ++c)
            io->mBuffers[c].mData = own.mBuffers[c].mData;
        return noErr;
    }

    const AudioTimeStamp* ts_ = nullptr;
    Quirk quirk_;
    OwnedList swap_;
};

// --- the synth -----------------------------------------------------------------------------------------------------

// synthin = the synth with one input element (an aumu that takes audio, as some vocoders and samplers do):
// what it pulls is added to its output, so a host that feeds it shows the input in the render.
// synthinstrict = the same synth whose input element refuses a stereo stream format (a strict instrument AU with an
// input element): a host that insists on stereo input fails its prepare.
#if defined(SUMMONER_LAB_FIXTURE_SYNTHIN) || defined(SUMMONER_LAB_FIXTURE_SYNTHINSTRICT) || defined(SUMMONER_LAB_FIXTURE_SYNTHINCHANGE)
constexpr UInt32 kSynthInputs = 1;
#else
constexpr UInt32 kSynthInputs = 0;
#endif

class LabSynth final : public Shell<ausdk::MusicDeviceBase> {
public:
    explicit LabSynth(AudioComponentInstance inst) : Shell(HostileConfig{}, inst, kSynthInputs, UInt32(1)) {}

    bool StreamFormatWritable(AudioUnitScope, AudioUnitElement) override { return !this->IsInitialized(); }

#if defined(SUMMONER_LAB_FIXTURE_SYNTHINSTRICT) || defined(SUMMONER_LAB_FIXTURE_SYNTHINCHANGE)
    bool ValidFormat(AudioUnitScope scope, AudioUnitElement element,
                     const AudioStreamBasicDescription& format) override {
        if (scope == kAudioUnitScope_Input && format.mChannelsPerFrame == 2) {
#if defined(SUMMONER_LAB_FIXTURE_SYNTHINCHANGE)
            if (this->Globals()->GetParameter(0) >= 0.5f) return false;
#else
            return false;
#endif
        }
        return MusicDeviceBase::ValidFormat(scope, element, format);
    }
#endif

    bool CanScheduleParameters() const override { return false; }

    OSStatus HandleMIDIEvent(UInt8 status, UInt8 channel, UInt8 d1, UInt8 d2, UInt32 frame) override {
        if (midi_.size() == midi_.capacity())
            return noErr;  // never grow on the audio thread
        MidiEvent m;
        m.offset = frame;
        m.size = 3;
        m.data[0] = uint8_t(status | channel);
        m.data[1] = d1;
        m.data[2] = d2;
        midi_.push_back(m);
        return noErr;
    }

    OSStatus Render(AudioUnitRenderActionFlags& flags, const AudioTimeStamp& ts, UInt32 frames) override {
        inRender_ = true;
        AudioBufferList& out = this->Output(0).PrepareBuffer(frames);
        const UInt32 n = std::min(out.mNumberBuffers, kMaxChannels);
        float* ch[kMaxChannels] = {};
        for (UInt32 c = 0; c < n; ++c) {
            ch[c] = static_cast<float*>(out.mBuffers[c].mData);
            std::memset(ch[c], 0, frames * sizeof(float));
        }
        runCore(ch, n, frames);
        if constexpr (kSynthInputs > 0) {
            // A host that gave the input element no callback (or no format) is a pull error: the output stays the
            // synth's alone.
            if (this->Input(0).IsActive() && this->Input(0).PullInput(flags, ts, 0, frames) == noErr) {
                const AudioBufferList& in = this->Input(0).GetBufferList();
                for (UInt32 c = 0; c < std::min(n, in.mNumberBuffers); ++c)
                    if (const auto* x = static_cast<const float*>(in.mBuffers[c].mData))
                        for (UInt32 i = 0; i < frames; ++i)
                            ch[c][i] += x[i];
            }
        } else {
            (void)ts;
        }
        flags &= ~kAudioUnitRenderAction_OutputIsSilence;
        inRender_ = false;
        // The needs-the-note fixture's failed block: an error status, never an exception (the SDK's render is
        // noexcept: a throw would terminate the process).
        return core_->blockFailed() ? OSStatus(kAudioUnitErr_CannotDoInCurrentContext) : OSStatus(noErr);
    }

    bool SupportsTail() override { return true; }
};

// The effect with its construction behaviour fixed at compile time: AUBaseFactory constructs `T(instance)`.
template <bool CtorCrash, bool PrepareCrash, uint32_t PrepareMs, uint32_t Lie, Quirk Q = Quirk::None,
          bool CtorHang = false, bool BadState = false>
class EffectVariant final : public LabEffect {
public:
    explicit EffectVariant(AudioComponentInstance inst)
        : LabEffect(inst, HostileConfig{CtorCrash, PrepareCrash, PrepareMs, Lie, CtorHang, BadState}, Q) {}
};

}  // namespace sas::lab::au

// The factories, one per AudioComponents entry of the bundle's Info.plist (the only exported symbols).
#define SUMMONER_LAB_FACTORY(fn, Fac, T)                                                     \
    extern "C" __attribute__((visibility("default"))) void* fn(const AudioComponentDescription* d); \
    extern "C" void* fn(const AudioComponentDescription* d) { return ausdk::Fac<T>::Factory(d); }

namespace sas::lab::au {
using Plain = EffectVariant<false, false, 0, 0>;
using CtorCrash = EffectVariant<true, false, 0, 0>;
using PrepareCrash = EffectVariant<false, true, 0, 0>;
using SlowPrepare = EffectVariant<false, false, 3000, 0>;
using Liar = EffectVariant<false, false, 0, 77>;
using Deaf = EffectVariant<false, false, 0, 0, Quirk::RefuseListeners>;
using DragCtor = EffectVariant<false, false, 0, 0, Quirk::DlopenInConstructor>;
using DragInit = EffectVariant<false, false, 0, 0, Quirk::DlopenInInitialize>;
using Hang = EffectVariant<false, false, 0, 0, Quirk::None, true>;
using BadState = EffectVariant<false, false, 0, 0, Quirk::None, false, true>;
}  // namespace sas::lab::au

#if defined(SUMMONER_LAB_FIXTURE_HANG)
// The scan fixtures, a bundle each: the one component never returns from its constructor; or refuses its own state.
SUMMONER_LAB_FACTORY(SummonerLabHangFactory, AUBaseFactory, sas::lab::au::Hang)
#elif defined(SUMMONER_LAB_FIXTURE_BADSTATE)
SUMMONER_LAB_FACTORY(SummonerLabBadStateFactory, AUBaseFactory, sas::lab::au::BadState)
#elif defined(SUMMONER_LAB_FIXTURE_NEEDSNOTE)
SUMMONER_LAB_FACTORY(SummonerLabNeedsNoteFactory, AUMusicDeviceFactory, sas::lab::au::LabSynth)
#elif defined(SUMMONER_LAB_FIXTURE_SYNTHIN)
SUMMONER_LAB_FACTORY(SummonerLabSynthInFactory, AUMusicDeviceFactory, sas::lab::au::LabSynth)
#elif defined(SUMMONER_LAB_FIXTURE_SYNTHINCHANGE)
SUMMONER_LAB_FACTORY(SummonerLabSynthInChangeFactory, AUMusicDeviceFactory, sas::lab::au::LabSynth)
#elif defined(SUMMONER_LAB_FIXTURE_SYNTHINSTRICT)
SUMMONER_LAB_FACTORY(SummonerLabSynthInStrictFactory, AUMusicDeviceFactory, sas::lab::au::LabSynth)
#elif defined(SUMMONER_LAB_FIXTURE_PARAMLIST)
SUMMONER_LAB_FACTORY(SummonerLabParamListFactory, AUBaseFactory, sas::lab::au::Plain)
#elif defined(SUMMONER_LAB_FIXTURE_HOSTCB)
SUMMONER_LAB_FACTORY(SummonerLabHostCbFactory, AUBaseFactory, sas::lab::au::Plain)
#elif defined(SUMMONER_LAB_FIXTURE_COCOAUI)
SUMMONER_LAB_FACTORY(SummonerLabCocoaUiFactory, AUBaseFactory, sas::lab::au::Plain)
#elif defined(SUMMONER_LAB_FIXTURE_OFFLINE)
// The gain core storing kAudioUnitProperty_OfflineRender (above).
SUMMONER_LAB_FACTORY(SummonerLabOfflineFactory, AUBaseFactory, sas::lab::au::Plain)
#elif defined(SUMMONER_LAB_FIXTURE_MULTI)
// Two entries whose uniqueIds (the XOR) collide, told apart only by their triples; B is the 77-parameter
// liar, so a host that picks A for B shows it in its parameter count.
SUMMONER_LAB_FACTORY(SummonerLabMultiAFactory, AUBaseFactory, sas::lab::au::Plain)
SUMMONER_LAB_FACTORY(SummonerLabMultiBFactory, AUBaseFactory, sas::lab::au::Liar)
#elif defined(SUMMONER_LAB_FIXTURE_BADCODES)
// One factory behind several entries, all but the first with a code the child must refuse at the scan.
SUMMONER_LAB_FACTORY(SummonerLabBadCodesFactory, AUBaseFactory, sas::lab::au::Plain)
#elif defined(SUMMONER_LAB_FIXTURE_BADORDER)
// A code refusal first (never instantiated), then an entry that refuses its own state (a validation
// failure), then a good one; the refused and the good entries share the plain factory.
SUMMONER_LAB_FACTORY(SummonerLabBadOrderOkFactory, AUBaseFactory, sas::lab::au::Plain)
SUMMONER_LAB_FACTORY(SummonerLabBadOrderStateFactory, AUBaseFactory, sas::lab::au::BadState)
#elif defined(SUMMONER_LAB_FIXTURE_PARAMS)
// The gain core with the PARAMS parameter shapes (above).
SUMMONER_LAB_FACTORY(SummonerLabParamsFactory, AUBaseFactory, sas::lab::au::Plain)
#elif SUMMONER_LAB_KIND == 1
SUMMONER_LAB_FACTORY(SummonerLabGainFactory, AUBaseFactory, sas::lab::au::Plain)
#elif SUMMONER_LAB_KIND == 2
SUMMONER_LAB_FACTORY(SummonerLabSynthFactory, AUMusicDeviceFactory, sas::lab::au::LabSynth)
#else
SUMMONER_LAB_FACTORY(SummonerLabHostileFactory, AUBaseFactory, sas::lab::au::Plain)
SUMMONER_LAB_FACTORY(SummonerLabHostileCtorCrashFactory, AUBaseFactory, sas::lab::au::CtorCrash)
SUMMONER_LAB_FACTORY(SummonerLabHostilePrepareCrashFactory, AUBaseFactory, sas::lab::au::PrepareCrash)
SUMMONER_LAB_FACTORY(SummonerLabHostileSlowPrepareFactory, AUBaseFactory, sas::lab::au::SlowPrepare)
SUMMONER_LAB_FACTORY(SummonerLabHostileLiarFactory, AUBaseFactory, sas::lab::au::Liar)
SUMMONER_LAB_FACTORY(SummonerLabHostileDeafFactory, AUBaseFactory, sas::lab::au::Deaf)
SUMMONER_LAB_FACTORY(SummonerLabHostileDragCtorFactory, AUBaseFactory, sas::lab::au::DragCtor)
SUMMONER_LAB_FACTORY(SummonerLabHostileDragInitFactory, AUBaseFactory, sas::lab::au::DragInit)
#endif
