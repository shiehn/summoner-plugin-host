// The VST3 event fill for one note of the block, included by shim.cpp (and by a test program).
#pragma once
// The VST3 event for one note of the block. Never throws, never allocates.

#include <cstdint>
#include <cstring>

#include "pluginterfaces/vst/ivstevents.h"

#include "summoner_vst3_shim.h"

namespace {

inline Steinberg::int8 to_i8(Steinberg::int16 v) noexcept {
    // Two's-complement truncation to 8 bits.
    return static_cast<Steinberg::int8>(static_cast<std::uint8_t>(static_cast<std::uint16_t>(v) & 0xFFu));
}

inline Steinberg::uint8 to_u8(Steinberg::int16 v) noexcept {
    return static_cast<Steinberg::uint8>(static_cast<std::uint16_t>(v) & 0xFFu);
}

inline void fill_note_off(const summoner_vst3_note& n, Steinberg::Vst::Event& e) noexcept {
    e.type = Steinberg::Vst::Event::kNoteOffEvent;
    e.noteOff.channel = n.channel;
    e.noteOff.pitch = n.pitch;
    e.noteOff.velocity = n.velocity;
    e.noteOff.noteId = -1;
    e.noteOff.tuning = 0.0f;
}

} // namespace

void fill_event(const summoner_vst3_note& n, Steinberg::Vst::Event& e) noexcept {
    using Steinberg::Vst::Event;

    std::memset(&e, 0, sizeof(Event));
    e.busIndex = 0;
    e.sampleOffset = n.offset;
    e.flags = 0;
    e.ppqPosition = 0;

    switch (n.kind) {
    case SUMMONER_VST3_NOTE_ON:
        e.type = Event::kNoteOnEvent;
        e.noteOn.channel = n.channel;
        e.noteOn.pitch = n.pitch;
        e.noteOn.tuning = 0.0f;
        e.noteOn.velocity = n.velocity;
        e.noteOn.length = 0;
        e.noteOn.noteId = -1;
        break;
    case SUMMONER_VST3_POLY_PRESSURE:
        e.type = Event::kPolyPressureEvent;
        e.polyPressure.channel = n.channel;
        e.polyPressure.pitch = n.pitch;
        e.polyPressure.pressure = n.velocity;
        e.polyPressure.noteId = -1;
        break;
    case SUMMONER_VST3_LEGACY_CC:
        e.type = Event::kLegacyMIDICCOutEvent;
        e.midiCCOut.controlNumber = to_u8(n.control);
        e.midiCCOut.channel = to_i8(n.channel);
        e.midiCCOut.value = to_i8(n.pitch);
        e.midiCCOut.value2 = to_i8(n.value2);
        break;
    case SUMMONER_VST3_NOTE_OFF:
    default:
        fill_note_off(n, e);
        break;
    }
}
