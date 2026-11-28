//! Load a VST3 plugin as a slot and run one block through it. The bundle's Audio Module classes are listed, the first
//! one becomes a slot description (the bundle's absolute path and the class id), and `load` gives a
//! `summoner_proc::Plugin`: prepared, it processes a stereo block of a test tone in place.
//!
//! ```sh
//! tools/build-plugins.sh                                   # the testkit's plugins, once
//! cargo run -p summoner-format-vst3 --example summoner_format_vst3_quickstart [-- <bundle.vst3>]
//! ```
//!
//! Only development bundles load (the build's `SUMMONER_DEV_PLUGIN_ROOTS` fence); with no argument it takes the
//! testkit's gain plugin.
use std::path::PathBuf;

use summoner_format_vst3::desc::{AUDIO_MODULE_CLASS, Description};
use summoner_proc::{Block, Plugin, Transport, with_channels};
use summoner_sys::bundle::{Bundle, Format, allowed_root};
use summoner_sys::vst3::Module;

fn main() -> Result<(), String> {
    let bundle = match std::env::args_os().nth(1) {
        Some(p) => PathBuf::from(p),
        None => allowed_root(Format::Vst3)
            .ok_or("no development plugin root in this build")?
            .join("summoner-lab-gain.vst3"),
    };
    let bundle = bundle
        .canonicalize()
        .map_err(|e| format!("{}: {e}", bundle.display()))?;
    // The gate runs before anything of the bundle is opened.
    let checked = Bundle::check(&bundle, Format::Vst3).map_err(|r| format!("refused: {r}"))?;
    let module = Module::open(&checked).map_err(|e| format!("{e:?}"))?;
    let class = module
        .classes()
        .into_iter()
        .find(|c| c.category == AUDIO_MODULE_CLASS)
        .ok_or("the bundle has no Audio Module class")?;
    let desc = Description::new(class.name.clone(), bundle.display().to_string(), class.cid);

    let plugin = summoner_format_vst3::load(&desc, 2, 2).map_err(|e| e.as_str().to_string())?;
    let mut dsp = plugin.prepare(48_000.0, 512);
    let mut channels: Vec<Vec<f32>> = (0..2)
        .map(|_| (0..64).map(|i| 0.5 * (i as f32 * 0.1).sin()).collect())
        .collect();
    let transport = Transport::default();
    let ok = with_channels(&mut channels, |channels| {
        dsp.process(&mut Block {
            frames: 64,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    if !ok {
        return Err("the plugin failed the block".into());
    }
    let peak = channels
        .iter()
        .flatten()
        .fold(0.0f32, |m, s| m.max(s.abs()));
    println!(
        "{}: {} parameters, latency {} frames, peak {peak:.4} after one block",
        plugin.name(),
        plugin.param_count(),
        plugin.latency()
    );
    Ok(())
}
