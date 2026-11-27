![Summoner](docs/images/plugin-host.png)

# Summoner

**An out-of-process audio plugin host.**

Summoner is the component that loads and runs VST3 and Audio Unit plugins in a separate helper process, so a
crashing or hanging plugin can't take your application down, and is part of **Signals & Sorcery**, an ecosystem of
Rust audio components for building high-performance audio applications and DAWs.

**Status: In preparation: the code is being made ready for its first public release.** The crates described below
exist and are tested, but they live in another repository today. This repository holds the README until the first
public release. The public API is still being reviewed, so names and signatures may change.

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

The sketch below shows the intended shape of the public API.

```rust
// Illustrative sketch: the planned shape of the public API, not final.
use std::sync::{mpsc, Arc};
use summoner::{ChainSpec, Command, Config, Event, Host, PluginRef};

// Start the supervisor. It spawns the helper process and does the handshake.
let (events_tx, events) = mpsc::sync_channel(4096);
let config = Config::new("path/to/summoner-host".into(), 48_000.0, Arc::new(MyStateHasher));
let host = Host::start(config, events_tx)?;

// Ask for a chain: one instrument followed by one effect.
let spec = ChainSpec::stereo()
    .slot(PluginRef::vst3("/Library/Audio/Plug-Ins/VST3/MySynth.vst3", synth_class_id))
    .slot(PluginRef::audio_unit(effect_type, effect_subtype, effect_manufacturer));
host.send(Command::Load { key: 1, spec, states: vec![] })?;

// React to what happens, on your control thread.
for event in events {
    match event {
        Event::ChainReady { key, link, slots, .. } => audio.install(key, link, slots),
        Event::ChainDown { key, reason } => audio.silence(key, reason),
        Event::StateRead { key, slot, blob, .. } => project.store_state(key, slot, blob),
        _ => {}
    }
}
```

On the audio thread, a chain's link is called once per block. It hands over this block's input, MIDI and transport,
and gives back the output the helper produced.

```rust
// Illustrative sketch: the audio-thread side.
fn process(&mut self, io: &mut AudioIo, midi: &[TimedMidi], transport: &Transport) {
    // No allocation, no lock, no blocking: one shared-memory exchange and at most one wake.
    self.link.exchange(io, midi, transport);
}
```

If the helper misses a deadline, the block is filled with silence (or the dry input, for an effect) and counted.
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
| `summoner` | the supervisor: spawn, handshake, the chain catalogue, respawn, quarantine, the wedge detector, the save burst, an injectable clock |
| `summoner-proto` | the protocol both processes agree on: the shared-memory layout (with a generated C header), the control-frame codec, plugin references and scan results, the state blob framing |
| `summoner-rt` | the realtime exchange on the host side: the shared-memory views, the per-block exchange, held notes and note chasing, the offline render worker |
| `summoner-scan` | the scan supervisor: one probe per process, its own process group, a time budget, a verdict for every plugin |
| `summoner-plugin` | the rules a host applies to every slot: the channel rule, the parameter order and name limits, the latency-change handover |
| `summoner-testkit` | the test double of the host program |

The host program itself, which links the VST3 and Audio Unit loaders, will ship in this repository too.

## Design principles

- **Crash isolation first.** Plugins run out of process. Nothing a plugin does can corrupt your application's memory
  or stop your audio thread.
- **Realtime safety.** The audio-thread side never allocates, locks or makes a blocking call. The only system call it
  makes is the doorbell that wakes the helper. Tests run the realtime path under an allocation guard, and the
  shared-memory handoff is model-checked with loom.
- **The helper is untrusted.** Every value read from shared memory or the control channel is checked and clamped.
  The codec has fuzz targets. A helper that lies can only hurt its own chains.
- **No global state.** Everything hangs off the value you start. Two hosts in one process don't share anything.
- **Headless operation.** Scanning, loading, state, parameters and render all work without a window. Editors are
  optional.
- **Safe code above the floor.** Every Summoner crate is `#![forbid(unsafe_code)]`. The system calls (shared memory,
  process spawn, descriptor passing) live in [Runecraft](https://github.com/shiehn/runecraft-audio-core), each with a
  written safety contract.
- **Your application decides.** The state hash is injected, the clock is injectable, and logging goes to a sink you
  provide (or nowhere).

## Platform support

| Platform | Plugin formats | Status |
|---|---|---|
| macOS (Apple silicon) | VST3, Audio Unit | where it runs today |
| Windows | VST3 | planned |
| Linux | VST3 | planned |
| Any | CLAP | planned |

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
SDK will be fetched at build time, never copied into this repository.

## The Signals & Sorcery components

Signals & Sorcery is an ecosystem of twelve Rust audio components. Each one is useful on its own, and together they
make a complete engine for audio applications and DAWs. Components meet at small published interfaces (traits, plain
data types, a wire protocol and documented file formats), never at each other's internals, so any one of them can be
replaced by another crate or by your own code.

| Component | Repository | Role | Status |
|---|---|---|---|
| Runecraft | [runecraft-audio-core](https://github.com/shiehn/runecraft-audio-core) | Shared contracts, realtime safety, process isolation and realtime-safe logging: the floor every other component stands on | In preparation |
| Orrery | [orrery-musical-time](https://github.com/shiehn/orrery-musical-time) | Tempo and meter, transports, bar, beat and sample maths, timed MIDI | In preparation |
| Summoner | [summoner-plugin-host](https://github.com/shiehn/summoner-plugin-host) | Out-of-process VST3 and Audio Unit plugin hosting with crash isolation | In preparation |
| Aethergate | [aethergate-audio-platform](https://github.com/shiehn/aethergate-audio-platform) | Audio devices (cpal, CoreAudio, a fake device for tests) and shared-memory meters | Planned |
| Leyline | [leyline-audio-graph](https://github.com/shiehn/leyline-audio-graph) | A realtime audio graph with delay compensation, and a compiler from an edit model to a graph | Planned |
| Cantrips | [cantrips-audio-dsp](https://github.com/shiehn/cantrips-audio-dsp) | Built-in mixer DSP: sampler, fader, pan, meters, compressor | Planned |
| Scriptorium | [scriptorium-audio-media](https://github.com/shiehn/scriptorium-audio-media) | Decode and encode, a sample store, streaming voices, crash-safe recording | Planned |
| Tapestry | [tapestry-session-model](https://github.com/shiehn/tapestry-session-model) | Tracks, clips, lanes and scenes; edits with undo; a crash-safe project store | Planned |
| Procession | [procession-timeline-engine](https://github.com/shiehn/procession-timeline-engine) | What plays in each block: regions, notes, automation, launches | Planned |
| Grimoire | [grimoire-daw-engine](https://github.com/shiehn/grimoire-daw-engine) | The assembled engine and its headless JSON-RPC protocol | Planned |
| Familiar | [familiar-plugin-sdk](https://github.com/shiehn/familiar-plugin-sdk) | An SDK for plugins and panels that extend a Signals & Sorcery engine | Planned |
| Glyphwork | [glyphwork-audio-ui](https://github.com/shiehn/glyphwork-audio-ui) | Audio UI widgets: meters, knobs, faders, waveforms | Planned |

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

## Licence

MIT. See [LICENSE](LICENSE).
