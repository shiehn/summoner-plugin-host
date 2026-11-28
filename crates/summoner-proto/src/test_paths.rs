//! Test paths that are absolute on every OS. The tests write bundle paths in their unix form (`/x/a.vst3`); on unix
//! these helpers return them unchanged, so every unix byte (and every golden) is exactly what it was. On Windows,
//! where a leading `/` is not absolute, each path is rooted at `C:` (`C:/x/a.vst3`, which `Path::is_absolute`
//! accepts and JSON writes with no escape), and a frame is compared in its unix form.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

/// The roots the tests write their paths under.
const ROOTS: [&str; 4] = ["/x/", "/a/", "/abs/", "/lab/"];

/// The Windows drive the tests' paths are rooted at.
#[cfg(windows)]
const DRIVE: &str = "C:";

/// A test path, absolute on this OS: unchanged on unix; on Windows a path under one of the test roots is rooted at
/// `C:`. Leaked, so it stands wherever a `&'static str` literal stood.
pub(crate) fn p(path: &'static str) -> &'static str {
    #[cfg(windows)]
    if ROOTS.iter().any(|r| path.starts_with(r)) {
        return Box::leak(format!("{DRIVE}{path}").into_boxed_str());
    }
    path
}

/// A JSON text naming test paths, with each quoted path under a test root made absolute on this OS (unchanged on
/// unix). Leaked, as [`p`].
pub(crate) fn j(text: &str) -> &'static str {
    #[cfg(windows)]
    {
        let mut s = text.to_string();
        for r in ROOTS {
            s = s.replace(&format!("\"{r}"), &format!("\"{DRIVE}{r}"));
        }
        Box::leak(s.into_boxed_str())
    }
    #[cfg(not(windows))]
    Box::leak(text.to_string().into_boxed_str())
}

/// A formatted JSON text or path naming test paths, made absolute on this OS as [`j`] and [`p`] do (unchanged on
/// unix), as an owned string where the test built one.
pub(crate) fn po(text: String) -> String {
    #[cfg(windows)]
    {
        if ROOTS.iter().any(|r| text.starts_with(r)) {
            return format!("{DRIVE}{text}");
        }
        j(&text).to_string()
    }
    #[cfg(not(windows))]
    text
}

/// A frame in its unix form, for a byte golden: unchanged on unix; on Windows the paths in its meta and in a text
/// blob are unrooted again
/// and the frame re-encoded (so its length fields are the unix ones). A frame the decoder refuses is returned as
/// it is (the golden then fails, as it should).
pub(crate) fn unix_frame(frame: &[u8]) -> Vec<u8> {
    #[cfg(windows)]
    {
        use crate::codec::{Direction, decode_header};
        use crate::layout::FRAME_HEADER;
        let Some(head) = frame.get(..FRAME_HEADER) else {
            return frame.to_vec();
        };
        let head: &[u8; FRAME_HEADER] = head.try_into().expect("the header's length");
        let Ok(h) = decode_header(head, Direction::ToChild)
            .or_else(|_| decode_header(head, Direction::ToHost))
        else {
            return frame.to_vec();
        };
        let (m, b) = (h.meta_len as usize, h.blob_len as usize);
        let Some(meta) = frame.get(FRAME_HEADER..FRAME_HEADER + m) else {
            return frame.to_vec();
        };
        // A header-only frame (a refused blob's) declares a blob it does not carry: keep what it carries.
        let blob = frame.get(FRAME_HEADER + m..).unwrap_or(&[]);
        let blob = &blob[..blob.len().min(b)];
        let whole = blob.len() == b;
        // The meta, and a text blob (the scan's JSON lines), unrooted; a binary blob is kept as it is.
        let unroot = |text: &str| {
            let mut t = text.to_string();
            for r in ROOTS {
                t = t.replace(&format!("\"{DRIVE}{r}"), &format!("\"{r}"));
            }
            t
        };
        let meta = unroot(&String::from_utf8_lossy(meta));
        let blob = match std::str::from_utf8(blob) {
            Ok(text) => unroot(text).into_bytes(),
            Err(_) => blob.to_vec(),
        };
        // Re-encoded with the unix meta and blob; a header-only frame keeps its declared blob length (the frame length
        // follows both).
        let blob_len = if whole { blob.len() } else { b };
        let mut out =
            crate::codec::header_bytes(h.kind, h.flags, h.id, meta.len() as u32, blob_len as u32)
                .to_vec();
        out.extend_from_slice(meta.as_bytes());
        out.extend_from_slice(&blob);
        out
    }
    #[cfg(not(windows))]
    frame.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_path_is_absolute_on_this_os_and_unchanged_on_unix() {
        for r in ROOTS {
            let path: &'static str = Box::leak(format!("{r}b.vst3").into_boxed_str());
            assert!(std::path::Path::new(p(path)).is_absolute(), "{}", p(path));
            if cfg!(unix) {
                assert_eq!(p(path), path);
            }
        }
        assert_eq!(
            p("rel/b.vst3"),
            "rel/b.vst3",
            "a relative path stays relative"
        );
        let text = r#"{"path":"/x/b.vst3","other":"/x"}"#;
        if cfg!(unix) {
            assert_eq!(j(text), text);
        } else {
            assert_eq!(j(text), r#"{"path":"C:/x/b.vst3","other":"/x"}"#);
        }
    }
}
