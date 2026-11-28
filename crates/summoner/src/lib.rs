//! `summoner`: the plugin host's supervisor, host side.
//!
//! - [`supervisor`]: the supervisor, reader and reaper threads: spawn with the control channel on descriptor 3 and the
//!   mach route, the chain catalogue, generations, the save burst, respawn with its gate, quarantine, the wedge
//!   detector. Its records go to the [`runecraft_contracts::log::LogSink`] and its state blob hashes to the
//!   [`runecraft_contracts::digest::StateHasher`] its `Config` names.
//! - [`depth`]: the adaptive pipeline depth.
//! - [`clock`]: the supervisor's injectable clock.
//!
//! Safe code only: every `unsafe` lives in the floor crates (`runecraft-rt`, `runecraft-ipc`).
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

pub mod clock;
pub mod depth;
#[cfg(not(loom))]
// spawn, sockets and mach ports: not model-checked (the loom build covers the region and the realtime exchange only)
pub mod supervisor;
