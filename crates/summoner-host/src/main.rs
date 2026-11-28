//! `summoner-host`: the Rust plugin host program, spawned by the summoner supervisor with
//! stdio and the child end of a socketpair on fd 3, nothing else. The protocol is summoner-proto's: every type and
//! offset comes from the protocol crates; every `unsafe` call is in the runecraft floor crates or this repo's
//! `crates/summoner-sys`.
//!
//! Threads (no event loop):
//! - main: the message thread ([`host::message_thread`]): load, prepare, state, editor, parameters, unload, shutdown;
//! - `hc-read`: the reader ([`reader::reader_thread`]): frames in, inline ops answered there, the rest receipted and
//!   posted to the message thread in order;
//! - `hc-mach`: the mach channel ([`mach::channel_thread`]): workgroup ports;
//! - `hc-chain-<key>`: one per chain ([`chain`]): the doorbell wait, the block, the ack;
//! - `hc-orphan`: the orphan watch: the parent gone means exit 3 ([`watch_parent`]).
//!
//! A fatal signal is logged by `summoner_sys::crash` (the crash log, with the plugin op in flight).
//!
//! Flags: `--scan` (the scan child, [`scan_mode`]: one bundle, no chains; with `--step-ms <n>`), `--parent-pid <pid>`
//! (the orphan watch; default: the spawning parent), `--tc-us <period>` (chain threads'
//! time-constraint period; default max_frames / rate), `--no-tc`, `--stats <path>`, `--counters <path>` (each
//! slot's re-activation counters, written at shutdown). Test flags (`test-hooks` builds only): `--ignore-slot0`,
//! `--mutant-reader-pongs`, `--lie-quarantined`, `--hang-at-hello`, and `--plugin-root <LAB>/bin/test-plugins/<sha8>`
//! (once; exit 2 on a refusal, before fd 3) and `--describe <bundle>`. `--source-hash` alone prints the hash of the
//! sources this binary was built from (`tools/source-hash.sh`); `--editor-dylib-path` alone, the editor dylib path it
//! resolves and exits.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

mod chain;
mod diag;
mod host;
mod mach;
mod reader;
mod scan_mode;
mod slots;
mod wire;

