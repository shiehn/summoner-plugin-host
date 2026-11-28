// The STOP fixture: a .component whose executable links the drag dylib, so loading the bundle brings a non-system
// image from outside the bundle into the process. Its factory is never reached (the load is a STOP first).
extern int summoner_lab_drag(void);
__attribute__((visibility("default"))) void* SummonerLabDragFactory(const void* desc);
void* SummonerLabDragFactory(const void* desc) {
    (void)desc;
    (void)summoner_lab_drag();
    return 0;
}
