//! The fatal-signal handler and the plugin breadcrumb (a crash is attributed to the plugin op in flight).
//!
//! The file and its lines are the crash-log format the app reads:
//!
//! ```text
//! <log dir>/summoner-host-crash-<pid>.log   (appended; stderr when it cannot be opened)
//!
//! ==== summoner-host crash ====
//! Signal: SIGSEGV (11)
//! PID: <pid>
//! Backtrace:
//! <backtrace_symbols_fd lines>
//! plugin-op: <op> <detail>                    (only while a plugin op is in flight)
//! ```
//!
//! then `[summoner-host] FATAL <signal>, crash log: <path>` and the breadcrumb on stderr, and the signal re-raised
//! with its default action, so the process still dies of it (the supervisor reads the signal).
//!
//! Async-signal-safety: the handler only reads atomics and fixed buffers and calls `open`, `write`, `close`,
//! `backtrace`, `backtrace_symbols_fd`, `signal` and `raise` (the two backtrace calls are not on POSIX's list). The
//! breadcrumb's writer is the message thread only; a torn read can at worst garble the one breadcrumb line, never
//! memory. `backtrace` and `backtrace_symbols_fd` may allocate on their first call (lazy initialisation in libunwind /
//! dyld), so [`install`] makes that first call, outside any signal context.
//!
//! Reentrancy: a second fatal signal while the handler runs (a SIGBUS inside the SIGSEGV handler, say) goes straight
//! to the default action; the handler's own stack is not trusted twice.
//!
//! **The alternate stack's limit.** The handler runs on the thread's alternate signal stack (`SA_ONSTACK`), which
//! Rust's std gives every thread *it* creates (and the main thread), so a stack overflow there is logged. A thread a
//! plugin creates itself (`pthread_create`) has no alternate stack: a stack overflow on it faults again inside the
//! handler and the kernel ends the process without a log. A fix (a `sigaltstack` per foreign thread, installed by a
//! `pthread_create` interposer or at the thread's first call into the host) is future work, if a real plugin shows it.
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use summoner_proto::host_names::{CRASH_BANNER, CRASH_LOG_STEM, crash_log_name};

const OP_CAP: usize = 63;
const DETAIL_CAP: usize = 191;
const DIR_CAP: usize = 512;

static OP: [AtomicU8; OP_CAP] = [const { AtomicU8::new(0) }; OP_CAP];
static OP_LEN: AtomicUsize = AtomicUsize::new(0);
static DETAIL: [AtomicU8; DETAIL_CAP] = [const { AtomicU8::new(0) }; DETAIL_CAP];
static DETAIL_LEN: AtomicUsize = AtomicUsize::new(0);
static SET: AtomicBool = AtomicBool::new(false);
static DIR: [AtomicU8; DIR_CAP] = [const { AtomicU8::new(0) }; DIR_CAP];
static DIR_LEN: AtomicUsize = AtomicUsize::new(0);
/// The diagnostic log file the drainer writes (its name, set before it is opened) and the
/// last `seq` it flushed: the crash handler adds them as one line, atomics only.
const DIAG_CAP: usize = 256;
static DIAG: [AtomicU8; DIAG_CAP] = [const { AtomicU8::new(0) }; DIAG_CAP];
static DIAG_LEN: AtomicUsize = AtomicUsize::new(0);
/// The last flushed `seq`'s source (`runecraft_log::flushed_seq`: one relaxed atomic load, async-signal-safe).
static DIAG_SEQ: std::sync::OnceLock<fn() -> u64> = std::sync::OnceLock::new();

/// Names the diagnostic log file for the crash handler's line: its FILE NAME (as the host application's fatal-signal
/// line, `diag <file name> seq <n>`), truncated to 256 bytes.
pub fn set_diag_file(path: &Path) {
    let name = path.file_name().unwrap_or(path.as_os_str());
    store(&DIAG, &DIAG_LEN, name.as_encoded_bytes());
}

/// Where the crash handler reads the last `seq` the diagnostic log flushed (`runecraft_log::flushed_seq`, set once
/// at the log's install). The function must be an atomic load and nothing else: the handler calls it.
pub fn set_diag_seq_source(f: fn() -> u64) {
    let _ = DIAG_SEQ.set(f);
}

