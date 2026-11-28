//! MIDI short message -> VST3 event or parameter point. Audio thread: pure, no allocation,
//! no lock, no panic for any input.

use summoner_sys::vst3 as sys;

/// Controller index of channel pressure in `IMidiMapping` (`kAfterTouch`).
const CTRL_AFTER_TOUCH: usize = 128;
/// Controller index of pitch bend in `IMidiMapping` (`kPitchBend`).
const CTRL_PITCH_BEND: usize = 129;
/// `kCtrlProgramChange`.
const LEGACY_PROGRAM_CHANGE: i16 = 130;
/// `kCtrlQuarterFrame`.
const LEGACY_QUARTER_FRAME: i16 = 132;

/// What one MIDI message becomes for a VST3 plugin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Vst3Midi {
    /// A note-on, note-off, poly-pressure or legacy-controller event.
    Event(sys::Note),
    /// A point for parameter `id` at `offset`.
    Point { id: u32, offset: i32, value: f64 },
    /// Nothing reaches the plugin.
    None,
}

#[inline]
fn clamp7(x: u8) -> u8 {
    if x > 127 { 127 } else { x }
}

#[inline]
fn unit_f32(x: u8) -> f32 {
    f32::from(x) / 127.0f32
}

#[inline]
fn unit_f64(x: u8) -> f64 {
    f64::from(x) / 127.0
}

#[inline]
fn event(offset: i32, kind: i32, channel: i16) -> sys::Note {
    sys::Note {
        offset,
        kind,
        channel,
        ..sys::Note::default()
    }
}

#[inline]
fn point(id: Option<u32>, offset: i32, value: f64) -> Vst3Midi {
    match id {
        Some(id) => Vst3Midi::Point { id, offset, value },
        None => Vst3Midi::None,
    }
}

/// `mapped(channel, controller)`: the parameter id the plugin maps `controller` (0..=129) on MIDI channel
/// `channel` (0..=15) to, or `None`. The caller owns the mapping table.
pub fn convert_midi(
    offset: i32,
    status: u8,
    d1: u8,
    d2: u8,
    mapped: impl Fn(usize, usize) -> Option<u32>,
) -> Vst3Midi {
    let ch_u = status & 0x0F;
    let ch = i16::from(ch_u);
    let chi = usize::from(ch_u);
    match status & 0xF0 {
        0x80 => {
            let mut n = event(offset, sys::NOTE_OFF, ch);
            n.pitch = i16::from(d1);
            n.velocity = unit_f32(d2);
            Vst3Midi::Event(n)
        }
        0x90 => {
            if d2 > 0 {
                let mut n = event(offset, sys::NOTE_ON, ch);
                n.pitch = i16::from(d1);
                n.velocity = unit_f32(d2);
                Vst3Midi::Event(n)
            } else {
                let mut n = event(offset, sys::NOTE_OFF, ch);
                n.pitch = i16::from(d1);
                n.velocity = 0.0;
                Vst3Midi::Event(n)
            }
        }
        0xA0 => {
            let mut n = event(offset, sys::POLY_PRESSURE, ch);
            n.pitch = i16::from(clamp7(d1));
            n.velocity = unit_f32(clamp7(d2));
            Vst3Midi::Event(n)
        }
        0xB0 => point(mapped(chi, usize::from(clamp7(d1))), offset, unit_f64(d2)),
        0xC0 => {
            let mut n = event(offset, sys::LEGACY_CC, ch);
            n.control = LEGACY_PROGRAM_CHANGE;
            n.pitch = i16::from(clamp7(d1));
            n.value2 = 0;
            Vst3Midi::Event(n)
        }
        0xD0 => point(mapped(chi, CTRL_AFTER_TOUCH), offset, unit_f64(d1)),
        0xE0 => {
            let bend = u32::from(d1 & 0x7F) | (u32::from(d2 & 0x7F) << 7);
            point(
                mapped(chi, CTRL_PITCH_BEND),
                offset,
                f64::from(bend) / 16383.0,
            )
        }
        0xF0 => {
            if status == 0xF1 {
                let mut n = event(offset, sys::LEGACY_CC, 0);
                n.control = LEGACY_QUARTER_FRAME;
                n.pitch = i16::from(d1 & 0x0F);
                n.value2 = 0;
                Vst3Midi::Event(n)
            } else {
                Vst3Midi::None
            }
        }
        // 0x00..=0x70: a data byte in the status position.
        _ => Vst3Midi::None,
    }
}
