// libsummoner_editor.dylib: the plugin editor windows, AppKit on the child's main thread
// and only there. A dylib of its own, loaded by the live child (dlopen, by its absolute path) on its first
// editor.open, so the child's binary links no AppKit, CoreGraphics or libobjc (release_build.rs's link guard) and the
// scan mode never touches the WindowServer.
//
// Every entry point is called on the main thread (the message thread), and every one runs inside @try / try: pumping
// the run loop dispatches into plugin views, so it is a plugin call. No entry point blocks beyond its timeout.
//
// - The windows are titled and closable, resizable only when asked, never released when closed, at
//   NSFloatingWindowLevel (today's editors stay on top), their content view a plain host NSView.
// - The application is the shared NSApplication, activation policy Accessory (no Dock icon, no menu bar).
// - A close button asks; it does not close: windowShouldClose: records the request and answers NO, and the host
//   closes the editor itself (every close runs the same path, then editor.closed).
// - A user resize goes through the window's hook (the plugin's checkSizeConstraint, then onSize).
#import <AppKit/AppKit.h>
#import <AudioToolbox/AudioToolbox.h>
#import <AudioUnit/AUCocoaUIView.h>
#import <CoreAudioKit/CoreAudioKit.h>

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <vector>

extern "C" {
// phase 0: may the content become *w × *h (the hook may change them; nonzero = allowed); phase 1: it did.
typedef int (*summoner_ed_resize_hook)(void* ctx, int phase, double* w, double* h);
}

@interface SasEdDelegate : NSObject <NSWindowDelegate>
@property(atomic) BOOL closeRequested;
@property(nonatomic) summoner_ed_resize_hook hook;
@property(nonatomic) void* hookCtx;
@end

@implementation SasEdDelegate
- (BOOL)windowShouldClose:(NSWindow*)sender {
    (void)sender;
    self.closeRequested = YES;
    return NO;
}
- (NSSize)windowWillResize:(NSWindow*)w toSize:(NSSize)frameSize {
    if (self.hook == nullptr)
        return frameSize;
    NSRect content = [w contentRectForFrameRect:NSMakeRect(0, 0, frameSize.width, frameSize.height)];
    double cw = content.size.width, ch = content.size.height;
    if (self.hook(self.hookCtx, 0, &cw, &ch) == 0)
        return w.frame.size;  // refused: keep the size it has
    NSRect frame = [w frameRectForContentRect:NSMakeRect(0, 0, cw, ch)];
    return frame.size;
}
- (void)windowDidResize:(NSNotification*)n {
    NSWindow* w = n.object;
    if (self.hook == nullptr)
        return;
    NSSize s = w.contentView.frame.size;
    double cw = s.width, ch = s.height;
    self.hook(self.hookCtx, 1, &cw, &ch);
}
@end

