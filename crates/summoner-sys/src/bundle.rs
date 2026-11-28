//! Only the development build's own plugin bundles (the development gate). An absolute path is not by itself a development plugin:
//! [`Bundle::check`] runs BEFORE any bundle is created or any file of it is opened, and the format loaders take only
//! a [`Bundle`] (the VST3 shim's module open, the AU registration).
//!
//! - **The pinned root:** also `<fence>/bin/test-plugins`, compiled in the same way,
//!   for read-only, sha-pinned copies of the test plugins (every test loads the same bytes);
//!   a bundle there only at `<sha8>/<vst3|au>/<name>.<ext>`, `<sha8>` exactly 8 lowercase hex digits, under every rule
//!   below, and then only if it is the pin's bytes: `<sha8>/manifest.sha256` a plain single-link file
//!   whose own sha256 starts with `<sha8>`, and every regular file of the bundle listed in it with a matching sha256,
//!   none missing, none extra ([`Refusal::PinMismatch`]), checked after the lstat walk and before the bundle is opened.
//! - **The allowed roots** are fixed at build time: this repo's `target.noindex/plugins/au` and
//!   `target.noindex/plugins/vst3` (from `CARGO_MANIFEST_DIR`), and they must lie lexically under the
//!   `SUMMONER_DEV_PLUGIN_ROOTS` this crate was built with. Nothing is read from the run-time
//!   environment, so no environment can widen them. Built without `SUMMONER_DEV_PLUGIN_ROOTS`, there are no roots and
//!   every bundle is refused.
//! - **Lexical first, no syscall:** absolute, no `.` / `..` / empty segment, the format's extension, under a root
//!   segment by segment. A path into `~/Library/Audio/Plug-Ins`, `/Library/…` or `/System/…` is refused here with
//!   nothing touched.
//! - **No symlink anywhere, none followed:** `lstat` of every segment from `SUMMONER_DEV_PLUGIN_ROOTS` down to the
//!   bundle, then of every entry inside the bundle (a walk of the development tree). Stricter than `realpath` on purpose:
//!   `realpath` would follow a symlink inside the fence and stat its target, a touch inside the user's folders. With no symlink
//!   present the lexical path is the canonical one.
//! - **No hard link:** `lstat` cannot tell one from a file, so every regular file in the bundle
//!   (the executable, `Info.plist`) must have `st_nlink == 1`.
//! - **After the load** ([`LoadCheck`]): the images dyld loaded must lie inside the bundle or the system's own
//!   (`/System/Library/`, `/usr/lib/`, plain paths only), and the bundle's executable must be among the loaded images.
//!   The same diff wraps an AU's instantiation and its `AudioUnitInitialize`. A mismatch (a swap between the check and
//!   the load, or plugin code that loads a foreign image) is a not-ok slot and a STOP: stderr, and [`stops`] counts it
//!   (the tests assert 0). After a STOP every further [`LoadCheck::begin`] is refused, so no plugin code runs again in
//!   that process. A shipped build (`release-loading`, no `SUMMONER_DEV_PLUGIN_ROOTS`) reports a foreign image instead
//!   and loads ([`foreign_images`]); the missing-executable case stays a STOP there too.
//!
//! - **The release policy** (the `release-loading` feature, OFF by default, so every development build keeps the development gate above
//!   exactly): [`Bundle::check`] runs [`Bundle::check_release`] instead: plain and absolute, the format's
//!   extension, symlinks followed with each hop checked before it is touched, the resolved path a directory; in a build
//!   made with `SUMMONER_DEV_PLUGIN_ROOTS`, fenced lexically to the development fence and the development gate's walk of the inside. The
//!   post-load check below is kept. Turning it on for real plugin folders is a separate, deliberate decision.
//!
//! No `unsafe` in this module: `std::fs::symlink_metadata` is the `lstat`; the image list is [`crate::dyld`].
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The two plugin formats and their roots under `target.noindex/plugins`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Format {
    /// An Audio Unit v2 `.component` bundle.
    Au,
    /// A VST3 `.vst3` bundle.
    Vst3,
}

impl Format {
    fn dir(self) -> &'static str {
        match self {
            Format::Au => "au",
            Format::Vst3 => "vst3",
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Format::Au => ".component",
            Format::Vst3 => ".vst3",
        }
    }
}

/// Why a path is not a development bundle. Every refusal happens before anything of the bundle is opened.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// No `SUMMONER_DEV_PLUGIN_ROOTS` at build time, or this repo is not under it: no roots at all.
    NoRoots,
    /// Not absolute, a `.` / `..` / empty segment, or not valid UTF-8.
    NotLexical,
    /// Not under this format's root, or without its extension.
    OutsideRoot,
    /// A symlink at this path (inside the development fence: nothing was followed).
    Symlink(PathBuf),
    /// A regular file with more than one link.
    HardLink(PathBuf),
    /// A segment that is missing, or not a directory / regular file where one is needed.
    NotABundle(PathBuf),
    /// Under the pinned root, but not a pin's bytes: no `manifest.sha256` (or not a plain file), one
    /// whose own sha256 does not start with the pin's name, or a bundle file missing from it, extra, or changed.
    PinMismatch(PathBuf),
    /// The release policy: the last segment lacks the format's extension (the given path or the resolved one).
    #[cfg(feature = "release-loading")]
    Extension(PathBuf),
    /// The release policy in a build made with `SUMMONER_DEV_PLUGIN_ROOTS` (the development fence): this path, or a link's
    /// target on the way, is outside the development fence. Refused lexically, before it is touched.
    #[cfg(feature = "release-loading")]
    OutsideLab(PathBuf),
    /// The release policy: more than [`MAX_LINKS`] symlinks on the way (a loop).
    #[cfg(feature = "release-loading")]
    LinkLoop(PathBuf),
}

/// What a loader's refusal starts with: the development gate's text by default, the release policy's under `release-loading`.
#[cfg(not(feature = "release-loading"))]
pub const REFUSED: &str = "not a development plugin bundle";
/// What a loader's refusal starts with: the development gate's text by default, the release policy's under `release-loading`.
#[cfg(feature = "release-loading")]
pub const REFUSED: &str = "not a loadable plugin bundle";

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::NoRoots => write!(
                f,
                "no dev plugin roots (built without SUMMONER_DEV_PLUGIN_ROOTS)"
            ),
            Refusal::NotLexical => write!(f, "not a plain absolute path"),
            Refusal::OutsideRoot => write!(f, "not under the lab's plugin output"),
            Refusal::Symlink(p) => write!(f, "a symlink: {}", p.display()),
            Refusal::HardLink(p) => write!(f, "a hard-linked file: {}", p.display()),
            Refusal::NotABundle(p) => write!(f, "not a bundle entry: {}", p.display()),
            Refusal::PinMismatch(p) => write!(f, "not the pin's bytes: {}", p.display()),
            #[cfg(feature = "release-loading")]
            Refusal::Extension(p) => write!(f, "not a bundle of this format: {}", p.display()),
            #[cfg(feature = "release-loading")]
            Refusal::OutsideLab(p) => write!(
                f,
                "outside the lab (a lab build of the release policy): {}",
                p.display()
            ),
            #[cfg(feature = "release-loading")]
            Refusal::LinkLoop(p) => write!(f, "too many symlinks: {}", p.display()),
        }
    }
}

