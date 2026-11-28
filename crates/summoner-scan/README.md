![Summoner](../../docs/images/plugin-host.png)

# summoner-scan

**Plugin scanning that cannot take your application down: one probe per process, in its own process group, under a
time budget, with a verdict for every plugin.**

`summoner-scan` is the crate that scans plugin bundles in processes of their own in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Scanning is where plugins misbehave most: a scan loads code nobody has run in this process before. `ScanSupervisor`
never loads a plugin itself. For each bundle it:

1. asks your **pedal** (`Pedal::pending`) to record the bundle as pending, durably, before anything runs, so a scan
   that takes the whole machine down is still known on the next start;
2. spawns the host program in scan mode (`--scan`), as the leader of **a process group of its own**, with its stderr
   captured into a bounded buffer;
3. sends one `scan` request and reads the answer through a reader with a blob budget (`SCAN_BLOB_BUDGET`);
4. enforces **a time budget** (`BUDGET`, 30 s for a VST3 bundle; `BUDGET_AU`, 70 s for an Audio Unit) and a step
   deadline the probe applies to each step, then kills the whole group, so a probe's own children cannot outlive it;
5. turns whatever happened into **a verdict** (`ScanEnd`): the classes found, or a failure, a timeout or a crash
   (with the crash log when one was written), and says whether it is a strike against the plugin (`is_strike`) or a
   fault of the host's own (a usage error, a protocol fault, an engine-side limit), which never counts against the
   plugin;
6. calls `Pedal::settle` with the report.

`ScanSupervisor::scan` always returns: a leader that ignores its kill is handed to a reaper thread after
`OUTER_LIMIT`, so the caller is never blocked forever.

## How it fits

Your application calls `summoner-scan` from a control thread, before it loads chains with the
[`summoner`](../summoner/README.md) supervisor (which this crate shares its spawn rules and crash-log folder check
with). The request and the answer are [`summoner-proto`](../summoner-proto/README.md)'s `Scan` and `Scanned`. The
probing itself (opening the bundle, instantiating each class, a few blocks, a state round trip) happens in the host
program, [`summoner-host`](../summoner-host/README.md), through [`summoner-probe`](../summoner-probe/README.md).
Below it are Runecraft's process calls (spawn, process groups, the group kill, waits).

It keeps no plugin list and no cache: the pedal is your application's, and so is the decision of what to do with a
strike.

## Quick start

```rust
//! Scan one plugin bundle in a process of its own. The scan supervisor spawns the host program in scan mode, in its
//! own process group, under a time budget, and turns whatever happens (a report, a crash, a hang, a protocol fault)
//! into a verdict. The pedal is where your application records a scan as pending before the spawn, so a scan that
//! takes the whole application down is still known on the next start.
//!
//! ```sh
//! cargo build -p summoner-host
//! cargo run -p summoner-scan --example summoner_scan_quickstart -- target/debug/summoner-host /Library/Audio/Plug-Ins/VST3/X.vst3
//! ```
use summoner_proto::codec::{Scan, ScanFormat};
use summoner_scan::scan::{Pedal, ScanConfig, ScanReport, ScanSupervisor};

/// A pedal that keeps its marks in memory. A real one writes them durably before it returns.
#[derive(Default)]
struct MemoryPedal {
    pending: Vec<String>,
}

impl Pedal for MemoryPedal {
    fn pending(&mut self, scan: &Scan) -> std::io::Result<()> {
        self.pending.push(scan.path().to_string());
        Ok(())
    }

    fn settle(&mut self, report: &ScanReport) -> std::io::Result<()> {
        self.pending.retain(|p| p != report.scan.path());
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(program), Some(bundle)) = (args.next(), args.next()) else {
        eprintln!("usage: summoner_scan_quickstart <host program> <bundle.vst3>");
        return Ok(());
    };
    let mut supervisor = ScanSupervisor::new(ScanConfig::new(program.into(), vec![]));
    let mut pedal = MemoryPedal::default();

    // The request is checked when it is made: an absolute bundle path of the named format.
    let scan = Scan::new(ScanFormat::Vst3, &bundle)?;
    let report = supervisor.scan(&scan, &mut pedal);

    println!(
        "{bundle}: {:?} in {:?}",
        report.end.verdict(),
        report.elapsed
    );
    if report.end.is_strike() {
        println!("a strike: the bundle crashed, hung or misbehaved during its scan");
    }
    for class in &report.discovered {
        println!("  found {}", class.name());
    }
    assert!(pedal.pending.is_empty(), "every started scan is settled");
    Ok(())
}
```

```sh
cargo build -p summoner-host
cargo run -p summoner-scan --example summoner_scan_quickstart -- target/debug/summoner-host <bundle.vst3>
```

## The API

| Item | What it is |
|---|---|
| `scan::ScanSupervisor` | `new(config)`, `scan(&scan, &mut pedal) -> ScanReport`, `counters` |
| `scan::ScanConfig` | `ScanConfig::new(program, env)`: the program, its environment, the budgets |
| `scan::Pedal` | your durable pending mark: `pending` before the spawn, `settle` after |
| `scan::ScanReport` | everything about one scan: the end, the exit, the captured stderr, the timings |
| `scan::ScanEnd` | the verdict, or why there is none (`NotRun`, `EngineFault`, `HostFault`) |
| `scan::Captured`, `scan::Bounded` | bounded capture of a probe's stderr |
| `scan::BUDGET`, `BUDGET_AU`, `STEP`, `STEP_AU`, `OUTER_LIMIT` | the default limits |

## Realtime and safety

- Control-side only: a scan blocks its caller for up to its budget, so run it on a thread of its own, never on the
  audio thread.
- `#![forbid(unsafe_code)]`: the process calls are Runecraft's.
- **The probe is untrusted.** Its answer goes through the codec's checks and the reader's blob budget, its stderr is
  capped (`STDERR_CAP`), and its exit status is judged, never believed.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

Process groups and the group kill are POSIX; the Audio Unit budgets apply on macOS only.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner`](../summoner/README.md): the supervisor for loading and running chains
- [`summoner-probe`](../summoner-probe/README.md): what the host program does in scan mode
- [`summoner-host`](../summoner-host/README.md): the host program
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
