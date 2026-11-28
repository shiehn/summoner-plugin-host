// A lab interposer for the AU parameter-list size edges (clean-room pins of the AU parameter list): injected with
// DYLD_INSERT_LIBRARIES into a test's own process, it answers AudioUnitGetPropertyInfo / AudioUnitGetProperty for
// kAudioUnitProperty_ParameterList (global scope) with a size chosen by SUMMONER_LAB_PLSIZE, which no AudioUnitSDK unit
// can produce (the SDK's sizes are always 4 x the parameter count). Every other call passes through untouched.
//   zero  : the size is 0
//   three : the size is 3 (below one id)
//   nine  : the size is 9 (two ids and a byte); a get writes min(given, 9) bytes of ids 0, 1, .. and reports that
//   short : the size is 12 (three ids), but a get writes and reports 4 bytes (one id): returned below the buffer
//   long  : the size is 8 (two ids); a get writes min(given, 8) bytes but reports 16: returned above the buffer
// A get with a zero-size buffer is refused (kAudio_ParamError, -50), as the AudioUnitSDK refuses it.
#include <AudioToolbox/AudioToolbox.h>
#include <stdlib.h>
#include <string.h>

#define INTERPOSE(replacement, replacee)                                                                 \
    __attribute__((used)) static struct {                                                                \
        const void* r;                                                                                   \
        const void* e;                                                                                   \
    } interpose_##replacee __attribute__((section("__DATA,__interpose"))) = {(const void*)&replacement, \
                                                                             (const void*)&replacee}

static int mode(void) {
    const char* m = getenv("SUMMONER_LAB_PLSIZE");
    if (m == NULL)
        return 0;
    if (strcmp(m, "zero") == 0)
        return 1;
    if (strcmp(m, "three") == 0)
        return 2;
    if (strcmp(m, "nine") == 0)
        return 3;
    if (strcmp(m, "short") == 0)
        return 4;
    if (strcmp(m, "long") == 0)
        return 5;
    return 0;
}

static UInt32 info_size(int m) {
    static const UInt32 sizes[] = {0, 0, 3, 9, 12, 8};
    return sizes[m];
}

static int ours(AudioUnitPropertyID id, AudioUnitScope scope) {
    return id == kAudioUnitProperty_ParameterList && scope == kAudioUnitScope_Global && mode() != 0;
}

static OSStatus plsize_get_info(AudioUnit unit, AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el,
                                UInt32* size, Boolean* writable) {
    if (!ours(id, scope))
        return AudioUnitGetPropertyInfo(unit, id, scope, el, size, writable);
    if (size != NULL)
        *size = info_size(mode());
    if (writable != NULL)
        *writable = false;
    return noErr;
}

static OSStatus plsize_get(AudioUnit unit, AudioUnitPropertyID id, AudioUnitScope scope, AudioUnitElement el,
                           void* data, UInt32* size) {
    if (!ours(id, scope))
        return AudioUnitGetProperty(unit, id, scope, el, data, size);
    if (size == NULL || *size == 0)
        return kAudio_ParamError;
    const int m = mode();
    UInt32 actual = m == 4 ? 4 : info_size(m);
    UInt32 n = *size < actual ? *size : actual;
    UInt8 bytes[16];
    for (UInt32 k = 0; k < 4; ++k) {
        const UInt32 v = k;
        memcpy(bytes + 4 * k, &v, 4);
    }
    memcpy(data, bytes, n);
    *size = m == 5 ? 16 : n;
    return noErr;
}

INTERPOSE(plsize_get_info, AudioUnitGetPropertyInfo);
INTERPOSE(plsize_get, AudioUnitGetProperty);
