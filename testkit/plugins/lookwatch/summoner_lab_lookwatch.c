// The lab's service-lookup recorder: injected with DYLD_INSERT_LIBRARIES, it appends the name of
// every mach / XPC service the process asks for, one per line ("<pid> <progname> <call> <name>"), to
// $SUMMONER_LOOKWATCH_LOG. Many processes may share one log (a test and the host children it spawns): each line is one
// O_APPEND write, so lines never interleave. It OBSERVES ONLY: every call is passed through unchanged, with its
// arguments and its result.
//
// The test asserts com.apple.audio.AudioComponentRegistrar (and com.apple.audio.AudioComponentPrefs) never appears.
//
// Its limits (why a system-wide fs_usage run is the authority):
// - it sees only calls that cross a library boundary into the exported symbols below; a lookup a library makes
//   internally (inside libxpc, say, or through a private entry point not listed here) is invisible to it;
// - it sees nothing another process does on our behalf;
// - four of the entry points are private (exported by libxpc, absent from the SDK's headers and link stubs):
//   bootstrap_look_up2 / 3 / _per_user and xpc_pipe_create. Their prototypes are Apple's open-source launchd
//   (bootstrap_priv.h) and libxpc; the arguments are passed through as they came. They were added because the first
//   control run showed libnotify's lookup goes through none of the public three;
// - so its positive control is required: the AU probe's control phase makes CoreFoundation connect to cfprefsd, and
//   the log must show that lookup between the probe's phase markers; a host child's own start-up lookups, logged under
//   its pid, are its control. Without the control the check is void.
#include <mach/mach.h>
#include <servers/bootstrap.h>
#include <xpc/xpc.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define INTERPOSE(replacement, replacee)                                                                 \
    __attribute__((used)) static struct {                                                                \
        const void* r;                                                                                   \
        const void* e;                                                                                   \
    } interpose_##replacee __attribute__((section("__DATA,__interpose"))) = {(const void*)&replacement, \
                                                                             (const void*)&replacee}

static int log_fd = -2;

static void record(const char* what, const char* name) {
    if (log_fd == -2) {
        const char* p = getenv("SUMMONER_LOOKWATCH_LOG");
        log_fd = p != NULL ? open(p, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : -1;
    }
    if (log_fd < 0)
        return;
    if (name == NULL)
        name = "(null)";
    char line[700];
    int p = snprintf(line, 100, "%d %s ", (int)getpid(), getprogname());
    size_t n = p > 0 && p < 100 ? (size_t)p : 0;
    size_t w = strlen(what), l = strlen(name);
    if (n + w + l + 2 > sizeof(line))
        l = sizeof(line) - n - w - 2;
    memcpy(line + n, what, w);
    n += w;
    line[n++] = ' ';
    memcpy(line + n, name, l);
    n += l;
    line[n++] = '\n';
    (void)!write(log_fd, line, n);
}

static kern_return_t my_bootstrap_look_up(mach_port_t bp, const name_t name, mach_port_t* sp) {
    record("bootstrap_look_up", name);
    return bootstrap_look_up(bp, name, sp);
}

static xpc_connection_t my_xpc_connection_create_mach_service(const char* name, dispatch_queue_t q, uint64_t flags) {
    record("xpc_connection_create_mach_service", name);
    return xpc_connection_create_mach_service(name, q, flags);
}

static xpc_connection_t my_xpc_connection_create(const char* name, dispatch_queue_t q) {
    record("xpc_connection_create", name);
    return xpc_connection_create(name, q);
}

// The private entry points (prototypes from launchd's bootstrap_priv.h and libxpc).
typedef struct xpc_pipe_s* xpc_pipe_t;
extern kern_return_t bootstrap_look_up2(mach_port_t bp, const name_t service_name, mach_port_t* sp, pid_t target_pid,
                                        uint64_t flags);
extern kern_return_t bootstrap_look_up3(mach_port_t bp, const name_t service_name, mach_port_t* sp, pid_t target_pid,
                                        const unsigned char instance_id[16], uint64_t flags);
extern kern_return_t bootstrap_look_up_per_user(mach_port_t bp, const name_t service_name, uid_t target_user,
                                                mach_port_t* sp);
extern xpc_pipe_t xpc_pipe_create(const char* name, uint64_t flags);

static kern_return_t my_bootstrap_look_up2(mach_port_t bp, const name_t name, mach_port_t* sp, pid_t pid,
                                           uint64_t flags) {
    record("bootstrap_look_up2", name);
    return bootstrap_look_up2(bp, name, sp, pid, flags);
}

static kern_return_t my_bootstrap_look_up3(mach_port_t bp, const name_t name, mach_port_t* sp, pid_t pid,
                                           const unsigned char instance_id[16], uint64_t flags) {
    record("bootstrap_look_up3", name);
    return bootstrap_look_up3(bp, name, sp, pid, instance_id, flags);
}

static kern_return_t my_bootstrap_look_up_per_user(mach_port_t bp, const name_t name, uid_t uid, mach_port_t* sp) {
    record("bootstrap_look_up_per_user", name);
    return bootstrap_look_up_per_user(bp, name, uid, sp);
}

static xpc_pipe_t my_xpc_pipe_create(const char* name, uint64_t flags) {
    record("xpc_pipe_create", name);
    return xpc_pipe_create(name, flags);
}

INTERPOSE(my_bootstrap_look_up, bootstrap_look_up);
INTERPOSE(my_bootstrap_look_up2, bootstrap_look_up2);
INTERPOSE(my_bootstrap_look_up3, bootstrap_look_up3);
INTERPOSE(my_bootstrap_look_up_per_user, bootstrap_look_up_per_user);
INTERPOSE(my_xpc_pipe_create, xpc_pipe_create);
INTERPOSE(my_xpc_connection_create_mach_service, xpc_connection_create_mach_service);
INTERPOSE(my_xpc_connection_create, xpc_connection_create);
