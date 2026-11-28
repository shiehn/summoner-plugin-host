//! The supervisor's clock, injectable so the 60 s forgiveness gate, the 2 s + 2 s wedge detector and the backoff are
//! tested without real sleeps (the brief's rule). Production uses [`SystemClock`].
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The supervisor's time source: monotonic, injectable.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
}

#[derive(Debug)]
/// The real monotonic clock, from its creation.
pub struct SystemClock(Instant);

impl SystemClock {
    /// A clock whose origin is now.
    pub fn new() -> Arc<SystemClock> {
        Arc::new(SystemClock(Instant::now()))
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// A clock that moves only when told to (tests).
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock at 1 s, still until advanced.
    pub fn new() -> Arc<ManualClock> {
        Arc::new(ManualClock(AtomicU64::new(1_000_000_000)))
    }

    /// Move the clock forward by `d`.
    pub fn advance(&self, d: Duration) {
        self.0.fetch_add(d.as_nanos() as u64, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.load(Ordering::SeqCst))
    }
}
