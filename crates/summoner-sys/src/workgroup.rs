//! The audio workgroup (`os_workgroup`, libSystem, macOS 11+): the groundwork for the chain threads' workgroup join,
//! NOT wired yet. The design: the host turns the mach port the host application sends into a workgroup with
//! `os_workgroup_create_with_port(<name>, port)`, deallocates the port, keeps every workgroup for the process lifetime
//! and bumps a generation; each chain thread, every loop iteration, leaves its old workgroup and `os_workgroup_join`s
//! the current one when the generation moved, counting joined / fallbacks on the child page (a failed join is a
//! counted fallback, never a silent one). This child cannot wire it until the host application can hand over a raw
//! port; this module is what the wiring will call.
//!
//! The C surface is declared here from the SDK headers (`os/workgroup_object.h`, `os/workgroup_parallel.h`,
//! `os/workgroup_base.h`, `os/object.h`, `mach/mach_port.h`); nothing is compiled.

use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;

/// `__OS_WORKGROUP_JOIN_TOKEN_SIZE__` on LP64 (`os/workgroup_base.h`).
const JOIN_TOKEN_SIZE: usize = 36;
/// `__OS_WORKGROUP_ATTR_SIZE__` (`os/workgroup_base.h`).
const ATTR_SIZE: usize = 60;
/// `_OS_WORKGROUP_ATTR_SIG_DEFAULT_INIT`: what `OS_WORKGROUP_ATTR_INITIALIZER_DEFAULT` sets (the rest zeroed).
const ATTR_SIG_DEFAULT_INIT: u32 = 0x2FA8_63B4;

/// `os_workgroup_join_token_opaque_s`: `{ uint32_t sig; char opaque[36]; }`, 40 bytes, align 4. Written by
/// `os_workgroup_join`, read by `os_workgroup_leave`; plain data (C copies it as a struct), so moving it is a copy.
#[repr(C)]
struct RawToken {
    sig: u32,
    opaque: [u8; JOIN_TOKEN_SIZE],
}

/// `os_workgroup_attr_opaque_s`: `{ uint32_t sig; char opaque[60]; }`.
#[repr(C)]
struct RawAttr {
    sig: u32,
    opaque: [u8; ATTR_SIZE],
}

type OsWorkgroup = *mut c_void;

unsafe extern "C" {
    fn os_workgroup_create_with_port(name: *const c_char, port: u32) -> OsWorkgroup;
    fn os_workgroup_parallel_create(name: *const c_char, attr: *mut RawAttr) -> OsWorkgroup;
    fn os_workgroup_copy_port(wg: OsWorkgroup, port_out: *mut u32) -> i32;
    fn os_workgroup_join(wg: OsWorkgroup, token_out: *mut RawToken) -> i32;
    fn os_workgroup_leave(wg: OsWorkgroup, token: *mut RawToken);
    fn os_workgroup_cancel(wg: OsWorkgroup);
    fn os_workgroup_testcancel(wg: OsWorkgroup) -> bool;
    fn os_retain(object: *mut c_void) -> *mut c_void;
    fn os_release(object: *mut c_void);
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    /// `mach_task_self()` is a macro over this (`mach/mach_init.h`): this task's port, set before main.
    static mach_task_self_: u32;
}

/// One retained `os_workgroup_t`. Clone retains (`os_retain`, atomic, no allocation); Drop releases (`os_release`).
/// The design keeps every workgroup for the process lifetime; holding a `Workgroup` (or a [`JoinToken`], which retains
/// its own) keeps it for as long as it is held.
#[derive(Debug)]
pub struct Workgroup(NonNull<c_void>);

// SAFETY: (Send, Sync) an os_workgroup is a reference-counted libdispatch object whose API is thread-safe: retain /
// release are atomic, and join / leave / cancel / copy_port may be called from any thread (a join's token is what is
// per-thread, and `JoinToken` is `!Send`). No Rust-side state is shared.
unsafe impl Send for Workgroup {}
// SAFETY: as above; every method takes `&self` and only passes the retained pointer to the thread-safe C API.
unsafe impl Sync for Workgroup {}

impl Workgroup {
    /// `os_workgroup_create_with_port(name, port)`: the workgroup a send right names (the host application's device
    /// workgroup, copied to a port). `None`: the port names no workgroup (the lab's stand-in port), which the design
    /// counts as a fallback. Does NOT consume or deallocate `port`: the caller owns the send right
    /// still and must release it ([`deallocate_port`]) once this returns, either way.
    pub fn from_port(name: &CStr, port: u32) -> Option<Workgroup> {
        // SAFETY: `name` is a live NUL-terminated string for the call (the API copies it); `port` is a plain integer
        // the kernel validates (a dead or wrong-type name yields NULL, not a crash). The result is +1 retained
        // (OS_WORKGROUP_RETURNS_RETAINED), owned by the returned `Workgroup`.
        let wg = unsafe { os_workgroup_create_with_port(name.as_ptr(), port) };
        NonNull::new(wg).map(Workgroup)
    }

    /// `os_workgroup_parallel_create(name, &OS_WORKGROUP_ATTR_INITIALIZER_DEFAULT)`: a workgroup with no device, for
    /// tests (and a future in-child use). `None`: the OS refused.
    pub fn parallel(name: &CStr) -> Option<Workgroup> {
        let mut attr = RawAttr {
            sig: ATTR_SIG_DEFAULT_INIT,
            opaque: [0; ATTR_SIZE],
        };
        // SAFETY: `name` is live and NUL-terminated for the call; `attr` is a live, initialised attribute (the default
        // initializer's exact layout) the call only reads. The result is +1 retained, owned by the `Workgroup`.
        let wg = unsafe { os_workgroup_parallel_create(name.as_ptr(), &mut attr) };
        NonNull::new(wg).map(Workgroup)
    }

