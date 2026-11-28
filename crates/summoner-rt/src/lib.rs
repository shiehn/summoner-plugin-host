//! `summoner-rt`: the plugin host's realtime exchange, host side.
//!
//! - [`region`]: typed atomic views of a region and the child page over `runecraft_ipc::bridge::Mapping`.
//! - [`rt`]: the callback's exchange: collect, the primed FIFO, publish, the miss and the breaker, the ring; the held
//!   notes a hosted instrument still sounds and the chase of notes already sounding.
//! - [`midi`]: the release carry.
//! - [`render`]: the render worker: a render set's synchronous exchange (one block per `seq`, the ack wake, the
//!   offline block budget, cancel) and the render-set RAM admission.
//! - [`sync`]: the atomics and mapping facade (`std` / `runecraft_ipc::bridge`, or loom under `--cfg loom`).
//!
//! Safe code only: every `unsafe` lives in the floor crates (`runecraft-rt`, `runecraft-ipc`).
//! - Errors: [`Error`] (admission refused, a reason), [`render::RenderError`] (a render block failed) and
//!   [`render::RenderRefused`] (the render admission's numbers): each implements `std::error::Error`.
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

pub mod midi;
pub mod region;
#[cfg(not(loom))]
// the render worker: a render thread's blocking exchange, not model-checked
pub mod render;
pub mod rt;
pub mod sync;

pub use region::{ChildPage, Region};
#[cfg(loom)]
pub use rt::Mutant;
pub use rt::{
    AudioCounters, ChainLink, ChainState, Chase, ChaseNote, ChaseQuery, ChaseStats, HeldNotes,
    MAX_CHASED_EDGES, MidiEvent, Tempo, Transport, Visit, chase_frames,
};

/// The crate's refusal with a reason (a render set not admitted, a configuration refused). Its `Display` is the
/// reason as the job's error and the logs have always worded it; its `Debug` is the reason's as a string's.
#[derive(Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// The reason's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The reason's text, for a caller that reports it in a message of its own.
impl From<Error> for String {
    fn from(e: Error) -> String {
        e.0
    }
}

impl Error {
    pub(crate) fn new(why: impl Into<String>) -> Error {
        Error(why.into())
    }
}
