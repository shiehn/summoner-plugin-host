// The lab editor fixture's view (SUMMONER_LAB_FIXTURE_EDITOR): an IPlugView over the SDK's CPluginView whose
// NSView is a plain coloured box, so the host's editor side can be tested with a real view. A test plugin, not
// a product: no controls, nothing drawn but its background.
//
// - Its size starts at 320 x 200; it resizes (canResize) within 200 x 120 .. 1200 x 800 (checkSizeConstraint clamps).
// - Every attached / onSize is counted in the shared LabEditorState, which the controller's chunk reports.
// - holdMs: the next attached() holds the calling (main) thread that long (an editor holding the main thread).
// - crashNext: the next attached() aborts (a crash inside an editor op is attributed to it).
// - sasLabResize asks the host to resize it (IPlugFrame::resizeView), as a plugin's own resize does.
#import <AppKit/AppKit.h>

#include "lab_view.h"
#include "summoner_lab_dsp.h"

#include "public.sdk/source/common/pluginview.h"

#include <algorithm>
#include <chrono>
#include <cstring>
#include <thread>

using namespace Steinberg;

namespace {

constexpr int32 kMinW = 200, kMinH = 120, kMaxW = 1200, kMaxH = 800;

class LabView final : public CPluginView {
public:
    explicit LabView(LabEditorState* s) : CPluginView(nullptr), st_(s) {
        rect = ViewRect(0, 0, s->w, s->h);
        s->view = this;
    }
    ~LabView() override {
        if (st_->view == this)
            st_->view = nullptr;
    }

    tresult PLUGIN_API isPlatformTypeSupported(FIDString type) override {
        return type != nullptr && std::strcmp(type, kPlatformTypeNSView) == 0 ? kResultTrue : kResultFalse;
    }

    tresult PLUGIN_API attached(void* parent, FIDString type) override {
        if (isPlatformTypeSupported(type) != kResultTrue || parent == nullptr)
            return kResultFalse;
        CPluginView::attached(parent, type);
        NSView* host = (__bridge NSView*)parent;
        mine_ = [[NSView alloc] initWithFrame:NSMakeRect(0, 0, rect.getWidth(), rect.getHeight())];
        mine_.wantsLayer = YES;
        mine_.layer.backgroundColor = [NSColor colorWithCalibratedRed:0.2 green:0.4 blue:0.7 alpha:1.0].CGColor;
        mine_.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
        [host addSubview:mine_];
        ++st_->attaches;
        if (st_->resetOnView) {
            st_->wasReset.store(1);  // the plugin "resets itself" as its editor opens (the guard's test)
            if (st_->reset2Ms > 0) {
                LabEditorState* s = st_;
                const uint32_t ms = s->reset2Ms;
                std::thread([s, ms] {
                    std::this_thread::sleep_for(std::chrono::milliseconds(ms));
                    s->wasReset.store(2);
                }).detach();
            }
        }
        if (st_->crashNext)
            sas::lab::labAbort();  // a plugin that crashes in its own editor (the crash attribution test)
        if (st_->holdMs > 0) {
            const uint32_t ms = st_->holdMs;
            st_->holdMs = 0;
            std::this_thread::sleep_for(std::chrono::milliseconds(ms));
        }
        return kResultOk;
    }

    tresult PLUGIN_API removed() override {
        [mine_ removeFromSuperview];
        mine_ = nil;
        ++st_->removes;
        return CPluginView::removed();
    }

    tresult PLUGIN_API onSize(ViewRect* r) override {
        if (r == nullptr)
            return kInvalidArgument;
        rect = *r;
        st_->w = r->getWidth();
        st_->h = r->getHeight();
        ++st_->onSizes;
        if (mine_ != nil)
            mine_.frame = NSMakeRect(0, 0, r->getWidth(), r->getHeight());
        return kResultTrue;
    }

    tresult PLUGIN_API canResize() override { return kResultTrue; }

    IPlugFrame* frame() const { return plugFrame; }

    tresult PLUGIN_API checkSizeConstraint(ViewRect* r) override {
        if (r == nullptr)
            return kInvalidArgument;
        r->right = r->left + std::clamp(r->getWidth(), kMinW, kMaxW);
        r->bottom = r->top + std::clamp(r->getHeight(), kMinH, kMaxH);
        return kResultTrue;
    }

private:
    LabEditorState* st_;
    NSView* mine_ = nil;
};

}  // namespace

IPlugView* sasLabMakeView(LabEditorState* s) {
    return new LabView(s);
}

bool sasLabResize(LabEditorState* s, int32 w, int32 h) {
    auto* v = static_cast<LabView*>(s->view);
    if (v == nullptr || v->frame() == nullptr)
        return false;
    ViewRect r(0, 0, w, h);
    return v->frame()->resizeView(v, &r) == kResultTrue;
}
