// The lab's DSP cores (summoner_lab_dsp.h). Plain C++17; compiled with -ffp-contract=off, so the arithmetic below is the
// arithmetic that runs, in either wrapper.
#include "summoner_lab_dsp.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <thread>

#include <signal.h>
#include <unistd.h>

namespace sas::lab {
namespace {

constexpr uint8_t kMagic[4] = {'S', 'A', 'S', 'L'};
constexpr uint32_t kVersion = 1;
constexpr size_t kBigState = 64u * 1024u * 1024u + 1u;
constexpr size_t kBig2State = 2u * 1024u * 1024u;  // BIG_GETSTATE's

void putU32(std::vector<uint8_t>& v, uint32_t x) {
    for (int i = 0; i < 4; ++i)
        v.push_back(static_cast<uint8_t>(x >> (8 * i)));
}

uint32_t getU32(const uint8_t* p) {
    return uint32_t(p[0]) | uint32_t(p[1]) << 8 | uint32_t(p[2]) << 16 | uint32_t(p[3]) << 24;
}

float bitsToFloat(uint32_t b) {
    float f;
    std::memcpy(&f, &b, 4);
    return f;
}

uint32_t floatToBits(float f) {
    uint32_t b;
    std::memcpy(&b, &f, 4);
    return b;
}

bool startsWith(const uint8_t* d, size_t n, const char* lit) {
    const size_t l = std::strlen(lit);
    return n >= l && std::memcmp(d, lit, l) == 0;
}

[[noreturn]] void segv() {
    volatile int* p = nullptr;
    *p = 1;  // a bad pointer inside a plugin
    std::abort();
}

bool quietDeaths() {
    static const bool q = [] {
        const char* v = std::getenv("SUMMONER_LAB_DEATH");
        return v != nullptr && std::strcmp(v, "kill") == 0;
    }();
    return q;
}

// The parameters every core keeps: raw floats, readable and writable from any thread.
template <size_t N>
class Params {
public:
    explicit Params(const std::array<ParamInfo, N>& info) : info_(info) {
        for (size_t i = 0; i < N; ++i)
            v_[i].store(info[i].defaultValue, std::memory_order_relaxed);
    }
    float get(uint32_t i) const { return i < N ? v_[i].load(std::memory_order_relaxed) : 0.0f; }
    void set(uint32_t i, float x) {
        if (i < N)
            v_[i].store(x, std::memory_order_relaxed);
    }
    ParamInfo info(uint32_t i) const { return i < N ? info_[i] : ParamInfo{"none", "none", 0.0f}; }

private:
    std::array<ParamInfo, N> info_;
    std::array<std::atomic<float>, N> v_;
};

}  // namespace

void labAbort() {
    if (quietDeaths())
        ::kill(::getpid(), SIGKILL);
    std::abort();
}

void labSegv() {
    if (quietDeaths())
        ::kill(::getpid(), SIGKILL);
    segv();
}

// --- the shared state format ---------------------------------------------------------------------------------------

std::vector<uint8_t> Processor::getState() const {
    std::vector<uint8_t> v(kMagic, kMagic + 4);
    putU32(v, static_cast<uint32_t>(kind()));
    putU32(v, kVersion);
    const uint32_t n = paramCount();
    putU32(v, n);
    for (uint32_t i = 0; i < n; ++i)
        putU32(v, floatToBits(param(i)));
    return v;
}

bool Processor::setState(const uint8_t* d, size_t n) {
    if (n < 16 || std::memcmp(d, kMagic, 4) != 0 || getU32(d + 4) != static_cast<uint32_t>(kind())
        || getU32(d + 8) != kVersion)
        return false;
    const uint32_t count = getU32(d + 12);
    if (count != paramCount() || n != 16 + size_t(count) * 4)
        return false;
    for (uint32_t i = 0; i < count; ++i)
        setParam(i, bitsToFloat(getU32(d + 16 + 4 * size_t(i))));
    return true;
}

// --- Gain ------------------------------------------------------------------------------------------------------------

class Gain final : public Processor {
public:
    Kind kind() const override { return Kind::Gain; }
    uint32_t paramCount() const override { return 1; }
    ParamInfo paramInfo(uint32_t i) const override { return p_.info(i); }
    float param(uint32_t i) const override { return p_.get(i); }
    void setParam(uint32_t i, float x) override { p_.set(i, x); }
    void prepare(double, uint32_t) override {}
    void reset() override {}
    void process(const Block& b) override {
        const float g = p_.get(0);
        for (uint32_t c = 0; c < b.numChannels; ++c)
            for (uint32_t i = 0; i < b.frames; ++i)
                b.channels[c][i] *= g;
    }

private:
    Params<1> p_{{ParamInfo{"gain", "Gain", 1.0f}}};
};

// --- Synth -----------------------------------------------------------------------------------------------------------

class Synth final : public Processor {
public:
    enum : uint32_t { kGain = 0, kRelease = 1 };  // release: value * 1 s, linear

