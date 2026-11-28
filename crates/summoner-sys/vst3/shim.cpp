// The VST3 host shim (summoner_vst3_shim.h): glue between summoner-sys and the pinned SDK. Every exported function catches
// every exception (SUMMONER_VST3_THREW); nothing here computes audio, chooses policy or transforms state bytes.
#include "summoner_vst3_shim.h"

#include "pluginterfaces/base/ibstream.h"
#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmessage.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "pluginterfaces/vst/ivstunits.h"
#include "public.sdk/source/common/memorystream.h"
#include "public.sdk/source/vst/hosting/eventlist.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"

#include <CoreFoundation/CoreFoundation.h>

#include "fill_event.h"

#include <atomic>
#include <new>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {

void setErr(char* err, size_t cap, const std::string& msg) noexcept {
    if (err == nullptr || cap == 0)
        return;
    const size_t n = std::min(cap - 1, msg.size());
    std::memcpy(err, msg.data(), n);
    err[n] = 0;
}

void copyStr(char* dst, size_t cap, const char* src) noexcept {
    size_t i = 0;
    for (; src != nullptr && i + 1 < cap && src[i] != 0; ++i)
        dst[i] = src[i];
    dst[i] = 0;
}

// A String128 (at most 128 UTF-16 units, to its terminator) to UTF-8, as the host application expects a VST3 title
// (decoded from UTF-16, a surrogate pair one code point; a 64-character name request ignores its length; written as
// UTF-8). A lone surrogate is U+FFFD here (a declared divergence: it is not written as the invalid 3-byte form of its
// own value). Never cuts a character: one that does
// not fit `cap` (with the terminator) ends the copy; 128 units need at most 384 bytes.
void copyStr16(char* dst, size_t cap, const char16* src, size_t units) noexcept {
    size_t o = 0;
    auto put = [&](uint32_t c) {
        char b[4];
        size_t n = 0;
        if (c < 0x80) {
            b[n++] = char(c);
        } else if (c < 0x800) {
            b[n++] = char(0xC0 | (c >> 6));
            b[n++] = char(0x80 | (c & 0x3F));
        } else if (c < 0x10000) {
            b[n++] = char(0xE0 | (c >> 12));
            b[n++] = char(0x80 | ((c >> 6) & 0x3F));
            b[n++] = char(0x80 | (c & 0x3F));
        } else {
            b[n++] = char(0xF0 | (c >> 18));
            b[n++] = char(0x80 | ((c >> 12) & 0x3F));
            b[n++] = char(0x80 | ((c >> 6) & 0x3F));
            b[n++] = char(0x80 | (c & 0x3F));
        }
        if (o + n + 1 > cap)
            return false;
        std::memcpy(dst + o, b, n);
        o += n;
        return true;
    };
    for (size_t i = 0; src != nullptr && i < units && src[i] != 0; ++i) {
        uint32_t c = uint16_t(src[i]);
        if (c >= 0xD800 && c <= 0xDBFF && i + 1 < units && uint16_t(src[i + 1]) >= 0xDC00
            && uint16_t(src[i + 1]) <= 0xDFFF) {
            c = 0x10000 + ((c - 0xD800) << 10) + (uint16_t(src[i + 1]) - 0xDC00);
            ++i;
        } else if (c >= 0xD800 && c <= 0xDFFF) {
            c = 0xFFFD;
        }
        if (!put(c))
            break;
    }
    if (cap > 0)
        dst[o] = 0;
}

// Run `f`, mapping exceptions to SUMMONER_VST3_THREW.
template <typename F>
int guarded(F&& f) noexcept {
    try {
        return f();
    } catch (...) {
        return SUMMONER_VST3_THREW;
    }
}

using BundleEntryFunc = bool (*)(CFBundleRef);
using BundleExitFunc = bool (*)();

// The host's IComponentHandler. Restarts are recorded. An edit (performEdit, from the controller: its editor, or
// the plugin on its own) goes to the processor by value (IComponentHandler::performEdit): its value rounded to a
// float, into a fixed ring the chain thread drains before its next block (GUI -> audio by value only), the edit
// counted (the message thread's poll moves childGen), and the controller set to it when its own value differs. A full
// ring drops the edit and counts the drop. Producers are any thread but the chain thread's, serialized by a mutex
// the chain thread never takes; the one consumer (the chain thread) is lock-free.
class ComponentHandler final : public IComponentHandler {
public:
    static constexpr uint32_t kRing = 1024;
    struct Edit {
        ParamID id;
        double value;
    };
    std::atomic<int32> restart{0};
    IEditController* controller = nullptr;  // set with setComponentHandler, cleared before the controller goes
    Edit ring[kRing] = {};
    std::atomic<uint32_t> head{0};  // written by producers (under `push`)
    std::atomic<uint32_t> tail{0};  // written by the consumer
    std::mutex push;
    std::atomic<uint64_t> edits{0};
    std::atomic<uint64_t> dropped{0};

