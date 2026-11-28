![Summoner](../../docs/images/plugin-host.png)

# summoner-host

**The host program: the helper process that loads plugins and runs their audio.**

`summoner-host` is the helper executable that runs plugins out of your application's process in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Your application links the [`summoner`](../summoner/README.md) supervisor, which spawns this program with its standard
streams and one end of a socket pair on descriptor 3, and nothing else. Everything after that is the protocol of
[`summoner-proto`](../summoner-proto/README.md): control frames over the socket, and audio, MIDI, parameters and the
transport through a shared-memory region with a doorbell. The program:

- loads chains of slots ([`summoner-format-vst3`](../summoner-format-vst3/README.md),
  [`summoner-format-au`](../summoner-format-au/README.md)), prepares them and runs each chain on its own realtime
  thread, with a time-constraint policy and the slot rules of [`summoner-plugin`](../summoner-plugin/README.md);
- reads and writes plugin state as framed blobs, lists and sets parameters, and opens and closes editor windows (it
  owns them);
- scans one bundle in `--scan` mode;
- logs a fatal signal with the plugin call in flight, and exits when its parent is gone.

Its threads: the message thread (`main`: load, prepare, state, editor, parameters, unload, shutdown), the reader
(`hc-read`: frames in, quick operations answered there, the rest posted to the message thread in order), the mach
channel (`hc-mach`), one chain thread per chain (`hc-chain-<key>`: the doorbell wait, the block, the acknowledgement)
and the orphan watch (`hc-orphan`). There is no event loop and no async runtime.

## How it fits

Above it: your application, through the supervisor; you never speak its protocol by hand. Below it: the slot crates,
[`summoner-sys`](../summoner-sys/README.md) for every system call, and the floors
([Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/),
[Orrery](https://signalsandsorcery.com/components/orrery-musical-time/)). It ships beside your application's
executable.

## Quick start

Build it, then point the supervisor at it (`summoner::supervisor::Config::new(<path to summoner-host>, sample_rate,
state_hasher)`):

```sh
tools/build-plugins.sh --fetch            # the VST3 and Audio Unit SDKs by their pins, and the testkit's plugins
cargo build -p summoner-host              # a development build
tools/build-shipped.sh                    # the build you ship: no test hooks, path-remapped, release-loading
target/debug/summoner-host --source-hash  # the hash of the sources this binary was built from
```

The flags the supervisor passes:

| Flag | What it does |
|---|---|
| `--scan` (with `--step-ms <n>`) | scan mode: one bundle, no chains |
| `--parent-pid <pid>` | the orphan watch's parent (default: the spawning parent) |
| `--tc-us <period>`, `--no-tc` | the chain threads' time-constraint period, or none |
| `--log-dir <dir>`, `--crash-log <dir>` | where the diagnostic log and the crash log go (a refused folder is exit 2) |
| `--stats <path>`, `--counters <path>` | each chain's block timings, and each slot's re-activation counters, written at shutdown |
| `--source-hash`, `--editor-dylib-path` | alone: print the build's source hash, or the editor library it resolves, and exit |

The `test-hooks` feature adds test-only flags and the test double ([`summoner-testproc`](../summoner-testproc/README.md)).
It is opt-in (the crate has no default features): the repository's tests need it, so run them with
`cargo test --workspace --features summoner-host/test-hooks`. A shipped build never has it (`tools/build-shipped.sh`
builds with `--no-default-features --features release-loading`).

Tests that need the testkit's plugins skip, with a `SKIPPED (no lab plugins)` line on stderr, until
`tools/build-plugins.sh` has built them; with `SUMMONER_REQUIRE_LAB_PLUGINS=1` they fail instead.

## Realtime and safety

- `#![forbid(unsafe_code)]`: every `unsafe` call is in [`summoner-sys`](../summoner-sys/README.md) or the
  [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) floor.
- A chain thread never allocates, locks or makes a blocking call in a block; it waits on the doorbell between blocks.
  The repository's tests run blocks under an allocation counter.
- It treats its peer as it is treated: every frame is length-checked, every region value bounded. A plugin that
  crashes takes only this process down; the supervisor sees it, blames the slot and respawns.
- It exits with `_exit` (no `atexit` handlers, no plugin's static destructors), and when its parent is gone.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. This program is macOS-only today (descriptor passing, mach ports, audio workgroups, AppKit editors).

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner`](../summoner/README.md): the supervisor that spawns it.
- [`summoner-proto`](../summoner-proto/README.md): the protocol it speaks.
- [`summoner-testkit`](../summoner-testkit/README.md): a stand-in host program for tests.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
