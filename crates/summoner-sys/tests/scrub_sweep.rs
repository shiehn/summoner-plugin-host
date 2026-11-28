//! The published files carry no internal design vocabulary, so a reader finds no dangling reference. Every file git
//! tracks (or leaves untracked and not ignored) under `crates/` and `testkit/plugins/` is read, this one included (its
//! planted words are assembled at run time), except the paths `tools/scrub-skip.txt` lists when a checkout carries
//! that file (one path per line; a published tree has none). Four rules, each with planted hits, near misses and a
//! one-cfg mutant (`mutant_scrub_<rule>_blind`, killed by `every_sweep_rule_finds_its_plant`):
//! - **A:** "the" and "core" as one phrase, case-insensitive (the protocol's two sides are the host and the child);
//! - **B, a bare design label:** one or two capitals, one or two digits and an optional lower-case letter, standing
//!   alone. Not a label: the realtime rules N1 to N12 (defined in Runecraft's `docs/realtime-rules.md`), an identifier
//!   this tree's code or API snapshots define where it is used as code, a short real-term list, and the exact lines of
//!   [`label_allow`];
//! - **C, a review or known-issue id:** an H, a dash and one digit; a K and an I, a dash and digits;
//! - **D, an ADR reference:** the word ADR and a four-digit number; or, in a comment, a bare ADR number (one to
//!   nineteen, four digits) with an ADR-shaped context: a decision label after it, a possessive or an opening
//!   parenthesis, or "in", "of" or "to" before it.
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

/// The paths a checkout keeps out of the published tree (`tools/scrub-skip.txt`, one per line, `#` comments), if
/// it has that file.
fn skipped(repo: &Path) -> HashSet<String> {
    std::fs::read_to_string(repo.join("tools/scrub-skip.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(String::from)
        .collect()
}

/// Rule B's exact exceptions (file, whole line), at most 8: the lab watcher's errno spellings, test data and the
/// documentation of them, the same four the release scan allows. Assembled at run time, so this file's own text
/// holds none of the tokens.
fn label_allow() -> [(&'static str, String); 4] {
    let w = "testkit/plugins/fswatch/fswatch_v2.rs";
    [
        (
            w,
            j(&[
                "/// other errno is `E<decimal>`, and a named one is never written by its number (`E",
                "2` for ENOENT is refused).",
            ]),
        ),
        (w, j(&["                post(\"ret=-1,errno=E", "2\"),"])),
        (
            w,
            j(&[
                "            (\"errno E",
                "0\", post(\"ret=-1,errno=E",
                "0\"), \"not a fixed name\"),",
            ]),
        ),
        (
            "testkit/plugins/fswatch/summoner_lab_fswatch.c",
            j(&["// written by its number, so `E", "2` is malformed):"]),
        ),
    ]
}

/// Real-world terms shaped like a label.
const TERMS: [&str; 14] = [
    "LP64", "FZ16", "A64", "X11", "U8", "U16", "U32", "U64", "I8", "I32", "I64", "F32", "F64",
    "AU2",
];

fn ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn idents(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !ident(c)).filter(|w| {
        w.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    })
}

fn j(parts: &[&str]) -> String {
    parts.concat()
}

/// A line split into its code (string literals blanked) and its comment (`//` in Rust and C-family files, `#` in
/// TOML, shell, Python and CMake, the whole line in Markdown).
fn split(file: &str, line: &str) -> (String, String) {
    if file.ends_with(".md") {
        return (String::new(), line.to_string());
    }
    let hash = [".toml", ".sh", ".py", ".txt", ".cmake"]
        .iter()
        .any(|e| file.ends_with(e));
    let cs: Vec<char> = line.chars().collect();
    let (mut code, mut in_str, mut esc) = (String::new(), false, false);
    for (i, &c) in cs.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            code.push(' ');
        } else if c == '"' {
            in_str = true;
            code.push(' ');
        } else if (!hash && c == '/' && cs.get(i + 1) == Some(&'/')) || (hash && c == '#') {
            return (code, cs[i..].iter().collect());
        } else {
            code.push(c);
        }
    }
    (code, String::new())
}

