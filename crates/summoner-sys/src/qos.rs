//! The calling thread's QoS class, READ ONLY (`pthread_get_qos_class_np`, read-only, with a SAFETY contract and a
//! test). Setting a class is runecraft-rt's (`set_thread_qos_utility`, for the scan child); this only reads it back,
//! so the scan child can say what it runs at and a test can check it.

/// A thread's QoS class (`qos_class_t`, `sys/qos.h`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Qos {
    /// `QOS_CLASS_USER_INTERACTIVE` (0x21).
    UserInteractive,
    /// `QOS_CLASS_USER_INITIATED` (0x19).
    UserInitiated,
    /// `QOS_CLASS_DEFAULT` (0x15).
    Default,
    /// `QOS_CLASS_UTILITY` (0x11).
    Utility,
    /// `QOS_CLASS_BACKGROUND` (0x09).
    Background,
    /// `QOS_CLASS_UNSPECIFIED` (0x00).
    Unspecified,
    /// A value outside `sys/qos.h`'s list (never seen; kept, never guessed).
    Other(u32),
}

impl Qos {
    fn from_raw(v: u32) -> Qos {
        match v {
            0x21 => Qos::UserInteractive,
            0x19 => Qos::UserInitiated,
            0x15 => Qos::Default,
            0x11 => Qos::Utility,
            0x09 => Qos::Background,
            0x00 => Qos::Unspecified,
            v => Qos::Other(v),
        }
    }
}

/// The calling thread's QoS class and its relative priority. `Err`: the errno `pthread_get_qos_class_np` returned.
pub fn current() -> std::io::Result<(Qos, i32)> {
    // Plain u32 storage, never libc's `qos_class_t` enum: the kernel writes the class as a 32-bit value, and reading a
    // value outside the enum's variants through the enum would be undefined behaviour.
    let mut raw: u32 = 0;
    let mut relative: libc::c_int = 0;
    // SAFETY: `pthread_self()` is this thread's own live handle (it takes no arguments and cannot fail). The call
    // writes one 32-bit `qos_class_t` through its second argument, which points at `raw` (a live, aligned `u32` of
    // that size: `qos_class_t` is `repr(u32)`), and one `int` through its third (`relative`, a live `c_int`); it reads
    // nothing else and keeps neither pointer. It returns 0 or an errno value, read as such.
    let rc = unsafe {
        libc::pthread_get_qos_class_np(
            libc::pthread_self(),
            (&mut raw as *mut u32).cast::<libc::qos_class_t>(),
            &mut relative,
        )
    };
    if rc == 0 {
        Ok((Qos::from_raw(raw), relative))
    } else {
        Err(std::io::Error::from_raw_os_error(rc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read-back on a fresh thread, before and after the class is set (set here through the same libc call
    /// the host application's `set_thread_qos_utility` makes, test-only): utility after, on that thread only.
    #[test]
    fn current_reads_back_a_class_set_on_this_thread_only() {
        let before_main = current().expect("read the test thread's class");
        let t = std::thread::spawn(|| {
            let before = current().expect("read before");
            // SAFETY: applies to the calling thread only; no pointers; returns 0 or an errno value.
            let rc = unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0)
            };
            assert_eq!(rc, 0, "set utility");
            (before, current().expect("read after"))
        });
        let (before, after) = t.join().unwrap();
        assert_ne!(
            before.0,
            Qos::Utility,
            "the control: a fresh thread is not at utility"
        );
        assert_eq!(after, (Qos::Utility, 0), "read back what was set");
        assert_eq!(
            current().unwrap(),
            before_main,
            "this thread's class unchanged"
        );
    }

    #[test]
    fn every_known_value_maps_and_an_unknown_one_is_kept() {
        assert_eq!(Qos::from_raw(0x11), Qos::Utility);
        assert_eq!(Qos::from_raw(0x00), Qos::Unspecified);
        assert_eq!(Qos::from_raw(0x42), Qos::Other(0x42));
    }
}
