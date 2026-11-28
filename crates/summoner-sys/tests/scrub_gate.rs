//! The exported crates carry no review label: no question id (a `Q`, a dash, a digit or a capital and a digit), no
//! job number written short (`S` or `s` and exactly three digits, alone) or as a ticket or decision id (`S` or `D`, a
//! dash, a digit), no job-numbered mutation cfg, no review decision label, no reviewer name. Every file git tracks
//! (or leaves untracked and not ignored) under `crates/` is read, paths and contents, this one included: its own
//! words and controls are assembled at run time, so its text holds none of them. Each rule has a planted control and
//! the near misses an audio crate is full of (AU codes, hex digests, sample rates, `s16`-style tokens).
use std::path::Path;
use std::process::Command;

fn ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The starts of `w` in `s` with no identifier character on either side.
fn words<'a>(s: &'a str, w: &'a str) -> impl Iterator<Item = usize> + 'a {
    s.match_indices(w).map(|(i, _)| i).filter(move |&i| {
        !s[..i].chars().next_back().is_some_and(ident)
            && !s[i + w.len()..].chars().next().is_some_and(ident)
    })
}

/// The label a line carries, if any.
fn label(line: &str) -> Option<String> {
    // Its mutant (never in a normal build): the rule sees nothing. `every_label_shape_is_found` fails.
    if cfg!(mutant_scrub_labels_blind) {
        return None;
    }
    for i in line.match_indices("Q-").map(|(i, _)| i) {
        let ok_before = match line[..i].chars().next_back() {
            Some('O') => !line[..i - 1].chars().next_back().is_some_and(ident),
            b => !b.is_some_and(ident),
        };
        let mut rest = line[i + 2..].chars();
        let id = match rest.next() {
            Some(c) if c.is_ascii_digit() => true,
            Some(c) if c.is_ascii_uppercase() => rest.next().is_some_and(|d| d.is_ascii_digit()),
            _ => false,
        };
        if ok_before && id {
            return Some(line[i..(i + 4).min(line.len())].to_string());
        }
    }
    let b = line.as_bytes();
    for i in 0..b.len() {
        if (b[i] == b'S' || b[i] == b's')
            && i + 4 <= b.len()
            && b[i + 1..i + 4].iter().all(u8::is_ascii_digit)
            && !line[..i].chars().next_back().is_some_and(ident)
            && !line[i + 4..].chars().next().is_some_and(ident)
        {
            return Some(line[i..i + 4].to_string());
        }
        // A job number at the start of an identifier (a test named after one): `s` or `S`, exactly three digits,
        // then `_`.
        if (b[i] == b'S' || b[i] == b's')
            && i + 5 <= b.len()
            && b[i + 1..i + 4].iter().all(u8::is_ascii_digit)
            && b[i + 4] == b'_'
            && !line[..i].chars().next_back().is_some_and(ident)
        {
            return Some(line[i..i + 5].to_string());
        }
    }
    for prefix in ["S-", "D-"] {
        for i in line.match_indices(prefix).map(|(i, _)| i) {
            let digit = line[i + 2..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit());
            if digit && !line[..i].chars().next_back().is_some_and(ident) {
                return Some(line[i..(i + 3).min(line.len())].to_string());
            }
        }
    }
    let cfg = j(&["mutant", "_s"]);
    if let Some(i) = line.find(&cfg)
        && line[i + cfg.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        return Some("a job-numbered cfg".into());
    }
    for w in WORDS {
        let w = j(w);
        if words(line, &w).next().is_some() {
            return Some(w);
        }
    }
    None
}

/// `parts` joined: the words below are never written whole in this file.
fn j(parts: &[&str]) -> String {
    parts.concat()
}

/// A review's words and the reviewers' names, in pieces.
const WORDS: [&[&str]; 10] = [
    &["rul", "ing"],
    &["Rul", "ing"],
    &["rul", "ings"],
    &["Rul", "ings"],
    &["the re", "view"],
    &["The re", "view"],
    &["mg", "mt"],
    &["Ste", "ve"],
    &["Mac", " A"],
    &["Mac", " B"],
];

#[test]
fn the_crates_carry_no_review_label() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "crates",
        ])
        .output()
        .expect("git ls-files");
    assert!(out.status.success());
    let mut files = 0;
    let mut bad = Vec::new();
    for f in out.stdout.split(|b| *b == 0).filter(|f| !f.is_empty()) {
        let f = String::from_utf8_lossy(f).into_owned();
        files += 1;
        if let Some(t) = label(&f) {
            bad.push(format!("{f} (path: {t})"));
        }
        let Ok(body) = std::fs::read(repo.join(&f)) else {
            continue;
        };
        for (i, line) in String::from_utf8_lossy(&body).lines().enumerate() {
            if let Some(t) = label(line) {
                bad.push(format!("{f}:{} ({t})", i + 1));
            }
        }
    }
    assert!(files > 100, "the crates are listed ({files} files)");
    assert!(bad.is_empty(), "{} label(s): {bad:#?}", bad.len());
}

/// The rule is not blind: each label shape is found, and the near misses are not.
#[test]
fn every_label_shape_is_found() {
    for (line, want) in [
        (j(&["// (Q", "-3) the bound"]), j(&["Q", "-3)"])),
        (
            j(&["/// amendment (c), Q", "-L1: a slot"]),
            j(&["Q", "-L1"]),
        ),
        (j(&["/// ADR 0005 OQ", "-6(1): after"]), j(&["Q", "-6("])),
        (j(&["/// Rev 1 S6", "31-2 / O3: a page"]), j(&["S6", "31"])),
        (j(&["// the s8", "55 contract"]), j(&["s8", "55"])),
        (j(&["    fn s8", "44_notices() {"]), j(&["s8", "44_"])),
        (j(&["// S", "-706 S3: the split"]), j(&["S", "-7"])),
        (j(&["// (D", "-16)"]), j(&["D", "-1"])),
        (
            j(&["    if cfg!(mutant", "_s671_chase_raw_pos) {"]),
            "a job-numbered cfg".into(),
        ),
        (
            j(&["// a protocol fault (rul", "ing 5)"]),
            j(&["rul", "ing"]),
        ),
        (
            j(&["// the re", "view's P1: a take"]),
            j(&["the re", "view"]),
        ),
        (j(&["// as mg", "mt asked"]), j(&["mg", "mt"])),
    ] {
        assert_eq!(label(&line).as_deref(), Some(want.as_str()), "{line}");
    }
    for clean in [
        "const CODES: &str = \"61756678-534c6761-5361534c\"; // aufx S1gx",
        "let d = \"a5e3f1s0b2\"; let s16 = 48_000; // s16le, S1234, xS123, S12, D16, ABS-1",
        "QQ-1 and FAQ-2 and Q-x and HQ-12",
        "fn s16_le() {} fn s1234_x() {} fn xs123_y() {} // s12_",
        &j(&[
            "#[cfg(mutant_scan_x)] fn rul",
            "ingx() {} // prerul",
            "ings, overrul",
            "ing, the re",
            "viewer, Ste",
            "ven",
        ]),
    ] {
        assert_eq!(label(clean), None, "{clean}");
    }
}
