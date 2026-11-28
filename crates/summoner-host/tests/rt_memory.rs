//! The child's realtime memory (one wired arena for the realtime path): the memory this
//! child's chain threads reach per block is runecraft-rt's wired arena (`runecraft_rt::arena`), counted in
//! `--counters`' `rt_arena` line. The inventory: a loaded chain's working set is live in the arena under this child's
//! owner tag, on wired chunks (nothing unwired, no `mlock` refused), and it is gone again after the unload.
#![forbid(unsafe_code)]

mod common;

use common::drive::{Bridged, Drive};
use common::*;
use std::time::Duration;
use summoner::supervisor::{Command, Event};
use summoner_proto::layout::Geometry;

const B: usize = 64;

/// The `rt_arena` line's fields, by name.
fn arena(text: &str) -> std::collections::BTreeMap<String, u64> {
    let line = text
        .lines()
        .find(|l| l.starts_with("rt_arena "))
        .unwrap_or_else(|| panic!("no rt_arena line:\n{text}"));
    let w: Vec<&str> = line.split_whitespace().skip(1).collect();
    w.chunks(2)
        .map(|p| (p[0].to_string(), p[1].parse().unwrap()))
        .collect()
}

/// A child, one chain of `slots` loaded (noise in, `blocks` blocks), then `unload` or not, then shut down: its
/// counters.
fn run(name: &str, slots: &[&str], blocks: usize, unload: bool) -> String {
    run_with(name, &[], slots, blocks, unload).0
}

/// As [`run`], the child given `args` too; also the supervisor's `child_wired_bytes` / `child_unwired_bytes` gauges
/// after the blocks (the sum of what `chain.loaded` reported).
fn run_with(
    name: &str,
    args: &[&str],
    slots: &[&str],
    blocks: usize,
    unload: bool,
) -> (String, (u64, u64)) {
    let dir = scratch(&format!("{name}-counters"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("counters.txt");
    let mut a: Vec<std::ffi::OsString> = vec!["--counters".into(), path.clone().into()];
    a.extend(args.iter().map(Into::into));
    let mut rig = Rig::start_with(name, a);
    let mut sp = spec(slots);
    sp.geometry = Geometry {
        ch_in: 2,
        ..Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: 71,
        spec: sp,
        states: vec![],
    });
    let link = match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: 71, .. } | Event::ChainDown { key: 71, .. }
        )
    }) {
        Event::ChainReady { link, .. } => link,
        e => panic!("{e:?}"),
    };
    let mut eng = Drive::new(noise(48_000), vec![Bridged::fx(&link)], B);
    for _ in 0..blocks {
        eng.block();
        assert!(null_harness::settle(&link, Duration::from_millis(600)));
    }
    let gauges = {
        let c = rig.bridge.counters();
        (c.child_wired_bytes(), c.child_unwired_bytes())
    };
    if unload {
        rig.send(Command::Unload { key: 71 });
        std::thread::sleep(Duration::from_millis(300));
    }
    drop(rig);
    let until = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(t) = std::fs::read_to_string(&path)
            && !t.is_empty()
        {
            let _ = std::fs::remove_dir_all(&dir);
            return (t, gauges);
        }
        assert!(std::time::Instant::now() < until, "no counters");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A loaded chain's working set (the channel and bypass buffers, the MIDI buffer, the faders) is live in the arena
/// under this child's owner tag, on wired memory: nothing unwired, no `mlock` refused.
#[test]
fn a_loaded_chains_working_set_is_wired_arena_memory() {
    if lab_plugins::absent("a_loaded_chains_working_set_is_wired_arena_memory") {
        return;
    }
    let text = run(
        "rt-mem-live",
        &[
            r#"{"format":"fake","fake":{"name":"a","gain":"0.5"}}"#,
            r#"{"format":"fake","fake":{"name":"b"}}"#,
        ],
        20,
        false,
    );
    let a = arena(&text);
    eprintln!("{a:?}");
    // At least the 2 + 2 buffers of 64 frames, the MIDI buffer and the faders.
    assert!(a["host_live"] >= 4 * B as u64 * 4, "{a:?}");
    assert!(a["wired_bytes"] > 0, "{a:?}");
    assert_eq!(
        (a["unwired_bytes"], a["lock_refused"], a["orphaned"]),
        (0, 0, 0),
        "{a:?}"
    );
    // Every block the chain thread reads, one by one, in a wired arena block. At the chain's start: the 2 + 2 channel
    // buffers, their two tables, the MIDI buffer, the faders (`--stats` off: no stats buffer), the chain record, its
    // slots' records, its procs gate (a `WiredLock`). At its prepare: the handover cells, the sides, the bypass
    // latencies, the delay lines' table, the slots' buses.
    assert!(
        text.contains("chain 71 rt_scratch checked 16 unwired 0"),
        "{text}"
    );
}