/// The log file at install was refused (not a plain, single-link regular file): the report goes to stderr.
static NO_FILE: AtomicBool = AtomicBool::new(false);
/// Set by the first fatal signal; a second one, while the handler runs, dies at once.
static IN_HANDLER: AtomicBool = AtomicBool::new(false);
/// Tests only: the handler raises SIGBUS after its header (a fault inside the handler).
#[cfg(test)]
static FAULT_IN_HANDLER: AtomicBool = AtomicBool::new(false);
/// Tests only: dying is `_exit(100 + signal)`, so the test sees the signal and macOS writes no crash report.
#[cfg(test)]
static EXIT_INSTEAD: AtomicBool = AtomicBool::new(false);

const SIGNALS: [libc::c_int; 5] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGABRT,
    libc::SIGFPE,
    libc::SIGILL,
];

fn store(dst: &[AtomicU8], len: &AtomicUsize, src: &[u8]) {
    let n = src.len().min(dst.len());
    for (d, s) in dst.iter().zip(&src[..n]) {
        d.store(*s, Ordering::Relaxed);
    }
    len.store(n, Ordering::Release);
}

/// Records the plugin op in flight (the message thread only): `op` and `detail` (the plugin's name), truncated to
/// caps of 63 and 191 bytes.
pub fn set_breadcrumb(op: &str, detail: &str) {
    SET.store(false, Ordering::Release);
    store(&OP, &OP_LEN, op.as_bytes());
    store(&DETAIL, &DETAIL_LEN, detail.as_bytes());
    SET.store(true, Ordering::Release);
}

/// No plugin op in flight.
pub fn clear_breadcrumb() {
    SET.store(false, Ordering::Release);
}

/// A fixed buffer the handler formats into (no allocation).
struct Line<const N: usize> {
    b: [u8; N],
    n: usize,
}

impl<const N: usize> Line<N> {
    fn new() -> Self {
        Line { b: [0; N], n: 0 }
    }
    fn bytes(&mut self, s: &[u8]) -> &mut Self {
        let k = s.len().min(N - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
        self.n += k;
        self
    }
    fn atomics(&mut self, src: &[AtomicU8], len: &AtomicUsize) -> &mut Self {
        let l = len.load(Ordering::Acquire).min(src.len());
        for a in &src[..l] {
            if self.n < N {
                self.b[self.n] = a.load(Ordering::Relaxed);
                self.n += 1;
            }
        }
        self
    }
    fn num(&mut self, v: i64) -> &mut Self {
        if v < 0 {
            self.bytes(b"-");
        }
        let mut d = [0u8; 20];
        let mut i = d.len();
        let mut u = v.unsigned_abs();
        loop {
            i -= 1;
            d[i] = b'0' + (u % 10) as u8;
            u /= 10;
            if u == 0 {
                break;
            }
        }
        self.bytes(&d[i..])
    }
    fn as_slice(&self) -> &[u8] {
        &self.b[..self.n]
    }
}

/// "plugin-op: <op> <detail>\n", or empty when no op is in flight.
fn breadcrumb_line(out: &mut Line<288>) {
    if !SET.load(Ordering::Acquire) {
        return;
    }
    out.bytes(b"plugin-op: ")
        .atomics(&OP, &OP_LEN)
        .bytes(b" ")
        .atomics(&DETAIL, &DETAIL_LEN)
        .bytes(b"\n");
}

fn write_all(fd: libc::c_int, s: &[u8]) {
    let mut off = 0;
    while off < s.len() {
        // SAFETY: the pointer and length name the live, initialised tail of `s`; write(2) only reads it.
        let n = unsafe { libc::write(fd, s[off..].as_ptr().cast(), s.len() - off) };
        if n <= 0 {
            return;
        }
        off += n as usize;
    }
}

fn signal_name(sig: libc::c_int) -> &'static [u8] {
    match sig {
        libc::SIGSEGV => b"SIGSEGV",
        libc::SIGBUS => b"SIGBUS",
        libc::SIGABRT => b"SIGABRT",
        libc::SIGFPE => b"SIGFPE",
        libc::SIGILL => b"SIGILL",
        _ => b"UNKNOWN",
    }
}

