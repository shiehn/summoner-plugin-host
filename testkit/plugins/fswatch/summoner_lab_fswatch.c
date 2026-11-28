// The lab's file-access recorder (v2 for the host application's recordings; v3 for recordings of a reference host):
// injected with DYLD_INSERT_LIBRARIES into a process under test, it appends one line per call below to
// $SUMMONER_FSWATCH_LOG. The tests assert that nothing under the plugin folders was touched, and can tell a read from a
// write, a failed call from a successful one, and where a relative path is.
//
// ============================================ THE FORMAT, v3 (this is the spec) =============================================
// Every line is TAB-separated fields, ending in LF; one write() per line (O_APPEND), so lines never interleave.
//
// HEADER, the first line each process writes (when it opens the log):
//     fswatch<TAB>v3<TAB>pid=<pid><TAB>cwd=<the process's cwd at that moment, escaped AS A DETAIL VALUE (',' too, as
//       `\x2c`: a raw ',' is never written); `?` if getcwd fails>
//     fswatch<TAB>v3<TAB>pid=<pid><TAB>cwd=?<TAB>trunc=1    when the escaped cwd was cut to fit (never written cut)
//   A later header in the same log is ANOTHER process's start (its own pid and cwd). A process that FORKS without
//   exec: the child writes its OWN header (its pid, its cwd) before its first record. (The header belongs to the
//   pid that writes it, compared at every record: lazily, at the child's first record, rather than in a
//   pthread_atfork child handler, which would have to run getcwd / snprintf / write in the child of a possibly
//   multithreaded parent, where they are not async-signal-safe. The guarantee is the same: no record of a pid comes
//   before that pid's header.) pid= on every record is getpid() at that record, never a cached value. A child that
//   forks AND execs writes TWO headers with the same pid (one before the exec, if it made a recorded call, and one
//   when its new image loads the recorder): both are valid, the second starting that pid's new image, and its cwd=
//   is the cwd at that moment. The header path allocates nothing (fixed buffers only). A parser that does not find a
//   header on the first line, or meets a header of another version, refuses the log. (v2 logs, `fswatch<TAB>v2<TAB>
//   pid=<pid>`, have the v2 layout below and stay parseable as v2.)
//
// EVERY CALL IS A PAIR (the watch catches ATTEMPTS, so the attempt is written before the call):
//   PRE, written BEFORE the real call:
//     <call><TAB><detail><TAB><path>                  one-path calls
//     <call><TAB><detail><TAB><path1><TAB><path2>     rename, renameat (from, to), symlink (target, link), link
//     its detail, IN THIS ORDER: pid=<pid>,tid=<thread id>,seq=<n>, then the call's own keys (in this order: dirfd,
//     dirpath, todirfd, todirpath, fd, fdpath, flags, flag, mode, fmode, len; each only where the call has it,
//     below), then trunc=1 (only if a field was cut). The path is ALWAYS the last field; ftruncate's and fchdir's
//     path is `-`. CUTS ARE ALWAYS MARKED: every field keeps room for `,trunc=1`; a key's `,key=` goes in whole or
//     not at all (only a value is ever cut); nothing follows the first cut in its field.
//   POST, written AFTER the real call returns (errno kept intact for the caller):
//     <call>.ret<TAB><detail><TAB>-
//     its detail, IN THIS ORDER: pid=<pid>,tid=<thread id>,seq=<n> (the same as its PRE's), then ret, errno,
//     image, cwd, trunc, each as follows:
//       ret=<n>         always: the call's return value (int and ssize_t calls); for the calls returning a pointer
//                       (fopen, opendir, dlopen) 0 on success and -1 on failure
//       errno=<NAME>    ONLY when the call failed (ret -1; fopen / opendir returning NULL), and never for dlopen
//                       (dlopen sets no errno: its failure is ret=-1 alone)
//       image=<escaped> dlopen only, only on success: the image dyld actually resolved (the loaded image whose
//                       dlopen(<its name>, RTLD_NOLOAD) handle is the one returned; dladdr(handle) does not work, a
//                       handle being no address in the image, measured 2026-10-31), or `?` if none matches. A bare-name
//                       or relative dlopen is then judged on the real image.
//       cwd=<escaped>   chdir / fchdir only, only on success: getcwd()'s ABSOLUTE result after the call, or `?`
//                       if getcwd fails
//       trunc=1         last, only if the image or the cwd was cut to fit (nothing else in a POST is long enough)
//     Nothing else: the paths are in the PRE. A POST's keys are EXACTLY these: ret; errno iff it failed and is not
//     dlopen; image iff a dlopen succeeded; cwd iff a chdir / fchdir succeeded; trunc only after an image or a cwd.
//     ret is -1 or more, and 0 or -1 for fopen, opendir, dlopen, chdir and fchdir.
//   tid is the thread's pthread_threadid_np (decimal); seq is a per-THREAD counter (thread-local), from 1 for each
//   thread's first call. The key of a pair is (pid, tid, seq), unique within a pid's IMAGE: a new header for the pid
//   (an exec) settles that pid's open PREs as a reap does (no POST of them can follow it), and its keys may then
//   recur (XNU gives an exec'd thread a new tid anyway). PAIR BY THE KEY, NEVER BY
//   ADJACENCY: other threads' and other processes' lines may fall between a PRE and its POST. A PRE with NO POST is a
//   call that never returned (a hang, a crash inside it): it stays in the log as the attempt it was. A POST with no
//   open PRE of its key before it is malformed.
//   Values carrying outside bytes (dirpath, todirpath, fdpath, fmode, cwd) are escaped as below, ',' included.
//
// THE CALLS and their keys (v3):
//   open        flags[,mode]                          openat     dirfd[,dirpath],flags[,mode]
//   creat       mode                                  fopen      fmode
//   stat, lstat, access, opendir, dlopen, getattrlist, statfs, readlink, unlink, rmdir, rename, symlink, link,
//   chdir       (no own keys)
//   fstatat, faccessat   dirfd[,dirpath]              unlinkat   dirfd[,dirpath],flag
//   mkdir       mode                                  mkdirat    dirfd[,dirpath],mode
//   renameat    dirfd[,dirpath],todirfd[,todirpath]   truncate   len
//   ftruncate   fd,fdpath,len                         fchdir     fd,fdpath
//   dirpath / todirpath appear only for a real dirfd (not AT_FDCWD = -2): fcntl F_GETPATH, or `?`. fdpath likewise.
//   An open's / openat's mode appears iff its flags have O_CREAT. These are EXACT: no other key, none missing.
//   flags=0x<lowercase hex> (the open flags: O_WRONLY 0x1, O_RDWR 0x2, O_APPEND 0x8, O_CREAT 0x200, O_TRUNC 0x400 mark
//   a write); mode=0<octal> (`0%llo`: mode 0 is `00`); flag=0x<hex>; len=<decimal>; fd=<decimal>;
//   fmode=<fopen's mode string>. Numbers in their canonical printf form only (no `+`, no leading zero, no `-0`):
//   pid 1..2^31-1, tid 0..2^63-1, seq from 1. A PRE marked trunc=1 may carry a cut value or lack later keys; its
//   keys keep their order.
//
// CHDIR / FCHDIR: chdir's PRE carries the path as given; fchdir's PRE its fd and fdpath (or `?`). On success
//   (POST ret=0) the process's tracked cwd becomes the POST's cwd= (the authoritative result: no parser resolves
//   `..` or symlinks itself); on failure, or with no POST, it does not change.
//
// ERRNO NAMES, the fixed set (any other value is written errno=E<decimal>, e.g. errno=E106; a named errno is never
// written by its number, so `E2` is malformed):
//   EPERM ENOENT ESRCH EINTR EIO ENXIO E2BIG ENOEXEC EBADF ECHILD EDEADLK ENOMEM EACCES EFAULT EBUSY EEXIST EXDEV
//   ENODEV ENOTDIR EISDIR EINVAL ENFILE EMFILE ENOTTY ETXTBSY EFBIG ENOSPC ESPIPE EROFS EMLINK EPIPE EAGAIN ELOOP
//   ENAMETOOLONG ENOTEMPTY EDQUOT ESTALE ENOTSUP EOVERFLOW EILSEQ ECANCELED
//
// ESCAPING (every field carrying outside bytes: a path, and the values listed above): `\` → `\\`, TAB → `\t`, LF →
//   `\n`, CR → `\r`, any other byte < 0x20 and 0x7f → `\xHH` (two lowercase hex digits); inside a detail VALUE also
//   ',' → `\x2c`. Bytes >= 0x80 (UTF-8) pass as they are. An empty path is an empty field (a host may
//   open "" at start-up).
//
// THE CONSUMER RULES (the host application's parser matches):
//   (a) a STOP-list PRE STOPS AT ONCE, judged on the attempt, without waiting for its POST, whatever the result. The
//       STOP list (both parsers), matched CASE-FOLDED as APFS folds (Unicode's folding: ſ is s, the ligature ﬆ is
//       st; a character whose canonical form mixes ASCII and non-ASCII, é = e + U+0301, is another name) on the path
//       as logged AND with its `.` / `..` applied, the first three clauses as SUBSTRINGS (so /System/Volumes/Data/…,
//       the firmlink spelling, and any `link/../..` walk are caught), writes by prefix:
//       any path under a plugin folder (any /Library/Audio/Plug-Ins, any home's Library/Audio/Plug-Ins);
//       /System/Library/Components, with ONE exemption (both parsers, word for word): "Exempt: provable READS of
//       /System/Library/Components/CoreAudio.component and anything under it, and a dlopen of exactly
//       /System/Library/Components/CoreAudio.component or
//       /System/Library/Components/CoreAudio.component/Contents/MacOS/CoreAudio (byte for byte; no case variant, no
//       `..`; for a v3 dlopen, both the path and image= must be one of those two). Any other dlopen under
//       /System/Library/Components, another file inside CoreAudio.component included, stops; so does any write."
//       Then (seen in a real recording), for those two EXACT names only (the sealed, read-only
//       system volume): a dlopen with ret=-1 is EXEMPT (listed as a failed load), and a successful one with image=?
//       is EXEMPT on its path alone (listed as image unmatched). Every other dlopen keeps both-must-match; a cut
//       image=, a dlopen that never returned and a name that is not absolute still stop. (To do, at the next recorder
//       change: image= missed that load; mask the handle's low mode bits in the compare, or dladdr a symbol of it.)
//       (Provable reads: the stat family, access, opendir, getattrlist, statfs, readlink, an open / fopen with no
//       writing flag or mode; named exactly too: that case, no `.` / `..`. A chdir is no read.) The REAL
//       home's Library/Preferences/com.*; and ANY WRITE under /System or /Library. Also STOPS at once: a PRE marked
//       trunc=1; a WRITE through `..`; a symlink whose TARGET text names a STOP-list path (as a write); a relative
//       image= on a dlopen POST; a CUT image= (its lost tail could name a listed image); a chdir / fchdir POST
//       whose cwd= was cut (trunc=1: it places nothing after it). A log that is not UTF-8 (a cut that split a
//       character) is UNREADABLE, and so a STOP: never read as an empty or partial log.
//       An absolute dlopen is judged on BOTH its path and its image= (a STOP if either is listed);
//   (b) a WRITE whose location cannot be placed STOPS AT ONCE, whatever its result. A READ whose location cannot be
//       placed (an empty or otherwise unplaceable path; a dlopen of a name that is not absolute, until its POST's
//       image=) is PENDING until its POST:
//       "failed, so clean" needs that POST with errno ENOENT or ENOTDIR (the call reached no file); it STOPS if its
//       process is reaped, its pid writes a new header (an exec), or the run ends, with no POST;
//       A FAILED dlopen has no image= and no errno, so it is never "clean": it STOPS if its name is not absolute;
//   (c) a POST with no matching open PRE is malformed: the log is refused;
//   - a relative path is placed against its pid's tracked cwd (the header's cwd=, then each successful chdir /
//     fchdir POST's cwd=). The cwd is UNKNOWN (nothing is placed against it) while a chdir / fchdir of that pid is
//     in flight (its PRE seen, its POST not yet), after a cwd=? and after a cut cwd=. Unplaceable: dirpath=?,
//     fdpath=?, cwd unknown, trunc=1.
//
// Limits (why a system-wide fs_usage run is the authority): it sees calls made through these exported symbols from
// any image in the process; a direct syscall, a $NOCANCEL / $INODE64 / _np variant not listed (renamex_np,
// renameatx_np, clonefile, …), or a path opened by another process (a daemon) is not seen.
// =============================================================================================================================
#include <dirent.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <mach-o/dyld.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/attr.h>
#include <sys/mount.h>
#include <sys/param.h>
#include <sys/stat.h>
#include <unistd.h>

