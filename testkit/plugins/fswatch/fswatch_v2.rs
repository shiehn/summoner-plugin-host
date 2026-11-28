//! The fswatch log's parser, v2 AND v3, each strict to its own version: the ONE reader of `summoner_lab_fswatch.c`'s
//! format (its header comment is the spec; v3: PRE / POST pairs, results, cwd tracking),
//! shared by every Rust consumer through `#[path]` (lab_gate, lab_vst3, the host Rig, the scan probe's tests), so a
//! format change is made in one place. It is FAIL-CLOSED ([`try_parse`]): it accepts only what the recorder writes,
//! and refuses everything else (an old v1 log, a v3 header, an unknown call or key, a bad escape, …), never reading
//! loosely, so a log can never pass with something unseen.
#![allow(dead_code)]

/// One record: the call, its detail's key=value pairs (unescaped), and its path(s) (unescaped). For v3, a PRE (the
/// attempt), with its POST (the result) attached when there is one, and the pid's tracked cwd at that attempt.
#[derive(Clone, Debug, PartialEq)]
pub struct Rec {
    pub call: String,
    pub detail: Vec<(String, String)>,
    pub paths: Vec<String>,
    /// The record as logged (for messages).
    pub raw: String,
    /// 2 or 3.
    pub version: u8,
    /// v3: the POST, if the call returned (a PRE without one is an attempt that never did).
    pub post: Option<Post>,
    /// v3: the process's tracked cwd when the attempt was made (`None`: unknown: `cwd=?`, a cut cwd, or a chdir /
    /// fchdir of the same pid in flight).
    pub cwd: Option<String>,
    /// v3: a chdir / fchdir of the same pid was in flight at this attempt (so its cwd is unknown).
    pub chdir_in_flight: bool,
}

/// A v3 POST: the call's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Post {
    pub ret: i64,
    pub errno: Option<String>,
    /// dlopen's resolved image (`?` kept as is).
    pub image: Option<String>,
    /// chdir / fchdir's new cwd (`?` kept as is).
    pub cwd: Option<String>,
    /// trunc=1: the image or the cwd was cut to fit (neither places anything then).
    pub trunc: bool,
    pub raw: String,
}

/// The calls the recorder writes, and whether each carries two paths.
const CALLS: [(&str, bool); 25] = [
    ("open", false),
    ("openat", false),
    ("creat", false),
    ("fopen", false),
    ("stat", false),
    ("lstat", false),
    ("fstatat", false),
    ("access", false),
    ("faccessat", false),
    ("opendir", false),
    ("dlopen", false),
    ("getattrlist", false),
    ("statfs", false),
    ("readlink", false),
    ("rename", true),
    ("renameat", true),
    ("unlink", false),
    ("unlinkat", false),
    ("mkdir", false),
    ("mkdirat", false),
    ("rmdir", false),
    ("truncate", false),
    ("ftruncate", false),
    ("symlink", true),
    ("link", true),
];

/// The detail keys the recorder writes.
const KEYS: [&str; 12] = [
    "flags",
    "mode",
    "fmode",
    "dirfd",
    "dirpath",
    "todirfd",
    "todirpath",
    "flag",
    "fd",
    "fdpath",
    "len",
    "trunc",
];

/// The recorder's escapes undone, STRICTLY: only what it writes is accepted. `\\`, `\t`, `\n`, `\r`, and `\xHH`
/// (two LOWERCASE hex digits, no sign) for a byte it escapes that way: 0x01..=0x1f other than TAB / LF / CR (those
/// have their short forms), 0x7f, and ',' (0x2c) inside a detail value only. A raw control byte, `\x00`, `\xff`,
/// `\x+f`, an unknown escape, or bytes that are not UTF-8 after unescaping are refused.
fn unescape_strict(s: &str, in_detail: bool) -> Result<String, String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    while i < b.len() {
        let c = b[i];
        if c < 0x20 || c == 0x7f {
            return Err(format!("a raw control byte 0x{c:02x} in {s:?}"));
        }
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        let e = *b
            .get(i + 1)
            .ok_or_else(|| format!("a dangling escape in {s:?}"))?;
        match e {
            b'\\' => out.push(b'\\'),
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'x' => {
                let (h, l) = match (
                    b.get(i + 2).copied().and_then(hex),
                    b.get(i + 3).copied().and_then(hex),
                ) {
                    (Some(h), Some(l)) => (h, l),
                    _ => {
                        return Err(format!(
                            "a \\x escape that is not two lowercase hex digits in {s:?}"
                        ));
                    }
                };
                let v = h * 16 + l;
                let ok = ((0x01..0x20).contains(&v) && !matches!(v, 0x09 | 0x0a | 0x0d))
                    || v == 0x7f
                    || (in_detail && v == 0x2c);
                if !ok {
                    return Err(format!(
                        "\\x{v:02x}, which the recorder never writes, in {s:?}"
                    ));
                }
                out.push(v);
                i += 2;
            }
            _ => return Err(format!("an unknown escape \\{} in {s:?}", e as char)),
        }
        i += 2;
    }
    String::from_utf8(out).map_err(|_| format!("not UTF-8 after unescaping: {s:?}"))
}

/// The recorder's escapes undone (a path's rules); panics on anything it never writes.
pub fn unescape(s: &str) -> String {
    unescape_strict(s, false).unwrap_or_else(|e| panic!("{e}"))
}

/// A version line: exactly `fswatch<TAB>v2<TAB>pid=<digits>`.
fn is_header(l: &str) -> bool {
    l.strip_prefix("fswatch\tv2\tpid=")
        .is_some_and(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}

fn record_v2(l: &str) -> Result<Rec, String> {
    let f: Vec<&str> = l.split('\t').collect();
    let two = CALLS
        .iter()
        .find(|(c, _)| *c == f[0])
        .map(|(_, two)| *two)
        .ok_or_else(|| format!("an unknown call: {l}"))?;
    let want = if two { 4 } else { 3 };
    if f.len() != want {
        return Err(format!("{} fields where {} has {want}: {l}", f.len(), f[0]));
    }
    let mut detail: Vec<(String, String)> = Vec::new();
    if f[1] != "-" {
        for kv in f[1].split(',') {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("not key=value: {kv} in {l}"))?;
            if !KEYS.contains(&k) {
                return Err(format!("an unknown detail key {k}: {l}"));
            }
            if detail.iter().any(|(d, _)| d == k) {
                return Err(format!("a duplicate detail key {k}: {l}"));
            }
            if k == "trunc" && v != "1" {
                return Err(format!("trunc={v} (the recorder writes only trunc=1): {l}"));
            }
            detail.push((k.to_string(), unescape_strict(v, true)?));
        }
    }
    let get = |k: &str| detail.iter().find(|(d, _)| d == k).map(|(_, v)| v.as_str());
    match f[0] {
        "open" | "openat" => {
            let flags = get("flags").ok_or_else(|| format!("an open without its flags: {l}"))?;
            let hex = flags.strip_prefix("0x").filter(|h| {
                !h.is_empty()
                    && h.bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            });
            if hex.is_none() {
                return Err(format!("bad open flags {flags}: {l}"));
            }
        }
        "fopen" => {
            get("fmode").ok_or_else(|| format!("an fopen without its mode: {l}"))?;
        }
        "ftruncate" if f[2] != "-" => return Err(format!("an ftruncate with a path: {l}")),
        _ => {}
    }
    let paths = f[2..]
        .iter()
        .map(|p| unescape_strict(p, false))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Rec {
        call: f[0].to_string(),
        detail,
        paths,
        raw: l.to_string(),
        version: 2,
        post: None,
        cwd: None,
        chdir_in_flight: false,
    })
}

/// A v3 pair's key: (pid, tid, seq).
type Key = (u32, u64, u64);

/// The errno names v3 writes (the spec's fixed set) and their numbers on this OS (the SDK's `sys/errno.h`): any
/// other errno is `E<decimal>`, and a named one is never written by its number (`E2` for ENOENT is refused).
const ERRNOS: [(&str, u32); 41] = [
    ("EPERM", 1),
    ("ENOENT", 2),
    ("ESRCH", 3),
    ("EINTR", 4),
    ("EIO", 5),
    ("ENXIO", 6),
    ("E2BIG", 7),
    ("ENOEXEC", 8),
    ("EBADF", 9),
    ("ECHILD", 10),
    ("EDEADLK", 11),
    ("ENOMEM", 12),
    ("EACCES", 13),
    ("EFAULT", 14),
    ("EBUSY", 16),
    ("EEXIST", 17),
    ("EXDEV", 18),
    ("ENODEV", 19),
    ("ENOTDIR", 20),
    ("EISDIR", 21),
    ("EINVAL", 22),
    ("ENFILE", 23),
    ("EMFILE", 24),
    ("ENOTTY", 25),
    ("ETXTBSY", 26),
    ("EFBIG", 27),
    ("ENOSPC", 28),
    ("ESPIPE", 29),
    ("EROFS", 30),
    ("EMLINK", 31),
    ("EPIPE", 32),
    ("EAGAIN", 35),
    ("ELOOP", 62),
    ("ENAMETOOLONG", 63),
    ("ENOTEMPTY", 66),
    ("EDQUOT", 69),
    ("ESTALE", 70),
    ("ENOTSUP", 45),
    ("EOVERFLOW", 84),
    ("EILSEQ", 92),
    ("ECANCELED", 89),
];

/// The canonical decimal `%lld` writes: no sign unless negative, no leading zero, no `-0`.
fn is_decimal(s: &str, signed: bool) -> bool {
    let (neg, d) = match s.strip_prefix('-') {
        Some(d) if signed => (true, d),
        _ => (false, s),
    };
    !d.is_empty()
        && d.bytes().all(|c| c.is_ascii_digit())
        && (d.len() == 1 || !d.starts_with('0'))
        && !(neg && d == "0")
}

/// A canonical signed decimal in `[lo, hi]`.
fn int_in(s: &str, lo: i64, hi: i64) -> bool {
    is_decimal(s, lo < 0) && s.parse::<i64>().is_ok_and(|v| (lo..=hi).contains(&v))
}

/// `0x` + canonical lowercase hex (`0x%llx` of an unsigned int).
fn is_hex_u32(s: &str) -> bool {
    s.strip_prefix("0x").is_some_and(|h| {
        !h.is_empty()
            && h.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
            && (h.len() == 1 || !h.starts_with('0'))
            && u32::from_str_radix(h, 16).is_ok()
    })
}

/// `0%llo` of a mode_t: `0` + canonical octal (so mode 0 is `00`), at most 0177777.
fn is_mode(s: &str) -> bool {
    s.strip_prefix('0').is_some_and(|o| {
        !o.is_empty()
            && o.bytes().all(|c| matches!(c, b'0'..=b'7'))
            && (o.len() == 1 || !o.starts_with('0'))
            && u32::from_str_radix(o, 8).is_ok_and(|v| v <= 0o177777)
    })
}

