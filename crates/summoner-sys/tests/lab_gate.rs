//! The lab gate against the file system: a path under the real `~/Library/Audio/Plug-Ins/Components`, a symlink from
//! the lab tree into a plugin-folder-shaped lab directory, a `..` climb out of `target.noindex/plugins`, a symlinked
//! executable, and a hard-linked one. Every one must be refused before any bundle is created, and the file-access
//! recorder (`target.noindex/plugins/libsummoner_lab_fswatch.dylib`, injected into a child run of this test binary) must
//! show that the check touched nothing but the lab's own gate-test tree, between two markers.
//!
//! The fixtures' "plugin folder" is a lab directory named like one (`target.noindex/gate-fixtures-<pid>/Library/Audio/
//! Plug-Ins/Components`), never the real folder, so even a mutant gate touches nothing of the user's. The real-home
//! path is refused lexically, with no syscall, which the recorder proves (no line at all in its window).

// The lab gate's tests: under the release policy (`release-loading`) `Bundle::check` is that policy instead, judged
// by release_loading.rs.
#![cfg(not(feature = "release-loading"))]
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
use std::path::{Path, PathBuf};
use std::process::Command;
use summoner_sys::bundle::{Bundle, Format, Refusal, allowed_root};

#[path = "../../../testkit/plugins/fswatch/fswatch_v2.rs"]
mod fswatch_v2;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// The real home (the lab env points HOME at the lab's): the dev fence (`SUMMONER_DEV_PLUGIN_ROOTS`) is one level
/// below it.
fn real_home() -> String {
    env!("SUMMONER_DEV_PLUGIN_ROOTS")
        .rsplit_once('/')
        .unwrap()
        .0
        .to_string()
}

struct Fixtures {
    fake: PathBuf,
    tree: PathBuf,
    dirlink: PathBuf,
}

impl Drop for Fixtures {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.dirlink);
        let _ = std::fs::remove_dir_all(&self.tree);
        let _ = std::fs::remove_dir_all(self.fake.ancestors().nth(5).unwrap());
    }
}

fn bundle_dirs(b: &Path) -> PathBuf {
    let m = b.join("Contents/MacOS");
    std::fs::create_dir_all(&m).unwrap();
    m
}

/// Builds the fixtures under this repo's target/ for run `tag`.
fn fixtures(tag: &str) -> Fixtures {
    let root = allowed_root(Format::Au).expect("built in the lab env");
    let fake = repo().join(format!(
        "target.noindex/gate-fixtures-{tag}/Library/Audio/Plug-Ins/Components/Fake.component"
    ));
    let fm = bundle_dirs(&fake);
    std::fs::write(fake.join("Contents/Info.plist"), b"<plist/>").unwrap();
    std::fs::write(fm.join("Fake"), b"not a plugin").unwrap();
    let tree = root.join(format!("gate-test-{tag}"));
    // A control that passes the gate: plain directories and single-link files.
    let good = tree.join("good.component");
    let gm = bundle_dirs(&good);
    std::fs::write(good.join("Contents/Info.plist"), b"<plist/>").unwrap();
    std::fs::write(gm.join("good"), b"x").unwrap();
    // The bundle itself a symlink into the plugin-folder-shaped directory.
    std::os::unix::fs::symlink(&fake, tree.join("link.component")).unwrap();
    // A real bundle whose executable is a symlink there.
    let el = tree.join("exe-link.component");
    let em = bundle_dirs(&el);
    std::fs::write(el.join("Contents/Info.plist"), b"<plist/>").unwrap();
    std::os::unix::fs::symlink(fm.join("Fake"), em.join("exe-link")).unwrap();
    // A real bundle whose executable is a hard link of the fake folder's.
    let hl = tree.join("hard.component");
    let hm = bundle_dirs(&hl);
    std::fs::write(hl.join("Contents/Info.plist"), b"<plist/>").unwrap();
    std::fs::hard_link(fm.join("Fake"), hm.join("hard")).unwrap();
    // A symlinked directory on the way down to an otherwise plain bundle.
    let dirlink = root.join(format!("gate-dirlink-{tag}"));
    std::os::unix::fs::symlink(&tree, &dirlink).unwrap();
    Fixtures {
        fake,
        tree,
        dirlink,
    }
}

