//! The gated scan of installed plugins: sandboxed, straight through, every bundle probed like the rest, results
//! kept in the lab with only counts leaving it. `#[ignore]`d: never part of a
//! workspace run; run by hand only, by a driver script that sets:
//!
//! - `SUMMONER_BULK_SCAN_ROOTS`: the folders to list, `:`-separated (the dry run: lab copies; the real run: the four
//!   folders);
//! - `SUMMONER_BULK_SCAN_PROGRAM`: what the host application's scan supervisor spawns for a VST3 (the sandbox wrapper
//!   around the shipped child);
//! - `SUMMONER_BULK_SCAN_PROGRAM_AU`: what it spawns for an AU: the shipped child itself, unsandboxed (VST3 is
//!   sandboxed, AU is not: AudioToolbox refuses in-process AU instantiation under any `sandbox-exec` profile, the dry
//!   run);
//! - `SUMMONER_BULK_SCAN_OUT`: the results directory (under the lab's transcripts; plugin names stay there);
//! - optional `SUMMONER_BULK_SCAN_FROM` (the first entry to probe, 1-based; default 1) and
//!   `SUMMONER_BULK_SCAN_DEADLINE_SECS` (start no probe after this many seconds; default 3300): a run longer than one
//!   fs_usage recording (3600 s) goes in batches, each
//!   its own recording (results-<from>.jsonl);
//! - optional `SUMMONER_BULK_SCAN_ONLY`: a file of bundle paths, one per line: Stage 1 probes only those (a re-probe).
//!
//! Stage 0 lists bundles with `readdir` / `lstat` only (plain non-bundle subfolders at most 3 deep; never into a
//! bundle; a symlink is listed as a link, never followed or probed), reading each executable's Mach-O header for its
//! architectures. Stage 1 probes each listed bundle, one at a time, through `summoner_scan::scan::ScanSupervisor` (the
//! production supervisor: its 30 s budget, its process-tree kill), and appends one JSON line per bundle.
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use summoner_proto::codec::{self, ScanFormat};
use summoner_scan::scan::{Pedal, ScanConfig, ScanEnd, ScanReport, ScanSupervisor};

struct NoPedal;
impl Pedal for NoPedal {
    fn pending(&mut self, _: &codec::Scan) -> std::io::Result<()> {
        Ok(())
    }
    fn settle(&mut self, _: &ScanReport) -> std::io::Result<()> {
        Ok(())
    }
}

/// One entry of Stage 0's list.
struct Entry {
    path: PathBuf,
    format: Option<ScanFormat>,
    link: bool,
}

fn format_of(name: &str) -> Option<ScanFormat> {
    let l = name.to_ascii_lowercase();
    if l.ends_with(".vst3") {
        Some(ScanFormat::Vst3)
    } else if l.ends_with(".component") {
        Some(ScanFormat::Au)
    } else {
        None
    }
}

fn list(dir: &Path, depth: u32, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<_> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    names.sort();
    for p in names {
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let format = format_of(&name);
        if md.file_type().is_symlink() {
            if format.is_some() {
                out.push(Entry {
                    path: p,
                    format,
                    link: true,
                });
            }
        } else if md.is_dir() {
            if format.is_some() {
                out.push(Entry {
                    path: p,
                    format,
                    link: false,
                });
            } else if depth < 3 {
                list(&p, depth + 1, out);
            }
        }
    }
}

fn arch(bundle: &Path) -> String {
    match summoner_probe::macho::executable(bundle)
        .map_err(String::from)
        .and_then(|e| summoner_probe::macho::arch_of(&e).map_err(|e| e.to_string()))
    {
        Ok(summoner_probe::macho::Arch::Arm64) => "arm64".into(),
        Ok(summoner_probe::macho::Arch::NotMachO) => "not-macho".into(),
        Ok(summoner_probe::macho::Arch::Other(t)) => format!(
            "no-arm64 {:?}",
            t.iter()
                .map(|c| summoner_probe::macho::cpu_name(*c))
                .collect::<Vec<_>>()
        ),
        Err(e) => format!("unreadable: {e}"),
    }
}

/// The child's environment: the lab env (HOME, CFFIXED_USER_HOME, TMPDIR) and a minimal PATH; nothing else.
fn child_env() -> Vec<(OsString, OsString)> {
    let mut env = Vec::new();
    for k in ["HOME", "CFFIXED_USER_HOME", "TMPDIR"] {
        let v = std::env::var_os(k).unwrap_or_else(|| panic!("{k}: run from the lab env"));
        env.push((k.into(), v));
    }
    env.push(("PATH".into(), "/usr/bin:/bin:/usr/sbin:/sbin".into()));
    env
}

