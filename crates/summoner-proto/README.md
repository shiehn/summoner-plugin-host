![Summoner](../../docs/images/plugin-host.png)

# summoner-proto

**The protocol between your application and Summoner's host program: the shared-memory layout, the control frames,
plugin references and the state blob framing.**

`summoner-proto` is the crate that defines every byte the two processes exchange in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio building blocks for building high-performance audio applications and DAWs ([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Summoner runs plugins in a separate host program. The two processes talk through two channels, and this crate is the
one definition both sides compile against:

- **The shared-memory region** ([`layout`](src/layout.rs)): one region per chain, holding two block slots used by `seq` parity (one
  being written while the other is read), the doorbell line, the acknowledgement line and a latency histogram. Every
  offset is a constant checked at compile time, and the field tables (`layout::SECTIONS`, `HEADER_FIELDS` and the
  others) are data a C header generator reads, so code in another language can lay out the same bytes.
  [`Geometry`](src/layout.rs) sizes a region for a channel count and a maximum block size, and `Geometry::validate`
  refuses one that does not fit.
- **The control channel** ([`codec`](src/codec.rs)): length-prefixed frames with a 24-byte header (`kind`, `flags`, request `id`,
  meta and blob lengths), JSON meta and an optional binary blob. `encode`, `encode_msg` and the streaming `Decoder`
  are here, with one typed message per frame kind (`Hello`, `ChainLoad`, `StateGet`, `ParamList`, `Scanned` and the
  rest) and the frame-kind table (`codec::kind`, `codec::KINDS`).
- **Plugin references and scan results** ([`plugin`](src/plugin.rs)): `PluginRef` names a VST3 class (bundle path and 32-digit class
  id) or an Audio Unit (bundle path and its type, subtype and manufacturer codes); `ScanClass` is one class a scan
  found. Both are checked when they are built and when they are decoded, with the same checks.
- **The state blob** ([`state_blob`](src/state_blob.rs)): the one canonical framing of a plugin's saved state (VST3 component and
  controller chunks, an Audio Unit's binary property list, or a built-in node's body), with a four-byte magic and a
  version. `encode` and `decode` are exact inverses, and `check` validates a blob without keeping its chunks.

## How it fits

`summoner-proto` is the bottom of Summoner. Every other Summoner crate depends on it: the supervisor
([`summoner`](../summoner/README.md)) writes requests and reads replies, the realtime exchange
([`summoner-rt`](../summoner-rt/README.md)) maps the region, and the host program
([`summoner-host`](../summoner-host/README.md)) reads requests and writes replies. Below it are only Runecraft's
contracts (the log levels and correlation ids the codec checks) and Orrery's transport bits.

It does no I/O: no sockets, no files, no threads. Reading and writing the channel is the supervisor's and the host
program's job; this crate turns bytes into checked values and back.

## Quick start

```rust
//! The protocol's three layers in one run: a control frame encoded and decoded back through the streaming decoder,
//! a plugin reference checked at construction, and a VST3 state blob framed and read back. Run it with
//! `cargo run -p summoner-proto --example summoner_proto_quickstart`.
use summoner_proto::codec::{Decoder, Direction, Hello, encode_msg, kind, kind_name};
use summoner_proto::plugin::PluginRef;
use summoner_proto::state_blob::{self, Blob};

fn main() {
    // A `hello` request, as the supervisor writes it on the control channel: a 24-byte header, JSON meta, no blob.
    let bytes = encode_msg(kind::HELLO, 0, 1, &Hello::new("host-1".into(), 1), &[]);

    // The helper's side reads requests. The decoder is streaming: feed it bytes in any split.
    let mut decoder = Decoder::new(Direction::ToChild);
    let mut frame = None;
    let (a, b) = bytes.split_at(10);
    decoder
        .feed(a, &mut |f| frame = Some(f))
        .expect("a valid prefix");
    assert!(frame.is_none(), "half a frame is not a frame yet");
    decoder
        .feed(b, &mut |f| frame = Some(f))
        .expect("the rest of the frame");
    assert!(decoder.at_boundary());
    let frame = frame.expect("one whole frame");
    let hello: Hello = frame.meta_as().expect("hello meta");
    println!(
        "{} #{}: {hello:?}",
        kind_name(frame.header.kind).unwrap_or("?"),
        frame.header.id
    );

    // A plugin reference is checked when it is made: the class id must be 32 lowercase hex digits.
    let synth = PluginRef::vst3(
        "/Library/Audio/Plug-Ins/VST3/Synth.vst3",
        "0123456789abcdef0123456789abcdef",
        "Synth",
    )
    .expect("a well-formed VST3 reference");
    assert!(
        PluginRef::vst3(
            "/Library/Audio/Plug-Ins/VST3/Synth.vst3",
            "not-hex",
            "Synth"
        )
        .is_err()
    );
    println!("{} at {:?}", synth.name(), synth.bundle_path());

    // A plugin's saved state, in the one framing both processes accept.
    let saved = Blob::Vst3 {
        component: Some(b"component state"),
        controller: None,
    };
    let framed = state_blob::encode(&saved).expect("a blob within the limits");
    let back = state_blob::decode(&framed).expect("the framing reads back");
    assert_eq!(back, saved);
    println!(
        "state blob: {} bytes, format {}",
        framed.len(),
        back.format().name()
    );
}

#[test]
fn the_example_runs() {
    main();
}
```

```sh
cargo run -p summoner-proto --example summoner_proto_quickstart
```

## The API

| Item | What it is |
|---|---|
| `layout::Geometry` | a region's shape: `Geometry::stereo(max_frames)`, the slot and section offsets, `mapped_bytes` |
| `layout::ABI_VERSION`, the `*_FIELDS` tables | the region's version and its byte-exact field tables |
| `codec::encode`, `codec::encode_msg` | one frame: header, meta, blob |
| `codec::Decoder` | the streaming decoder: feed any split of the stream, get whole `Frame`s |
| `codec::Frame::meta_as` | a frame's meta as its typed message |
| `codec::kind`, `codec::kind_name` | the frame kinds |
| `codec::ProtoError` | why a frame was refused |
| `plugin::PluginRef` | `PluginRef::vst3`, `PluginRef::au`: a plugin to load, checked |
| `plugin::ScanClass` | a class a scan found |
| `state_blob::{encode, decode, check}` | the state blob framing and its check |
| `state_blob::Refusal` | why a state blob was refused |

## Realtime and safety

- `#![forbid(unsafe_code)]`: safe code only.
- **Every input is untrusted.** The decoder bounds the meta and the blob (`MAX_META`, `MAX_BLOB`) before it reads
  them, refuses unknown fields in plugin references, checks class ids and Audio Unit codes, and caps the counts in a
  scan result. A peer that sends garbage gets a `ProtoError`, never a panic or an unbounded allocation.
- Nothing here runs on the audio thread except the layout's constants and geometry arithmetic.
- The `test-double` feature adds a constructor for the test double's plugin references (`PluginRef::fake`). A build
  with it carries a marker string, so a shipped binary can be checked for it.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress.

The layout and the codec are plain Rust and portable; the processes that use them are macOS-only today.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): how the crates fit together.
- [`summoner`](../summoner/README.md): the supervisor that speaks this protocol
- [`summoner-rt`](../summoner-rt/README.md): the realtime exchange over the region
- [`summoner-host`](../summoner-host/README.md): the host program, the other end
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/) · [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) · [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/) · [all components](https://signalsandsorcery.com/components/)