#define INTERPOSE(replacement, replacee)                                                                 \
    __attribute__((used)) static struct {                                                                \
        const void* r;                                                                                   \
        const void* e;                                                                                   \
    } interpose_##replacee __attribute__((section("__DATA,__interpose"))) = {(const void*)&replacement, \
                                                                             (const void*)&replacee}

// The log: opened once per image (O_CLOEXEC, so an exec'd image opens its own); its header written once per PROCESS
// (a forked child inherits the descriptor but writes its own header, its own pid and cwd, before its first record).
static int log_fd = -2;
static int header_pid = 0;
// The per-thread pair counter (the `seq` of the (pid, tid, seq) key), from 1 in every process: a forked child's copy
// of the thread restarts it (`seq_pid` notes whose count it is).
static __thread unsigned long long seq_ctr = 0;
static __thread int seq_pid = 0;
// Inside the recorder itself (its header, its getcwd): the calls IT causes (getcwd stats the cwd through libSystem,
// which IS interposed) pass through unrecorded, so nothing comes before a process's header and no record is the
// recorder's own. Thread-local: another thread's calls are recorded as ever.
static __thread int in_recorder = 0;

// getcwd, with the recorder's own calls inside it unrecorded.
static const char* own_getcwd(char* buf, size_t cap) {
    ++in_recorder;
    const char* r = getcwd(buf, cap);
    --in_recorder;
    return r;
}

