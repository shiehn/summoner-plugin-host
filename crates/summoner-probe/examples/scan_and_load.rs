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