    tresult PLUGIN_API beginEdit(ParamID) override { return kResultOk; }
    tresult PLUGIN_API performEdit(ParamID id, ParamValue v) override {
        const double value = double(float(v));
        {
            std::lock_guard<std::mutex> g(push);
            const uint32_t h = head.load(std::memory_order_relaxed);
            if (h - tail.load(std::memory_order_acquire) >= kRing) {
                dropped.fetch_add(1, std::memory_order_relaxed);
            } else {
                ring[h % kRing] = Edit{id, value};
                head.store(h + 1, std::memory_order_release);
            }
        }
        edits.fetch_add(1, std::memory_order_acq_rel);
        if (controller != nullptr && controller->getParamNormalized(id) != v)
            return controller->setParamNormalized(id, v);
        return kResultOk;
    }
    // The chain thread's pop: true and the oldest edit, or false when there is none. Never blocks.
    bool pop(Edit& out) {
        const uint32_t t = tail.load(std::memory_order_relaxed);
        if (t == head.load(std::memory_order_acquire))
            return false;
        out = ring[t % kRing];
        tail.store(t + 1, std::memory_order_release);
        return true;
    }
    tresult PLUGIN_API endEdit(ParamID) override { return kResultOk; }
    tresult PLUGIN_API restartComponent(int32 flags) override {
        restart.fetch_or(flags, std::memory_order_acq_rel);
        return kResultOk;
    }
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IComponentHandler::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IComponentHandler*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }  // owned by the plugin record, outlives the plugin
    uint32 PLUGIN_API release() override { return 1; }
};

// The editor's IPlugFrame: a plugin's resizeView sets the window's content size
// through `setSize` (the editor dylib's summoner_ed_window_set_size, passed in at attach: this shim links no AppKit), then
// confirms with onSize at the size the window now has, unless that is a same-size re-entry from inside onSize
// (IPlugFrame::resizeView / IPlugView::onSize: a re-entry guard). Main thread only.
using SetSizeFn = int (*)(void* window, double w, double h);
using GetSizeFn = int (*)(void* window, double* w, double* h);
class PlugFrame final : public IPlugFrame {
public:
    IPlugView* view = nullptr;
    SetSizeFn setSize = nullptr;
    GetSizeFn getSize = nullptr;
    void* window = nullptr;
    bool inOnSize = false;
    tresult PLUGIN_API resizeView(IPlugView* v, ViewRect* r) override {
        if (v == nullptr || r == nullptr || v != view || setSize == nullptr)
            return kInvalidArgument;
        double ow = 0, oh = 0;
        if (getSize != nullptr)
            getSize(window, &ow, &oh);
        setSize(window, double(r->getWidth()), double(r->getHeight()));
        double nw = r->getWidth(), nh = r->getHeight();
        if (getSize != nullptr)
            getSize(window, &nw, &nh);
        if (nw != ow || nh != oh || !inOnSize) {
            ViewRect now(0, 0, int32(nw), int32(nh));
            inOnSize = true;
            view->onSize(&now);
            inOnSize = false;
        }
        return kResultTrue;
    }
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IPlugFrame::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IPlugFrame*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }  // owned by the plugin record
    uint32 PLUGIN_API release() override { return 1; }
};

// The block's OUTPUT parameter changes, the plugin's own, fixed-capacity: queues sized at prepare, each
// keeping its first kPts points (later ones replace the last). After the block, each queue's last value goes into
// `OutRing` for the message thread, which hands it to the controller (summoner_vst3_plugin_flush_outputs): the plugin's
// outputParameterChanges reach its controller.
class OutQueue final : public IParamValueQueue {
public:
    static constexpr int32 kPts = 16;
    ParamID id = 0;
    int32 count = 0;
    struct Pt {
        int32 offset;
        ParamValue value;
    } pts[kPts] = {};
    ParamID PLUGIN_API getParameterId() override { return id; }
    int32 PLUGIN_API getPointCount() override { return count; }
    tresult PLUGIN_API getPoint(int32 index, int32& offset, ParamValue& value) override {
        if (index < 0 || index >= count)
            return kResultFalse;
        offset = pts[index].offset;
        value = pts[index].value;
        return kResultOk;
    }
    tresult PLUGIN_API addPoint(int32 offset, ParamValue value, int32& index) override {
        index = count < kPts ? count++ : kPts - 1;
        pts[index] = Pt{offset, value};
        return kResultOk;
    }
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IParamValueQueue::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IParamValueQueue*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }
    uint32 PLUGIN_API release() override { return 1; }
};

class OutChanges final : public IParameterChanges {
public:
    // Placed at prepare in the caller's wired memory (rtLayout), never grown after.
    OutQueue* queues = nullptr;
    int32 cap = 0;
    int32 used = 0;
    void place(void* mem, int32 n) {
        queues = static_cast<OutQueue*>(mem);
        cap = n;
        for (int32 i = 0; i < n; ++i)
            new (&queues[i]) OutQueue{};
        used = 0;
    }
    int32 PLUGIN_API getParameterCount() override { return used; }
    IParamValueQueue* PLUGIN_API getParameterData(int32 index) override {
        return index >= 0 && index < used ? &queues[index] : nullptr;
    }
    IParamValueQueue* PLUGIN_API addParameterData(const ParamID& id, int32& index) override {
        for (int32 i = 0; i < used; ++i)
            if (queues[i].id == id) {
                index = i;
                return &queues[i];
            }
        if (used == cap)
            return nullptr;
        index = used;
        OutQueue& q = queues[used++];
        q.id = id;
        q.count = 0;
        return &q;
    }
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IParameterChanges::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IParameterChanges*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }
    uint32 PLUGIN_API release() override { return 1; }
};

