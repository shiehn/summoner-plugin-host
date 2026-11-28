//! The allocation counter (`target.noindex/plugins/libsummoner_lab_malloccount.dylib`), when it is injected into this
//! process: arm it on the calling thread, run the code under test, disarm it for the count. Absent (not injected),
//! [`arm`] says so and nothing is counted.
use std::ffi::c_void;

fn symbol(name: &std::ffi::CStr) -> Option<*mut c_void> {
    // SAFETY: dlsym with RTLD_DEFAULT and a NUL-terminated name only looks a symbol up.
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!p.is_null()).then_some(p)
}

/// Starts counting allocations made on this thread; false when the counter is not injected.
pub fn arm() -> bool {
    let Some(p) = symbol(c"summoner_malloccount_arm") else {
        return false;
    };
    // SAFETY: the symbol is the counter's `void summoner_malloccount_arm (void)` (testkit/plugins/malloccount), a C function
    // with no argument and no result.
    let f: extern "C" fn() = unsafe { std::mem::transmute::<*mut c_void, extern "C" fn()>(p) };
    f();
    true
}

/// Stops counting; the allocations made on the armed thread since [`arm`], or `None` without the counter.
pub fn disarm() -> Option<u64> {
    let p = symbol(c"summoner_malloccount_disarm")?;
    // SAFETY: the symbol is the counter's `uint64_t summoner_malloccount_disarm (void)`.
    let f: extern "C" fn() -> u64 =
        unsafe { std::mem::transmute::<*mut c_void, extern "C" fn() -> u64>(p) };
    Some(f())
}

/// The counter's two entry points, resolved once (on the message thread), so a realtime thread can arm and disarm it
/// without a `dlsym` inside the counted window (for the host child's first-block allocation check).
#[derive(Clone, Copy)]
pub struct Counter {
    arm: extern "C" fn(),
    disarm: extern "C" fn() -> u64,
}

impl Counter {
    /// `None` when the counter is not injected.
    pub fn resolve() -> Option<Counter> {
        let a = symbol(c"summoner_malloccount_arm")?;
        let d = symbol(c"summoner_malloccount_disarm")?;
        // SAFETY: the symbols are the counter's `void summoner_malloccount_arm (void)` and `uint64_t summoner_malloccount_disarm
        // (void)` (testkit/plugins/malloccount), C functions with these signatures, in an image that stays loaded
        // (injected at process start).
        unsafe {
            Some(Counter {
                arm: std::mem::transmute::<*mut c_void, extern "C" fn()>(a),
                disarm: std::mem::transmute::<*mut c_void, extern "C" fn() -> u64>(d),
            })
        }
    }

    /// Starts counting on this thread.
    pub fn arm(&self) {
        (self.arm)();
    }

    /// Stops counting: the allocations on this thread since [`Counter::arm`].
    pub fn disarm(&self) -> u64 {
        (self.disarm)()
    }
}
