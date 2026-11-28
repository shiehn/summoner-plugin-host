![Summoner](../../docs/images/plugin-host.png)

# summoner

**The supervisor: the library your application links to run plugins in Summoner's host program, and to survive
whatever they do.**

`summoner` is the crate that spawns, supervises and talks to the host program in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Your application never touches a plugin. It starts a `Bridge`, sends it `Command`s, and reads `Event`s from a
channel it owns. Behind the `Bridge`, the supervisor's threads:

- **spawn** the host program with its control channel on descriptor 3, do the `hello` handshake within a deadline,
  and check that it speaks the same protocol version;
- keep **the chain catalogue**: every chain you asked for (`Command::Load` with a `ChainSpec`: a region geometry, a
  role (live or render) and an ordered list of plugin slots), its generation, and the last known state of every slot;
- hand your audio thread a **link** per chain (`Event::ChainReady` carries an `Arc<ChainLink>`; the per-block
  exchange is [`summoner-rt`](../summoner-rt/README.md)'s);
- read and write **plugin state**: `Command::GetState` and `Command::SetState` per slot, and `Command::SaveBurst`,
  which reads back only the slots whose state moved since the last save, inside a time budget and a byte budget;
- list and set **parameters** (`Command::ParamList` in pages, `Command::ParamSet` by the plugin's own id), open and
  close **editor windows** (the host program owns them), park and unpark chains, and attach the host program's
  realtime threads to your audio workgroup;
- **detect failures and recover**: crashes, wedged chains, stalled sends and protocol faults (below);
- adapt each chain's **pipeline depth** to how late the host program's answers arrive (`depth`).

## Failures are part of the API

| What happens | What the supervisor does | What your application sees |
|---|---|---|
| The host program crashes | respawns it after a backoff (2, 4, then 8 s), reloads every chain with its last known state | `ChainDown` with `DownReason::Crash`, `HostRestarted`, then `ChainReady` again |
| A plugin kills the host program twice in a row | blames that slot and quarantines it: the slot keeps its place and passes audio through | `Quarantined`, then the chain back with that slot marked quarantined |
| A chain stops making progress and does not answer a ping | kills the host program (the wedge detector) and treats it as a crash | `ChainDown` with `DownReason::Wedge` |
| A send to the host program stalls | treats it as wedged | `ChainDown` with `DownReason::Stall` |
| The host program breaks the protocol | kills it and respawns | `ChainDown` with `DownReason::Protocol` |
| The host program speaks another protocol version | refuses it and does not respawn | `ChainDown` with `DownReason::Incompatible` |
| The host program was started with bad arguments | treats it as your error, not a plugin's, and does not respawn | `ChainDown` with `DownReason::Usage` |

After three failed attempts the supervisor stops (`Event::Exhausted`): chains stay silent until your application
sends `Command::RestartHost`. A host program that stays up long enough (`Config::forgive_after`, 60 s) resets the
count.

## How it fits

`summoner` is the crate your application depends on. It uses
[`summoner-proto`](../summoner-proto/README.md) for every frame, [`summoner-rt`](../summoner-rt/README.md) for the
regions and the links, and [`summoner-plugin`](../summoner-plugin/README.md) for the parameter limits and the
handover budget. Below it are Runecraft (the log contract, the state-hash trait, the realtime floor, and the
process, channel and shared-memory calls) and nothing else. Plugin scanning is a separate supervisor,
[`summoner-scan`](../summoner-scan/README.md).

The other end is the host program, [`summoner-host`](../summoner-host/README.md), or in tests the test double,
[`summoner-testkit`](../summoner-testkit/README.md).

What it does not do: it has no audio graph and does no mixing (it runs chains; routing them is your application's
job), it owns no audio device (your callback calls the link), and it has no project model (it does not decide which
plugins to load or where their state is saved: it hands you the bytes and their digest).

## Quick start

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

## The API

| Item | What it is |
|---|---|
| `supervisor::Bridge` | `Bridge::start(config, events)`, `send`, `send_corr`, `shutdown` |
| `supervisor::Config` | `Config::new(program, sample_rate, state_hasher)`, then its public fields: timeouts, backoff, log sink, clock |
| `supervisor::Command` | what you ask for: `Load`, `Unload`, `GetState`, `SetState`, `SaveBurst`, `ParamList`, `ParamSet`, `EditorOpen`, `RestartHost`, ... |
| `supervisor::Event` | what happens: `ChildUp`, `ChainReady`, `ChainDown`, `StateRead`, `Burst`, `Quarantined`, `Exhausted`, ... |
| `supervisor::ChainSpec` | a chain to load: geometry, role, slots |
| `supervisor::DownReason` | why a chain went down |
| `supervisor::Busy` | a command handed back because the queue was full |
| `clock::Clock`, `clock::ManualClock` | the injectable clock (tests drive time by hand) |
| `depth::DepthController` | the adaptive pipeline depth |

## Realtime and safety

- **Sending never waits.** Commands go through a bounded queue; a full one hands the command back as `Busy`. Events
  go to a bounded channel you own, so your control thread decides when to look at them.
- **The host program is untrusted.** Every frame it sends is checked by the codec, every count and length is
  clamped, and the bytes the reader thread accepts are budgeted, so a host program that lies can only hurt its own
  chains.
- **Nothing global.** Everything hangs off the `Bridge` you start; two bridges in one process share nothing.
- **Your application decides**: the state hash is injected (`runecraft_contracts::digest::StateHasher`; there is no
  default, so you cannot forget it silently), the clock is injectable, and records go to the `LogSink` you give it
  (or nowhere).
- `#![forbid(unsafe_code)]`: the system calls (spawn, descriptor passing, shared memory) are Runecraft's, each with a
  written safety contract.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

The spawn, the mach-port route the host program registers through and the audio-workgroup join are macOS-specific
today.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner-rt`](../summoner-rt/README.md): the per-block exchange your audio callback runs
- [`summoner-scan`](../summoner-scan/README.md): plugin scanning in processes of their own
- [`summoner-host`](../summoner-host/README.md): the host program
- [`summoner-testkit`](../summoner-testkit/README.md): the test double, for tests without real plugins
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
