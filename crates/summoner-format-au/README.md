![Summoner](../../docs/images/plugin-host.png)

# summoner-format-au

**Audio Unit (v2) plugins as chain slots.**

`summoner-format-au` is the crate that hosts Audio Unit plugins in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

An Audio Unit slot is a bundle and its component's three codes (type, subtype, manufacturer: `desc::Description`, or
`desc::of_ref` from a plugin reference). The bundle is registered in this process only (never in the system's
registry), and `load` returns an `AuPlugin`, a [`summoner_proc::Plugin`](../summoner-proc/README.md):

- **Parameters:** in the unit's `ParameterList` order; a value `v` becomes `min + range * v`, set on the chain thread
  before the render. A repeated id is reported but its values are dropped and counted.
- **MIDI:** only to the kinds that take it (music devices, music effects, MIDI processors): short messages through
  `MusicDeviceMIDIEvent`, longer ones as SysEx.
- **Latency:** `round(seconds * rate)`, re-read after prepare and state, and whenever the unit's listener says it
  changed.
- **State:** the unit's `ClassInfo` as a binary property list; a refused state is reported, not applied.
- **Failures:** a failed render (an error status, or an output list the unit rewrote) is a failed, silent block.

| Item | What it is |
|---|---|
| `load(&Description, ch_in, ch_out)` | a loaded slot, or an `Error` with the reason |
| `AuPlugin` | the slot: `summoner_proc::Plugin`, plus `codes`, `input_lines`, `inputs_refused`, `listening`, `raw_param` |
| `pick_by_triple` | the entry with exactly these codes, never a fallback to the first |
| `desc::Description`, `desc::of_ref`, `desc::check_codes`, `desc::codes_string`, `desc::wants_midi` | a slot's identity and its rules |

The `summoner-au-probe` binary in this crate loads one bundle and prints what it found.

## How it fits

Below it: [`summoner-sys`](../summoner-sys/README.md)'s `au` module (AudioToolbox behind owned handles) and
[`summoner-plugin`](../summoner-plugin/README.md)'s slot rules. Above it: the scan probe and the host program, which
run it out of your process.

## Quick start

```rust
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
```

## Realtime and safety

- `#![forbid(unsafe_code)]`: every AudioToolbox call is in `summoner-sys`.
- The render runs on a chain thread with no allocation; parameter changes are applied before it, not inside it.
- Plugins are untrusted: only development bundles load, registered in this process only, and the codes must name
  exactly one entry.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. Audio Units are a macOS format, so this crate stays macOS-only; Windows and Linux hosting is VST3's.

## Trademarks

Audio Unit is a trademark of Apple Inc.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-format-vst3`](../summoner-format-vst3/README.md): VST3 slots.
- [`summoner-probe`](../summoner-probe/README.md): scanning a bundle before it is loaded.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
