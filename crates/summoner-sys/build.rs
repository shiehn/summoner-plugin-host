//! With the `vst3` feature: compile the VST3 host shim (vst3/shim.cpp) and exactly the SDK files below, by path from
//! the verified SDK root (tools/vst3-sdk.sh refuses a tree that is not its pin). The SDK's own CMake is never run.
//! The SDK's mac module loader (module_mac.mm, which enumerates the system plugin folders) is deliberately absent.
//! On Windows, where a `.sh` cannot be executed, the SDK root comes from `build_support/vst3_sdk.rs` (the script's
//! checks in Rust, with directory junctions for its links).
#[cfg(feature = "vst3")]
use std::path::Path;
#[cfg(all(feature = "vst3", not(windows)))]
use std::process::Command;

#[path = "build_support/dev_roots.rs"]
mod dev_roots;
#[path = "build_support/workspace_root.rs"]
mod workspace_root;

#[cfg(all(feature = "vst3", windows))]
#[path = "src/sha256.rs"]
mod sha256;
#[cfg(all(feature = "vst3", windows))]
#[path = "build_support/vst3_sdk.rs"]
mod vst3_sdk;

/// The SDK files the shim is built with (VENDORED.md names the same list).
#[cfg(feature = "vst3")]
const SDK_FILES: &[&str] = &[
    "pluginterfaces/base/coreiids.cpp",
    "pluginterfaces/base/funknown.cpp",
    "pluginterfaces/base/ustring.cpp",
    "public.sdk/source/common/commoniids.cpp",
    "public.sdk/source/common/commonstringconvert.cpp",
    "public.sdk/source/common/memorystream.cpp",
    "public.sdk/source/vst/vstinitiids.cpp",
    "public.sdk/source/vst/hosting/eventlist.cpp",
    "public.sdk/source/vst/hosting/hostclasses.cpp",
    "public.sdk/source/vst/hosting/pluginterfacesupport.cpp",
    "public.sdk/source/vst/utility/stringconvert.cpp",
];

fn main() {
    let dev = fence().is_some();
    #[cfg(feature = "vst3")]
    vst3();
    #[cfg(feature = "editor")]
    editor(dev);
    #[cfg(not(feature = "editor"))]
    let _ = dev;
}

/// The build-time dev plugin roots (`SUMMONER_DEV_PLUGIN_ROOTS`), through the one validator: a malformed value fails
/// the build here. An accepted fence reaches the crate as `SUMMONER_DEV_FENCE` (bundle.rs reads it with
/// `option_env!`, so the crate never sees an unchecked string); unset, nothing is emitted and there are no dev roots.
fn fence() -> Option<String> {
    println!("cargo:rerun-if-env-changed={}", dev_roots::VAR);
    let fence = dev_roots::dev_roots(std::env::var_os(dev_roots::VAR).as_deref())
        .unwrap_or_else(|e| panic!("refused: {e}"));
    if let Some(f) = &fence {
        println!("cargo:rustc-env=SUMMONER_DEV_FENCE={f}");
        // The dev build roots' base: the workspace root, by its manifest, never by this crate's folder name.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let root = workspace_root::workspace_root(manifest).unwrap_or_else(|| {
            panic!(
                "refused: no [workspace] manifest above {}",
                manifest.display()
            )
        });
        let root = root.to_str().unwrap_or_else(|| {
            panic!(
                "refused: the workspace root is not UTF-8: {}",
                root.display()
            )
        });
        println!("cargo:rustc-env=SUMMONER_DEV_REPO={root}");
    }
    fence
}

/// libsummoner_editor.dylib: editor/editor.mm, Objective-C++ with ARC, AppKit linked into this dylib
/// only, into OUT_DIR; its absolute path reaches editor.rs as SUMMONER_EDITOR_DYLIB.
#[cfg(feature = "editor")]
fn editor(dev: bool) {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = manifest.join("editor/editor.mm");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap())
        .join("libsummoner_editor.dylib");
    let st = std::process::Command::new("/usr/bin/clang++")
        .args([
            "-std=c++17",
            "-fobjc-arc",
            "-dynamiclib",
            "-mmacosx-version-min=11.0",
            "-O2",
            "-Wall",
        ])
        .args([
            "-framework",
            "AppKit",
            "-framework",
            "AudioToolbox",
            "-framework",
            "CoreAudioKit",
            "-install_name",
            "@rpath/libsummoner_editor.dylib",
            "-o",
        ])
        .arg(&out)
        .arg(&src)
        .status()
        .unwrap_or_else(|e| panic!("cannot run clang++: {e}"));
    assert!(st.success(), "clang++ failed on {}", src.display());
    // Only a dev build (an accepted SUMMONER_DEV_PLUGIN_ROOTS, the same answer as the fence's) compiles the build
    // path in, as editor.rs's fallback. A release build gets an empty path, so no build-machine path is in its binary:
    // it loads the dylib beside the executable (tools/build-shipped.sh puts it there) or has no editor.
    let path = if dev {
        out.display().to_string()
    } else {
        String::new()
    };
    println!("cargo:rustc-env=SUMMONER_EDITOR_DYLIB={path}");
    println!("cargo:rerun-if-changed={}", src.display());
}

#[cfg(feature = "vst3")]
fn vst3() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.join("../..");
    let sdk = sdk_root(&repo);
    let mut b = cc::Build::new();
    b.cpp(true)
        .std("c++17")
        .include(&sdk)
        .include(manifest.join("vst3"))
        .define("RELEASE", "1")
        // rustc links aarch64-apple-darwin for macOS 11.0; the shim matches it.
        .flag("-mmacosx-version-min=11.0")
        .warnings(false)
        .file(manifest.join("vst3/shim.cpp"));
    for f in SDK_FILES {
        b.file(Path::new(&sdk).join(f));
    }
    b.compile("summoner_vst3_shim");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-env=SUMMONER_VST3_SDK_ROOT={sdk}");
    for p in [
        "vst3",
        "../../testkit/plugins/vst3-sdk.pins",
        "../../tools/vst3-sdk.sh",
    ] {
        println!("cargo:rerun-if-changed={}", manifest.join(p).display());
    }
    println!("cargo:rerun-if-env-changed=SUMMONER_VST3_SDK_CACHE");
}

/// The verified SDK include root, printed by tools/vst3-sdk.sh.
#[cfg(all(feature = "vst3", not(windows)))]
fn sdk_root(repo: &Path) -> String {
    let script = repo.join("tools/vst3-sdk.sh");
    let out = Command::new(&script)
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "{} refused the SDK (set SUMMONER_VST3_SDK_CACHE; tools/build-plugins.sh --fetch fetches it by pin): {}",
        script.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The verified SDK include root, from the same checks in Rust (build_support/vst3_sdk.rs).
#[cfg(all(feature = "vst3", windows))]
fn sdk_root(repo: &Path) -> String {
    match vst3_sdk::root(repo) {
        Ok(p) => p.display().to_string(),
        Err(e) => panic!(
            "the VST3 SDK was refused (set SUMMONER_VST3_SDK_CACHE; tools/fetch-vst3-subs.sh --superproject in Git Bash): {e}"
        ),
    }
}
