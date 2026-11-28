//! No public crate of the plugin host signals a pid or a group directly. A raw signal to a pid (`sigkill(c.pid)`, as
//! `incompatible()` once had) would reach a freed pid, possibly another process's, whenever the reaper got there first.
//! The floor keeps its raw signals crate-private (`runecraft_ipc::process::Child::kill` and
//! `GroupLeader::kill_group` are the only ways out, and both refuse once the child is reaped), so a call through the
//! floor no longer compiles; this source guard covers the rest (a crate's own `libc::kill`, `killpg`, or a helper of
//! the same name). It fails at test time on any raw signal call in the `src` of every public crate but the floor's.
//!
//! Its control plants a raw call in a scratch copy of a crate's sources and must find it.

use std::path::{Path, PathBuf};

/// The calls that signal a pid or a group directly.
const RAW: [&str; 5] = [
    "sigkill(",
    "sigterm(",
    "signal_pid(",
    "killpg(",
    "libc::kill(",
];

/// Every public crate that could call them (all but runecraft-ipc, which owns the one `kill(2)` and `killpg(2)`).
const GUARDED: [&str; 10] = [
    "runecraft-contracts",
    "runecraft-rt",
    "runecraft-log",
    "orrery-time",
    "summoner-proto",
    "summoner-plugin",
    "summoner-rt",
    "summoner",
    "summoner-scan",
    "summoner-testkit",
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The raw signal calls in `text` outside comments, as (line number, line).
fn raw_calls(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let code = l.split("//").next().unwrap_or("");
            RAW.iter().any(|r| {
                code.match_indices(r).any(|(i, _)| {
                    // `sigkill(` inside a longer name (`sigkill_self(` does not match; `my_sigkill(` would) is another
                    // function: require a non-identifier character (or the start) before the match.
                    code[..i]
                        .chars()
                        .last()
                        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
                })
            })
        })
        .map(|(n, l)| (n + 1, l.trim().to_string()))
        .collect()
}

/// Every raw call under the `src` folders of `crates` below `root`, as `file:line: text`.
fn scan(root: &Path, crates: &[&str]) -> (usize, Vec<String>) {
    let mut files = Vec::new();
    for c in crates {
        let src = root.join(c).join("src");
        // The floor's crates (runecraft-*, orrery-time) sit beside this crate in this repo but ship in their own
        // repos in an export: a floor crate that is absent here is guarded where it lives. A summoner crate is
        // always beside this one, so its absence is a failure (`rs_files` panics on it).
        if !c.starts_with("summoner") && !src.is_dir() {
            continue;
        }
        rs_files(&src, &mut files);
    }
    let mut found = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (n, l) in raw_calls(&text) {
            found.push(format!("{}:{n}: {l}", f.display()));
        }
    }
    (files.len(), found)
}

fn crates_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn no_public_crate_sends_a_raw_signal() {
    let (files, found) = scan(&crates_root(), &GUARDED);
    assert!(
        files >= GUARDED.len(),
        "the guard reads the sources ({files} files)"
    );
    assert!(
        found.is_empty(),
        "raw signal calls (kill through the Child or the GroupLeader instead):\n{}",
        found.join("\n")
    );
}

/// The control: a raw call planted in a scratch copy of a guarded crate's sources is found, and the copy is clean
/// before the plant (so the find is the plant's).
#[test]
fn a_raw_call_planted_in_a_scratch_copy_is_found() {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("no-raw-signals-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let src = crates_root().join("summoner-scan").join("src");
    let dst = scratch.join("summoner-scan").join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    for f in &files {
        let to = dst.join(f.strip_prefix(&src).unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(f, &to).unwrap();
    }
    assert_eq!(
        scan(&scratch, &["summoner-scan"]).1,
        Vec::<String>::new(),
        "the copy starts clean"
    );
    let lib = dst.join("lib.rs");
    let mut text = std::fs::read_to_string(&lib).unwrap();
    text.push_str("\nfn planted(pid: i32) {\n    let _ = unsafe { libc::kill(pid, 9) };\n}\n");
    std::fs::write(&lib, text).unwrap();
    let (_, found) = scan(&scratch, &["summoner-scan"]);
    let _ = std::fs::remove_dir_all(&scratch);
    assert_eq!(found.len(), 1, "the planted call: {found:?}");
    assert!(found[0].contains("libc::kill(pid, 9)"), "{found:?}");
}

/// The guard's own controls: what it must flag and what it must not.
#[test]
fn the_guard_flags_a_raw_kill_and_nothing_else() {
    assert_eq!(
        raw_calls("        runecraft_ipc::process::sigkill(c.pid);").len(),
        1
    );
    assert_eq!(raw_calls("    unsafe { libc::kill(pid, 9) };").len(), 1);
    assert_eq!(raw_calls("    signal_pid(pid, 15);").len(), 1);
    assert_eq!(raw_calls("    killpg(pgid, 9);").len(), 1);
    assert!(raw_calls("        c.process.kill();").is_empty());
    assert!(raw_calls("    let _ = leader.kill_group();").is_empty());
    assert!(raw_calls("    // a comment naming sigkill(c.pid) is not code").is_empty());
    assert!(raw_calls("    sigkill_self();").is_empty());
    assert!(raw_calls("    my_sigkill(x);").is_empty());
}