    Kind kind() const override { return Kind::Synth; }
    uint32_t paramCount() const override { return 2; }
    ParamInfo paramInfo(uint32_t i) const override { return p_.info(i); }
    float param(uint32_t i) const override { return p_.get(i); }
    void setParam(uint32_t i, float x) override { p_.set(i, x); }

    explicit Synth(bool needsNote = false) : needsNote_(needsNote) {}

    void prepare(double sampleRate, uint32_t) override {
        rate_ = sampleRate > 0.0 ? sampleRate : 48000.0;
        for (auto& v : voices_)
            v = Voice{};
        pedal_ = 0;
        age_ = 0;
        blocks_ = 0;
        noteSeen_ = false;
        failed_ = false;
    }

    bool blockFailed() const override { return failed_; }

    void reset() override {
        for (auto& v : voices_)
            v = Voice{};
        pedal_ = 0;
    }

    void process(const Block& b) override {
        if (needsNote_) {
            for (uint32_t m = 0; m < b.midiCount; ++m)
                if (b.midi[m].size == 3 && (b.midi[m].data[0] & 0xf0) == 0x90 && b.midi[m].data[2] > 0)
                    noteSeen_ = true;
            failed_ = ++blocks_ >= 8 && !noteSeen_;
        }
        if (b.numChannels == 0)
            return;
        const float gain = p_.get(kGain);
        const double releaseFrames = std::max(1.0, double(std::max(0.0f, p_.get(kRelease))) * rate_);
        uint32_t m = 0;
        for (uint32_t i = 0; i < b.frames; ++i) {
            while (m < b.midiCount && b.midi[m].offset <= i)
                event(b.midi[m++], releaseFrames);
            double sum = 0.0;
            for (auto& v : voices_) {
                if (v.state == Off)
                    continue;
                double env = 1.0;
                if (v.state == Releasing) {
                    env = v.releaseLeft / v.releaseFrames;
                    if (--v.releaseLeft <= 0.0) {
                        v.state = Off;
                        continue;
                    }
                }
                sum += v.amp * env * std::sin(v.phase);
                v.phase += v.inc;
                if (v.phase >= kTwoPi)
                    v.phase -= kTwoPi;
            }
            const float y = static_cast<float>(sum) * gain;
            for (uint32_t c = 0; c < b.numChannels; ++c)
                b.channels[c][i] = y;
        }
    }

private:
    static constexpr double kTwoPi = 6.283185307179586476925286766559;
    bool needsNote_ = false;
    uint32_t blocks_ = 0;
    bool noteSeen_ = false;
    bool failed_ = false;
    enum State : uint8_t { Off, On, Sustained, Releasing };
    struct Voice {
        State state = Off;
        uint8_t channel = 0, note = 0;
        uint64_t born = 0;
        double phase = 0.0, inc = 0.0, amp = 0.0, releaseLeft = 0.0, releaseFrames = 1.0;
    };

    void release(Voice& v, double frames) {
        v.state = Releasing;
        v.releaseLeft = frames;
        v.releaseFrames = frames;
    }