/// Rule A.
fn rule_the_core(line: &str) -> Option<String> {
    if cfg!(mutant_scrub_the_core_blind) {
        return None;
    }
    let phrase = j(&["the c", "ore"]);
    let low = line.to_ascii_lowercase();
    low.match_indices(phrase.as_str()).find_map(|(i, _)| {
        let before = low[..i].chars().next_back().is_some_and(ident);
        let after = low[i + 8..].chars().next().is_some_and(ident);
        (!before && !after).then(|| line[i..i + 8].to_string())
    })
}

/// Rule B. A token the crates' code defines is exempt only where it is code: in the code part of the line, or in the
/// comment as a path (`Mode::V1`) or inside backticks. The same word bare in comment prose is a label.
fn rule_bare_label(line: &str, comment: &str, defined: &HashSet<String>) -> Option<String> {
    if cfg!(mutant_scrub_label_blind) {
        return None;
    }
    let cs: Vec<char> = line.chars().collect();
    let cstart = cs.len() - comment.chars().count();
    // In a comment: a path (`::` before it) or inside backticks is code. Its mutant (never in a normal build): every
    // defined token is code wherever it stands; `every_sweep_rule_finds_its_plant` fails.
    let as_code = |i: usize| {
        cfg!(mutant_scrub_label_prose_blind)
            || i < cstart
            || (i >= 2 && cs[i - 2] == ':' && cs[i - 1] == ':')
            || cs[cstart..i].iter().filter(|c| **c == '`').count() % 2 == 1
    };
    let edge =
        |c: Option<&char>| c.is_some_and(|c| ident(*c) || matches!(c, '-' | '.' | '/' | '+'));
    let mut i = 0;
    while i < cs.len() {
        if cs[i].is_ascii_uppercase() && !edge(if i == 0 { None } else { cs.get(i - 1) }) {
            let mut k = i;
            while k < cs.len() && k - i < 2 && cs[k].is_ascii_uppercase() {
                k += 1;
            }
            let d0 = k;
            while k < cs.len() && k - d0 < 2 && cs[k].is_ascii_digit() {
                k += 1;
            }
            if k > d0 {
                if k < cs.len()
                    && cs[k].is_ascii_lowercase()
                    && !cs.get(k + 1).is_some_and(|c| ident(*c))
                {
                    k += 1;
                }
                if !cs.get(k).is_some_and(|c| ident(*c)) {
                    let tok: String = cs[i..k].iter().collect();
                    let n_rule = tok
                        .strip_prefix('N')
                        .and_then(|n| n.parse::<u32>().ok())
                        .is_some_and(|n| (1..=12).contains(&n));
                    if !n_rule
                        && !TERMS.contains(&tok.as_str())
                        && !(defined.contains(&tok) && as_code(i))
                    {
                        return Some(tok);
                    }
                }
            }
            i = k.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Rule C.
fn rule_hyphen_label(line: &str) -> Option<String> {
    if cfg!(mutant_scrub_hyphen_blind) {
        return None;
    }
    for p in [j(&["H", "-"]), j(&["K", "I-"])] {
        for (i, _) in line.match_indices(p.as_str()) {
            let before = line[..i]
                .chars()
                .next_back()
                .is_some_and(|c| ident(c) || matches!(c, '.' | '/' | '-'));
            let digits = line[i + p.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .count();
            let ok = if p.len() == 2 {
                digits == 1
            } else {
                digits >= 1
            };
            if !before && ok {
                return Some(line[i..i + p.len() + digits].to_string());
            }
        }
    }
    None
}

/// Rule D.
fn rule_adr(line: &str, comment: &str) -> Option<String> {
    if cfg!(mutant_scrub_adr_blind) {
        return None;
    }
    let word = j(&["AD", "R"]);
    for (i, _) in line.match_indices(word.as_str()) {
        let rest = &line[i + 3..];
        let before = line[..i].chars().next_back().is_some_and(ident);
        if !before
            && (rest.starts_with(' ') || rest.starts_with('-'))
            && rest.len() >= 5
            && rest.as_bytes()[1..5].iter().all(u8::is_ascii_digit)
            && !rest.as_bytes().get(5).is_some_and(u8::is_ascii_digit)
        {
            return Some(line[i..i + 8].to_string());
        }
    }
    let b = comment.as_bytes();
    for i in 0..b.len().saturating_sub(3) {
        let d = &b[i..i + 4];
        let adr = d[0] == b'0'
            && d[1] == b'0'
            && (d[2] == b'0' && d[3].is_ascii_digit() && d[3] != b'0'
                || d[2] == b'1' && d[3].is_ascii_digit());
        if !adr {
            continue;
        }
        let prev = comment[..i].chars().next_back();
        if prev
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | ':' | '-'))
        {
            continue;
        }
        let after = &b[i + 4..];
        let ctx_after = after.starts_with(b"'s")
            || after.starts_with(b" (")
            || (after.len() >= 3
                && after[0] == b' '
                && matches!(after[1], b'D' | b'T' | b'A')
                && after[2].is_ascii_digit());
        let ctx_before = ["in ", "of ", "to "].iter().any(|w| {
            comment[..i].ends_with(w)
                && !comment[..i - w.len()]
                    .chars()
                    .next_back()
                    .is_some_and(ident)
                && !after
                    .first()
                    .is_some_and(|c| c.is_ascii_digit() || *c == b':' || *c == b'.')
        });
        if ctx_after || ctx_before {
            return Some(comment[i..i + 4].to_string());
        }
    }
    None
}

fn ls(repo: &Path) -> Vec<String> {
    let skip = skipped(repo);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--",
            "crates",
            "testkit/plugins",
        ])
        .output()
        .expect("git ls-files");
    assert!(out.status.success());
    out.stdout
        .split(|b| *b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .filter(|f| !skip.contains(f))
        .collect()
}

#[test]
fn the_published_files_carry_no_internal_design_vocabulary() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut defined: HashSet<String> = HashSet::new();
    let mut texts = Vec::new();
    for f in ls(&repo) {
        let Ok(body) = std::fs::read_to_string(repo.join(&f)) else {
            continue;
        };
        if [".rs", ".cpp", ".h", ".mm", ".c"]
            .iter()
            .any(|e| f.ends_with(e))
        {
            for line in body.lines() {
                defined.extend(idents(&split(&f, line).0).map(String::from));
            }
        }
        texts.push((f, body));
    }
    if let Ok(api) = std::fs::read_dir(repo.join("api")) {
        for e in api.flatten() {
            for line in std::fs::read_to_string(e.path())
                .unwrap_or_default()
                .lines()
            {
                defined.extend(idents(line).map(String::from));
            }
        }
    }
    let allow = label_allow();
    let mut bad = Vec::new();
    for (f, body) in &texts {
        for (i, line) in body.lines().enumerate() {
            let comment = split(f, line).1;
            let found = [
                rule_the_core(line),
                (!allow.iter().any(|(af, al)| af == f && al == line))
                    .then(|| rule_bare_label(line, &comment, &defined))
                    .flatten(),
                rule_hyphen_label(line),
                rule_adr(line, &comment),
            ];
            for t in found.into_iter().flatten() {
                bad.push(format!("{f}:{} ({t})", i + 1));
            }
        }
    }
    assert!(
        texts.len() > 100,
        "the exported files are listed ({} files)",
        texts.len()
    );
    assert!(bad.is_empty(), "{} hit(s): {bad:#?}", bad.len());
}

