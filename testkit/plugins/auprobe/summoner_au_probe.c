// The AU registrar probe: does an AU registered IN-PROCESS reach the system registrar?
//
// It contains its own AU v2 component (a generator that renders a known ramp), registers it with
// AudioComponentRegister (documented as in-process only), and instantiates, initializes, renders and disposes it. It
// never calls AudioComponentFindNext, AudioComponentCount or anything that enumerates; it reads no file.
//
// It links NOTHING but libSystem: on macOS 26 CoreFoundation's own load closure includes AudioToolbox,
// AudioToolboxCore and CoreAudio (seen with DYLD_PRINT_LIBRARIES), so a CoreFoundation-linked probe would load them,
// initializers and all, at process start. Here no audio or CoreFoundation image is loaded before the release:
// AudioToolbox (and with it CoreFoundation) is dlopen'ed as the first act of the au window, and CoreFoundation's
// functions are reached by dlsym.
//
// Run under the lookup recorder (testkit/plugins/lookwatch) and with a system-wide fs_usage. Phases are marked in the
// recorder's log by looking up non-existent service names ("sas.lab.phase.<name>"; the lookups fail, harmlessly):
//   au-begin … au-end             loading AudioToolbox, and every AudioToolbox call;
//   control-begin … control-end   then the positive control: a preferences read, which CoreFoundation serves through
//                                 cfprefsd: a framework's own lookup, made in the probe's call path in the window
//                                 (notifyd, the first choice, is already connected at process start).
//
//   summoner_au_probe [--hold <marker> [--hold-secs <n>]] [--pid-file <path>] [--control-only]
#include <AudioToolbox/AudioToolbox.h>  // types and constants only: nothing is linked
#include <dlfcn.h>
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int failures = 0;
static void check(const char* name, bool ok, const char* why) {
    printf("check %s %s%s%s\n", name, ok ? "ok" : "FAIL", ok ? "" : " ", ok ? "" : why);
    if (!ok)
        ++failures;
}

static void mark(const char* phase) {
    char name[128];
    snprintf(name, sizeof(name), "sas.lab.phase.%s", phase);
    mach_port_t p = MACH_PORT_NULL;
    (void)bootstrap_look_up(bootstrap_port, name, &p);  // fails (no such service): a marker in the recorder's log
}

// --- the in-binary component: a stereo Float32 generator rendering i / 64 - 0.5 ----------------------------------

typedef struct {
    AudioComponentPlugInInterface iface;  // first: the toolbox passes this pointer back as `self`
    AudioComponentInstance instance;
    Float64 rate;
    UInt32 maxFrames;
    bool initialized;
} Probe;

static AudioStreamBasicDescription format(Float64 rate) {
    AudioStreamBasicDescription f = {0};
    f.mSampleRate = rate;
    f.mFormatID = kAudioFormatLinearPCM;
    f.mFormatFlags = kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved;
    f.mBytesPerPacket = 4;
    f.mFramesPerPacket = 1;
    f.mBytesPerFrame = 4;
    f.mChannelsPerFrame = 2;
    f.mBitsPerChannel = 32;
    return f;
}

