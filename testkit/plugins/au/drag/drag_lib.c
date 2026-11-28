// A lab dylib outside every plugin bundle (target/plugins/au-fixture/): the post-load image checks
// must STOP when a plugin's load, construction or initialization brings it in. It does nothing.
int summoner_lab_drag(void) { return 42; }