// A field being built: its bytes, and whether anything was cut to fit.
typedef struct {
    char b[2048];
    size_t n;
    int trunc;
    size_t lim;  // the usable size; 0: all of b less the mark's room (a call's own keys use less: see DETAIL)
} field;

// Room kept at the end of every field for the cut mark (`,trunc=1`), so a cut can never go unmarked.
#define MARK ",trunc=1"
#define RESERVE (sizeof(MARK) - 1)
// The longest pair-key head, `pid=2147483647,tid=<19 digits>,seq=<19 digits>`, and the `,` after it: a call's own
// keys are built in a field this much smaller, so pre() can always append them whole after the key.
#define KEYROOM 64

static size_t cap(const field* f) { return f->lim != 0 ? f->lim : sizeof(f->b) - RESERVE; }

// Nothing after the first cut (a later, shorter piece that would still fit is dropped too: never a garbled field).
static void fput(field* f, const char* s, size_t l) {
    if (f->trunc)
        return;
    if (f->n + l > cap(f)) {
        f->trunc = 1;
        return;  // never half an escape
    }
    memcpy(f->b + f->n, s, l);
    f->n += l;
}

// The cut mark, into the room kept for it (`trunc=1` alone in an empty field). Only emit() calls it, once.
static void mark(field* d) {
    const char* m = d->n > 0 ? MARK : &MARK[1];
    const size_t l = strlen(m);
    memcpy(d->b + d->n, m, l);
    d->n += l;
}

