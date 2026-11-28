//! The MIDI carry: while a chain's breaker is open, releases only are carried to the
//! first request after the close; everything else is dropped. Note-offs are deduplicated per (channel, note); CC 64
//! keeps its last value per channel (only values below 64, sustain off, are releases); CC 120 and CC 123 are flags.
//! A channel with more than 32 carried note-offs carries one CC 123 instead. So the carry is bounded at
//! 16 × (32 + 3) = 560 events and a release is never dropped. Fixed arrays: no allocation (N1).

/// A short MIDI event (status, data1, data2).
pub type Event3 = [u8; 3];

/// The note-off cap per channel (the fork's fix).
pub const NOTE_OFF_CAP: u32 = 32;
/// The carry's bound: 16 channels × (32 note-offs + CC 64 + CC 120 + CC 123).
pub const CARRY_BOUND: usize = 16 * (NOTE_OFF_CAP as usize + 3);

/// What [`MidiCarry::offer`] did with an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Offered {
    /// A release: carried (or already covered by a carried one).
    Carried,
    /// A release that made its channel pass the cap: replaced by one CC 123.
    AllNotesOff,
    /// Not a release: dropped (counted `bridgeMidiDropped`).
    Dropped,
}

/// D5's carry: the releases offered while a chain's breaker is open, held for the first request after the close
/// (note-offs per channel and note, CC 64's last value per channel, CC 120 and CC 123 as flags). Bounded by
/// [`CARRY_BOUND`]; no allocation.
#[derive(Clone, Debug)]
pub struct MidiCarry {
    note_off: [[u64; 2]; 16],
    count: [u8; 16],
    /// CC 64's last carried value per channel; 0xff = none.
    cc64: [u8; 16],
    cc120: u16,
    cc123: u16,
}

impl Default for MidiCarry {
    fn default() -> Self {
        MidiCarry {
            note_off: [[0; 2]; 16],
            count: [0; 16],
            cc64: [0xff; 16],
            cc120: 0,
            cc123: 0,
        }
    }
}

/// Whether an event is a release in D5's sense.
#[inline]
pub fn is_release(e: Event3) -> bool {
    match e[0] & 0xf0 {
        0x80 => true,
        0x90 => e[2] == 0,
        0xb0 => (e[1] == 64 && e[2] < 64) || e[1] == 120 || e[1] == 123,
        _ => false,
    }
}

impl MidiCarry {
    /// Whether nothing is carried.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Events a drain would emit now (≤ [`CARRY_BOUND`]).
    pub fn len(&self) -> usize {
        let mut n = 0;
        for ch in 0..16 {
            n += self.count[ch] as usize;
            n += (self.cc64[ch] != 0xff) as usize;
            n += (self.cc120 >> ch & 1) as usize;
            n += (self.cc123 >> ch & 1) as usize;
        }
        n
    }

    /// Offer one event of a block that is not being published.
    #[inline]
    pub fn offer(&mut self, e: Event3) -> Offered {
        if !is_release(e) {
            return Offered::Dropped;
        }
        let ch = (e[0] & 0x0f) as usize;
        let bit = 1u16 << ch;
        match e[0] & 0xf0 {
            0x80 | 0x90 => {
                if self.cc123 & bit != 0 {
                    return Offered::Carried; // covered by the channel's all-notes-off
                }
                let note = (e[1] & 0x7f) as usize;
                let (w, b) = (note >> 6, 1u64 << (note & 63));
                if self.note_off[ch][w] & b != 0 {
                    return Offered::Carried; // deduplicated
                }
                if self.count[ch] as u32 >= NOTE_OFF_CAP {
                    self.note_off[ch] = [0, 0];
                    self.count[ch] = 0;
                    self.cc123 |= bit;
                    return Offered::AllNotesOff;
                }
                self.note_off[ch][w] |= b;
                self.count[ch] += 1;
                Offered::Carried
            }
            _ => {
                match e[1] {
                    64 => self.cc64[ch] = e[2],
                    120 => self.cc120 |= bit,
                    _ => {
                        // CC 123 covers every carried note-off of the channel.
                        self.cc123 |= bit;
                        self.note_off[ch] = [0, 0];
                        self.count[ch] = 0;
                    }
                }
                Offered::Carried
            }
        }
    }

    /// Emit and clear the carry: per channel the all-notes-off or the note-offs (ascending), then CC 64, then CC 120.
    pub fn drain(&mut self, mut emit: impl FnMut(Event3)) -> usize {
        let mut n = 0;
        for ch in 0..16u8 {
            let c = ch as usize;
            if self.cc123 >> ch & 1 != 0 {
                emit([0xb0 | ch, 123, 0]);
                n += 1;
            } else {
                for w in 0..2 {
                    let mut bits = self.note_off[c][w];
                    while bits != 0 {
                        let b = bits.trailing_zeros();
                        bits &= bits - 1;
                        emit([0x80 | ch, (w as u32 * 64 + b) as u8, 0]);
                        n += 1;
                    }
                }
            }
            if self.cc64[c] != 0xff {
                emit([0xb0 | ch, 64, self.cc64[c]]);
                n += 1;
            }
            if self.cc120 >> ch & 1 != 0 {
                emit([0xb0 | ch, 120, 0]);
                n += 1;
            }
        }
        *self = MidiCarry::default();
        n
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;

    #[test]
    fn only_releases_are_carried_deduplicated_and_capped() {
        let mut c = MidiCarry::default();
        assert_eq!(c.offer([0x90, 60, 100]), Offered::Dropped);
        assert_eq!(
            c.offer([0xb0, 64, 127]),
            Offered::Dropped,
            "sustain on is not a release"
        );
        assert_eq!(c.offer([0x80, 60, 0]), Offered::Carried);
        assert_eq!(
            c.offer([0x90, 60, 0]),
            Offered::Carried,
            "a velocity-0 note-on is a note-off"
        );
        assert_eq!(c.len(), 1, "deduplicated per (channel, note)");
        c.offer([0xb0, 64, 10]);
        c.offer([0xb0, 64, 0]);
        let mut out = Vec::new();
        c.drain(|e| out.push(e));
        assert_eq!(out, vec![[0x80, 60, 0], [0xb0, 64, 0]]);
        assert!(c.is_empty());
        // 33 distinct note-offs on channel 2: one CC 123 instead.
        for n in 0..32 {
            assert_eq!(c.offer([0x82, n, 0]), Offered::Carried);
        }
        assert_eq!(c.offer([0x82, 40, 0]), Offered::AllNotesOff);
        assert_eq!(c.offer([0x82, 41, 0]), Offered::Carried);
        let mut out = Vec::new();
        c.drain(|e| out.push(e));
        assert_eq!(out, vec![[0xb2, 123, 0]]);
    }

    #[test]
    fn the_bound_holds_whatever_arrives() {
        let mut c = MidiCarry::default();
        let mut x = 0x1234_5678u32;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let e = [
                (x >> 24) as u8 | 0x80,
                (x >> 8) as u8 & 0x7f,
                x as u8 & 0x7f,
            ];
            c.offer(e);
            assert!(c.len() <= CARRY_BOUND);
        }
    }
}
