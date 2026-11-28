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
