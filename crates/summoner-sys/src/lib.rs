//! `summoner-sys`: one of the two crates in this repo allowed `unsafe` (the other: `summoner-sys-ui`, the scan probe's
//! AppKit / CoreGraphics surfaces, kept apart so the host child never links them), as the host application confines its
//! own `unsafe`. Every other crate is `#![forbid(unsafe_code)]` and reaches the OS through the runecraft floor crates
//! or through this crate's safe API. One module per FFI surface; every `unsafe` block carries a reviewed `// SAFETY:`
//! comment.
//!
//! - [`alloc_count`]: the injected allocation counter's arm / disarm (the no-allocation tests).
//! - [`exit_watch`]: the kernel's process-exit note (kqueue `EVFILT_PROC` / `NOTE_EXIT`), for the orphan watch.
//! - [`crash`]: the fatal-signal handler, its crash log, and the plugin breadcrumb it appends.
//! - [`num`]: the C library's `strtof`, as the strict number parsing uses it.
//! - [`vst3`] (feature `vst3`): the VST3 host shim over the pinned SDK, behind safe owned handles.
//! - [`au`] (feature `au`): AU v2 hosting through AudioToolbox, behind safe owned handles (lab bundles only).
//! - [`bundle`]: the development gate (no `unsafe`): only the lab's own bundles reach either loader, checked before they
//!   are opened and again after they are loaded.
//! - [`images`]: the post-load check's portable types (the loaded images, the system rule, the foreign list).
//! - [`dyld`]: the loaded-image list the post-load check reads (macOS's body of [`images::loaded`]).
//! - [`editor`] (feature `editor`): the plugin editor windows, in a dylib of their own loaded on first use.
//! - [`proc_exit`]: the immediate exit (`_exit`), the way the child always ends: no `atexit` handlers, no plugin's
//!   static destructors.
//! - [`qos`]: the calling thread's QoS class, read back (`pthread_get_qos_class_np`, read-only).
//! - [`workgroup`] (macOS): the audio workgroup (`os_workgroup` create-with-port / join / leave), groundwork,
//!   not wired yet (the host application cannot hand over a raw port).
//!
//! macOS only, like the child.
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]

pub mod alloc_count;
#[cfg(feature = "au")]
pub mod au;
pub mod bundle;
pub mod crash;
pub mod dyld;
#[cfg(feature = "editor")]
pub mod editor;
pub mod exit_watch;
pub mod images;
pub mod num;
pub mod proc_exit;
pub mod qos;
pub mod sha256;
#[cfg(feature = "vst3")]
pub mod vst3;
#[cfg(target_os = "macos")]
pub mod workgroup;

/// The crate's refusal with a reason: its `Display` is the reason as the host's lines and replies word it, its `Debug`
/// the reason's as a string's.
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

impl From<String> for Error {
    fn from(why: String) -> Error {
        Error(why)
    }
}

impl From<&str> for Error {
    fn from(why: &str) -> Error {
        Error(why.to_string())
    }
}

/// The reason's text, for a caller that reports it in a message of its own.
impl From<Error> for String {
    fn from(e: Error) -> String {
        e.0
    }
}
