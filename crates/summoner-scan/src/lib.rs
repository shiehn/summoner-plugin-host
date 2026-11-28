//! `summoner-scan`: the plugin host's scan supervisor.
//!
//! - [`scan`]: one probe per process, in its own process group, its stderr captured, the reader's blob budget, the
//!   time budget, the verdict table.
//!
//! Safe code only: every `unsafe` lives in the floor crates (`runecraft-rt`, `runecraft-ipc`).
#![deny(missing_docs)]
#![deny(clippy::print_stderr, clippy::print_stdout)]
#![forbid(unsafe_code)]

#[cfg(not(loom))]
// spawn, pipes and process groups: not model-checked
pub mod scan;

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
