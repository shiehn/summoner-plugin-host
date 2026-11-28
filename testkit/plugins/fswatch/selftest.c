// The fswatch recorder's self-test (v3): run with DYLD_INSERT_LIBRARIES=<libsummoner_lab_fswatch.dylib> and
// SUMMONER_FSWATCH_LOG=<log>, its working directory and argv[1] the build tree's scratch dir (never anywhere else). It makes
// each interposed call on files of its own, then reads the log and checks every call's PRE (the attempt: its keys and
// path, exactly) and, by the pair's key (pid, tid, seq), its POST (the result, exactly; every expected result is the
// one the call actually returned here, never a guess). Plus v3's cases: a failed open of "", a relative open placed by
// the header's cwd, a chdir then a relative open, an fchdir, a write refused with EACCES, a bare-name dlopen with its
// resolved image=, a call blocked mid-way (its PRE while it hangs; no POST once its process is killed in the call), a
// fork (the child's own header and pairs) and a fork + exec (two headers, one pid). And the cuts, always marked:
// a cwd too long once escaped (a header writes `cwd=?<TAB>trunc=1`; a chdir's POST a cut cwd=
// then `,trunc=1`) and a renameat whose two dirpaths overflow its detail (the mark kept, nothing after the cut).
// Exit 0 on success, 1 (with every failed check on stderr) otherwise. `--exec-child <path>` is the exec'd image.
#include <dirent.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;
static char logbuf[1 << 18];
static const char* from = logbuf;  // this process's part of the log (from its own header on)
static char me[64];                // "pid=<pid>,tid=<tid>,seq="

static void fail(const char* what) {
    fprintf(stderr, "fswatch selftest: %s\n", what);
    ++failures;
}

// `call`'s PRE by this thread, its detail after the key exactly `rest` ("" or ",keys…") and its path field(s) exactly
// `paths`; and its POST (the same key) with the tail exactly `post`.
static void pair(const char* call, const char* rest, const char* paths, const char* post) {
    char head[128], tail[2400];
    snprintf(head, sizeof(head), "\n%s\t%s", call, me);
    snprintf(tail, sizeof(tail), "%s\t%s\n", rest, paths);
    for (const char* p = strstr(from, head); p != NULL; p = strstr(p + 1, head)) {
        const char* s = p + strlen(head);
        char* end = NULL;
        const unsigned long long seq = strtoull(s, &end, 10);
        if (end == s || strncmp(end, tail, strlen(tail)) != 0)
            continue;
        char want[2600];
        snprintf(want, sizeof(want), "\n%s.ret\t%s%llu,%s\t-\n", call, me, seq, post);
        if (strstr(from, want) == NULL) {
            char w[3000];
            snprintf(w, sizeof(w), "no POST \"%s\" for the PRE of %s%s %s", want + 1, call, rest, paths);
            fail(w);
        }
        return;
    }
    char w[3000];
    snprintf(w, sizeof(w), "no PRE: %s %s %s", call, rest, paths);
    fail(w);
}

