//! `sys-ui::nsapp` (the activation-policy check; DRAFT, behind the feature `nsapp`, enabled by nothing until
//! accepted): the activation policy of an NSApplication that SOMETHING ELSE made in this
//! process (a plugin, while it was instantiated or prepared), without making one and without linking AppKit.
//!
//! Everything is found at run time with `dlsym(RTLD_DEFAULT, …)` in the images already loaded:
//! - the `NSApp` global (AppKit's `extern NSApplication *NSApp`): if AppKit is not loaded, the symbol does not resolve
//!   and there is no application (`None`); if it is loaded but `NSApp` is nil, there is none either (`None`);
//! - only then `sel_registerName` and `objc_msgSend` (libobjc, loaded with AppKit), and one message,
//!   `[NSApp activationPolicy]` (`NSInteger (*)(id, SEL)`), the selector `appkit` sends.
//!
//! No class is looked up and `sharedApplication` is never sent, so nothing is created; nothing is loaded (`dlsym` on
//! RTLD_DEFAULT only searches what is already there). Call it on the main thread, after the scan's stages.
use std::ffi::{CStr, c_char, c_void};

unsafe extern "C" {
    // libSystem's dlsym; RTLD_DEFAULT is ((void *) -2) on macOS (<dlfcn.h>).
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

fn sym(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: dlsym takes a handle (RTLD_DEFAULT, a documented pseudo-handle) and a NUL-terminated name; it only
    // searches the images already loaded and returns the symbol's address or null.
    let p = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
    (!p.is_null()).then_some(p)
}

/// The activation policy of the existing NSApplication, or `None` when this process has none (AppKit not loaded, or
/// `NSApp` nil). Never creates one.
pub fn existing_app_policy() -> Option<i64> {
    let global = sym(c"NSApp")?;
    // SAFETY: `global` is the address of AppKit's `NSApp` variable (an object pointer), resolved by name in the loaded
    // AppKit image; reading it is a plain load of a pointer-sized value AppKit keeps for the process's lifetime.
    let app = unsafe { *(global as *const *mut c_void) };
    if app.is_null() {
        return None;
    }
    let reg = sym(c"sel_registerName")?;
    let send = sym(c"objc_msgSend")?;
    // SAFETY: `reg` is libobjc's sel_registerName (`SEL (*)(const char *)`), resolved by name; its argument is a
    // NUL-terminated selector name; it returns the interned selector.
    let sel = unsafe {
        let f: unsafe extern "C" fn(*const c_char) -> *mut c_void = std::mem::transmute(reg);
        f(c"activationPolicy".as_ptr())
    };
    // SAFETY: `send` is libobjc's objc_msgSend, called with the exact signature of -[NSApplication activationPolicy],
    // `NSInteger (*)(id, SEL)` (the arm64 ABI needs the cast); `app` is the non-nil NSApp, an NSApplication.
    let policy = unsafe {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> i64 = std::mem::transmute(send);
        f(app, sel)
    };
    Some(policy)
}
