//! `summoner-testkit`: the plugin host's test double.
//!
//! - [`fake_host`]: a child that speaks the real protocol over trivial DSP; [`fake_host::main`] is the whole program
//!   (an application wraps it in a binary of its own).
//!
//! Safe code only. Never linked into a shipped build.
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

pub mod fake_host;
