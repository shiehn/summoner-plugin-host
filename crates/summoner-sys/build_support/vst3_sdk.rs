//! `tools/vst3-sdk.sh` in Rust, for Windows build hosts (a `.sh` is not a Win32 executable). `build.rs` uses it on
//! Windows only; elsewhere it still runs the script. The same checks, in the same order, with the same messages:
//!
//! - the cache: `SUMMONER_VST3_SDK_CACHE`, required (unset is a refusal). On Windows it must be an absolute
//!   Windows path (an MSYS `/c/...` path is refused: `cl.exe` cannot read it);
//! - for every line of `testkit/plugins/vst3-sdk.pins` (name, repository, commit, licence sha256): the fetch directory
//!   `.fetch-*<-name>` (none for the superproject, which also skips the three submodule suffixes) holding a `.git`
//!   directory whose `HEAD` is the pinned commit (the last such in name order, as the shell glob gives); none is a
//!   refusal;
//! - its `LICENSE.txt`'s sha256 must be the pinned one;
//! - each submodule is linked into `<cache>/root/<name>`, the include root the SDK's own `#include`s expect. Windows
//!   has no symlink without Developer Mode or a privilege, so the link is a directory junction (`mklink /J`). An
//!   existing junction to another target is replaced; anything at that path that is not a junction (a copied tree,
//!   a file) is a refusal, never deleted: it is not ours to remove.
//!
//! Reads files only; no git command, nothing in the fetched trees is executed. Returns the root, a Windows path.

use std::path::{Path, PathBuf};

const SUBMODULES: [&str; 3] = ["pluginterfaces", "base", "public.sdk"];

pub fn root(repo: &Path) -> Result<PathBuf, String> {
    let cache = match std::env::var_os("SUMMONER_VST3_SDK_CACHE").filter(|v| !v.is_empty()) {
        Some(c) => PathBuf::from(c),
        None => {
            return Err(
                "SUMMONER_VST3_SDK_CACHE is unset: set it to the VST3 SDK fetch cache (tools/fetch-vst3-subs.sh)"
                    .into(),
            );
        }
    };
    if !cache.is_absolute() {
        return Err(format!(
            "vst3-sdk: the cache {} is not an absolute Windows path (an MSYS /c/... path cannot reach cl.exe)",
            cache.display()
        ));
    }
    let out = cache.join("root");
    std::fs::create_dir_all(&out).map_err(|e| format!("vst3-sdk: {}: {e}", out.display()))?;
    let pins_path = repo.join("testkit/plugins/vst3-sdk.pins");
    let pins = std::fs::read_to_string(&pins_path)
        .map_err(|e| format!("vst3-sdk: {}: {e}", pins_path.display()))?;
    // Fail closed, as the script: the pins name the four trees once each, or nothing is verified and nothing printed.
    let mut names: Vec<&str> = pins
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| !n.starts_with('#'))
        .collect();
    names.sort_unstable();
    if names != ["base", "pluginterfaces", "public.sdk", "vst3sdk"] {
        return Err(format!(
            "vst3-sdk: {} must pin vst3sdk, pluginterfaces, base and public.sdk once each",
            pins_path.display()
        ));
    }
    let mut fetches: Vec<String> = std::fs::read_dir(&cache)
        .map_err(|e| format!("vst3-sdk: {}: {e}", cache.display()))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(".fetch-"))
        .collect();
    fetches.sort();
    for line in pins.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let Some(&name) = f.first() else { continue };
        if name.starts_with('#') {
            continue;
        }
        let (Some(&sha), Some(&lic)) = (f.get(2), f.get(3)) else {
            return Err(format!(
                "vst3-sdk: a pins line without a commit and a licence hash: {line}"
            ));
        };
        let superproject = name == "vst3sdk";
        let suffix = if superproject {
            String::new()
        } else {
            format!("-{name}")
        };
        let mut dir = None;
        for d in &fetches {
            // The glob `.fetch-*<suffix>`: the suffix after the `.fetch-` prefix (`.fetch-base` is not `.fetch-*-base`).
            if !d[".fetch-".len()..].ends_with(&suffix) {
                continue;
            }
            if superproject && SUBMODULES.iter().any(|s| d.ends_with(&format!("-{s}"))) {
                continue;
            }
            let p = cache.join(d);
            if !p.join(".git").is_dir() {
                continue;
            }
            let head = std::fs::read_to_string(p.join(".git/HEAD")).unwrap_or_default();
            if head.trim_end_matches('\n') == sha {
                dir = Some(p);
            }
        }
        let Some(dir) = dir else {
            return Err(format!(
                "vst3-sdk: no fetched {name} at {sha} under {} (tools/fetch-vst3-subs.sh)",
                cache.display()
            ));
        };
        let licence = std::fs::read(dir.join("LICENSE.txt"))
            .map_err(|e| format!("vst3-sdk: {name} LICENSE.txt: {e}"))?;
        let got = crate::sha256::hex(&licence);
        if got != lic {
            return Err(format!(
                "vst3-sdk: {name} LICENSE.txt is {got}, pinned {lic}"
            ));
        }
        if !superproject {
            junction(&out.join(name), &dir)?;
        }
    }
    Ok(out)
}

/// `link` is a directory junction to `target` when this returns.
fn junction(link: &Path, target: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    };
    match std::fs::symlink_metadata(link) {
        Ok(m) if m.file_type().is_symlink() => {
            if same(link, target) {
                return Ok(());
            }
            // A junction to another fetch: remove the junction itself (remove_dir never touches its target).
            std::fs::remove_dir(link).map_err(|e| {
                format!("vst3-sdk: cannot replace the link {}: {e}", link.display())
            })?;
        }
        Ok(_) => {
            return Err(format!(
                "vst3-sdk: {} exists and is not a link (a copied tree?); remove it by hand, it is not removed here",
                link.display()
            ));
        }
        Err(_) => {}
    }
    let (l, t) = (link.display().to_string(), target.display().to_string());
    if l.contains('"') || t.contains('"') {
        return Err(format!("vst3-sdk: a quote in a path: {l} / {t}"));
    }
    let out = std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(format!("mklink /J \"{l}\" \"{t}\""))
        .output()
        .map_err(|e| format!("vst3-sdk: cannot run cmd for mklink: {e}"))?;
    if !out.status.success() || !same(link, target) {
        return Err(format!(
            "vst3-sdk: mklink /J {l} {t} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(())
}