/// Every allowed line is still in its file, and the list stays short.
#[test]
fn every_label_allowance_is_live() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let allow = label_allow();
    assert!(allow.len() <= 8);
    for (f, l) in allow {
        let body = std::fs::read_to_string(repo.join(f)).unwrap_or_default();
        assert!(
            body.lines().any(|x| x == l),
            "stale allow-label entry: {f}: {l}"
        );
    }
}

#[test]
fn every_sweep_rule_finds_its_plant() {
    let core = j(&["the c", "ore"]);
    assert!(rule_the_core(&format!("// what {core} sent")).is_some());
    assert!(rule_the_core(&format!("\"The C{}'s decoder\"", "ore")).is_some());
    for clean in [
        j(&["// core::ptr, the c", "ores, the c", "ore_x and CoreAudio"]),
        j(&["the c", "oreless"]),
    ] {
        assert_eq!(rule_the_core(&clean), None, "{clean}");
    }
    let defined: HashSet<String> = [j(&["EX", "1"])].into();
    for planted in [
        j(&["// as D", "7 says"]),
        j(&["// (W", "5, x)"]),
        j(&["// L", "1a: the blob"]),
    ] {
        assert!(
            rule_bare_label(&planted, &split("a.rs", &planted).1, &defined).is_some(),
            "{planted}"
        );
    }
    for clean in [
        j(&[
            "// N1 N12 `EX",
            "1` LP64 U64 3D",
            "7 D",
            "7_x a.D",
            "7 x-D",
            "7 D",
            "123 v2 C",
            "-12",
        ]),
        j(&["let x = D", "7x2; // D", "7xy"]),
    ] {
        assert_eq!(
            rule_bare_label(&clean, &split("a.rs", &clean).1, &defined),
            None,
            "{clean}"
        );
    }
    // A word the code defines is code only as code: a path or backticked in a comment; bare in prose it is a label.
    let v1: HashSet<String> = [j(&["V", "1"])].into();
    let prose = j(&["pub enum Mode { V", "1 } // the child is untrusted, V", "1"]);
    assert!(
        rule_bare_label(&prose, &split("a.rs", &prose).1, &v1).is_some(),
        "{prose}"
    );
    for code in [
        j(&[
            "let m = Mode::V",
            "1; // Mode::V",
            "1 and `V",
            "1` are code",
        ]),
        j(&["match m { V", "1 => {} }"]),
    ] {
        assert_eq!(
            rule_bare_label(&code, &split("a.rs", &code).1, &v1),
            None,
            "{code}"
        );
    }
    assert!(rule_hyphen_label(&j(&["// the H", "-2 deadline"])).is_some());
    assert!(rule_hyphen_label(&j(&["// (K", "I-42 x)"])).is_some());
    for clean in [
        j(&["// Z-1 G-1 N-1 HQ-12 K", "I-x H- x.H", "-1 SK", "I-4"]),
        j(&["H", "-12"]),
    ] {
        assert_eq!(rule_hyphen_label(&clean), None, "{clean}");
    }
    let a = j(&["// see AD", "R 00", "05"]);
    assert!(rule_adr(&a, &a).is_some());
    for c in [
        j(&["// its 00", "05 D", "7 wedge"]),
        j(&["// 00", "07's wire budget"]),
        j(&["// not in 00", "04"]),
        j(&["// 00", "03 (the graph)"]),
    ] {
        assert!(rule_adr(&c, &c).is_some(), "{c}");
    }
    for c in [
        j(&[
            "let p = 0x0005; // exit 00",
            "04, 1.0005, 00:05, 0005x, in 00",
            "04.5, AD",
            "Rs 0005",
        ]),
        j(&["// in 0020 and 0000 D", "7"]),
    ] {
        assert_eq!(rule_adr(&c, &c), None, "{c}");
    }
    assert_eq!(split("a.rs", "let s = \"// no\"; // yes").1, "// yes");
    assert_eq!(split("a.toml", "x = \"#no\" # yes").1, "# yes");
}
