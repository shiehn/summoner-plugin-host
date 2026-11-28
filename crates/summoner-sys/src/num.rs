//! The child's strict number parsing: `strictFloat` is the C library's `strtof` with a full-consumption check, so this
//! calls `strtof` itself (the system libc, the "C" locale: the child never calls `setlocale`). What that
//! means, and the table that pins it, is in `summoner-testproc`.
use std::ffi::CString;

/// `strictFloat`: empty is refused; otherwise the bytes up to the first NUL
/// (a `std::string`'s `c_str()`) go to `strtof`, and the value is accepted when `strtof` stopped at that end. So
/// leading whitespace, hex floats, `inf` / `nan` (any case) are accepted, overflow is ±inf and underflow is 0 or
/// subnormal (`ERANGE` is not checked), and trailing bytes are refused.
pub fn strtof_strict(s: &[u8]) -> Option<f32> {
    if s.is_empty() {
        return None;
    }
    let c = s.iter().position(|b| *b == 0).map_or(s, |n| &s[..n]);
    let cs = CString::new(c).expect("no NUL by construction");
    let mut end: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `cs` is a NUL-terminated string that outlives the call; `end` is a live out-parameter that strtof sets
    // to a position inside `cs` (at most its terminator).
    let v = unsafe { libc::strtof(cs.as_ptr(), &mut end) };
    // SAFETY: strtof's end pointer is inside `cs` or at its terminator, so the distance is in 0..=len.
    let used = unsafe { end.offset_from(cs.as_ptr()) } as usize;
    (used == c.len()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::strtof_strict as f;

    #[test]
    fn strtof_as_the_oracle_child_reads_it() {
        assert_eq!(f(b"0.5"), Some(0.5));
        assert_eq!(f(b" \t0.5"), Some(0.5), "leading whitespace");
        assert_eq!(f(b"0x1p-2"), Some(0.25), "hex float");
        assert_eq!(f(b"1e40"), Some(f32::INFINITY), "overflow");
        assert_eq!(f(b"1e-50"), Some(0.0), "underflow");
        assert!(f(b"NaN").unwrap().is_nan());
        assert_eq!(f(b"0.5 "), None, "trailing");
        assert_eq!(f(b""), None);
        assert_eq!(f(b" "), None);
        assert_eq!(f(b"0.5\0junk"), Some(0.5), "c_str stops at the NUL");
        assert_eq!(
            f(b"\0"),
            Some(0.0),
            "an empty c_str: strtof stops at once, at the end"
        );
    }
}
