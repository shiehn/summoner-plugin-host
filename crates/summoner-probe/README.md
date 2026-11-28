![Summoner](../../docs/images/plugin-host.png)

# summoner-probe

**One plugin bundle scanned: its classes discovered, then each one validated.**

`summoner-probe` is the crate that does the work of a plugin scan in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A scan of a plugin folder runs one process per bundle (the [`summoner-scan`](../summoner-scan/README.md) supervisor
starts them, with a time budget, in their own process group). This crate is what runs inside that process, for one
bundle, in a fixed order where each stage runs only if the one before allowed it:

1. **the development fence** (`summoner_sys::bundle::Bundle::check`), before anything of the bundle is opened;
2. **the architecture** (`macho`): the executable's own Mach-O header, before any load; no arm64 slice is a
   `failed` report with nothing loaded;
3. **discovery**: each class's raw facts (`ScanClass`), from VST3 `moduleinfo.json` or the factory, or from an Audio
   Unit bundle's `AudioComponents` entries;
4. **validation**: each class instantiated, prepared at 48 kHz / 512, run for 8 blocks (one note in the first for an
   instrument), its state round-tripped through the state blob, and torn down.

The result is a `Report`: the protocol's `Scanned` (`ok` with the classes, or `failed` with a reason and a detail),
plus what the probe saw (`Probe`: the references, each validation, the loaded images). A crash or a timeout is never
reported from inside: those are the supervisor's verdicts, from outside the process.

| Item | What it is |
|---|---|
| `scan(&Scan, &dyn Step)` | the whole order above, for one bundle; `Step` names each stage (`NoSteps` for none) |
| `Report`, `Probe`, `Validated` | the outcome, and what was found and checked |
| `discover_vst3`, `discover_au`, `plugin_ref` | discovery alone, and a class's plugin reference |
| `macho::arch_of`, `macho::executable` | the architecture check |
| `failed_report`, `word` | a refusal as a report |

## How it fits

Below it: the format crates ([`summoner-format-vst3`](../summoner-format-vst3/README.md),
[`summoner-format-au`](../summoner-format-au/README.md)) and [`summoner-sys`](../summoner-sys/README.md). Above it:
the scan process ([`summoner-probe-bin`](../summoner-probe-bin/README.md), and the host program's `--scan` mode),
which the [`summoner-scan`](../summoner-scan/README.md) supervisor runs. Your application asks the supervisor for a
scan; it never runs this in its own process (a plugin may crash during a scan, and that is the point of the process).

## Quick start

The example scans the testkit's gain plugin, loads the first class it reports, and renders one block through it, all
in this process (the real scan runs each bundle in a process of its own):

```rust
//! Scan one testkit plugin and load it, offline: the plugin host's path from a bundle on disk to rendered audio,
//! without the supervisor, the host program or an audio device.
//!
//! 1. `tools/build-plugins.sh` builds the testkit's lab plugins into `target.noindex/plugins/`.
//! 2. [`summoner_probe::scan`] discovers the bundle's classes and validates each (in this process here; the host runs
//!    every scan in a child process of its own).
//! 3. The first reference it reports becomes a slot description ([`summoner_format_vst3::desc::of_ref`]), which
//!    [`summoner_format_vst3::load`] loads; the bundle must pass the dev fence (`SUMMONER_DEV_PLUGIN_ROOTS`, compiled
//!    in) like every load.
//! 4. One 64-frame stereo block of a test tone is rendered through it, and its peak printed.
//!
//! ```sh
//! cargo run -p summoner-probe --example scan_and_load [-- <bundle.vst3>]
//! ```
//!
//! With no argument it takes the testkit's gain plugin. `cargo test -p summoner-probe --example scan_and_load` runs
//! the same path as a test.
use std::path::{Path, PathBuf};

use summoner_probe::{NoSteps, Scan, ScanFormat, scan};
use summoner_proc::Plugin;

/// The testkit's gain plugin, as `tools/build-plugins.sh` builds it.
fn default_bundle() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target.noindex/plugins/vst3/summoner-lab-gain.vst3")
}

/// What the example found: the plugin's name, its parameter count and the block's peak.
struct Run {
    name: String,
    params: u32,
    peak: f32,
}

fn run(bundle: &Path) -> Result<Run, String> {
    let bundle = bundle.canonicalize().map_err(|e| {
        format!(
            "{}: {e} (run tools/build-plugins.sh first)",
            bundle.display()
        )
    })?;
    let path = bundle.display().to_string();
    let report = scan(
        &Scan::new(ScanFormat::Vst3, path.clone()).map_err(|e| format!("{e:?}"))?,
        &NoSteps,
    );
    let first = report
        .probe
        .refs
        .first()
        .ok_or_else(|| format!("{path}: the scan reported no plugin"))?;
    let desc = summoner_format_vst3::desc::of_ref(first).ok_or("not a VST3 reference")?;
    let plugin = summoner_format_vst3::load(&desc, 2, 2)?;
    let mut dsp = plugin.prepare(48_000.0, 512);
    let mut channels: Vec<Vec<f32>> = (0..2)
        .map(|_| (0..64).map(|i| 0.5 * (i as f32 * 0.1).sin()).collect())
        .collect();
    let transport = summoner_proc::Transport::default();
    let ok = summoner_proc::with_channels(&mut channels, |channels| {
        dsp.process(&mut summoner_proc::Block {
            frames: 64,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    if !ok {
        return Err("the block failed".into());
    }
    let peak = channels
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    Ok(Run {
        name: plugin.name().to_string(),
        params: plugin.param_count(),
        peak,
    })
}

fn main() {
    let bundle = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(default_bundle);
    match run(&bundle) {
        Ok(r) => println!(
            "{}: {} parameters, peak {:.4} after one block",
            r.name, r.params, r.peak
        ),
        Err(e) => {
            eprintln!("scan_and_load: {e}");
            std::process::exit(1);
        }
    }
}

// The test below skips, loudly, when the lab plugins are not built (it fails instead with
// SUMMONER_REQUIRE_LAB_PLUGINS=1).
#[cfg(test)]
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;

#[cfg(test)]
mod tests {
    use super::lab_plugins;

    #[test]
    fn the_example_scans_loads_and_renders_the_testkit_gain() {
        if lab_plugins::absent("the_example_scans_loads_and_renders_the_testkit_gain") {
            return;
        }
        let r = super::run(&super::default_bundle()).expect("the example runs");
        assert!(!r.name.is_empty());
        assert!(r.params > 0, "{}: no parameters", r.name);
        assert!(
            r.peak > 0.0 && r.peak.is_finite(),
            "{}: peak {}",
            r.name,
            r.peak
        );
    }
}
```

```sh
tools/build-plugins.sh                                       # the testkit's plugins, once
cargo run -p summoner-probe --example scan_and_load [-- <bundle.vst3>]
```

## Realtime and safety

- `#![forbid(unsafe_code)]`.
- Nothing here is realtime: a scan runs in its own process, off any audio thread.
- The bundle is untrusted from the first byte: the fence runs before it is opened, the architecture check before it is
  loaded, and every fact it reports is checked against the protocol's limits.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. Discovery and validation are macOS-only today (bundles, Mach-O, Audio Units); the VST3 path is the one planned for Windows and Linux.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-probe-bin`](../summoner-probe-bin/README.md): the scan process around this crate.
- [`summoner-scan`](../summoner-scan/README.md): the supervisor that runs scans out of process.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
