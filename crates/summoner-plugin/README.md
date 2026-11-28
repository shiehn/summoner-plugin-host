![Summoner](../../docs/images/plugin-host.png)

# summoner-plugin

**The rules a plugin host applies to every hosted slot: the channel rule, the parameter order and name limits, and
the handover when a plugin changes its latency.**

`summoner-plugin` is the crate that holds the slot rules every Summoner host applies the same way in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A host that loads third-party plugins has to make the same small decisions for every slot, and two hosts that make
them differently produce different audio and different parameter lists. This crate is those decisions, written
once, as pure functions and one small state machine:

- **The channel rule** ([`channels`](src/channels.rs)): a chain runs on a two-channel block, and each slot declares
  how many inputs it takes and how many outputs it writes. `channel_rule_before` adapts the block to the slot before
  its plugin runs (a mono-input slot hears the average of left and right, unused inputs are zeroed), and
  `channel_rule_after` adapts the result for the next slot (unused outputs are zeroed, a mono slot's output is copied
  to both channels). The arithmetic is specified to the operation (one `f32` add and one multiply by 0.5, no fused
  multiply-add), so every host gets the same bits.
- **Parameters** ([`params`](src/params.rs)): `vst3_unit_order` and `au_clump_order` flatten a plugin's parameter
  groups (VST3 units, Audio Unit clumps) into one depth-first order, without recursion, so a hostile plugin's group
  tree cannot overflow a stack; `cut_chars` cuts a name to a bounded length (`NAME_CHARS`, `LABEL_CHARS`) at a
  character boundary.
- **The latency-change handover** ([`handover`](src/handover.rs)): when a plugin reports a new latency it must be
  deactivated and reactivated, which cannot happen on the audio thread. The handover moves the slot's processor
  between its chain thread and the host program's main thread through one atomic state word (`HOSTED`, `DRAINING`,
  `FADING`, `OUT`, `RETURNING`), moved only by compare-and-swap. While the plugin is out, the slot plays a bypass
  signal delayed by its last latency (`DelayLine`), with a raised-cosine crossfade (`FADE` frames) on the way out and
  back, so the change is heard as a short crossfade rather than a click or a gap.

## How it fits

The host program ([`summoner-host`](../summoner-host/README.md)) applies these rules around every plugin call, and
so does the test double ([`summoner-testkit`](../summoner-testkit/README.md)), so tests see what real plugins see.
The supervisor ([`summoner`](../summoner/README.md)) uses the parameter limits and the handover's time budget to
judge what the host program reports. Below it are only [`summoner-rt`](../summoner-rt/README.md)'s atomics facade and
Runecraft's one-word realtime lock.

It loads no plugin and knows no plugin format's API: the format crates
([`summoner-format-vst3`](../summoner-format-vst3/README.md), [`summoner-format-au`](../summoner-format-au/README.md))
read the units, clumps and bus counts and pass plain numbers in.

## Quick start

```rust
//! The slot rules a host applies around every hosted plugin: the channel rule for a mono effect in a stereo chain,
//! the flattened parameter order of a plugin's parameter groups, and the name cut. Run it with
//! `cargo run -p summoner-plugin --example summoner_plugin_quickstart`.
use summoner_plugin::channels::{channel_rule_after, channel_rule_before};
use summoner_plugin::params::{NAME_CHARS, VST3_ROOT_UNIT, cut_chars, vst3_unit_order};

fn main() {
    // A stereo block of 4 frames, through a slot that takes one input channel and writes one output channel.
    let mut block = vec![vec![1.0f32, 0.5, 0.0, -1.0], vec![0.0f32, 0.5, 1.0, -1.0]];
    let (inputs, outputs, frames) = (1, 1, 4);
    channel_rule_before(inputs, outputs, &mut block, frames);
    // The plugin sees the average of left and right on channel 0, and silence on channel 1.
    assert_eq!(block[0], [0.5, 0.5, 0.5, -1.0]);
    assert_eq!(block[1], [0.0; 4]);
    // ... the plugin processes channel 0 here (a gain of 2, say) ...
    block[0].iter_mut().for_each(|s| *s *= 2.0);
    channel_rule_after(inputs, outputs, &mut block, frames);
    // The next slot gets the mono result on both channels.
    assert_eq!(block[0], block[1]);
    println!("after the slot: {:?}", block);

    // Parameters in groups: unit 1 ("Filter") under the root, unit 2 ("Envelope") under unit 1. Parameter i lives in
    // param_units[i]. A group sorts by its first parameter, so the walk is: unit 1, unit 2, then parameters 0 and 3
    // (unit 2), 2 (unit 1) and 1 (the root). The order is each parameter's position in that walk.
    let units = [(VST3_ROOT_UNIT, -1), (1, VST3_ROOT_UNIT), (2, 1)];
    let param_units = [2, VST3_ROOT_UNIT, 1, 2];
    let order = vst3_unit_order(&units, &param_units);
    assert_eq!(order, [0, 3, 2, 1]);
    println!("parameter order: {order:?}");

    // A plugin's parameter name is cut to a bounded length at a character boundary.
    let long = "Ä".repeat(NAME_CHARS + 10);
    assert_eq!(cut_chars(long, NAME_CHARS).chars().count(), NAME_CHARS);
}

#[test]
fn the_example_runs() {
    main();
}
```

```sh
cargo run -p summoner-plugin --example summoner_plugin_quickstart
```

## The API

| Item | What it is |
|---|---|
| `channels::channel_rule_before`, `channels::channel_rule_after` | the slot channel rule around one plugin call |
| `params::vst3_unit_order`, `params::au_clump_order` | the flattened parameter order |
| `params::cut_chars`, `NAME_CHARS`, `LABEL_CHARS` | the name and label cut |
| `handover::Cell` | one slot's processor and its state word |
| `handover::ChainSide` | the chain thread's side: `begin` and `end` around each block |
| `handover::DelayLine`, `handover::Fade`, `handover::Mix` | the bypass signal and the crossfade |
| `handover::take_bound`, `handover::cycle_budget` | the time budgets of a handover |

## Realtime and safety

- `#![forbid(unsafe_code)]`: safe code only.
- The channel rule touches only frames `0..frames` and never allocates, locks or changes the floating-point mode;
  NaN and infinities pass through unchanged in kind.
- The handover's chain-thread side never waits: it reaches the processor only through a `try_lock` (one
  compare-and-swap), and a slot whose processor is away passes its bypass signal through. Only the main thread
  ever waits, in bounded polls. Under `--cfg loom` its atomics come from loom, so the state machine can be
  model-checked.
- The parameter functions are for the control side; they allocate their result and are never called on the audio
  thread.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

The crate is plain Rust with no system calls of its own.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner-host`](../summoner-host/README.md): the host program that applies these rules
- [`summoner-proc`](../summoner-proc/README.md): the slot traits a hosted plugin implements
- [`summoner-rt`](../summoner-rt/README.md): the realtime exchange
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
