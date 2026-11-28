//! The release loading policy (`release-loading`), judged with lab paths only. Run: `cargo test -p summoner-sys
//! --features release-loading,vst3 --test release_loading`. Every bundle here is a copy of the lab's own build output
//! in this test's scratch directory (target.noindex/tmp), OUTSIDE the lab gate's roots: the lab gate refuses it, the
//! release policy accepts it. Every path outside the lab is refused by the lab fence (this build has
//! `SUMMONER_DEV_PLUGIN_ROOTS`) lexically, and a link pointing out of the lab before its target is touched: the
//! file-access recorder (`libsummoner_lab_fswatch.dylib`, injected into a child run of this binary) proves no syscall left
//! the lab.
#![cfg(feature = "release-loading")]
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use summoner_sys::bundle::{Bundle, Format, MAX_LINKS, Refusal};

#[path = "../../../testkit/plugins/fswatch/fswatch_v2.rs"]
mod fswatch_v2;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The real home (the lab env points HOME at the lab's): only ever named, never touched.
fn real_home() -> &'static str {
    env!("SUMMONER_DEV_PLUGIN_ROOTS")
        .rsplit_once('/')
        .unwrap()
        .0
}

/// A fresh scratch directory under target.noindex/tmp (canonical: no link on the way, so it is what a check resolves).
fn scratch(name: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("release-loading-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().filter_map(Result::ok) {
        let t = e.file_type().unwrap();
        let dst = to.join(e.file_name());
        if t.is_dir() {
            copy_tree(&e.path(), &dst);
        } else {
            assert!(t.is_file(), "{:?}", e.path());
            std::fs::copy(e.path(), &dst).unwrap();
        }
    }
}

/// A copy of the lab's built `name` (`vst3/summoner-lab-gain.vst3`, say) at `to`.
fn lab_copy(name: &str, to: &Path) -> PathBuf {
    copy_tree(&lab_plugins::dir().join(name), to);
    to.to_path_buf()
}

fn check(p: &Path, f: Format) -> Result<PathBuf, Refusal> {
    Bundle::check(p, f).map(|b| b.path().to_path_buf())
}

#[test]
fn a_copy_outside_c1s_roots_is_refused_by_c1_and_accepted_by_the_release_policy() {
    let s = scratch("copy");
    let vst3 = lab_copy(
        "vst3/summoner-lab-gain.vst3",
        &s.join("elsewhere/summoner-lab-gain.vst3"),
    );
    let au = lab_copy(
        "au/summoner-lab-gain.component",
        &s.join("elsewhere/summoner-lab-gain.component"),
    );
    assert_eq!(
        Bundle::check_dev(&vst3, Format::Vst3).unwrap_err(),
        Refusal::OutsideRoot
    );
    assert_eq!(
        Bundle::check_dev(&au, Format::Au).unwrap_err(),
        Refusal::OutsideRoot
    );
    assert_eq!(check(&vst3, Format::Vst3).unwrap(), vst3);
    assert_eq!(check(&au, Format::Au).unwrap(), au);
    // The extension, ASCII case-insensitive.
    let upper = s.join("elsewhere/summoner-lab-gain.VST3");
    std::fs::rename(&vst3, &upper).unwrap();
    assert_eq!(check(&upper, Format::Vst3).unwrap(), upper);
}

/// The copy loads: the shim opens it, its factory lists the gain, and the post-load image check (kept) finds only the
/// copy's executable and the system's own: no STOP.
#[cfg(feature = "vst3")]
#[test]
fn a_copy_outside_c1s_roots_loads_clean() {
    let s = scratch("load");
    let vst3 = lab_copy(
        "vst3/summoner-lab-gain.vst3",
        &s.join("x/summoner-lab-gain.vst3"),
    );
    let b = Bundle::check(&vst3, Format::Vst3).unwrap();
    let m = summoner_sys::vst3::Module::open(&b).expect("the copy opens");
    assert!(!m.classes().is_empty(), "the gain's factory lists a class");
    assert_eq!(summoner_sys::bundle::stops(), 0);
    let images = summoner_sys::dyld::images();
    assert!(
        images.iter().any(|i| Path::new(i).starts_with(&vst3)),
        "the copy's executable is loaded: {images:?}"
    );
}