static void readlog(const char* logp) {
    FILE* lf = fopen(logp, "r");
    if (lf == NULL) {
        fail("no log");
        exit(1);
    }
    logbuf[0] = '\n';
    const size_t n = fread(logbuf + 1, 1, sizeof(logbuf) - 2, lf);
    logbuf[n + 1] = 0;
    fclose(lf);
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "--exec-child") == 0) {
        struct stat st;
        return stat(argv[2], &st) == 0 ? 0 : 1;  // the exec'd image: one recorded call
    }
    const char* logp = getenv("SUMMONER_FSWATCH_LOG");
    if (argc < 2 || logp == NULL || getenv("DYLD_INSERT_LIBRARIES") == NULL) {
        fprintf(stderr, "usage: SUMMONER_FSWATCH_LOG=… DYLD_INSERT_LIBRARIES=… %s <scratch dir>\n", argv[0]);
        return 2;
    }
    unsigned long long tid = 0;
    pthread_threadid_np(NULL, &tid);
    snprintf(me, sizeof(me), "pid=%d,tid=%llu,seq=", (int)getpid(), tid);
    char start[1024], realdir[1024];
    if (getcwd(start, sizeof(start)) == NULL || realpath(argv[1], realdir) == NULL)
        return 2;
    if (strcmp(start, realdir) != 0)
        fail("run me from the scratch dir (argv[1] must be the cwd: the relative cases need it)");
    char d[1100], a[1200], b[1200], c[1200], s[1200], l[1200], sub[1200], fifo[1200], ro[1200], tabname[1200],
        lfname[1200], rel[1200], line[2600], r1[1400], r2[1400];
    snprintf(d, sizeof(d), "%s/fswatch-selftest-%d", realdir, (int)getpid());
    snprintf(a, sizeof(a), "%s/a", d);
    snprintf(b, sizeof(b), "%s/b", d);
    snprintf(c, sizeof(c), "%s/c", d);
    snprintf(s, sizeof(s), "%s/s", d);
    snprintf(l, sizeof(l), "%s/l", d);
    snprintf(sub, sizeof(sub), "%s/sub", d);
    snprintf(fifo, sizeof(fifo), "%s/fifo", d);
    snprintf(ro, sizeof(ro), "%s/ro", d);
    snprintf(tabname, sizeof(tabname), "%s/t\tab", d);
    snprintf(lfname, sizeof(lfname), "%s/l\nf", d);
    snprintf(rel, sizeof(rel), "fswatch-selftest-%d/a", (int)getpid());

    // ---- the calls (on our own scratch files only); every result kept for its check ----
    const int r_mkdir = mkdir(d, 0755);
    const int wfd = open(a, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    (void)!write(wfd, "x", 1);
    const int r_ftr = ftruncate(wfd, 0);
    close(wfd);
    const int afd = openat(AT_FDCWD, a, O_RDONLY);
    close(afd);
    const int cfd = creat(c, 0600);
    close(cfd);
    FILE* f = fopen(c, "w");
    fclose(f);
    const int r_tr = truncate(c, 3);
    const int r_rn = rename(a, b);
    const int r_rna = renameat(AT_FDCWD, b, AT_FDCWD, a);
    const int r_sym = symlink(a, s);
    const int r_ln = link(a, l);
    struct stat st;
    const int r_st = stat(a, &st);
    const int r_lst = lstat(s, &st);
    const int r_acc = access(a, R_OK);
    char rl[64];
    const ssize_t r_rl = readlink(s, rl, sizeof(rl));
    DIR* dir = opendir(d);
    closedir(dir);
    const int bad = open("", O_RDONLY);       // v3: the empty path (the oracle toolkit opens "" at start-up)
    const int relfd = open(rel, O_RDONLY);    // v3: relative, placed by the header's cwd
    close(relfd);
    const int startfd = open(start, O_RDONLY | O_DIRECTORY);
    const int r_cd = chdir(d);                // v3: a chdir, then a relative open
    const int afd2 = open("a", O_RDONLY);
    close(afd2);
    const int r_fcd = fchdir(startfd);        // v3: an fchdir back
    close(startfd);
    close(open(ro, O_WRONLY | O_CREAT, 0444));
    const int denied = open(ro, O_WRONLY);    // v3: a write refused with EACCES, still the attempt it was
    (void)!chmod(ro, 0644);
    void* z = dlopen("libz.dylib", RTLD_NOW);  // v3: a bare name; its POST names the image dyld resolved
    Dl_info zi;
    memset(&zi, 0, sizeof(zi));
    const int zok = z != NULL && dladdr(dlsym(z, "zlibVersion"), &zi) != 0 && zi.dli_fname != NULL;
    const int tfd = open(tabname, O_WRONLY | O_CREAT, 0600);  // a TAB and a LF in names
    close(tfd);
    const int lfd = open(lfname, O_WRONLY | O_CREAT, 0600);
    close(lfd);
    const int dfd = open(d, O_RDONLY | O_DIRECTORY);  // an openat against a real dirfd
    const int rfd = openat(dfd, "rel", O_WRONLY | O_CREAT, 0600);
    close(rfd);
    const int r_ua = unlinkat(dfd, "rel", 0);
    static char longp[5000];  // a path cut to fit
    memset(longp, 'q', sizeof(longp) - 1);
    longp[0] = '/';
    (void)!stat(longp, &st);

    // A call blocked mid-way: a child opens a FIFO no one writes (open blocks). While it hangs its PRE is in the log
    // and no POST; killed IN the call (SIGKILL: a crash-report-free stand-in for a crash), it never gets one.
    (void)!mkfifo(fifo, 0600);
    const pid_t hung = fork();
    if (hung == 0) {
        (void)!open(fifo, O_RDONLY);
        _exit(0);
    }
    usleep(300 * 1000);
    int ws = 0;
    const pid_t kid = fork();  // the fork case: the child's own header, then its pair
    if (kid == 0) {
        _exit(stat(d, &st) == 0 ? 0 : 1);
    }
    waitpid(kid, &ws, 0);
    const pid_t ex = fork();  // the fork + exec case: a recorded call before the exec, then the new image's
    if (ex == 0) {
        (void)!stat(d, &st);
        execl(argv[0], argv[0], "--exec-child", d, (char*)NULL);
        _exit(127);
    }
    waitpid(ex, &ws, 0);
    // The cuts: a child goes three directories deep, each named with 200 control bytes (0x01, 4 bytes escaped), so its
    // cwd escapes to ~2.5 KiB, more than a field holds. A grandchild's first record writes its header with that cwd;
    // the third chdir's POST cuts its cwd=; a renameat between depths 1 and 2 overflows its detail.
    char comp[201];
    memset(comp, 1, 200);
    comp[200] = 0;
    int gpipe[2];
    (void)!pipe(gpipe);
    const pid_t deep = fork();
    if (deep == 0) {
        (void)!chdir(d);
        (void)!mkdir(comp, 0755);
        (void)!chdir(comp);
        const int fd1 = open(".", O_RDONLY | O_DIRECTORY);
        (void)!mkdir(comp, 0755);
        (void)!chdir(comp);
        const int fd2 = open(".", O_RDONLY | O_DIRECTORY);
        (void)!mkdir(comp, 0755);
        (void)!chdir(comp);
        const pid_t gc = fork();
        if (gc == 0)
            _exit(stat("x", &st) == 0 ? 1 : 0);
        int gs = 0;
        waitpid(gc, &gs, 0);
        (void)!renameat(fd1, "nope", fd2, "x");
        char fds[64];
        snprintf(fds, sizeof(fds), "%d %d %d\n", (int)gc, fd1, fd2);
        (void)!write(gpipe[1], fds, strlen(fds));
        for (int k = 0; k < 3; ++k) {
            (void)!chdir("..");
            (void)!rmdir(comp);
        }
        _exit(0);
    }
    close(gpipe[1]);
    char gbuf[64] = {0};
    (void)!read(gpipe[0], gbuf, sizeof(gbuf) - 1);
    close(gpipe[0]);
    waitpid(deep, &ws, 0);
    int gc = 0, dfd1 = 0, dfd2 = 0;
    if (sscanf(gbuf, "%d %d %d", &gc, &dfd1, &dfd2) != 3)
        fail("the deep child reported nothing");
    char hungpre[64], hungpost[64];
    snprintf(hungpre, sizeof(hungpre), "\nopen\tpid=%d,", (int)hung);
    snprintf(hungpost, sizeof(hungpost), "\nopen.ret\tpid=%d,", (int)hung);
    readlog(logp);
    if (strstr(logbuf, hungpre) == NULL)
        fail("the hung call's PRE is not in the log while it hangs");
    if (strstr(logbuf, hungpost) != NULL)
        fail("the hung call has a POST while it hangs");
    kill(hung, SIGKILL);
    waitpid(hung, &ws, 0);

    // ---- the checks (the log reread after the kill) ----
    readlog(logp);
    if (strstr(logbuf, hungpost) != NULL)
        fail("the call killed mid-way has a POST");
    char head[1200];
    snprintf(head, sizeof(head), "\nfswatch\tv3\tpid=%d\tcwd=%s\n", (int)getpid(), start);
    const char* mine = strstr(logbuf, head);
    if (mine == NULL)
        fail("no v3 header for this pid with the cwd it started in");
    else
        from = mine;
#define RET(v) (snprintf(r2, sizeof(r2), "ret=%lld", (long long)(v)), r2)
#define FD_RET(v) ((v) >= 0 ? RET(v) : "ret=-1")
#define P(call, rest, post, ...) (snprintf(line, sizeof(line), __VA_ARGS__), pair(call, rest, line, post))
    P("mkdir", ",mode=0755", RET(r_mkdir), "%s", d);
    snprintf(r1, sizeof(r1), ",flags=0x%x,mode=0644", (unsigned)(O_WRONLY | O_CREAT | O_TRUNC));
    P("open", r1, FD_RET(wfd), "%s", a);
    snprintf(r1, sizeof(r1), ",fd=%d,fdpath=%s,len=0", wfd, a);
    pair("ftruncate", r1, "-", RET(r_ftr));
    snprintf(r1, sizeof(r1), ",dirfd=%d,flags=0x%x", AT_FDCWD, (unsigned)O_RDONLY);
    P("openat", r1, FD_RET(afd), "%s", a);
    P("creat", ",mode=0600", FD_RET(cfd), "%s", c);
    P("fopen", ",fmode=w", "ret=0", "%s", c);
    P("truncate", ",len=3", RET(r_tr), "%s", c);
    P("rename", "", RET(r_rn), "%s\t%s", a, b);
    snprintf(r1, sizeof(r1), ",dirfd=%d,todirfd=%d", AT_FDCWD, AT_FDCWD);
    P("renameat", r1, RET(r_rna), "%s\t%s", b, a);
    P("symlink", "", RET(r_sym), "%s\t%s", a, s);
    P("link", "", RET(r_ln), "%s\t%s", a, l);
    P("stat", "", RET(r_st), "%s", a);
    P("lstat", "", RET(r_lst), "%s", s);
    P("access", "", RET(r_acc), "%s", a);
    P("readlink", "", RET(r_rl), "%s", s);
    P("opendir", "", "ret=0", "%s", d);
    snprintf(r1, sizeof(r1), ",flags=0x%x", (unsigned)O_RDONLY);
    if (bad != -1)
        fail("open(\"\") succeeded");
    P("open", r1, "ret=-1,errno=ENOENT", "%s", "");  // the empty path: a failed call that reached no file
    P("open", r1, FD_RET(relfd), "%s", rel);         // relative: placed by the header's cwd
    snprintf(r2, sizeof(r2), "ret=%d,cwd=%s", r_cd, d);
    P("chdir", "", r2, "%s", d);
    P("open", r1, FD_RET(afd2), "%s", "a");  // relative, after the chdir
    snprintf(r1, sizeof(r1), ",fd=%d,fdpath=%s", startfd, start);
    snprintf(r2, sizeof(r2), "ret=%d,cwd=%s", r_fcd, start);
    pair("fchdir", r1, "-", r2);
    snprintf(r1, sizeof(r1), ",flags=0x%x", (unsigned)O_WRONLY);
    if (denied != -1)
        fail("the read-only file opened for writing (running as root?)");
    P("open", r1, "ret=-1,errno=EACCES", "%s", ro);  // the refused write, recorded as the attempt
    if (!zok)
        fail("libz.dylib did not load, or its image could not be named");
    else {
        snprintf(r2, sizeof(r2), "ret=0,image=%s", zi.dli_fname);
        P("dlopen", "", r2, "%s", "libz.dylib");
    }
    snprintf(r1, sizeof(r1), ",flags=0x%x,mode=0600", (unsigned)(O_WRONLY | O_CREAT));
    P("open", r1, FD_RET(tfd), "%s/t\\tab", d);
    P("open", r1, FD_RET(lfd), "%s/l\\nf", d);
    snprintf(r1, sizeof(r1), ",dirfd=%d,dirpath=%s,flags=0x%x,mode=0600", dfd, d, (unsigned)(O_WRONLY | O_CREAT));
    P("openat", r1, FD_RET(rfd), "%s", "rel");
    snprintf(r1, sizeof(r1), ",dirfd=%d,dirpath=%s,flag=0x0", dfd, d);
    P("unlinkat", r1, RET(r_ua), "%s", "rel");
    close(dfd);
    {
        char sh[128];
        snprintf(sh, sizeof(sh), "\nstat\t%s", me);
        int cut = 0;
        for (const char* p = strstr(from, sh); p != NULL; p = strstr(p + 1, sh)) {
            const char* e = strchr(p + 1, '\n');
            const char* t = strstr(p, ",trunc=1\t/qqqq");
            if (t != NULL && e != NULL && t < e)
                cut = 1;
        }
        if (!cut)
            fail("the long path's PRE is not marked trunc=1");
    }
    {
        char kh[96], kp[96], kr[96], eh[96];
        snprintf(kh, sizeof(kh), "\nfswatch\tv3\tpid=%d\tcwd=", (int)kid);
        snprintf(kp, sizeof(kp), "\nstat\tpid=%d,", (int)kid);
        snprintf(kr, sizeof(kr), "\nstat.ret\tpid=%d,", (int)kid);
        const char* h = strstr(logbuf, kh);
        const char* p = strstr(logbuf, kp);
        if (h == NULL || p == NULL || p < h || strstr(logbuf, kr) == NULL)
            fail("the forked child: its own header first, then its stat's PRE and POST");
        snprintf(eh, sizeof(eh), "\nfswatch\tv3\tpid=%d\tcwd=", (int)ex);
        const char* h1 = strstr(logbuf, eh);
        if (h1 == NULL || strstr(h1 + 1, eh) == NULL)
            fail("the fork + exec child: two headers with its pid");
    }
    // The cuts, always marked, nothing after them.
    {
        static char x1[1000], e1[1200], e2[2200], e3[3200], want[3600];
        x1[0] = 0;
        for (int k = 0; k < 200; ++k)
            strcat(x1, "\\x01");
        snprintf(e1, sizeof(e1), "%s/%s", d, x1);
        snprintf(e2, sizeof(e2), "%s/%s", e1, x1);
        snprintf(e3, sizeof(e3), "%s/%s", e2, x1);
        snprintf(want, sizeof(want), "\nfswatch\tv3\tpid=%d\tcwd=?\ttrunc=1\n", gc);
        if (strstr(logbuf, want) == NULL)
            fail("the grandchild's header: a cut cwd must be written cwd=?<TAB>trunc=1");
        // The third chdir's POST: cwd= a PREFIX of the real escaped cwd, then exactly `,trunc=1<TAB>-`.
        snprintf(want, sizeof(want), "\nchdir.ret\tpid=%d,", (int)deep);
        int cutcwd = 0;
        for (const char* p = strstr(logbuf, want); p != NULL; p = strstr(p + 1, want)) {
            const char* e = strchr(p + 1, '\n');
            const char* c = strstr(p, ",cwd=");
            if (e == NULL || c == NULL || c > e)
                continue;
            const char* t = strstr(c, ",trunc=1\t-\n");
            if (t == NULL || t + strlen(",trunc=1\t-\n") != e + 1)
                continue;
            c += 5;
            if ((size_t)(t - c) < strlen(e3) && strncmp(c, e3, (size_t)(t - c)) == 0)
                cutcwd = 1;
        }
        if (!cutcwd)
            fail("the deep chdir's POST: a cut cwd= (a prefix of the real one) then exactly ,trunc=1");
        // The renameat: its dirpath whole, its todirpath a prefix of the real one, the mark, then its two paths.
        snprintf(want, sizeof(want), "\nrenameat\tpid=%d,", (int)deep);
        const char* p = strstr(logbuf, want);
        const char* e = p != NULL ? strchr(p + 1, '\n') : NULL;
        char mid[1400];
        snprintf(mid, sizeof(mid), ",dirfd=%d,dirpath=%s,todirfd=%d,todirpath=", dfd1, e1, dfd2);
        const char* m = p != NULL ? strstr(p, mid) : NULL;
        const char* t = m != NULL ? strstr(m, ",trunc=1\tnope\tx\n") : NULL;
        if (p == NULL || m == NULL || t == NULL || m > e || t + strlen(",trunc=1\tnope\tx\n") != e + 1)
            fail("the renameat: dirpath whole, then todirpath, then exactly ,trunc=1<TAB>nope<TAB>x");
        else {
            const char* v = m + strlen(mid);
            if (!((size_t)(t - v) < strlen(e2) && strncmp(v, e2, (size_t)(t - v)) == 0))
                fail("the renameat's todirpath is not a prefix of the real one (garbled after the cut)");
        }
    }
    // The spec's first rule, everywhere in this run's part of the log: no record of a pid before that pid's header (the
    // recorder's own getcwd once wrote a stat before its process's header: this is the check that caught it).
    {
        int seen[64] = {0}, nseen = 0;
        for (const char* ln = from + 1; *ln != 0;) {
            const char* end = strchr(ln, '\n');
            const char* pp = strstr(ln, "pid=");
            if (pp != NULL && (end == NULL || pp < end)) {
                const int pid = atoi(pp + 4);
                int known = 0;
                for (int k = 0; k < nseen; ++k)
                    known |= seen[k] == pid;
                if (strncmp(ln, "fswatch\t", 8) == 0) {
                    if (!known && nseen < 64)
                        seen[nseen++] = pid;
                } else if (!known) {
                    char w[200];
                    snprintf(w, sizeof(w), "a record of pid %d before its header: %.60s", pid, ln);
                    fail(w);
                    if (nseen < 64)
                        seen[nseen++] = pid;  // once per pid
                }
            }
            if (end == NULL)
                break;
            ln = end + 1;
        }
    }
    // Every line from this process's header on is whole: a header, or a known call / call.ret, then a TAB.
    static const char* calls[] = {"fswatch", "open", "openat", "creat", "fopen", "stat", "lstat", "fstatat", "access",
                                  "faccessat", "opendir", "dlopen", "getattrlist", "statfs", "readlink", "rename",
                                  "renameat", "unlink", "unlinkat", "mkdir", "mkdirat", "rmdir", "truncate",
                                  "ftruncate", "symlink", "link", "chdir", "fchdir", NULL};
    for (const char* ln = from + 1; *ln != 0;) {
        const char* end = strchr(ln, '\n');
        const size_t len = end != NULL ? (size_t)(end - ln) : strlen(ln);
        int known = 0;
        for (const char** k = calls; *k != NULL; ++k) {
            const size_t cl = strlen(*k);
            if (len > cl && strncmp(ln, *k, cl) == 0 && (ln[cl] == '\t' || strncmp(ln + cl, ".ret\t", 5) == 0))
                known = 1;
        }
        if (!known) {
            char w[200];
            snprintf(w, sizeof(w), "not a whole record: %.*s", (int)(len > 80 ? 80 : len), ln);
            fail(w);
        }
        if (end == NULL)
            break;
        ln = end + 1;
    }
    // Our scratch files, removed.
    (void)!unlink(fifo);
    (void)!unlink(ro);
    (void)!unlink(tabname);
    (void)!unlink(lfname);
    (void)!unlink(l);
    (void)!unlinkat(AT_FDCWD, s, 0);
    (void)!unlink(a);
    (void)!unlink(c);
    (void)!rmdir(d);
    printf("fswatch selftest: %s (%d failed checks)\n", failures == 0 ? "PASS" : "FAIL", failures);
    return failures == 0 ? 0 : 1;
}