namespace {
std::atomic<int> gOpen{0};

struct Win {
    NSWindow* window = nil;
    SasEdDelegate* delegate = nil;
    bool hookActive = true;
    id frameObserver = nil;  // an AU view's frame changes resize the window (AU v2 views size themselves)
    bool shown = false;      // counted in gOpen (shown once); a minimised window is not `visible` but still counts
    id inputMonitor = nil;   // the reset guard's: any user mouse / key event in this window
    std::atomic<bool> userInput{false};
};

// The reset guard's input record: a local monitor of the user's input to the plugin's view (the run loop's, so on
// the main thread); the event passes on unchanged. Only what reaches the content view counts: a mouse-down or a
// scroll whose point is inside the content view, or a key-down while this window is key. A title-bar drag,
// the window buttons or a stray scroll over the frame is not an edit.
static void watchInput(Win* w) {
    Win* const target = w;
    __weak NSWindow* weakWin = w->window;
    const NSEventMask mask = NSEventMaskLeftMouseDown | NSEventMaskRightMouseDown | NSEventMaskOtherMouseDown |
                             NSEventMaskKeyDown | NSEventMaskScrollWheel;
    w->inputMonitor = [NSEvent addLocalMonitorForEventsMatchingMask:mask
                                                            handler:^NSEvent*(NSEvent* e) {
                                                              NSWindow* win = weakWin;
                                                              if (e.window == nil || e.window != win)
                                                                  return e;
                                                              if (e.type == NSEventTypeKeyDown) {
                                                                  if (win.keyWindow)
                                                                      target->userInput.store(true);
                                                                  return e;
                                                              }
                                                              NSView* content = win.contentView;
                                                              const NSPoint p = [content convertPoint:e.locationInWindow
                                                                                             fromView:nil];
                                                              if (NSPointInRect(p, content.bounds))
                                                                  target->userInput.store(true);
                                                              return e;
                                                            }];
}

// AU v2: the view the AU names in kAudioUnitProperty_CocoaUI (its bundle's class, conforming to AUCocoaUIBase,
// uiViewForAudioUnit:withSize:), or nil.
NSView* cocoaView(AudioUnit unit) {
    UInt32 size = 0;
    Boolean writable = false;
    if (AudioUnitGetPropertyInfo(unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, &size, &writable) != noErr
        || size < sizeof(AudioUnitCocoaViewInfo))
        return nil;
    std::vector<uint8_t> buf(size);
    auto* info = reinterpret_cast<AudioUnitCocoaViewInfo*>(buf.data());
    if (AudioUnitGetProperty(unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0, info, &size) != noErr)
        return nil;
    NSURL* url = (__bridge_transfer NSURL*)info->mCocoaAUViewBundleLocation;
    const UInt32 classes = (size - offsetof(AudioUnitCocoaViewInfo, mCocoaAUViewClass)) / sizeof(CFStringRef);
    NSString* name = classes > 0 ? (__bridge_transfer NSString*)info->mCocoaAUViewClass[0] : nil;
    for (UInt32 i = 1; i < classes; ++i)
        CFRelease(info->mCocoaAUViewClass[i]);
    if (url == nil || name == nil)
        return nil;
    NSBundle* bundle = [NSBundle bundleWithURL:url];
    Class c = [bundle classNamed:name];
    if (c == nil || ![c conformsToProtocol:@protocol(AUCocoaUIBase)])
        return nil;
    id<AUCocoaUIBase> factory = [c new];
    return [factory uiViewForAudioUnit:unit withSize:NSMakeSize(400, 300)];
}

template <class F>
int guarded(F&& f) {
    @try {
        try {
            return f();
        } catch (...) {
            return -2;
        }
    } @catch (NSException* e) {
        (void)e;
        return -1;
    }
}
}  // namespace

extern "C" {

__attribute__((visibility("default"))) int summoner_ed_init(void) {
    return guarded([] {
        NSApplication* app = [NSApplication sharedApplication];
        [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
        if (![app isRunning])
            [app finishLaunching];
        return 0;
    });
}

// Runs the main thread's run loop for at most `seconds`: every pending event is dispatched (into plugin views too),
// and the call returns when none is left or the time is up. 0 ok; < 0 something threw and was caught.
__attribute__((visibility("default"))) int summoner_ed_pump(double seconds) {
    return guarded([seconds] {
        @autoreleasepool {
            NSDate* until = [NSDate dateWithTimeIntervalSinceNow:seconds];
            for (;;) {
                NSEvent* e = [NSApp nextEventMatchingMask:NSEventMaskAny
                                                untilDate:until
                                                   inMode:NSDefaultRunLoopMode
                                                  dequeue:YES];
                if (e == nil)
                    break;
                [NSApp sendEvent:e];
                until = [NSDate dateWithTimeIntervalSinceNow:0];  // drain what is queued, then return
            }
            [NSApp updateWindows];
            // Then the main run loop's sources and timers once, without waiting: an AU's parameter notifications
            // (AUEventListener on the main run loop) and a view's dispatch_after are delivered there, so they are
            // served between events as well as by the event drain (not shown to be required: the
            // flake that prompted it was the test's own read).
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0, false);
        }
        return 0;
    });
}

// A window of content size w × h. Returns its handle, or nullptr.
__attribute__((visibility("default"))) void* summoner_ed_window_open(double w, double h, int resizable, const char* title) {
    Win* out = nullptr;
    const int r = guarded([&] {
        // Close and minimise buttons; no zoom.
        NSWindowStyleMask style =
            NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskMiniaturizable;
        if (resizable)
            style |= NSWindowStyleMaskResizable;
        NSWindow* win = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, w, h)
                                                    styleMask:style
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        win.releasedWhenClosed = NO;
        win.level = NSFloatingWindowLevel;
        win.title = [NSString stringWithUTF8String:(title != nullptr ? title : "")] ?: @"";
        win.contentView = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, w, h)];
        auto* d = [SasEdDelegate new];
        win.delegate = d;
        [win center];
        out = new Win{win, d};
        watchInput(out);
        return 0;
    });
    return r == 0 ? out : nullptr;
}