use runecraft_ipc::bridge::MachReceive;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The child's command line, parsed ([`parse_args`]).
#[derive(Clone, Debug, Default)]
pub struct Opts {
    /// `--parent-pid <pid>`: the parent the orphan watch follows (a pid of 0 or 1 is ignored); `None`, the spawning
    /// parent.
    pub parent_pid: Option<u32>,
    /// The chain threads' time-constraint period in µs: `None` off (`--no-tc`), `Some(0)` the default.
    pub tc_us: Option<u64>,
    /// `--stats <path>`: each chain's block timing samples, written there at shutdown.
    pub stats: Option<String>,
    /// `--counters <path>`: each slot's re-activation counters, written at shutdown (child-side; no protocol field).
    pub counters: Option<String>,
    /// `--ignore-slot0` (test-hooks): the registered mach port (slot 0) is cleared but never used.
    pub ignore_slot0: bool,
    /// `--mutant-reader-pongs` (test-hooks): a side thread answers every wedge ping, so a wedged chain looks alive.
    pub mutant_pongs: bool,
    /// `--lie-quarantined` (test-hooks): a quarantined slot reports a made-up latency and parameter count (999, 77).
    pub lie_quarantined: bool,
    /// `--hang-at-hello` (test-hooks): the child wedges at the hello, before its first answer.
    pub hang_at_hello: bool,
    /// test-hooks: `SUMMONER_TEST_SCAN_REPEAT=<n>` in the environment (with `--scan`): each scanned description n times
    /// (the oversized `scanned` line's test). An environment variable, not a flag: the host application's
    /// ScanSupervisor spawns the child with fixed arguments and an explicit environment.
    pub scan_repeat: usize,
    /// `--noalloc-blocks <n>` (test-hooks): each chain thread counts the allocations of its first `n` blocks under the
    /// injected counter (`libsummoner_lab_malloccount.dylib`), written with `--counters`.
    pub noalloc_blocks: u32,
    /// `--scan`: the scan child ([`scan_mode`]): one bundle, no chains.
    pub scan: bool,
    /// `--step-ms <n>` (with `--scan`): each probe step's deadline (default [`scan_mode::DEFAULT_STEP_MS`], 25 000);
    /// past it, exit 75.
    pub step_ms: Option<u64>,
    /// `--crash-log <dir>` (both modes): the crash log's folder, given by the host application (the
    /// app's folder in production). Absolute, canonical, existing, no link on it (the pinned
    /// `runecraft_ipc::process::unix::resolve_explicit_dir`), at most 512 bytes: else exit 2, before fd 3 and any plugin
    /// code. Without it, `$HOME/Library/Logs` as before.
    pub crash_log: Option<String>,
    /// `--log-dir <dir>` (both modes): the diagnostic log's folder, given by the host application. Resolved exactly as
    /// `--crash-log` (else exit 2, before fd 3). Without it, diagnostic records at `warn` and above go to stderr.
    pub log_dir: Option<String>,
    /// `--plugin-root <LAB>/bin/test-plugins/<sha8>` (test-hooks; once): checked in [`main`] before fd 3 (exit 2 on a
    /// refusal); then every VST3 slot must lie at `<root>/vst3/<name>.vst3` too ([`slots`]). Without it, the
    /// compiled-in roots alone apply.
    #[cfg(feature = "test-hooks")]
    pub plugin_root: Option<String>,
    /// `--describe <bundle>` (test-hooks): the root and bundle checks, then each class's `PluginRef` (JSON) on one
    /// line, then exit; no fd 3, no chain.
    #[cfg(feature = "test-hooks")]
    pub describe: Option<String>,
    /// `--test-press-close <ms>` (test-hooks): each editor window's close button is pressed once (AppKit's
    /// `performClose:`), this long after the window opened: the user's close path, without a click.
    #[cfg(feature = "test-hooks")]
    pub press_close_ms: Option<u64>,
    /// `--test-minimize <ms>` (test-hooks): each editor window's minimise button is pressed once (AppKit's
    /// `performMiniaturize:`), this long after the window opened.
    #[cfg(feature = "test-hooks")]
    pub minimize_ms: Option<u64>,
    /// `--test-click <ms>` (test-hooks): a user's click is posted to each editor window once, this long after it
    /// opened (a user event stands the editor-reset guard down).
    #[cfg(feature = "test-hooks")]
    pub click_ms: Option<u64>,
    /// `--test-click-title-bar` (test-hooks): the `--test-click` lands in the title bar (not an edit).
    #[cfg(feature = "test-hooks")]
    pub click_title_bar: bool,
    /// `--test-no-activate` (test-hooks): editor windows never activate the child (the lab's tests open windows on a
    /// screen someone is typing on; the shipped default activates).
    #[cfg(feature = "test-hooks")]
    pub no_activate: bool,
    /// `--test-editor-dylib <path>` (test-hooks): load the editor dylib from here instead (a missing or broken one must
    /// leave the child working, with no window).
    #[cfg(feature = "test-hooks")]
    pub editor_dylib: Option<String>,
    /// `--test-latency-no-cycle` (test-hooks): a VST3 `kLatencyChanged` is reported without the re-activation, so a
    /// real plugin's null can be taken without the re-activation in it.
    #[cfg(feature = "test-hooks")]
    pub latency_no_cycle: bool,
    /// `--test-slot-enabled <slot>:<on|off>@<block>[,...]` (test-hooks): the chain thread sets every chain's
    /// slot flag at the top of that processed block (`@0`: at load, no fade), until summoner-proto's codec carries
    /// `chain.load`'s `enabled` and `slot.setEnabled` (protocol 3.2) at the pin.
    #[cfg(feature = "test-hooks")]
    pub slot_enabled: Vec<(u32, bool, u64)>,
}

/// `--test-slot-enabled`'s value; a malformed one is a usage error.
#[cfg(feature = "test-hooks")]
fn parse_slot_enabled(v: &str) -> Vec<(u32, bool, u64)> {
    v.split(',')
        .map(|item| {
            let bad = || -> ! {
                usage(&format!(
                    "--test-slot-enabled: {item:?} is not <slot>:<on|off>@<block>"
                ))
            };
            let Some((slot, rest)) = item.split_once(':') else {
                bad()
            };
            let Some((state, at)) = rest.split_once('@') else {
                bad()
            };
            let on = match state {
                "on" => true,
                "off" => false,
                _ => bad(),
            };
            let (Ok(slot), Ok(at)) = (slot.parse::<u32>(), at.parse::<u64>()) else {
                bad()
            };
            (slot, on, at)
        })
        .collect()
}