/// The cases: (name, path, expected refusal or None for the control).
fn cases(f: &Fixtures, tag: &str) -> Vec<(&'static str, String, Option<Refusal>)> {
    let root = allowed_root(Format::Au).unwrap();
    let t = |n: &str| f.tree.join(n).display().to_string();
    vec![
        ("control", t("good.component"), None),
        (
            "real-home",
            format!(
                "{}/Library/Audio/Plug-Ins/Components/Any.component",
                real_home()
            ),
            Some(Refusal::OutsideRoot),
        ),
        (
            "system",
            "/Library/Audio/Plug-Ins/Components/Any.component".into(),
            Some(Refusal::OutsideRoot),
        ),
        // A pin's shape outside the fence: refused lexically like any other path out of the fence (the mutant
        // `mutant_dev_roots_unfenced` takes the shape alone and goes on to lstat it).
        (
            "pin-shape-outside",
            "/Library/bin/test-plugins/0123abcd/au/Any.component".into(),
            Some(Refusal::OutsideRoot),
        ),
        (
            "symlink",
            t("link.component"),
            Some(Refusal::Symlink(f.tree.join("link.component"))),
        ),
        (
            "exe-symlink",
            t("exe-link.component"),
            Some(Refusal::Symlink(
                f.tree.join("exe-link.component/Contents/MacOS/exe-link"),
            )),
        ),
        (
            "hard-link",
            t("hard.component"),
            Some(Refusal::HardLink(
                f.tree.join("hard.component/Contents/MacOS/hard"),
            )),
        ),
        (
            "dotdot",
            format!(
                "{}/../../gate-fixtures-{tag}/Library/Audio/Plug-Ins/Components/Fake.component",
                root.display()
            ),
            Some(Refusal::NotLexical),
        ),
        (
            "dir-symlink",
            root.join(format!("gate-dirlink-{tag}/good.component"))
                .display()
                .to_string(),
            Some(Refusal::Symlink(root.join(format!("gate-dirlink-{tag}")))),
        ),
    ]
}

/// The child half: runs one case's check between two markers (stat of a marker path) and prints the result.
#[test]
fn gate_child() {
    let (Ok(path), Ok(begin), Ok(end)) = (
        std::env::var("SUMMONER_GATE_PATH"),
        std::env::var("SUMMONER_GATE_BEGIN"),
        std::env::var("SUMMONER_GATE_END"),
    ) else {
        return; // only as the parent's child
    };
    let _ = std::fs::metadata(&begin);
    let r = Bundle::check(Path::new(&path), Format::Au);
    let _ = std::fs::metadata(&end);
    println!("GATE {:?}", r.map(|b| b.path().display().to_string()));
}

#[test]
fn every_refusal_is_made_before_any_bundle_is_opened_and_touches_only_the_lab() {
    if lab_plugins::absent(
        "every_refusal_is_made_before_any_bundle_is_opened_and_touches_only_the_lab",
    ) {
        return;
    }
    let fswatch = lab_plugins::dir().join("libsummoner_lab_fswatch.dylib");
    assert!(fswatch.is_file(), "tools/build-plugins.sh first");
    let tag = std::process::id().to_string();
    let f = fixtures(&tag);
    let lab = env!("SUMMONER_DEV_PLUGIN_ROOTS");
    let work = repo().join(format!("target.noindex/gate-fixtures-{tag}"));
    for (name, path, expect) in cases(&f, &tag) {
        let log = work.join(format!("{name}.fswatch"));
        let (begin, end) = (
            work.join(format!("{name}.begin")),
            work.join(format!("{name}.end")),
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gate_child", "--nocapture", "--test-threads=1"])
            .env("DYLD_INSERT_LIBRARIES", &fswatch)
            .env("SUMMONER_FSWATCH_LOG", &log)
            .env("SUMMONER_GATE_PATH", &path)
            .env("SUMMONER_GATE_BEGIN", &begin)
            .env("SUMMONER_GATE_END", &end)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.find("GATE ").map(|i| &l[i..]))
            .unwrap_or_else(|| panic!("{name}: {stdout}"));
        let want = match &expect {
            None => format!("GATE Ok({path:?})"),
            Some(r) => format!("GATE Err({r:?})"),
        };
        assert_eq!(line, want, "{name}");
        // The recorder's window: every path between the markers.
        let text = fswatch_v2::read_log(&log);
        let recs = fswatch_v2::parse(&text);
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
        assert!(
            !window.is_empty() || expect.is_some(),
            "{name}: the control was checked by lstat"
        );
        for r in window {
            let (call, l) = (r.call.as_str(), &r.raw);
            let locs = r
                .locations()
                .unwrap_or_else(|| panic!("{name}: an unresolvable location (a STOP): {l}"));
            assert!(!r.is_write(), "{name}: a write in the gate's window: {l}");
            for p in &locs {
                assert!(p.starts_with(lab), "{name}: touched outside the lab: {l}");
            }
            let p = locs.join("\t");
            assert!(
                !p.contains("gate-fixtures"),
                "{name}: touched the plugin-folder fixture: {l}"
            );
            assert!(
                !["open", "openat", "fopen", "dlopen"].contains(&call),
                "{name}: opened: {l}"
            );
        }
        match name {
            // Refused lexically: no syscall at all.
            "real-home" | "system" | "pin-shape-outside" | "dotdot" => {
                assert!(window.is_empty(), "{name}: {window:?}")
            }
            _ => {}
        }
    }
    drop(f);
}