/// A bundle path that passed [`Bundle::check`]: the only thing the format loaders accept. It is the **bundle**, never
/// its executable: the module body resolves the binary inside it (macOS: the CFBundle's executable; Windows:
/// `<name>.vst3\Contents\<arch>-win\<name>.vst3`, or a legacy single-file `.vst3` DLL, which is its own image).
/// The type is the release policy's too ([`Bundle::check_release`]), not only the development gate's.
#[derive(Clone, Debug)]
pub struct Bundle {
    path: PathBuf,
    format: Format,
}

/// The dev fence this crate was built with: the one `SUMMONER_DEV_PLUGIN_ROOTS` value `build.rs` accepted
/// (`build_support/dev_roots.rs`), compiled in as `SUMMONER_DEV_FENCE`; `None` in a build without it (every release
/// build). Never read from the run-time environment.
pub fn dev_fence() -> Option<&'static str> {
    option_env!("SUMMONER_DEV_FENCE").filter(|r| lexical_segments(r).is_some())
}

/// The pinned test plugins: `<fence>/bin/test-plugins`, compiled in like the build roots, where
/// each read-only, self-contained pin is put. A pinned bundle is accepted only at exactly
/// `<pins>/<sha8>/<vst3|au>/<name>.<ext>`, its format matching, under every rule the build roots have.
pub fn pinned_root() -> Option<PathBuf> {
    dev_fence().map(|l| PathBuf::from(format!("{l}/bin/test-plugins")))
}

/// The format's allowed root: `<repo>/target.noindex/plugins/<au|vst3>`, if it lies under the build's
/// `SUMMONER_DEV_PLUGIN_ROOTS`.
pub fn allowed_root(format: Format) -> Option<PathBuf> {
    allowed_root_in(dev_fence()?, dev_repo()?, format)
}

/// The workspace root this crate was built in (a dev build only: `build.rs` finds it by the nearest `[workspace]`
/// manifest, `build_support/workspace_root.rs`, so it never depends on this crate's folder name); `None` in a build
/// without the dev fence.
pub fn dev_repo() -> Option<&'static str> {
    option_env!("SUMMONER_DEV_REPO").filter(|r| lexical_segments(r).is_some())
}

/// [`allowed_root`], pure: `<repo>/target.noindex/plugins/<au|vst3>` if it lies lexically under `fence`, else `None`
/// (a repo outside the fence has no build roots at all).
pub fn allowed_root_in(fence: &str, repo: &str, format: Format) -> Option<PathBuf> {
    let root = format!("{repo}/target.noindex/plugins/{}", format.dir());
    let (l, r) = (lexical_segments(fence)?, lexical_segments(&root)?);
    (r.len() > l.len() && (unfenced() || r[..l.len()] == l[..])).then(|| PathBuf::from(root))
}

/// The required mutant of the fence (`RUSTFLAGS="--cfg mutant_dev_roots_unfenced"`, never an environment variable):
/// every fence comparison is skipped. `a_root_outside_the_dev_fence_is_no_root` and
/// `a_plugin_root_is_exactly_a_pin_and_admits_only_its_vst3_bundles` must turn red.
const fn unfenced() -> bool {
    cfg!(mutant_dev_roots_unfenced)
}

/// The segments of a plain absolute path, or `None` (relative, `.` / `..` / empty segments, a trailing `/`).
fn lexical_segments(p: &str) -> Option<Vec<&str>> {
    let rest = p.strip_prefix('/')?;
    let segs: Vec<&str> = rest.split('/').collect();
    segs.iter()
        .all(|s| !s.is_empty() && *s != "." && *s != "..")
        .then_some(segs)
}

/// `lstat` of `p`: a directory (`dir`) or a regular file with one link (`!dir`), never a symlink.
fn plain(p: &Path, dir: bool) -> Result<(), Refusal> {
    let m = std::fs::symlink_metadata(p).map_err(|_| Refusal::NotABundle(p.into()))?;
    let t = m.file_type();
    if t.is_symlink() {
        return Err(Refusal::Symlink(p.into()));
    }
    if dir {
        return if t.is_dir() {
            Ok(())
        } else {
            Err(Refusal::NotABundle(p.into()))
        };
    }
    if !t.is_file() {
        return Err(Refusal::NotABundle(p.into()));
    }
    if m.nlink() != 1 {
        return Err(Refusal::HardLink(p.into()));
    }
    Ok(())
}

/// Every entry under `dir` (a directory already checked): directories or single-link regular files, no symlink.
fn walk(dir: &Path, depth: u32) -> Result<(), Refusal> {
    if depth > 16 {
        return Err(Refusal::NotABundle(dir.into()));
    }
    let entries = std::fs::read_dir(dir).map_err(|_| Refusal::NotABundle(dir.into()))?;
    for e in entries {
        let e = e.map_err(|_| Refusal::NotABundle(dir.into()))?;
        let p = e.path();
        let t = e.file_type().map_err(|_| Refusal::NotABundle(p.clone()))?;
        if t.is_symlink() {
            return Err(Refusal::Symlink(p));
        }
        if t.is_dir() {
            plain(&p, true)?;
            walk(&p, depth + 1)?;
        } else {
            plain(&p, false)?;
        }
    }
    Ok(())
}

