//! dyld's list of the images loaded into this process (`_dyld_image_count`, `_dyld_get_image_name`): what
//! [`crate::bundle::LoadCheck`] compares before and after a bundle's load, and what the tests read to prove no
//! non-lab bundle was loaded.
use std::ffi::{CStr, c_char};

unsafe extern "C" {
    fn _dyld_image_count() -> u32;
    fn _dyld_get_image_name(index: u32) -> *const c_char;
}

/// The paths of every loaded image, in dyld's order. An index that another thread's unload emptied is skipped.
pub fn images() -> Vec<String> {
    // SAFETY: no arguments; dyld's own count, read at once.
    let n = unsafe { _dyld_image_count() };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        // SAFETY: any index may be passed: past the (possibly shrunk) end dyld returns null, handled below.
        let p = unsafe { _dyld_get_image_name(i) };
        if p.is_null() {
            continue;
        }
        // SAFETY: a non-null name is a NUL-terminated string owned by dyld for as long as the image is loaded; it is
        // copied out at once.
        out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_list_holds_this_test_binary_and_libsystem() {
        let imgs = super::images();
        let me = std::env::current_exe().unwrap();
        assert!(
            imgs.iter().any(|i| std::path::Path::new(i) == me),
            "{imgs:?}"
        );
        assert!(
            imgs.iter().any(|i| i.starts_with("/usr/lib/libSystem")),
            "{imgs:?}"
        );
    }
}
