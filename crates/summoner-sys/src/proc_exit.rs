//! The process's immediate exit (`_exit(2)`), the one way the child ends (a fatal host error, the reader's EOF,
//! `shutdown`, the orphan watch).
//!
//! `std::process::exit` runs the C library's `exit`: the `atexit` handlers and the C++ static destructors of every
//! image loaded, which here includes every hosted plugin's (many plugins carry their own singletons) and the editor
//! dylib's, while chain threads may still be inside plugin code. `_exit` ends the process at once: no handlers, no
//! destructors, no stdio flush. A caller that wrote to stdout flushes it first; files it wrote must already be closed.

/// One hook run by [`exit_now`] before `_exit` (the diagnostic log's bounded flush and `session.end`). Set once;
/// it must be bounded (the diagnostic log's waits at most 20 ms) and never take a lock a chain thread can hold.
/// [`exit_now`] is never called from a signal handler (the crash handler leaves through its own `_exit` / raise).
static PRE_EXIT: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Registers the pre-exit hook (the first registration wins).
pub fn set_pre_exit(f: fn()) {
    let _ = PRE_EXIT.set(f);
}

/// End the process now with `code`, without running `atexit` handlers or static destructors, and without flushing
/// stdio buffers (Rust's `stdout` included: flush it first if anything was printed). The pre-exit hook, if one is
/// set, runs first.
pub fn exit_now(code: i32) -> ! {
    if let Some(f) = PRE_EXIT.get() {
        f();
    }
    // SAFETY: `_exit` takes a plain int and never returns; it touches no memory of ours. Skipping destructors and
    // handlers is the point (they may run plugin code while other threads are inside it); the kernel closes every
    // descriptor and reclaims the address space.
    unsafe { libc::_exit(code) }
}
