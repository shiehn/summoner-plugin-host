// The default channel count's helpers, included by shim.cpp (and by a test program).
#pragma once

#include <cstdint>

#include "pluginterfaces/vst/ivstcomponent.h"

namespace {

int32_t clamp_channel_total(int64_t total) noexcept
{
    if (total < 0)
        return 0;
    if (total > static_cast<int64_t>(INT32_MAX))
        return INT32_MAX;
    return static_cast<int32_t>(total);
}

int64_t sum_default_active_audio(Steinberg::Vst::IComponent* component,
                                 Steinberg::Vst::BusDirection direction)
{
    const Steinberg::int32 n = component->getBusCount(Steinberg::Vst::kAudio, direction);
    if (n <= 0)
        return 0;

    int64_t total = 0;
    for (Steinberg::int32 i = 0; i < n; ++i) {
        Steinberg::Vst::BusInfo info {};
        if (component->getBusInfo(Steinberg::Vst::kAudio, direction, i, info) != Steinberg::kResultOk)
            continue;
        if ((info.flags & Steinberg::Vst::BusInfo::kDefaultActive) == 0)
            continue;
        total += static_cast<int64_t>(info.channelCount);
    }
    return total;
}

} // namespace