/// A scratch pin of this test's own: `<pins>/<sha8>/vst3/summoner-lab-gain.vst3`, copied (real copies) from the build
/// output, with a generated manifest whose own sha256 names it, and a marker file (`.lab-gate-scratch`, listed in the
/// manifest, outside the bundle) that makes the name unique and lets a later run find and remove it. Real pins are
/// read-only and carry no marker: never touched.
const MARKER: &str = ".lab-gate-scratch";

fn sha8_like(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn remove_stale_scratch_pins(pins: &Path) {
    let Ok(dir) = std::fs::read_dir(pins) else {
        return;
    };
    for e in dir.filter_map(Result::ok) {
        let p = e.path();
        let name = e.file_name();
        if sha8_like(&name.to_string_lossy())
            && std::fs::symlink_metadata(p.join(MARKER)).is_ok_and(|m| m.is_file())
        {
            std::fs::remove_dir_all(&p).expect("a stale scratch pin");
        }
    }
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

fn manifest_of(root: &Path) -> String {
    fn files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            if e.file_type().unwrap().is_dir() {
                files(&e.path(), out);
            } else {
                out.push(e.path());
            }
        }
    }
    let mut all = Vec::new();
    files(root, &mut all);
    let mut lines: Vec<String> = all
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n != "manifest.sha256"))
        .map(|p| {
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            format!(
                "{}  ./{rel}",
                summoner_sys::sha256::hex(&std::fs::read(p).unwrap())
            )
        })
        .collect();
    lines.sort_by(|a, b| a[66..].cmp(&b[66..]));
    lines.iter().map(|l| format!("{l}\n")).collect()
}

struct ScratchPin(PathBuf);

