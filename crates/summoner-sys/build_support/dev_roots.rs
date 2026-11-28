//! The one validator of `SUMMONER_DEV_PLUGIN_ROOTS`, the build-time dev plugin roots (the fence of
//! `src/bundle.rs`). `build.rs` calls [`dev_roots`] and panics on a refusal, so a malformed value fails the build;
//! only a value it accepted reaches the crate (as `SUMMONER_DEV_FENCE`). `tests/dev_roots.rs` runs it on every shape.
//!
//! Accepted: exactly one plain absolute path, valid UTF-8, with no empty, `.` or `..` segment and no trailing `/`.
//! Unset is "no dev roots" (a release build). Everything else is refused: an empty value, a relative path, a
//! malformed segment, more than one entry (`:` separates entries; one fence is all this build supports), non-UTF-8.

use std::ffi::OsStr;

/// The variable's name: the only place it is spelled for the build.
pub const VAR: &str = "SUMMONER_DEV_PLUGIN_ROOTS";

/// `value` is the variable as the build saw it (`None` when unset). `Ok(None)`: no dev roots. `Ok(Some(fence))`: the
/// one accepted fence. `Err`: why the build must fail.
pub fn dev_roots(value: Option<&OsStr>) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(v) = value.to_str() else {
        return Err(format!("{VAR} is not valid UTF-8: {value:?}"));
    };
    if v.contains(':') {
        return Err(format!(
            "{VAR} holds more than one entry ({v:?}); this build supports exactly one dev plugin root"
        ));
    }
    if malformed(v) {
        return Err(format!(
            "{VAR} must be one plain absolute path (no empty, `.` or `..` segment, no trailing /): {v:?}"
        ));
    }
    Ok(Some(v.to_string()))
}

/// Not one plain absolute path.
#[cfg(not(mutant_dev_roots_malformed_accepted))]
fn malformed(v: &str) -> bool {
    match v.strip_prefix('/') {
        None => true,
        Some(rest) => rest
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == ".."),
    }
}

/// The mutant (`--cfg mutant_dev_roots_malformed_accepted`): a relative path or a `..` segment is taken.
/// `tests/dev_roots.rs`'s `a_malformed_dev_roots_value_fails_the_build` must turn red.
#[cfg(mutant_dev_roots_malformed_accepted)]
fn malformed(v: &str) -> bool {
    v.is_empty()
}
