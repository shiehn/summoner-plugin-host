//! The mach channel thread: workgroup ports arrive here as mach messages carrying the `workgroup.attach` token.
//!
//! Joining a workgroup needs `os_workgroup_create_with_port` / `os_workgroup_join`, which the pinned FFI crate does
//! not wrap. In the lab the port is the supervisor's stand-in (no device), which cannot be made into a workgroup
//! either; so each chain thread counts as a time-constraint-only fallback (a counted defect) and `joined` is false.
//! The real join is still open.
use crate::host::Shared;
use crate::wire;
use runecraft_ipc::bridge::MachReceive;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use summoner_proto::codec::{self, kind};
use summoner_proto::layout::P_WG_FALLBACKS;

pub fn channel_thread(rx: Arc<MachReceive>, sh: Arc<Shared>) {
    loop {
        match rx.recv(1000) {
            Ok(m) => {
                let ok = m.port.is_some();
                if ok && let Some(p) = sh.page() {
                    let n = sh.chain_count() as u64;
                    p.word64(P_WG_FALLBACKS)
                        .fetch_add(n.max(1), Ordering::Relaxed);
                }
                let r = codec::WorkgroupAttached::new(m.token, ok, false);
                wire::event(&sh.w, kind::WORKGROUP_ATTACHED, &r, &[]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}
