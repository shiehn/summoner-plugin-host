//! `sys::appkit` (DRAFT, unused until accepted): the scan child's
//! `NSApplication`, through the Objective-C runtime (no Objective-C compiled here, no window, no run loop of ours).
//!
//! - [`prohibit_activation`]: `[NSApplication sharedApplication]`, then `setActivationPolicy:` Prohibited (2), read
//!   back. Called on the MAIN thread, BEFORE any plugin code runs; `activate` is never called anywhere. A
//!   Prohibited app has no Dock tile, no menu bar and cannot become frontmost.
//! - [`own_activation_policy`]: the same read-back, after validation: a plugin may have undone it.
//! - [`frontmost_pid`]: `[[NSWorkspace sharedWorkspace] frontmostApplication].processIdentifier`, for the no-UI
//!   checks (the frontmost app must not change across a scan). Read-only.
//! - [`activation_policy_of`]: `[NSRunningApplication runningApplicationWithProcessIdentifier:].activationPolicy`, for
//!   the "no Dock tile" check from the test side (Regular = 0 is the only policy with a Dock tile). Read-only.
//!
//! Every call goes through `objc_msgSend` cast to the exact C signature of the method sent (the arm64 ABI requires
//! the cast: `objc_msgSend` is not variadic there). Classes and selectors are looked up by name each call: these are
//! called a handful of times per process, never on an audio thread.
use std::ffi::{CStr, c_void};

type Id = *mut c_void;
type Sel = *mut c_void;

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::ffi::c_char) -> Id;
    fn sel_registerName(name: *const std::ffi::c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// An autorelease pool around each call of this module, so the autoreleased objects AppKit returns
/// (`frontmostApplication`, `runningApplicationWithProcessIdentifier:`) are freed on a thread that has no pool of its
/// own, and nothing is logged. Popped on drop, on the thread that pushed it.
struct Pool(*mut c_void);

impl Pool {
    fn push() -> Pool {
        // SAFETY: objc_autoreleasePoolPush has no preconditions; it returns the pool's token for the matching pop.
        Pool(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: the token from this pool's own push, popped once, on the same thread (a Pool is neither Send nor
        // Sync: it holds a raw pointer), after every object the call autoreleased.
        unsafe { objc_autoreleasePoolPop(self.0) }
    }
}

// AppKit holds NSApplication / NSWorkspace / NSRunningApplication; linking it loads it (and Foundation).
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

/// `NSApplicationActivationPolicyProhibited`.
pub const POLICY_PROHIBITED: i64 = 2;
/// `NSApplicationActivationPolicyRegular`: the only policy with a Dock tile.
pub const POLICY_REGULAR: i64 = 0;

fn class(name: &CStr) -> Option<Id> {
    // SAFETY: objc_getClass takes a NUL-terminated C string (a CStr) and returns the class or null; no other
    // precondition.
    let c = unsafe { objc_getClass(name.as_ptr()) };
    (!c.is_null()).then_some(c)
}

fn sel(name: &CStr) -> Sel {
    // SAFETY: sel_registerName takes a NUL-terminated C string and returns the (interned) selector; never null.
    unsafe { sel_registerName(name.as_ptr()) }
}

/// `[receiver sel]` for a method returning an object (`id (*)(id, SEL)`).
///
/// # Safety
/// `receiver` is a valid object (or class) that responds to `sel` with that exact signature.
unsafe fn send_id(receiver: Id, sel: Sel) -> Id {
    // SAFETY: objc_msgSend called with the signature of the method being sent, as the arm64 ABI requires; the caller
    // guarantees the receiver responds to `sel` with `id (*)(id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(receiver, sel)
    }
}

/// `[receiver sel]` for a method returning an `NSInteger` (`long (*)(id, SEL)`).
///
/// # Safety
/// As [`send_id`], for `long (*)(id, SEL)`.
unsafe fn send_long(receiver: Id, sel: Sel) -> i64 {
    // SAFETY: as send_id, with the method's own signature `long (*)(id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> i64 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(receiver, sel)
    }
}

