// The DSP cores' own unit tests (no framework: each CHECK prints and counts; exit 1 on any failure). The crash and
// wedge switches run in a forked child of this test, which is waited for or killed by its recorded pid.
#include "summoner_lab_dsp.h"

#include <sys/wait.h>
#include <unistd.h>

#include <chrono>
#include <cmath>
#include <csignal>
#include <cstdio>
#include <cstring>
#include <functional>
#include <string>
#include <thread>
#include <vector>

using namespace sas::lab;

static int failures = 0;
#define CHECK(cond)                                                                \
    do {                                                                           \
        if (!(cond)) {                                                             \
            std::fprintf(stderr, "%s:%d: CHECK failed: %s\n", __FILE__, __LINE__, #cond); \
            ++failures;                                                            \
        }                                                                          \
    } while (0)

namespace {

uint32_t bits(float f) {
    uint32_t b;
    std::memcpy(&b, &f, 4);
    return b;
}

struct Buf {
    std::vector<std::vector<float>> ch;
    std::vector<float*> ptrs;
    Buf(uint32_t n, uint32_t frames, float fill = 0.0f) : ch(n, std::vector<float>(frames, fill)) {
        for (auto& c : ch)
            ptrs.push_back(c.data());
    }
    Block block(uint32_t frames, const std::vector<MidiEvent>& midi = {}) {
        return Block{ptrs.data(), uint32_t(ptrs.size()), frames, midi.data(), uint32_t(midi.size())};
    }
};

MidiEvent ev(uint32_t at, uint8_t a, uint8_t b, uint8_t c) {
    MidiEvent e;
    e.offset = at;
    e.size = 3;
    e.data[0] = a;
    e.data[1] = b;
    e.data[2] = c;
    return e;
}

// The forked child reports a fatal signal as exit status 100 + signal instead of dying of it: the test still sees
// which signal the plugin raised, and macOS's ReportCrash writes no report (it ignores the lab HOME and would write
// into the real ~/Library/Logs/DiagnosticReports).
extern "C" void exitWithSignal(int sig) {
    _exit(100 + sig);
}

bool diedOf(int st, int sig) {
    return st != -1 && WIFEXITED(st) && WEXITSTATUS(st) == 100 + sig;
}

// Runs `f` in a forked child; returns its wait status, or -1 if it was still running after `ms` (then SIGKILLed by
// its pid and reaped).
int inChild(const std::function<void()>& f, int ms) {
    std::fflush(nullptr);
    const pid_t pid = fork();
    if (pid == 0) {
        for (int sig : {SIGABRT, SIGSEGV, SIGBUS})
            std::signal(sig, exitWithSignal);
        f();
        _exit(0);
    }
    const auto until = std::chrono::steady_clock::now() + std::chrono::milliseconds(ms);
    int st = 0;
    while (std::chrono::steady_clock::now() < until) {
        if (waitpid(pid, &st, WNOHANG) == pid)
            return st;
        std::this_thread::sleep_for(std::chrono::milliseconds(5));
    }
    kill(pid, SIGKILL);
    waitpid(pid, &st, 0);
    return -1;
}

void testGain() {
    auto g = makeGain();
    CHECK(g->paramCount() == 1);
    g->prepare(48000, 64);
    g->setParam(0, 0.3f);
    Buf b(2, 64, 0.7f);
    g->process(b.block(64));
    CHECK(bits(b.ch[0][0]) == bits(0.7f * 0.3f));
    CHECK(bits(b.ch[1][63]) == bits(0.7f * 0.3f));
    auto s = g->getState();
    auto h = makeGain();
    CHECK(h->setState(s.data(), s.size()));
    CHECK(bits(h->param(0)) == bits(0.3f));
    CHECK(h->getState() == s);
    // A state of another kind, a short one, a bad version are refused.
    auto syn = makeSynth()->getState();
    CHECK(!h->setState(syn.data(), syn.size()));
    CHECK(!h->setState(s.data(), 10));
    auto bad = s;
    bad[8] = 9;
    CHECK(!h->setState(bad.data(), bad.size()));
}

std::vector<float> render(Processor& p, const std::vector<std::vector<MidiEvent>>& blocks, uint32_t frames) {
    std::vector<float> out;
    Buf b(2, frames);
    for (const auto& m : blocks) {
        for (auto& c : b.ch)
            std::fill(c.begin(), c.end(), 0.0f);
        p.process(b.block(frames, m));
        out.insert(out.end(), b.ch[0].begin(), b.ch[0].end());
        for (uint32_t i = 0; i < frames; ++i)
            CHECK(bits(b.ch[0][i]) == bits(b.ch[1][i]));
    }
    return out;
}

void testSynth() {
    const std::vector<std::vector<MidiEvent>> blocks = {
        {ev(10, 0x90, 69, 127)}, {}, {ev(5, 0x80, 69, 0)}, {}, {}, {}, {}, {}, {}, {},
    };
    auto a = makeSynth(), b = makeSynth();
    a->setParam(1, 0.002f);  // release: 96 frames at 48 kHz
    b->setParam(1, 0.002f);
    a->prepare(48000, 64);
    b->prepare(48000, 64);
    const auto x = render(*a, blocks, 64), y = render(*b, blocks, 64);
    CHECK(x.size() == y.size());
    bool same = true;
    for (size_t i = 0; i < x.size(); ++i)
        same = same && bits(x[i]) == bits(y[i]);
    CHECK(same);  // deterministic: two instances, the same bytes
    for (int i = 0; i < 10; ++i)
        CHECK(x[size_t(i)] == 0.0f);  // silent before the note
    CHECK(x[11] != 0.0f);             // sounding after it (sin of the first step)
    CHECK(x[64 * 9 + 63] == 0.0f);    // released to silence
    // Re-prepared, it renders the same bytes again (phase restarts).
    a->prepare(48000, 64);
    const auto z = render(*a, blocks, 64);
    bool again = true;
    for (size_t i = 0; i < x.size(); ++i)
        again = again && bits(x[i]) == bits(z[i]);
    CHECK(again);
    // The pedal holds a released note until it comes up; CC 123 releases through the pedal; CC 120 cuts at once.
    auto s = makeSynth();
    s->setParam(1, 0.0f);  // release: 1 frame
    s->prepare(48000, 64);
    auto held = render(*s, {{ev(0, 0xb0, 64, 127), ev(0, 0x90, 60, 100), ev(8, 0x80, 60, 0)}, {}}, 64);
    CHECK(held[64 + 32] != 0.0f);
    auto up = render(*s, {{ev(0, 0xb0, 64, 0)}, {}}, 64);
    CHECK(up[2] == 0.0f && up[64 + 32] == 0.0f);
    render(*s, {{ev(0, 0x90, 60, 100)}}, 64);
    auto cut = render(*s, {{ev(0, 0xb0, 120, 0)}}, 64);
    CHECK(cut[0] == 0.0f && cut[63] == 0.0f);
    // A state round trip is bit-exact.
    auto st = a->getState();
    auto c = makeSynth();
    CHECK(c->setState(st.data(), st.size()));
    CHECK(c->getState() == st);
}

void testHostile() {
    using namespace hostile;
    auto h = makeHostile();
    CHECK(h->paramCount() == Count);
    h->prepare(48000, 64);
    auto out = [&](uint32_t p) {
        auto x = makeHostile();
        x->prepare(48000, 64);
        x->setParam(p, 1.0f);
        Buf b(2, 8, 0.5f);
        x->process(b.block(8));
        return b.ch[1][7];
    };
    CHECK(std::isnan(out(Nan)));
    CHECK(std::isinf(out(Inf)));
    CHECK(bits(out(Denormal)) == 1u);
    CHECK(out(Huge) == 1e30f);
    // Latency moves with its parameter as reported, but is applied (a delay) only at the next prepare.
    CHECK(h->latency() == 0);
    h->setParam(LatencyFrames, 0.5f);
    CHECK(h->latency() == 4096);
    {
        Buf before(2, 8, 0.5f);
        h->process(before.block(8));
        CHECK(before.ch[0][0] == 0.5f);  // not yet applied: no delay
        h->prepare(48000, 64);
        Buf after(2, 8, 0.5f);
        h->process(after.block(8));
        CHECK(after.ch[0][0] == 0.0f && after.ch[1][7] == 0.0f);  // applied: 4096 frames of delay, zeros first
        // A delay of 3 frames shows the line exactly: input i comes out at i + 3.
        auto d = makeHostile();
        d->setParam(LatencyFrames, 3.0f / 8192.0f);
        d->prepare(48000, 64);
        Buf r(1, 8);
        for (uint32_t i = 0; i < 8; ++i)
            r.ch[0][i] = float(i + 1);
        d->process(r.block(8));
        CHECK(r.ch[0][0] == 0.0f && r.ch[0][2] == 0.0f && r.ch[0][3] == 1.0f && r.ch[0][7] == 5.0f);
        h->setParam(LatencyFrames, 0.0f);
        h->prepare(48000, 64);
    }
    // Burn: at least the asked time.
    h->setParam(BurnMs, 0.2f);  // 20 ms
    Buf b(2, 8);
    const auto t0 = std::chrono::steady_clock::now();
    h->process(b.block(8));
    CHECK(std::chrono::steady_clock::now() - t0 >= std::chrono::milliseconds(20));
    h->setParam(BurnMs, 0.0f);
    // Throw is an exception, not a crash.
    h->setParam(Throw, 1.0f);
    bool threw = false;
    try {
        h->process(b.block(8));
    } catch (const std::exception&) {
        threw = true;
    }
    CHECK(threw);
    // The crashes and the wedge, in a forked child.
    auto inProcess = [](uint32_t p) {
        return [p] {
            auto x = makeHostile();
            x->prepare(48000, 64);
            x->setParam(p, 1.0f);
            Buf bb(2, 8);
            x->process(bb.block(8));
        };
    };
    int st = inChild(inProcess(Abort), 2000);
    CHECK(diedOf(st, SIGABRT));
    st = inChild(inProcess(Segv), 2000);
    CHECK(diedOf(st, SIGSEGV) || diedOf(st, SIGBUS));
    CHECK(inChild(inProcess(Wedge), 300) == -1);  // still inside process() after 300 ms
    st = inChild([] { makeHostile(HostileConfig{true, false, 0, 0}); }, 2000);
    CHECK(diedOf(st, SIGABRT));
    st = inChild([] { makeHostile(HostileConfig{false, true, 0, 0})->prepare(48000, 64); }, 2000);
    CHECK(diedOf(st, SIGABRT));
    st = inChild([] {
        const char s[] = "CRASH";
        makeHostile()->setState(reinterpret_cast<const uint8_t*>(s), 5);
    }, 2000);
    CHECK(diedOf(st, SIGABRT));
    // States: FAIL refused, SLOW sleeps, BIG is over 64 MiB, a normal state round-trips.
    auto x = makeHostile();
    CHECK(!x->setState(reinterpret_cast<const uint8_t*>("FAIL"), 4));
    const auto s0 = std::chrono::steady_clock::now();
    CHECK(x->setState(reinterpret_cast<const uint8_t*>("SLOW=30"), 7));
    CHECK(std::chrono::steady_clock::now() - s0 >= std::chrono::milliseconds(30));
    CHECK(x->setState(reinterpret_cast<const uint8_t*>("BIG"), 3));
    CHECK(x->getState().size() == 64u * 1024u * 1024u + 1u);
    x->setParam(hostile::Gain, 0.25f);
    auto good = makeHostile()->getState();
    CHECK(x->setState(good.data(), good.size()));
    CHECK(x->getState() == good);
    // A lying parameter count is reported, and its state round-trips.
    auto liar = makeHostile(HostileConfig{false, false, 0, 77});
    CHECK(liar->paramCount() == 77);
    auto ls = liar->getState();
    CHECK(liar->setState(ls.data(), ls.size()));
}

// The getState knobs and the TRANSPORT reporter (summoner_lab_dsp.h).
void testKnobs() {
    auto set = [](Processor& p, const char* str) {
        return p.setState(reinterpret_cast<const uint8_t*>(str), std::strlen(str));
    };
    auto str = [](const std::vector<uint8_t>& v) { return std::string(v.begin(), v.end()); };
    // SLOW_GETSTATE: getState sleeps, then returns the knob's own bytes; a round trip keeps the mode.
    auto x = makeHostile();
    CHECK(set(*x, "SLOW_GETSTATE=40"));
    auto t0 = std::chrono::steady_clock::now();
    auto g = x->getState();
    CHECK(std::chrono::steady_clock::now() - t0 >= std::chrono::milliseconds(40));
    CHECK(str(g) == "SLOW_GETSTATE=40");
    CHECK(x->setState(g.data(), g.size()) && str(x->getState()) == "SLOW_GETSTATE=40");
    // A normal state ends the mode at once.
    auto normal = makeHostile()->getState();
    CHECK(x->setState(normal.data(), normal.size()));
    t0 = std::chrono::steady_clock::now();
    CHECK(x->getState() == normal);
    CHECK(std::chrono::steady_clock::now() - t0 < std::chrono::milliseconds(1000));
    // BIG_GETSTATE: exactly 2 MiB, deterministic, round-trips; a changed byte is refused and ends the mode.
    CHECK(set(*x, "BIG_GETSTATE"));
    auto big = x->getState();
    CHECK(big.size() == 2u * 1024u * 1024u);
    CHECK(std::memcmp(big.data(), "SASBIG2M", 8) == 0);
    CHECK(big[8] == uint8_t((uint64_t(8) * 2654435761u) >> 24));
    CHECK(big[big.size() - 1] == uint8_t((uint64_t(big.size() - 1) * 2654435761u) >> 24));
    auto y = makeHostile();
    CHECK(y->setState(big.data(), big.size()) && y->getState() == big);
    auto bad = big;
    bad[1000] ^= 1;
    CHECK(!y->setState(bad.data(), bad.size()) && y->getState() != big);
    // The older words keep their meaning beside the new ones that share a prefix.
    CHECK(set(*y, "BIG") && y->getState().size() == 64u * 1024u * 1024u + 1u);
    CHECK(!set(*y, "FAIL"));
    CHECK(!y->getStateFails());
    // FAIL_GETSTATE: getStateFails, until another state.
    CHECK(set(*y, "FAIL_GETSTATE") && y->getStateFails());
    CHECK(y->setState(normal.data(), normal.size()) && !y->getStateFails());
    // TRANSPORT: the report overwrites the output (TRANSPORT_ENCODING); getState is the constant word.
    auto r = makeHostile();
    r->prepare(48000, 64);
    CHECK(set(*r, "TRANSPORT") && str(r->getState()) == "TRANSPORT");
    Transport t;
    t.playing = true;
    t.tempoValid = true;
    t.musicValid = true;
    t.tempo = 97.5;
    t.projectTimeMusic = 12.3456789;
    t.projectTimeSamples = 480000;
    Buf b(2, 8, 0.5f);
    Block blk = b.block(8);
    blk.transport = &t;
    r->process(blk);
    CHECK(b.ch[0][0] == float(12.3456789) && b.ch[0][7] == float(12.3456789));
    CHECK(b.ch[1][0] == 97.5f && b.ch[1][1] == float(1 | 8 | 16 | 32) && b.ch[1][2] == 480000.0f);
    CHECK(b.ch[1][3] == 0.0f && b.ch[1][7] == 0.0f);
    // No transport given: flags 0, every value 0.
    Buf n(2, 4, 0.5f);
    r->process(n.block(4));
    CHECK(n.ch[0][0] == 0.0f && n.ch[1][0] == 0.0f && n.ch[1][1] == 0.0f && n.ch[1][2] == 0.0f);
    // Not playing, tempo invalid: tempo reads 0, the flags only "given" and music valid.
    t.playing = false;
    t.tempoValid = false;
    Buf q(2, 2, 0.5f);
    Block qb = q.block(2);
    qb.transport = &t;
    r->process(qb);  // 2 frames: ch1 carries tempo and the flags, the sample count does not fit
    CHECK(q.ch[1][0] == 0.0f && q.ch[1][1] == float(16 | 32));
    // Off again after a normal state: the input passes (gain 1).
    CHECK(r->setState(normal.data(), normal.size()));
    Buf o(2, 4, 0.5f);
    Block ob = o.block(4);
    ob.transport = &t;
    r->process(ob);
    CHECK(o.ch[0][0] == 0.5f && o.ch[1][1] == 0.5f);
}

// The needs-the-note synth: blocks 1-7 pass, the 8th and later fail without a note-on; with one in the first
// block none fails; prepare() starts the count again. The plain synth never fails.
void testNeedsNote() {
    auto run = [](Processor& p, bool note, int blocks) {
        int firstFail = 0;
        for (int k = 1; k <= blocks; ++k) {
            Buf b(2, 64);
            std::vector<MidiEvent> m;
            if (note && k == 1)
                m.push_back(ev(0, 0x90, 60, 100));
            p.process(b.block(64, m));
            if (p.blockFailed() && firstFail == 0)
                firstFail = k;
        }
        return firstFail;
    };
    auto s = makeSynth(true);
    s->prepare(48000, 64);
    CHECK(run(*s, false, 10) == 8);
    s->prepare(48000, 64);
    CHECK(run(*s, true, 10) == 0);
    s->prepare(48000, 64);
    CHECK(run(*s, false, 7) == 0);  // the count restarts at prepare
    auto plain = makeSynth();
    plain->prepare(48000, 64);
    CHECK(run(*plain, false, 10) == 0);
}

}  // namespace

int main() {
    testGain();
    testSynth();
    testHostile();
    testKnobs();
    testNeedsNote();
    std::printf("summoner-lab-dsp tests: %s (%d failed checks)\n", failures == 0 ? "PASS" : "FAIL", failures);
    return failures == 0 ? 0 : 1;
}