// AU v2: a window whose content view IS the AU's view: its Cocoa UI, else CoreAudioKit's AUGenericView (the
// fallback). Sized to the view's frame (400 x 300 if it has none yet), the
// window following the view's own frame changes. *generic: 1 when the generic view was used. Not yet on screen.
__attribute__((visibility("default"))) void* summoner_ed_au_window_open(void* au, const char* title, int* generic) {
    Win* out = nullptr;
    const int r = guarded([&] {
        auto unit = static_cast<AudioUnit>(au);
        NSView* v = cocoaView(unit);
        *generic = v == nil ? 1 : 0;
        if (v == nil) {
            AUGenericView* g = [[AUGenericView alloc] initWithAudioUnit:unit];
            g.showsExpertParameters = YES;
            v = g;
        }
        NSSize sz = v.frame.size;
        if (sz.width < 1 || sz.height < 1)
            sz = NSMakeSize(400, 300);
        NSWindow* win = [[NSWindow alloc] initWithContentRect:NSMakeRect(0, 0, sz.width, sz.height)
                                                    styleMask:NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                                                              NSWindowStyleMaskMiniaturizable
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        win.releasedWhenClosed = NO;
        win.level = NSFloatingWindowLevel;
        win.title = [NSString stringWithUTF8String:(title != nullptr ? title : "")] ?: @"";
        win.contentView = v;
        auto* d = [SasEdDelegate new];
        win.delegate = d;
        [win center];
        out = new Win{win, d};
        watchInput(out);
        v.postsFrameChangedNotifications = YES;
        __weak NSWindow* weakWin = win;
        out->frameObserver = [[NSNotificationCenter defaultCenter]
            addObserverForName:NSViewFrameDidChangeNotification
                        object:v
                         queue:nil
                    usingBlock:^(NSNotification* n) {
                      NSView* view = n.object;
                      NSWindow* w = weakWin;
                      if (w != nil && !NSEqualSizes(w.contentView.frame.size, view.frame.size))
                          [w setContentSize:view.frame.size];
                    }];
        return 0;
    });
    return r == 0 ? out : nullptr;
}

// A shown or re-fronted editor activates this process, so the plugin's window takes the keyboard (typing into its
// fields works). Off only for the lab's tests, which open windows on a screen someone is using
// (summoner_ed_set_activate). Handing focus back on close is the app's (it
// re-activates).
static std::atomic<bool> gActivate{true};

__attribute__((visibility("default"))) void summoner_ed_set_activate(int on) {
    gActivate.store(on != 0);
}

static void activateApp() {
    if (!gActivate.load())
        return;
    if (@available(macOS 14.0, *))
        [NSApp activate];
    else
        [NSApp activateIgnoringOtherApps:YES];
}

// Ordered front (after the plugin's view is attached to the content view).
__attribute__((visibility("default"))) int summoner_ed_window_show(void* handle) {
    auto* w = static_cast<Win*>(handle);
    return guarded([w] {
        activateApp();
        [w->window makeKeyAndOrderFront:nil];
        if (!w->shown) {
            w->shown = true;
            gOpen.fetch_add(1);
        }
        return 0;
    });
}

// An open window brought to front again (a second editor.open):
// de-miniaturized if it was, then ordered front and made key. Not a new open (gOpen unchanged).
__attribute__((visibility("default"))) int summoner_ed_window_front(void* handle) {
    auto* w = static_cast<Win*>(handle);
    return guarded([w] {
        if (w->window.miniaturized)
            [w->window deminiaturize:nil];
        activateApp();
        [w->window makeKeyAndOrderFront:nil];
        return 0;
    });
}

// 1 when the window is the first of the app's ordered windows (front-to-back), else 0 (a test's check of the
// bring-to-front path).
__attribute__((visibility("default"))) int summoner_ed_window_is_front(void* handle) {
    auto* w = static_cast<Win*>(handle);
    int front = 0;
    guarded([w, &front] {
        front = [NSApp orderedWindows].firstObject == w->window ? 1 : 0;
        return 0;
    });
    return front;
}

// The minimise button pressed, as AppKit presses it (performMiniaturize:); 1 when the window is then miniaturized
// (it only can be with NSWindowStyleMaskMiniaturizable: a test's check of the minimise button).
__attribute__((visibility("default"))) int summoner_ed_window_minimize(void* handle) {
    auto* w = static_cast<Win*>(handle);
    int done = 0;
    guarded([w, &done] {
        [w->window performMiniaturize:nil];
        done = w->window.miniaturized ? 1 : 0;
        return 0;
    });
    return done;
}

// 1 when the user sent this window a mouse, key or scroll event since it opened (the reset guard's input record).
__attribute__((visibility("default"))) int summoner_ed_window_took_input(void* handle) {
    return static_cast<Win*>(handle)->userInput.load() ? 1 : 0;
}

// A test's user click: a left mouse down and up posted to the app's queue for this window (the run loop delivers them,
// the input monitor sees them): at the content view's centre, or with title_bar 1 in the title bar (above the content
// view, inside the frame: a click that must not count as an edit).
__attribute__((visibility("default"))) int summoner_ed_window_post_click(void* handle, int title_bar) {
    auto* w = static_cast<Win*>(handle);
    return guarded([w, title_bar] {
        const NSRect f = w->window.contentView.frame;
        const CGFloat frameH = w->window.frame.size.height;
        const NSPoint p = title_bar != 0 ? NSMakePoint(NSMidX(f), (NSMaxY(f) + frameH) / 2)
                                         : NSMakePoint(NSMidX(f), NSMidY(f));
        for (NSEventType t : {NSEventTypeLeftMouseDown, NSEventTypeLeftMouseUp}) {
            NSEvent* e = [NSEvent mouseEventWithType:t
                                            location:p
                                       modifierFlags:0
                                           timestamp:[NSProcessInfo processInfo].systemUptime
                                        windowNumber:w->window.windowNumber
                                             context:nil
                                         eventNumber:0
                                          clickCount:1
                                            pressure:1.0];
            [NSApp postEvent:e atStart:NO];
        }
        return 0;
    });
}

// The window's content view (an NSView*, unretained: valid while the window is open).
__attribute__((visibility("default"))) void* summoner_ed_window_view(void* handle) {
    auto* w = static_cast<Win*>(handle);
    return (__bridge void*)w->window.contentView;
}

// The content size becomes w × h (a plugin's resizeView). The hook is not called for it.
__attribute__((visibility("default"))) int summoner_ed_window_set_size(void* handle, double w, double h) {
    auto* win = static_cast<Win*>(handle);
    return guarded([&] {
        win->hookActive = false;
        id d = win->window.delegate;
        summoner_ed_resize_hook saved = win->delegate.hook;
        win->delegate.hook = nullptr;
        [win->window setContentSize:NSMakeSize(w, h)];
        win->delegate.hook = saved;
        win->hookActive = true;
        (void)d;
        return 0;
    });
}

// The window's content size now.
__attribute__((visibility("default"))) int summoner_ed_window_get_size(void* handle, double* w, double* h) {
    auto* win = static_cast<Win*>(handle);
    return guarded([&] {
        NSSize s = win->window.contentView.frame.size;
        *w = s.width;
        *h = s.height;
        return 0;
    });
}

__attribute__((visibility("default"))) void summoner_ed_window_set_hook(void* handle, summoner_ed_resize_hook hook, void* ctx) {
    auto* w = static_cast<Win*>(handle);
    w->delegate.hookCtx = ctx;
    w->delegate.hook = hook;
}

// 1 if the close button was pressed since the last call (the request is taken), else 0.
__attribute__((visibility("default"))) int summoner_ed_window_take_close(void* handle) {
    auto* w = static_cast<Win*>(handle);
    if (!w->delegate.closeRequested)
        return 0;
    w->delegate.closeRequested = NO;
    return 1;
}

// The close button pressed, as AppKit presses it (performClose:, the button's own action): windowShouldClose: runs
// and records the request (a test hook's use: the user's path, without a click).
__attribute__((visibility("default"))) int summoner_ed_window_perform_close(void* handle) {
    auto* w = static_cast<Win*>(handle);
    return guarded([w] {
        [w->window performClose:nil];
        return 0;
    });
}

// Ordered out and freed (after the plugin's view was removed). The handle is invalid after.
__attribute__((visibility("default"))) int summoner_ed_window_close(void* handle) {
    auto* w = static_cast<Win*>(handle);
    const int r = guarded([w] {
        if (w->inputMonitor != nil)
            [NSEvent removeMonitor:w->inputMonitor];
        w->inputMonitor = nil;
        if (w->frameObserver != nil)
            [[NSNotificationCenter defaultCenter] removeObserver:w->frameObserver];
        w->frameObserver = nil;
        w->delegate.hook = nullptr;
        w->window.delegate = nil;
        if (w->shown)
            gOpen.fetch_sub(1);
        w->shown = false;
        [w->window orderOut:nil];
        w->window.contentView = nil;
        [w->window close];
        return 0;
    });
    w->window = nil;
    w->delegate = nil;
    delete w;
    return r;
}

// How many editor windows are on screen now (the tests' count).
__attribute__((visibility("default"))) int summoner_ed_window_count(void) {
    return gOpen.load();
}

}  // extern "C"