    void event(const MidiEvent& e, double releaseFrames) {
        if (e.size == 0)
            return;
        const uint8_t s = e.data[0], d1 = e.size > 1 ? e.data[1] : 0, d2 = e.size > 2 ? e.data[2] : 0;
        const uint8_t ch = s & 0x0f;
        switch (s & 0xf0) {
            case 0x90:
                if (d2 > 0) {
                    noteOn(ch, d1, d2);
                    break;
                }
                [[fallthrough]];
            case 0x80:
                for (auto& v : voices_)
                    if (v.state == On && v.channel == ch && v.note == d1) {
                        if ((pedal_ >> ch) & 1u)
                            v.state = Sustained;
                        else
                            release(v, releaseFrames);
                    }
                break;
            case 0xb0:
                if (d1 == 120) {  // all sound off: at once
                    for (auto& v : voices_)
                        v.state = Off;
                } else if (d1 == 123) {  // all notes off: released (the pedal does not hold them)
                    for (auto& v : voices_)
                        if (v.state == On || v.state == Sustained)
                            release(v, releaseFrames);
                } else if (d1 == 64) {
                    if (d2 >= 64) {
                        pedal_ |= uint16_t(1u << ch);
                    } else {
                        pedal_ &= uint16_t(~(1u << ch));
                        for (auto& v : voices_)
                            if (v.state == Sustained && v.channel == ch)
                                release(v, releaseFrames);
                    }
                }
                break;
            default: break;
        }
    }

    void noteOn(uint8_t ch, uint8_t note, uint8_t vel) {
        Voice* pick = nullptr;
        for (auto& v : voices_)
            if (v.state == Off) {
                pick = &v;
                break;
            }
        if (pick == nullptr) {  // steal the oldest
            pick = &voices_[0];
            for (auto& v : voices_)
                if (v.born < pick->born)
                    pick = &v;
        }
        const double hz = 440.0 * std::pow(2.0, (double(note) - 69.0) / 12.0);
        *pick = Voice{};
        pick->state = On;
        pick->channel = ch;
        pick->note = note;
        pick->born = ++age_;
        pick->inc = kTwoPi * hz / rate_;
        pick->amp = 0.25 * double(vel) / 127.0;
    }

    Params<2> p_{{ParamInfo{"gain", "Gain", 1.0f}, ParamInfo{"release", "Release", 0.05f}}};
    std::array<Voice, 16> voices_{};
    uint16_t pedal_ = 0;
    uint64_t age_ = 0;
    double rate_ = 48000.0;
};

// --- Hostile ---------------------------------------------------------------------------------------------------------

class Hostile final : public Processor {
public:
    explicit Hostile(const HostileConfig& cfg) : cfg_(cfg) {
        if (cfg_.crashInConstructor)
            labAbort();
        while (cfg_.hangInConstructor)  // a hang, not a spin: the scan's step deadline must end it
            std::this_thread::sleep_for(std::chrono::seconds(1));
    }

    Kind kind() const override { return Kind::Hostile; }
    uint32_t paramCount() const override { return cfg_.lieParamCount != 0 ? cfg_.lieParamCount : hostile::Count; }
    ParamInfo paramInfo(uint32_t i) const override { return p_.info(i); }
    float param(uint32_t i) const override { return p_.get(i); }
    void setParam(uint32_t i, float x) override { p_.set(i, x); }
    uint32_t latency() const override {
        const float v = std::max(0.0f, std::min(1.0f, p_.get(hostile::LatencyFrames)));
        return static_cast<uint32_t>(std::lround(double(v) * 8192.0));
    }

    // The latency it reports is applied (as a delay) only here, at prepare, as a plugin re-sizes its buffering only
    // when the host re-activates it: until then the reported latency may run ahead of the applied one.
    void prepare(double, uint32_t) override {
        if (cfg_.prepareSleepMs > 0)
            std::this_thread::sleep_for(std::chrono::milliseconds(cfg_.prepareSleepMs));
        if (cfg_.crashInPrepare)
            labAbort();
        applied_ = latency();
        for (auto& l : line_)
            l.assign(std::max<uint32_t>(applied_, 1), 0.0f);
        pos_ = 0;
    }

    uint32_t appliedLatency() const { return applied_; }

    void reset() override {}