/// After the unload, the chain's blocks are given back (the message thread drops them): the owner's live bytes fall.
#[test]
fn an_unloaded_chains_working_set_is_given_back() {
    if lab_plugins::absent("an_unloaded_chains_working_set_is_given_back") {
        return;
    }
    let slots = [r#"{"format":"fake","fake":{"name":"a","gain":"0.5"}}"#];
    let live = arena(&run("rt-mem-kept", &slots, 10, false))["host_live"];
    let gone = arena(&run("rt-mem-gone", &slots, 10, true))["host_live"];
    assert!(
        live > 0 && gone < live,
        "live {live}, after the unload {gone}"
    );
}

/// The `locked_regions` line's fields.
fn locked(text: &str) -> std::collections::BTreeMap<String, u64> {
    let line = text
        .lines()
        .find(|l| l.starts_with("locked_regions "))
        .unwrap_or_else(|| panic!("no locked_regions line:\n{text}"));
    let w: Vec<&str> = line.split_whitespace().skip(1).collect();
    w.chunks(2)
        .map(|p| (p[0].to_string(), p[1].parse().unwrap()))
        .collect()
}

/// Shim ABI 3: a prepared VST3 slot's process state (its parameter queues in and out, its events, its output
/// ring) is the shim's, placed in a wired region the child owns: the child's locked regions are mapped and wired, all
/// of them.
#[test]
fn a_vst3_slots_shim_state_is_in_a_wired_region() {
    if lab_plugins::absent("a_vst3_slots_shim_state_is_in_a_wired_region") {
        return;
    }
    let text = run("rt-mem-vst3", &[&gain_xml()], 10, false);
    let l = locked(&text);
    eprintln!("{l:?}");
    assert!(l["mapped_bytes"] > 0, "a region for the shim: {l:?}");
    assert_eq!(
        l["wired_bytes"], l["mapped_bytes"],
        "all of it wired: {l:?}"
    );
}

/// A line's `key value` pairs after its first `skip` words.
fn pairs(line: &str, skip: usize) -> std::collections::BTreeMap<String, u64> {
    let w: Vec<&str> = line.split_whitespace().skip(skip).collect();
    w.chunks(2)
        .map(|p| (p[0].to_string(), p[1].parse().unwrap()))
        .collect()
}

fn line<'a>(text: &'a str, prefix: &str) -> &'a str {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line:\n{text}"))
}

/// The chain's region (this side's view) and the child page are wired in place, whole
/// pages, and the chain thread wired the top 256 KiB of its own stack; nothing was refused. `chain.loaded` reported
/// it: the supervisor's `child_wired_bytes` holds at least the region, the stack and the arena's blocks, and
/// `child_unwired_bytes` is 0.
#[test]
fn a_loaded_chains_region_page_and_stack_are_wired_and_reported() {
    if lab_plugins::absent("a_loaded_chains_region_page_and_stack_are_wired_and_reported") {
        return;
    }
    let (text, (wired, unwired)) = run_with(
        "rt-mem-wire",
        &[],
        &[
            r#"{"format":"fake","fake":{"name":"a","gain":"0.5"}}"#,
            r#"{"format":"fake","fake":{"name":"b"}}"#,
        ],
        10,
        false,
    );
    let page = 16_384u64;
    let c = pairs(line(&text, "chain 71 wired "), 3);
    eprintln!("{c:?}\n{}", line(&text, "child_page "));
    let (region, stack) = (c["region_bytes"], c["stack_bytes"]);
    assert!(region >= page && region % page == 0, "{c:?}");
    assert_eq!((stack, c["stack_unwired_bytes"]), (256 << 10, 0), "{c:?}");
    let p = pairs(line(&text, "child_page "), 1);
    assert!(p["wired_bytes"] >= page && p["unwired_bytes"] == 0, "{p:?}");
    let host_live = arena(&text)["host_live"];
    assert!(
        wired >= region + stack + host_live / 2 && unwired == 0,
        "chain.loaded's wiredBytes {wired} / unwiredBytes {unwired}: region {region}, stack {stack}, arena {host_live}"
    );
}

/// The stack wire size (measured, with a 256 KiB floor): the deepest the chain thread's stack goes while it runs
/// the lab's plugins (the test double's two slots, then the lab's VST3 gain) stays under half the wired top.
#[test]
fn the_chain_threads_stack_stays_well_inside_its_wired_top() {
    if lab_plugins::absent("the_chain_threads_stack_stays_well_inside_its_wired_top") {
        return;
    }
    for (name, slots) in [
        (
            "rt-mem-water-fake",
            vec![
                r#"{"format":"fake","fake":{"name":"a","gain":"0.5"}}"#.to_string(),
                r#"{"format":"fake","fake":{"name":"b"}}"#.to_string(),
            ],
        ),
        ("rt-mem-water-vst3", vec![gain_xml()]),
    ] {
        let slots: Vec<&str> = slots.iter().map(String::as_str).collect();
        let (text, _) = run_with(name, &["--stack-water"], &slots, 20, false);
        let c = pairs(line(&text, "chain 71 wired "), 3);
        let used = c["stack_used"];
        eprintln!("{name}: {c:?}");
        assert!(
            used > 0 && used <= (256 << 10) / 2,
            "{name}: the chain thread used {used} bytes of stack"
        );
    }
}