/// A path value the recorder writes for a dirpath / todirpath / fdpath / cwd: F_GETPATH's or getcwd's absolute
/// result, or `?`.
fn is_abs_or_unknown(s: &str) -> bool {
    s == "?" || s.starts_with('/')
}

/// The pid / tid / seq head of every v3 record: pid 1..=i32::MAX, tid 0..=i64::MAX (`%lld` of the u64), seq
/// 1..=i64::MAX; then the record's own pairs (values unescaped).
fn v3_detail(f1: &str, l: &str) -> Result<(Key, Vec<(String, String)>), String> {
    let mut pairs = Vec::new();
    for kv in f1.split(',') {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("not key=value: {kv} in {l}"))?;
        if pairs.iter().any(|(d, _): &(String, String)| d == k) {
            return Err(format!("a duplicate detail key {k}: {l}"));
        }
        pairs.push((k.to_string(), unescape_strict(v, true)?));
    }
    let head: Vec<&str> = pairs.iter().take(3).map(|(k, _)| k.as_str()).collect();
    if head != ["pid", "tid", "seq"] {
        return Err(format!(
            "a v3 record whose detail does not begin pid, tid, seq: {l}"
        ));
    }
    let max = i64::MAX;
    if !int_in(&pairs[0].1, 1, i32::MAX as i64)
        || !int_in(&pairs[1].1, 0, max)
        || !int_in(&pairs[2].1, 1, max)
    {
        return Err(format!(
            "a pid / tid / seq out of its canonical range (pid 1..=2^31-1, tid 0..=2^63-1, seq >= 1): {l}"
        ));
    }
    let key = (
        pairs[0].1.parse().unwrap_or(0),
        pairs[1].1.parse().unwrap_or(0),
        pairs[2].1.parse().unwrap_or(0),
    );
    Ok((key, pairs.split_off(3)))
}

/// The calls v3 adds (one path field each: chdir's argument, fchdir's `-`).
const V3_CALLS: [&str; 2] = ["chdir", "fchdir"];
/// AT_FDCWD on this OS: the dirfd that has no dirpath.
const AT_FDCWD: &str = "-2";
/// O_CREAT: the open flag that brings a mode argument (and so a mode= key).
const O_CREAT: u32 = 0x200;

/// The exact keys a v3 PRE of `call` carries, in order, given the values already read (a dirpath only for a real
/// dirfd, an open's mode only with O_CREAT): the spec's table.
fn v3_expected_keys(call: &str, rest: &[(String, String)]) -> Vec<&'static str> {
    let get = |k: &str| rest.iter().find(|(d, _)| d == k).map(|(_, v)| v.as_str());
    let real = |k: &str| get(k).is_some_and(|fd| fd != AT_FDCWD);
    let creat = || {
        get("flags")
            .and_then(|f| f.strip_prefix("0x"))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .is_some_and(|f| f & O_CREAT != 0)
    };
    let mut k: Vec<&'static str> = Vec::new();
    let dirfd = |v: &mut Vec<&'static str>| {
        v.push("dirfd");
        if real("dirfd") {
            v.push("dirpath");
        }
    };
    match call {
        "open" => {
            k.push("flags");
            if creat() {
                k.push("mode");
            }
        }
        "openat" => {
            dirfd(&mut k);
            k.push("flags");
            if creat() {
                k.push("mode");
            }
        }
        "creat" | "mkdir" => k.push("mode"),
        "fopen" => k.push("fmode"),
        "fstatat" | "faccessat" => dirfd(&mut k),
        "unlinkat" => {
            dirfd(&mut k);
            k.push("flag");
        }
        "mkdirat" => {
            dirfd(&mut k);
            k.push("mode");
        }
        "renameat" => {
            dirfd(&mut k);
            k.push("todirfd");
            if real("todirfd") {
                k.push("todirpath");
            }
        }
        "truncate" => k.push("len"),
        "ftruncate" => k.extend(["fd", "fdpath", "len"]),
        "fchdir" => k.extend(["fd", "fdpath"]),
        _ => {}
    }
    k
}

/// One own key's value, as the recorder formats it.
fn v3_value_ok(k: &str, v: &str) -> bool {
    match k {
        "dirfd" | "todirfd" | "fd" => int_in(v, i32::MIN as i64, i32::MAX as i64),
        "dirpath" | "todirpath" | "fdpath" => is_abs_or_unknown(v),
        "flags" | "flag" => is_hex_u32(v),
        "mode" => is_mode(v),
        "len" => int_in(v, i64::MIN, i64::MAX),
        "fmode" => true,
        _ => false,
    }
}

/// A v3 PRE: the call, its key, and the record. Its own keys are EXACTLY the spec's set for the call, each value in
/// the recorder's form; a PRE marked trunc=1 (a field was cut: a value may be cut, later keys missing) need only keep
/// the keys' order, and is a STOP anyway.
fn record_v3_pre(l: &str) -> Result<(Key, Rec), String> {
    let f: Vec<&str> = l.split('\t').collect();
    let two = CALLS
        .iter()
        .find(|(c, _)| *c == f[0])
        .map(|(_, two)| *two)
        .or_else(|| V3_CALLS.contains(&f[0]).then_some(false))
        .ok_or_else(|| format!("an unknown call: {l}"))?;
    let want = if two { 4 } else { 3 };
    if f.len() != want {
        return Err(format!("{} fields where {} has {want}: {l}", f.len(), f[0]));
    }
    let (key, mut rest) = v3_detail(f[1], l)?;
    let trunc = match rest.last() {
        Some((k, v)) if k == "trunc" => {
            if v != "1" {
                return Err(format!("trunc={v} (the recorder writes only trunc=1): {l}"));
            }
            rest.pop();
            true
        }
        _ => false,
    };
    let keys: Vec<&str> = rest.iter().map(|(k, _)| k.as_str()).collect();
    let expected = v3_expected_keys(f[0], &rest);
    if trunc {
        let mut at = 0usize;
        for k in &keys {
            let i = expected[at..]
                .iter()
                .position(|e| e == k)
                .ok_or_else(|| format!("detail key {k} unknown or out of order: {l}"))?;
            at += i + 1;
        }
    } else {
        if keys != expected {
            return Err(format!(
                "a {} PRE's keys {keys:?}, where the spec's are exactly {expected:?}: {l}",
                f[0]
            ));
        }
        if let Some((k, v)) = rest.iter().find(|(k, v)| !v3_value_ok(k, v)) {
            return Err(format!("{k}={v} is not in the recorder's form: {l}"));
        }
    }
    if matches!(f[0], "ftruncate" | "fchdir") && f[2] != "-" {
        return Err(format!("an {} with a path: {l}", f[0]));
    }
    let paths = f[2..]
        .iter()
        .map(|p| unescape_strict(p, false))
        .collect::<Result<Vec<_>, _>>()?;
    let mut detail = vec![
        ("pid".to_string(), key.0.to_string()),
        ("tid".to_string(), key.1.to_string()),
        ("seq".to_string(), key.2.to_string()),
    ];
    detail.extend(rest);
    if trunc {
        detail.push(("trunc".to_string(), "1".to_string()));
    }
    Ok((
        key,
        Rec {
            call: f[0].to_string(),
            detail,
            paths,
            raw: l.to_string(),
            version: 3,
            post: None,
            cwd: None,
            chdir_in_flight: false,
        },
    ))
}

/// A v3 POST: its call, key and result. Its keys are EXACTLY: ret; errno iff the call failed (ret -1) and is not
/// dlopen; image iff a dlopen succeeded; cwd iff a chdir / fchdir succeeded; trunc=1 only after an image or a cwd
/// (the only POST values long enough to be cut). ret is -1 or more, and 0 or -1 for the pointer calls (fopen,
/// opendir, dlopen) and chdir / fchdir. An errno is one of the fixed names, or `E<decimal>` for any other number.
fn record_v3_post(l: &str) -> Result<(String, Key, Post), String> {
    let f: Vec<&str> = l.split('\t').collect();
    let call = f[0]
        .strip_suffix(".ret")
        .ok_or_else(|| format!("not a POST: {l}"))?;
    if !CALLS.iter().any(|(c, _)| *c == call) && !V3_CALLS.contains(&call) {
        return Err(format!("a POST of an unknown call: {l}"));
    }
    if f.len() != 3 || f[2] != "-" {
        return Err(format!("a POST is `<call>.ret<TAB><detail><TAB>-`: {l}"));
    }
    let (key, rest) = v3_detail(f[1], l)?;
    let get = |k: &str| rest.iter().find(|(d, _)| d == k).map(|(_, v)| v.clone());
    let ret_s = get("ret").ok_or_else(|| format!("a POST without its ret: {l}"))?;
    let binary = matches!(call, "fopen" | "opendir" | "dlopen" | "chdir" | "fchdir");
    let hi = if binary { 0 } else { i64::MAX };
    if !int_in(&ret_s, -1, hi) {
        return Err(format!(
            "ret={ret_s}, where {call} returns {}: {l}",
            if binary { "0 or -1" } else { "-1 or more" }
        ));
    }
    let ret: i64 = ret_s.parse().unwrap_or(-1);
    let mut expected: Vec<&str> = vec!["ret"];
    if ret == -1 && call != "dlopen" {
        expected.push("errno");
    }
    if ret == 0 && call == "dlopen" {
        expected.push("image");
    }
    if ret == 0 && matches!(call, "chdir" | "fchdir") {
        expected.push("cwd");
    }
    let mut keys: Vec<&str> = rest.iter().map(|(k, _)| k.as_str()).collect();
    let trunc = keys.last() == Some(&"trunc");
    if trunc {
        if get("trunc").as_deref() != Some("1") {
            return Err(format!("trunc is only ever trunc=1: {l}"));
        }
        keys.pop();
        if !keys.iter().any(|k| matches!(*k, "image" | "cwd")) {
            return Err(format!(
                "trunc=1 on a POST with no image or cwd to cut: {l}"
            ));
        }
    }
    if keys != expected {
        return Err(format!(
            "a {call} POST with ret={ret} has exactly the keys {expected:?}, not {keys:?} \
             (errno only on failure and never for dlopen; image / cwd only on success): {l}"
        ));
    }
    let errno = get("errno");
    if let Some(e) = &errno {
        let named = ERRNOS.iter().any(|(n, _)| n == e);
        let numbered = e.strip_prefix('E').is_some_and(|d| {
            int_in(d, 1, i32::MAX as i64) && !ERRNOS.iter().any(|(_, n)| d.parse::<u32>() == Ok(*n))
        });
        if !named && !numbered {
            return Err(format!(
                "errno={e}: not a fixed name, nor E<decimal> for a number that has none: {l}"
            ));
        }
    }
    let cwd = get("cwd");
    if let Some(c) = &cwd
        && !is_abs_or_unknown(c)
    {
        return Err(format!("cwd={c}: getcwd's result is absolute (or ?): {l}"));
    }
    let image = get("image");
    if image.as_deref() == Some("") {
        return Err(format!("an empty image=: {l}"));
    }
    Ok((
        call.to_string(),
        key,
        Post {
            ret,
            errno,
            image,
            cwd,
            trunc,
            raw: l.to_string(),
        },
    ))
}