/// A pin's name: exactly 8 lowercase hex digits.
fn is_sha8(s: &str) -> bool {
    s.len() == 8
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Every regular file in `dir` (already walked: no links), as paths relative to `base`, sorted.
fn files_under(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> Result<(), Refusal> {
    let entries = std::fs::read_dir(dir).map_err(|_| Refusal::NotABundle(dir.into()))?;
    for e in entries {
        let e = e.map_err(|_| Refusal::NotABundle(dir.into()))?;
        let p = e.path();
        let t = e.file_type().map_err(|_| Refusal::NotABundle(p.clone()))?;
        if t.is_dir() {
            files_under(&p, base, out)?;
        } else if t.is_file() {
            out.push(
                p.strip_prefix(base)
                    .map_err(|_| Refusal::NotABundle(p.clone()))?
                    .into(),
            );
        } else {
            return Err(Refusal::NotABundle(p));
        }
    }
    Ok(())
}

/// The entries of a pin's manifest, as `shasum -a 256` writes them (`shasum -a 256` run inside the pin): one line per
/// file, `<64 lowercase hex>  ./<path>`, every line ended by `\n` (the last one may lack it). Lines are split on `\n`
/// ONLY: a file name may end in `\r` (Finder's `Icon\r`, in real bundles), which `str::lines` would strip. A name
/// with a `\` or a `\n` in it is written escaped: the line starts with `\`, and in its path `\\` is a backslash and
/// `\n` a newline (any other escape is malformed); an unescaped line with a `\` in it, and a path listed twice, are
/// malformed too (shasum writes neither). `None` for a malformed manifest: the pin check then refuses.
fn manifest_entries(text: &str) -> Option<Vec<(&str, String)>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for line in text.split_terminator('\n') {
        // `--cfg mutant_manifest_strips_cr` (never in a normal build): the old `lines()` read, a trailing `\r` cut.
        #[cfg(mutant_manifest_strips_cr)]
        let line = line.strip_suffix('\r').unwrap_or(line);
        let (escaped, line) = match line.strip_prefix('\\') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let (sha, path) = line.split_once("  ")?;
        if sha.len() != 64
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let path = path.strip_prefix("./")?;
        let path = if escaped {
            let mut p = String::with_capacity(path.len());
            let mut chars = path.chars();
            while let Some(c) = chars.next() {
                p.push(if c != '\\' {
                    c
                } else {
                    match chars.next()? {
                        '\\' => '\\',
                        'n' => '\n',
                        _ => return None,
                    }
                });
            }
            p
        } else if path.contains('\\') && !cfg!(mutant_manifest_literal_backslash) {
            // `--cfg mutant_manifest_literal_backslash` (never in a normal build): a literal `\` accepted.
            // shasum escapes every name with a backslash in it: a literal one here was not written by it.
            return None;
        } else {
            path.to_owned()
        };
        // `--cfg mutant_manifest_dup_accepted` (never in a normal build): a second listing accepted.
        if path.is_empty() || (!seen.insert(path.clone()) && !cfg!(mutant_manifest_dup_accepted)) {
            // A name listed twice (shasum lists each file once): which sha would be meant is not ours to guess.
            return None;
        }
        out.push((sha, path));
    }
    Some(out)
}

/// The pin check: `<pin>/manifest.sha256` is a plain single-link file whose own sha256 starts with the pin's name, and
/// the bundle's files are exactly its entries under the bundle, each with a matching sha256.
fn verify_pin(pin: &Path, bundle: &Path) -> Result<(), Refusal> {
    let name = pin
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or(Refusal::OutsideRoot)?;
    let manifest = pin.join("manifest.sha256");
    plain(&manifest, false).map_err(|e| match e {
        Refusal::NotABundle(p) => Refusal::PinMismatch(p),
        e => e,
    })?;
    let text = std::fs::read(&manifest).map_err(|_| Refusal::PinMismatch(manifest.clone()))?;
    if !crate::sha256::hex(&text).starts_with(name) {
        return Err(Refusal::PinMismatch(manifest));
    }
    let text = String::from_utf8(text).map_err(|_| Refusal::PinMismatch(manifest.clone()))?;
    let rel = bundle.strip_prefix(pin).map_err(|_| Refusal::OutsideRoot)?;
    let mut want = std::collections::BTreeMap::new();
    for (sha, path) in manifest_entries(&text).ok_or(Refusal::PinMismatch(manifest.clone()))? {
        let path = PathBuf::from(path);
        if path.starts_with(rel) {
            want.insert(path, sha);
        }
    }
    let mut have = Vec::new();
    files_under(bundle, pin, &mut have)?;
    for f in &have {
        let full = pin.join(f);
        let sha = want
            .remove(f.as_path())
            .ok_or(Refusal::PinMismatch(full.clone()))?;
        let bytes = std::fs::read(&full).map_err(|_| Refusal::PinMismatch(full.clone()))?;
        if crate::sha256::hex(&bytes) != sha {
            return Err(Refusal::PinMismatch(full));
        }
    }
    match want.into_keys().next() {
        Some(missing) => Err(Refusal::PinMismatch(pin.join(missing))),
        None => Ok(()),
    }
}

impl Bundle {
    /// The gate every loader runs before anything of a bundle is opened: the development gate ([`Self::check_dev`]) by
    /// default, the release policy ([`Self::check_release`]) in a build with the `release-loading` feature.
    #[cfg(not(feature = "release-loading"))]
    pub fn check(path: &Path, format: Format) -> Result<Bundle, Refusal> {
        Self::check_dev(path, format)
    }

    /// The gate every loader runs (see the default build's [`Self::check`]): here the release policy.
    #[cfg(feature = "release-loading")]
    pub fn check(path: &Path, format: Format) -> Result<Bundle, Refusal> {
        Self::check_release(path, format)
    }

    /// The development gate. `path` is checked lexically first (no syscall), then by `lstat` inside the development fence only.
    pub fn check_dev(path: &Path, format: Format) -> Result<Bundle, Refusal> {
        let lab = dev_fence().ok_or(Refusal::NoRoots)?;
        let root = allowed_root(format).ok_or(Refusal::NoRoots)?;
        let s = path.to_str().ok_or(Refusal::NotLexical)?;
        let segs = lexical_segments(s).ok_or(Refusal::NotLexical)?;
        let root_s = root.to_str().ok_or(Refusal::NoRoots)?;
        let rsegs = lexical_segments(root_s).ok_or(Refusal::NoRoots)?;
        let named = segs
            .last()
            .is_some_and(|l| l.ends_with(format.extension()) && l.len() > format.extension().len());
        let under = |r: &[&str]| segs.len() > r.len() && segs[..r.len()] == r[..];
        let in_build = under(&rsegs);
        // The pinned root: exactly <pins>/<sha8>/<vst3|au>/<bundle>, the format dir matching.
        let pins = pinned_root().ok_or(Refusal::NoRoots)?;
        let psegs =
            lexical_segments(pins.to_str().ok_or(Refusal::NoRoots)?).ok_or(Refusal::NoRoots)?;
        let pin_base = if unfenced() {
            let n = segs.len();
            (n > 5 && segs[n - 5..n - 3] == ["bin", "test-plugins"]).then(|| n - 3)
        } else {
            (under(&psegs) && segs.len() == psegs.len() + 3).then_some(psegs.len())
        };
        let in_pin = pin_base.is_some_and(|b| is_sha8(segs[b]) && segs[b + 1] == format.dir());
        if !named || !(in_build || in_pin) {
            return Err(Refusal::OutsideRoot);
        }
        // Lexically inside the development fence: now lstat each segment from SUMMONER_DEV_PLUGIN_ROOTS down (all inside the development fence
        // tree).
        let lsegs = lexical_segments(lab).ok_or(Refusal::NoRoots)?;
        let mut p = PathBuf::from("/");
        for (i, seg) in segs.iter().enumerate() {
            p.push(seg);
            if i + 1 >= lsegs.len() {
                plain(&p, true)?;
            }
        }
        walk(&p, 0)?;
        if !in_build {
            // A pin: after the lstat walk (so a link is still refused as one), before anything opens the
            // bundle as a bundle, every file matches the pin's manifest.
            let pin = p
                .parent()
                .and_then(Path::parent)
                .ok_or(Refusal::OutsideRoot)?;
            verify_pin(pin, &p)?;
        }
        Ok(Bundle { path: p, format })
    }

    /// The release policy: plain and absolute, the format's extension, the development fence (a build made with
    /// `SUMMONER_DEV_PLUGIN_ROOTS` only), every symlink on the way followed with its target checked before it is
    /// touched, the resolved path a directory with the extension, and in a development build the development gate's walk of the inside.
    /// A shipped build (no `SUMMONER_DEV_PLUGIN_ROOTS`) does not walk the inside: a link or hard link inside a shipped
    /// bundle is not refused.
    #[cfg(feature = "release-loading")]
    pub fn check_release(path: &Path, format: Format) -> Result<Bundle, Refusal> {
        Bundle::check_release_in(path, format, dev_fence())
    }

    /// [`Bundle::check_release`] for the development root `lab` (the compiled-in one; `None` is a shipped build). Private:
    /// only the unit tests pass anything but [`lab_root`], so a development build can judge the shipped variant on lab paths.
    #[cfg(feature = "release-loading")]
    fn check_release_in(path: &Path, format: Format, lab: Option<&str>) -> Result<Bundle, Refusal> {
        let s = path.to_str().ok_or(Refusal::NotLexical)?;
        let segs = lexical_segments(s).ok_or(Refusal::NotLexical)?;
        if !has_extension(segs.last().copied().unwrap_or_default(), format) {
            return Err(Refusal::Extension(path.into()));
        }
        let fence: Option<Vec<&str>> = lab.and_then(lexical_segments);
        let compare = fence.as_deref().filter(|_| !unfenced());
        if let Some(f) = compare
            && !(segs.len() > f.len() && segs[..f.len()] == f[..])
        {
            return Err(Refusal::OutsideLab(path.into()));
        }
        let p = resolve(&segs, compare)?;
        let leaf = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !has_extension(leaf, format) {
            return Err(Refusal::Extension(p));
        }
        plain(&p, true)?;
        if fence.is_some() {
            walk(&p, 0)?;
        }
        Ok(Bundle { path: p, format })
    }

    /// The checked bundle's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The bundle's format.
    pub fn format(&self) -> Format {
        self.format
    }
}

/// The most symlinks [`resolve`] follows for one path (as the kernel's `MAXSYMLINKS`).
#[cfg(feature = "release-loading")]
pub const MAX_LINKS: u32 = 32;

/// The format's extension on `leaf`, ASCII case-insensitive, with a name
/// before it.
#[cfg(feature = "release-loading")]
fn has_extension(leaf: &str, format: Format) -> bool {
    let ext = format.extension();
    leaf.len() > ext.len()
        && leaf.is_char_boundary(leaf.len() - ext.len())
        && leaf[leaf.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// `realpath` by hand (the release policy): each segment lstat'd in turn, a symlink's target read (`readlink`, which
/// touches only the link) and spliced in, `.` / `..` applied lexically to the already-resolved prefix (which holds no
/// link, so lexical is physical). With a `fence` (the development root's segments), every candidate at or below it must lie
/// under it BEFORE it is lstat'd, and the fence's ancestors are taken as they are (never lstat'd), so nothing outside
/// the development fence is ever touched.
#[cfg(feature = "release-loading")]
fn resolve(segs: &[&str], fence: Option<&[&str]>) -> Result<PathBuf, Refusal> {
    use std::collections::VecDeque;
    let mut done: Vec<String> = Vec::new();
    let mut todo: VecDeque<String> = segs.iter().map(|s| s.to_string()).collect();
    let mut links = 0;
    let join = |d: &[String]| PathBuf::from(format!("/{}", d.join("/")));
    while let Some(seg) = todo.pop_front() {
        match seg.as_str() {
            "" | "." => continue,
            ".." => {
                done.pop();
                continue;
            }
            _ => {}
        }
        done.push(seg);
        let here = join(&done);
        if let Some(f) = fence {
            let n = done.len();
            let ancestor = n < f.len() && done.iter().zip(f).all(|(a, b)| a == b);
            if ancestor {
                continue;
            }
            if !(n >= f.len() && done.iter().zip(f).all(|(a, b)| a == b)) {
                return Err(Refusal::OutsideLab(here));
            }
        }
        let m = std::fs::symlink_metadata(&here).map_err(|_| Refusal::NotABundle(here.clone()))?;
        if !m.file_type().is_symlink() {
            continue;
        }
        links += 1;
        if links > MAX_LINKS {
            return Err(Refusal::LinkLoop(here));
        }
        let target = std::fs::read_link(&here).map_err(|_| Refusal::NotABundle(here.clone()))?;
        let target = target.to_str().ok_or(Refusal::NotLexical)?;
        done.pop();
        if target.starts_with('/') {
            done.clear();
        }
        for t in target.split('/').rev() {
            todo.push_front(t.to_string());
        }
    }
    if done.is_empty() {
        return Err(Refusal::NotLexical);
    }
    Ok(join(&done))
}

/// `--plugin-root <LAB>/bin/test-plugins/<sha8>` (test-hooks builds; the root check and the bundle check's lexical
/// half, with the refusal texts the host application's `t4a_vst3` asserts). A root narrows the development gate, never
/// widens it: a bundle must lie at exactly `<root>/vst3/<name>.vst3` AND pass [`Bundle::check`] (lstat, the pin's
/// manifest). `<LAB>` is the `SUMMONER_DEV_PLUGIN_ROOTS` this crate was built with, never the run-time environment.
#[derive(Clone, Debug)]
pub struct PluginRoot {
    path: PathBuf,
}

/// lstat `p`: a directory, not a symlink (the refusal texts the host application asserts).
fn lstat_dir(p: &Path) -> Result<(), String> {
    let m = std::fs::symlink_metadata(p).map_err(|e| format!("{}: {e}", p.display()))?;
    if m.file_type().is_symlink() {
        return Err(format!("symlink: {}", p.display()));
    }
    if !m.is_dir() {
        return Err(format!("{} is not a directory", p.display()));
    }
    Ok(())
}

/// A caller's path as text for the lexical checks: one that is not Unicode is refused as not plain.
fn path_text(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("not plain: {} is not Unicode", p.display()))
}

/// The segments of a plain absolute path, or the `not plain` refusal.
fn plain_segments(p: &str) -> Result<Vec<&str>, String> {
    if !p.starts_with('/') {
        return Err(format!("not plain: {p} is not absolute"));
    }
    lexical_segments(p)
        .ok_or_else(|| format!("not plain: {p} has an empty, `.` or `..` segment, or a trailing /"))
}

impl PluginRoot {
    /// At start (before fd 3): `value` is plain, absolute, exactly `<LAB>/bin/test-plugins/<sha8>`, and
    /// every segment from `<LAB>` down lstats as a directory, none a symlink.
    pub fn check(value: impl AsRef<Path>) -> Result<PluginRoot, crate::Error> {
        Self::check_why(value).map_err(crate::Error::from)
    }

    fn check_why(value: impl AsRef<Path>) -> Result<PluginRoot, String> {
        let value = path_text(value.as_ref())?;
        let lab = dev_fence().ok_or(
            "this child was built without SUMMONER_DEV_PLUGIN_ROOTS: every plugin root is refused"
                .to_string(),
        )?;
        let lsegs = plain_segments(lab).map_err(|e| format!("the build's lab root: {e}"))?;
        let segs = plain_segments(value)?;
        let want = format!("{lab}/bin/test-plugins/");
        let shaped = if unfenced() {
            segs.len() >= 3 && segs[segs.len() - 3..segs.len() - 1] == ["bin", "test-plugins"]
        } else {
            segs.len() == lsegs.len() + 3 && value.starts_with(&want)
        };
        if !shaped || !segs.last().is_some_and(|s| is_sha8(s)) {
            return Err(format!(
                "outside the lab's pins: {value} is not {want}<sha8> (8 lowercase hex digits)"
            ));
        }
        for n in lsegs.len().min(segs.len())..=segs.len() {
            lstat_dir(Path::new(&format!("/{}", segs[..n].join("/"))))?;
        }
        Ok(PluginRoot {
            path: PathBuf::from(value),
        })
    }

    /// The root's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The bundle check's lexical half (no syscall): `file` is exactly `<root>/vst3/<name>.vst3`. The rest (lstat of
    /// every entry, the pin's manifest) is [`Bundle::check`], which every load runs after this.
    pub fn admits(&self, file: impl AsRef<Path>) -> Result<(), crate::Error> {
        let why = || -> Result<(), String> {
            let file = path_text(file.as_ref())?;
            let root = self.path.to_str().unwrap_or_default();
            let rsegs = plain_segments(root)?;
            let segs = plain_segments(file)?;
            let under = segs.len() == rsegs.len() + 2
                && segs[..rsegs.len()] == rsegs[..]
                && segs[rsegs.len()] == "vst3";
            let leaf = segs.last().copied().unwrap_or_default();
            if !under || leaf.len() <= ".vst3".len() || !leaf.ends_with(".vst3") {
                return Err(format!(
                    "outside --plugin-root: {file} is not {root}/vst3/<name>.vst3"
                ));
            }
            Ok(())
        };
        why().map_err(crate::Error::from)
    }

    /// The whole bundle check: [`Self::admits`], then [`Bundle::check`].
    pub fn bundle(&self, file: impl AsRef<Path>) -> Result<Bundle, crate::Error> {
        let why = || -> Result<Bundle, String> {
            let file = path_text(file.as_ref())?;
            self.admits(file)?;
            Bundle::check(Path::new(file), Format::Vst3).map_err(|r| r.to_string())
        };
        why().map_err(crate::Error::from)
    }
}

static STOPS: AtomicU64 = AtomicU64::new(0);
static FOREIGN: AtomicU64 = AtomicU64::new(0);

/// Report only: a shipped build (`release-loading`, no `SUMMONER_DEV_PLUGIN_ROOTS`) reports a foreign image and loads
/// the plugin; every development build refuses it and STOPs, as before.
fn report_only() -> bool {
    shipped_build()
}

/// A shipped build: `release-loading` and no compiled-in `SUMMONER_DEV_PLUGIN_ROOTS`. Every development build is not.
pub fn shipped_build() -> bool {
    cfg!(feature = "release-loading") && dev_fence().is_none()
}

/// What a post-load check found ([`judge`]).
#[derive(Debug, PartialEq)]
enum Verdict {
    Clean,
    /// Foreign images in a shipped build: reported on stderr and counted ([`foreign_images`]); the plugin loads.
    Report(Vec<String>),
    /// A STOP: foreign images in a development build, or (in every build) none of the loaded images inside the bundle.
    Stop(Vec<String>),
}

/// The post-load decision, pure: the images new since `before` that lie outside `bundle` and the system's own
/// (`/System/Library/`, `/usr/lib/`, plain paths only) are foreign; if no loaded image is inside the bundle, its
/// executable was not loaded (a swap between the check and the load), which is a STOP in every build.
fn judge(before: &[String], after: &[String], bundle: &Path, report_only: bool) -> Verdict {
    let inside = |img: &str| Path::new(img).starts_with(bundle) && lexical_segments(img).is_some();
    let system = |img: &str| {
        crate::images::is_system_image(Path::new(img)) && lexical_segments(img).is_some()
    };
    let mut bad: Vec<String> = after
        .iter()
        .filter(|i| !before.contains(i))
        .filter(|i| !inside(i) && !system(i))
        .cloned()
        .collect();
    if !after.iter().any(|i| inside(i)) {
        bad.push("(the bundle's executable is not among the loaded images)".into());
        return Verdict::Stop(bad);
    }
    match (bad.is_empty(), report_only) {
        (true, _) => Verdict::Clean,
        (false, true) => Verdict::Report(bad),
        (false, false) => Verdict::Stop(bad),
    }
}

/// How many foreign images shipped-build loads have brought in and reported (report only; 0 in a development build,
/// where each is a STOP instead).
pub fn foreign_images() -> u64 {
    FOREIGN.load(Ordering::SeqCst)
}

/// The images loaded before this process's first [`LoadCheck::begin`] (or its first [`process_images_clean`]): what
/// was there before any plugin code could run.
static FIRST: OnceLock<Vec<String>> = OnceLock::new();

fn first_images() -> &'static [String] {
    FIRST.get_or_init(crate::dyld::images)
}

/// The two recorders a test may inject (`testkit/plugins/fswatch`, `testkit/plugins/lookwatch`).
const RECORDERS: [&str; 2] = [
    "libsummoner_lab_fswatch.dylib",
    "libsummoner_lab_lookwatch.dylib",
];

/// The image check for the whole process: every loaded image is this executable, a
/// development bundle (under an allowed root), the system's own (`/System/Library/`, `/usr/lib/`, plain paths), or one of the
/// two recorders, and a recorder only if ALL of these hold, so the environment alone can never widen it:
/// - it is named in this process's `DYLD_INSERT_LIBRARIES`;
/// - it lies directly in the build's plugin directory (`<repo>/target.noindex/plugins/`);
/// - its file name is one of the two recorders;
/// - it was loaded before the first [`LoadCheck::begin`].
///
/// Anything else, under `target.noindex/plugins` outside the format roots included, is listed in the `Err`.
pub fn process_images_clean() -> Result<(), crate::images::ForeignImages> {
    let first = first_images();
    let me = std::env::current_exe().ok();
    let roots: Vec<PathBuf> = [Format::Au, Format::Vst3]
        .into_iter()
        .filter_map(allowed_root)
        .chain(pinned_root())
        .collect();
    let plugins = allowed_root(Format::Au).and_then(|r| r.parent().map(Path::to_path_buf));
    let inserted: Vec<String> = std::env::var("DYLD_INSERT_LIBRARIES")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let recorder = |img: &str| {
        let p = Path::new(img);
        lexical_segments(img).is_some()
            && inserted.iter().any(|i| i == img)
            && plugins.as_deref().is_some_and(|d| p.parent() == Some(d))
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| RECORDERS.contains(&n))
            && first.iter().any(|f| f == img)
    };
    let bad: Vec<String> = crate::dyld::images()
        .into_iter()
        .filter(|img| {
            let p = Path::new(img);
            let plain = lexical_segments(img).is_some();
            let ok = me.as_deref() == Some(p)
                || (plain && roots.iter().any(|r| p.starts_with(r)))
                || (plain && crate::images::is_system_image(p))
                || recorder(img);
            !ok
        })
        .collect();
    if bad.is_empty() {
        Ok(())
    } else {
        Err(crate::images::ForeignImages(
            bad.into_iter().map(PathBuf::from).collect(),
        ))
    }
}

