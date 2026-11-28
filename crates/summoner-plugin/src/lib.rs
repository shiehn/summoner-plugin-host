//! `summoner-plugin`: the rules a plugin host applies to a hosted plugin's slot, shared by every host.
//!
//! - [`channels`]: the slot channel rule around each call of a slot's plugin (what it sees, what the next slot gets).
//! - [`params`]: the flattened parameter-tree order a host reports for each parameter, and the name cut.
//! - [`handover`]: the host side of a plugin's latency-change reactivation: the five-state word, the bypass delay
//!   line and the fade, loom-modelled.
//!
//! Safe code only. `handover` runs on a hosted chain's thread (its clippy.toml binds it); `params` never does.
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

pub mod channels;
pub mod handover;
pub mod params;

#[cfg(loom)]
pub use handover::HandoverMutant;
