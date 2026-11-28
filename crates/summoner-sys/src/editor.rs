//! The plugin editor windows: `libsummoner_editor.dylib` (editor/editor.mm, AppKit),
//! loaded with `dlopen` by its absolute build path on first use, never linked. The child's binary therefore links no
//! AppKit, CoreGraphics or libobjc (the release-build link guard), and the scan mode, which never calls
//! [`Editor::load`], never reaches the WindowServer.
//!
//! Main thread only: [`Editor`] is neither `Send` nor `Sync`, and every call into the dylib runs on the thread that
//! loaded it, the child's main (message) thread, where AppKit must live.
use std::cell::Cell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::marker::PhantomData;

/// The dylib's absolute build path, from build.rs: a lab build's only; empty in a shipped build (no build-machine
/// path in a shipped binary; it loads the dylib beside the executable instead).
pub const DYLIB: &str = env!("SUMMONER_EDITOR_DYLIB");

/// This process's real user id (for the host's check that a dylib beside the executable is this user's).
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// A user-resize hook, as the dylib calls it: phase 0 asks (it may change `*w`, `*h`; nonzero allows), phase 1 tells.
pub type ResizeHook =
    unsafe extern "C" fn(ctx: *mut c_void, phase: c_int, w: *mut f64, h: *mut f64) -> c_int;

struct Fns {
    init: unsafe extern "C" fn() -> c_int,
    pump: unsafe extern "C" fn(f64) -> c_int,
    open: unsafe extern "C" fn(f64, f64, c_int, *const c_char) -> *mut c_void,
    au_open: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int) -> *mut c_void,
    show: unsafe extern "C" fn(*mut c_void) -> c_int,
    view: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    set_size: unsafe extern "C" fn(*mut c_void, f64, f64) -> c_int,
    get_size: unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> c_int,
    set_hook: unsafe extern "C" fn(*mut c_void, Option<ResizeHook>, *mut c_void),
    take_close: unsafe extern "C" fn(*mut c_void) -> c_int,
    perform_close: unsafe extern "C" fn(*mut c_void) -> c_int,
    close: unsafe extern "C" fn(*mut c_void) -> c_int,
    count: unsafe extern "C" fn() -> c_int,
    front: unsafe extern "C" fn(*mut c_void) -> c_int,
    is_front: unsafe extern "C" fn(*mut c_void) -> c_int,
    minimize: unsafe extern "C" fn(*mut c_void) -> c_int,
    took_input: unsafe extern "C" fn(*mut c_void) -> c_int,
    post_click: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    set_activate: unsafe extern "C" fn(c_int),
}

/// The loaded dylib (it stays loaded: AppKit cannot be unloaded). Main thread only.
pub struct Editor {
    f: Fns,
    _main_thread_only: PhantomData<*const Cell<()>>,
}

/// One editor window. Closed by [`Editor::close`]; dropping it without closing leaks the window (never freed twice).
#[derive(Debug)]
pub struct Window(*mut c_void);

impl Window {
    /// The window's content view (an `NSView*`), for a plugin's view to attach to; valid while the window is open.
    pub fn content_view(&self, ed: &Editor) -> usize {
        // SAFETY: the handle is a live window of this editor (only `Editor::open` makes one, `close` consumes it).
        unsafe { (ed.f.view)(self.0) as usize }
    }

    /// The handle, as the plugin's resize callback takes it (`ctx` of [`Editor::set_size_fn`]).
    pub fn handle(&self) -> usize {
        self.0 as usize
    }
}

/// Why an editor call failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// `dlopen` or a `dlsym` failed (the message).
    Load(String),
    /// AppKit threw (caught in the dylib) or refused.
    Failed(i32),
}

fn check(r: c_int) -> Result<(), Error> {
    if r == 0 {
        Ok(())
    } else {
        Err(Error::Failed(r))
    }
}

impl Editor {
    /// Loads the dylib at its build path ([`DYLIB`]); see [`Editor::load_from`].
    pub fn load() -> Result<Editor, Error> {
        Editor::load_from(DYLIB)
    }