/// A v3 header: `fswatch<TAB>v3<TAB>pid=<1..=2^31-1><TAB>cwd=<absolute, escaped | ?>`, or, for a cwd the recorder
/// had to cut, exactly `…<TAB>cwd=?<TAB>trunc=1`: (pid, cwd; `None` for `?`).
fn header_v3(l: &str) -> Option<(u32, Option<String>)> {
    let f: Vec<&str> = l.split('\t').collect();
    let cut = f.len() == 5 && f[3] == "cwd=?" && f[4] == "trunc=1";
    if !(f.len() == 4 || cut) || f[0] != "fswatch" || f[1] != "v3" {
        return None;
    }
    let pid = f[2]
        .strip_prefix("pid=")
        .filter(|p| int_in(p, 1, i32::MAX as i64))?
        .parse()
        .ok()?;
    let raw = f[3].strip_prefix("cwd=")?;
    // The header's cwd is escaped as a detail value is, ',' as `\x2c` (the recorder's fesc_as(.., 1), measured
    // in a cwd with ',' and TAB): a raw ',' is a form it never writes.
    if raw.contains(',') {
        return None;
    }
    let cwd = unescape_strict(raw, true).ok()?;
    if !is_abs_or_unknown(&cwd) {
        return None;
    }
    Some((pid, (cwd != "?").then_some(cwd)))
}

fn try_parse_v3(text: &str) -> Result<Vec<Rec>, String> {
    use std::collections::{HashMap, HashSet};
    let mut out: Vec<Rec> = Vec::new();
    // The open PREs, the keys used in each pid's current image, the chdir / fchdir PREs in flight, the tracked cwds.
    let mut open: HashMap<Key, usize> = HashMap::new();
    let mut used: HashSet<Key> = HashSet::new();
    let mut chdirs: HashMap<u32, HashSet<Key>> = HashMap::new();
    let mut cwd: HashMap<u32, Option<String>> = HashMap::new();
    for l in text.split_terminator('\n') {
        if l.starts_with("fswatch\t") {
            // Another process's start, or a pid's new image (fork + exec): its open PREs are settled as a reap
            // settles them (no POST will come: a later POST of their key is refused), its keys may be used again,
            // and its cwd is the header's.
            let (pid, c) = header_v3(l).ok_or_else(|| format!("not a v3 version line: {l:?}"))?;
            open.retain(|k, _| k.0 != pid);
            used.retain(|k| k.0 != pid);
            chdirs.remove(&pid);
            cwd.insert(pid, c);
            continue;
        }
        let first = l.split('\t').next().unwrap_or_default();
        if first.ends_with(".ret") {
            let (call, key, post) = record_v3_post(l)?;
            let i = open
                .remove(&key)
                .ok_or_else(|| format!("a POST with no open PRE of its key: {l}"))?;
            if out[i].call != call {
                return Err(format!(
                    "a POST of {call} for a PRE of {}: {l}",
                    out[i].call
                ));
            }
            if matches!(call.as_str(), "chdir" | "fchdir") {
                if let Some(s) = chdirs.get_mut(&key.0) {
                    s.remove(&key);
                }
                if post.ret == 0 {
                    // A cut or unknown cwd places nothing after it.
                    let c = post.cwd.clone().filter(|c| c != "?" && !post.trunc);
                    cwd.insert(key.0, c);
                }
            }
            out[i].post = Some(post);
        } else {
            let (key, mut r) = record_v3_pre(l)?;
            let c = cwd
                .get(&key.0)
                .ok_or_else(|| format!("a record of pid {} before its header: {l}", key.0))?;
            if !used.insert(key) {
                return Err(format!(
                    "a key used twice in one image (pid, tid, seq are unique until the pid's next header): {l}"
                ));
            }
            let in_flight = chdirs.get(&key.0).is_some_and(|s| !s.is_empty());
            r.chdir_in_flight = in_flight;
            r.cwd = if in_flight { None } else { c.clone() };
            if matches!(r.call.as_str(), "chdir" | "fchdir") {
                chdirs.entry(key.0).or_default().insert(key);
            }
            open.insert(key, out.len());
            out.push(r);
        }
    }
    Ok(out)
}

/// Every record of a v2 log, or why it is not one (FAIL-CLOSED, as the host application's lab_watch is): the FIRST
/// line must be a version line; any later `fswatch<TAB>…` line must be one too (another process appending: a v3 or a
/// malformed header is refused, never skipped); every record is a known call with exactly its field count, known and
/// unique detail keys, `trunc` only `=1`, an open's hex flags and an fopen's mode present, and only the escapes the
/// recorder writes; the text ends with a newline (an unterminated last line is refused).
pub fn try_parse(text: &str) -> Result<Vec<Rec>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if !text.ends_with('\n') {
        return Err("an unterminated last line".into());
    }
    let mut lines = text.split_terminator('\n');
    let first = lines.next().unwrap_or_default();
    if first.starts_with("fswatch\tv3\t") {
        if header_v3(first).is_none() {
            return Err(format!("a malformed v3 version line: {first:?}"));
        }
        return try_parse_v3(text);
    }
    if !is_header(first) {
        return Err(format!(
            "not an fswatch v2 log (the first line is not its version line): {first:?}"
        ));
    }
    let mut out = Vec::new();
    for l in lines {
        if l.starts_with("fswatch\t") {
            if !is_header(l) {
                return Err(format!("not a v2 version line: {l:?}"));
            }
            continue;
        }
        out.push(record_v2(l)?);
    }
    Ok(out)
}

/// An fswatch log's text for [`parse`]: `""` if there is no log (the recorder opens it at a process's first recorded
/// call, so a process that made none writes none), and a PANIC if its bytes are not UTF-8 (a cut that split a
/// character, say): an unreadable log is a STOP, never an empty one. Every consumer reads its log through this
/// (`read_to_string(..).unwrap_or_default()` turned an unreadable log into a clean empty one).
pub fn read_log(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b).unwrap_or_else(|e| {
            panic!(
                "{}: the fswatch log is not UTF-8 (unreadable: a STOP): {e}",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => panic!(
            "{}: the fswatch log cannot be read (a STOP): {e}",
            path.display()
        ),
    }
}

/// As [`try_parse`], panicking on a refusal (every consumer's use: a refused log fails its test loudly).
pub fn parse(text: &str) -> Vec<Rec> {
    try_parse(text).unwrap_or_else(|e| panic!("{e}:\n{text}"))
}

impl Rec {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.detail
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn truncated(&self) -> bool {
        self.get("trunc") == Some("1")
    }

    /// A call that may change the file system (the open flags O_WRONLY / O_RDWR / O_CREAT / O_TRUNC / O_APPEND, a
    /// writing fopen mode, or a write call).
    pub fn is_write(&self) -> bool {
        const W: u32 = 0x1 | 0x2 | 0x8 | 0x200 | 0x400;
        match self.call.as_str() {
            "open" | "openat" => self
                .get("flags")
                .and_then(|f| u32::from_str_radix(f.trim_start_matches("0x"), 16).ok())
                .is_none_or(|f| f & W != 0),
            "fopen" => self
                .get("fmode")
                .is_none_or(|m| m.contains(['w', 'a', '+'])),
            "creat" | "rename" | "renameat" | "unlink" | "unlinkat" | "mkdir" | "mkdirat"
            | "rmdir" | "truncate" | "ftruncate" | "symlink" | "link" => true,
            _ => false,
        }
    }

    /// A call that provably only READS (the CoreAudio exemption's sense): the stat family, access, opendir,
    /// getattrlist, statfs, readlink, and an open / fopen with no writing flag or mode. Not dlopen (a load runs the
    /// image's code), not chdir / fchdir, not anything else.
    pub fn is_provable_read(&self) -> bool {
        match self.call.as_str() {
            "stat" | "lstat" | "fstatat" | "access" | "faccessat" | "opendir" | "getattrlist"
            | "statfs" | "readlink" => true,
            "open" | "openat" | "fopen" => !self.is_write(),
            _ => false,
        }
    }

    /// The paths this record WRITES, as logged (symlink's first path is its target TEXT, not a location; ftruncate's
    /// is its fdpath).
    fn written_paths(&self) -> Vec<&str> {
        if !self.is_write() {
            return Vec::new();
        }
        if self.call == "ftruncate" {
            return self.get("fdpath").into_iter().collect();
        }
        let skip = usize::from(self.call == "symlink");
        self.paths.iter().skip(skip).map(String::as_str).collect()
    }

    /// The absolute location of each path, or `None` when any cannot be placed (the spec's placing rules, and v3's):
    /// - an absolute path is itself; a relative one is joined to its dirpath (todirpath for the second path of a
    ///   renameat) when the call used a real dirfd, else (v3) to the process's tracked cwd at the attempt (unknown
    ///   while one of its chdir / fchdir calls is in flight, or after a cut or unknown cwd);
    /// - ftruncate / fchdir are their fdpath;
    /// - a v3 dlopen is BOTH its path (if absolute) and the image its POST names (if absolute and whole); `None` if
    ///   neither;
    /// - a cut field (trunc=1), `?`, an empty path, or a relative path with nothing to join it to: `None`.
    pub fn locations(&self) -> Option<Vec<String>> {
        if self.truncated() {
            return None;
        }
        if matches!(self.call.as_str(), "ftruncate" | "fchdir") {
            return self
                .get("fdpath")
                .filter(|p| *p != "?")
                .map(|p| vec![p.to_string()]);
        }
        if self.call == "dlopen" {
            let mut out = Vec::new();
            if self.paths[0].starts_with('/') {
                out.push(self.paths[0].clone());
            }
            if let Some(img) = self
                .post
                .as_ref()
                .filter(|p| !p.trunc)
                .and_then(|p| p.image.as_deref())
                .filter(|i| i.starts_with('/'))
            {
                out.push(img.to_string());
            }
            return (!out.is_empty()).then_some(out);
        }
        let real_dirfd = |k: &str| self.get(k).is_some_and(|fd| fd != AT_FDCWD);
        let mut out = Vec::new();
        for (i, p) in self.paths.iter().enumerate() {
            // symlink's first path is the link's TARGET text, not a location (judged apart, in `stops`).
            if self.call == "symlink" && i == 0 {
                continue;
            }
            if p.starts_with('/') {
                out.push(p.clone());
                continue;
            }
            if p.is_empty() {
                return None;
            }
            let (fdk, dirk) = if i == 0 {
                ("dirfd", "dirpath")
            } else {
                ("todirfd", "todirpath")
            };
            let base = if real_dirfd(fdk) {
                self.get(dirk).map(str::to_string)
            } else if self.version == 3 {
                self.cwd.clone()
            } else {
                None
            };
            match base {
                Some(b) if b != "?" => out.push(format!("{b}/{p}")),
                _ => return None,
            }
        }
        Some(out)
    }

    /// v3: the call failed having reached no file (its POST's errno ENOENT or ENOTDIR).
    pub fn reached_no_file(&self) -> bool {
        self.post
            .as_ref()
            .and_then(|p| p.errno.as_deref())
            .is_some_and(|e| matches!(e, "ENOENT" | "ENOTDIR"))
    }
}

/// The writes of `recs` whose location cannot be resolved (each a STOP): v2's rule; v3's in full is [`stops`].
pub fn unresolved_writes(recs: &[Rec]) -> Vec<String> {
    recs.iter()
        .filter(|r| r.is_write() && r.locations().is_none() && !r.reached_no_file())
        .map(|r| r.raw.clone())
        .collect()
}

/// The real home (the dev fence's parent: `SUMMONER_DEV_PLUGIN_ROOTS`, read at build time, is `<real home>/<fence>`),
/// for the STOP list.
fn real_home() -> Option<String> {
    option_env!("SUMMONER_DEV_PLUGIN_ROOTS")
        .and_then(|r| r.rsplit_once('/'))
        .map(|(home, _)| home.to_string())
}

/// `p` with `.` and `..` applied and repeated `/` collapsed, lexically (`..` at the root stays at the root; a
/// relative path keeps its leading `..`s). No symlink is resolved: the recorder's cwd= is authoritative for those.
fn lexical(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else if !abs {
                    parts.push("..");
                }
            }
            c => parts.push(c),
        }
    }
    let j = parts.join("/");
    if abs { format!("/{j}") } else { j }
}

