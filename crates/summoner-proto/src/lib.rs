//! `summoner-proto`: the plugin-host child protocol, the bytes both processes must agree on.
//!
//! - [`layout`]: the byte-exact region (two slots by `seq` parity), the child page and the frame header, with
//!   compile-time offset checks and the field table a C header generator reads ([`layout::SECTIONS`]).
//! - [`codec`]: the control frames, the streaming decoder and the typed meta (fuzzed).
//! - [`plugin`]: what a host says about a plugin: the ref of a class to load and a class a scan found.
//! - [`state_blob`]: the one canonical framing of a plugin state blob (format v1, its four-byte magic), and its frame check
//!   (fuzzed).
//!
//! Safe code only; nothing here runs on a realtime thread except the layout's constants and geometry.
//! - Errors: [`codec::ProtoError`] (a frame refused), [`codec::ScannedFault`] (a `scanned` line refused, with the
//!   engine-limit cases apart), [`state_blob::Refusal`] (a state blob refused) and [`layout::GeometryError`] (a
//!   region geometry refused): one per layer, each implementing `std::error::Error`.
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

pub mod codec;
pub mod host_names;
pub mod layout;
pub mod log_vocab;
pub mod plugin;
pub mod state_blob;
