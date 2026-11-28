![Summoner](../../docs/images/plugin-host.png)

# summoner-testproc

**A test double for a chain slot, configured by knobs, with no plugin on disk.**

`summoner-testproc` is the crate that provides a scriptable stand-in for a hosted plugin in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Testing a plugin host against real plugins is slow and flaky, and the failure paths (a crash in the middle of a
block, a hang, a state that is refused, a latency that changes at run time) are the ones a real plugin rarely shows on
demand. `summoner-testproc` is a `summoner_proc::Plugin` whose behaviour is set by the knobs of its plugin reference:

```text
{"format":"fake","fake":{"name":"...","gain":"g","delay":"n","synth":"1","sustain":"1","probe":"1","echo":"e","feedback":"f"}}
```

- **Signal:** a gain, a decaying echo of `e` frames with feedback `f`, then a fixed delay of `n` frames (its
  reported latency); `synth` makes it an instrument gate that sounds while a note is held (with sustain-pedal rules);
  `probe` writes the transport it was given into the first frames of channel 0, so a host's transport can be checked.
- **Failures on demand:** parameters 1 to 3 crash, wedge or burn CPU inside the block; `crash-load` and
  `exit-load="n"` fail while the plugin is made; states `CRASH`, `SEGV`, `FAIL`, `SLOWGET=<ms>`, `SLOWSET=<ms>`,
  `BLOCKMS=<ms>` and `BIGGET=<n>` crash, refuse, stall or overflow the state paths.
- **Run-time changes:** state `latency=<n>`, `RESTART=<n>` and `REACTIVATE[=<ms>]` exercise latency notices and
  re-activation; `inputs`, `outputs` and `write` set the buses and how the block is written; `xparams` adds described
  parameters.

The crate documentation lists every knob and its exact rule.

## How it fits

It implements [`summoner-proc`](../summoner-proc/README.md)'s traits, and the host program
([`summoner-host`](../summoner-host/README.md)) loads it for a `fake` reference when it is built with its
`test-hooks` feature. A shipped host program never contains it. For a whole stand-in host program (the protocol over
trivial DSP, out of process), see [`summoner-testkit`](../summoner-testkit/README.md).

## Quick start

```rust
//! The test double as a slot: a plugin reference with knobs (`gain`, `delay`, `synth`, ...) becomes a `Config`, then
//! a `summoner_proc::Plugin` you can prepare and run, with no plugin bundle on disk. Here: a gain of 0.5 and a delay of
//! 3 frames (its reported latency) over one stereo block. Run it with
//! `cargo run -p summoner-testproc --example summoner_testproc_quickstart`.
use summoner_proc::{Block, Transport, with_channels};
use summoner_proto::plugin::PluginRef;
use summoner_testproc::{Config, make};

fn main() {
    let r: PluginRef = serde_json::from_str(
        r#"{"format":"fake","fake":{"name":"double","gain":"0.5","delay":"3"}}"#,
    )
    .expect("a test-double reference");
    let cfg = Config::from_ref(&r, 0).expect("the test double's knobs");
    assert_eq!((cfg.gain, cfg.delay, cfg.latency), (0.5, 3, 3));
    let plugin = make(cfg);
    assert_eq!((plugin.name(), plugin.latency()), ("double", 3));

    let mut dsp = plugin.prepare(48_000.0, 64);
    let mut channels = vec![vec![1.0f32; 16]; 2];
    let transport = Transport::default();
    let ok = with_channels(&mut channels, |channels| {
        dsp.process(&mut Block {
            frames: 16,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    assert!(ok);
    // The first 3 frames are the delay line's silence, then the input at half gain.
    assert_eq!(&channels[0][..4], &[0.0, 0.0, 0.0, 0.5]);
    println!(
        "{}: latency {} frames, block {:?}",
        plugin.name(),
        plugin.latency(),
        &channels[0][..6]
    );
}

#[test]
fn the_example_runs() {
    main();
}
```

```sh
cargo run -p summoner-testproc --example summoner_testproc_quickstart
```

## Realtime and safety

- `#![forbid(unsafe_code)]`.
- Its normal paths keep the realtime rules a real slot must keep (no allocation in `process`); its failure knobs
  break them on purpose, which is what they are for. Never enable it in a shipped build.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. The double itself is plain Rust; its crash knobs use `summoner-sys`, which is macOS-only today.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-proc`](../summoner-proc/README.md): the traits it implements.
- [`summoner-testkit`](../summoner-testkit/README.md): a whole stand-in host program for supervisor tests.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