/// How many post-load checks found an image that should not be there (each one a STOP).
pub fn stops() -> u64 {
    STOPS.load(Ordering::SeqCst)
}

/// The images dyld had loaded before a bundle's load: [`LoadCheck::finish`] compares.
pub struct LoadCheck {
    before: Vec<String>,
}

impl LoadCheck {
    /// Refused once any check in this process has found a STOP: after one, no plugin code of any format
    /// is loaded, instantiated or initialized again in this process.
    pub fn begin() -> Result<LoadCheck, crate::Error> {
        Self::begin_why().map_err(crate::Error::from)
    }

    fn begin_why() -> Result<LoadCheck, String> {
        if stops() > 0 {
            return Err("STOP earlier in this process: no further plugin load".into());
        }
        first_images(); // the pre-plugin snapshot, taken at the first begin
        Ok(LoadCheck {
            before: crate::dyld::images(),
        })
    }

    /// After the load: `Err` (and a STOP counted) if none of the loaded images is inside the bundle, or, in a lab
    /// build, if a new image lies outside the bundle and the system's own; in a shipped build such an image is
    /// reported on stderr and counted, and the load stands ([`report_only`]).
    pub fn finish(self, b: &Bundle) -> Result<(), crate::Error> {
        let why = || -> Result<(), String> {
            match judge(&self.before, &crate::dyld::images(), &b.path, report_only()) {
                Verdict::Clean => Ok(()),
                Verdict::Report(foreign) => {
                    FOREIGN.fetch_add(foreign.len() as u64, Ordering::SeqCst);
                    eprintln!(
                        "[summoner-sys] REPORT (report only): loading {} brought in {}",
                        b.path.display(),
                        foreign.join(", ")
                    );
                    Ok(())
                }
                Verdict::Stop(bad) => {
                    STOPS.fetch_add(1, Ordering::SeqCst);
                    let msg = format!(
                        "STOP: loading {} brought in {}",
                        b.path.display(),
                        bad.join(", ")
                    );
                    eprintln!("[summoner-sys] {msg}");
                    Err(msg)
                }
            }
        };
        why().map_err(crate::Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shipped build (no development root) does not walk the bundle's inside; a development build does (the development gate's walk). The
    /// same lab-scratch copy of the test gain plugin, a symlink and a hard link inside it (both pointing inside the bundle),
    /// judged both ways. The shipped variant has no fence, so only lab paths are given to it, and nothing it follows
    /// leaves the bundle.
    #[cfg(feature = "release-loading")]
    #[test]
    fn a_shipped_build_does_not_walk_the_inside_and_a_lab_build_does() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let gain = repo.join("target.noindex/plugins/vst3/summoner-lab-gain.vst3");
        let s = repo
            .join(format!(
                "target.noindex/tmp/g3-no-walk-{}",
                std::process::id()
            ))
            .to_path_buf();
        let _ = std::fs::remove_dir_all(&s);
        std::fs::create_dir_all(&s).unwrap();
        let s = s.canonicalize().unwrap();
        fn copy(from: &Path, to: &Path) {
            std::fs::create_dir_all(to).unwrap();
            for e in std::fs::read_dir(from).unwrap().map(Result::unwrap) {
                let t = e.file_type().unwrap();
                if t.is_dir() {
                    copy(&e.path(), &to.join(e.file_name()));
                } else {
                    assert!(t.is_file(), "{:?}", e.path());
                    std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
                }
            }
        }
        let b = s.join("summoner-lab-gain.vst3");
        copy(&gain, &b);
        let lab = dev_fence();
        let link = b.join("Contents/Resources-link");
        std::os::unix::fs::symlink("MacOS", &link).unwrap();
        let shipped = |p: &Path| Bundle::check_release_in(p, Format::Vst3, None).map(|x| x.path);
        let labbed = |p: &Path| Bundle::check_release_in(p, Format::Vst3, lab).map(|x| x.path);
        assert_eq!(labbed(&b).unwrap_err(), Refusal::Symlink(link.clone()));
        assert_eq!(
            shipped(&b).unwrap(),
            b,
            "no walk: the inside link is not looked at"
        );
        std::fs::remove_file(&link).unwrap();
        let plist = b.join("Contents/Info.plist");
        std::fs::hard_link(&plist, s.join("plist.hard")).unwrap();
        assert_eq!(labbed(&b).unwrap_err(), Refusal::HardLink(plist));
        assert_eq!(
            shipped(&b).unwrap(),
            b,
            "no walk: the hard link is not looked at"
        );
        // Everything else of the policy is the same without a development root: the lexical and extension refusals, and the
        // resolved path a directory with the extension.
        assert_eq!(
            shipped(&s.join("x/../summoner-lab-gain.vst3")).unwrap_err(),
            Refusal::NotLexical
        );
        assert_eq!(
            shipped(&s.join("summoner-lab-gain.vst")).unwrap_err(),
            Refusal::Extension(s.join("summoner-lab-gain.vst"))
        );
        let file = s.join("file.vst3");
        std::fs::write(&file, b"not a bundle").unwrap();
        assert!(shipped(&file).is_err(), "a regular file is not a bundle");
        std::fs::remove_dir_all(&s).unwrap();
    }