/// Whether `p` has a `..` component.
fn has_dotdot(p: &str) -> bool {
    p.split('/').any(|c| c == "..")
}

/// The non-ASCII characters whose canonical caseless form (Unicode's NFD(casefold(NFD(c))); derived with Python
/// 3.9's unicodedata, Unicode 13.0.0, 2026-10-31) is WHOLLY ASCII: the only ones that can make a name equal an
/// ASCII STOP-list name on APFS, which compares names case-insensitively as Unicode does (ſ is s, the ligature ﬅ is
/// st). A character whose form mixes ASCII and non-ASCII (é is e + U+0301) names a different file, so it stays.
const FOLD_TO_ASCII: [(char, &str); 13] = [
    ('\u{00df}', "ss"),
    ('\u{017f}', "s"),
    ('\u{037e}', ";"),
    ('\u{1e9e}', "ss"),
    ('\u{1fef}', "`"),
    ('\u{212a}', "k"),
    ('\u{fb00}', "ff"),
    ('\u{fb01}', "fi"),
    ('\u{fb02}', "fl"),
    ('\u{fb03}', "ffi"),
    ('\u{fb04}', "ffl"),
    ('\u{fb05}', "st"),
    ('\u{fb06}', "st"),
];

/// `s` case-folded for matching the ASCII STOP-list names as APFS would: ASCII lowered, [`FOLD_TO_ASCII`] applied,
/// every other character kept.
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match FOLD_TO_ASCII.iter().find(|(f, _)| *f == c) {
            Some((_, a)) => out.push_str(a),
            None => out.push(c.to_ascii_lowercase()),
        }
    }
    out
}

/// /System/Library/Components/CoreAudio.component: the one exemption (READS of it and everything under it; a dlopen
/// of exactly it or its own executable).
const COREAUDIO: &str = "/System/Library/Components/CoreAudio.component";
/// The two names a dlopen may have to be exempt (the exemption's sentence: byte for byte).
const COREAUDIO_DLOPEN: [&str; 2] = [
    COREAUDIO,
    "/System/Library/Components/CoreAudio.component/Contents/MacOS/CoreAudio",
];

/// Whether `path`, a location of `r`, is exempt from the /System/Library/Components clause, by the exemption's
/// sentence (both parsers): "Exempt: provable READS of /System/Library/Components/CoreAudio.component and anything
/// under it, and a dlopen of exactly /System/Library/Components/CoreAudio.component or
/// /System/Library/Components/CoreAudio.component/Contents/MacOS/CoreAudio (byte for byte; no case variant, no `..`;
/// for a v3 dlopen, both the path and image= must be one of those two). Any other dlopen under
/// /System/Library/Components, another file inside CoreAudio.component included, stops; so does any write."
/// A v3 dlopen with no image= (it failed, or its POST never came) or a cut one has no image that is one of the two,
/// so it is not exempt.
fn exempt(r: &Rec, path: &str) -> bool {
    exemption(r, path).is_some()
}

/// The exemption `path` (a location of `r`) has, by name, or `None`. Two later exemptions (seen in a real
/// recording), for the two EXACT names only (the sealed, read-only system volume): (2) a
/// dlopen of one with ret=-1 is exempt ("a failed load"); (3) a successful dlopen of one with image=? is exempt on
/// the path alone ("image unmatched": the recorder's RTLD_NOLOAD match did not find it). Every other dlopen keeps
/// both-must-match; a cut image=, a dlopen that never returned, and a name that is not one of the two still stop.
fn exemption(r: &Rec, path: &str) -> Option<&'static str> {
    if r.call == "dlopen" {
        let named = |p: &str| COREAUDIO_DLOPEN.contains(&p);
        if !named(&r.paths[0]) || !named(path) {
            return None;
        }
        if r.version == 2 {
            return Some("a CoreAudio dlopen (v2: by its path)");
        }
        let p = r.post.as_ref()?;
        return match (p.ret, p.trunc, p.image.as_deref()) {
            (-1, _, _) => Some("a CoreAudio dlopen: a failed load"),
            (0, false, Some("?")) => Some("a CoreAudio dlopen: image unmatched"),
            (0, false, Some(i)) if named(i) => Some("a CoreAudio dlopen"),
            _ => None,
        };
    }
    let read = r.is_provable_read()
        && !r.is_write()
        && lexical(path) == path
        && (path == COREAUDIO || path.starts_with(&format!("{COREAUDIO}/")));
    read.then_some("a CoreAudio read")
}

/// Every exemption `stops` granted, each with its kind, for the report ("listed"): one line per record with
/// at least one exempt location and no STOP of its own.
pub fn exemptions(recs: &[Rec]) -> Vec<String> {
    let mut out = Vec::new();
    for r in recs {
        let Some(locs) = r.locations() else { continue };
        if let Some(why) = locs.iter().find_map(|p| exemption(r, p))
            && stops(std::slice::from_ref(r)).is_empty()
        {
            out.push(format!("exempt, {why}: {}", r.raw));
        }
    }
    out
}

/// Why a location is on the STOP list, or `None` (the spec's rule (a), both parsers). APFS folds case (Unicode's
/// folding: [`fold`]), so every clause is matched folded, on the path as logged AND with its `.` / `..` applied, and
/// the first three as SUBSTRINGS:
/// - a plugin folder (any `Library/Audio/Plug-Ins`);
/// - /System/Library/Components, unless `exempt` (see [`exempt`]: the exemption's sentence);
/// - the REAL home's `Library/Preferences/com.*`;
/// - any WRITE under /System or /Library (or of either itself).
fn on_stop_list(path: &str, write: bool, exempt: bool) -> Option<&'static str> {
    let home = real_home().map(|h| format!("{}/library/preferences/com.", fold(&h)));
    for v in [fold(path), fold(&lexical(path))] {
        // SUBSTRING matches: the same folder by another spelling (/System/Volumes/Data/…, the
        // firmlink), a `link/../..` walk, any home's.
        if v.contains("library/audio/plug-ins") {
            return Some("a plugin folder");
        }
        if v.contains("/system/library/components") && !exempt {
            return Some("/System/Library/Components");
        }
        if let Some(h) = &home
            && v.contains(h.as_str())
        {
            return Some("the real home's Library/Preferences/com.*");
        }
        // Writes: by prefix (a substring "/library/" would be every home's Library, the lab's own included); the
        // firmlink spelling /System/Volumes/Data/Library/… is under /System, so it is caught.
        let system = |r: &str| v == r || v.starts_with(&format!("{r}/"));
        if write && (system("/system") || system("/library")) {
            return Some("a write under /System or /Library");
        }
    }
    None
}

