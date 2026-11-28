//! `summoner-sys-ui`: the second and only other crate in this repo allowed `unsafe`, under exactly
//! `summoner-sys`'s rules (one module per FFI surface; every `unsafe` block with a `// SAFETY:` comment; each new one
//! reviewed). It exists apart from `summoner-sys` because a `summoner-sys` feature would be unified into every
//! build of the workspace and so link AppKit into the host child (measured). Only the scan probe
//! (`summoner-probe-bin`) depends on it; the release-build link guard checks the host child never does.
//!
//! - `appkit` (feature `appkit`, enabled by nothing: an NSApplication in the scan child brings
//!   the WindowServer, LaunchServices and a TCC ListenEvent preflight): the activation policy and the frontmost app.
//! - `nsapp` (feature `nsapp`, DRAFT): the activation-policy reader, the policy of an NSApplication a plugin made,
//!   found with `dlsym` (never created, AppKit never linked).
//! - `windows` (feature `windows`, enabled by nothing: listing windows alone brings the WindowServer's
//!   TCC ListenEvent and ScreenCapture preflights): the windows a process owns, owner pid / layer / bounds only.
//!
//! macOS only.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

#[cfg(feature = "appkit")]
pub mod appkit;
#[cfg(feature = "nsapp")]
pub mod nsapp;
#[cfg(feature = "windows")]
pub mod windows;
