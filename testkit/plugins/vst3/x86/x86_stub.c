// The x86 fixture: a VST3-shaped bundle whose executable is x86_64 ONLY. It is built, never
// executed: the scan's Mach-O check must refuse it (`failed arch`) before anything loads it, so Rosetta is never
// asked. The one symbol is what a VST3 module exports; it does nothing.
void* GetPluginFactory(void) { return 0; }