    void process(const Block& b) override {
        using namespace hostile;
        if (p_.get(Abort) >= 0.5f)
            labAbort();
        if (p_.get(Segv) >= 0.5f)
            labSegv();
        if (p_.get(Throw) >= 0.5f)
            throw std::runtime_error("the hostile plugin throws from process()");
        if (p_.get(Wedge) >= 0.5f) {
            volatile bool forever = true;
            while (forever) {
            }
        }
        const float burn = std::max(0.0f, std::min(1.0f, p_.get(BurnMs)));
        if (burn > 0.0f) {
            const auto until = std::chrono::steady_clock::now()
                               + std::chrono::microseconds(static_cast<int64_t>(double(burn) * 100000.0));
            while (std::chrono::steady_clock::now() < until) {
            }
        }
        const float g = p_.get(hostile::Gain);
        const uint32_t nc = std::min<uint32_t>(b.numChannels, uint32_t(line_.size()));
        float forced = 0.0f;
        bool force = true;
        if (p_.get(Nan) >= 0.5f)
            forced = std::nanf("");
        else if (p_.get(Inf) >= 0.5f)
            forced = INFINITY;
        else if (p_.get(Denormal) >= 0.5f)
            forced = bitsToFloat(1u);
        else if (p_.get(Huge) >= 0.5f)
            forced = 1e30f;
        else
            force = false;
        for (uint32_t i = 0; i < b.frames; ++i) {
            for (uint32_t c = 0; c < nc; ++c) {
                float x = b.channels[c][i] * g;
                if (applied_ > 0) {
                    const float y = line_[c][pos_];
                    line_[c][pos_] = x;
                    x = y;
                }
                b.channels[c][i] = force ? forced : x;
            }
            if (applied_ > 0)
                pos_ = (pos_ + 1) % applied_;
        }
        if (Mode(mode_.load(std::memory_order_relaxed)) == Mode::Transport)
            reportTransport(b);
    }

    // TRANSPORT_ENCODING (summoner_lab_dsp.h).
    static void reportTransport(const Block& b) {
        const Transport none{};
        const Transport& t = b.transport != nullptr ? *b.transport : none;
        const float ppq = t.musicValid ? float(t.projectTimeMusic) : 0.0f;
        const uint32_t flags = (t.playing ? 1u : 0u) | (t.recording ? 2u : 0u) | (t.cycleActive ? 4u : 0u)
                               | (t.tempoValid ? 8u : 0u) | (t.musicValid ? 16u : 0u)
                               | (b.transport != nullptr ? 32u : 0u);
        const float ch1[3] = {t.tempoValid ? float(t.tempo) : 0.0f, float(flags), float(t.projectTimeSamples)};
        for (uint32_t c = 0; c < b.numChannels; ++c)
            for (uint32_t f = 0; f < b.frames; ++f)
                b.channels[c][f] = c == 0 ? ppq : c == 1 && f < 3 ? ch1[f] : 0.0f;
    }

    std::vector<uint8_t> getState() const override {
        switch (Mode(mode_.load(std::memory_order_acquire))) {
            case Mode::Big: return std::vector<uint8_t>(kBigState, 0x42);
            case Mode::SlowGet: {
                const uint32_t ms = slowMs_.load(std::memory_order_relaxed);
                std::this_thread::sleep_for(std::chrono::milliseconds(ms));
                const std::string s = "SLOW_GETSTATE=" + std::to_string(ms);
                return std::vector<uint8_t>(s.begin(), s.end());
            }
            case Mode::Big2: return big2State();
            case Mode::Transport: return {'T', 'R', 'A', 'N', 'S', 'P', 'O', 'R', 'T'};
            case Mode::FailGet:  // the wrapper asks getStateFails() first; a direct caller gets the parameters
            case Mode::None: break;
        }
        if (cfg_.refusesOwnState) {
            auto v = Processor::getState();
            v[8] ^= 0xff;  // the version field: setState() refuses it (the round-trip fixture)
            return v;
        }
        return Processor::getState();
    }

    bool getStateFails() const override { return Mode(mode_.load(std::memory_order_acquire)) == Mode::FailGet; }