static OSStatus p_open(void* self, AudioComponentInstance instance) {
    Probe* p = (Probe*)self;
    p->instance = instance;
    p->rate = 48000.0;
    p->maxFrames = 4096;
    return noErr;
}
static OSStatus p_close(void* self) {
    free(self);
    return noErr;
}
static OSStatus p_initialize(void* self) {
    ((Probe*)self)->initialized = true;
    return noErr;
}
static OSStatus p_uninitialize(void* self) {
    ((Probe*)self)->initialized = false;
    return noErr;
}
static OSStatus p_get_property_info(void* self, AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement elem,
                                    UInt32* outSize, Boolean* outWritable) {
    (void)self, (void)scope, (void)elem;
    UInt32 size = 0;
    if (id == kAudioUnitProperty_StreamFormat)
        size = sizeof(AudioStreamBasicDescription);
    else if (id == kAudioUnitProperty_MaximumFramesPerSlice)
        size = sizeof(UInt32);
    else
        return kAudioUnitErr_InvalidProperty;
    if (outSize)
        *outSize = size;
    if (outWritable)
        *outWritable = true;
    return noErr;
}
static OSStatus p_get_property(void* self, AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement elem,
                               void* out, UInt32* ioSize) {
    Probe* p = (Probe*)self;
    (void)scope, (void)elem;
    if (id == kAudioUnitProperty_StreamFormat && *ioSize >= sizeof(AudioStreamBasicDescription)) {
        AudioStreamBasicDescription f = format(p->rate);
        memcpy(out, &f, sizeof(f));
        *ioSize = sizeof(f);
        return noErr;
    }
    if (id == kAudioUnitProperty_MaximumFramesPerSlice && *ioSize >= sizeof(UInt32)) {
        memcpy(out, &p->maxFrames, sizeof(UInt32));
        *ioSize = sizeof(UInt32);
        return noErr;
    }
    return kAudioUnitErr_InvalidProperty;
}
static OSStatus p_set_property(void* self, AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement elem,
                               const void* in, UInt32 size) {
    Probe* p = (Probe*)self;
    (void)scope, (void)elem;
    if (id == kAudioUnitProperty_StreamFormat && size >= sizeof(AudioStreamBasicDescription)) {
        p->rate = ((const AudioStreamBasicDescription*)in)->mSampleRate;
        return noErr;
    }
    if (id == kAudioUnitProperty_MaximumFramesPerSlice && size >= sizeof(UInt32)) {
        memcpy(&p->maxFrames, in, sizeof(UInt32));
        return noErr;
    }
    return kAudioUnitErr_InvalidProperty;
}
static OSStatus p_reset(void* self, AudioUnitScope scope, AudioUnitElement elem) {
    (void)self, (void)scope, (void)elem;
    return noErr;
}
static OSStatus p_render(void* self, AudioUnitRenderActionFlags* flags, const AudioTimeStamp* ts, UInt32 bus,
                         UInt32 frames, AudioBufferList* io) {
    (void)self, (void)flags, (void)ts, (void)bus;
    for (UInt32 b = 0; b < io->mNumberBuffers; ++b) {
        float* d = (float*)io->mBuffers[b].mData;
        if (d == NULL)
            return kAudio_ParamError;
        for (UInt32 i = 0; i < frames; ++i)
            d[i] = (float)i / 64.0f - 0.5f;
    }
    return noErr;
}
static AudioComponentMethod p_lookup(SInt16 selector) {
    switch (selector) {
        case kAudioUnitInitializeSelect: return (AudioComponentMethod)p_initialize;
        case kAudioUnitUninitializeSelect: return (AudioComponentMethod)p_uninitialize;
        case kAudioUnitGetPropertyInfoSelect: return (AudioComponentMethod)p_get_property_info;
        case kAudioUnitGetPropertySelect: return (AudioComponentMethod)p_get_property;
        case kAudioUnitSetPropertySelect: return (AudioComponentMethod)p_set_property;
        case kAudioUnitResetSelect: return (AudioComponentMethod)p_reset;
        case kAudioUnitRenderSelect: return (AudioComponentMethod)p_render;
        default: return NULL;
    }
}
static AudioComponentPlugInInterface* p_factory(const AudioComponentDescription* desc) {
    (void)desc;
    Probe* p = (Probe*)calloc(1, sizeof(Probe));
    if (p == NULL)
        return NULL;
    p->iface.Open = p_open;
    p->iface.Close = p_close;
    p->iface.Lookup = p_lookup;
    p->iface.reserved = NULL;
    return &p->iface;
}

// --- the host side: AudioToolbox through dlsym only --------------------------------------------------------------

typedef AudioComponent (*RegisterFn)(const AudioComponentDescription*, CFStringRef, UInt32,
                                     AudioComponentFactoryFunction);
typedef OSStatus (*InstanceNewFn)(AudioComponent, AudioComponentInstance*);
typedef OSStatus (*InstanceDisposeFn)(AudioComponentInstance);
typedef OSStatus (*UnitFn)(AudioUnit);
typedef OSStatus (*RenderFn)(AudioUnit, AudioUnitRenderActionFlags*, const AudioTimeStamp*, UInt32, UInt32,
                             AudioBufferList*);
typedef CFStringRef (*CFStringCreateFn)(CFAllocatorRef, const char*, CFStringEncoding);
typedef CFPropertyListRef (*CFPrefsCopyFn)(CFStringRef, CFStringRef);
typedef void (*CFReleaseFn)(CFTypeRef);

static void* at = NULL;  // AudioToolbox, once loaded
static CFStringCreateFn cf_string;
static CFReleaseFn cf_release;