#[test]
fn lexical_and_shape_refusals() {
    let s = scratch("shape");
    let vst3 = lab_copy(
        "vst3/summoner-lab-gain.vst3",
        &s.join("a/summoner-lab-gain.vst3"),
    );
    let d = s.display();
    for p in [
        "summoner-lab-gain.vst3".to_string(),
        "a/summoner-lab-gain.vst3".to_string(),
        format!("{d}/a/../a/summoner-lab-gain.vst3"),
        format!("{d}/./a/summoner-lab-gain.vst3"),
        format!("{d}//a/summoner-lab-gain.vst3"),
        format!("{d}/a/summoner-lab-gain.vst3/"),
    ] {
        assert_eq!(
            check(Path::new(&p), Format::Vst3).unwrap_err(),
            Refusal::NotLexical,
            "{p}"
        );
    }
    // The wrong format's extension; the extension alone; none.
    assert_eq!(
        check(&vst3, Format::Au).unwrap_err(),
        Refusal::Extension(vst3.clone())
    );
    let bare = s.join("a/.vst3");
    assert_eq!(
        check(&bare, Format::Vst3).unwrap_err(),
        Refusal::Extension(bare.clone())
    );
    let none = s.join("a");
    assert_eq!(
        check(&none, Format::Vst3).unwrap_err(),
        Refusal::Extension(none.clone())
    );
    // A plain file named like a bundle; a missing bundle.
    let file = s.join("a/file.vst3");
    std::fs::write(&file, b"not a bundle").unwrap();
    assert_eq!(
        check(&file, Format::Vst3).unwrap_err(),
        Refusal::NotABundle(file.clone())
    );
    let missing = s.join("a/missing.vst3");
    assert_eq!(
        check(&missing, Format::Vst3).unwrap_err(),
        Refusal::NotABundle(missing.clone())
    );
}

#[test]
fn symlinks_inside_the_lab_are_followed_and_the_target_is_what_is_checked() {
    let s = scratch("links");
    let vst3 = lab_copy(
        "vst3/summoner-lab-gain.vst3",
        &s.join("real/summoner-lab-gain.vst3"),
    );
    // The bundle a link (relative and absolute targets), and a linked directory on the way.
    symlink("real/summoner-lab-gain.vst3", s.join("rel.vst3")).unwrap();
    symlink(&vst3, s.join("abs.vst3")).unwrap();
    symlink("real", s.join("dirlink")).unwrap();
    symlink("rel.vst3", s.join("chain.vst3")).unwrap();
    for p in [
        "rel.vst3",
        "abs.vst3",
        "dirlink/summoner-lab-gain.vst3",
        "chain.vst3",
    ] {
        assert_eq!(check(&s.join(p), Format::Vst3).unwrap(), vst3, "{p}");
    }
    // A link to a directory that is not a bundle of the format; to a file; a loop; a dangling link.
    symlink("real", s.join("notabundle.vst3")).unwrap();
    assert_eq!(
        check(&s.join("notabundle.vst3"), Format::Vst3).unwrap_err(),
        Refusal::Extension(s.join("real"))
    );
    std::fs::write(s.join("real/plain.vst3"), b"x").unwrap();
    symlink("real/plain.vst3", s.join("tofile.vst3")).unwrap();
    assert_eq!(
        check(&s.join("tofile.vst3"), Format::Vst3).unwrap_err(),
        Refusal::NotABundle(s.join("real/plain.vst3"))
    );
    symlink("loop.vst3", s.join("loop.vst3")).unwrap();
    assert_eq!(
        check(&s.join("loop.vst3"), Format::Vst3).unwrap_err(),
        Refusal::LinkLoop(s.join("loop.vst3"))
    );
    assert_eq!(MAX_LINKS, 32);
    symlink("gone.vst3", s.join("dangling.vst3")).unwrap();
    assert_eq!(
        check(&s.join("dangling.vst3"), Format::Vst3).unwrap_err(),
        Refusal::NotABundle(s.join("gone.vst3"))
    );
}

/// In a lab build the inside of the bundle is the lab gate's walk: no symlink, no hard link (a link could lead CFBundle
/// out).
#[test]
fn in_a_lab_build_the_inside_has_no_link() {
    let s = scratch("inside");
    let a = lab_copy(
        "vst3/summoner-lab-gain.vst3",
        &s.join("a/summoner-lab-gain.vst3"),
    );
    let link = a.join("Contents/Resources-link");
    symlink("MacOS", &link).unwrap();
    assert_eq!(
        check(&a, Format::Vst3).unwrap_err(),
        Refusal::Symlink(link.clone())
    );
    std::fs::remove_file(&link).unwrap();
    assert_eq!(check(&a, Format::Vst3).unwrap(), a);
    let plist = a.join("Contents/Info.plist");
    let hard = s.join("plist.hard");
    std::fs::hard_link(&plist, &hard).unwrap();
    assert_eq!(
        check(&a, Format::Vst3).unwrap_err(),
        Refusal::HardLink(plist)
    );
}

/// The fence cases: (name, path, expected refusal). `s` is a scratch directory holding the links.
fn fence_cases(s: &Path) -> Vec<(&'static str, String, Refusal)> {
    // The format is the extension's (`.component`: AU, else VST3): see `format_of`.
    let home = real_home();
    let p = |x: &str| PathBuf::from(x);
    vec![
        (
            "system-vst3",
            "/Library/Audio/Plug-Ins/VST3/Any.vst3".into(),
            Refusal::OutsideLab(p("/Library/Audio/Plug-Ins/VST3/Any.vst3")),
        ),
        (
            "home-components",
            format!("{home}/Library/Audio/Plug-Ins/Components/Any.component"),
            Refusal::OutsideLab(p(&format!(
                "{home}/Library/Audio/Plug-Ins/Components/Any.component"
            ))),
        ),
        (
            "system-components",
            "/System/Library/Components/Any.component".into(),
            Refusal::OutsideLab(p("/System/Library/Components/Any.component")),
        ),
        (
            "link-to-system",
            s.join("out-system.vst3").display().to_string(),
            Refusal::OutsideLab(p("/Library")),
        ),
        (
            "link-to-home",
            s.join("out-home.vst3").display().to_string(),
            Refusal::OutsideLab(p(&format!("{home}/Library"))),
        ),
        (
            "link-climbing-out",
            s.join("out-climb.vst3").display().to_string(),
            Refusal::OutsideLab(p(&format!("{home}/Library"))),
        ),
    ]
}

