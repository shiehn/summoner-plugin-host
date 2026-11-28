//! On Apple targets std may box a `std::sync::Mutex`'s pthread mutex lazily, so the FIRST `try_lock` of a fresh mutex,
//! made on a realtime thread, allocates there. This measures it on this toolchain, under the allocation counter
//! (`target.noindex/plugins/libsummoner_lab_malloccount.dylib`, injected into a child run of this test binary), armed on a
//! second thread around exactly one `try_lock`:
//! - a mutex created on the main thread and first touched on the second thread (the chain thread's case);
//! - the same after one throwaway `try_lock` on the main thread (the prime);
//! - a second `try_lock` on the second thread (the control: steady state).

#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};

/// Allocations on a fresh thread during `f`, with the counter armed around it alone.
fn counted(f: impl FnOnce() + Send + 'static) -> u64 {
    std::thread::spawn(move || {
        assert!(summoner_sys::alloc_count::arm(), "the counter is injected");
        f();
        summoner_sys::alloc_count::disarm().unwrap()
    })
    .join()
    .unwrap()
}

#[test]
fn mutex_child() {
    if std::env::var_os("SUMMONER_MUTEX_CHILD").is_none() {
        return;
    }
    let fresh = Arc::new(Mutex::new(0u32));
    let f = fresh.clone();
    let first = counted(move || drop(f.try_lock()));
    let f = fresh.clone();
    let second = counted(move || drop(f.try_lock()));
    let primed = Arc::new(Mutex::new(0u32));
    drop(primed.try_lock()); // the prime, on this (the "main") thread
    let p = primed.clone();
    let after_prime = counted(move || drop(p.try_lock()));
    let rw = Arc::new(RwLock::new(0u32));
    let r = rw.clone();
    let rw_first = counted(move || drop(r.try_read()));
    // The spawn itself, as a baseline for the counted closure (nothing locked).
    let baseline = counted(|| {});
    println!(
        "MUTEX first={first} second={second} after_prime={after_prime} rwlock_first={rw_first} baseline={baseline}"
    );
}

#[test]
fn a_fresh_mutexs_first_try_lock_on_another_thread() {
    if lab_plugins::absent("a_fresh_mutexs_first_try_lock_on_another_thread") {
        return;
    }
    let counter = lab_plugins::dir().join("libsummoner_lab_malloccount.dylib");
    assert!(counter.is_file(), "tools/build-plugins.sh first");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "mutex_child", "--nocapture", "--test-threads=1"])
        .env("DYLD_INSERT_LIBRARIES", &counter)
        .env("SUMMONER_MUTEX_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.find("MUTEX ").map(|i| &l[i..]))
        .unwrap_or_else(|| panic!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr)));
    println!("{line}");
    let n = |k: &str| -> u64 {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(&format!("{k}=")))
            .unwrap()
            .parse()
            .unwrap()
    };
    // The steady state and the primed mutex allocate nothing on the second thread; the first touch is reported.
    assert_eq!(n("second"), 0, "{line}");
    assert_eq!(n("after_prime"), 0, "{line}");
    assert_eq!(n("baseline"), 0, "{line}");
}
