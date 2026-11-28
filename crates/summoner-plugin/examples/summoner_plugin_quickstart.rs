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