/// The way out: the default action for `sig`, then `sig` again. Raised from inside its own handler, `sig` is blocked
/// (no `SA_NODEFER`), so it is delivered as the handler returns, and the process dies of it.
fn die_of(sig: libc::c_int) {
    #[cfg(test)]
    if EXIT_INSTEAD.load(Ordering::Relaxed) {
        // SAFETY: _exit(2) is async-signal-safe and has no memory preconditions; it does not return.
        unsafe { libc::_exit(100 + sig) };
    }
    // SAFETY: restoring the default action and re-raising are the documented way out of a fatal-signal handler; the
    // process dies of `sig` once this handler returns.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

extern "C" fn handler(sig: libc::c_int) {
    if IN_HANDLER.swap(true, Ordering::AcqRel) {
        return die_of(sig); // a fault inside the handler: no second log on a stack we no longer trust
    }
    let pid = std::process::id() as i64; // getpid(2): async-signal-safe
    let mut path = Line::<640>::new();
    path.atomics(&DIR, &DIR_LEN)
        .bytes(b"/")
        .bytes(CRASH_LOG_STEM.as_bytes())
        .bytes(b"-")
        .num(pid)
        .bytes(b".log\0");
    // A log file refused at install: straight to stderr, nothing opened.
    let mut fd = if NO_FILE.load(Ordering::Acquire) {
        -1
    } else {
        // SAFETY: `path` is NUL-terminated just above (a truncated path loses its NUL only past 640 bytes, which
        // DIR_CAP rules out: 512 + 40, and `install` refuses a longer folder); open(2) reads it as a C string.
        unsafe {
            libc::open(
                path.as_slice().as_ptr().cast(),
                // No link followed at the file name, and not inherited by anything exec'd.
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_APPEND
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o644 as libc::c_uint,
            )
        }
    };
    // The child makes no directory in its HOME (the launcher's or the OS's business); a log that cannot be opened (no
    // $HOME/Library/Logs, say) is never lost: the whole report goes to stderr instead, where the supervisor's capture
    // has it.
    let to_stderr = fd < 0;
    if to_stderr {
        fd = libc::STDERR_FILENO;
    }
    let mut head = Line::<160>::new();
    head.bytes(b"\n")
        .bytes(CRASH_BANNER.as_bytes())
        .bytes(b"\nSignal: ")
        .bytes(signal_name(sig))
        .bytes(b" (")
        .num(sig as i64)
        .bytes(b")\nPID: ")
        .num(pid)
        .bytes(b"\nBacktrace:\n");
    write_all(fd, head.as_slice());
    if DIAG_LEN.load(Ordering::Acquire) > 0 {
        // The diagnostic log's file and the last seq it flushed (no ring is formatted here), in the host application's
        // token: `diag <file name> seq <n>`.
        let mut diag = Line::<320>::new();
        diag.bytes(b"diag ")
            .atomics(&DIAG, &DIAG_LEN)
            .bytes(b" seq ")
            .num(DIAG_SEQ.get().map_or(0, |f| f()) as i64)
            .bytes(b"\n");
        write_all(fd, diag.as_slice());
    }
    #[cfg(test)]
    if FAULT_IN_HANDLER.load(Ordering::Relaxed) {
        // SAFETY: raise(2) has no memory preconditions; SIGBUS is not blocked here, so it re-enters the handler.
        unsafe { libc::raise(libc::SIGBUS) };
    }
    let mut frames = [std::ptr::null_mut::<libc::c_void>(); 64];
    // SAFETY: `frames` holds 64 writable pointers; backtrace writes at most that many and returns the count, which
    // backtrace_symbols_fd reads back from the same buffer.
    unsafe {
        let n = libc::backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
        libc::backtrace_symbols_fd(frames.as_ptr(), n, fd);
    }
    let mut crumb = Line::<288>::new();
    breadcrumb_line(&mut crumb);
    write_all(fd, crumb.as_slice());
    if fd != libc::STDERR_FILENO {
        // SAFETY: fd was opened above by this handler and is closed once.
        unsafe { libc::close(fd) };
    }
    let mut notice = Line::<800>::new();
    notice
        .bytes(b"[summoner-host] FATAL ")
        .bytes(signal_name(sig))
        .bytes(if to_stderr {
            b", crash log NOT written (cannot open " as &[u8]
        } else {
            b", crash log: "
        })
        .bytes(&path.as_slice()[..path.n - 1])
        .bytes(if to_stderr {
            b"): the report is above, on stderr\n" as &[u8]
        } else {
            b"\n"
        });
    write_all(libc::STDERR_FILENO, notice.as_slice());
    if !to_stderr {
        write_all(libc::STDERR_FILENO, crumb.as_slice()); // (already on stderr when the report is)
    }
    die_of(sig);
}

/// The handler's calls that may initialise lazily (allocate) on first use, made once here, outside any signal
/// context: `backtrace`, `backtrace_symbols_fd` (into /dev/null) and `getpid`.
fn prime() {
    let _ = std::process::id();
    let mut frames = [std::ptr::null_mut::<libc::c_void>(); 8];
    // SAFETY: the path is a NUL-terminated literal; `frames` holds 8 writable pointers and backtrace writes at most
    // that many; backtrace_symbols_fd reads back the count it returned; the descriptor is opened here and closed once.
    unsafe {
        let n = libc::backtrace(frames.as_mut_ptr(), frames.len() as libc::c_int);
        let fd = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
        if fd >= 0 {
            libc::backtrace_symbols_fd(frames.as_ptr(), n, fd);
            libc::close(fd);
        }
    }
}

/// Installs the handler for SIGSEGV, SIGBUS, SIGABRT, SIGFPE and SIGILL, logging into `log_dir` (normally
/// `$HOME/Library/Logs`, or `/tmp` without a HOME). The directory is not created: if it is missing, the log
/// goes to stderr. Call once, at start, before any thread that hosts a plugin.
/// `Err` (nothing installed) when `log_dir` does not fit the handler's buffer (longer than 512 bytes: never
/// truncated), or when this process's log file already exists as anything but a plain regular file with one link (as
/// the host application's crash log does). The caller decides: with `--crash-log` a usage
/// error; without it, [`install_to_stderr`].
pub fn install(log_dir: &Path) -> Result<(), crate::Error> {
    install_why(log_dir).map_err(crate::Error::from)
}

fn install_why(log_dir: &Path) -> Result<(), String> {
    let dir = log_dir.as_os_str().as_encoded_bytes();
    if dir.len() > DIR_CAP {
        return Err(format!(
            "the crash-log folder is {} bytes, longer than {DIR_CAP}",
            dir.len()
        ));
    }
    let file = log_dir.join(crash_log_name(std::process::id()));
    if let Ok(m) = std::fs::symlink_metadata(&file) {
        use std::os::unix::fs::MetadataExt;
        if !m.file_type().is_file() || m.nlink() != 1 {
            return Err(format!(
                "{} exists and is not a plain regular file",
                file.display()
            ));
        }
    }
    store(&DIR, &DIR_LEN, dir);
    NO_FILE.store(false, Ordering::Release);
    arm();
    Ok(())
}

/// The handler, every report to stderr (the supervisor's capture has it): the no-flag fallback when [`install`]
/// refuses its folder.
pub fn install_to_stderr() {
    NO_FILE.store(true, Ordering::Release);
    arm();
}

fn arm() {
    prime();
    for sig in SIGNALS {
        // SAFETY: an all-zero sigaction is a valid starting value (no flags, empty mask) that is filled in before use;
        // `handler` is an `extern "C" fn(c_int)`, the shape sa_sigaction takes without SA_SIGINFO.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_ONSTACK;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// The lab's death switch (feature `lab-deaths`, enabled by the test double alone, so the product child never reads
/// it: the release-build link guard checks the name is absent from a build without test-hooks): with
/// `SUMMONER_LAB_DEATH=kill` in the environment, every lab death (the test double's, the lab plugins') is a SIGKILL,
/// which leaves no crash report in the user's real `~/Library/Logs/DiagnosticReports`. Tests that only need "the child
/// died" set it; the few that test a signal's kind or its handler do not. The supervisor maps any death to a crash
/// either way.
#[cfg(feature = "lab-deaths")]
pub fn quiet_deaths() -> bool {
    static Q: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *Q.get_or_init(|| {
        std::env::var_os(summoner_proto::host_names::LAB_DEATH_ENV).is_some_and(|v| v == "kill")
    })
}

#[cfg(feature = "lab-deaths")]
fn kill_self() -> ! {
    // SAFETY: kill(2) on our own pid with SIGKILL has no memory preconditions; the process ends at once.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    loop {
        std::hint::spin_loop();
    }
}

/// A lab death by `abort()`, or a SIGKILL under [`quiet_deaths`].
#[cfg(feature = "lab-deaths")]
pub fn lab_abort() -> ! {
    if quiet_deaths() {
        kill_self();
    }
    std::process::abort()
}

/// A lab death by SIGSEGV ([`raise_segv`]), or a SIGKILL under [`quiet_deaths`].
#[cfg(feature = "lab-deaths")]
pub fn lab_segv() -> ! {
    if quiet_deaths() {
        kill_self();
    }
    raise_segv()
}

/// The test double's hostile state `SEGV`: a SIGSEGV on the calling thread, as a bad pointer inside a plugin gives.
pub fn raise_segv() -> ! {
    // SAFETY: raise(2) has no memory preconditions; with the handler installed it logs and re-raises, without it the
    // default action ends the process. Either way control does not return here.
    unsafe {
        libc::raise(libc::SIGSEGV);
    }
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `install` refuses this process's log file when it already exists as a symlink or a multi-link file (as a host
    /// application's own crash-log install does), and a folder longer than the handler's buffer; a plain
    /// existing log, or none, is accepted.
    #[test]
    fn install_refuses_a_planted_log_file_and_a_too_long_folder() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target.noindex/tmp")
            .join(format!("crash-n1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = dir.join(crash_log_name(std::process::id()));
        assert!(install(&dir).is_ok(), "no file yet");
        std::fs::write(&name, b"old").unwrap();
        assert!(install(&dir).is_ok(), "a plain existing log is appended to");
        std::fs::remove_file(&name).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), &name).unwrap();
        assert!(install(&dir).is_err(), "a symlink at the name");
        std::fs::remove_file(&name).unwrap();
        std::fs::write(dir.join("other"), b"x").unwrap();
        std::fs::hard_link(dir.join("other"), &name).unwrap();
        assert!(install(&dir).is_err(), "a hard link at the name");
        let long = Path::new("/").join("d".repeat(600));
        assert!(
            install(&long).is_err(),
            "longer than DIR_CAP: refused, never truncated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fault inside the handler (SIGBUS while it logs a SIGSEGV) dies of the second signal with one log, instead of
    /// re-entering. Run in a forked child of this test (waited for by its pid), which exits 100 + signal.
    #[test]
    fn a_fault_inside_the_handler_dies_at_once_with_one_log() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("crash-reentry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: fork(2) in a test process; the child only stores atomics, installs the handler and raises, then
        // leaves by _exit (from die_of), never returning into the test harness.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            EXIT_INSTEAD.store(true, Ordering::Relaxed);
            FAULT_IN_HANDLER.store(true, Ordering::Relaxed);
            install(&dir).expect("install");
            // SAFETY: as raise_segv; with EXIT_INSTEAD the handler leaves by _exit.
            unsafe {
                libc::raise(libc::SIGSEGV);
                libc::_exit(0);
            }
        }
        let mut st = 0;
        // SAFETY: pid is this process's child; st is a live out-parameter.
        assert_eq!(unsafe { libc::waitpid(pid, &mut st, 0) }, pid);
        let log = std::fs::read_to_string(dir.join(crash_log_name(pid as u32))).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(libc::WIFEXITED(st), "status {st}");
        assert_eq!(
            libc::WEXITSTATUS(st),
            100 + libc::SIGBUS,
            "died of the second signal"
        );
        assert_eq!(log.matches(CRASH_BANNER).count(), 1, "{log}");
        assert!(log.contains("Signal: SIGSEGV (11)"), "{log}");
    }

    #[test]
    fn the_breadcrumb_line_is_the_oracle_childs_and_truncates_to_its_caps() {
        set_breadcrumb("setState", "segv");
        let mut l = Line::<288>::new();
        breadcrumb_line(&mut l);
        assert_eq!(l.as_slice(), b"plugin-op: setState segv\n");
        clear_breadcrumb();
        let mut l = Line::<288>::new();
        breadcrumb_line(&mut l);
        assert!(l.as_slice().is_empty());
        let long = "x".repeat(400);
        set_breadcrumb(&long, &long);
        let mut l = Line::<288>::new();
        breadcrumb_line(&mut l);
        assert_eq!(l.n, "plugin-op: ".len() + OP_CAP + 1 + DETAIL_CAP + 1);
        clear_breadcrumb();
        let mut n = Line::<32>::new();
        n.num(0).bytes(b" ").num(-42).bytes(b" ").num(1234567890123);
        assert_eq!(n.as_slice(), b"0 -42 1234567890123");
    }
}