// The output changes' last values, chain thread -> message thread: single producer (the chain thread, after its
// block), single consumer (the message thread's flush), lock-free both sides. A full ring drops and counts.
struct OutRing {
    static constexpr uint32_t kRing = 1024;
    struct Value {
        ParamID id;
        double value;
    } ring[kRing] = {};
    std::atomic<uint32_t> head{0}, tail{0};
    std::atomic<uint64_t> dropped{0};
    void push(ParamID id, double v) {
        const uint32_t h = head.load(std::memory_order_relaxed);
        if (h - tail.load(std::memory_order_acquire) >= kRing) {
            dropped.fetch_add(1, std::memory_order_relaxed);
            return;
        }
        ring[h % kRing] = Value{id, v};
        head.store(h + 1, std::memory_order_release);
    }
    bool pop(Value& out) {
        const uint32_t t = tail.load(std::memory_order_relaxed);
        if (t == head.load(std::memory_order_acquire))
            return false;
        out = ring[t % kRing];
        tail.store(t + 1, std::memory_order_release);
        return true;
    }
};

// The block's parameter changes, fixed-capacity (the SDK's host ParameterChanges grows a queue past 5 points, an
// allocation on the audio thread): one pool of points and one table of queues, both sized at prepare. The host passes
// the block's points sorted by id, stably, so each queue is a contiguous run of the pool.
class Queue final : public IParamValueQueue {
public:
    ParamID id = 0;
    const summoner_vst3_point* first = nullptr;
    int32 count = 0;
    ParamID PLUGIN_API getParameterId() override { return id; }
    int32 PLUGIN_API getPointCount() override { return count; }
    tresult PLUGIN_API getPoint(int32 index, int32& offset, ParamValue& value) override {
        if (index < 0 || index >= count)
            return kResultFalse;
        offset = first[index].offset;
        value = first[index].value;
        return kResultOk;
    }
    tresult PLUGIN_API addPoint(int32, ParamValue, int32&) override { return kResultFalse; }  // input only
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IParamValueQueue::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IParamValueQueue*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }  // owned by the plugin record
    uint32 PLUGIN_API release() override { return 1; }
};

class Changes final : public IParameterChanges {
public:
    // Placed at prepare in the caller's wired memory (rtLayout), never grown after.
    Queue* queues = nullptr;
    int32 cap = 0;
    int32 used = 0;
    void place(void* mem, int32 n) {
        queues = static_cast<Queue*>(mem);
        cap = n;
        for (int32 i = 0; i < n; ++i)
            new (&queues[i]) Queue{};
        used = 0;
    }
    // Points sorted by id (stable): one queue per run. Returns false if a run would not fit (the caps are the host's).
    bool fill(const summoner_vst3_point* points, int32 n) {
        used = 0;
        for (int32 i = 0; i < n;) {
            int32 j = i + 1;
            while (j < n && points[j].id == points[i].id)
                ++j;
            if (used == cap)
                return false;
            Queue& q = queues[used++];
            q.id = points[i].id;
            q.first = points + i;
            q.count = j - i;
            i = j;
        }
        return true;
    }
    int32 PLUGIN_API getParameterCount() override { return used; }
    IParamValueQueue* PLUGIN_API getParameterData(int32 index) override {
        return index >= 0 && index < used ? &queues[index] : nullptr;
    }
    IParamValueQueue* PLUGIN_API addParameterData(const ParamID&, int32&) override { return nullptr; }  // input only
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IParameterChanges::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IParameterChanges*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }
    uint32 PLUGIN_API release() override { return 1; }
};

}  // namespace

// The host context every plugin is initialized with: ONE for the process. The SDK's HostApplication is a singleton
// by design (its addRef and release both return 1, hostclasses.cpp): one per module, released through an IPtr, was
// never freed, a leak of 848 bytes per module open (`leaks` on a churned child showed one root per VST3 load).
// Function-local static: built once, thread-safely, never destroyed (plugins may still hold it at exit).
static HostApplication* sharedHost() {
    static HostApplication* const host = new HostApplication();
    return host;
}

struct summoner_vst3_module {
    CFBundleRef bundle = nullptr;
    BundleExitFunc exit = nullptr;
    IPtr<IPluginFactory> factory;
    HostApplication* host = sharedHost();
};

// The block's events, a fixed-capacity IEventList over memory placed at prepare (the SDK's EventList allocates its
// own array), never grown after.
class FixedEvents final : public IEventList {
public:
    Event* ev = nullptr;
    int32 cap = 0;
    int32 count = 0;
    void place(void* mem, int32 n) {
        ev = static_cast<Event*>(mem);
        cap = n;
        count = 0;
    }
    void clear() { count = 0; }
    int32 PLUGIN_API getEventCount() override { return count; }
    tresult PLUGIN_API getEvent(int32 index, Event& e) override {
        if (index < 0 || index >= count)
            return kResultFalse;
        e = ev[index];
        return kResultOk;
    }
    tresult PLUGIN_API addEvent(Event& e) override {
        if (count >= cap)
            return kResultFalse;
        ev[count++] = e;
        return kResultOk;
    }
    tresult PLUGIN_API queryInterface(const TUID iid, void** obj) override {
        if (FUnknownPrivate::iidEqual(iid, IEventList::iid) || FUnknownPrivate::iidEqual(iid, FUnknown::iid)) {
            *obj = static_cast<IEventList*>(this);
            return kResultOk;
        }
        *obj = nullptr;
        return kNoInterface;
    }
    uint32 PLUGIN_API addRef() override { return 1; }  // owned by the plugin record
    uint32 PLUGIN_API release() override { return 1; }
};

// Where prepare places the process call's state in the caller's memory: each part aligned to its type.
struct RtLayout {
    size_t queues, outQueues, events, ring, total;
};

static size_t alignUp(size_t x, size_t a) {
    return (x + a - 1) / a * a;
}

