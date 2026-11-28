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