/// Every STOP in `recs`, with its reason (the spec's consumer rules; the host application's lab_watch implements the
/// same). Both versions: (a) a STOP-list attempt, whatever its result (case-folded, with and without its `..`); a write
/// through `..`; a symlink whose TARGET text names a STOP-list path (judged as a write: a link into the list is a
/// way to write there). v3 also: a PRE marked trunc=1; a cut chdir / fchdir cwd= (it places nothing after it); a
/// relative image= on a dlopen POST; (b) an UNPLACEABLE WRITE at once, and an unplaceable READ unless its POST failed
/// with ENOENT / ENOTDIR (it reached no file), so also one that never returned (reaped, or settled by its pid's next
/// header). v2 has no results: its (b) is an unplaceable write.
pub fn stops(recs: &[Rec]) -> Vec<String> {
    let mut out = Vec::new();
    for r in recs {
        let w = r.is_write();
        let mut stop = |why: &str| out.push(format!("{why}: {}", r.raw));
        if r.version == 3 && r.truncated() {
            stop("trunc=1 on the attempt");
            continue;
        }
        if let Some(p) = &r.post {
            if p.trunc && p.cwd.is_some() {
                stop("a cut cwd= (it places nothing after it)");
            }
            if p.image
                .as_deref()
                .is_some_and(|i| i != "?" && !i.starts_with('/'))
            {
                stop("a relative image= on a dlopen");
            }
            if p.trunc && p.image.is_some() {
                stop("a cut image= (its lost tail could name a STOP-list image)");
            }
        }
        if r.written_paths().iter().any(|p| has_dotdot(p)) {
            stop("a write through ..");
        }
        let locs = r.locations();
        if r.call == "symlink" {
            let t = &r.paths[0];
            let mut texts = vec![t.clone()];
            if !t.starts_with('/')
                && let Some(link) = locs.as_ref().and_then(|l| l.first())
            {
                // A relative target is relative to the link's own directory.
                let dir = link.rsplit_once('/').map_or("", |(d, _)| d);
                texts.push(format!("{dir}/{t}"));
            }
            if let Some(why) = texts.iter().find_map(|t| on_stop_list(t, true, false)) {
                stop(&format!("a symlink whose target names {why}"));
            }
        }
        match locs {
            Some(locs) => {
                if let Some(why) = locs.iter().find_map(|p| on_stop_list(p, w, exempt(r, p))) {
                    stop(why);
                }
            }
            None => {
                // Not placeable: the paths as logged are judged against the list, then the attempt on its kind.
                let skip = usize::from(r.call == "symlink");
                if let Some(why) = r.paths[skip..]
                    .iter()
                    .find_map(|p| on_stop_list(p, w, false))
                {
                    stop(why);
                }
                let why = if w {
                    Some("an unplaceable write (at once, whatever its result)")
                } else if r.version == 2 || r.reached_no_file() {
                    None
                } else if r.post.is_none() {
                    Some("an unplaceable read with no POST (it never returned)")
                } else if r.chdir_in_flight {
                    Some("an unplaceable read (a chdir / fchdir of its pid was in flight)")
                } else {
                    Some("an unplaceable read that reached a file (or a failed bare dlopen)")
                };
                if let Some(why) = why {
                    stop(why);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v2_log_parses_unescapes_and_locates() {
        let log = "fswatch\tv2\tpid=7\n\
                   open\tflags=0x601,mode=0644\t/x/t\\tab\n\
                   openat\tdirfd=3,dirpath=/lab/d\\x2cir,flags=0x201,mode=0600\trel\n\
                   rename\t-\t/a\t/b\\nc\n\
                   stat\ttrunc=1\t/qqq\n\
                   ftruncate\tfd=4,fdpath=?,len=0\t-\n\
                   unlinkat\tdirfd=-2,flag=0x0\trelative\n\
                   stat\t-\trelative-read\n";
        let r = parse(log);
        assert_eq!(r.len(), 7);
        assert_eq!(r[0].paths, ["/x/t\tab"]);
        assert_eq!(r[0].get("flags"), Some("0x601"));
        assert!(r[0].is_write());
        assert_eq!(
            r[1].get("dirpath"),
            Some("/lab/d,ir"),
            "a comma inside a value stays"
        );
        assert_eq!(r[1].locations(), Some(vec!["/lab/d,ir/rel".to_string()]));
        assert_eq!(r[2].paths, ["/a", "/b\nc"]);
        assert!(r[3].truncated() && r[3].locations().is_none());
        assert!(r[4].locations().is_none(), "fdpath=?");
        assert!(r[5].locations().is_none(), "relative, no dirpath");
        assert_eq!(
            unresolved_writes(&r),
            [r[4].raw.clone(), r[5].raw.clone()],
            "the unresolved writes; a truncated READ and a relative read are not writes"
        );
        assert_eq!(unescape("a\\x01b\\\\c"), "a\u{1}b\\c");
    }

    #[test]
    #[should_panic(expected = "not an fswatch v2 log")]
    fn a_v1_log_is_refused() {
        parse("stat /Library/Audio/Plug-Ins/VST3\n");
    }

    /// The host application's lab_watch list: each form the recorder never writes is REFUSED, with the rule
    /// it broke named. One line each, inside an otherwise good log (so it is that line that is refused).
    #[test]
    fn everything_the_recorder_never_writes_is_refused() {
        let h = "fswatch\tv2\tpid=7\n";
        let cases: [(&str, String, &str); 16] = [
            (
                "an unknown call",
                format!("{h}chmod\t-\t/a\n"),
                "an unknown call",
            ),
            (
                "extra path fields",
                format!("{h}stat\t-\t/a\t/b\n"),
                "fields where stat has 3",
            ),
            (
                "a two-path call with one",
                format!("{h}rename\t-\t/a\n"),
                "fields where rename has 4",
            ),
            (
                "an unknown key",
                format!("{h}stat\tcolour=red\t/a\n"),
                "an unknown detail key",
            ),
            (
                "a duplicate key",
                format!("{h}open\tflags=0x0,flags=0x1\t/a\n"),
                "a duplicate detail key",
            ),
            ("trunc=0", format!("{h}stat\ttrunc=0\t/a\n"), "trunc=0"),
            (
                "a flagless open",
                format!("{h}open\t-\t/a\n"),
                "an open without its flags",
            ),
            (
                "a bad-flag open",
                format!("{h}openat\tdirfd=-2,flags=601\t/a\n"),
                "bad open flags",
            ),
            (
                "uppercase flags",
                format!("{h}open\tflags=0x60A\t/a\n"),
                "bad open flags",
            ),
            (
                "an fopen without its mode",
                format!("{h}fopen\t-\t/a\n"),
                "an fopen without its mode",
            ),
            (
                "\\x+f",
                format!("{h}stat\t-\t/a\\x+f\n"),
                "not two lowercase hex digits",
            ),
            ("\\xff", format!("{h}stat\t-\t/a\\xff\n"), "never writes"),
            ("\\x00", format!("{h}stat\t-\t/a\\x00\n"), "never writes"),
            (
                "a v3 header",
                format!("{h}fswatch\tv3\tpid=8\nstat\t-\t/a\n"),
                "not a v2 version line",
            ),
            (
                "a header not first",
                "stat\t-\t/a\nfswatch\tv2\tpid=7\n".to_string(),
                "the first line is not",
            ),
            (
                "an unterminated last line",
                format!("{h}stat\t-\t/a"),
                "an unterminated last line",
            ),
        ];
        for (what, log, why) in cases {
            match try_parse(&log) {
                Ok(r) => panic!("{what}: accepted as {r:?}"),
                Err(e) => assert!(
                    e.contains(why),
                    "{what}: refused, but for {e:?}, not {why:?}"
                ),
            }
        }
        // And the recorder's own forms still pass (the controls): an escaped ',' in a detail value (never in a path),
        // a later process's v2 header, a short escape.
        assert!(try_parse(&format!("{h}openat\tdirfd=3,dirpath=/a\\x2cb,flags=0x0\tx\nfswatch\tv2\tpid=9\nstat\t-\t/t\\tx\n")).is_ok());
        assert!(
            try_parse(&format!("{h}stat\t-\t/a\\x2cb\n")).is_err(),
            "\\x2c is a detail-value escape only"
        );
        assert!(
            try_parse(&format!("{h}stat\t-\t/a\\x09b\n")).is_err(),
            "TAB has its short form"
        );
    }

    /// The STOPs of a v3 log (it must parse).
    fn v3_stops(body: &str) -> Vec<String> {
        stops(&parse(&format!("fswatch\tv3\tpid=7\tcwd=/w\n{body}")))
    }

    fn refused(what: &str, log: &str, why: &str) {
        match try_parse(log) {
            Ok(r) => panic!("{what}: accepted as {r:?}"),
            Err(e) => assert!(
                e.contains(why),
                "{what}: refused, but for {e:?}, not {why:?}"
            ),
        }
    }

    /// v3: PRE / POST pairs by (pid, tid, seq), never by adjacency; cwd tracked per pid
    /// (the header's, then a successful chdir / fchdir POST's cwd=).
    #[test]
    fn a_v3_log_pairs_and_tracks_cwd() {
        let log = "fswatch\tv3\tpid=7\tcwd=/lab/w\n\
                   open\tpid=7,tid=1,seq=1,flags=0x0\t\n\
                   stat\tpid=7,tid=2,seq=1\trel-read\n\
                   open.ret\tpid=7,tid=1,seq=1,ret=-1,errno=ENOENT\t-\n\
                   stat.ret\tpid=7,tid=2,seq=1,ret=0\t-\n\
                   chdir\tpid=7,tid=1,seq=2\tsub\n\
                   chdir.ret\tpid=7,tid=1,seq=2,ret=0,cwd=/lab/w/sub\t-\n\
                   open\tpid=7,tid=1,seq=3,flags=0x0\tafter\n\
                   open.ret\tpid=7,tid=1,seq=3,ret=5\t-\n\
                   dlopen\tpid=7,tid=1,seq=4\tlibz.dylib\n\
                   dlopen.ret\tpid=7,tid=1,seq=4,ret=0,image=/usr/lib/libz.1.dylib\t-\n\
                   stat\tpid=7,tid=1,seq=5\t\n\
                   fswatch\tv3\tpid=8\tcwd=?\n\
                   stat\tpid=8,tid=9,seq=1\tx\n\
                   stat.ret\tpid=8,tid=9,seq=1,ret=0\t-\n";
        let r = try_parse(log).expect("a v3 log");
        assert_eq!(r.len(), 7, "PREs, each with its POST attached");
        assert!(r.iter().all(|x| x.version == 3));
        assert!(r[0].reached_no_file(), "the empty path: ENOENT");
        let at = |i: usize| r[i].locations();
        assert_eq!(
            at(1),
            Some(vec!["/lab/w/rel-read".to_string()]),
            "placed by the header's cwd"
        );
        assert_eq!(
            at(3),
            Some(vec!["/lab/w/sub/after".to_string()]),
            "placed after the chdir"
        );
        assert_eq!(
            at(4),
            Some(vec!["/usr/lib/libz.1.dylib".to_string()]),
            "the dlopen's image"
        );
        assert!(r[5].post.is_none(), "a PRE that never returned");
        assert_eq!(at(6), None, "pid 8's cwd is ?");
        let st = stops(&r);
        assert_eq!(
            st.len(),
            2,
            "the never-returned empty stat and pid 8's: {st:?}"
        );
        assert!(
            st[0].starts_with("an unplaceable read with no POST"),
            "{st:?}"
        );
        assert!(
            st[1].starts_with("an unplaceable read that reached a file"),
            "{st:?}"
        );
    }

    /// Rule 1: READS of CoreAudio.component AND everything under it are exempt; never a write, a chdir, a
    /// dlopen, a `..` path or a case variant.
    #[test]
    fn rule_1_the_coreaudio_exemption_is_reads_of_the_bundle_and_under_it() {
        let ca = "/System/Library/Components/CoreAudio.component";
        let clean = format!(
            "stat\tpid=7,tid=1,seq=1\t{ca}\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n\
             open\tpid=7,tid=1,seq=2,flags=0x0\t{ca}/Contents/Info.plist\nopen.ret\tpid=7,tid=1,seq=2,ret=3\t-\n\
             fopen\tpid=7,tid=1,seq=3,fmode=r\t{ca}/Contents/Resources/x\nfopen.ret\tpid=7,tid=1,seq=3,ret=0\t-\n\
             opendir\tpid=7,tid=1,seq=4\t{ca}/Contents\nopendir.ret\tpid=7,tid=1,seq=4,ret=0\t-\n"
        );
        assert_eq!(
            v3_stops(&clean),
            Vec::<String>::new(),
            "reads of the bundle and under it"
        );
        for (what, body) in [
            (
                "a write under it",
                format!("open\tpid=7,tid=1,seq=1,flags=0x1\t{ca}/x\n"),
            ),
            (
                "a write-mode fopen",
                format!("fopen\tpid=7,tid=1,seq=1,fmode=r+\t{ca}/x\n"),
            ),
            (
                "a chdir into it",
                format!("chdir\tpid=7,tid=1,seq=1\t{ca}\n"),
            ),
            (
                "a dlopen of another file inside it",
                format!("dlopen\tpid=7,tid=1,seq=1\t{ca}/Contents/Resources/x.dylib\n"),
            ),
            (
                "a case variant",
                "stat\tpid=7,tid=1,seq=1\t/System/Library/Components/coreaudio.component\n"
                    .to_string(),
            ),
            (
                "via ..",
                format!("stat\tpid=7,tid=1,seq=1\t{ca}/../Other.component\n"),
            ),
            (
                "a .. that stays inside",
                format!("stat\tpid=7,tid=1,seq=1\t{ca}/Contents/../Info\n"),
            ),
            (
                "a sibling",
                "stat\tpid=7,tid=1,seq=1\t/System/Library/Components/CoreAudio.componentX\n".into(),
            ),
            (
                "the folder itself",
                "opendir\tpid=7,tid=1,seq=1\t/System/Library/Components\n".into(),
            ),
        ] {
            let st = v3_stops(&body);
            assert!(
                st.iter().any(|s| s.contains("/System/Library/Components")),
                "{what}: {st:?}"
            );
        }
    }

    /// A path under a home directory (`/<home root>/<rest>`), the root assembled so this file names none.
    fn home(rest: &str) -> String {
        format!("/{}/{rest}", ["Use", "rs"].concat())
    }

    /// Rule 2: APFS folds case, and the list is matched on the path as logged AND with `..` applied.
    #[test]
    fn rule_2_the_stop_list_folds_case_and_applies_dotdot() {
        for (what, path) in [
            ("lower case", "/library/audio/plug-ins/X.vst3"),
            ("upper case", "/LIBRARY/AUDIO/PLUG-INS/VST3/X.vst3"),
            (
                "a home's, mixed",
                &home("a/Library/audio/Plug-ins/Components/X.component"),
            ),
            ("through ..", "/Library/Audio/Other/../Plug-Ins/X.vst3"),
            ("through . and //", "/Library//Audio/./Plug-Ins/X"),
            (
                "system components, folded",
                "/system/library/COMPONENTS/Foo.component",
            ),
        ] {
            let st = v3_stops(&format!(
                "stat\tpid=7,tid=1,seq=1\t{path}\nstat.ret\tpid=7,tid=1,seq=1,ret=-1,errno=ENOENT\t-\n"
            ));
            assert_eq!(st.len(), 1, "{what}: {st:?}");
        }
        let st = v3_stops("stat\tpid=7,tid=1,seq=1\t/Library/Audio/Plug-Ins/../Other\n");
        assert_eq!(
            st.len(),
            1,
            "the path AS LOGGED names the plugin folder, though `..` leaves it: {st:?}"
        );
    }

    /// Rule 3: an unplaceable WRITE stops at once, whatever its result; only a READ waits for its POST.
    #[test]
    fn rule_3_an_unplaceable_write_stops_at_once() {
        let st = v3_stops(
            "fswatch\tv3\tpid=8\tcwd=?\n\
             open\tpid=8,tid=1,seq=1,flags=0x601,mode=0644\trel\n\
             open.ret\tpid=8,tid=1,seq=1,ret=-1,errno=ENOENT\t-\n\
             mkdir\tpid=8,tid=1,seq=2,mode=0755\td\n",
        );
        assert_eq!(
            st.len(),
            2,
            "the ENOENT write and the unanswered one: {st:?}"
        );
        assert!(
            st.iter().all(|s| s.starts_with("an unplaceable write")),
            "{st:?}"
        );
        let st = v3_stops(
            "fswatch\tv3\tpid=8\tcwd=?\n\
             stat\tpid=8,tid=1,seq=1\trel\n\
             stat.ret\tpid=8,tid=1,seq=1,ret=-1,errno=ENOTDIR\t-\n",
        );
        assert!(
            st.is_empty(),
            "a read that reached no file is clean: {st:?}"
        );
    }

    /// Rule 4: while a chdir / fchdir of its pid is in flight, a relative call's cwd is UNKNOWN.
    #[test]
    fn rule_4_a_relative_call_during_a_chdir_has_no_cwd() {
        let r = parse(
            "fswatch\tv3\tpid=7\tcwd=/w\n\
             chdir\tpid=7,tid=1,seq=1\t/elsewhere\n\
             stat\tpid=7,tid=2,seq=1\trel\n\
             stat\tpid=7,tid=2,seq=2\t/abs\n\
             chdir.ret\tpid=7,tid=1,seq=1,ret=0,cwd=/elsewhere\t-\n\
             stat.ret\tpid=7,tid=2,seq=1,ret=0\t-\n\
             stat.ret\tpid=7,tid=2,seq=2,ret=0\t-\n\
             stat\tpid=7,tid=2,seq=3\trel2\n\
             stat.ret\tpid=7,tid=2,seq=3,ret=0\t-\n\
             fchdir\tpid=7,tid=1,seq=2,fd=4,fdpath=/x\t-\n\
             stat\tpid=7,tid=2,seq=4\trel3\n\
             stat.ret\tpid=7,tid=2,seq=4,ret=-1,errno=ENOENT\t-\n",
        );
        assert!(
            r[1].chdir_in_flight && r[1].locations().is_none(),
            "unknown while the chdir is in flight"
        );
        assert_eq!(
            r[2].locations(),
            Some(vec!["/abs".to_string()]),
            "an absolute path needs no cwd"
        );
        assert_eq!(
            r[3].locations(),
            Some(vec!["/elsewhere/rel2".to_string()]),
            "placed after its POST"
        );
        assert!(
            r[5].chdir_in_flight,
            "an fchdir that never returned keeps it unknown"
        );
        let st = stops(&r);
        assert_eq!(
            st.len(),
            1,
            "the read that reached a file meanwhile; the ENOENT one is clean: {st:?}"
        );
        assert!(
            st[0].contains("in flight") && st[0].ends_with("\trel"),
            "{st:?}"
        );
    }

    /// Rule 5: a write through `..`; a symlink whose TARGET names a STOP-list path; a relative image=; a cut
    /// cwd= (which places nothing after it).
    #[test]
    fn rule_5_dotdot_writes_symlink_targets_relative_images_and_cut_cwds_stop() {
        let st = v3_stops("open\tpid=7,tid=1,seq=1,flags=0x601,mode=0644\t/w/a/../b\n");
        assert!(
            st.iter().any(|s| s.starts_with("a write through ..")),
            "{st:?}"
        );
        assert!(
            v3_stops("stat\tpid=7,tid=1,seq=1\t/w/a/../b\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n")
                .is_empty(),
            "a READ through .. elsewhere is fine"
        );
        for t in [
            "/Library/Audio/Plug-Ins/VST3",
            "../../../Library/Audio/Plug-Ins",
            "/System/Library/Components/X",
        ] {
            let st = v3_stops(&format!("symlink\tpid=7,tid=1,seq=1\t{t}\t/w/a/b/link\n"));
            assert!(
                st.iter()
                    .any(|s| s.starts_with("a symlink whose target names")),
                "{t}: {st:?}"
            );
        }
        assert!(v3_stops("symlink\tpid=7,tid=1,seq=1\t../elsewhere\t/w/link\nsymlink.ret\tpid=7,tid=1,seq=1,ret=0\t-\n").is_empty());
        let st = v3_stops(
            "dlopen\tpid=7,tid=1,seq=1\tlibx.dylib\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=lib/libx.dylib\t-\n",
        );
        assert!(
            st.iter().any(|s| s.starts_with("a relative image=")),
            "{st:?}"
        );
        let st = v3_stops(
            "chdir\tpid=7,tid=1,seq=1\t/deep\n\
             chdir.ret\tpid=7,tid=1,seq=1,ret=0,cwd=/deep/cut,trunc=1\t-\n\
             stat\tpid=7,tid=1,seq=2\trel\n\
             stat.ret\tpid=7,tid=1,seq=2,ret=0\t-\n",
        );
        assert!(st[0].starts_with("a cut cwd="), "{st:?}");
        assert!(
            st[1].starts_with("an unplaceable read"),
            "the cut cwd placed nothing: {st:?}"
        );
    }

    /// Rule 6: an absolute dlopen is judged on BOTH its path and its image=.
    #[test]
    fn rule_6_an_absolute_dlopen_is_judged_on_its_path_and_its_image() {
        let st = v3_stops(
            "dlopen\tpid=7,tid=1,seq=1\t/w/lib.dylib\n\
             dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=/Library/Audio/Plug-Ins/x/lib.dylib\t-\n",
        );
        assert_eq!(st.len(), 1, "by its image: {st:?}");
        let st = v3_stops(
            "dlopen\tpid=7,tid=1,seq=1\t/Library/Audio/Plug-Ins/x/lib.dylib\n\
             dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=/w/lib.dylib\t-\n",
        );
        assert_eq!(st.len(), 1, "by its path: {st:?}");
        let r = parse(
            "fswatch\tv3\tpid=7\tcwd=/w\n\
             dlopen\tpid=7,tid=1,seq=1\t/a/lib.dylib\n\
             dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=/b/lib.dylib\t-\n",
        );
        assert_eq!(
            r[0].locations(),
            Some(vec!["/a/lib.dylib".to_string(), "/b/lib.dylib".to_string()])
        );
        assert!(stops(&r).is_empty());
    }

    /// Rule 7: a pid's new header (an exec) settles its open PREs like a reap; a key may recur after it, and
    /// no POST pairs across it.
    #[test]
    fn rule_7_a_new_header_settles_the_pid_and_frees_its_keys() {
        let log = "fswatch\tv3\tpid=7\tcwd=?\n\
                   stat\tpid=7,tid=1,seq=1\trel\n\
                   stat\tpid=7,tid=1,seq=2\t/abs\n\
                   fswatch\tv3\tpid=7\tcwd=/new\n\
                   stat\tpid=7,tid=1,seq=1\trel\n\
                   stat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n";
        let r = parse(log);
        assert_eq!(r.len(), 3);
        assert!(
            r[0].post.is_none() && r[1].post.is_none(),
            "settled: no POST will come"
        );
        assert_eq!(
            r[2].locations(),
            Some(vec!["/new/rel".to_string()]),
            "the new image's cwd"
        );
        let st = stops(&r);
        assert_eq!(
            st.len(),
            1,
            "the settled unplaceable read STOPS (the settled /abs is clean): {st:?}"
        );
        assert!(
            st[0].starts_with("an unplaceable read with no POST") && st[0].ends_with("\trel"),
            "{st:?}"
        );
        refused(
            "a POST across a new header",
            "fswatch\tv3\tpid=7\tcwd=/w\nstat\tpid=7,tid=1,seq=1\t/a\nfswatch\tv3\tpid=7\tcwd=/w\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n",
            "no open PRE",
        );
        refused(
            "a key reused within one image",
            "fswatch\tv3\tpid=7\tcwd=/w\nstat\tpid=7,tid=1,seq=1\t/a\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\nstat\tpid=7,tid=1,seq=1\t/b\n",
            "a key used twice",
        );
    }

    /// Rule 8: the grammar, fail-closed to the spec.
    #[test]
    fn rule_8_everything_v3_never_writes_is_refused() {
        let h = "fswatch\tv3\tpid=7\tcwd=/w\n";
        let pre = "stat\tpid=7,tid=1,seq=1\t/a\n";
        let post = |p: &str| format!("{h}{pre}stat.ret\tpid=7,tid=1,seq=1,{p}\t-\n");
        let cases: Vec<(&str, String, &str)> = vec![
            (
                "a POST with no PRE",
                format!("{h}stat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"),
                "no open PRE",
            ),
            (
                "a record before its pid's header",
                format!("{h}stat\tpid=9,tid=1,seq=1\t/a\n"),
                "before its header",
            ),
            (
                "errno on success",
                post("ret=0,errno=ENOENT"),
                "has exactly the keys",
            ),
            (
                "a failure without errno",
                post("ret=-1"),
                "has exactly the keys",
            ),
            (
                "an unknown errno name",
                post("ret=-1,errno=EBOGUS"),
                "not a fixed name",
            ),
            (
                "a named errno by number",
                post("ret=-1,errno=E2"),
                "not a fixed name",
            ),
            ("errno E0", post("ret=-1,errno=E0"), "not a fixed name"),
            (
                "errno E0106",
                post("ret=-1,errno=E0106"),
                "not a fixed name",
            ),
            (
                "image= off a dlopen",
                post("ret=0,image=/x"),
                "has exactly the keys",
            ),
            (
                "cwd= off a chdir",
                post("ret=0,cwd=/x"),
                "has exactly the keys",
            ),
            (
                "a dlopen's errno",
                format!(
                    "{h}dlopen\tpid=7,tid=1,seq=1\t/a\ndlopen.ret\tpid=7,tid=1,seq=1,ret=-1,errno=ENOENT\t-\n"
                ),
                "has exactly the keys",
            ),
            (
                "a successful dlopen with no image",
                format!(
                    "{h}dlopen\tpid=7,tid=1,seq=1\t/a\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"
                ),
                "has exactly the keys",
            ),
            (
                "a successful chdir with no cwd",
                format!("{h}chdir\tpid=7,tid=1,seq=1\t/a\nchdir.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"),
                "has exactly the keys",
            ),
            (
                "a relative cwd=",
                format!(
                    "{h}chdir\tpid=7,tid=1,seq=1\t/a\nchdir.ret\tpid=7,tid=1,seq=1,ret=0,cwd=a\t-\n"
                ),
                "absolute",
            ),
            (
                "a pointer call's ret 5",
                format!(
                    "{h}opendir\tpid=7,tid=1,seq=1\t/a\nopendir.ret\tpid=7,tid=1,seq=1,ret=5\t-\n"
                ),
                "0 or -1",
            ),
            (
                "a chdir's ret 1",
                format!("{h}chdir\tpid=7,tid=1,seq=1\t/a\nchdir.ret\tpid=7,tid=1,seq=1,ret=1\t-\n"),
                "0 or -1",
            ),
            ("ret -2", post("ret=-2,errno=EIO"), "-1 or more"),
            ("ret -0", post("ret=-0"), "-1 or more"),
            ("ret 01", post("ret=01"), "-1 or more"),
            (
                "trunc=1 with nothing to cut",
                post("ret=0,trunc=1"),
                "no image or cwd",
            ),
            (
                "keys out of order",
                format!("{h}openat\tpid=7,tid=1,seq=1,flags=0x0,dirfd=-2\t/a\n"),
                "exactly",
            ),
            (
                "dirpath with AT_FDCWD",
                format!("{h}fstatat\tpid=7,tid=1,seq=1,dirfd=-2,dirpath=/x\t/a\n"),
                "exactly",
            ),
            (
                "no dirpath with a real dirfd",
                format!("{h}fstatat\tpid=7,tid=1,seq=1,dirfd=3\ta\n"),
                "exactly",
            ),
            (
                "an open's mode without O_CREAT",
                format!("{h}open\tpid=7,tid=1,seq=1,flags=0x1,mode=0644\t/a\n"),
                "exactly",
            ),
            (
                "O_CREAT without its mode",
                format!("{h}open\tpid=7,tid=1,seq=1,flags=0x201\t/a\n"),
                "exactly",
            ),
            (
                "a stat with an own key",
                format!("{h}stat\tpid=7,tid=1,seq=1,mode=0644\t/a\n"),
                "exactly",
            ),
            (
                "flags 0X1",
                format!("{h}open\tpid=7,tid=1,seq=1,flags=0X1\t/a\n"),
                "recorder's form",
            ),
            (
                "flags 0x01",
                format!("{h}open\tpid=7,tid=1,seq=1,flags=0x01\t/a\n"),
                "recorder's form",
            ),
            (
                "mode 644",
                format!("{h}mkdir\tpid=7,tid=1,seq=1,mode=644\t/a\n"),
                "recorder's form",
            ),
            (
                "mode 0",
                format!("{h}mkdir\tpid=7,tid=1,seq=1,mode=0\t/a\n"),
                "recorder's form",
            ),
            (
                "mode 0800",
                format!("{h}mkdir\tpid=7,tid=1,seq=1,mode=0800\t/a\n"),
                "recorder's form",
            ),
            (
                "dirfd 03",
                format!("{h}fstatat\tpid=7,tid=1,seq=1,dirfd=03,dirpath=/x\ta\n"),
                "recorder's form",
            ),
            (
                "a relative dirpath",
                format!("{h}fstatat\tpid=7,tid=1,seq=1,dirfd=3,dirpath=x\ta\n"),
                "recorder's form",
            ),
            (
                "len +5",
                format!("{h}truncate\tpid=7,tid=1,seq=1,len=+5\t/a\n"),
                "recorder's form",
            ),
            (
                "seq 0",
                format!("{h}stat\tpid=7,tid=1,seq=0\t/a\n"),
                "canonical range",
            ),
            (
                "pid 0",
                "fswatch\tv3\tpid=0\tcwd=/w\n".into(),
                "malformed v3",
            ),
            (
                "pid 07",
                format!("{h}stat\tpid=07,tid=1,seq=1\t/a\n"),
                "canonical range",
            ),
            (
                "tid past i64",
                format!("{h}stat\tpid=7,tid=9223372036854775808,seq=1\t/a\n"),
                "canonical range",
            ),
            (
                "a relative header cwd",
                "fswatch\tv3\tpid=7\tcwd=w\n".into(),
                "malformed v3",
            ),
            (
                "an empty header cwd",
                "fswatch\tv3\tpid=7\tcwd=\n".into(),
                "malformed v3",
            ),
            (
                "no pid first",
                format!("{h}stat\ttid=1,pid=7,seq=1\t/a\n"),
                "begin pid, tid, seq",
            ),
            (
                "two open PREs with one key",
                format!("{h}{pre}stat\tpid=7,tid=1,seq=1\t/b\n"),
                "a key used twice",
            ),
            (
                "a POST of another call",
                format!("{h}{pre}lstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"),
                "a POST of lstat",
            ),
            (
                "trunc=2",
                format!("{h}stat\tpid=7,tid=1,seq=1,trunc=2\t/a\n"),
                "only trunc=1",
            ),
            (
                "a v2 header in a v3 log",
                format!("{h}fswatch\tv2\tpid=8\n"),
                "not a v3 version line",
            ),
            (
                "a v3 key in a v2 log",
                "fswatch\tv2\tpid=7\nstat\tpid=7\t/a\n".to_string(),
                "an unknown detail key",
            ),
        ];
        for (what, log, why) in cases {
            refused(what, &log, why);
        }
        // What the recorder does write is accepted: E<decimal> for an unnamed number, mode 00, a cut PRE.
        parse(&post("ret=-1,errno=E106"));
        parse(&format!("{h}mkdir\tpid=7,tid=1,seq=1,mode=00\t/a\n"));
        parse(&format!(
            "{h}openat\tpid=7,tid=1,seq=1,dirfd=3,dirpath=/cu,trunc=1\t/a\n"
        ));
    }

    /// The exemption's sentence: a dlopen of EXACTLY the bundle or its own executable is exempt, with (v3) its
    /// image= one of the two as well; every other dlopen under /System/Library/Components stops. The accepted case is
    /// one seen in a real recording (every CoreAudio client loads it at start-up).
    #[test]
    fn the_coreaudio_dlopens_by_mgmts_sentence() {
        let ca = "/System/Library/Components/CoreAudio.component";
        let exe = format!("{ca}/Contents/MacOS/CoreAudio");
        let dl = |path: &str, post: &str| {
            let body = format!("dlopen\tpid=7,tid=1,seq=1\t{path}\n{post}");
            v3_stops(&body)
        };
        let ok = |img: &str| format!("dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image={img}\t-\n");
        // Accepted: sitting 2's line, and the bundle path resolving to its executable; v2 on the path alone.
        assert!(
            dl(&exe, &ok(&exe)).is_empty(),
            "sitting 2: the executable, its image the executable"
        );
        assert!(
            dl(ca, &ok(&exe)).is_empty(),
            "the bundle path, its image the executable"
        );
        let v2 = parse(&format!("fswatch\tv2\tpid=7\ndlopen\t-\t{exe}\n"));
        assert!(stops(&v2).is_empty(), "v2: the path alone");
        // Refused.
        for (what, path, post) in [
            (
                "another file inside the bundle",
                format!("{ca}/Contents/Resources/x.dylib"),
                ok(&format!("{ca}/Contents/Resources/x.dylib")),
            ),
            (
                "the executable, its image elsewhere under Components",
                exe.clone(),
                ok("/System/Library/Components/Other.component/Contents/MacOS/Other"),
            ),
            (
                "the executable, its image a lab file",
                exe.clone(),
                ok("/w/fake"),
            ),
            (
                "a bare name resolving to the executable",
                "CoreAudio".to_string(),
                ok(&exe),
            ),
            (
                "a dlopen of the executable that never returned",
                exe.clone(),
                String::new(),
            ),
            (
                "a case variant",
                format!("{ca}/Contents/MacOS/coreaudio"),
                ok(&format!("{ca}/Contents/MacOS/coreaudio")),
            ),
            (
                "a .. variant",
                format!("{ca}/Contents/MacOS/../MacOS/CoreAudio"),
                ok(&exe),
            ),
            (
                "another component",
                "/System/Library/Components/Other.component".to_string(),
                ok("/System/Library/Components/Other.component/Contents/MacOS/Other"),
            ),
        ] {
            let st = dl(&path, &post);
            assert!(
                st.iter()
                    .any(|s| s.starts_with("/System/Library/Components")),
                "{what}: {st:?}"
            );
        }
        // A cut image= is no image that is one of the two.
        let st = dl(
            &exe,
            &format!(
                "dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image={ca}/Contents/MacOS/Core,trunc=1\t-\n"
            ),
        );
        assert!(
            st.iter()
                .any(|s| s.starts_with("/System/Library/Components")),
            "a cut image: {st:?}"
        );
    }

    /// The recorder never writes a cut cwd as if whole: a header whose cwd was cut is exactly
    /// `cwd=?<TAB>trunc=1`, and nothing is placed against it.
    #[test]
    fn a_cut_header_cwd_is_cwd_unknown_and_nothing_else_is_accepted() {
        let r = parse(
            "fswatch\tv3\tpid=7\tcwd=?\ttrunc=1\n\
             stat\tpid=7,tid=1,seq=1\trel\n\
             stat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n\
             stat\tpid=7,tid=1,seq=2\t/abs\n\
             stat.ret\tpid=7,tid=1,seq=2,ret=0\t-\n",
        );
        assert_eq!(r[0].locations(), None, "nothing placed against a cut cwd");
        assert_eq!(r[1].locations(), Some(vec!["/abs".to_string()]));
        assert_eq!(stops(&r).len(), 1, "the relative read that reached a file");
        for (what, h) in [
            (
                "a cwd marked cut but written",
                "fswatch\tv3\tpid=7\tcwd=/w\ttrunc=1\n",
            ),
            ("trunc=2", "fswatch\tv3\tpid=7\tcwd=?\ttrunc=2\n"),
            ("another fifth field", "fswatch\tv3\tpid=7\tcwd=?\tx\n"),
            ("six fields", "fswatch\tv3\tpid=7\tcwd=?\ttrunc=1\t\n"),
        ] {
            refused(what, h, "v3 version line");
        }
    }

    /// APFS's Unicode case folding, not ASCII alone.
    #[test]
    fn the_stop_list_folds_case_as_unicode_does() {
        let read = |p: &str| {
            v3_stops(&format!(
                "stat\tpid=7,tid=1,seq=1\t{p}\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"
            ))
        };
        for (what, p) in [
            ("ſ (U+017F) for s", "/Library/Audio/Plug-Inſ/X.vst3"),
            (
                "ﬆ (U+FB06) for st",
                "/Sy\u{fb06}em/Library/Components/Foo.component",
            ),
            (
                "ﬅ (U+FB05) for st",
                "/Sy\u{fb05}em/Library/Components/Foo.component",
            ),
            ("ſ in a home's", &home("a/Library/Audio/Plug-Inſ/VST3")),
        ] {
            assert_eq!(read(p).len(), 1, "{what}: {p}");
        }
        assert_eq!(fold("Plug-Inſ ﬀ ẞ \u{212a}"), "plug-ins ff ss k");
        assert!(
            read("/Library/Audio/Plug-In\u{15b}/X").is_empty(),
            "ś (s + U+0301 canonically) is another name on APFS, not Plug-Ins"
        );
    }

    /// SUBSTRING matching: the firmlink spelling, `link/../..` walks, any home's.
    #[test]
    fn the_stop_list_matches_substrings() {
        let read = |p: &str| {
            v3_stops(&format!(
                "stat\tpid=7,tid=1,seq=1\t{p}\nstat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n"
            ))
        };
        let mut cases = vec![
            "/System/Volumes/Data/Library/Audio/Plug-Ins/VST3/X.vst3".to_string(),
            "/System/Volumes/Data/System/Library/Components/Foo.component".to_string(),
            home("someone/Library/Audio/Plug-Ins/Components/X.component"),
        ];
        if let Some(h) = real_home() {
            cases.push(format!(
                "/System/Volumes/Data{h}/Library/Preferences/com.apple.x.plist"
            ));
        }
        for p in &cases {
            assert_eq!(read(p).len(), 1, "{p}");
        }
        // A relative walk with no cwd to place it: judged on the path as logged.
        let st = v3_stops(
            "fswatch\tv3\tpid=8\tcwd=?\n\
             stat\tpid=8,tid=1,seq=1\tlink/../../Library/Audio/Plug-Ins/X\n\
             stat.ret\tpid=8,tid=1,seq=1,ret=-1,errno=ENOENT\t-\n",
        );
        assert!(
            st.iter().any(|s| s.starts_with("a plugin folder")),
            "{st:?}"
        );
        // Writes stay by prefix: a home's own Library (the lab's crash logs) is no STOP.
        assert!(
            v3_stops(&format!("open\tpid=7,tid=1,seq=1,flags=0x209,mode=0644\t{}\nopen.ret\tpid=7,tid=1,seq=1,ret=3\t-\n", home("x/lab/home/Library/Logs/c.log")))
                .is_empty()
        );
        let st = v3_stops("open\tpid=7,tid=1,seq=1,flags=0x1\t/System/Volumes/Data/Library/x\n");
        assert!(
            st.iter().any(|s| s.starts_with("a write under /System")),
            "the firmlink spelling: {st:?}"
        );
    }

    /// A cut image= on an otherwise clean absolute dlopen STOPS.
    #[test]
    fn a_cut_image_stops() {
        let st = v3_stops(
            "dlopen\tpid=7,tid=1,seq=1\t/w/lib.dylib\n\
             dlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=/w/li,trunc=1\t-\n",
        );
        assert_eq!(st.len(), 1, "{st:?}");
        assert!(st[0].starts_with("a cut image="), "{st:?}");
    }

    /// A cut that splits a UTF-8 character leaves bytes that are not UTF-8: the log is refused (unreadable), which
    /// fails its consumer: a STOP.
    #[test]
    fn a_split_utf8_character_is_refused() {
        let mut b =
            b"fswatch\tv3\tpid=7\tcwd=/w\nstat\tpid=7,tid=1,seq=1,trunc=1\t/w/\xc3".to_vec();
        b.extend_from_slice(b"\n");
        let t = unsafe_free_lossless(&b);
        assert!(
            t.is_none(),
            "not UTF-8: no &str can hold it, so no log text reaches the parser"
        );
        // As text, the recorder's escapes are the only way non-UTF-8 could appear, and \xHH >= 0x80 is refused.
        refused(
            "an escaped high byte",
            "fswatch\tv3\tpid=7\tcwd=/w\nstat\tpid=7,tid=1,seq=1\t/w/\\xc3\n",
            "never writes",
        );
    }

    fn unsafe_free_lossless(b: &[u8]) -> Option<&str> {
        std::str::from_utf8(b).ok()
    }

    /// A header's cwd escapes ',' as `\x2c` (and TAB as `\t`), as the
    /// real recorder writes it; a raw ',' is refused, first header or later.
    #[test]
    fn a_header_cwd_escapes_its_commas() {
        let r = parse(
            "fswatch\tv3\tpid=7\tcwd=/a\\x2cb\\tc\n\
             stat\tpid=7,tid=1,seq=1\trel\n\
             stat.ret\tpid=7,tid=1,seq=1,ret=0\t-\n",
        );
        assert_eq!(
            r[0].locations(),
            Some(vec!["/a,b\tc/rel".to_string()]),
            "unescaped, then placed"
        );
        refused(
            "a raw ',' in the first header",
            "fswatch\tv3\tpid=7\tcwd=/a,b\n",
            "malformed v3",
        );
        refused(
            "a raw ',' in a later header",
            "fswatch\tv3\tpid=7\tcwd=/w\nfswatch\tv3\tpid=7\tcwd=/a,b\n",
            "not a v3 version line",
        );
    }

    /// Exemptions (2) and (3) (seen in a real recording: two dlopens of exactly the executable, ret=-1 then
    /// ret=0,image=?): for the two EXACT names only, a failed load and an unmatched image are exempt, and listed.
    #[test]
    fn a_failed_or_unmatched_coreaudio_dlopen_is_exempt_and_listed() {
        let ca = "/System/Library/Components/CoreAudio.component";
        let exe = format!("{ca}/Contents/MacOS/CoreAudio");
        let log = |body: &str| parse(&format!("fswatch\tv3\tpid=7\tcwd=/w\n{body}"));
        // The sitting's two lines, exactly.
        let r = log(&format!(
            "dlopen\tpid=7,tid=1,seq=1\t{exe}\ndlopen.ret\tpid=7,tid=1,seq=1,ret=-1\t-\n\
             dlopen\tpid=7,tid=1,seq=2\t{exe}\ndlopen.ret\tpid=7,tid=1,seq=2,ret=0,image=?\t-\n\
             dlopen\tpid=7,tid=1,seq=3\t{ca}\ndlopen.ret\tpid=7,tid=1,seq=3,ret=0,image=?\t-\n\
             dlopen\tpid=7,tid=1,seq=4\t{exe}\ndlopen.ret\tpid=7,tid=1,seq=4,ret=0,image={exe}\t-\n\
             stat\tpid=7,tid=1,seq=5\t{ca}/Contents/Info.plist\nstat.ret\tpid=7,tid=1,seq=5,ret=0\t-\n"
        ));
        assert!(stops(&r).is_empty(), "{:?}", stops(&r));
        let ex = exemptions(&r);
        assert_eq!(ex.len(), 5, "{ex:?}");
        assert!(
            ex[0].starts_with("exempt, a CoreAudio dlopen: a failed load"),
            "{ex:?}"
        );
        assert!(
            ex[1].starts_with("exempt, a CoreAudio dlopen: image unmatched"),
            "{ex:?}"
        );
        assert!(
            ex[2].starts_with("exempt, a CoreAudio dlopen: image unmatched"),
            "the bundle path: {ex:?}"
        );
        assert!(
            ex[3].starts_with("exempt, a CoreAudio dlopen: dlopen\t"),
            "both names: {ex:?}"
        );
        assert!(ex[4].starts_with("exempt, a CoreAudio read"), "{ex:?}");
        // Still stopping: every other name, with ? or failed; a cut image; one that never returned.
        for (what, body) in [
            ("another file inside, image ?", format!("dlopen\tpid=7,tid=1,seq=1\t{ca}/Contents/Resources/x.dylib\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=?\t-\n")),
            ("another file inside, failed", format!("dlopen\tpid=7,tid=1,seq=1\t{ca}/Contents/Resources/x.dylib\ndlopen.ret\tpid=7,tid=1,seq=1,ret=-1\t-\n")),
            ("another component, image ?", "dlopen\tpid=7,tid=1,seq=1\t/System/Library/Components/Other.component\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=?\t-\n".to_string()),
            ("a case variant, failed", format!("dlopen\tpid=7,tid=1,seq=1\t{ca}/Contents/MacOS/coreaudio\ndlopen.ret\tpid=7,tid=1,seq=1,ret=-1\t-\n")),
            ("a cut image", format!("dlopen\tpid=7,tid=1,seq=1\t{exe}\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=/Sys,trunc=1\t-\n")),
            ("never returned", format!("dlopen\tpid=7,tid=1,seq=1\t{exe}\n")),
        ] {
            let r = log(&body);
            assert!(!stops(&r).is_empty(), "{what}");
            assert!(exemptions(&r).is_empty(), "{what}: not listed as exempt");
        }
        let r = log(
            "dlopen\tpid=7,tid=1,seq=1\tCoreAudio\ndlopen.ret\tpid=7,tid=1,seq=1,ret=0,image=?\t-\n",
        );
        assert!(
            !stops(&r).is_empty(),
            "a bare name with image=? stops (unplaceable)"
        );
    }
}
