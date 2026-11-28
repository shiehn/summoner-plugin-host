//! The test double's configuration from its protocol-4.0 ref (`{"format":"fake","fake":{…}}`), and its state parse
//! (the body knobs, as the host application's test double reads them).
#![forbid(unsafe_code)]

use summoner_proc::Plugin;
use summoner_proto::plugin::PluginRef;
use summoner_testproc::{Config, TestPlugin};

fn fake(knobs: serde_json::Value) -> PluginRef {
    serde_json::from_value(serde_json::json!({"format": "fake", "fake": knobs})).unwrap()
}

#[test]
fn a_ref_s_knobs_configure_the_double() {
    let c = Config::from_ref(&fake(serde_json::json!({})), 3).unwrap();
    assert_eq!(
        (c.name.as_str(), c.gain, c.delay),
        ("fake3", 1.0, 0),
        "defaults; the name is fake<slot>"
    );
    let c = Config::from_ref(
        &fake(serde_json::json!({"name": "a", "gain": "0.5", "delay": "3", "synth": "1", "echo": 8, "feedback": 0.25})),
        0,
    )
    .unwrap();
    assert_eq!(
        (c.name.as_str(), c.gain, c.delay, c.synth),
        ("a", 0.5, 3, true)
    );
    assert_eq!(
        (c.echo, c.feedback),
        (8, 0.25),
        "a number knob reads as its JSON text"
    );
    assert_eq!(c.latency, 3, "latency defaults to the delay");
    // Strict number parsing, as before: a hex float, a refused value keeps the default, a NaN is NaN.
    let g = |v: &str| {
        Config::from_ref(&fake(serde_json::json!({"gain": v})), 0)
            .unwrap()
            .gain
    };
    assert_eq!(g("0x1p-1"), 0.5);
    assert_eq!(g("0.5 "), 1.0, "trailing white space is refused");
    assert!(g("nan").is_nan());
    let d = |v: &str| {
        Config::from_ref(&fake(serde_json::json!({"delay": v})), 0)
            .unwrap()
            .delay
    };
    assert_eq!(d("+3"), 0, "digits only");
    assert_eq!(d("99999999999"), 1 << 20, "capped at the longest line");
    // A flag is its presence, whatever its value.
    assert!(
        Config::from_ref(&fake(serde_json::json!({"sustain": false})), 0)
            .unwrap()
            .sustain
    );
    // Not the test double: a vst3 or au ref.
    let v: PluginRef = serde_json::from_str(
        r#"{"format":"vst3","path":"/x/a.vst3","classId":"5341534c4c4142010000005000000000","name":"a"}"#,
    )
    .unwrap();
    assert!(Config::from_ref(&v, 0).is_none());
}

/// (state, the gain after it, from 1.0).
const STATES: &[(&[u8], f32, &str)] = &[
    (b"gain=0.25", 0.25, "a gain"),
    (b"gain= 0.5", 0.5, "strtof skips leading white space"),
    (b"gain=0x1p-1", 0.5, "a hex float"),
    (b"gain=0.5;gain=0.75", 0.75, "the last part wins"),
    (b"x;gain=0.5", 0.5, "any part"),
    (b" gain=0.5", 1.0, "a part must start with the key"),
    (b"gain=0.5 ", 1.0, "trailing white space is refused"),
    (b"gain=0.5\0junk", 0.5, "c_str stops at the NUL"),
    (b"gain=\xff", 1.0, "a byte strtof cannot read"),
    (
        b"SLOWGET=+5;gain=0.5",
        0.5,
        "a refused SLOWGET does not stop the rest",
    ),
];

#[test]
fn states_parse_by_their_knobs() {
    let mut bad = Vec::new();
    for (blob, want, why) in STATES {
        let p = TestPlugin::new(Config::from_ref(&fake(serde_json::json!({})), 0).unwrap());
        p.set_state(blob).unwrap();
        let mut pl = p.prepare(48_000.0, 16);
        // The gain, read through a block: one frame of 1.0 in, gain × 1.0 out.
        let mut ch = vec![vec![1.0f32; 1], vec![0.0; 1]];
        let t = summoner_proc::Transport::default();
        summoner_proc::with_channels(&mut ch, |channels| {
            pl.process(&mut summoner_proc::Block {
                frames: 1,
                channels,
                midi: &[],
                transport: &t,
            })
        });
        if ch[0][0].to_bits() != want.to_bits() {
            bad.push(format!(
                "{:?} ({why}): gain {}, want {want}",
                String::from_utf8_lossy(blob),
                ch[0][0]
            ));
        }
        assert_eq!(p.get_state(), *blob, "the state comes back byte for byte");
    }
    assert!(
        bad.is_empty(),
        "{} rows differ:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
