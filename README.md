![Summoner](docs/images/plugin-host.png)

# Summoner

**An out-of-process audio plugin host.**

Summoner is the component that loads and runs VST3 and Audio Unit plugins in a separate helper process, so a
crashing or hanging plugin can't take your application down, and is part of **Signals & Sorcery**, an ecosystem of
Rust audio building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com),
[Summoner's page](https://signalsandsorcery.com/components/summoner-plugin-host/)).

The crates described below, the library and the host program, are in this repository. They are not yet published to
crates.io, and some names and signatures may still change before their first release.

**Documentation:** [signalsandsorcery.com/components/summoner-plugin-host](https://signalsandsorcery.com/components/summoner-plugin-host/) · [all twelve components](https://signalsandsorcery.com/components/) · [for agents](https://signalsandsorcery.com/#for-agents) · [llms.txt](https://signalsandsorcery.com/llms.txt)

## What it does

Third-party plugins are the least trusted code in any audio application. A plugin can crash, deadlock, leak, take
too long, or corrupt memory, and when it runs inside your process, it takes your process with it. Summoner moves every
plugin into a helper process and gives your application a supervised, realtime-safe connection to it.

Summoner handles:

- **Scanning.** It finds plugins on disk and probes each one in its own short-lived process, with a time budget, so a
  plugin that crashes or hangs during a scan is recorded as such and never stops the scan.
- **Loading chains.** Your application asks for a chain of plugins (instruments and effects in order) and gets back a
  realtime link for it once every slot is loaded and prepared.
- **Realtime block exchange.** Audio, MIDI, parameter changes and the transport pass between your audio thread and the
  helper process through shared memory, with a doorbell to wake the other side. The audio thread never blocks, locks
  or allocates.
- **State.** Plugin state can be read and written as versioned, framed blobs. When your application saves, only the
  slots whose state changed are read back, inside a bounded time and byte budget.
- **Parameters.** Parameter lists are read in pages, and single parameters can be set by the plugin's own id.
- **Editors.** Plugin editor windows open and close in the helper process, which owns them.
- **Offline render.** A chain can run in a render mode with a synchronous, one-block-at-a-time exchange, so offline
  bounces use the same plugins and code as live playback.
- **Failure handling.** Crashes, hangs and protocol faults are detected, reported as events, and recovered from:
  the helper process is respawned with backoff, chains are reloaded with their last known state, and a plugin that
  keeps crashing is quarantined.

## How it is used

Summoner has two parts:

1. **The library**, which links into your application. It spawns and supervises the helper process, talks to it over
   a control channel, and gives your audio thread a link per chain.
2. **The host program**, a small helper executable that ships with your application. It loads the plugins, runs
   their audio on its own realtime threads, and owns their editor windows.

Your application talks to Summoner through two bounded queues: **commands** in and **events** out. Sending a
command never waits. If the queue is full, the send returns "busy" straight away and you can retry. Events arrive
on a channel you own, so your control thread decides when to look at them.

In code, the library is the crate `summoner`: `Bridge::start` takes a `Config` (the host program's path, the sample
rate and your state hasher) and the sender of your event channel, spawns the host program and does the handshake.
`Bridge::send` takes a `Command` (`Load` a `ChainSpec` under a key, `Unload`, `SetState`, `GetState`, `ParamList`,
`Park`, `Unpark` and more) and returns `Busy` instead of waiting. The `Event`s that come back include `ChildUp`,
`ChainReady` (with the chain's `ChainLink` for your audio thread), `ChainDown` (with a `DownReason`), `StateRead` and
`ParamList`.

```rust
//! Start the supervisor, load one chain and watch its events. The supervisor spawns the host program (the helper
//! process that loads the plugins), does the handshake, and reports what happens as events on a channel you own.
//!
//! ```sh
//! cargo build -p summoner-host
//! cargo run -p summoner --example summoner_quickstart -- target/debug/summoner-host [<bundle.vst3> <class id>]
//! ```
//!
//! With no plugin named, the chain holds one empty slot, which passes its input through.
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use runecraft_contracts::digest::NoStateHash;
use summoner::supervisor::{Bridge, ChainSpec, Command, Config, Event};
use summoner_proto::codec::Role;
use summoner_proto::layout::Geometry;
use summoner_proto::plugin::PluginRef;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(program) = args.next() else {
        eprintln!("usage: summoner_quickstart <host program> [<bundle.vst3> <class id>]");
        return Ok(());
    };
    let plugin = match (args.next(), args.next()) {
        (Some(bundle), Some(class_id)) => Some(PluginRef::vst3(bundle, class_id, "plugin")?),
        _ => None,
    };

    // The state hasher is your application's choice; NoStateHash reports no digest.
    let config = Config::new(program.into(), 48_000.0, Arc::new(NoStateHash));
    let (events_tx, events) = sync_channel(config.event_bound);
    let bridge = Bridge::start(config, events_tx)?;

    // One live stereo chain of one slot, for blocks of up to 512 frames. Sending never waits: a full command queue
    // hands the command back (`Busy`) for you to retry.
    let spec = ChainSpec::new(Geometry::stereo(512), Role::Live, vec![plugin]);
    if let Err(busy) = bridge.send(Command::Load {
        key: 1,
        spec,
        states: vec![],
    }) {
        eprintln!("the command queue is full: {:?}", busy.0);
    }

    // React on your control thread. `ChainReady` carries the link your audio callback exchanges blocks through.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::ChildUp {
                pid, incarnation, ..
            }) => println!("host program up: pid {pid}, incarnation {incarnation}"),
            Ok(Event::ChainReady {
                key, slots, link, ..
            }) => {
                println!(
                    "chain {key} ready: {} slot(s), delay compensation {} frames",
                    slots.len(),
                    link.pdc_latency(512)
                );
                break;
            }
            Ok(Event::ChainDown { key, reason }) => {
                println!("chain {key} down: {reason:?}");
                break;
            }
            Ok(other) => println!("event: {other:?}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    bridge.shutdown();
    Ok(())
}
```

```sh
cargo build -p summoner-host
cargo run -p summoner --example summoner_quickstart -- target/debug/summoner-host
```

With no plugin named, the chain holds one empty slot, which passes its input through.

On the audio thread, a chain's `ChainLink` is driven once per block through
[`summoner-rt`](crates/summoner-rt/README.md): it hands over this block's input, MIDI and transport, and takes back
the output the helper produced, with no allocation, no lock and at most one wake.

If the helper misses a deadline, the block is played as a short ramp to silence (or as the dry input, for a chain
marked as a monitor) and counted.
Your audio thread is never held up.

### Failures are part of the API

Failure behaviour is the feature, so it is spelled out:

| What happens | What Summoner does | What your application sees |
|---|---|---|
| The helper crashes | respawns it with backoff (a few attempts), reloads every chain with its last known state | `ChainDown`, then `ChainReady` again |
| A plugin crashes the helper | blames the slot it was running in and quarantines that plugin as an empty pass-through | the chain comes back with that slot marked quarantined |
| The helper stops answering | the wedge detector kills it and treats it as a crash | `ChainDown` with reason `wedge` |
| The helper speaks a different protocol version | refuses it and doesn't respawn | `ChainDown` with reason `incompatible` |
| The helper is started with bad arguments | treats it as your error, not the plugin's, and doesn't respawn | `ChainDown` with reason `usage` |

Respawn attempts are limited. When they run out, chains stay silent until your application asks for a restart.

### Testing without real plugins

The `summoner-testkit` crate contains a test double of the host program. It speaks the real protocol over trivial
DSP, and its behaviour (crashes, hangs, slow blocks and more) is set by knobs. Your own tests can use it to
exercise every failure path without installing a single third-party plugin.

## The crates

| Crate | What it holds |
|---|---|
| [`summoner`](crates/summoner/README.md) | the supervisor: spawn, handshake, the chain catalogue, respawn, quarantine, the wedge detector, the save burst, an injectable clock |
| [`summoner-proto`](crates/summoner-proto/README.md) | the protocol both processes agree on: the shared-memory layout (with field tables a C header can be generated from), the control-frame codec, plugin references and scan results, the state blob framing |
| [`summoner-rt`](crates/summoner-rt/README.md) | the realtime exchange on the host side: the shared-memory views, the per-block exchange, held notes and note chasing, the offline render worker |
| [`summoner-scan`](crates/summoner-scan/README.md) | the scan supervisor: one probe per process, its own process group, a time budget, a verdict for every plugin |
| [`summoner-plugin`](crates/summoner-plugin/README.md) | the rules a host applies to every slot: the parameter order and name limits, the latency-change handover |
| [`summoner-testkit`](crates/summoner-testkit/README.md) | the test double of the host program |
| [`summoner-host`](crates/summoner-host/README.md) | the host program: the helper executable that loads the plugins, runs their chains on its realtime threads and owns their editors |
| [`summoner-proc`](crates/summoner-proc/README.md) | what the host program runs in a chain slot: the `Plugin` and `Dsp` traits and the block a `Dsp` processes |
| [`summoner-format-vst3`](crates/summoner-format-vst3/README.md) | VST3 slots |
| [`summoner-format-au`](crates/summoner-format-au/README.md) | Audio Unit (v2) slots |
| [`summoner-probe`](crates/summoner-probe/README.md) | the scan's discovery and validation of one bundle |
| [`summoner-probe-bin`](crates/summoner-probe-bin/README.md) | the scan probe program |
| [`summoner-testproc`](crates/summoner-testproc/README.md) | the in-process test double for a chain slot |
| [`summoner-sys`](crates/summoner-sys/README.md) | the host program's system and SDK calls (process-exit watch, crash handler, the VST3 and Audio Unit shims) |
| [`summoner-sys-ui`](crates/summoner-sys-ui/README.md) | the scan probe's AppKit and CoreGraphics calls, apart from everything the host program links |

## Design principles

- **Crash isolation first.** Plugins run out of process. Nothing a plugin does can corrupt your application's memory
  or stop your audio thread.
- **Realtime safety.** The audio-thread side never allocates, locks or makes a blocking call. The only system call it
  makes is the doorbell that wakes the helper. Tests run the realtime path under an allocation guard, and the
  handoffs underneath are Runecraft's, which are model-checked with loom.
- **The helper is untrusted.** Every value read from shared memory or the control channel is checked and clamped.
  The codec's decoder bounds every length before it reads. A helper that lies can only hurt its own chains.
- **No global state.** Everything hangs off the value you start. Two hosts in one process don't share anything.
- **Headless operation.** Scanning, loading, state, parameters and render all work without a window. Editors are
  optional.
- **Safe code above the floor.** Every Summoner crate except `summoner-sys` and `summoner-sys-ui` is
  `#![forbid(unsafe_code)]`; those two hold the host program's system and plugin-SDK calls, one module per surface,
  each `unsafe` block with its safety comment. The supervisor's system calls (shared memory,
  process spawn, descriptor passing) live in [Runecraft](https://github.com/shiehn/runecraft-audio-core), each with a
  written safety contract.
- **Your application decides.** The state hash is injected, the clock is injectable, and logging goes to a sink you
  provide (or nowhere).

## Platform support

| Platform | Plugin formats | Status |
|---|---|---|
| macOS (Apple silicon) | VST3, Audio Unit | where it runs today |
| Windows | VST3 | in progress |
| Linux | VST3 | in progress |
| Any | CLAP | planned |

Plugin hosting runs on macOS today. CI covers more than that: on Linux (x86_64) it builds and tests the crates that
do not load plugins (summoner-proto, summoner-plugin, summoner-rt, summoner, summoner-scan, summoner-testkit,
summoner-sys-ui and summoner-proc). On Windows (x86_64) it compile-checks summoner-proto, summoner-rt,
summoner-plugin and summoner, runs the tests of summoner-rt and summoner, and runs the library tests of summoner-proto
(its quickstart example builds a macOS plugin path, so on Windows it is compiled, not run). Portable paths for
summoner-proto's examples and integration tests, so that they run on Windows too, are planned for 0.5.

The helper's realtime threads can join the host application's audio workgroup on macOS, so the system schedules them
together.

WebAssembly is out of scope: a browser can't load native plugins. A web build could still drive a native Summoner
through [Grimoire](https://github.com/shiehn/grimoire-daw-engine)'s protocol.

An in-process mode, for simple tools that would rather trade isolation for simplicity, is planned behind the same API.

## How it fits

Summoner sits in the "sound and devices" row of Signals & Sorcery. It depends only on the floor:

- [Runecraft](https://github.com/shiehn/runecraft-audio-core) for the log contract, the realtime helpers and the
  process and shared-memory primitives;
- [Orrery](https://github.com/shiehn/orrery-musical-time) for the transport it passes to plugins (tempo, position,
  loop points, bar start).

Above it, [Leyline](https://github.com/shiehn/leyline-audio-graph) wraps a chain link as a graph node and
[Grimoire](https://github.com/shiehn/grimoire-daw-engine) wires plugin chains into a full engine. Neither is
required: an effects rack, a batch processor or a plugin test rig can use Summoner with nothing else from Signals &
Sorcery.

What Summoner deliberately doesn't do:

- **No audio graph and no mixing.** It runs chains; routing them is the graph's job.
- **No audio devices.** Your application (or [Aethergate](https://github.com/shiehn/aethergate-audio-platform))
  owns the device and calls Summoner from its callback.
- **No project model.** It doesn't decide which plugins to load or where state is saved. It hands you the bytes.
- **No plugin authoring.** For writing plugins in Rust, crates such as nih-plug and clack already serve that well.

## Trademarks

VST is a registered trademark of Steinberg Media Technologies GmbH. Audio Unit is a trademark of Apple Inc. The VST3
SDK and Apple's AudioUnitSDK are fetched at build time at pinned revisions (`tools/build-plugins.sh --fetch`), never
copied into this repository; see [VENDORED.md](VENDORED.md).

## The Signals & Sorcery components

Signals & Sorcery is an ecosystem of twelve Rust audio components ([signalsandsorcery.com](https://signalsandsorcery.com)). Each one is useful on its own, and together they
make a complete engine for audio applications and DAWs. Components meet at small published interfaces (traits, plain
data types, a wire protocol and documented file formats), never at each other's internals, so any one of them can be
replaced by another crate or by your own code.

| Component | Repository | Role | Status |
|---|---|---|---|
| [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) | [runecraft-audio-core](https://github.com/shiehn/runecraft-audio-core) | Shared contracts, realtime safety, process isolation and realtime-safe logging: the floor every other component stands on | Available (0.4, macOS) |
| [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) | [orrery-musical-time](https://github.com/shiehn/orrery-musical-time) | Tempo and meter, transports, bar, beat and sample maths, timed MIDI | Available (0.4, macOS) |
| [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) | [summoner-plugin-host](https://github.com/shiehn/summoner-plugin-host) | Out-of-process VST3 and Audio Unit plugin hosting with crash isolation | Available (0.4, macOS) |
| [Aethergate](https://signalsandsorcery.com/components/aethergate-audio-platform/) | [aethergate-audio-platform](https://github.com/shiehn/aethergate-audio-platform) | Audio devices (cpal, CoreAudio, a fake device for tests) and shared-memory meters | Planned |
| [Leyline](https://signalsandsorcery.com/components/leyline-audio-graph/) | [leyline-audio-graph](https://github.com/shiehn/leyline-audio-graph) | A realtime audio graph with delay compensation, and a compiler from an edit model to a graph | Planned |
| [Cantrips](https://signalsandsorcery.com/components/cantrips-audio-dsp/) | [cantrips-audio-dsp](https://github.com/shiehn/cantrips-audio-dsp) | Built-in mixer DSP: sampler, fader, pan, meters, compressor | Planned |
| [Scriptorium](https://signalsandsorcery.com/components/scriptorium-audio-media/) | [scriptorium-audio-media](https://github.com/shiehn/scriptorium-audio-media) | Decode and encode, a sample store, streaming voices, crash-safe recording | Planned |
| [Tapestry](https://signalsandsorcery.com/components/tapestry-session-model/) | [tapestry-session-model](https://github.com/shiehn/tapestry-session-model) | Tracks, clips, lanes and scenes; edits with undo; a crash-safe project store | Planned |
| [Procession](https://signalsandsorcery.com/components/procession-timeline-engine/) | [procession-timeline-engine](https://github.com/shiehn/procession-timeline-engine) | What plays in each block: regions, notes, automation, launches | Planned |
| [Grimoire](https://signalsandsorcery.com/components/grimoire-daw-engine/) | [grimoire-daw-engine](https://github.com/shiehn/grimoire-daw-engine) | The assembled engine and its headless JSON-RPC protocol | Planned |
| [Familiar](https://signalsandsorcery.com/components/familiar-plugin-sdk/) | [familiar-plugin-sdk](https://github.com/shiehn/familiar-plugin-sdk) | An SDK for plugins and panels that extend a Signals & Sorcery engine | Planned |
| [Glyphwork](https://signalsandsorcery.com/components/glyphwork-audio-ui/) | [glyphwork-audio-ui](https://github.com/shiehn/glyphwork-audio-ui) | Audio UI widgets: meters, knobs, faders, waveforms | Planned |

Dependencies point down, never up. A component may depend on components in the rows below it, never on one above
it, and most need far less than everything below them.

```text
Extend and display   Familiar, Glyphwork
The engine           Grimoire
Musical structure    Tapestry, Procession
Signal flow          Leyline
Sound and devices    Cantrips, Scriptorium, Aethergate, Summoner
The floor            Runecraft, Orrery
```

## Related repositories

The three code repositories of Signals & Sorcery today, each with its own crates:

- [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) (`runecraft-audio-core`): shared
  contracts, realtime safety, process isolation and realtime-safe logging.
- [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) (`orrery-musical-time`): musical time.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) (`summoner-plugin-host`): out-of-process
  VST3 and Audio Unit plugin hosting.

Every component, with its repository, is in the table above and on the
[components index](https://signalsandsorcery.com/components/).

## Licence

MIT. See [LICENSE](LICENSE).