static RtLayout rtLayout(int32 points, int32 notes) {
    RtLayout l{};
    size_t at = 0;
    l.queues = alignUp(at, alignof(Queue));
    at = l.queues + sizeof(Queue) * size_t(points);
    l.outQueues = alignUp(at, alignof(OutQueue));
    at = l.outQueues + sizeof(OutQueue) * size_t(points);
    l.events = alignUp(at, alignof(Event));
    at = l.events + sizeof(Event) * size_t(notes);
    l.ring = alignUp(at, alignof(OutRing));
    l.total = l.ring + sizeof(OutRing);
    return l;
}

struct summoner_vst3_plugin {
    summoner_vst3_module* module = nullptr;
    IPtr<IComponent> component;
    IPtr<IAudioProcessor> processor;
    IPtr<IEditController> controller;
    bool separateController = false;
    // The two connection points, connected to each other directly (no ConnectionProxy: it needs the SDK's
    // Objective-C++ thread checker, and the host already keeps every call on its message thread).
    IPtr<IConnectionPoint> componentCP, controllerCP;
    ComponentHandler handler;
    IPtr<IPlugView> view;  // the open editor's view, or null
    PlugFrame frame;
    // The process call's queues, events and output ring live in the wired memory
    // the Rust side hands to prepare (a LockedRegion it owns for as long as this record); the record holds the views.
    Changes changes;
    OutChanges outChanges;
    OutRing* outRing = nullptr;
    FixedEvents events;
    ProcessContext context{};
    double rate = 48000.0;
    int32_t maxFrames = 0;
    // kOffline for a render set's slot (set by prepare), kRealtime for every live one. Kept for
    // reactivate and every process call.
    int32_t processMode = kRealtime;
    bool active = false;
};