static void au_phase(void) {
    mark("au-begin");
    at = dlopen("/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox", RTLD_NOW | RTLD_LOCAL);
    check("audiotoolbox_loaded", at != NULL, dlerror());
    if (at == NULL) {
        mark("au-end");
        return;
    }
    cf_string = (CFStringCreateFn)dlsym(RTLD_DEFAULT, "CFStringCreateWithCString");
    cf_release = (CFReleaseFn)dlsym(RTLD_DEFAULT, "CFRelease");
    RegisterFn reg = (RegisterFn)dlsym(at, "AudioComponentRegister");
    InstanceNewFn inew = (InstanceNewFn)dlsym(at, "AudioComponentInstanceNew");
    InstanceDisposeFn idispose = (InstanceDisposeFn)dlsym(at, "AudioComponentInstanceDispose");
    UnitFn init = (UnitFn)dlsym(at, "AudioUnitInitialize");
    UnitFn uninit = (UnitFn)dlsym(at, "AudioUnitUninitialize");
    RenderFn render = (RenderFn)dlsym(at, "AudioUnitRender");
    check("symbols", reg && inew && idispose && init && uninit && render && cf_string && cf_release,
          "a symbol is missing");
    if (!(reg && inew && idispose && init && uninit && render && cf_string && cf_release)) {
        mark("au-end");
        return;
    }
    AudioComponentDescription desc = {kAudioUnitType_Generator, 'sasp', 'SASL', 0, 0};
    CFStringRef name = cf_string(NULL, "Summoner Lab: registrar probe", kCFStringEncodingUTF8);
    AudioComponent comp = reg(&desc, name, 0x00010000, p_factory);
    cf_release(name);
    check("registered_in_process", comp != NULL, "AudioComponentRegister returned NULL");
    AudioComponentInstance inst = NULL;
    OSStatus st = comp ? inew(comp, &inst) : -1;
    check("instance_new", st == noErr && inst != NULL, "AudioComponentInstanceNew failed");
    if (inst != NULL) {
        st = init(inst);
        check("initialize", st == noErr, "AudioUnitInitialize failed");
        float l[64], r[64];
        struct {
            UInt32 n;
            AudioBuffer b[2];
        } abl = {2, {{1, sizeof(l), l}, {1, sizeof(r), r}}};
        AudioTimeStamp ts = {0};
        ts.mFlags = kAudioTimeStampSampleTimeValid;
        AudioUnitRenderActionFlags flags = 0;
        st = render(inst, &flags, &ts, 0, 64, (AudioBufferList*)&abl);
        bool ramp = st == noErr;
        for (int i = 0; i < 64 && ramp; ++i)
            ramp = l[i] == (float)i / 64.0f - 0.5f && r[i] == l[i];
        check("render_is_the_ramp", ramp, "AudioUnitRender failed or rendered something else");
        uninit(inst);
        check("dispose", idispose(inst) == noErr, "AudioComponentInstanceDispose failed");
    }
    mark("au-end");
}

int main(int argc, char** argv) {
    const char *marker = NULL, *pidfile = NULL;
    long secs = 3600;
    bool control_only = false;
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--hold") && i + 1 < argc)
            marker = argv[++i];
        else if (!strcmp(argv[i], "--hold-secs") && i + 1 < argc)
            secs = atol(argv[++i]);
        else if (!strcmp(argv[i], "--pid-file") && i + 1 < argc)
            pidfile = argv[++i];
        else if (!strcmp(argv[i], "--control-only"))
            control_only = true;
        else {
            fprintf(stderr, "usage: summoner_au_probe [--hold <marker> [--hold-secs <n>]] [--pid-file <path>] [--control-only]\n");
            return 64;
        }
    }
    printf("pid %d\n", (int)getpid());
    fflush(stdout);
    if (pidfile) {
        FILE* f = fopen(pidfile, "w");
        if (f)
            fprintf(f, "%d", (int)getpid()), fclose(f);
    }
    if (marker) {
        time_t until = time(NULL) + secs;
        struct stat st;
        while (stat(marker, &st) != 0) {
            if (time(NULL) > until) {
                printf("hold: no marker in %ld s; no AudioToolbox call made\n", secs);
                return 2;
            }
            usleep(250000);
        }
        printf("hold: released\n");
    }
    if (!control_only)
        au_phase();
    else
        dlopen("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation", RTLD_NOW | RTLD_LOCAL);
    // The positive control: CoreFoundation's own connection to cfprefsd, made in this call, between the markers. (The
    // lab HOME's preferences: lab-env sets CFFIXED_USER_HOME.)
    mark("control-begin");
    CFStringCreateFn mk = (CFStringCreateFn)dlsym(RTLD_DEFAULT, "CFStringCreateWithCString");
    CFPrefsCopyFn copy = (CFPrefsCopyFn)dlsym(RTLD_DEFAULT, "CFPreferencesCopyAppValue");
    CFReleaseFn rel = (CFReleaseFn)dlsym(RTLD_DEFAULT, "CFRelease");
    bool have = mk && copy && rel;
    if (have) {
        CFStringRef key = mk(NULL, "probe", kCFStringEncodingUTF8);
        CFStringRef app = mk(NULL, "org.summoner.testkit.au-probe", kCFStringEncodingUTF8);
        CFPropertyListRef v = copy(key, app);
        if (v != NULL)
            rel(v);
        rel(key);
        rel(app);
    }
    check("control_preferences_read", have, "CoreFoundation's functions not found");
    mark("control-end");
    printf("done %s\n", failures == 0 ? "ok" : "FAIL");
    return failures == 0 ? 0 : 1;
}