    bool setState(const uint8_t* d, size_t n) override {
        // The knobs that are modes: checked before the older words they share a prefix with (SLOW, FAIL, BIG).
        if (startsWith(d, n, "SLOW_GETSTATE=")) {
            uint64_t ms = 0;
            for (size_t i = 14; i < n && d[i] >= '0' && d[i] <= '9' && ms <= 60000; ++i)
                ms = ms * 10 + (d[i] - '0');
            slowMs_.store(uint32_t(std::min<uint64_t>(ms, 60000)), std::memory_order_relaxed);
            mode_.store(uint32_t(Mode::SlowGet), std::memory_order_release);
            return true;
        }
        if (n == 12 && startsWith(d, n, "BIG_GETSTATE")) {
            mode_.store(uint32_t(Mode::Big2), std::memory_order_release);
            return true;
        }
        if (startsWith(d, n, "SASBIG2M")) {
            const auto want = big2State();
            const bool same = n == want.size() && std::memcmp(d, want.data(), n) == 0;
            mode_.store(uint32_t(same ? Mode::Big2 : Mode::None), std::memory_order_release);
            return same;
        }
        if (n == 13 && startsWith(d, n, "FAIL_GETSTATE")) {
            mode_.store(uint32_t(Mode::FailGet), std::memory_order_release);
            return true;
        }
        if (n == 9 && startsWith(d, n, "TRANSPORT")) {
            mode_.store(uint32_t(Mode::Transport), std::memory_order_release);
            return true;
        }
        mode_.store(uint32_t(Mode::None), std::memory_order_release);
        if (startsWith(d, n, "CRASH"))
            labAbort();
        if (startsWith(d, n, "SEGV"))
            labSegv();
        if (startsWith(d, n, "SLOW=")) {
            uint64_t ms = 0;
            for (size_t i = 5; i < n && d[i] >= '0' && d[i] <= '9' && ms < 60000; ++i)
                ms = ms * 10 + (d[i] - '0');
            std::this_thread::sleep_for(std::chrono::milliseconds(std::min<uint64_t>(ms, 60000)));
            return true;
        }
        if (startsWith(d, n, "FAIL"))
            return false;
        if (startsWith(d, n, "LATER=")) {
            // "LATER=<ms>:<frames>": the latency changes to <frames>, <ms> later, on a thread of the plugin's own (a
            // plugin that changes its latency outside any host call; the controller raises the restart, see the
            // VST3 wrapper). Applied (true) at once; nothing else in the state.
            uint64_t ms = 0, frames = 0;
            size_t i = 6;
            for (; i < n && d[i] >= '0' && d[i] <= '9' && ms < 60000; ++i)
                ms = ms * 10 + (d[i] - '0');
            if (i < n && d[i] == ':')
                for (++i; i < n && d[i] >= '0' && d[i] <= '9' && frames < 8192; ++i)
                    frames = frames * 10 + (d[i] - '0');
            const float v = float(std::min<uint64_t>(frames, 8192)) / 8192.0f;
            std::thread([this, ms, v] {
                std::this_thread::sleep_for(std::chrono::milliseconds(ms));
                p_.set(hostile::LatencyFrames, v);
            }).detach();
            return true;
        }
        if (startsWith(d, n, "BIG")) {
            mode_.store(uint32_t(Mode::Big), std::memory_order_release);
            return true;
        }
        return Processor::setState(d, n);  // with a lying count, the lie round-trips (the ghosts read as 0)
    }

private:
    HostileConfig cfg_;
    Params<hostile::Count> p_{{
        ParamInfo{"gain", "Gain", 1.0f},
        ParamInfo{"abort", "Abort", 0.0f},
        ParamInfo{"segv", "SIGSEGV", 0.0f},
        ParamInfo{"throw", "Throw", 0.0f},
        ParamInfo{"wedge", "Wedge", 0.0f},
        ParamInfo{"burn", "Burn (x 100 ms)", 0.0f},
        ParamInfo{"nan", "NaN out", 0.0f},
        ParamInfo{"inf", "Inf out", 0.0f},
        ParamInfo{"denormal", "Denormal out", 0.0f},
        ParamInfo{"huge", "Huge out", 0.0f},
        ParamInfo{"latency", "Latency (x 8192)", 0.0f},
    }};
    enum class Mode : uint32_t { None, Big, SlowGet, Big2, FailGet, Transport };
    static std::vector<uint8_t> big2State() {
        std::vector<uint8_t> v(kBig2State);
        std::memcpy(v.data(), "SASBIG2M", 8);
        for (size_t i = 8; i < v.size(); ++i)
            v[i] = uint8_t((uint64_t(i) * 2654435761u) >> 24);
        return v;
    }
    std::atomic<uint32_t> mode_{0};
    std::atomic<uint32_t> slowMs_{0};
    // The applied delay (frames) and its lines, sized at prepare (never in process).
    uint32_t applied_ = 0;
    std::array<std::vector<float>, 8> line_{};
    size_t pos_ = 0;
};

std::unique_ptr<Processor> makeGain() { return std::make_unique<Gain>(); }
std::unique_ptr<Processor> makeSynth(bool needsNote) { return std::make_unique<Synth>(needsNote); }
std::unique_ptr<Processor> makeHostile(const HostileConfig& cfg) { return std::make_unique<Hostile>(cfg); }

}  // namespace sas::lab