// `s` escaped into `f` (stopping, marked, at the first byte that does not fit whole); in a detail VALUE a ',' too.
static void fesc_as(field* f, const char* s, int in_detail) {
    static const char hex[] = "0123456789abcdef";
    for (const unsigned char* c = (const unsigned char*)s; *c != 0 && !f->trunc; ++c) {
        char e[4];
        size_t l = 2;
        e[0] = '\\';
        switch (*c) {
            case '\\': e[1] = '\\'; break;
            case '\t': e[1] = 't'; break;
            case '\n': e[1] = 'n'; break;
            case '\r': e[1] = 'r'; break;
            default:
                if (*c < 0x20 || *c == 0x7f || (in_detail && *c == ',')) {
                    e[1] = 'x';
                    e[2] = hex[*c >> 4];
                    e[3] = hex[*c & 15];
                    l = 4;
                } else {
                    e[0] = (char)*c;
                    l = 1;
                }
        }
        fput(f, e, l);
    }
}

static void fesc(field* f, const char* s) { fesc_as(f, s, 0); }

// A detail key (ours: plain ASCII) and its value (escaped, its commas too).
// The `,key=` head goes in whole or not at all (a cut never leaves a key without its `=`); only the value may be cut.
static void kv(field* d, const char* key, const char* value) {
    const size_t kl = strlen(key), need = (d->n > 0) + kl + 1;
    if (!d->trunc && d->n + need > cap(d))
        d->trunc = 1;
    if (d->trunc)
        return;
    if (d->n > 0)
        fput(d, ",", 1);
    fput(d, key, kl);
    fput(d, "=", 1);
    fesc_as(d, value, 1);
}

static void kvi(field* d, const char* key, const char* fmt, long long v) {
    char t[40];
    snprintf(t, sizeof(t), fmt, v);
    kv(d, key, t);
}

