//! Load an Audio Unit (v2) as a slot and run one block through it. A slot is the bundle's absolute path and its
//! component's three codes (type, subtype, manufacturer); the bundle is registered in this process only (never in
//! the system's registry), and `load` gives a `summoner_proc::Plugin`: prepared, it processes a stereo block of a
//! test tone in place.
//!
//! ```sh
//! tools/build-plugins.sh                                   # the testkit's plugins, once
//! cargo run -p summoner-format-au --example summoner_format_au_quickstart
//! ```
//!
//! Only development bundles load (the build's `SUMMONER_DEV_PLUGIN_ROOTS` fence); this one is the testkit's gain.
use summoner_format_au::desc::{Description, check_codes};
use summoner_proc::{Block, Plugin, Transport, with_channels};
use summoner_sys::bundle::{Format, allowed_root};

/// A four-character code as the component's description carries it.
fn fourcc(b: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*b)
}

fn main() -> Result<(), String> {
    let bundle = allowed_root(Format::Au)
        .ok_or("no development plugin root in this build")?
        .join("summoner-lab-gain.component");
    let codes = [fourcc(b"aufx"), fourcc(b"SLgn"), fourcc(b"SaSL")];
    check_codes(codes).map_err(|e| e.as_str().to_string())?;
    let desc = Description::new(String::new(), bundle.display().to_string(), codes);

    let plugin = summoner_format_au::load(&desc, 2, 2).map_err(|e| e.as_str().to_string())?;
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
        "{} ({}): {} parameters, peak {peak:.4} after one block",
        plugin.name(),
        plugin.codes(),
        plugin.param_count()
    );
    Ok(())
}