    /// Report only: the post-load decision on made-up image lists (nothing is
    /// loaded). A foreign image (a vendor framework, a shell plugin's engine) is a Report in a shipped build and a
    /// STOP in a development build; a load that brought no image of the bundle is a STOP in both; system images and the
    /// bundle's own are clean in both; an image already loaded before is never foreign.
    #[test]
    fn report_only_in_a_shipped_build_stop_in_a_lab_build() {
        let b = Path::new("/Library/Audio/Plug-Ins/VST3/Some.vst3");
        let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let before = v(&["/usr/lib/libSystem.B.dylib", "/opt/old/libalready.dylib"]);
        let exe = "/Library/Audio/Plug-Ins/VST3/Some.vst3/Contents/MacOS/Some";
        let fw = "/Library/Frameworks/Vendor.framework/Versions/A/Vendor";
        let sys = "/System/Library/Frameworks/Accelerate.framework/Versions/A/Accelerate";
        let after = |xs: &[&str]| [before.clone(), v(xs)].concat();
        for shipped in [true, false] {
            let j = |xs: &[&str]| judge(&before, &after(xs), b, shipped);
            assert_eq!(j(&[exe]), Verdict::Clean, "the bundle's own");
            assert_eq!(j(&[exe, sys]), Verdict::Clean, "and the system's");
            let foreign = j(&[exe, sys, fw]);
            if shipped {
                assert_eq!(
                    foreign,
                    Verdict::Report(v(&[fw])),
                    "shipped: reported, loads"
                );
            } else {
                assert_eq!(foreign, Verdict::Stop(v(&[fw])), "lab: a STOP");
            }
            assert!(
                matches!(j(&[fw]), Verdict::Stop(_)),
                "no image of the bundle: a STOP in every build"
            );
            assert!(
                matches!(
                    j(&["/Library/Audio/Plug-Ins/VST3/Some.vst3/../x/y"]),
                    Verdict::Stop(_)
                ),
                "a non-plain path is not inside"
            );
        }
        // This build (a development build: SUMMONER_DEV_PLUGIN_ROOTS compiled in) is never report-only.
        assert!(!report_only());
    }

