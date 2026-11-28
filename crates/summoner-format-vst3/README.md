![Summoner](../../docs/images/plugin-host.png)

# summoner-format-vst3

**VST3 plugins as chain slots.**

`summoner-format-vst3` is the crate that hosts VST3 plugins in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A VST3 slot is a bundle and a class id (`desc::Description`, or `desc::of_ref` from a plugin reference). `load`
opens the module by its absolute path (after the development fence), picks the Audio Module class with that id, sets
up the buses, and returns a `Vst3Plugin`, a [`summoner_proc::Plugin`](../summoner-proc/README.md):

- **Parameters:** the region's index `i` is the controller's parameter `i`; a value set before a block becomes a
  point at offset 0 of that block. A plugin whose parameter count lies (a failed info, a repeated id) has those
  indices reported but never driven, and the dropped values are counted.
- **MIDI:** notes become VST3 note events; controllers, channel pressure and pitch bend go through the plugin's
  `IMidiMapping` when it maps them; poly pressure, program change and MTC quarter frames go in as their VST3 events.
- **Transport:** the block's `Transport` fills `ProcessContext`, each field marked valid only when the host gave it.
- **State:** the component chunk and the controller chunk, framed together by the host as one state blob.
- **Failures:** a C++ exception from the plugin's `process` is caught in the shim and becomes a failed, silent block.

| Item | What it is |
|---|---|
| `load(&Description, ch_in, ch_out)` | a loaded slot, or an `Error` with the reason |
| `Vst3Plugin` | the slot: `summoner_proc::Plugin`, plus `chunk` / `set_chunk`, `buses`, `take_restart` |
| `desc::Description`, `desc::of_ref`, `desc::parse_class_id`, `desc::class_id_hex`, `desc::match_class` | a slot's identity and its class lookup |

The `summoner-vst3-probe` binary in this crate loads one bundle and prints what it found, for checking a plugin by
hand.

## How it fits

Below it: [`summoner-sys`](../summoner-sys/README.md)'s `vst3` shim (the only place the SDK is called) and
[`summoner-plugin`](../summoner-plugin/README.md)'s slot rules (parameter order, name limits). Above it: the scan
probe and the host program, which run it out of your process. Load plugins in your own process only in tools and
tests: crash isolation is what the host program is for.

## Quick start

```rust
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
```

The VST3 SDK is fetched at build time by its pinned commit (`tools/vst3-sdk.sh`, `SUMMONER_VST3_SDK_CACHE`), never
copied into this repository.

## Realtime and safety

- `#![forbid(unsafe_code)]`: every SDK call is in `summoner-sys`.
- The block path runs on a chain thread with no allocation; parameter points and note events go into buffers sized
  at `prepare`.
- Plugins are untrusted: only development bundles load, every value a plugin reports (counts, ids, bus widths) is
  bounded, and an exception never unwinds into Rust.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. Loading is macOS-only today (bundles, the dynamic loader); VST3 is the first format planned for Windows and Linux.

## Trademarks

VST is a registered trademark of Steinberg Media Technologies GmbH.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-format-au`](../summoner-format-au/README.md): Audio Unit slots.
- [`summoner-probe`](../summoner-probe/README.md): scanning a bundle before it is loaded.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