/// The shared NSApplication's activation policy set to Prohibited, and read back. Call on the main thread, before
/// any plugin code. `Err` names the step that failed (the scan child then refuses to scan: a scan that could show a
/// Dock tile does not run).
pub fn prohibit_activation() -> Result<(), &'static str> {
    let _pool = Pool::push();
    let app_class = class(c"NSApplication").ok_or("no NSApplication class")?;
    // SAFETY: +[NSApplication sharedApplication] returns the shared application object (`id (*)(id, SEL)`).
    let app = unsafe { send_id(app_class, sel(c"sharedApplication")) };
    if app.is_null() {
        return Err("no shared NSApplication");
    }
    // SAFETY: -[NSApplication setActivationPolicy:] has the signature `BOOL (*)(id, SEL, NSInteger)`; `app` is the
    // shared application from above.
    let set = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, i64) -> bool =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(app, sel(c"setActivationPolicy:"), POLICY_PROHIBITED)
    };
    if !set {
        return Err("setActivationPolicy: refused");
    }
    if own_policy(app) != POLICY_PROHIBITED {
        return Err("the activation policy did not stay Prohibited");
    }
    Ok(())
}

/// `[app activationPolicy]` of the shared application `app`.
fn own_policy(app: Id) -> i64 {
    // SAFETY: -[NSApplication activationPolicy] is `NSInteger (*)(id, SEL)`; `app` is the shared application.
    unsafe { send_long(app, sel(c"activationPolicy")) }
}

/// This process's own activation policy, re-read after validation (a plugin may have changed it), or
/// `None` if there is no shared application. The scan child's policy reader in production.
pub fn own_activation_policy() -> Option<i64> {
    let _pool = Pool::push();
    let app_class = class(c"NSApplication")?;
    // SAFETY: +[NSApplication sharedApplication] is `id (*)(id, SEL)`; called only after prohibit_activation created
    // it on the main thread, so here it only returns the existing object.
    let app = unsafe { send_id(app_class, sel(c"sharedApplication")) };
    (!app.is_null()).then(|| own_policy(app))
}

/// The frontmost application's pid (the no-UI checks), or `None` if there is none.
pub fn frontmost_pid() -> Option<i32> {
    let _pool = Pool::push();
    let ws_class = class(c"NSWorkspace")?;
    // SAFETY: +[NSWorkspace sharedWorkspace] is `id (*)(id, SEL)`.
    let ws = unsafe { send_id(ws_class, sel(c"sharedWorkspace")) };
    if ws.is_null() {
        return None;
    }
    // SAFETY: -[NSWorkspace frontmostApplication] is `id (*)(id, SEL)`, returning an NSRunningApplication or nil.
    let app = unsafe { send_id(ws, sel(c"frontmostApplication")) };
    if app.is_null() {
        return None;
    }
    // SAFETY: -[NSRunningApplication processIdentifier] is `pid_t (*)(id, SEL)` (an `int`); `app` is non-nil.
    let pid = unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> i32 =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(app, sel(c"processIdentifier"))
    };
    Some(pid)
}

/// The activation policy of the running application `pid` (the Dock-tile check: [`POLICY_REGULAR`] has one), or
/// `None` if `pid` is not a running application LaunchServices knows (a plain process with no NSApplication).
pub fn activation_policy_of(pid: i32) -> Option<i64> {
    let _pool = Pool::push();
    let ra_class = class(c"NSRunningApplication")?;
    // SAFETY: +[NSRunningApplication runningApplicationWithProcessIdentifier:] is `id (*)(id, SEL, pid_t)`, returning
    // the application or nil.
    let app = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, i32) -> Id =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        f(
            ra_class,
            sel(c"runningApplicationWithProcessIdentifier:"),
            pid,
        )
    };
    if app.is_null() {
        return None;
    }
    // SAFETY: -[NSRunningApplication activationPolicy] is `NSInteger (*)(id, SEL)`; `app` is non-nil.
    Some(unsafe { send_long(app, sel(c"activationPolicy")) })
}