/// A usage error, before fd 3 and before any plugin code: `exit(2)` (not `_exit`).
fn usage(why: &str) -> ! {
    eprintln!("summoner-host: {why}");
    std::process::exit(2);
}

fn parse_args(args: &[String]) -> Opts {
    let mut o = Opts {
        tc_us: Some(0),
        ..Opts::default()
    };
    #[cfg(feature = "test-hooks")]
    {
        o.scan_repeat = std::env::var("SUMMONER_TEST_SCAN_REPEAT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1);
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut next = || {
            it.next()
                .cloned()
                .unwrap_or_else(|| usage(&format!("missing value for {a}")))
        };
        match a.as_str() {
            "--parent-pid" => o.parent_pid = next().parse().ok().filter(|p| *p > 1),
            "--scan" => o.scan = true,
            // The production spawn passes it explicitly; a value it cannot mean is a usage error, never
            // silently the default.
            "--step-ms" => {
                let v = next();
                o.step_ms = Some(
                    v.parse::<u64>()
                        .ok()
                        .filter(|n| (1..=120_000).contains(n))
                        .unwrap_or_else(|| {
                            usage(&format!("--step-ms: {v:?} is not 1..=120000 ms"))
                        }),
                );
            }
            "--crash-log" => o.crash_log = Some(next()),
            "--log-dir" => o.log_dir = Some(next()),
            "--tc-us" => o.tc_us = Some(next().parse().unwrap_or(0)),
            "--no-tc" => o.tc_us = None,
            "--stats" => o.stats = Some(next()),
            "--counters" => o.counters = Some(next()),
            #[cfg(feature = "test-hooks")]
            "--ignore-slot0" => o.ignore_slot0 = true,
            #[cfg(feature = "test-hooks")]
            "--mutant-reader-pongs" => o.mutant_pongs = true,
            #[cfg(feature = "test-hooks")]
            "--lie-quarantined" => o.lie_quarantined = true,
            #[cfg(feature = "test-hooks")]
            "--hang-at-hello" => o.hang_at_hello = true,
            #[cfg(feature = "test-hooks")]
            "--test-editor-dylib" => o.editor_dylib = Some(next()),
            #[cfg(feature = "test-hooks")]
            "--test-latency-no-cycle" => o.latency_no_cycle = true,
            #[cfg(feature = "test-hooks")]
            "--test-slot-enabled" => o.slot_enabled.extend(parse_slot_enabled(&next())),
            #[cfg(feature = "test-hooks")]
            "--test-click-title-bar" => o.click_title_bar = true,
            #[cfg(feature = "test-hooks")]
            "--test-no-activate" => o.no_activate = true,
            #[cfg(feature = "test-hooks")]
            "--test-click" => {
                o.click_ms = Some(
                    next()
                        .parse()
                        .unwrap_or_else(|_| usage("--test-click <ms>")),
                )
            }
            #[cfg(feature = "test-hooks")]
            "--test-minimize" => {
                o.minimize_ms = Some(
                    next()
                        .parse()
                        .unwrap_or_else(|_| usage("--test-minimize <ms>")),
                )
            }
            #[cfg(feature = "test-hooks")]
            "--test-press-close" => {
                o.press_close_ms = Some(
                    next()
                        .parse()
                        .unwrap_or_else(|_| usage("--test-press-close <ms>")),
                )
            }
            #[cfg(feature = "test-hooks")]
            "--stack-water" => {
                crate::chain::STACK_WATER.store(true, std::sync::atomic::Ordering::Relaxed)
            }
            #[cfg(feature = "test-hooks")]
            "--noalloc-blocks" => o.noalloc_blocks = next().parse().unwrap_or(0),
            #[cfg(feature = "test-hooks")]
            "--plugin-root" => {
                if o.plugin_root.is_some() {
                    usage(&format!("{a} given twice"));
                }
                let v = next();
                if v.is_empty() {
                    usage(&format!("an empty value for {a}"));
                }
                o.plugin_root = Some(v);
            }
            #[cfg(feature = "test-hooks")]
            "--describe" => o.describe = Some(next()),
            _ => usage(&format!("unknown argument {a}")),
        }
    }
    o
}

/// The orphan watch: exit 3 once the parent is gone. The parent is
/// `--parent-pid` or else the spawning parent; gone is the kernel's exit note on it (kqueue NOTE_EXIT: at once, a
/// zombie included), with the re-parent backstop (our parent is no longer that pid). The 250 ms slice only re-checks
/// the backstop; it is not the detection latency. If the kqueue watch cannot be registered, the slice polls instead.
fn watch_parent(given: Option<u32>) {
    let parent = given.unwrap_or_else(runecraft_ipc::process::parent_pid);
    if parent <= 1 {
        summoner_sys::proc_exit::exit_now(3);
    }
    let watch = summoner_sys::exit_watch::ExitWatch::new(parent);
    if let Err(e) = &watch {
        eprintln!("[summoner-host] parent watch: {e}; polling every 250 ms");
    }
    std::thread::Builder::new()
        .name("hc-orphan".into())
        .spawn(move || {
            let slice = Duration::from_millis(250);
            loop {
                let exited = match &watch {
                    Ok(w) => w.wait(Some(slice)).unwrap_or(false),
                    Err(_) => {
                        std::thread::sleep(slice);
                        !runecraft_ipc::process::pid_exists(parent)
                    }
                };
                if exited || runecraft_ipc::process::parent_pid() != parent {
                    // `_exit`: chain threads may be inside plugin code.
                    summoner_sys::proc_exit::exit_now(3);
                }
            }
        })
        .expect("the orphan watch");
}

/// A given plugin root is checked here, before fd 3 and before any plugin code (exit 2
/// on a refusal), and kept for every VST3 slot ([`slots::set_plugin_root`]); then `--describe` runs and exits.
#[cfg(feature = "test-hooks")]
fn plugin_root(o: &Opts) {
    use summoner_sys::bundle::PluginRoot;
    let root = o.plugin_root.as_deref().map(|v| {
        PluginRoot::check(v)
            .unwrap_or_else(|why| usage(&format!("the plugin root is refused: {why}")))
    });
    if let Some(bundle) = &o.describe {
        let Some(root) = &root else {
            usage("describe: no plugin root (a hooks build describes only inside its root)");
        };
        let b = root
            .bundle(bundle)
            .unwrap_or_else(|why| usage(&format!("describe: refused: {why}")));
        // The bundle's classes (moduleinfo.json, else the factory, which runs plugin code: only here, never at a
        // load), each the `PluginRef` a slot loads it by (protocol 4.0), one JSON line each. Exit 0 with at least one
        // line, 1 with none.
        let found: Vec<_> = summoner_probe::discover_vst3(&b)
            .unwrap_or_else(|e| {
                eprintln!("summoner-host: describe: {e}");
                Vec::new()
            })
            .iter()
            .filter(
                |c| matches!(c, summoner_probe::ScanClass::Vst3(v) if v.instantiated != Some(false)),
            )
            .filter_map(summoner_probe::plugin_ref)
            .collect();
        for r in &found {
            println!("{}", serde_json::to_string(r).expect("a ref serialises"));
        }
        // `exit` (stdout flushed by it).
        std::process::exit(if found.is_empty() { 1 } else { 0 });
    }
    if let Some(root) = root {
        slots::set_plugin_root(root);
    }
}

/// The crash log's directory: `$HOME/Library/Logs` (the lab home in the lab), or `/tmp` without a HOME.
fn crash_log_dir() -> std::path::PathBuf {
    match std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        Some(h) => std::path::Path::new(&h).join("Library/Logs"),
        None => "/tmp".into(),
    }
}

