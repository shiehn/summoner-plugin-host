//! A fresh std Mutex's first lock allocates, so: a FRESH chain's FIRST
//! blocks, not after a warm-up, allocate nothing on the chain thread. The child runs under the allocation counter
//! (`libsummoner_lab_malloccount.dylib`, injected with the two recorders) with `--noalloc-blocks 8`, which arms the counter
//! around each of the chain thread's first 8 blocks, the ack included. The chain holds a hosted `<fake>` slot and an
//! unhosted one (a description this build does not host: its cell is one `prepare` never locks), so both of the
//! chain thread's mutexes, the procs lock and a cell first touched by it, are in the window.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive, Input};
use common::*;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;

const B: usize = 64;
const KEY: u64 = 61;
const COUNTED: u32 = 8;

fn settle(link: &Arc<summoner_rt::rt::ChainLink>) {
    let until = Instant::now() + Duration::from_secs(5);
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        assert!(Instant::now() < until, "the chain never acked");
        std::thread::yield_now();
    }
}

/// A fresh chain of `slots` (an effect with noise in, or an instrument with `events`), its chain thread's first
/// `COUNTED` blocks counted: the counters line.
fn first_blocks(
    name: &str,
    slots: &[&str],
    fx: bool,
    events: Vec<summoner_rt::rt::MidiEvent>,
    hosted: &[bool],
) -> String {
    let counter = lab_plugins::dir().join("libsummoner_lab_malloccount.dylib");
    assert!(counter.is_file(), "tools/build-plugins.sh");
    let dir = scratch(&format!("{name}-counters"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("counters.txt");
    let mut rig = Rig::start_inserting(
        name,
        &child_bin(),
        vec![
            "--noalloc-blocks".into(),
            COUNTED.to_string().into(),
            "--counters".into(),
            path.clone().into(),
        ],
        &[counter],
    );
    let mut sp = spec(slots);
    sp.geometry = Geometry {
        ch_in: if fx { 2 } else { 0 },
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let Event::ChainReady {
        link, slots: got, ..
    } = rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    let ok: Vec<bool> = got.iter().map(|s| s.ok).collect();
    assert_eq!(ok, hosted, "{name}: the slots hosted as expected");
    let mut eng = Drive::new(
        if fx { noise(48_000) } else { Input::None },
        vec![Bridged {
            link: link.clone(),
            events: Arc::from(events),
            fx,
            params: Vec::new(),
        }],
        B,
    );
    for _ in 0..COUNTED + 4 {
        eng.block();
        settle(&link);
    }
    let lookups = std::fs::read_to_string(&rig.lookups).unwrap_or_default();
    drop(rig); // shutdown: the child writes its counters
    let text = std::fs::read_to_string(&path).expect("the counters");
    let line = text
        .lines()
        .find(|l| l.starts_with(&format!("chain {KEY} noalloc_blocks")))
        .unwrap_or_else(|| panic!("no noalloc line:\n{text}"))
        .to_string();
    println!("{name}: {line}");
    assert!(!lookups.is_empty(), "{name}: the lookup recorder ran");
    line
}

fn au_xml(bundle: &str, name: &str, kind: &[u8; 4], sub: &[u8; 4]) -> String {
    au_ref(
        &lab_plugins::dir()
            .join(format!("au/{bundle}.component"))
            .display()
            .to_string(),
        kind,
        sub,
        b"SaSL",
        name,
    )
}

/// The same, with the lab AUs (after the first AU load): the gain as an effect, the HostCallbacks fixture, the
/// synth with notes in its first blocks. AudioToolbox's render dispatch, the input callback, the transport's publish
/// and the HostCallbacks are inside the window.
#[test]
fn a_fresh_au_chains_first_blocks_allocate_nothing_on_the_chain_thread() {
    if lab_plugins::absent("a_fresh_au_chains_first_blocks_allocate_nothing_on_the_chain_thread") {
        return;
    }
    let want = format!("chain {KEY} noalloc_blocks {COUNTED} allocations 0");
    let gain = au_xml("summoner-lab-gain", "Summoner Lab Gain", b"aufx", b"SLgn");
    assert_eq!(
        first_blocks("noalloc-au-gain", &[&gain], true, vec![], &[true]),
        want,
        "the AU gain"
    );
    // The HostCallbacks: the block's transport published before each render, and the fixture's three
    // callbacks answered from it on the chain thread, inside the window.
    let hostcb = au_xml(
        "summoner-lab-hostcb",
        "Summoner Lab Host Callbacks (reports them)",
        b"aufx",
        b"SLhc",
    );
    assert_eq!(
        first_blocks("noalloc-au-hostcb", &[&hostcb], true, vec![], &[true]),
        want,
        "the AU asking for its host callbacks"
    );
    let synth = au_xml("summoner-lab-synth", "Summoner Lab Synth", b"aumu", b"SLsy");
    let notes = vec![
        summoner_rt::rt::MidiEvent::new(0, [0x90, 69, 100]),
        summoner_rt::rt::MidiEvent::new(70, [0x90, 72, 90]),
        summoner_rt::rt::MidiEvent::new(130, [0xb0, 64, 127]),
        summoner_rt::rt::MidiEvent::new(300, [0x80, 69, 0]),
    ];
    assert_eq!(
        first_blocks("noalloc-au-synth", &[&synth], false, notes, &[true]),
        want,
        "the AU synth"
    );
}

#[test]
fn a_fresh_chains_first_blocks_allocate_nothing_on_the_chain_thread() {
    if lab_plugins::absent("a_fresh_chains_first_blocks_allocate_nothing_on_the_chain_thread") {
        return;
    }
    let counter = lab_plugins::dir().join("libsummoner_lab_malloccount.dylib");
    assert!(counter.is_file(), "tools/build-plugins.sh");
    let dir = scratch("noalloc-first-block-counters");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("counters.txt");
    let mut rig = Rig::start_inserting(
        "noalloc-first-block",
        &child_bin(),
        vec![
            "--noalloc-blocks".into(),
            COUNTED.to_string().into(),
            "--counters".into(),
            path.clone().into(),
        ],
        &[counter],
    );
    let mut sp = spec(&[
        r#"{"format":"fake","fake":{"name":"g","gain":"0.5"}}"#,
        r#"{"format":"vst3","path":"/nowhere/none.vst3","classId":"00000000000000000000000000000000","name":"none"}"#,
    ]);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let Event::ChainReady { link, slots, .. } =
        rig.wait(|e| matches!(e, Event::ChainReady { key: KEY, .. }))
    else {
        unreachable!()
    };
    assert!(slots[0].ok && !slots[1].ok, "one hosted slot, one unhosted");
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], B);
    for _ in 0..COUNTED + 4 {
        eng.block();
        settle(&link);
    }
    drop(rig); // shutdown: the child writes its counters
    let text = std::fs::read_to_string(&path).expect("the counters");
    let line = text
        .lines()
        .find(|l| l.starts_with(&format!("chain {KEY} noalloc_blocks")))
        .unwrap_or_else(|| panic!("no noalloc line:\n{text}"));
    println!("{line}");
    assert_eq!(
        line,
        format!("chain {KEY} noalloc_blocks {COUNTED} allocations 0"),
        "the chain thread's first blocks allocate nothing"
    );
}
