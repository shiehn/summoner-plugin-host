//! `summoner-scan-probe`: scans ONE lab bundle and prints the report as JSON on stdout: the host application's own
//! `codec::Scanned` (`format`, `path`, `outcome: ok` | `failed` + `reason` + `detail`, `descriptions` with each VST3's
//! `classId` since 1488c63), plus the probe's extras (`probe.xml`, `probe.validation`, …). It stands in for
//! `summoner-host --scan` until that mode speaks the protocol. Its argument is checked as the host application checks a
//! `scan` (`codec::Scan::new`).
//!
//! ```text
//! summoner-scan-probe <bundle.vst3|bundle.component> [--step-ms <n>]
//! ```
//!
//! - Exit 0: a report was printed (`ok` or `failed`; the outcome is in the report).
//! - Exit 64: not a `scan` the host application could send (not an absolute `.vst3` / `.component` bundle path):
//!   nothing scanned.
//! - Exit 75: a step passed its deadline (`--step-ms`, default 25 000, as the child's scan mode): stderr names the
//!   step. The child never reports `timeout`; the supervisor records it, from this exit or from its own 30 s budget.
//! - Killed by a signal: the plugin crashed (the supervisor's `crashed`).
//!
//! The order is the library's: the lab gate, then the Mach-O check (nothing loaded before it), then discovery, then
//! validation. Around it, the no-UI rules (through summoner-sys-ui):
//! - NO NSApplication (making one brings the WindowServer, LaunchServices and a TCC ListenEvent preflight): this
//!   process never makes one, and AppKit is not even linked;
//! - NO window listing either: CGWindowListCopyWindowInfo alone connects the process to the WindowServer, which
//!   preflights TCC ListenEvent and ScreenCapture for it. The no-UI watchdog (`failed {ui}`) is PARKED, with
//!   sys-ui's `windows` behind a feature nothing enables; `probe.windowChecks` stays 0;
//! - the activation-policy check (a plugin may make a Regular NSApplication): PARKED with it (sys-ui's `nsapp`,
//!   unused);
//!   `probe.policyAfter` stays null.
//!
//! So this process makes no WindowServer contact at all (scan.rs asserts no `windowserver.*` lookup).
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use summoner_probe::Report;

const DEADLINE_EXIT: i32 = 75;

struct Watched {
    now: Arc<Mutex<(String, Instant)>>,
}

impl summoner_probe::Step for Watched {
    fn step(&self, name: &str) {
        *self.now.lock().unwrap() = (name.to_string(), Instant::now());
    }
}

/// The report goes out once: from main, or from the watchdog (which then exits).
static PRINTED: AtomicBool = AtomicBool::new(false);

fn print_once(r: &Report) -> bool {
    if PRINTED.swap(true, Ordering::AcqRel) {
        return false;
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string(r).expect("a report serialises")
    );
    let _ = out.flush();
    true
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(bundle) = args
        .get(1)
        .filter(|a| !a.starts_with("--"))
        .map(PathBuf::from)
    else {
        eprintln!("usage: summoner-scan-probe <bundle.vst3|bundle.component> [--step-ms <n>]");
        std::process::exit(64);
    };
    // The host application's own check of a `scan` (an absolute bundle path naming its format): the probe takes only
    // what the host application could send.
    let format = match bundle.extension().and_then(|e| e.to_str()) {
        Some("vst3") => summoner_probe::ScanFormat::Vst3,
        Some("component") => summoner_probe::ScanFormat::Au,
        _ => {
            eprintln!(
                "summoner-scan-probe: {}: not a .vst3 or .component bundle",
                bundle.display()
            );
            std::process::exit(64);
        }
    };
    let scan = match summoner_probe::Scan::new(format, bundle.display().to_string()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("summoner-scan-probe: {e}");
            std::process::exit(64);
        }
    };
    let step_ms: u64 = args
        .iter()
        .position(|a| a == "--step-ms")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(25_000); // as the child's scan mode (real plugins need ~21 s)
    let now = Arc::new(Mutex::new(("start".to_string(), Instant::now())));
    {
        let now = now.clone();
        let deadline = Duration::from_millis(step_ms);
        std::thread::Builder::new()
            .name("scan-deadline".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(50));
                    let (step, since) = now.lock().unwrap().clone();
                    if since.elapsed() > deadline {
                        eprintln!(
                            "summoner-scan-probe: step {step} passed its {step_ms} ms deadline"
                        );
                        // `_exit`: the scan thread is inside plugin code (no static destructors, no atexit).
                        summoner_sys::proc_exit::exit_now(DEADLINE_EXIT);
                    }
                }
            })
            .expect("the deadline thread");
    }
    let before = summoner_sys::dyld::images();
    let mut report = summoner_probe::scan(&scan, &Watched { now });
    report.probe.loaded_images = summoner_sys::dyld::images()
        .into_iter()
        .filter(|i| !before.contains(i))
        .collect();
    print_once(&report);
    // The bundle's code is loaded: `_exit` (stdout flushed first), so no plugin's static destructors run at exit
    // (as the host child exits).
    let _ = std::io::stdout().flush();
    summoner_sys::proc_exit::exit_now(0);
}
