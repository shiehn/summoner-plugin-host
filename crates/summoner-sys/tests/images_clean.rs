//! `bundle::process_images_clean`: an injected image is allowed only if it is named in DYLD_INSERT_LIBRARIES AND
//! lies directly in the build's lab plugin directory AND is one of the two recorders AND was loaded before the first
//! LoadCheck::begin, so the environment alone can never widen the check. Each case runs in a child run of this test
//! binary with its own DYLD_INSERT_LIBRARIES; no plugin is loaded.

#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;
use std::path::{Path, PathBuf};
use std::process::Command;
use summoner_sys::bundle::{Format, allowed_root};

fn plugins() -> PathBuf {
    allowed_root(Format::Au)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[test]
fn images_child() {
    if lab_plugins::absent("images_child") {
        return;
    }
    if std::env::var_os("SUMMONER_IMAGES_CHILD").is_none() {
        return;
    }
    match summoner_sys::bundle::process_images_clean() {
        Ok(()) => println!("IMAGES clean"),
        Err(bad) => println!(
            "IMAGES bad {}",
            bad.0
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn run(inserted: &[&Path]) -> String {
    let joined = inserted
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "images_child", "--nocapture", "--test-threads=1"])
        .env("DYLD_INSERT_LIBRARIES", &joined)
        .env("SUMMONER_IMAGES_CHILD", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|l| l.find("IMAGES ").map(|i| l[i..].to_string()))
        .unwrap_or_else(|| {
            panic!(
                "{joined}: {stdout}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

#[test]
fn only_the_two_recorders_from_the_lab_plugin_dir_are_allowed_injected() {
    if lab_plugins::absent("only_the_two_recorders_from_the_lab_plugin_dir_are_allowed_injected") {
        return;
    }
    let (fs, look, count) = (
        plugins().join("libsummoner_lab_fswatch.dylib"),
        plugins().join("libsummoner_lab_lookwatch.dylib"),
        plugins().join("libsummoner_lab_malloccount.dylib"),
    );
    assert!(
        fs.is_file() && look.is_file() && count.is_file(),
        "tools/build-plugins.sh first"
    );
    // The control: nothing injected, then the two recorders: clean.
    assert_eq!(run(&[]), "IMAGES clean");
    assert_eq!(run(&[&fs, &look]), "IMAGES clean");
    // A third lab dylib under target.noindex/plugins that is not a recorder: refused, by its path.
    let r = run(&[&fs, &look, &count]);
    assert!(
        r.starts_with("IMAGES bad") && r.contains("libsummoner_lab_malloccount.dylib"),
        "{r}"
    );
    // A recorder's own file under another lab directory: refused (not the plugin dir).
    let elsewhere = plugins()
        .parent()
        .unwrap()
        .join(format!("images-test-{}", std::process::id()));
    std::fs::create_dir_all(&elsewhere).unwrap();
    let copy = elsewhere.join("libsummoner_lab_fswatch.dylib");
    std::fs::copy(&fs, &copy).unwrap();
    let r = run(&[&copy, &look]);
    let _ = std::fs::remove_dir_all(&elsewhere);
    assert!(
        r.starts_with("IMAGES bad") && r.contains(&*copy.to_string_lossy()),
        "{r}"
    );
}