impl Drop for ScratchPin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch_pin(pins: &Path) -> ScratchPin {
    let stage = repo().join(format!(
        "target.noindex/gate-pin-stage-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&stage);
    copy_tree(
        &lab_plugins::dir().join("vst3/summoner-lab-gain.vst3"),
        &stage.join("vst3/summoner-lab-gain.vst3"),
    );
    let nonce = format!(
        "{} {:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
    );
    std::fs::write(stage.join(MARKER), nonce).unwrap();
    let manifest = manifest_of(&stage);
    std::fs::write(stage.join("manifest.sha256"), &manifest).unwrap();
    let sha8 = &summoner_sys::sha256::hex(manifest.as_bytes())[..8];
    let dest = pins.join(sha8);
    assert!(
        !dest.exists(),
        "a scratch pin's name collides with {}",
        dest.display()
    );
    std::fs::create_dir_all(pins).unwrap();
    std::fs::rename(&stage, &dest).unwrap();
    ScratchPin(dest)
}

/// A line on the real stderr, past libtest's capture (`eprintln!` is captured and hidden when the test passes): a
/// skipped half of the test is never silent.
fn loud(msg: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "lab_gate: {msg}");
}

/// The real pins (read-only) under `pins`, each judged by [`Bundle::check`] as it is: every bundle in each format
/// folder the pin carries (`vst3/`, `au/`) must pass in that format, and a pin must carry at least one. A pin need not
/// carry both formats (an AU-only pin has no `vst3/`): a pin without a format folder is named on the real stderr, as
/// is a run whose real pins carry no VST3 at all, or that finds no real pin (a machine without pins, CI),
/// and the VST3 refusals below are then judged on the scratch pin alone. Returns the bundles checked.
fn check_real_pins(pins: &Path) -> Vec<PathBuf> {
    let mut checked = Vec::new();
    let (mut real, mut with_vst3) = (0, 0);
    let mut entries: Vec<PathBuf> = std::fs::read_dir(pins)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    for pin in entries {
        if pin.join(MARKER).exists() || !pin.join("manifest.sha256").is_file() {
            continue;
        }
        real += 1;
        let before = checked.len();
        for (dir, format) in [("vst3", Format::Vst3), ("au", Format::Au)] {
            let d = pin.join(dir);
            if !d.is_dir() {
                loud(&format!(
                    "SKIPPED {format:?} for real pin {}: it has no {dir}/ folder",
                    pin.display()
                ));
                continue;
            }
            let mut bundles: Vec<PathBuf> = std::fs::read_dir(&d)
                .unwrap_or_else(|e| panic!("{}: {e}", d.display()))
                .map(|e| e.unwrap().path())
                .collect();
            bundles.sort();
            assert!(
                !bundles.is_empty(),
                "a real pin's empty {dir}/: {}",
                d.display()
            );
            for b in bundles {
                Bundle::check(&b, format)
                    .unwrap_or_else(|r| panic!("a real pin's {format:?} {}: {r:?}", b.display()));
                checked.push(b);
            }
            with_vst3 += usize::from(format == Format::Vst3);
        }
        assert!(
            checked.len() > before,
            "a real pin with neither vst3/ nor au/: {}",
            pin.display()
        );
    }
    if real == 0 {
        loud(&format!(
            "SKIPPED the real-pin half: no real pin under {} (the scratch pin alone is judged)",
            pins.display()
        ));
    } else if with_vst3 == 0 {
        loud(&format!(
            "SKIPPED VST3 on real pins: none of the {real} real pin(s) under {} has a vst3/ folder (the scratch pin \
             alone judges VST3)",
            pins.display()
        ));
    }
    checked
}

/// The pinned root: a bundle under `<fence>/bin/test-plugins` is accepted only at `<sha8>/<vst3|au>/<bundle>`, `<sha8>`
/// 8 lowercase hex, and only as the pin's bytes (its manifest names the pin; every bundle file listed and matching,
/// none extra). Each refusal below is made on a scratch pin of this test's own (stale ones removed first); real pins
/// are only read, and each one's gain bundles must pass.
#[test]
fn the_pinned_root_takes_only_a_pins_bytes() {
    if lab_plugins::absent("the_pinned_root_takes_only_a_pins_bytes") {
        return;
    }
    use summoner_sys::bundle::pinned_root;
    let pins = pinned_root().expect("built in the lab env");
    remove_stale_scratch_pins(&pins);
    check_real_pins(&pins);
    let sp = scratch_pin(&pins);
    let pin = sp.0.clone();
    let gain = pin.join("vst3/summoner-lab-gain.vst3");
    let check = || Bundle::check(&gain, Format::Vst3);
    assert_eq!(check().expect("a scratch pin passes").path(), gain);

    // Shapes, lexically: another depth, the other format's dir, a name not 8 lowercase hex.
    let bad = |p: PathBuf| Bundle::check(&p, Format::Vst3).unwrap_err();
    assert_eq!(
        bad(pin.join("summoner-lab-gain.vst3")),
        Refusal::OutsideRoot
    );
    assert_eq!(
        bad(pin.join("au/summoner-lab-gain.vst3")),
        Refusal::OutsideRoot
    );
    for name in ["ABCDEF12", "abcdef1", "abcdef123", "abcdefg1"] {
        assert_eq!(
            bad(pins.join(name).join("vst3/summoner-lab-gain.vst3")),
            Refusal::OutsideRoot,
            "{name}"
        );
    }

    // One changed byte in a bundle file.
    let plist = gain.join("Contents/Info.plist");
    let orig = std::fs::read(&plist).unwrap();
    let mut changed = orig.clone();
    *changed.last_mut().unwrap() ^= 1;
    std::fs::write(&plist, &changed).unwrap();
    assert_eq!(check().unwrap_err(), Refusal::PinMismatch(plist.clone()));
    std::fs::write(&plist, &orig).unwrap();
    check().expect("restored");

    // An extra file; a missing one.
    let extra = gain.join("Contents/extra");
    std::fs::write(&extra, b"x").unwrap();
    assert_eq!(check().unwrap_err(), Refusal::PinMismatch(extra.clone()));
    std::fs::remove_file(&extra).unwrap();
    let aside = pin.join("Info.plist.aside");
    std::fs::rename(&plist, &aside).unwrap();
    assert_eq!(check().unwrap_err(), Refusal::PinMismatch(plist.clone()));
    std::fs::rename(&aside, &plist).unwrap();
    check().expect("restored");

    // The manifest: one that does not name the pin; none at all.
    let manifest = pin.join("manifest.sha256");
    let text = std::fs::read(&manifest).unwrap();
    std::fs::write(&manifest, [&text[..], b"\n"].concat()).unwrap();
    assert_eq!(check().unwrap_err(), Refusal::PinMismatch(manifest.clone()));
    std::fs::remove_file(&manifest).unwrap();
    assert_eq!(check().unwrap_err(), Refusal::PinMismatch(manifest.clone()));
    std::fs::write(&manifest, &text).unwrap();
    check().expect("restored");

    // Links still fail as links, before the pin check: a symlinked bundle, a hard-linked bundle file.
    let link = pin.join("vst3/link.vst3");
    std::os::unix::fs::symlink(&gain, &link).unwrap();
    assert_eq!(
        Bundle::check(&link, Format::Vst3).unwrap_err(),
        Refusal::Symlink(link.clone())
    );
    std::fs::remove_file(&link).unwrap();
    let hard = pin.join("Info.plist.hard");
    std::fs::hard_link(&plist, &hard).unwrap();
    assert_eq!(check().unwrap_err(), Refusal::HardLink(plist.clone()));
    std::fs::remove_file(&hard).unwrap();
    check().expect("restored");
}