    /// The manifest is split on `\n` only. A name ending in `\r` (Finder's `Icon\r`) keeps it; `shasum`'s
    /// escaped form (a leading `\`, `\\` and `\n` in the path) is read; anything else malformed is refused.
    /// Mutation: `mutant_manifest_strips_cr` (the old `lines()` read) fails it.
    #[test]
    fn a_manifest_keeps_a_trailing_cr_and_reads_shasums_escapes() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let text = format!(
            "{a}  ./vst3/X.vst3/Icon\r\n\\{b}  ./vst3/X.vst3/a\\\\b\\nc\n{a}  ./vst3/X.vst3/sp  ace"
        );
        assert_eq!(
            manifest_entries(&text).unwrap(),
            vec![
                (a.as_str(), "vst3/X.vst3/Icon\r".to_string()),
                (b.as_str(), "vst3/X.vst3/a\\b\nc".to_string()),
                (a.as_str(), "vst3/X.vst3/sp  ace".to_string()),
            ]
        );
        assert_eq!(manifest_entries("").unwrap(), vec![]);
        for bad in [
            format!("{a}  ./x\n\n{a}  ./y\n"),
            format!("{a} ./x\n"),
            format!("{a}  x\n"),
            format!("{a}  ./\n"),
            format!("{}  ./x\n", "A".repeat(64)),
            format!("{}  ./x\n", "a".repeat(63)),
            format!("\\{a}  ./x\\t\n"),
            format!("\\{a}  ./x\\\n"),
        ] {
            assert_eq!(manifest_entries(&bad), None, "{bad:?}");
        }
    }

    /// Shasum lists each file once and escapes every name with a backslash, so a duplicate
    /// path (escaped or not, however it is spelled) and an unescaped `\` are refused.
    /// Mutations: `mutant_manifest_dup_accepted` and `mutant_manifest_literal_backslash` each fail it.
    #[test]
    fn a_manifest_with_a_duplicate_path_or_an_unescaped_backslash_is_refused() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert!(manifest_entries(&format!("{a}  ./x\n{b}  ./y\n")).is_some());
        for bad in [
            format!("{a}  ./x\n{b}  ./x\n"),
            format!("{a}  ./x\n{a}  ./x\n"),
            format!("{a}  ./x\n\\{b}  ./x\n"),
            format!("\\{a}  ./a\\\\b\n\\{b}  ./a\\\\b\n"),
            format!("{a}  ./a\\b\n"),
            format!("{a}  ./a\\nb\n"),
        ] {
            assert_eq!(manifest_entries(&bad), None, "{bad:?}");
        }
    }

    /// A pin whose bundle holds `Icon\r` (and names `shasum` escapes) verifies against the
    /// manifest `shasum -a 256` itself writes, run inside the pin; one changed byte is refused.
    /// The scratch pin lives in this repo's target.noindex/tmp, never under the real pins.
    /// Mutation: `mutant_manifest_strips_cr` fails it.
    #[test]
    fn a_pin_with_an_icon_cr_file_verifies_against_shasums_own_manifest() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let s = repo.join(format!(
            "target.noindex/tmp/pin-check-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&s);
        let stage = s.join("stage");
        let contents = stage.join("vst3/X.vst3/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(contents.join("Info.plist"), b"<plist/>").unwrap();
        std::fs::write(contents.join("MacOS/X"), b"exe").unwrap();
        std::fs::write(stage.join("vst3/X.vst3/Icon\r"), b"").unwrap();
        std::fs::write(contents.join("back\\slash"), b"b").unwrap();
        std::fs::write(contents.join("new\nline"), b"n").unwrap();
        let mut files = Vec::new();
        files_under(&stage, &stage, &mut files).unwrap();
        let mut names: Vec<String> = files
            .iter()
            .map(|f| format!("./{}", f.to_str().unwrap()))
            .collect();
        names.sort();
        let out = std::process::Command::new("/usr/bin/shasum")
            .args(["-a", "256"])
            .args(&names)
            .current_dir(&stage)
            .output()
            .unwrap();
        assert!(out.status.success(), "shasum");
        let manifest = out.stdout;
        assert!(
            manifest.windows(6).any(|w| w == b"Icon\r\n"),
            "shasum writes the \\r raw"
        );
        assert_eq!(
            manifest.iter().filter(|&&c| c == b'\n').count(),
            5,
            "one line per file (the newline name escaped)"
        );
        std::fs::write(stage.join("manifest.sha256"), &manifest).unwrap();
        let pin = s.join(&crate::sha256::hex(&manifest)[..8]);
        std::fs::rename(&stage, &pin).unwrap();
        let bundle = pin.join("vst3/X.vst3");
        assert_eq!(verify_pin(&pin, &bundle), Ok(()));
        let icon = bundle.join("Icon\r");
        std::fs::write(&icon, b"x").unwrap();
        assert_eq!(verify_pin(&pin, &bundle), Err(Refusal::PinMismatch(icon)));
        std::fs::remove_dir_all(&s).unwrap();
    }

    #[test]
    fn lexical_refusals_touch_nothing() {
        for p in [
            "relative/x.component",
            "/a/./b.component",
            "/a/../b.component",
            "/a//b.component",
            "/a/b.component/",
        ] {
            assert_eq!(lexical_segments(p), None, "{p}");
        }
        assert_eq!(
            lexical_segments("/a/b.component"),
            Some(vec!["a", "b.component"])
        );
    }

    /// The fence is lexical and decides alone: a repo outside it has no build roots, one under it has both.
    /// Mutant: `mutant_dev_roots_unfenced` turns it red.
    #[test]
    fn a_root_outside_the_dev_fence_is_no_root() {
        for f in [Format::Au, Format::Vst3] {
            assert_eq!(allowed_root_in("/dev/fence", "/elsewhere/repo", f), None);
            assert_eq!(allowed_root_in("/dev/fence", "/dev/fenced/repo", f), None);
            assert_eq!(allowed_root_in("/dev/fence", "/dev", f), None);
            assert_eq!(
                allowed_root_in("/dev/fence", "/dev/fence/repo", f),
                Some(PathBuf::from(format!(
                    "/dev/fence/repo/target.noindex/plugins/{}",
                    f.dir()
                )))
            );
            assert_eq!(allowed_root_in("dev/fence", "/dev/fence/repo", f), None);
        }
    }

    #[test]
    fn the_roots_are_this_repos_and_under_the_lab() {
        // The tests are built with SUMMONER_DEV_PLUGIN_ROOTS set.
        let lab = dev_fence().expect("the tests are built with SUMMONER_DEV_PLUGIN_ROOTS");
        for f in [Format::Au, Format::Vst3] {
            let r = allowed_root(f).expect("a root");
            assert!(r.starts_with(lab), "{} under {lab}", r.display());
            assert!(r.ends_with(format!("target.noindex/plugins/{}", f.dir())));
        }
    }

    #[test]
    fn a_plugin_root_is_exactly_a_pin_and_admits_only_its_vst3_bundles() {
        let lab = dev_fence().expect("the tests are built with SUMMONER_DEV_PLUGIN_ROOTS");
        let pins = format!("{lab}/bin/test-plugins");
        // The pins root must exist for the lstat walk's row below (`00000000`); a fresh clone has no pins yet, so
        // the test makes the empty root itself, independent of test order.
        std::fs::create_dir_all(&pins).expect("the lab's pins root");
        let refused = |v: &str, why: &str| {
            let e = PluginRoot::check(v).unwrap_err();
            assert!(e.as_str().contains(why), "{v}: {e}");
        };
        refused("bin/test-plugins/a1e8e94a", "not absolute");
        refused(&format!("{pins}/../test-plugins/a1e8e94a"), "not plain");
        refused(&format!("{pins}/a1e8e94a/"), "not plain");
        refused(&format!("{lab}/bin"), "outside the lab's pins");
        refused(&format!("{pins}/A1E8E94A"), "outside the lab's pins");
        refused(&format!("{pins}/a1e8e94"), "outside the lab's pins");
        refused(
            &format!("{lab}/elsewhere/tmp/bin/test-plugins/a1e8e94a"),
            "outside the lab's pins",
        );
        refused(&format!("{pins}/a1e8e94a/vst3"), "outside the lab's pins");
        // Lexically a pin, but not there: the lstat walk refuses it (the pins root itself exists).
        refused(&format!("{pins}/00000000"), "/00000000");
        // Lexical admission, with no syscall (a root value made by hand, as `check` would accept it).
        let root = PluginRoot {
            path: PathBuf::from(format!("{pins}/a1e8e94a")),
        };
        root.admits(format!("{pins}/a1e8e94a/vst3/summoner-lab-gain.vst3"))
            .unwrap();
        for f in [
            format!("{pins}/a1e8e94a/au/summoner-lab-gain.component"),
            format!("{pins}/a1e8e94a/vst3/x/summoner-lab-gain.vst3"),
            format!("{pins}/a1e8e94a/vst3/.vst3"),
            format!("{pins}/a1e8e94a/vst3/summoner-lab-gain.vst"),
            format!("{pins}/bbf2293c/vst3/summoner-lab-gain.vst3"),
            format!("{lab}/another-repo/target.noindex/plugins/vst3/summoner-lab-gain.vst3"),
        ] {
            let e = root.admits(&f).unwrap_err();
            assert!(
                e.as_str().starts_with("outside --plugin-root: "),
                "{f}: {e}"
            );
        }
        assert!(
            root.admits(format!("{pins}/a1e8e94a/vst3/../vst3/g.vst3"))
                .unwrap_err()
                .as_str()
                .contains("not plain")
        );
    }

    #[test]
    fn outside_the_root_is_refused_lexically() {
        let fence = dev_fence().expect("the tests are built with SUMMONER_DEV_PLUGIN_ROOTS");
        let home = Path::new(fence).parent().unwrap().display().to_string();
        for p in [
            format!("{home}/Library/Audio/Plug-Ins/Components/Any.component"),
            "/Library/Audio/Plug-Ins/Components/Any.component".into(),
            "/System/Library/Components/Any.component".into(),
        ] {
            assert_eq!(
                Bundle::check_dev(Path::new(&p), Format::Au).unwrap_err(),
                Refusal::OutsideRoot,
                "{p}"
            );
        }
        let au = allowed_root(Format::Au).unwrap();
        let climb = format!("{}/../../../x.component", au.display());
        assert_eq!(
            Bundle::check_dev(Path::new(&climb), Format::Au).unwrap_err(),
            Refusal::NotLexical
        );
        let wrong_ext = format!("{}/x.vst3", au.display());
        assert_eq!(
            Bundle::check_dev(Path::new(&wrong_ext), Format::Au).unwrap_err(),
            Refusal::OutsideRoot
        );
    }
}