    /// `os_workgroup_copy_port`: a NEW send right naming this workgroup. The caller owns it and must send it (moving
    /// it) or release it with [`deallocate_port`]. `Err`: the errno-style value the call returned. Observed (macOS
    /// 26.2): a [`Workgroup::parallel`] one refuses with EINVAL (22); only shareable workgroups (a device's, or the
    /// private interval SPI's, which this crate does not use) copy to a port.
    pub fn copy_port(&self) -> Result<u32, i32> {
        let mut port: u32 = 0;
        // SAFETY: `self.0` is a live retained workgroup; `port` is a live, aligned `mach_port_t` (u32) the call
        // writes once and does not keep.
        let rc = unsafe { os_workgroup_copy_port(self.0.as_ptr(), &mut port) };
        if rc == 0 { Ok(port) } else { Err(rc) }
    }

    /// `os_workgroup_join` on the calling thread. `Err`: the value the call returned (EINVAL when the workgroup is
    /// cancelled, EALREADY when this thread is in a workgroup already): a counted fallback, never fatal.
    /// No allocation (the token lives inline; `os_retain` is an atomic increment), so it may run on a chain thread.
    pub fn join(&self) -> Result<JoinToken, i32> {
        let mut raw = RawToken {
            sig: 0,
            opaque: [0; JOIN_TOKEN_SIZE],
        };
        // SAFETY: `self.0` is a live retained workgroup; `raw` is a live token of the header's exact size and
        // alignment, which the call fills on success and does not keep.
        let rc = unsafe { os_workgroup_join(self.0.as_ptr(), &mut raw) };
        if rc != 0 {
            return Err(rc);
        }
        Ok(JoinToken {
            wg: self.clone(),
            raw,
            _not_send: PhantomData,
        })
    }

    /// `os_workgroup_cancel`: later joins fail; threads already joined stay joined until they leave.
    pub fn cancel(&self) {
        // SAFETY: `self.0` is a live retained workgroup; cancel takes no other argument and is idempotent.
        unsafe { os_workgroup_cancel(self.0.as_ptr()) }
    }

    /// `os_workgroup_testcancel`.
    pub fn is_cancelled(&self) -> bool {
        // SAFETY: `self.0` is a live retained workgroup; the call only reads its state.
        unsafe { os_workgroup_testcancel(self.0.as_ptr()) }
    }
}

impl Clone for Workgroup {
    fn clone(&self) -> Workgroup {
        // SAFETY: `self.0` is a live retained os_object; `os_retain` adds one reference and returns the same pointer,
        // owned by the clone (released by its Drop).
        let p = unsafe { os_retain(self.0.as_ptr()) };
        Workgroup(NonNull::new(p).unwrap_or(self.0))
    }
}

impl Drop for Workgroup {
    fn drop(&mut self) {
        // SAFETY: this `Workgroup` owns exactly one reference (from create or `os_retain`), released once here.
        unsafe { os_release(self.0.as_ptr()) }
    }
}

/// This thread's membership of a workgroup: leaves on drop (or [`JoinToken::leave`]). Retains its workgroup, so the
/// workgroup outlives the membership. `!Send` and `!Sync`: libdispatch requires the leave on the joining thread.
/// Joins do not nest: leave before joining another (the chain thread's "leave old, join current").
#[derive(Debug)]
pub struct JoinToken {
    wg: Workgroup,
    raw: RawToken,
    _not_send: PhantomData<*const ()>,
}

impl std::fmt::Debug for RawToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawToken").field("sig", &self.sig).finish()
    }
}

impl JoinToken {
    /// The workgroup this thread joined.
    pub fn workgroup(&self) -> &Workgroup {
        &self.wg
    }

    /// `os_workgroup_leave` now (the same as dropping the token).
    pub fn leave(self) {}
}

impl Drop for JoinToken {
    fn drop(&mut self) {
        // SAFETY: `self.raw` was filled by a successful `os_workgroup_join` of `self.wg` on this thread (the token is
        // `!Send`, so this Drop runs on the joining thread) and is left exactly once, here; `self.wg` is still
        // retained (dropped after this body).
        unsafe { os_workgroup_leave(self.wg.0.as_ptr(), &mut self.raw) }
    }
}

/// `mach_port_deallocate(mach_task_self(), port)`: releases one user reference of a send right this task owns (the
/// port [`Workgroup::copy_port`] returns, or the one the host application sends, after [`Workgroup::from_port`]).
/// `Err`: the `kern_return_t` (KERN_INVALID_NAME / KERN_INVALID_RIGHT for a name this task does not hold; nothing is
/// freed).
pub fn deallocate_port(port: u32) -> Result<(), i32> {
    // SAFETY: `mach_task_self_` is libSystem's task-port global (a u32), set before main and never written after; the
    // kernel validates `port` against this task's name space, so a wrong name is an error code, not memory unsafety.
    // (Releasing a name another part of this process still uses is a logic error, which the caller owns.)
    let rc = unsafe { mach_port_deallocate(mach_task_self_, port) };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layouts match the SDK's LP64 structs.
    #[test]
    fn layouts_match_the_sdk() {
        assert_eq!(std::mem::size_of::<RawToken>(), 40);
        assert_eq!(std::mem::align_of::<RawToken>(), 4);
        assert_eq!(std::mem::size_of::<RawAttr>(), 64);
    }
}