    /// Loads the editor dylib at the absolute `path` and starts the shared NSApplication (Accessory). Call on the main
    /// thread, once. A missing or foreign file is an `Err` (the caller answers `editor.availability` false).
    pub fn load_from(path: impl AsRef<std::path::Path>) -> Result<Editor, Error> {
        let path = path.as_ref();
        // Absolute under this OS's rule (on macOS exactly a leading `/`).
        if !path.is_absolute() {
            return Err(Error::Load(format!(
                "not an absolute path: {}",
                path.display()
            )));
        }
        let path = path.as_os_str().as_encoded_bytes();
        let path = CString::new(path).map_err(|_| Error::Load("a NUL in the path".into()))?;
        // SAFETY: `path` is a NUL-terminated absolute path to this repo's own build output; RTLD_NOW resolves every
        // symbol before it returns. The handle is never closed (AppKit cannot be unloaded).
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if h.is_null() {
            // SAFETY: dlerror returns a NUL-terminated message owned by libSystem, or null.
            let why = unsafe { libc::dlerror() };
            let why = if why.is_null() {
                "dlopen failed".to_string()
            } else {
                // SAFETY: non-null, NUL-terminated, valid until the next dl* call on this thread.
                unsafe { std::ffi::CStr::from_ptr(why) }
                    .to_string_lossy()
                    .into_owned()
            };
            return Err(Error::Load(why));
        }
        macro_rules! sym {
            ($name:literal, $t:ty) => {{
                let n = concat!($name, "\0");
                // SAFETY: `h` is a live handle; `n` is NUL-terminated.
                let p = unsafe { libc::dlsym(h, n.as_ptr().cast()) };
                if p.is_null() {
                    return Err(Error::Load(format!("no {} in the editor dylib", $name)));
                }
                // SAFETY: the symbol is the dylib's `extern "C"` function of exactly this signature (editor.mm).
                unsafe { std::mem::transmute::<*mut c_void, $t>(p) }
            }};
        }
        let f = Fns {
            init: sym!("summoner_ed_init", unsafe extern "C" fn() -> c_int),
            pump: sym!("summoner_ed_pump", unsafe extern "C" fn(f64) -> c_int),
            open: sym!(
                "summoner_ed_window_open",
                unsafe extern "C" fn(f64, f64, c_int, *const c_char) -> *mut c_void
            ),
            au_open: sym!(
                "summoner_ed_au_window_open",
                unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_int) -> *mut c_void
            ),
            show: sym!(
                "summoner_ed_window_show",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            view: sym!(
                "summoner_ed_window_view",
                unsafe extern "C" fn(*mut c_void) -> *mut c_void
            ),
            set_size: sym!(
                "summoner_ed_window_set_size",
                unsafe extern "C" fn(*mut c_void, f64, f64) -> c_int
            ),
            get_size: sym!(
                "summoner_ed_window_get_size",
                unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> c_int
            ),
            set_hook: sym!(
                "summoner_ed_window_set_hook",
                unsafe extern "C" fn(*mut c_void, Option<ResizeHook>, *mut c_void)
            ),
            take_close: sym!(
                "summoner_ed_window_take_close",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            perform_close: sym!(
                "summoner_ed_window_perform_close",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            close: sym!(
                "summoner_ed_window_close",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            count: sym!("summoner_ed_window_count", unsafe extern "C" fn() -> c_int),
            front: sym!(
                "summoner_ed_window_front",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            is_front: sym!(
                "summoner_ed_window_is_front",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            minimize: sym!(
                "summoner_ed_window_minimize",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            took_input: sym!(
                "summoner_ed_window_took_input",
                unsafe extern "C" fn(*mut c_void) -> c_int
            ),
            post_click: sym!(
                "summoner_ed_window_post_click",
                unsafe extern "C" fn(*mut c_void, c_int) -> c_int
            ),
            set_activate: sym!("summoner_ed_set_activate", unsafe extern "C" fn(c_int)),
        };
        // SAFETY: a function of the dylib, on the main thread (this type is not Send).
        check(unsafe { (f.init)() })?;
        Ok(Editor {
            f,
            _main_thread_only: PhantomData,
        })
    }

    /// Runs the run loop for at most `seconds` (events dispatched, plugin views included).
    pub fn pump(&self, seconds: f64) -> Result<(), Error> {
        // SAFETY: main thread; the dylib guards the call.
        check(unsafe { (self.f.pump)(seconds) })
    }

    /// A window of content size `w` × `h`, not yet on screen.
    pub fn open(&self, w: f64, h: f64, resizable: bool, title: &str) -> Result<Window, Error> {
        let t = CString::new(title.replace('\0', "")).unwrap_or_default();
        // SAFETY: main thread; `t` outlives the call (the dylib copies it).
        let p = unsafe { (self.f.open)(w, h, c_int::from(resizable), t.as_ptr()) };
        if p.is_null() {
            Err(Error::Failed(-1))
        } else {
            Ok(Window(p))
        }
    }

    /// An AU v2's editor window: its Cocoa UI, else CoreAudioKit's generic view, as the content view; not yet on
    /// screen. Returns the window and whether the generic view was used.
    ///
    /// The caller guarantees `unit` is a live `AudioUnit` and stays so until the window is closed (the host closes an
    /// AU's window before it releases the plugin).
    pub fn open_au(&self, unit: usize, title: &str) -> Result<(Window, bool), Error> {
        let t = CString::new(title.replace('\0', "")).unwrap_or_default();
        let mut generic: c_int = 0;
        // SAFETY: main thread; `unit` is a live AudioUnit by the contract above (the dylib's view keeps using it until
        // the window is closed); `t` and `generic` outlive the call.
        let p = unsafe { (self.f.au_open)(unit as *mut c_void, t.as_ptr(), &mut generic) };
        if p.is_null() {
            Err(Error::Failed(-1))
        } else {
            Ok((Window(p), generic == 1))
        }
    }

    /// Orders the window front and makes it key (after the plugin's view is attached).
    pub fn show(&self, w: &Window) -> Result<(), Error> {
        // SAFETY: a live window of this editor.
        check(unsafe { (self.f.show)(w.0) })
    }

    /// Sets the window's content size, in points (a plugin's own resize: the resize hook is not called).
    pub fn set_size(&self, w: &Window, width: f64, height: f64) -> Result<(), Error> {
        // SAFETY: a live window of this editor.
        check(unsafe { (self.f.set_size)(w.0, width, height) })
    }

    /// The window's content size `(width, height)`, in points.
    pub fn size(&self, w: &Window) -> Result<(f64, f64), Error> {
        let (mut a, mut b) = (0.0, 0.0);
        // SAFETY: a live window; both out-pointers are live locals.
        check(unsafe { (self.f.get_size)(w.0, &mut a, &mut b) })?;
        Ok((a, b))
    }

    /// The window's user-resize hook (`ctx` passed back to it), or none.
    ///
    /// The caller guarantees `ctx` stays valid for `hook` until the window is closed or the hook replaced: it is a
    /// plugin record that outlives its editor window (the host closes the window before it releases the plugin).
    pub fn set_hook(&self, w: &Window, hook: Option<ResizeHook>, ctx: usize) {
        // SAFETY: a live window; the dylib stores the pointer and calls `hook` only on the main thread, while the
        // window is open (the contract above keeps `ctx` valid for that long).
        unsafe { (self.f.set_hook)(w.0, hook, ctx as *mut c_void) }
    }

    /// The window's size setter, as a plain C function for a plugin's `IPlugFrame::resizeView` (`ctx` is
    /// [`Window::handle`]).
    pub fn set_size_fn(&self) -> unsafe extern "C" fn(*mut c_void, f64, f64) -> c_int {
        self.f.set_size
    }

    /// The window's size getter, as a plain C function (`ctx` is [`Window::handle`]).
    pub fn get_size_fn(&self) -> unsafe extern "C" fn(*mut c_void, *mut f64, *mut f64) -> c_int {
        self.f.get_size
    }

    /// Whether the close button was pressed since the last call.
    pub fn take_close(&self, w: &Window) -> bool {
        // SAFETY: a live window of this editor.
        unsafe { (self.f.take_close)(w.0) == 1 }
    }

    /// The close button pressed as AppKit presses it (`performClose:`): the request is then taken by
    /// [`Editor::take_close`], the user's path (a test hook's use).
    pub fn perform_close(&self, w: &Window) -> Result<(), Error> {
        // SAFETY: a live window of this editor; main thread.
        check(unsafe { (self.f.perform_close)(w.0) })
    }

    /// An open window brought to front again (de-miniaturized if it was), not a new open: a second `editor.open`
    /// (editor-windows.md gap 1).
    pub fn front(&self, w: &Window) -> Result<(), Error> {
        // SAFETY: a live window of this editor; main thread.
        check(unsafe { (self.f.front)(w.0) })
    }

    /// Whether the window is the front one of the app's ordered windows (a test's check).
    pub fn is_front(&self, w: &Window) -> bool {
        // SAFETY: a live window of this editor; main thread.
        unsafe { (self.f.is_front)(w.0) == 1 }
    }

    /// The minimise button pressed as AppKit presses it (`performMiniaturize:`); whether the window is then
    /// miniaturized (a test hook's use, editor-windows.md gap 2).
    pub fn minimize(&self, w: &Window) -> bool {
        // SAFETY: a live window of this editor; main thread.
        unsafe { (self.f.minimize)(w.0) == 1 }
    }

    /// Whether a shown or re-fronted window activates this process (default on: the plugin takes the keyboard, as a
    /// plugin editor window should; off for the lab's tests, which open windows on a screen in use).
    pub fn set_activate(&self, on: bool) {
        // SAFETY: a function of the dylib, on the main thread; it stores an atomic.
        unsafe { (self.f.set_activate)(c_int::from(on)) }
    }

    /// Whether the user sent the window a mouse, key or scroll event since it opened (the editor-reset guard's
    /// divergence 1: a real edit is never reverted).
    pub fn took_input(&self, w: &Window) -> bool {
        // SAFETY: a live window of this editor (the flag is an atomic the dylib's input monitor sets on this thread).
        unsafe { (self.f.took_input)(w.0) == 1 }
    }

    /// A user's click posted to the window (a test hook's use): delivered by the next pump, seen by the input monitor;
    /// in the content view, or with `title_bar` in the title bar (which is not an edit).
    pub fn post_click(&self, w: &Window, title_bar: bool) -> Result<(), Error> {
        // SAFETY: a live window of this editor; main thread.
        check(unsafe { (self.f.post_click)(w.0, c_int::from(title_bar)) })
    }

    /// Ordered out and freed (after the plugin's view was removed).
    pub fn close(&self, w: Window) -> Result<(), Error> {
        // SAFETY: a live window, consumed here (never used again).
        check(unsafe { (self.f.close)(w.0) })
    }

    /// The editor windows on screen now.
    pub fn count(&self) -> u32 {
        // SAFETY: main thread; reads an atomic.
        unsafe { (self.f.count)() }.max(0) as u32
    }
}