// Where a real dirfd points (read-only fcntl F_GETPATH), or "?".
static void dirpath(field* d, const char* key, int fd) {
    if (fd == AT_FDCWD)
        return;
    char p[MAXPATHLEN];
    kv(d, key, fcntl(fd, F_GETPATH, p) != -1 ? p : "?");
}

static void fdpath(field* d, int fd) {
    char p[MAXPATHLEN];
    kv(d, "fdpath", fcntl(fd, F_GETPATH, p) != -1 ? p : "?");
}

// The fixed errno names (the header's list); anything else is E<decimal>.
static void errname(int e, char* out, size_t cap) {
#define N(x) \
    case x: snprintf(out, cap, "%s", #x); return
    switch (e) {
        N(EPERM); N(ENOENT); N(ESRCH); N(EINTR); N(EIO); N(ENXIO); N(E2BIG); N(ENOEXEC); N(EBADF); N(ECHILD);
        N(EDEADLK); N(ENOMEM); N(EACCES); N(EFAULT); N(EBUSY); N(EEXIST); N(EXDEV); N(ENODEV); N(ENOTDIR);
        N(EISDIR); N(EINVAL); N(ENFILE); N(EMFILE); N(ENOTTY); N(ETXTBSY); N(EFBIG); N(ENOSPC); N(ESPIPE); N(EROFS);
        N(EMLINK); N(EPIPE); N(EAGAIN); N(ELOOP); N(ENAMETOOLONG); N(ENOTEMPTY); N(EDQUOT); N(ESTALE); N(ENOTSUP);
        N(EOVERFLOW); N(EILSEQ); N(ECANCELED);
        default: snprintf(out, cap, "E%d", e);
    }
#undef N
}

// Calls inside this image are not interposed, so open(), write(), fcntl() and getcwd() here are the real ones.
// Returns whether there is a log to write to; writes this PROCESS's header first if it has not yet.
static int header_once(void) {
    if (log_fd == -2) {
        const char* p = getenv("SUMMONER_FSWATCH_LOG");
        log_fd = p != NULL ? open(p, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644) : -1;
    }
    if (log_fd < 0)
        return 0;
    const int me = (int)getpid();
    int seen = __atomic_load_n(&header_pid, __ATOMIC_ACQUIRE);
    if (seen != me && __atomic_compare_exchange_n(&header_pid, &seen, me, 0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
        char cwd[MAXPATHLEN];
        field c = {.n = 0, .trunc = 0};
        fesc_as(&c, own_getcwd(cwd, sizeof(cwd)) != NULL ? cwd : "?", 1);
        char head[64 + sizeof(c.b)];
        // A cwd cut to fit is never written as if whole: `cwd=?` and the header's trunc mark, so nothing is placed
        // against it.
        int n = c.trunc ? snprintf(head, sizeof(head), "fswatch\tv3\tpid=%d\tcwd=?\ttrunc=1\n", me)
                        : snprintf(head, sizeof(head), "fswatch\tv3\tpid=%d\tcwd=%.*s\n", me, (int)c.n, c.b);
        if (n > 0)
            (void)!write(log_fd, head, (size_t)n);
    }
    return 1;
}

// The pair key's head: pid (getpid() now, never cached), tid, seq.
static void key(field* d, unsigned long long seq) {
    unsigned long long tid = 0;
    pthread_threadid_np(NULL, &tid);
    kvi(d, "pid", "%lld", (long long)getpid());
    kvi(d, "tid", "%lld", (long long)tid);
    kvi(d, "seq", "%lld", (long long)seq);
}

// One line: call (+ suffix), detail, path(s); trunc=1 added when any field was cut. One write(): never interleaved.
static void emit(const char* call, const char* suffix, field* d, const char* path, const char* path2) {
    static __thread field p1, p2;  // per thread: no lock, no 4 KiB of stack per call
    p1.n = p1.trunc = 0;
    p2.n = p2.trunc = 0;
    fesc(&p1, path != NULL ? path : "-");
    if (path2 != NULL)
        fesc(&p2, path2);
    if (d->trunc || p1.trunc || p2.trunc)
        mark(d);
    char line[3 * sizeof(p1.b) + 64];
    size_t n = 0;
    size_t l = strlen(call);
    memcpy(line + n, call, l);
    n += l;
    l = strlen(suffix);
    memcpy(line + n, suffix, l);
    n += l;
    line[n++] = '\t';
    memcpy(line + n, d->b, d->n);
    n += d->n;
    line[n++] = '\t';
    memcpy(line + n, p1.b, p1.n);
    n += p1.n;
    if (path2 != NULL) {
        line[n++] = '\t';
        memcpy(line + n, p2.b, p2.n);
        n += p2.n;
    }
    line[n++] = '\n';
    (void)!write(log_fd, line, n);
}

// The PRE, BEFORE the call: pid, tid, seq, the call's own keys (`own`), the path(s). Returns the pair's seq.
static unsigned long long pre(const char* call, field* own, const char* path, const char* path2) {
    if (in_recorder > 0)
        return 0;  // a call the recorder itself made: unrecorded (and so its POST too)
    const int e = errno;
    unsigned long long seq = 0;
    ++in_recorder;
    const int have = header_once();
    --in_recorder;
    if (have) {
        const int me = (int)getpid();
        if (seq_pid != me) {
            seq_pid = me;
            seq_ctr = 0;
        }
        seq = ++seq_ctr;
        static __thread field d;
        d.n = d.trunc = 0;
        key(&d, seq);
        if (own->n > 0) {  // always fits whole: own is KEYROOM smaller than d (DETAIL)
            fput(&d, ",", 1);
            fput(&d, own->b, own->n);
        }
        d.trunc |= own->trunc;
        emit(call, "", &d, path, path2);
    }
    errno = e;
    return seq;
}

// The POST, AFTER the call: the same key, ret, errno (`err`, non-zero only on failure; never for dlopen), cwd
// (`newcwd`, chdir / fchdir success). errno is left as the call left it.
static void post(const char* call, unsigned long long seq, long long ret, int err, const char* image,
                 const char* newcwd) {
    const int e = errno;
    if (seq != 0 && log_fd >= 0) {
        static __thread field d;
        d.n = d.trunc = 0;
        key(&d, seq);
        kvi(&d, "ret", "%lld", ret);
        if (err != 0) {
            char en[24];
            errname(err, en, sizeof(en));
            kv(&d, "errno", en);
        }
        if (image != NULL)
            kv(&d, "image", image);
        if (newcwd != NULL)
            kv(&d, "cwd", newcwd);
        emit(call, ".ret", &d, "-", NULL);
    }
    errno = e;
}

#define DETAIL field d = {.n = 0, .trunc = 0, .lim = sizeof(d.b) - RESERVE - KEYROOM}

static mode_t vmode(int flags, va_list ap) { return (flags & O_CREAT) ? (mode_t)va_arg(ap, int) : 0; }

// A call: its PRE (only when it has a path, as in v2), the real call, its POST with the result.
#define CALL(name, ret_t, has_path, prep, call_expr, path1, path2, ret_ll, failed)  \
    do {                                                                            \
        unsigned long long q_ = 0;                                                  \
        if (has_path) {                                                             \
            DETAIL;                                                                 \
            prep;                                                                   \
            q_ = pre(name, &d, path1, path2);                                       \
        }                                                                           \
        r = call_expr;                                                              \
        const int e_ = errno;                                                       \
        post(name, q_, (long long)(ret_ll), (failed) ? e_ : 0, NULL, NULL);         \
        errno = e_;                                                                 \
    } while (0)

static int my_open(const char* path, int flags, ...) {
    va_list ap;
    va_start(ap, flags);
    mode_t mode = vmode(flags, ap);
    va_end(ap);
    int r;
    CALL("open", int, path != NULL,
         (kvi(&d, "flags", "0x%llx", (unsigned)flags), (flags & O_CREAT) ? kvi(&d, "mode", "0%llo", mode) : (void)0),
         open(path, flags, mode), path, NULL, r, r == -1);
    return r;
}
static int my_openat(int fd, const char* path, int flags, ...) {
    va_list ap;
    va_start(ap, flags);
    mode_t mode = vmode(flags, ap);
    va_end(ap);
    int r;
    CALL("openat", int, path != NULL,
         (kvi(&d, "dirfd", "%lld", fd), dirpath(&d, "dirpath", fd), kvi(&d, "flags", "0x%llx", (unsigned)flags),
          (flags & O_CREAT) ? kvi(&d, "mode", "0%llo", mode) : (void)0),
         openat(fd, path, flags, mode), path, NULL, r, r == -1);
    return r;
}
static int my_creat(const char* path, mode_t mode) {
    int r;
    CALL("creat", int, path != NULL, kvi(&d, "mode", "0%llo", mode), creat(path, mode), path, NULL, r, r == -1);
    return r;
}
static FILE* my_fopen(const char* path, const char* mode) {
    FILE* r;
    CALL("fopen", FILE*, path != NULL, kv(&d, "fmode", mode != NULL ? mode : ""), fopen(path, mode), path, NULL,
         r != NULL ? 0 : -1, r == NULL);
    return r;
}

// The calls with a path and no keys of their own.
#define PLAIN(name, ret_t, args, call_args)                                                  \
    static ret_t my_##name args {                                                            \
        ret_t r;                                                                             \
        CALL(#name, ret_t, path != NULL, (void)0, name call_args, path, NULL, r, r == -1);   \
        return r;                                                                            \
    }
PLAIN(stat, int, (const char* path, struct stat* st), (path, st))
PLAIN(lstat, int, (const char* path, struct stat* st), (path, st))
PLAIN(access, int, (const char* path, int mode), (path, mode))
PLAIN(getattrlist, int, (const char* path, void* list, void* buf, size_t size, unsigned int opts),
      (path, list, buf, size, opts))
PLAIN(statfs, int, (const char* path, struct statfs* b), (path, b))
PLAIN(readlink, ssize_t, (const char* path, char* buf, size_t size), (path, buf, size))
PLAIN(unlink, int, (const char* path), (path))
PLAIN(rmdir, int, (const char* path), (path))

static DIR* my_opendir(const char* path) {
    DIR* r;
    CALL("opendir", DIR*, path != NULL, (void)0, opendir(path), path, NULL, r != NULL ? 0 : -1, r == NULL);
    return r;
}
// The image dyld resolved for `h`: the loaded image whose dlopen(<its name>, RTLD_NOLOAD) handle is `h` (each probe
// handle closed again: nothing loaded, no count changed). dladdr(h) does not work: a handle is no address in the image.
static const char* image_of(void* h) {
    const uint32_t n = _dyld_image_count();
    for (uint32_t i = 0; i < n; ++i) {
        const char* name = _dyld_get_image_name(i);
        if (name == NULL)
            continue;
        void* g = dlopen(name, RTLD_NOLOAD | RTLD_LAZY);  // this image's own dlopen: not interposed
        if (g == NULL)
            continue;
        dlclose(g);
        if (g == h)
            return name;
    }
    return "?";
}

static void* my_dlopen(const char* path, int mode) {
    unsigned long long q = 0;
    if (path != NULL) {
        DETAIL;
        q = pre("dlopen", &d, path, NULL);
    }
    void* r = dlopen(path, mode);
    const int e = errno;
    // No errno for dlopen (it sets none); on success, the image it resolved.
    post("dlopen", q, r != NULL ? 0 : -1, 0, (q != 0 && r != NULL) ? image_of(r) : NULL, NULL);
    errno = e;
    return r;
}
static int my_fstatat(int fd, const char* path, struct stat* st, int flag) {
    int r;
    CALL("fstatat", int, path != NULL, (kvi(&d, "dirfd", "%lld", fd), dirpath(&d, "dirpath", fd)),
         fstatat(fd, path, st, flag), path, NULL, r, r == -1);
    return r;
}
static int my_faccessat(int fd, const char* path, int mode, int flag) {
    int r;
    CALL("faccessat", int, path != NULL, (kvi(&d, "dirfd", "%lld", fd), dirpath(&d, "dirpath", fd)),
         faccessat(fd, path, mode, flag), path, NULL, r, r == -1);
    return r;
}
static int my_rename(const char* from, const char* to) {
    int r;
    CALL("rename", int, 1, (void)0, rename(from, to), from, to != NULL ? to : "-", r, r == -1);
    return r;
}
static int my_renameat(int fromfd, const char* from, int tofd, const char* to) {
    int r;
    CALL("renameat", int, 1,
         (kvi(&d, "dirfd", "%lld", fromfd), dirpath(&d, "dirpath", fromfd), kvi(&d, "todirfd", "%lld", tofd),
          dirpath(&d, "todirpath", tofd)),
         renameat(fromfd, from, tofd, to), from, to != NULL ? to : "-", r, r == -1);
    return r;
}
static int my_unlinkat(int fd, const char* path, int flag) {
    int r;
    CALL("unlinkat", int, path != NULL,
         (kvi(&d, "dirfd", "%lld", fd), dirpath(&d, "dirpath", fd), kvi(&d, "flag", "0x%llx", (unsigned)flag)),
         unlinkat(fd, path, flag), path, NULL, r, r == -1);
    return r;
}
static int my_mkdir(const char* path, mode_t mode) {
    int r;
    CALL("mkdir", int, path != NULL, kvi(&d, "mode", "0%llo", mode), mkdir(path, mode), path, NULL, r, r == -1);
    return r;
}
static int my_mkdirat(int fd, const char* path, mode_t mode) {
    int r;
    CALL("mkdirat", int, path != NULL,
         (kvi(&d, "dirfd", "%lld", fd), dirpath(&d, "dirpath", fd), kvi(&d, "mode", "0%llo", mode)),
         mkdirat(fd, path, mode), path, NULL, r, r == -1);
    return r;
}
static int my_truncate(const char* path, off_t len) {
    int r;
    CALL("truncate", int, path != NULL, kvi(&d, "len", "%lld", (long long)len), truncate(path, len), path, NULL, r,
         r == -1);
    return r;
}
static int my_ftruncate(int fd, off_t len) {
    int r;
    CALL("ftruncate", int, 1, (kvi(&d, "fd", "%lld", fd), fdpath(&d, fd), kvi(&d, "len", "%lld", (long long)len)),
         ftruncate(fd, len), "-", NULL, r, r == -1);
    return r;
}
static int my_symlink(const char* target, const char* linkpath) {
    int r;
    CALL("symlink", int, 1, (void)0, symlink(target, linkpath), target, linkpath != NULL ? linkpath : "-", r,
         r == -1);
    return r;
}
static int my_link(const char* existing, const char* newpath) {
    int r;
    CALL("link", int, 1, (void)0, link(existing, newpath), existing, newpath != NULL ? newpath : "-", r, r == -1);
    return r;
}

// v3: the cwd's moves. The POST's cwd= (getcwd's absolute result, or "?") is the process's new cwd on success.
static int my_chdir(const char* path) {
    unsigned long long q = 0;
    if (path != NULL) {
        DETAIL;
        q = pre("chdir", &d, path, NULL);
    }
    const int r = chdir(path);
    const int e = errno;
    char cwd[MAXPATHLEN];
    post("chdir", q, r, r == -1 ? e : 0, NULL, r == 0 ? (own_getcwd(cwd, sizeof(cwd)) != NULL ? cwd : "?") : NULL);
    errno = e;
    return r;
}
static int my_fchdir(int fd) {
    DETAIL;
    kvi(&d, "fd", "%lld", fd);
    fdpath(&d, fd);
    const unsigned long long q = pre("fchdir", &d, "-", NULL);
    const int r = fchdir(fd);
    const int e = errno;
    char cwd[MAXPATHLEN];
    post("fchdir", q, r, r == -1 ? e : 0, NULL, r == 0 ? (own_getcwd(cwd, sizeof(cwd)) != NULL ? cwd : "?") : NULL);
    errno = e;
    return r;
}

INTERPOSE(my_open, open);
INTERPOSE(my_openat, openat);
INTERPOSE(my_creat, creat);
INTERPOSE(my_fopen, fopen);
INTERPOSE(my_stat, stat);
INTERPOSE(my_lstat, lstat);
INTERPOSE(my_fstatat, fstatat);
INTERPOSE(my_access, access);
INTERPOSE(my_faccessat, faccessat);
INTERPOSE(my_opendir, opendir);
INTERPOSE(my_dlopen, dlopen);
INTERPOSE(my_getattrlist, getattrlist);
INTERPOSE(my_statfs, statfs);
INTERPOSE(my_readlink, readlink);
INTERPOSE(my_rename, rename);
INTERPOSE(my_renameat, renameat);
INTERPOSE(my_unlink, unlink);
INTERPOSE(my_unlinkat, unlinkat);
INTERPOSE(my_mkdir, mkdir);
INTERPOSE(my_mkdirat, mkdirat);
INTERPOSE(my_rmdir, rmdir);
INTERPOSE(my_truncate, truncate);
INTERPOSE(my_ftruncate, ftruncate);
INTERPOSE(my_symlink, symlink);
INTERPOSE(my_link, link);
INTERPOSE(my_chdir, chdir);
INTERPOSE(my_fchdir, fchdir);
