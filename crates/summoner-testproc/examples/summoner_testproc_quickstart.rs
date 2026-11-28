//! The test double as a slot: a plugin reference with knobs (`gain`, `delay`, `synth`, ...) becomes a `Config`, then
//! a `summoner_proc::Plugin` you can prepare and run, with no plugin bundle on disk. Here: a gain of 0.5 and a delay of
//! 3 frames (its reported latency) over one stereo block. Run it with
//! `cargo run -p summoner-testproc --example summoner_testproc_quickstart`.
use summoner_proc::{Block, Transport, with_channels};
use summoner_proto::plugin::PluginRef;
use summoner_testproc::{Config, make};

fn main() {
    let r: PluginRef = serde_json::from_str(
        r#"{"format":"fake","fake":{"name":"double","gain":"0.5","delay":"3"}}"#,
    )
    .expect("a test-double reference");
    let cfg = Config::from_ref(&r, 0).expect("the test double's knobs");
    assert_eq!((cfg.gain, cfg.delay, cfg.latency), (0.5, 3, 3));
    let plugin = make(cfg);
    assert_eq!((plugin.name(), plugin.latency()), ("double", 3));

    let mut dsp = plugin.prepare(48_000.0, 64);
    let mut channels = vec![vec![1.0f32; 16]; 2];
    let transport = Transport::default();
    let ok = with_channels(&mut channels, |channels| {
        dsp.process(&mut Block {
            frames: 16,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    assert!(ok);
    // The first 3 frames are the delay line's silence, then the input at half gain.
    assert_eq!(&channels[0][..4], &[0.0, 0.0, 0.0, 0.5]);
    println!(
        "{}: latency {} frames, block {:?}",
        plugin.name(),
        plugin.latency(),
        &channels[0][..6]
    );
}

#[test]
fn the_example_runs() {
    main();
}
