//! A process's exit, from the kernel: kqueue `EVFILT_PROC` / `NOTE_EXIT`, registered once. The note fires at exit,
//! before the process is reaped, so a parent left as a zombie still counts as gone; and a pid registered once can never
//! be confused with a later process that reuses it.
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

/// One process's exit watch: a kqueue with that pid's `NOTE_EXIT` registered.
pub struct ExitWatch {
    kq: OwnedFd,
    /// The pid did not exist when the watch was registered (`ESRCH`).
    gone: bool,
}

fn kevent_proc(pid: u32, flags: u16) -> libc::kevent {
    libc::kevent {
        ident: pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    }
}

impl ExitWatch {
    /// Registers the watch on `pid`. A pid that is already gone gives a watch whose [`ExitWatch::wait`] returns true
    /// at once.
    pub fn new(pid: u32) -> io::Result<ExitWatch> {
        // SAFETY: kqueue takes no arguments; a non-negative return is a new descriptor this process owns.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just returned by kqueue and is owned by nothing else.
        let kq = unsafe { OwnedFd::from_raw_fd(fd) };
        let change = kevent_proc(pid, libc::EV_ADD | libc::EV_ONESHOT);
        // SAFETY: one change record that lives across the call, no event list (nevents 0, null pointer), no timeout
        // (null: a registration does not wait).
        let r = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(ExitWatch { kq, gone: true });
            }
            return Err(e);
        }
        Ok(ExitWatch { kq, gone: false })
    }

    /// Waits up to `timeout` (forever with `None`); true once the process has exited.
    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        if self.gone {
            return Ok(true);
        }
        let ts = timeout.map(|d| libc::timespec {
            tv_sec: d.as_secs() as libc::time_t,
            tv_nsec: d.subsec_nanos() as libc::c_long,
        });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        let mut ev = kevent_proc(0, 0);
        loop {
            // SAFETY: no changes (null, 0); one writable event record that outlives the call; tsp is null or points
            // at `ts`, which lives to the end of this function.
            let n =
                unsafe { libc::kevent(self.kq.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, tsp) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(n > 0 && ev.filter == libc::EVFILT_PROC && ev.fflags & libc::NOTE_EXIT != 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_running_process_is_not_gone_and_an_exited_one_is_seen_at_once() {
        let mut c = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let w = ExitWatch::new(c.id()).unwrap();
        assert!(!w.wait(Some(Duration::from_millis(20))).unwrap());
        let t0 = Instant::now();
        c.kill().unwrap(); // our own child, by its recorded handle
        assert!(w.wait(Some(Duration::from_secs(5))).unwrap());
        let dt = t0.elapsed();
        c.wait().unwrap();
        assert!(dt < Duration::from_millis(100), "{dt:?}");
        // Reaped and gone: a new watch says so without waiting.
        assert!(
            ExitWatch::new(c.id())
                .unwrap()
                .wait(Some(Duration::ZERO))
                .unwrap()
        );
    }
}
