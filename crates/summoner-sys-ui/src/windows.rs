//! `sys::windows` (DRAFT, unused until accepted): the windows a process owns, for the scan child's no-UI watchdog
//! (`failed {ui}`) and the proof that the window fixture's window exists although nobody can see it.
//!
//! No privacy prompt: [`windows_of`] reads ONLY each window's `kCGWindowOwnerPID`, `kCGWindowLayer` and
//! `kCGWindowBounds`. It never reads `kCGWindowName`, and no image API is called, so nothing here needs Screen
//! Recording permission. (`CGWindowListCopyWindowInfo` itself does not prompt: without the permission, windows'
//! names are simply absent from its dictionaries, and we never ask for them.)
//!
//! `kCGWindowListOptionAll` lists on- and off-screen windows alike, so an alpha-0 or off-screen window still counts
//! as a window. Called from a watchdog thread of its own every 250 ms, never from an audio thread.
use std::ffi::c_void;

type CFTypeRef = *const c_void;

/// One window of the process: its layer and its bounds (x, y, width, height, in global display points).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    pub layer: i32,
    pub bounds: (f64, f64, f64, f64),
}

#[repr(C)]
#[derive(Default)]
struct CGRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// `kCGWindowListOptionAll`: every window, on screen or not.
const OPTION_ALL: u32 = 0;
/// `kCGNullWindowID`.
const NULL_WINDOW: u32 = 0;
/// `kCFNumberSInt32Type`.
const SINT32: isize = 3;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFTypeRef, rect: *mut CGRect) -> bool;
    static kCGWindowOwnerPID: CFTypeRef;
    static kCGWindowLayer: CFTypeRef;
    static kCGWindowBounds: CFTypeRef;
}

// The five CoreFoundation calls this module needs (declared here, one module per FFI surface; `au` has its own).
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetCount(a: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> bool;
}

/// A CF number in `dict` under `key` as an `i32`, if present and representable.
///
/// # Safety
/// `dict` is a valid CFDictionary; `key` a valid CFString.
unsafe fn i32_at(dict: CFTypeRef, key: CFTypeRef) -> Option<i32> {
    // SAFETY: the caller's dictionary and key; CFDictionaryGetValue returns the value (not retained) or null.
    let n = unsafe { CFDictionaryGetValue(dict, key) };
    if n.is_null() {
        return None;
    }
    let mut v: i32 = 0;
    // SAFETY: `n` is the window-info dictionary's CFNumber for this key (CoreGraphics documents both keys as
    // CFNumbers); `v` is a valid i32 for kCFNumberSInt32Type to write.
    let ok = unsafe { CFNumberGetValue(n, SINT32, (&mut v as *mut i32).cast()) };
    ok.then_some(v)
}

/// Every window `pid` owns, on screen or off, with its layer and bounds; nothing else is read (no privacy prompt).
pub fn windows_of(pid: i32) -> Vec<Window> {
    // SAFETY: CGWindowListCopyWindowInfo has no preconditions; it returns a new CFArray (which we release) or null.
    let list = unsafe { CGWindowListCopyWindowInfo(OPTION_ALL, NULL_WINDOW) };
    if list.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // SAFETY: `list` is the CFArray returned above, alive until the CFRelease below.
    let n = unsafe { CFArrayGetCount(list) };
    for i in 0..n {
        // SAFETY: `i` is in 0..count of `list`; each element is a CFDictionary (CoreGraphics' window-info format).
        let d = unsafe { CFArrayGetValueAtIndex(list, i) };
        if d.is_null() {
            continue;
        }
        // SAFETY: `d` is a window-info dictionary; kCGWindowOwnerPID is CoreGraphics' own constant CFString key.
        if unsafe { i32_at(d, kCGWindowOwnerPID) } != Some(pid) {
            continue;
        }
        // SAFETY: as above, for kCGWindowLayer.
        let layer = unsafe { i32_at(d, kCGWindowLayer) }.unwrap_or(0);
        let mut r = CGRect::default();
        // SAFETY: `d` and kCGWindowBounds as above; the bounds value is a CFDictionary (or null, which
        // CGRectMakeWithDictionaryRepresentation rejects); `r` is a valid CGRect to write.
        let have = unsafe {
            let b = CFDictionaryGetValue(d, kCGWindowBounds);
            !b.is_null() && CGRectMakeWithDictionaryRepresentation(b, &mut r)
        };
        out.push(Window {
            layer,
            bounds: if have {
                (r.x, r.y, r.w, r.h)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            },
        });
    }
    // SAFETY: `list` came from a Copy function (we own one reference) and is released exactly once, after its last
    // use above.
    unsafe { CFRelease(list) };
    out
}
