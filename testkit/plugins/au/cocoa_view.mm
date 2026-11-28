// The lab AU Cocoa UI fixture's view factory (SUMMONER_LAB_FIXTURE_COCOAUI): the class the AU names in
// kAudioUnitProperty_CocoaUI, in the AU's own bundle, conforming to AUCocoaUIBase, so the host's AU v2 editor path (the
// AU's own view, not the generic one) has a lab fixture. A test plugin: a plain coloured view, 360 x 240.
#import <AppKit/AppKit.h>
#import <AudioToolbox/AudioUnitUtilities.h>
#import <AudioUnit/AUCocoaUIView.h>

#include <atomic>

// The parameter-change notifications the view received that were not its own: the host's, after it
// set parameter 0. The AU reports it in its output (lab_au.cpp, SUMMONER_LAB_FIXTURE_COCOAUI).
static std::atomic<unsigned> gForeignNotices{0};
extern "C" unsigned sasLabViewNotices(void) {
    return gForeignNotices.load();
}

static void viewEvent(void* self, void* object, const AudioUnitEvent* e, UInt64, AudioUnitParameterValue) {
    if (e != nullptr && e->mEventType == kAudioUnitEvent_ParameterValueChange && object != self)
        gForeignNotices.fetch_add(1);
}

// The view: listens to its AU's parameter 0 on the main run loop for as long as it lives.
@interface SummonerLabCocoaView : NSView {
    AUEventListenerRef listener_;
}
- (instancetype)initWithUnit:(AudioUnit)unit;
@end

@implementation SummonerLabCocoaView
- (instancetype)initWithUnit:(AudioUnit)unit {
    self = [super initWithFrame:NSMakeRect(0, 0, 360, 240)];
    if (self == nil)
        return nil;
    if (AUEventListenerCreate(viewEvent, (__bridge void*)self, CFRunLoopGetMain(), kCFRunLoopDefaultMode, 0, 0,
                              &listener_) == noErr) {
        AudioUnitEvent e{};
        e.mEventType = kAudioUnitEvent_ParameterValueChange;
        e.mArgument.mParameter = AudioUnitParameter{unit, 0, kAudioUnitScope_Global, 0};
        AUEventListenerAddEventType(listener_, nullptr, &e);
    }
    return self;
}
- (void)dealloc {
    if (listener_ != nullptr)
        AUListenerDispose(listener_);
}
@end

@interface SummonerLabCocoaViewFactory : NSObject <AUCocoaUIBase>
@end

@implementation SummonerLabCocoaViewFactory
- (unsigned)interfaceVersion {
    return 0;
}
- (NSView*)uiViewForAudioUnit:(AudioUnit)unit withSize:(NSSize)size {
        (void)size;
    NSView* v = [[SummonerLabCocoaView alloc] initWithUnit:unit];
    v.wantsLayer = YES;
    v.layer.backgroundColor = [NSColor colorWithCalibratedRed:0.6 green:0.3 blue:0.2 alpha:1.0].CGColor;
    // A user's edit, as a control makes one: 200 ms after the view exists, while it is still in a
    // window, parameter 0 (the gain) to 0.5 through AUParameterSet, which notifies the AU's event listeners.
    __weak NSView* weakView = v;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC), dispatch_get_main_queue(), ^{
      NSView* live = weakView;
      if (live == nil || live.window == nil)
          return;
      AudioUnitParameter p = {unit, 0, kAudioUnitScope_Global, 0};
      AUParameterSet(nullptr, (__bridge void*)live, &p, 0.5f, 0);
    });
    return v;
}
@end
