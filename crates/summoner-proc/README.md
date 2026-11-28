![Summoner](../../docs/images/plugin-host.png)

# summoner-proc

**The slot traits: what the host program runs in each place of a chain.**

`summoner-proc` is the crate that defines what a hosted plugin slot is in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A chain is a row of slots, and every slot, whatever its format, is two values split by thread:

- a **`Plugin`** (`Send + Sync`), called on the host program's message thread: its name, latency, parameters,
  state, editor and `prepare`; `set_param` alone is also called on the chain thread, just before a block;
- a **`Dsp`**, made by `Plugin::prepare` and owned by the chain thread: `process` runs one `Block` in place.

The host program owns everything protocol-shaped (the shared-memory region, MIDI order, the reset's all-notes-off,
the transport, the acknowledgements); an implementation sees only plain buffers, a sorted MIDI list and a
`Transport`. Three implementations exist: the test double ([`summoner-testproc`](../summoner-testproc/README.md)),
VST3 ([`summoner-format-vst3`](../summoner-format-vst3/README.md)) and Audio Unit
([`summoner-format-au`](../summoner-format-au/README.md)).

| Item | What it is |
|---|---|
| `Plugin` | the message-thread side; most methods have defaults, eight are required (`name`, `latency`, `param_count`, `param_name`, `set_param`, `set_state`, `get_state`, `prepare`) |
| `Dsp` | the chain-thread side: `io`, `reset`, `process`, and `reactivate` for a plugin that asks to be restarted |
| `Block` | one block: `frames`, the channel buffers (`runecraft_rt::arena::ArenaSlice<f32>`), `midi`, `transport` |
| `MidiEvent`, `Transport`, `Io` | the block's MIDI (up to 16 bytes, at a frame offset), musical time, and the buses a processor takes and writes |
| `StateKind`, `Chunks`, `SetOutcome` | how a slot's state is framed: one built-in blob, or the VST3 component and controller chunks |
| `ParamMeta`, `Notice` | a parameter's metadata, and what `Plugin::poll` found (a changed latency, a restart request) |
| `with_channels`, `unwired_channels`, `warm_channels`, `rt_line`, `RtVec` | buffers for callers that are not realtime (scans, tests) and for the chain threads' wired memory |

## How it fits

Below it: [`runecraft-rt`](https://signalsandsorcery.com/components/runecraft-audio-core/) (the wired arena the
channel buffers live in) and `runecraft-ipc`. Above it: the format crates, the scan probe and the host program.
Your application does not implement these traits to host plugins (the format crates do); you implement them to put
your own processor in a chain, or to test a host without plugins.

What it must not do: no protocol, no threads, no files. A slot never sees the region or the control channel.

## Quick start

```rust
//! A slot of your own: a one-parameter gain as a `Plugin` (the shared, message-thread side) and the `Dsp` its
//! `prepare` makes (owned by the chain thread), then one stereo block processed in place, as the host program runs
//! every slot. Run it with `cargo run -p summoner-proc --example summoner_proc_quickstart`.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use summoner_proc::{Block, Dsp, Error, Io, Plugin, Transport, with_channels};

/// The plugin: its one parameter lives in an atomic, because `set_param` is called on the chain thread.
struct Gain {
    gain: Arc<AtomicU32>,
}

/// The prepared processor: it reads the same atomic at each block.
struct GainDsp {
    gain: Arc<AtomicU32>,
}

impl Plugin for Gain {
    fn name(&self) -> &str {
        "Example Gain"
    }
    fn latency(&self) -> u32 {
        0
    }
    fn param_count(&self) -> u32 {
        1
    }
    fn param_name(&self, _index: u32) -> String {
        "Gain".into()
    }
    fn set_param(&self, index: u32, value: f32) {
        if index == 0 {
            self.gain.store(value.to_bits(), Ordering::Relaxed);
        }
    }
    fn set_state(&self, blob: &[u8]) -> Result<(), Error> {
        let bytes: [u8; 4] = blob
            .try_into()
            .map_err(|_| Error::from("a gain state is 4 bytes"))?;
        self.gain
            .store(u32::from_le_bytes(bytes), Ordering::Relaxed);
        Ok(())
    }
    fn get_state(&self) -> Vec<u8> {
        self.gain.load(Ordering::Relaxed).to_le_bytes().to_vec()
    }
    fn prepare(&self, _rate: f64, _max_frames: usize) -> Box<dyn Dsp> {
        Box::new(GainDsp {
            gain: self.gain.clone(),
        })
    }
}

impl Dsp for GainDsp {
    fn io(&self) -> Io {
        Io::STEREO
    }
    fn reset(&mut self) {}
    fn process(&mut self, block: &mut Block<'_>) -> bool {
        // The block's channels are realtime buffers: no allocation, no lock, no system call in here.
        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        for ch in block.channels.iter_mut() {
            for s in &mut ch[..block.frames] {
                *s *= g;
            }
        }
        true
    }
}

fn main() {
    let plugin = Gain {
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
    };
    plugin.set_param(0, 0.5);
    let mut dsp = plugin.prepare(48_000.0, 64);
    let mut channels = vec![vec![1.0f32; 64]; 2];
    let transport = Transport::default();
    let ok = with_channels(&mut channels, |channels| {
        dsp.process(&mut Block {
            frames: 64,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    assert!(ok && channels.iter().flatten().all(|s| *s == 0.5));
    // The state round trip a host uses to save and restore the slot.
    let saved = plugin.get_state();
    plugin.set_param(0, 1.0);
    plugin.set_state(&saved).expect("its own state");
    assert_eq!(plugin.get_state(), saved);
    println!("{}: one block at gain 0.5, state {saved:?}", plugin.name());
}

#[test]
fn the_example_runs() {
    main();
}
```

```sh
cargo run -p summoner-proc --example summoner_proc_quickstart
```

## Realtime and safety

- `#![forbid(unsafe_code)]`.
- `Dsp::process` runs on a realtime chain thread: an implementation must not allocate, lock or make a blocking call
  there. The channel buffers are wired arena slices, sized at `prepare`, so nothing needs to grow in a block.
- `process` returns `false` for a failed block (a plugin exception caught at a format's boundary); the host program
  then makes the block silent and reports it, with no unwinding on the chain thread.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. This crate is plain Rust; `ParentWindow` already names the Windows (`Hwnd`) and X11 parents an editor will attach to.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-testproc`](../summoner-testproc/README.md): the test double, the smallest real implementation.
- [`summoner-format-vst3`](../summoner-format-vst3/README.md): VST3 slots.
- [`summoner-format-au`](../summoner-format-au/README.md): Audio Unit slots.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