/// The diagnostic log (off by default: every area at `warn`): `SUMMONER_LOG` (area=level,...), `SUMMONER_LOG_SALT` (the
/// per-install hash key, hex), `--log-dir`. A bad `--log-dir` is a usage error (exit 2, before fd 3); a bad `SUMMONER_LOG`
/// entry is ignored with a warning.
fn diag_init(opts: &Opts) {
    // The folder resolved and held open (its descriptor goes to runecraft-log, which never walks a path again).
    let dir = opts.log_dir.as_ref().map(|d| {
        runecraft_ipc::process::unix::resolve_explicit_dir(std::path::Path::new(d))
            .unwrap_or_else(|e| usage(&format!("--log-dir: {e}")))
    });
    let env = &summoner_proto::log_vocab::HOST_VOCAB;
    let (vocab, levels, warnings) = diag::parse(&std::env::var(env.env_levels).unwrap_or_default());
    for w in warnings {
        eprintln!("[summoner-host] {w}");
    }
    diag::init(vocab, dir, levels, std::env::var(env.env_salt).ok());
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--source-hash" {
        println!("{}", env!("SUMMONER_HOST_SOURCE_HASH"));
        return;
    }
    // `--editor-dylib-path` alone prints the editor dylib path this binary resolves (beside the executable,
    // or a lab build's build path; empty in a shipped build without one) and exits: no plugin, no socket.
    if args.len() == 2 && args[1] == "--editor-dylib-path" {
        println!("{}", host::editor_dylib_path());
        return;
    }
    let opts = parse_args(&args[1..]);
    match &opts.crash_log {
        // The host application's folder: refused at once (exit 2), never a fallback.
        Some(dir) => {
            let dir =
                match runecraft_ipc::process::unix::resolve_explicit_dir(std::path::Path::new(dir))
                {
                    Ok((_fd, dir)) => dir, // checked here; the handler opens by path (N1)
                    Err(e) => usage(&format!("--crash-log: {e}")),
                };
            if let Err(e) = summoner_sys::crash::install(&dir) {
                usage(&format!("--crash-log: {e}"));
            }
        }
        // As before: `$HOME/Library/Logs`; a refused log file there leaves the report on stderr.
        None => {
            if let Err(e) = summoner_sys::crash::install(&crash_log_dir()) {
                eprintln!("[summoner-host] crash log: {e}; reports go to stderr");
                summoner_sys::crash::install_to_stderr();
            }
        }
    }
    diag_init(&opts);
    // The host application's wired arena, ready before the first chain (its orphan ring,
    // its soft cap, a reserve chunk). Never called on a chain thread.
    runecraft_rt::arena::RtArena::global().init();
    #[cfg(feature = "test-hooks")]
    plugin_root(&opts);
    #[cfg(feature = "test-hooks")]
    summoner_format_vst3::set_latency_no_cycle(opts.latency_no_cycle);
    #[cfg(feature = "test-hooks")]
    if !opts.slot_enabled.is_empty() {
        let _ = chain::TEST_SLOT_ENABLED.set(opts.slot_enabled.clone());
    }
    let Some(fd) = runecraft_ipc::process::unix::take_fd3() else {
        eprintln!("summoner-host: no descriptor 3 (run me through the bridge)");
        // Before any plugin code: `exit(2)`, not `_exit`.
        std::process::exit(2);
    };
    // Take slot 0 and clear the registration before anything else.
    let slot0 = runecraft_ipc::bridge::take_registered_port();
    if opts.scan {
        // The scan child: no mach channel (slot 0 is taken and dropped), no chains; the orphan watch as ever.
        drop(slot0);
        watch_parent(opts.parent_pid);
        let sock = UnixStream::from(fd);
        let w = wire::writer(sock.try_clone().expect("clone the socket"));
        scan_mode::run(
            sock,
            w,
            opts.step_ms.unwrap_or(scan_mode::DEFAULT_STEP_MS),
            opts.scan_repeat.max(1),
        );
    }
    let slot0 = if opts.ignore_slot0 { None } else { slot0 };
    let own = MachReceive::new().ok().map(Arc::new);
    watch_parent(opts.parent_pid);

    let sock = UnixStream::from(fd);
    let writer = wire::writer(sock.try_clone().expect("clone the socket"));
    let sh = Arc::new(host::Shared::new(opts, writer));
    if let Some(rx) = own.clone() {
        let sh = sh.clone();
        std::thread::Builder::new()
            .name("hc-mach".into())
            .spawn(move || mach::channel_thread(rx, sh))
            .expect("the mach channel thread");
    }
    let (op_tx, op_rx) = std::sync::mpsc::channel();
    {
        let sh = sh.clone();
        let own = own.clone();
        std::thread::Builder::new()
            .name("hc-read".into())
            .spawn(move || reader::reader_thread(sock, sh, op_tx, slot0, own))
            .expect("the reader thread");
    }
    if sh.opts.mutant_pongs {
        // The wedge mutation: a reader-side thread answers the wedge ping, so a wedged chain looks alive.
        let sh = sh.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(20));
                for c in sh.all_chains() {
                    let p = c.region.ping().load(Ordering::Acquire);
                    if p != 0 {
                        c.region.pong().store(p, Ordering::Release);
                    }
                }
            }
        });
    }
    host::message_thread(op_rx, sh, own);
}
