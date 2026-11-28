// The lab editor fixture's view (lab_view.mm), as the C++ controller (lab_vst3.cpp) sees it. Main thread only.
#pragma once

#include "pluginterfaces/gui/iplugview.h"

#include <atomic>
#include <cstdint>

// What the editor did, for the controller's chunk to report.
struct LabEditorState {
    Steinberg::int32 w = 320, h = 200;  // the view's size: at first its getSize, then the last onSize
    uint32_t attaches = 0, removes = 0, onSizes = 0;
    uint32_t holdMs = 0;                    // the next attached() holds the main thread this long
    bool crashNext = false;                 // the next attached() aborts (labAbort: SIGKILL under SUMMONER_LAB_DEATH=kill)
    Steinberg::IPlugView* view = nullptr;   // the live view, if any (not owned)
    // The editor-reset guard's tests (editor-windows.md gap 5): with `quiet` (UIQUIET) the controller's chunk leaves the counters
    // out (an open is then no state change) and carries `wasReset` instead; with `resetOnView` (UIRESETONVIEW) the view's attached()
    // sets `wasReset`, as a plugin that resets its patch when its editor opens. Setting a chunk sets `wasReset` back.
    bool quiet = false;
    bool resetOnView = false;
    // UIRESET2=<ms> (a review's test): a second reset step <ms> after attached() (wasReset 2, from a thread of its
    // own, as a plugin that finishes an async preset load), so a restore made before it is undone by it.
    uint32_t reset2Ms = 0;
    std::atomic<uint32_t> wasReset{0};
};

// A new view (refcount 1, the caller's).
Steinberg::IPlugView* sasLabMakeView(LabEditorState* s);
// Asks the host to resize the live view (IPlugFrame::resizeView); false when there is none, or no frame.
bool sasLabResize(LabEditorState* s, Steinberg::int32 w, Steinberg::int32 h);
