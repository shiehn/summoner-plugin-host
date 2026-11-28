//! The workgroup surface without a device: a parallel workgroup joined from two threads at once, left and rejoined on
//! one thread (a nested join refused), a join on a cancelled workgroup failing with an error code, not a crash, and
//! the port path's edges. A parallel workgroup does NOT copy to a port (EINVAL, pinned below), so `from_port`'s
//! happy path needs a shareable workgroup the public API cannot make without a device; it is tested here only on the
//! null port (no workgroup: the counted fallback).
#![cfg(target_os = "macos")]

use std::sync::{Arc, Barrier};
use summoner_sys::workgroup::{Workgroup, deallocate_port};

fn parallel() -> Workgroup {
    Workgroup::parallel(c"summoner-test").expect("parallel workgroup")
}

#[test]
fn joined_by_two_threads_at_once() {
    let wg = Arc::new(parallel());
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = (0..2)
        .map(|_| {
            let (wg, barrier) = (Arc::clone(&wg), Arc::clone(&barrier));
            std::thread::spawn(move || {
                let token = wg.join().expect("join");
                barrier.wait(); // both threads are members at the same time
                token.leave();
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

#[test]
fn leave_then_rejoin_on_one_thread() {
    let wg = parallel();
    std::thread::spawn(move || {
        let first = wg.join().expect("first join");
        // Joins do not nest: a second join while a member fails with a code, not fatal.
        let nested = wg.join().map(drop).expect_err("a nested join must fail");
        eprintln!("nested join: {nested}");
        first.leave();
        let second = wg.join().expect("rejoin after leave");
        drop(second); // leaves on drop
        wg.join().expect("third join").leave();
    })
    .join()
    .unwrap();
}

#[test]
fn join_on_a_cancelled_workgroup_fails() {
    let wg = parallel();
    assert!(!wg.is_cancelled());
    wg.cancel();
    assert!(wg.is_cancelled());
    let rc = std::thread::spawn(move || wg.join().map(drop))
        .join()
        .unwrap();
    let err = rc.expect_err("join on a cancelled workgroup must fail");
    assert_ne!(err, 0);
    eprintln!("join on cancelled: {err}");
}

#[test]
fn a_member_stays_joined_through_a_cancel_and_leaves_cleanly() {
    let wg = parallel();
    std::thread::spawn(move || {
        let token = wg.join().expect("join");
        wg.cancel();
        token.leave();
        assert!(wg.join().is_err());
    })
    .join()
    .unwrap();
}

#[test]
fn a_parallel_workgroup_does_not_copy_to_a_port() {
    let err = parallel()
        .copy_port()
        .expect_err("a parallel workgroup is not shareable");
    assert_eq!(err, libc::EINVAL);
}

#[test]
fn from_the_null_port_is_none() {
    assert!(Workgroup::from_port(c"summoner-host", 0).is_none());
}

#[test]
fn deallocate_a_name_not_held_is_an_error() {
    assert!(deallocate_port(0x7fff_fff3).is_err());
}
