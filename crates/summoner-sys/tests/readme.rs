//! The README shows real code: every `rust` block in this crate's `README.md` is, byte for byte, one of the crate's
//! `examples/*.rs`, which `cargo test` builds (and runs, for an example declared with `test = true`). Change the
//! example, then paste it into the README; this test fails until the two agree. The README starts with its image.
#![forbid(unsafe_code)]
// A test of the README, never on a realtime thread: a crate's realtime clippy list (its clippy.toml) does not bind it.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::Path;

/// The `rust` blocks of a Markdown text, each as written between its fences (every line ends with a newline). A
/// fence that marks Rust any other way (`rust,ignore`, `rs`, `no_run`) is refused: README code is never left
/// unchecked.
fn rust_blocks(md: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    for (n, line) in md.lines().enumerate() {
        let fence = line.trim_end();
        match cur.as_mut() {
            None if fence == "```rust" => cur = Some(String::new()),
            None if fence.starts_with("```rust")
                || fence.starts_with("```rs")
                || (fence.starts_with("```") && fence.contains("no_run")) =>
            {
                return Err(format!("line {}: an unchecked Rust fence `{fence}`", n + 1));
            }
            None => {}
            Some(_) if fence == "```" => out.extend(cur.take()),
            Some(b) => {
                b.push_str(line);
                b.push('\n');
            }
        }
    }
    match cur {
        Some(_) => Err("an unclosed rust block".into()),
        None => Ok(out),
    }
}

/// The crate's examples, by file name.
fn examples(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir.join("examples")).expect("the crate's examples/ folder") {
        let p = e.expect("a folder entry").path();
        if p.extension().is_some_and(|x| x == "rs") {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            out.push((name, std::fs::read_to_string(&p).expect("an example")));
        }
    }
    out
}

#[test]
fn every_rust_block_in_the_readme_is_one_of_the_examples() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = std::fs::read_to_string(dir.join("README.md")).expect("the crate's README.md");
    assert!(
        readme.starts_with("![")
            && readme
                .lines()
                .next()
                .is_some_and(|l| l.contains("](../../docs/images/")),
        "README.md does not start with the repository's image"
    );
    let blocks = rust_blocks(&readme).unwrap_or_else(|e| panic!("README.md: {e}"));
    assert!(!blocks.is_empty(), "README.md has no rust quick start");
    let examples = examples(dir);
    for (i, b) in blocks.iter().enumerate() {
        assert!(
            examples.iter().any(|(_, e)| e == b),
            "README.md's rust block {} is none of examples/{:?} byte for byte",
            i + 1,
            examples.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );
    }
}

/// The reader is not blind: it finds a planted block, refuses an unchecked fence and an unclosed block, and a block
/// that differs by one byte from an example does not match it.
#[test]
fn the_block_reader_finds_planted_blocks() {
    let md = "x\n\n```rust\nfn main() {}\n```\n\n```sh\ncargo run\n```\n";
    assert_eq!(rust_blocks(md), Ok(vec!["fn main() {}\n".to_string()]));
    assert!(rust_blocks("```rust,ignore\nx\n```\n").is_err());
    assert!(rust_blocks("```rust\nfn main() {}\n").is_err());
    assert_ne!(rust_blocks(md).unwrap()[0], "fn main() { }\n");
}
