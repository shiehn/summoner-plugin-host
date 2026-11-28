![Summoner](../../docs/images/plugin-host.png)

# summoner-testkit

**A test double of Summoner's host program: the real protocol over trivial DSP, its behaviour set by knobs, so your
tests can exercise every failure path without a single third-party plugin.**

`summoner-testkit` is the crate that holds the test double of the host program in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

`fake_host::main` is a whole host program. It speaks the real protocol (descriptor 3, the mach route, the shared
region, the wake page, the control frames, the scan mode) and runs trivial DSP in each slot: a gain, an optional
delay that reports its latency, an instrument gate that sounds while notes are held, a transport probe that writes
the transport it received into its output so a test can read it back. Knobs on a slot's test-double reference make
it misbehave on purpose:

- crash while loading, or inside a block;
- hang (wedge) or burn time in a block, so the wedge detector and the miss path run;
- keep processing while disabled, ignore all-notes-off, hold notes through the sustain pedal;
- report extra parameters, parameter groups and steps, and keep a state body that `param.set` changes.

A scan script (comma-separated `key=value` knobs in the `SUMMONER_FAKE_SCAN` environment variable) makes scan mode
report what a test asks for.

## How it fits

Wrap `fake_host::main` in a binary of your own (the quick start below is one) and pass its path to the
[`summoner`](../summoner/README.md) supervisor's `Config::new` in place of
[`summoner-host`](../summoner-host/README.md). Test-double references are built with
[`summoner-proto`](../summoner-proto/README.md)'s `PluginRef::fake`, behind its `test-double` feature. The slot rules
are [`summoner-plugin`](../summoner-plugin/README.md)'s, the same the real host program applies, so a test sees the
channel rule and the parameter order real plugins get.

Never link it into a shipped build: it exists to fail on request. Its destructive knobs, and its scan mode, act only when
the environment variable named by `fake_host::DEATH_SWITCH_ENV` is exactly `kill`.

## Quick start

```rust
//! The test double as a program of your own: a host program that speaks the real protocol over trivial DSP, for
//! tests that must exercise the supervisor's every path (a crash, a hang, a slow block) without a third-party plugin.
//! The supervisor spawns it like the real host program.
//!
//! ```sh
//! cargo build -p summoner-testkit --example summoner_testkit_quickstart
//! cargo run -p summoner --example summoner_quickstart -- target/debug/examples/summoner_testkit_quickstart
//! ```
fn main() {
    summoner_testkit::fake_host::main();
}
```

```sh
cargo build -p summoner-testkit --example summoner_testkit_quickstart
cargo run -p summoner --example summoner_quickstart -- target/debug/examples/summoner_testkit_quickstart
```

## The API

| Item | What it is |
|---|---|
| `fake_host::main` | the whole test-double program |
| `fake_host::SCAN_SCRIPT_ENV` | the environment variable that scripts scan mode |
| `fake_host::DEATH_SWITCH_ENV` | the environment variable that arms the destructive knobs |

## Realtime and safety

- `#![forbid(unsafe_code)]`: safe code only.
- Its chain thread follows the same rules as the real host program's (no allocation, no lock), except where a knob
  asks it to break them.
- It watches its parent and exits when the parent is gone, so a crashed test leaves no orphan behind.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner`](../summoner/README.md): the supervisor it stands in front of
- [`summoner-testproc`](../summoner-testproc/README.md): the in-process test double for one slot
- [`summoner-host`](../summoner-host/README.md): the real host program
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