/// Makes the fence cases' links (creating a link never touches its target).
fn fence_links(s: &Path) {
    let home = real_home();
    symlink(
        "/Library/Audio/Plug-Ins/VST3/Any.vst3",
        s.join("out-system.vst3"),
    )
    .unwrap();
    symlink(
        format!("{home}/Library/Audio/Plug-Ins/VST3/Any.vst3"),
        s.join("out-home.vst3"),
    )
    .unwrap();
    // `..` up to the real home (the scratch dir is <home>/<lab>/<repo>/target.noindex/tmp/<name>).
    let up = s
        .strip_prefix(home)
        .unwrap()
        .components()
        .map(|_| "..")
        .collect::<Vec<_>>()
        .join("/");
    symlink(
        format!("{up}/Library/Audio/Plug-Ins/VST3/Any.vst3"),
        s.join("out-climb.vst3"),
    )
    .unwrap();
}

fn format_of(path: &str) -> Format {
    if path.ends_with(".component") {
        Format::Au
    } else {
        Format::Vst3
    }
}

/// The child half: one fence case's check between two markers (a stat of a lab path each), its result printed.
#[test]
fn fence_child() {
    let (Ok(path), Ok(begin), Ok(end)) = (
        std::env::var("SUMMONER_FENCE_PATH"),
        std::env::var("SUMMONER_FENCE_BEGIN"),
        std::env::var("SUMMONER_FENCE_END"),
    ) else {
        return; // only as the parent's child
    };
    let _ = std::fs::metadata(&begin);
    let r = Bundle::check(Path::new(&path), format_of(&path));
    let _ = std::fs::metadata(&end);
    println!("FENCE {:?}", r.map(|b| b.path().display().to_string()));
}

#[test]
fn the_lab_fence_refuses_every_path_out_of_the_lab_before_touching_it() {
    let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
    assert!(fswatch.is_file(), "tools/build-plugins.sh first");
    let s = scratch("fence");
    fence_links(&s);
    let lab = env!("SUMMONER_DEV_PLUGIN_ROOTS");
    for (name, path, expect) in fence_cases(&s) {
        // In-process first (the refusal itself), then under the recorder.
        assert_eq!(
            Bundle::check(Path::new(&path), format_of(&path)).unwrap_err(),
            expect,
            "{name}"
        );
        let log = s.join(format!("{name}.fswatch"));
        let (begin, end) = (
            s.join(format!("{name}.begin")),
            s.join(format!("{name}.end")),
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fence_child", "--nocapture", "--test-threads=1"])
            .env("DYLD_INSERT_LIBRARIES", &fswatch)
            .env("SUMMONER_FSWATCH_LOG", &log)
            .env("SUMMONER_FENCE_PATH", &path)
            .env("SUMMONER_FENCE_BEGIN", &begin)
            .env("SUMMONER_FENCE_END", &end)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.find("FENCE ").map(|i| &l[i..]))
            .unwrap_or_else(|| panic!("{name}: {stdout}"));
        assert_eq!(line, format!("FENCE Err({expect:?})"), "{name}");
        let recs = fswatch_v2::parse(&fswatch_v2::read_log(&log));
        let (bm, em) = (begin.to_string_lossy(), end.to_string_lossy());
        let b = recs
            .iter()
            .position(|r| r.paths.last().is_some_and(|p| *p == *bm))
            .expect("begin marker");
        let e = recs
            .iter()
            .position(|r| r.paths.last().is_some_and(|p| *p == *em))
            .expect("end marker");
        let window = &recs[b + 1..e];
        for r in window {
            let l = &r.raw;
            let locs = r
                .locations()
                .unwrap_or_else(|| panic!("{name}: an unresolvable location (a STOP): {l}"));
            assert!(!r.is_write(), "{name}: a write in the window: {l}");
            for p in &locs {
                assert!(p.starts_with(lab), "{name}: touched outside the lab: {l}");
            }
            assert!(
                !["open", "openat", "fopen", "dlopen"].contains(&r.call.as_str()),
                "{name}: opened: {l}"
            );
        }
        if !name.starts_with("link-") {
            assert!(window.is_empty(), "{name}: refused lexically: {window:?}");
        } else {
            assert!(!window.is_empty(), "{name}: the lab link itself was read");
        }
    }
}
