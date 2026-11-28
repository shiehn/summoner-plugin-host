//! The post-load image check's portable types: which images are loaded into this process, which of
//! them are the system's own, and the foreign ones a check found. The policy ([`crate::bundle::LoadCheck`],
//! [`crate::bundle::process_images_clean`]) is the same on every OS; only the enumeration and the system rule
//! differ (macOS: dyld's list, `/System/Library/` and `/usr/lib/`; the Windows bodies are the Windows port's).
use std::path::{Path, PathBuf};

/// The paths of every image loaded into this process, in the loader's order (macOS: [`crate::dyld::images`]).
pub fn loaded() -> Vec<PathBuf> {
    crate::dyld::images()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

/// Whether `p` is one of the system's own images, by its path's text (macOS: under `/System/Library/` or `/usr/lib/`;
/// the caller also requires a plain path). The mutation check (never in a normal build): any `/usr/` path passes.
/// `only_the_systems_own_folders_are_system_images` must fail.
pub fn is_system_image(p: &Path) -> bool {
    let usr = if cfg!(mutant_image_system_any_usr) {
        "/usr/"
    } else {
        "/usr/lib/"
    };
    p.to_str()
        .is_some_and(|s| s.starts_with("/System/Library/") || s.starts_with(usr))
}

/// The images a check found where they should not be, as their paths. `Display` is the list's text as the STOP and
/// REPORT lines have always written it (`a, b`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignImages(pub Vec<PathBuf>);

impl std::fmt::Display for ForeignImages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.0.iter().map(|p| p.display().to_string()).collect();
        f.write_str(&names.join(", "))
    }
}

impl ForeignImages {
    /// Whether it names no image.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_systems_own_folders_are_system_images() {
        assert!(is_system_image(Path::new("/usr/lib/libSystem.B.dylib")));
        assert!(is_system_image(Path::new(
            "/System/Library/Frameworks/AppKit.framework/AppKit"
        )));
        assert!(!is_system_image(Path::new("/usr/local/lib/libx.dylib")));
        assert!(!is_system_image(Path::new("/usr/libexec/x")));
        assert!(!is_system_image(Path::new(
            "/Library/Audio/Plug-Ins/x.vst3/Contents/MacOS/x"
        )));
        assert_eq!(
            ForeignImages(vec!["/a/b.dylib".into(), "/c.dylib".into()]).to_string(),
            "/a/b.dylib, /c.dylib"
        );
    }

    #[test]
    fn the_loaded_list_holds_this_test_binary() {
        let me = std::env::current_exe().unwrap();
        assert!(loaded().contains(&me));
    }
}
