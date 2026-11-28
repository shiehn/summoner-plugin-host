![Summoner](../../docs/images/plugin-host.png)

# summoner-rt

**The realtime side of Summoner: what your audio callback calls to hand a block to the host program and take its
output back, without blocking, locking or allocating.**

`summoner-rt` is the crate that runs the per-block exchange between your audio thread and the host program in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

When a chain is ready, the supervisor gives your application a `ChainLink`: the chain's shared-memory region, the
host program's page of wake words, and the chain's fixed facts (its pipeline depth, its delay compensation, its
parameter ids). Your audio callback keeps a `ChainState` per chain and calls `ChainState::visit` once per block:

1. **Collect**: if the host program has finished an earlier block, its output is read from the region into your
   buffers.
2. **Publish**: this block's input, MIDI, parameter values and transport are written into the free slot, and the
   host program is woken with one doorbell call.
3. **Miss**: if the output is not there in time, the block is played as a short ramp to silence (or as the dry
   input, for a chain marked as a monitor) and counted. Repeated misses open a breaker, so a stalled host program costs your callback almost
   nothing until it recovers.

Around that exchange:

- **Held notes** (`HeldNotes`): the notes and pedals an instrument chain still sounds, so a stop, a loop wrap or a
  chain going down always sends the matching releases and no note is left hanging.
- **Note chasing** (`ChainState::visit_chased`): when playback starts or loops into the middle of a note, the
  instrument is sent that note again so it sounds, using Orrery's chase offsets.
- **The release carry** (`midi::MidiCarry`): while a chain's breaker is open, its releases are kept (in fixed arrays)
  for the first block after it closes; everything else is dropped and counted.
- **Offline render** ([`render`](src/render.rs)): a render set runs block by block with a synchronous exchange (one
  block per `seq`, an acknowledgement wake, a per-block time budget and a cancel flag), so a bounce uses the same
  plugins and the same code path as live playback. `admit_render_set` checks there is memory for a render set before
  one is started.
- **Counters** (`AudioCounters`): every miss, drop and breaker trip, readable from any thread.

## How it fits

`summoner-rt` sits between the supervisor ([`summoner`](../summoner/README.md)), which creates the regions and hands
out the links, and your audio callback, which calls it. It reads and writes the layout
[`summoner-proto`](../summoner-proto/README.md) defines. Below it are Runecraft's realtime floor (the handoff rings,
the realtime lock, prefaulting, the stderr writer) and its cross-process floor (the shared mapping, the doorbell and
the waits), and Orrery's transport arithmetic.

It does not spawn, supervise or decode anything: a link comes from the supervisor, and a chain that goes down is the
supervisor's event to report. It knows nothing about audio devices: your application owns the device and calls this
crate from its callback.

## Quick start

The exchange itself needs a running host program (see the [`summoner`](../summoner/README.md) quick start); this
example shows the parts that run on their own: a region, the held-note book and the release carry.

```rust
//! The host side of the realtime exchange, without a helper process: a shared region laid out for one stereo chain,
//! the held-note book a chain keeps so that a stop never leaves a note hanging, and the release carry a muted chain
//! keeps for its next block. Run it with `cargo run -p summoner-rt --example summoner_rt_quickstart`.
use summoner_proto::layout::Geometry;
use summoner_rt::HeldNotes;
use summoner_rt::midi::{MidiCarry, Offered};
use summoner_rt::region::Region;

fn main() -> std::io::Result<()> {
    // One chain's region: an unlinked shared mapping, laid out for stereo blocks of up to 512 frames. The returned
    // handle is what the supervisor sends to the helper process with the chain's load.
    let geometry = Geometry::stereo(512);
    let (region, _handle_for_the_helper) = Region::create(geometry, 7, 48_000.0, 0)?;
    println!(
        "region of chain {}: {} bytes mapped, {} per slot, sample rate {}",
        region.chain_key(),
        region.geometry().mapped_bytes(),
        region.geometry().slot_bytes(),
        region.sample_rate()
    );

    // The notes an instrument still sounds. A note-on is admitted once, then recorded as sent.
    let mut held = HeldNotes::default();
    let note_on = [0x90, 60, 100];
    assert!(held.admit(note_on));
    held.sent(note_on);
    assert!(held.is_held(0, 60));
    // On a stop, every held note gets its release (and the pedals are lifted), with no allocation.
    let mut releases = 0;
    held.release(true, |e| {
        releases += 1;
        println!("release on stop: {e:02x?}");
    });
    assert!(held.is_empty());
    assert!(releases >= 1, "at least the note-off");

    // While a chain is muted, its releases are carried to the next block it plays; anything else is dropped.
    let mut carry = MidiCarry::default();
    assert_eq!(carry.offer([0x80, 60, 0]), Offered::Carried);
    assert_eq!(carry.offer([0x90, 62, 90]), Offered::Dropped);
    let mut carried = 0;
    carry.drain(|_| carried += 1);
    assert_eq!(carried, 1);
    Ok(())
}

#[test]
fn the_example_runs() {
    main().expect("the example runs");
}
```

```sh
cargo run -p summoner-rt --example summoner_rt_quickstart
```

## The API

| Item | What it is |
|---|---|
| `ChainLink` | one chain's region, page and fixed facts, from the supervisor's `ChainReady` event |
| `ChainState` | your callback's per-chain state; `visit` (and `visit_chased`) once per block |
| `Visit` | this block's position, length, transport and loop |
| `HeldNotes` | the notes and pedals a chain still sounds, and their releases |
| `AudioCounters` | misses, drops and breaker trips |
| `region::Region`, `region::ChildPage` | typed atomic views of the shared region and of the host program's wake page |
| `midi::MidiCarry` | the releases kept while a chain's breaker is open |
| `render::RenderWorker`, `render::RenderJob` | the offline render exchange |
| `render::admit_render_set` | the memory check before a render set starts |

## Realtime and safety

- **The callback path never allocates, locks or blocks.** The only system call it makes is the doorbell that wakes
  the host program. The crate's `clippy.toml` refuses locks, blocking calls, channels and growing collections in the
  code the callback runs; the allocation guard in Runecraft catches an allocation at run time in tests.
- **The host program is untrusted.** Everything read from the region is read through bounds-checked atomics and
  clamped: a misbehaving host program can only hurt its own chain's audio, never your process's memory.
- `#![forbid(unsafe_code)]`: the unsafe code (the mapping, the doorbell) lives in Runecraft, behind a written safety
  contract.
- Built for model checking: under `--cfg loom` the atomics and the mapping come from loom, so the region handoff can
  be checked under every interleaving.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

The doorbell and the waits use macOS's address-wait calls today; the Linux and Windows equivalents are part of the
in-progress ports.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner`](../summoner/README.md): the supervisor that hands out chain links
- [`summoner-proto`](../summoner-proto/README.md): the region layout
- [`summoner-plugin`](../summoner-plugin/README.md): the slot rules the host program applies
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
