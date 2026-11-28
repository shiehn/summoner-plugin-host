//! The safe API over the host program's system calls: watch a child process exit through the kernel (kqueue
//! `NOTE_EXIT`, so a reused pid is never mistaken for it), read the calling thread's QoS class, and take a SHA-256.
//! Run it with `cargo run -p summoner-sys --example summoner_sys_quickstart`.
use std::process::Command;
use std::time::Duration;

use summoner_sys::exit_watch::ExitWatch;

fn main() {
    let mut child = Command::new("/bin/sleep")
        .arg("0.2")
        .spawn()
        .expect("spawn /bin/sleep");
    // Registered once on this pid: the watch fires at the exit, before the child is reaped.
    let watch = ExitWatch::new(child.id()).expect("the exit watch");
    let gone = watch.wait(Some(Duration::from_secs(10))).expect("the wait");
    assert!(gone, "the child exits within the timeout");
    child.wait().expect("reap the child");

    let (qos, priority) = summoner_sys::qos::current().expect("this thread's QoS class");
    println!("the child exited; this thread runs at {qos:?} (relative priority {priority})");

    let digest = summoner_sys::sha256::hex(b"abc");
    // The well-known test vector: 64 hex digits, starting `ba7816bf`.
    assert!(digest.len() == 64 && digest.starts_with("ba7816bf"));
    println!("sha256(\"abc\") = {digest}");
}

#[test]
fn the_example_runs() {
    main();
}
