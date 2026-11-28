//! The one validator of `SUMMONER_DEV_PLUGIN_ROOTS` (`build_support/dev_roots.rs`, which `build.rs` panics through):
//! unset is no dev roots, exactly one plain absolute path is the fence, and every other value fails the build.
//! Mutant: `mutant_dev_roots_malformed_accepted` turns `a_malformed_dev_roots_value_fails_the_build` red.
#[path = "../build_support/dev_roots.rs"]
mod dev_roots;

use dev_roots::dev_roots;
use std::ffi::OsStr;

#[test]
fn unset_is_no_dev_roots_and_one_plain_path_is_the_fence() {
    assert_eq!(dev_roots(None), Ok(None));
    for v in ["/a", "/dev/fence", "/a/b c/d"] {
        assert_eq!(
            dev_roots(Some(OsStr::new(v))),
            Ok(Some(v.to_string())),
            "{v}"
        );
    }
}

#[test]
fn a_malformed_dev_roots_value_fails_the_build() {
    for v in [
        "",
        "relative/root",
        "root",
        "./root",
        "/",
        "/a/",
        "/a//b",
        "/a/./b",
        "/a/../b",
        "/a/..",
        "/a:/b",
        ":/a",
    ] {
        let e = dev_roots(Some(OsStr::new(v))).expect_err(v);
        assert!(e.contains(dev_roots::VAR), "{v}: {e}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let e = dev_roots(Some(OsStr::from_bytes(b"/a/\xff"))).unwrap_err();
        assert!(e.contains("not valid UTF-8"), "{e}");
    }
}

/// The build ran the same validator: a test build is a dev build, so the fence it compiled in is the value it saw.
#[test]
fn the_compiled_in_fence_is_the_validated_value() {
    let fence = summoner_sys::bundle::dev_fence()
        .expect("the tests are built with SUMMONER_DEV_PLUGIN_ROOTS");
    assert_eq!(Some(fence), option_env!("SUMMONER_DEV_PLUGIN_ROOTS"));
    assert_eq!(
        dev_roots(Some(OsStr::new(fence))),
        Ok(Some(fence.to_string()))
    );
}