extern "C" {

int summoner_vst3_module_open(const char* abs_path, summoner_vst3_module** out, char* err, size_t cap) noexcept {
    return guarded([&] {
        *out = nullptr;
        if (abs_path == nullptr || abs_path[0] != '/') {
            setErr(err, cap, "not an absolute path");
            return SUMMONER_VST3_REFUSED;
        }
        auto m = std::make_unique<summoner_vst3_module>();
        CFURLRef url = CFURLCreateFromFileSystemRepresentation(kCFAllocatorDefault,
                                                               reinterpret_cast<const UInt8*>(abs_path),
                                                               CFIndex(std::strlen(abs_path)), true);
        if (url == nullptr) {
            setErr(err, cap, "cannot make a URL of the path");
            return SUMMONER_VST3_FAILED;
        }
        m->bundle = CFBundleCreate(kCFAllocatorDefault, url);
        CFRelease(url);
        CFErrorRef e = nullptr;
        if (m->bundle == nullptr || !CFBundleLoadExecutableAndReturnError(m->bundle, &e)) {
            std::string why = "cannot load the bundle";
            if (e != nullptr) {
                if (CFStringRef s = CFErrorCopyDescription(e)) {
                    char buf[512];
                    if (CFStringGetCString(s, buf, sizeof(buf), kCFStringEncodingUTF8))
                        why = buf;
                    CFRelease(s);
                }
                CFRelease(e);
            }
            if (m->bundle != nullptr)
                CFRelease(m->bundle);
            setErr(err, cap, why);
            return SUMMONER_VST3_FAILED;
        }
        auto fn = [&](const char* name) {
            CFStringRef s = CFStringCreateWithCString(kCFAllocatorDefault, name, kCFStringEncodingASCII);
            void* p = CFBundleGetFunctionPointerForName(m->bundle, s);
            CFRelease(s);
            return p;
        };
        auto entry = reinterpret_cast<BundleEntryFunc>(fn("bundleEntry"));
        m->exit = reinterpret_cast<BundleExitFunc>(fn("bundleExit"));
        auto getFactory = reinterpret_cast<GetFactoryProc>(fn("GetPluginFactory"));
        if (entry == nullptr || m->exit == nullptr || getFactory == nullptr) {
            CFRelease(m->bundle);
            setErr(err, cap, "the bundle lacks bundleEntry, bundleExit or GetPluginFactory");
            return SUMMONER_VST3_FAILED;
        }
        if (!entry(m->bundle)) {
            CFRelease(m->bundle);
            setErr(err, cap, "bundleEntry failed");
            return SUMMONER_VST3_FAILED;
        }
        m->factory = owned(getFactory());
        if (!m->factory) {
            m->exit();
            CFRelease(m->bundle);
            setErr(err, cap, "GetPluginFactory returned null");
            return SUMMONER_VST3_FAILED;
        }
        *out = m.release();
        return SUMMONER_VST3_OK;
    });
}

void summoner_vst3_module_close(summoner_vst3_module* m) noexcept {
    if (m == nullptr)
        return;
    try {
        m->factory = nullptr;
        if (m->exit != nullptr)
            m->exit();
        if (m->bundle != nullptr)
            CFRelease(m->bundle);
    } catch (...) {
    }
    delete m;
}

int32_t summoner_vst3_class_count(const summoner_vst3_module* m) noexcept {
    try {
        return m != nullptr && m->factory ? m->factory->countClasses() : 0;
    } catch (...) {
        return 0;
    }
}

int summoner_vst3_class_get(const summoner_vst3_module* m, int32_t index, summoner_vst3_class* out) noexcept {
    return guarded([&] {
        std::memset(out, 0, sizeof(*out));
        PClassInfo info{};
        if (m->factory->getClassInfo(index, &info) != kResultOk)
            return SUMMONER_VST3_FAILED;
        std::memcpy(out->cid, info.cid, 16);
        out->cardinality = info.cardinality;
        copyStr(out->category, sizeof(out->category), info.category);
        copyStr(out->name, sizeof(out->name), info.name);
        FUnknownPtr<IPluginFactory2> f2(m->factory);
        PClassInfo2 i2{};
        if (f2 && f2->getClassInfo2(index, &i2) == kResultOk) {
            out->class_flags = i2.classFlags;
            copyStr(out->vendor, sizeof(out->vendor), i2.vendor);
            copyStr(out->version, sizeof(out->version), i2.version);
            copyStr(out->sdk_version, sizeof(out->sdk_version), i2.sdkVersion);
            copyStr(out->sub_categories, sizeof(out->sub_categories), i2.subCategories);
        }
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_factory_vendor(const summoner_vst3_module* m, char* out, size_t cap) noexcept {
    return guarded([&] {
        if (cap > 0)
            out[0] = 0;
        PFactoryInfo info{};
        if (m->factory->getFactoryInfo(&info) != kResultOk)
            return SUMMONER_VST3_FAILED;
        copyStr(out, cap, info.vendor);
        return SUMMONER_VST3_OK;
    });
}

// --- The default channel count -----------------------------------------------
#include "default_channels.h"

int32_t summoner_vst3_plugin_default_channels(const summoner_vst3_plugin* p, int32_t dir) SUMMONER_NOEXCEPT
{
    // Null record or component: 0, and nothing is called.
    if (p == nullptr || p->component == nullptr)
        return 0;

    // Not run through `guarded`: an exception here must give 0, not SUMMONER_VST3_THREW.
    try {
        const Steinberg::Vst::BusDirection direction =
            dir == 0 ? Steinberg::Vst::kInput : Steinberg::Vst::kOutput;
        return clamp_channel_total(sum_default_active_audio(p->component, direction));
    } catch (...) {
        return 0;
    }
}

int summoner_vst3_plugin_create(summoner_vst3_module* m, const uint8_t cid[16], summoner_vst3_plugin** out, char* err,
                           size_t cap) noexcept {
    return guarded([&] {
        *out = nullptr;
        auto p = std::make_unique<summoner_vst3_plugin>();
        p->module = m;
        TUID id;
        std::memcpy(id, cid, 16);
        IComponent* c = nullptr;
        if (m->factory->createInstance(id, IComponent::iid, reinterpret_cast<void**>(&c)) != kResultOk || !c) {
            setErr(err, cap, "createInstance (IComponent) failed");
            return SUMMONER_VST3_FAILED;
        }
        p->component = owned(c);
        if (p->component->initialize(static_cast<IHostApplication*>(m->host)) != kResultOk) {
            setErr(err, cap, "IComponent::initialize failed");
            return SUMMONER_VST3_FAILED;
        }
        p->processor = FUnknownPtr<IAudioProcessor>(p->component);
        if (!p->processor) {
            p->component->terminate();
            setErr(err, cap, "not an IAudioProcessor");
            return SUMMONER_VST3_FAILED;
        }
        TUID ccid;
        if (p->component->getControllerClassId(ccid) == kResultOk && FUID::fromTUID(ccid).isValid()) {
            IEditController* ec = nullptr;
            if (m->factory->createInstance(ccid, IEditController::iid, reinterpret_cast<void**>(&ec)) == kResultOk
                && ec) {
                p->controller = owned(ec);
                p->separateController = true;
                if (p->controller->initialize(static_cast<IHostApplication*>(m->host)) != kResultOk) {
                    p->controller = nullptr;
                    p->separateController = false;
                }
            }
        }
        if (!p->controller)
            p->controller = FUnknownPtr<IEditController>(p->component);  // a single-component plugin
        if (p->controller) {
            p->handler.controller = p->controller.get();
            p->controller->setComponentHandler(&p->handler);
            if (p->separateController) {
                FUnknownPtr<IConnectionPoint> a(p->component), b(p->controller);
                if (a && b) {
                    p->componentCP = a;
                    p->controllerCP = b;
                    a->connect(b);
                    b->connect(a);
                }
            }
            // Sync the controller to the component's (default) state, as the SDK's PlugProvider does.
            MemoryStream s;
            if (p->component->getState(&s) == kResultOk) {
                s.seek(0, IBStream::kIBSeekSet, nullptr);
                p->controller->setComponentState(&s);
            }
        }
        *out = p.release();
        return SUMMONER_VST3_OK;
    });
}

void summoner_vst3_plugin_destroy(summoner_vst3_plugin* p) noexcept {
    if (p == nullptr)
        return;
    try {
        if (p->active) {
            p->processor->setProcessing(false);
            p->component->setActive(false);
        }
        if (p->componentCP && p->controllerCP) {
            p->componentCP->disconnect(p->controllerCP);
            p->controllerCP->disconnect(p->componentCP);
        }
        p->componentCP = nullptr;
        p->controllerCP = nullptr;
        if (p->controller) {
            p->controller->setComponentHandler(nullptr);
            p->handler.controller = nullptr;
            if (p->separateController)
                p->controller->terminate();
        }
        p->controller = nullptr;
        p->processor = nullptr;
        if (p->component)
            p->component->terminate();
        p->component = nullptr;
    } catch (...) {
    }
    delete p;
}

int summoner_vst3_plugin_buses(summoner_vst3_plugin* p, int32_t in_ch, int32_t out_ch, int32_t* got_in, int32_t* got_out,
                          int32_t* has_events) noexcept {
    return guarded([&] {
        auto arr = [](int32_t n) { return n == 1 ? SpeakerArr::kMono : n == 2 ? SpeakerArr::kStereo : 0; };
        const int32 nIn = p->component->getBusCount(kAudio, kInput);
        const int32 nOut = p->component->getBusCount(kAudio, kOutput);
        SpeakerArrangement ins[1] = {arr(in_ch)}, outs[1] = {arr(out_ch)};
        p->processor->setBusArrangements(ins, nIn > 0 ? 1 : 0, outs, nOut > 0 ? 1 : 0);
        auto channels = [&](BusDirection d) -> int32_t {
            if (p->component->getBusCount(kAudio, d) < 1)
                return 0;
            SpeakerArrangement a = 0;
            p->processor->getBusArrangement(d, 0, a);
            p->component->activateBus(kAudio, d, 0, true);
            return SpeakerArr::getChannelCount(a);
        };
        *got_in = channels(kInput);
        *got_out = channels(kOutput);
        *has_events = p->component->getBusCount(kEvent, kInput) > 0 ? 1 : 0;
        if (*has_events)
            p->component->activateBus(kEvent, kInput, 0, true);
        return SUMMONER_VST3_OK;
    });
}

int32_t summoner_vst3_shim_abi(void) noexcept {  // 3: the placed RT state (summoner_vst3_rt_bytes, prepare's rt / rt_len)
    return SUMMONER_VST3_SHIM_ABI;
}

size_t summoner_vst3_rt_bytes(int32_t max_points, int32_t max_notes) noexcept {
    return rtLayout(std::max(max_points, 1), std::max(max_notes, 1)).total;
}

int summoner_vst3_plugin_prepare(summoner_vst3_plugin* p, double rate, int32_t max_frames, int32_t max_points,
                            int32_t max_notes, int32_t offline, void* rt, size_t rt_len) noexcept {
    return guarded([&] {
        const RtLayout lay = rtLayout(std::max(max_points, 1), std::max(max_notes, 1));
        if (rt == nullptr || rt_len < lay.total)
            return SUMMONER_VST3_REFUSED;  // nothing placed: the record still points at its previous memory
        if (p->active) {
            p->processor->setProcessing(false);
            p->component->setActive(false);
            p->active = false;
        }
        // The output values still in the old ring reach the controller before the ring moves (main thread).
        if (p->outRing != nullptr) {
            OutRing::Value v{};
            while (p->outRing->pop(v))
                if (p->controller)
                    p->controller->setParamNormalized(v.id, v.value);
        }
        auto* base = static_cast<uint8_t*>(rt);
        p->changes.place(base + lay.queues, std::max(max_points, 1));
        p->outChanges.place(base + lay.outQueues, std::max(max_points, 1));
        p->events.place(base + lay.events, std::max(max_notes, 1));
        p->outRing = new (base + lay.ring) OutRing{};
        const int32_t mode = offline ? kOffline : kRealtime;
        ProcessSetup setup{mode, kSample32, max_frames, rate};
        if (p->processor->canProcessSampleSize(kSample32) != kResultTrue
            || p->processor->setupProcessing(setup) != kResultOk)
            return SUMMONER_VST3_FAILED;
        p->rate = rate;
        p->maxFrames = max_frames;
        p->processMode = mode;
        if (p->component->setActive(true) != kResultOk)
            return SUMMONER_VST3_FAILED;
        p->active = true;
        p->processor->setProcessing(true);  // kNotImplemented is allowed
        return SUMMONER_VST3_OK;
    });
}

// --- The load-time processing setup ------------------------------------------
int summoner_vst3_plugin_setup_only(summoner_vst3_plugin* p, double rate, int32_t max_frames) SUMMONER_NOEXCEPT
{
    return guarded([&]() -> int {
        if (p->active)
            return SUMMONER_VST3_FAILED;

        Steinberg::Vst::ProcessSetup setup {};
        setup.processMode        = Steinberg::Vst::kRealtime;
        setup.symbolicSampleSize = Steinberg::Vst::kSample32;
        setup.maxSamplesPerBlock = max_frames;
        setup.sampleRate         = rate;

        const Steinberg::tresult r = p->processor->setupProcessing(setup);
        return r == Steinberg::kResultOk ? SUMMONER_VST3_OK : SUMMONER_VST3_FAILED;
    });
}

int summoner_vst3_plugin_release(summoner_vst3_plugin* p) noexcept {
    return guarded([&] {
        if (p->active) {
            p->processor->setProcessing(false);
            p->component->setActive(false);
            p->active = false;
        }
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_plugin_reactivate(summoner_vst3_plugin* p) noexcept {
    return guarded([&] {
        if (p->maxFrames <= 0)
            return SUMMONER_VST3_FAILED;
        if (p->active) {
            p->processor->setProcessing(false);
            p->component->setActive(false);
            p->active = false;
        }
        ProcessSetup setup{p->processMode, kSample32, p->maxFrames, p->rate};
        if (p->processor->setupProcessing(setup) != kResultOk || p->component->setActive(true) != kResultOk)
            return SUMMONER_VST3_FAILED;
        p->active = true;
        p->processor->setProcessing(true);
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_plugin_process(summoner_vst3_plugin* p, const summoner_vst3_block* b) noexcept {
    return guarded([&] {
        if (!p->changes.fill(b->points, b->num_points))
            return SUMMONER_VST3_FAILED;
        p->events.clear();
        for (int32_t i = 0; i < b->num_notes; ++i) {
            const auto& n = b->notes[i];
            Event e{};
            fill_event(n, e);
            p->events.addEvent(e);
        }
        const auto& t = *b->transport;
        ProcessContext& c = p->context;
        c = ProcessContext{};
        c.sampleRate = p->rate;
        c.projectTimeSamples = t.project_samples;
        c.projectTimeMusic = t.ppq;
        c.state = ProcessContext::kProjectTimeMusicValid;
        if (t.playing) c.state |= ProcessContext::kPlaying;
        if (t.cycle_active) c.state |= ProcessContext::kCycleActive;
        if (t.recording) c.state |= ProcessContext::kRecording;
        if (t.tempo_valid) { c.state |= ProcessContext::kTempoValid; c.tempo = t.tempo; }
        if (t.time_sig_valid) {
            c.state |= ProcessContext::kTimeSigValid;
            c.timeSigNumerator = t.time_sig_num;
            c.timeSigDenominator = t.time_sig_den;
        }
        if (t.bar_valid) { c.state |= ProcessContext::kBarPositionValid; c.barPositionMusic = t.bar_ppq; }
        if (t.cycle_valid) {
            c.state |= ProcessContext::kCycleValid;
            c.cycleStartMusic = t.cycle_start_ppq;
            c.cycleEndMusic = t.cycle_end_ppq;
        }
        if (t.system_time_valid) { c.state |= ProcessContext::kSystemTimeValid; c.systemTime = t.system_time_ns; }

        AudioBusBuffers in{}, out{};
        in.numChannels = b->num_inputs;
        in.channelBuffers32 = const_cast<float**>(b->inputs);
        out.numChannels = b->num_outputs;
        out.channelBuffers32 = const_cast<float**>(b->outputs);
        ProcessData d{};
        d.processMode = p->processMode;
        d.symbolicSampleSize = kSample32;
        d.numSamples = b->frames;
        d.numInputs = b->num_inputs > 0 ? 1 : 0;
        d.numOutputs = b->num_outputs > 0 ? 1 : 0;
        d.inputs = b->num_inputs > 0 ? &in : nullptr;
        d.outputs = b->num_outputs > 0 ? &out : nullptr;
        d.inputParameterChanges = &p->changes;
        d.outputParameterChanges = &p->outChanges;
        d.inputEvents = &p->events;
        d.processContext = &c;
        p->outChanges.used = 0;
        const tresult r = p->processor->process(d);
        for (int32 i = 0; i < p->outChanges.used; ++i) {
            const OutQueue& q = p->outChanges.queues[i];
            if (q.count > 0)
                p->outRing->push(q.id, double(float(q.pts[q.count - 1].value)));  // rounded to a float
        }
        return r == kResultOk ? SUMMONER_VST3_OK : SUMMONER_VST3_FAILED;
    });
}

uint32_t summoner_vst3_plugin_latency(summoner_vst3_plugin* p) noexcept {
    try {
        return p->processor->getLatencySamples();
    } catch (...) {
        return 0;
    }
}

int32_t summoner_vst3_plugin_take_restart(summoner_vst3_plugin* p) noexcept {
    return p->handler.restart.exchange(0, std::memory_order_acq_rel);
}

int summoner_vst3_controller_set_param(summoner_vst3_plugin* p, uint32_t id, double value) noexcept {
    return guarded([&] {
        if (!p->controller)
            return SUMMONER_VST3_FAILED;
        return p->controller->setParamNormalized(ParamID(id), value) == kResultOk ? SUMMONER_VST3_OK : SUMMONER_VST3_FAILED;
    });
}

int summoner_vst3_editor_create(summoner_vst3_plugin* p, int32_t* w, int32_t* h, int32_t* resizable) noexcept {
    return guarded([&] {
        if (!p->controller || p->view)
            return SUMMONER_VST3_REFUSED;
        IPlugView* v = p->controller->createView(ViewType::kEditor);
        if (v == nullptr)
            return SUMMONER_VST3_REFUSED;
        IPtr<IPlugView> view = owned(v);
        if (view->isPlatformTypeSupported(kPlatformTypeNSView) != kResultTrue)
            return SUMMONER_VST3_REFUSED;
        ViewRect r{};
        if (view->getSize(&r) != kResultOk)
            r = ViewRect(0, 0, 400, 300);
        *w = std::max(r.getWidth(), 10);
        *h = std::max(r.getHeight(), 10);
        *resizable = view->canResize() == kResultTrue ? 1 : 0;
        p->view = view;
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_editor_attach(summoner_vst3_plugin* p, void* nsview, void* set_size, void* get_size, void* window) noexcept {
    return guarded([&] {
        if (!p->view || nsview == nullptr)
            return SUMMONER_VST3_FAILED;
        p->frame.view = p->view.get();
        p->frame.setSize = reinterpret_cast<SetSizeFn>(set_size);
        p->frame.getSize = reinterpret_cast<GetSizeFn>(get_size);
        p->frame.window = window;
        p->view->setFrame(&p->frame);
        return p->view->attached(nsview, kPlatformTypeNSView) == kResultOk ? SUMMONER_VST3_OK : SUMMONER_VST3_FAILED;
    });
}

int summoner_vst3_editor_user_resize(void* ctx, int phase, double* w, double* h) noexcept {
    auto* p = static_cast<summoner_vst3_plugin*>(ctx);
    try {
        if (!p->view)
            return 0;
        ViewRect r(0, 0, int32(*w), int32(*h));
        if (phase == 0) {
            if (p->view->canResize() != kResultTrue)
                return 0;
            p->view->checkSizeConstraint(&r);
            *w = r.getWidth();
            *h = r.getHeight();
            return 1;
        }
        p->frame.inOnSize = true;
        p->view->onSize(&r);
        p->frame.inOnSize = false;
        return 1;
    } catch (...) {
        p->frame.inOnSize = false;
        return 0;
    }
}

int summoner_vst3_editor_close(summoner_vst3_plugin* p) noexcept {
    const int r = guarded([&] {
        if (!p->view)
            return SUMMONER_VST3_REFUSED;
        p->view->removed();
        p->view->setFrame(nullptr);
        return SUMMONER_VST3_OK;
    });
    p->view = nullptr;  // released here, after removed()
    p->frame = PlugFrame{};
    return r;
}

int32_t summoner_vst3_plugin_flush_outputs(summoner_vst3_plugin* p) noexcept {
    int32_t n = 0;
    try {
        OutRing::Value v{};
        if (p->outRing == nullptr)
            return 0;  // never prepared: no ring yet
        while (p->outRing->pop(v)) {
            if (p->controller)
                p->controller->setParamNormalized(v.id, v.value);
            ++n;
        }
    } catch (...) {
        return -1;
    }
    return n;
}

uint64_t summoner_vst3_plugin_edit_count(summoner_vst3_plugin* p) noexcept {
    return p->handler.edits.load(std::memory_order_acquire);
}

int summoner_vst3_plugin_take_edit(summoner_vst3_plugin* p, uint32_t* id, double* value) noexcept {
    ComponentHandler::Edit e{};
    if (!p->handler.pop(e))
        return 0;
    *id = e.id;
    *value = e.value;
    return 1;
}

int32_t summoner_vst3_param_count(summoner_vst3_plugin* p) noexcept {
    try {
        return p->controller ? p->controller->getParameterCount() : 0;
    } catch (...) {
        return 0;
    }
}

int summoner_vst3_param_get(summoner_vst3_plugin* p, int32_t index, summoner_vst3_param* out) noexcept {
    return guarded([&] {
        std::memset(out, 0, sizeof(*out));
        ParameterInfo info{};
        if (!p->controller || p->controller->getParameterInfo(index, info) != kResultOk)
            return SUMMONER_VST3_FAILED;
        out->id = info.id;
        copyStr16(out->title, sizeof(out->title), info.title, sizeof(info.title) / sizeof(info.title[0]));
        out->default_normalized = info.defaultNormalizedValue;
        out->step_count = info.stepCount;
        out->flags = info.flags;
        copyStr16(out->units, sizeof(out->units), info.units, sizeof(info.units) / sizeof(info.units[0]));
        out->unit_id = info.unitId;
        return SUMMONER_VST3_OK;
    });
}

int32_t summoner_vst3_unit_count(summoner_vst3_plugin* p) noexcept {
    try {
        FUnknownPtr<IUnitInfo> ui(p->controller);
        return ui ? ui->getUnitCount() : 0;
    } catch (...) {
        return 0;
    }
}

int summoner_vst3_unit_get(summoner_vst3_plugin* p, int32_t index, int32_t* id, int32_t* parent) noexcept {
    return guarded([&] {
        *id = 0;
        *parent = 0;
        FUnknownPtr<IUnitInfo> ui(p->controller);
        UnitInfo u{};
        if (!ui || ui->getUnitInfo(index, u) != kResultOk)
            return SUMMONER_VST3_FAILED;
        *id = u.id;
        *parent = u.parentUnitId;
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_controller_get_param(summoner_vst3_plugin* p, uint32_t id, double* value) noexcept {
    return guarded([&] {
        *value = 0.0;
        if (!p->controller)
            return SUMMONER_VST3_FAILED;
        *value = p->controller->getParamNormalized(ParamID(id));
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_midi_mapping(summoner_vst3_plugin* p, int16_t channel, int16_t cc, uint32_t* id) noexcept {
    return guarded([&] {
        FUnknownPtr<IMidiMapping> mm(p->controller);
        ParamID pid = 0;
        if (!mm || mm->getMidiControllerAssignment(0, channel, CtrlNumber(cc), pid) != kResultTrue)
            return SUMMONER_VST3_FAILED;
        *id = pid;
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_state_get(summoner_vst3_plugin* p, int32_t which, uint8_t** data, size_t* len) noexcept {
    return guarded([&] {
        *data = nullptr;
        *len = 0;
        MemoryStream s;
        tresult r = kResultFalse;
        if (which == 0)
            r = p->component->getState(&s);
        else if (p->controller)
            r = p->controller->getState(&s);
        if (r != kResultOk)
            return SUMMONER_VST3_FAILED;
        const size_t n = size_t(s.getSize());
        auto* buf = static_cast<uint8_t*>(std::malloc(n > 0 ? n : 1));
        if (buf == nullptr)
            return SUMMONER_VST3_FAILED;
        if (n > 0)
            std::memcpy(buf, s.getData(), n);
        *data = buf;
        *len = n;
        return SUMMONER_VST3_OK;
    });
}

int summoner_vst3_state_set(summoner_vst3_plugin* p, int32_t which, const uint8_t* data, size_t len) noexcept {
    return guarded([&] {
        MemoryStream s(const_cast<uint8_t*>(data), TSize(len));
        if (which == 0) {
            if (p->component->setState(&s) != kResultOk)
                return SUMMONER_VST3_REFUSED;
            if (p->controller) {
                s.seek(0, IBStream::kIBSeekSet, nullptr);
                p->controller->setComponentState(&s);
            }
            return SUMMONER_VST3_OK;
        }
        if (!p->controller)
            return SUMMONER_VST3_FAILED;
        return p->controller->setState(&s) == kResultOk ? SUMMONER_VST3_OK : SUMMONER_VST3_REFUSED;
    });
}

void summoner_vst3_free(void* data) noexcept {
    std::free(data);
}

}  // extern "C"