#[test]
#[ignore]
fn bulk_scan() {
    let (Ok(roots), Ok(program), Ok(program_au), Ok(out)) = (
        std::env::var("SUMMONER_BULK_SCAN_ROOTS"),
        std::env::var("SUMMONER_BULK_SCAN_PROGRAM"),
        std::env::var("SUMMONER_BULK_SCAN_PROGRAM_AU"),
        std::env::var("SUMMONER_BULK_SCAN_OUT"),
    ) else {
        panic!("run through the gated-scan driver (it sets SUMMONER_BULK_SCAN_*)");
    };
    let home = std::env::var("HOME").unwrap();
    assert!(
        home == format!("{}/home", env!("SUMMONER_DEV_PLUGIN_ROOTS")),
        "the lab env: HOME is {home}"
    );
    let out = PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap();
    // Stage 0.
    let mut entries = Vec::new();
    for r in roots.split(':').filter(|r| !r.is_empty()) {
        list(Path::new(r), 0, &mut entries);
    }
    let mut lst = std::fs::File::create(out.join("list.tsv")).unwrap();
    for e in &entries {
        let a = if e.link {
            "link".to_string()
        } else {
            arch(&e.path)
        };
        writeln!(lst, "{}\t{:?}\t{a}", e.path.display(), e.format).unwrap();
    }
    eprintln!("bulk scan stage 0: {} entries", entries.len());
    // Stage 1.
    let mut sup = ScanSupervisor::new(ScanConfig::new(PathBuf::from(&program), child_env()));
    let mut sup_au = ScanSupervisor::new(ScanConfig::new(PathBuf::from(&program_au), child_env()));
    let from: usize = std::env::var("SUMMONER_BULK_SCAN_FROM")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1);
    let deadline = std::time::Duration::from_secs(
        std::env::var("SUMMONER_BULK_SCAN_DEADLINE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3300),
    );
    let only: Option<Vec<String>> = std::env::var("SUMMONER_BULK_SCAN_ONLY").ok().map(|f| {
        std::fs::read_to_string(f)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    });
    let started = Instant::now();
    let mut res = std::fs::File::create(out.join(format!("results-{from}.jsonl"))).unwrap();
    for (i, e) in entries.iter().enumerate().skip(from - 1) {
        if started.elapsed() > deadline {
            eprintln!(
                "bulk scan batch deadline: next SUMMONER_BULK_SCAN_FROM={}",
                i + 1
            );
            break;
        }
        let path = e.path.to_string_lossy().into_owned();
        if only.as_ref().is_some_and(|o| !o.contains(&path)) {
            continue;
        }
        let mut line = serde_json::json!({
            "path": path,
            "format": format!("{:?}", e.format),
        });
        if e.link {
            line["skipped"] = "a symlink: listed, not probed".into();
        } else {
            line["arch"] = arch(&e.path).into();
            match codec::Scan::new(e.format.unwrap(), path.clone()) {
                Err(why) => {
                    line["skipped"] =
                        format!("not a scan the host application could send: {why:?}").into()
                }
                Ok(scan) => {
                    let t = Instant::now();
                    let au = e.format == Some(ScanFormat::Au);
                    let no_sandbox =
                        std::env::var("SUMMONER_BULK_SCAN_NO_SANDBOX").is_ok_and(|v| v == "1");
                    line["sandboxed"] = (!au && !no_sandbox).into();
                    let r = if au { &mut sup_au } else { &mut sup }.scan(&scan, &mut NoPedal);
                    line["ms"] = (t.elapsed().as_millis() as u64).into();
                    line["exit"] = format!("{:?}", r.exit).into();
                    line["end"] = match &r.end {
                        ScanEnd::Verdict(v) => serde_json::to_value(v).unwrap(),
                        other => format!("{other:?}").into(),
                    };
                    let err = r.stderr.text();
                    line["reports"] = err
                        .lines()
                        .filter(|l| l.contains("REPORT (report only)"))
                        .map(String::from)
                        .collect::<Vec<_>>()
                        .into();
                    line["stderr_head"] = err.chars().take(4000).collect::<String>().into();
                }
            }
        }
        writeln!(res, "{line}").unwrap();
        res.flush().unwrap();
        eprintln!("bulk scan {}/{}: {}", i + 1, entries.len(), line["end"]);
    }
}
