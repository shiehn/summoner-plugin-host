//! No crate of the child signals another process by its raw pid: a raw signal to a pid can reach a freed pid,
//! possibly another process's. The floor keeps its raw signals crate-private, so a call through it does not
//! compile; this source guard covers the rest (a crate's own `libc::kill`, `killpg`, or a helper of the same name)
//! in the `src` of every crate under `crates/`. The one allowed call is the crash path's kill of this process itself
//! (`libc::kill(libc::getpid(), …)`). Its control plants a raw call in a scratch copy and must find it.
use std::path::{Path, PathBuf};

const RAW: [&str; 5] = [
    "sigkill(",
    "sigterm(",
    "signal_pid(",
    "killpg(",
    "libc::kill(",
];

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd {
        let p = e.unwrap().path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// The raw signal calls in `text` outside comments, as (line number, line); a kill of this process itself is not one.
fn raw_calls(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let code = l.split("//").next().unwrap_or("");
            if code.contains("libc::kill(libc::getpid()") {
                return false;
            }
            RAW.iter().any(|r| {
                code.match_indices(r).any(|(i, _)| {
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

/// Every raw call under `crates_dir/<crate>/src`.
fn scan(crates_dir: &Path) -> (usize, Vec<String>) {
    let mut files = Vec::new();
    for e in std::fs::read_dir(crates_dir).unwrap() {
        rs_files(&e.unwrap().path().join("src"), &mut files);
    }
    let mut found = Vec::new();
    for f in &files {
        for (n, l) in raw_calls(&std::fs::read_to_string(f).unwrap()) {
            found.push(format!("{}:{n}: {l}", f.display()));
        }
    }
    (files.len(), found)
}

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[test]
fn no_crate_sends_a_raw_signal() {
    let (files, found) = scan(&crates_dir());
    assert!(files > 20, "the guard reads the sources ({files} files)");
    assert!(found.is_empty(), "raw signal calls:\n{}", found.join("\n"));
}

#[test]
fn a_raw_call_planted_in_a_scratch_copy_is_found() {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("no-raw-signals-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    // This crate's own sources (its directory name differs between this repo and an export of it).
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let dst = scratch.join("this").join("src");
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(&src).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "rs") {
            std::fs::copy(&p, dst.join(p.file_name().unwrap())).unwrap();
        }
    }
    assert!(scan(&scratch).1.is_empty(), "the copy starts clean");
    let lib = dst.join("lib.rs");
    let mut text = std::fs::read_to_string(&lib).unwrap();
    text.push_str("\nfn planted(pid: i32) {\n    let _ = unsafe { libc::kill(pid, 9) };\n}\n");
    std::fs::write(&lib, text).unwrap();
    let (_, found) = scan(&scratch);
    let _ = std::fs::remove_dir_all(&scratch);
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn the_guard_flags_a_raw_kill_and_nothing_else() {
    assert_eq!(raw_calls("    unsafe { libc::kill(pid, 9) };").len(), 1);
    assert_eq!(raw_calls("    killpg(pgid, 9);").len(), 1);
    assert!(raw_calls("        libc::kill(libc::getpid(), libc::SIGKILL);").is_empty());
    assert!(raw_calls("    // sigkill(pid) in a comment").is_empty());
    assert!(raw_calls("    child.kill();").is_empty());
}
