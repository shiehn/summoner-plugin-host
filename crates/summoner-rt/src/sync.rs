//! The facade the region views and the realtime exchange are written against: `std` and
//! `runecraft_ipc::bridge` normally, `loom` and a heap model of the mapping under `RUSTFLAGS="--cfg loom"`, so a loom
//! model of the region runs the shipped [`crate::region`] and [`crate::rt`] code, not a copy, as
//! the telemetry crate's sync and `runecraft_rt::handoff` do.
//!
//! Under loom the mapping is not shared memory (no process, no `mmap`): it is one loom atomic per naturally aligned
//! word of each width. That is exact for every peer that keeps D12's rule, which the host application does by construction (each
//! field is accessed only at its declared width, so a `u32` word and the `u64` word over the same bytes are never
//! both used). The doorbell is a no-op there: the model's child observes the hub word itself.

#[cfg(not(loom))]
pub use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

#[cfg(loom)]
pub use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

// The handover's per-slot cell no longer takes a `Mutex` from here: it is `runecraft_rt::rt_lock::WiredLock`, whose
// loom build is runecraft-rt's own (it is compiled against loom under `--cfg loom` too).

#[cfg(not(loom))]
pub use runecraft_ipc::bridge::Mapping;

/// The owned doorbell of a region or child page word and its borrowed handle (the callback rings a hub's `Waker`).
#[cfg(not(loom))]
pub use runecraft_ipc::bridge::{Doorbell, DoorbellKind, Waker};

/// Host ticks (`mach_absolute_time`, the commpage: no kernel entry).
#[cfg(not(loom))]
#[inline]
pub fn host_ticks() -> u64 {
    runecraft_ipc::bridge::host_ticks()
}

/// The loom model's doorbell kind (the kind changes nothing in the model).
#[cfg(loom)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DoorbellKind {
    /// Wakes every waiter.
    FanOut,
    /// Wakes one waiter.
    One,
}

/// The loom model's owned doorbell (`runecraft_ipc::bridge::Doorbell`): its kind only.
#[cfg(loom)]
#[derive(Debug)]
pub struct Doorbell {
    kind: DoorbellKind,
}

#[cfg(loom)]
impl Doorbell {
    /// The model's doorbell of a word.
    pub fn create(kind: DoorbellKind) -> std::io::Result<Doorbell> {
        Ok(Doorbell { kind })
    }

    #[inline]
    /// The borrowed handle on `word`.
    pub fn waker<'a>(&'a self, word: &'a AtomicU64) -> Waker<'a> {
        let _ = self.kind;
        Waker { word }
    }
}

/// The loom model's waker (`runecraft_ipc::bridge::Waker`'s ring: store(value, Release), then the wake, a no-op
/// here: loom threads spin on the word).
#[cfg(loom)]
#[derive(Clone, Copy, Debug)]
pub struct Waker<'a> {
    word: &'a AtomicU64,
}

#[cfg(loom)]
impl Waker<'_> {
    #[inline]
    /// Stores `value` with Release, then wakes (a no-op in the model).
    pub fn ring(&self, value: u64) -> Result<(), i32> {
        self.word.store(value, Ordering::Release);
        Ok(())
    }
}

#[cfg(loom)]
#[inline]
/// The loom model's host ticks: always 0.
pub fn host_ticks() -> u64 {
    0
}

/// The loom model of a mapping: `len` bytes as loom atomics, one per aligned `u32` and one per aligned `u64`, all
/// zero. The accessors bounds-check exactly as `runecraft_ipc::bridge::Mapping`'s do (a violation is a panic: a bug of the
/// caller's geometry).
#[cfg(loom)]
pub struct Mapping {
    w32: Box<[AtomicU32]>,
    w64: Box<[AtomicU64]>,
    len: usize,
}

#[cfg(loom)]
impl std::fmt::Debug for Mapping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Mapping(model, {} bytes)", self.len)
    }
}

#[cfg(loom)]
impl Mapping {
    /// A model mapping of `len` bytes, all zero.
    pub fn model(len: usize) -> Mapping {
        Mapping {
            w32: (0..len / 4).map(|_| AtomicU32::new(0)).collect(),
            w64: (0..len / 8).map(|_| AtomicU64::new(0)).collect(),
            len,
        }
    }

    /// The mapping's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    /// The `u32` at byte offset `off` (aligned and in bounds, else a panic).
    pub fn u32(&self, off: usize) -> &AtomicU32 {
        assert!(off.is_multiple_of(4) && off + 4 <= self.len, "u32 at {off}");
        &self.w32[off / 4]
    }

    #[inline]
    /// The `u64` at byte offset `off` (aligned and in bounds, else a panic).
    pub fn u64(&self, off: usize) -> &AtomicU64 {
        assert!(off.is_multiple_of(8) && off + 8 <= self.len, "u64 at {off}");
        &self.w64[off / 8]
    }

    #[inline]
    /// `n` `u32`s from byte offset `off` (aligned and in bounds, else a panic).
    pub fn u32s(&self, off: usize, n: usize) -> &[AtomicU32] {
        assert!(
            off.is_multiple_of(4) && off + 4 * n <= self.len,
            "u32s at {off}"
        );
        &self.w32[off / 4..off / 4 + n]
    }
}
