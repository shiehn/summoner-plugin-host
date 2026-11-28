//! The plugin host's log vocabulary: its neutral defaults. A host owns four areas, at their nibbles in the level word
//! ([`PINNED`]): `control` (its log's own records), `audio`, `host` and `plugin`; the other twelve are placeholders.
//! A host adopts the full set of words from its parent's level spec at start (`runecraft_log::adopt_areas`, with
//! [`PINNED`]): a parent passes `describe` of its levels, 16 words in nibble order, so the host then writes and parses
//! its parent's words, nibble for nibble.
//!
//! The host reads its levels and its salt from [`HOST_VOCAB`]'s variables (`SUMMONER_LOG`, `SUMMONER_LOG_SALT`), which
//! the supervisor writes for it from its own application's values (`summoner::supervisor::host_child_env`), and names
//! its drainer thread and stderr lines with the host's own words.

use runecraft_contracts::log::{Area, Event, Level, Vocab};

/// The host log's own records (`session.start`, `log.levels`, ...): its meta area, nibble 1.
pub const CONTROL: Area = Area::from_u8_const(1);
/// The audio area, nibble 4.
pub const AUDIO: Area = Area::from_u8_const(4);
/// The host's own area (its lifecycle and its chains), nibble 11.
pub const HOST: Area = Area::from_u8_const(11);
/// The plugin area (what a plugin did or failed to do), nibble 12.
pub const PLUGIN: Area = Area::from_u8_const(12);

/// The nibbles whose words are the host's own: a parent spec that names them otherwise is not adopted.
pub const PINNED: [u8; 4] = [1, 4, 11, 12];

/// The host's realtime codes (indexes into [`EVENTS`]).
pub mod code {
    /// One chain block's cost: `chain` (the chain key), `ticks`, `frames`, `seq` (trace).
    pub const CHAIN_BLOCK: u16 = 0;
    /// A chain block over its budget: `chain`, `ticks`, `budgetTicks`, `frames`, `seq` (info).
    pub const CHAIN_SLOW: u16 = 1;
}

/// The host's realtime rows (a code never leaves the process: the host's own drainer formats its own rings).
pub static EVENTS: [Event; 2] = [
    Event {
        name: "chain.block",
        area: HOST,
        level: Level::Trace,
        fields: &["chain", "ticks", "frames", "seq"],
    },
    Event {
        name: "chain.slow",
        area: HOST,
        level: Level::Info,
        fields: &["chain", "ticks", "budgetTicks", "frames", "seq"],
    },
];

/// The host's vocabulary before adoption. No folder: a host writes only into the folder its parent passes.
pub static HOST_VOCAB: Vocab = Vocab::new(
    [
        "area0", "control", "area2", "area3", "audio", "area5", "area6", "area7", "area8", "area9",
        "area10", "host", "plugin", "area13", "area14", "area15",
    ],
    CONTROL,
    &EVENTS,
)
.with_env("SUMMONER_LOG", "SUMMONER_LOG_SALT")
.with_stderr("summoner-log", "summoner-log", "summoner-");

const _: () = assert!(HOST_VOCAB.check().is_ok());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_own_words_are_at_their_nibbles() {
        for (n, w) in [(1, "control"), (4, "audio"), (11, "host"), (12, "plugin")] {
            assert_eq!(HOST_VOCAB.areas[n], w);
            assert!(PINNED.contains(&(n as u8)));
        }
        assert_eq!(HOST_VOCAB.folder, None);
        assert_eq!(HOST_VOCAB.meta_area, CONTROL);
        assert_eq!(EVENTS[code::CHAIN_BLOCK as usize].name, "chain.block");
        assert_eq!(EVENTS[code::CHAIN_SLOW as usize].name, "chain.slow");
    }
}
