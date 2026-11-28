//! The child page's ring-group hubs across the two sides: the host application's page and its objects, the child's
//! open of them, a waiter's endpoint for a chain thread, and a ring of the waiter's group reaching it. On unix the
//! page's hubs carry no object; on Windows one hub-set section and one event per waiter.
#![cfg(not(loom))]
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use runecraft_ipc::bridge::sync::Waited;
use std::time::{Duration, Instant};
use summoner_rt::ChildPage;

#[test]
fn a_ring_of_a_waiters_group_reaches_it() {
    let (host, page) = ChildPage::create().unwrap();
    let mut bells = host.bell_transferables().unwrap();
    assert_eq!(
        bells.len(),
        usize::from(cfg!(windows)),
        "unix: no object; Windows: the hub set's one section"
    );
    let child = ChildPage::open(page, &mut bells).unwrap();
    assert!(bells.is_empty(), "the open takes every object");

    // The host side's objects for slot 3, and the child's endpoints (every group) from them.
    let mut objs = host.hub_waiter_objects(3).unwrap();
    assert_eq!(
        objs.len(),
        usize::from(cfg!(windows)),
        "Windows: the slot's event"
    );
    let eps = child.receive_hub_endpoints(3, &mut objs).unwrap();
    assert!(objs.is_empty());
    assert_eq!(eps.len(), summoner_proto::layout::RING_GROUPS);
    assert!(eps.get(eps.len()).is_none(), "past the last group: none");
    let ep = eps.get(1).unwrap();

    let seen = child.hub(1).load(std::sync::atomic::Ordering::Acquire);
    std::thread::scope(|s| {
        let t = s.spawn(|| {
            let t0 = Instant::now();
            // a short wait first, across group 2's ring (spurious returns are allowed, so only the second wait's
            // wake is asserted; that another group's ring is not heard is the IPC floor's own Windows test)
            let _ = ep.wait(child.hub(1), seen, Some(t0 + Duration::from_millis(200)));
            ep.wait(
                child.hub(1),
                seen,
                Some(Instant::now() + Duration::from_secs(5)),
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        host.hub(2)
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        host.hub_waker(2).wake().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        host.hub(1)
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        host.hub_waker(1).wake().unwrap();
        assert_eq!(t.join().unwrap(), Waited::Woken);
    });
}

#[test]
fn a_slot_past_the_fan_out_bound_is_refused_on_both_sides() {
    let (host, page) = ChildPage::create().unwrap();
    let mut bells = host.bell_transferables().unwrap();
    let child = ChildPage::open(page, &mut bells).unwrap();
    let past = runecraft_ipc::bridge::FAN_SLOTS;
    assert!(host.hub_waiter_objects(past).is_err());
    assert_eq!(
        child
            .receive_hub_endpoints(past, &mut Vec::new())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}
