//! Real installed plugins in a chain. The
//! shipped child (tools/build-shipped.sh's build, run unsigned so the recorders inject) loads each description in
//! `SUMMONER_REAL_DESC` (a lab file of the gated scan's own descriptions; plugin names stay in the lab, never in this
//! repo), through the summoner supervisor and the tests' realtime driver (`common::drive`), as an effect with noise in:
//! a VST3 by its installed path, an AU by its bundle path and codes (protocol 4.0's `PluginRef`; there is no registry
//! route). Each renders 64 blocks and its state is read back.
//! `#[ignore]`d: run by hand only, under fs_usage:
//!
//! `SUMMONER_REAL_DESC=<lab file> cargo test -p summoner-host --test real_chain -- --ignored --nocapture`
//!
//! The recorders' STOPs (the shared rules) are printed, not asserted: a real plugin writes its own folders (the
//! rule: a vendor's own folders are fine, anything else is a STOP judged from the fs_usage recording).
mod common;

use common::null_harness::*;
use common::*;
use std::path::PathBuf;
use std::process::Command;

fn shipped_child() -> PathBuf {
    let out = Command::new(repo().join("tools/build-shipped.sh"))
        .current_dir(repo())
        .output()
        .expect("tools/build-shipped.sh");
    assert!(
        out.status.success(),
        "build-shipped: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo().join("target.noindex/shipped/release/summoner-host")
}

#[test]
#[ignore = "real installed plugins: by hand, under fs_usage"]
fn real_plugins_load_render_and_read_state_in_the_shipped_child() {
    let file = std::env::var("SUMMONER_REAL_DESC")
        .expect("SUMMONER_REAL_DESC: the lab file of scan classes");
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let child = shipped_child();
    for (i, v) in list.iter().enumerate() {
        let d = class_slot(v).expect("SUMMONER_REAL_DESC: a scan's classes (protocol 4.0)");
        let how = if d.au {
            "AU by path and codes"
        } else {
            "VST3 by path"
        };
        let xml = d.slot.clone();
        let mut s = fx(&format!("real-chain-{i}"), &[&xml]);
        s.blocks = 64;
        s.stereo = true;
        let t = run_judged(&child, &s, &[], |rig, name| {
            let recs = fswatch_v2::parse(&fswatch_v2::read_log(&rig.log));
            let stops = fswatch_v2::stops(&recs);
            eprintln!(
                "{name}: {} records, {} recorder STOP lines",
                recs.len(),
                stops.len()
            );
            for l in stops.iter().take(20) {
                eprintln!("{name}: STOP? {l}");
            }
        });
        // Counts and shapes only (no plugin names in the output that reaches the repo's reports).
        let samples: Vec<f32> = t.out.iter().map(|b| f32::from_bits(*b)).collect();
        let loud = samples.iter().filter(|x| x.abs() > 1e-4).count();
        let hosted = t
            .ready
            .first()
            .is_some_and(|r| r.contains("[(true, false,"));
        let shape = t
            .ready
            .first()
            .and_then(|r| r.split_once("] [").map(|(_, x)| x.to_string()))
            .unwrap_or_default();
        println!(
            "REAL {i} {how}: hosted {hosted}; slot {shape}; samples {} finite {} loud {loud}; state bytes {:?}; events {}",
            samples.len(),
            samples.iter().all(|x| x.is_finite()),
            t.read.iter().map(Vec::len).collect::<Vec<_>>(),
            t.events.len()
        );
        assert!(hosted, "{how}: not hosted");
        assert!(samples.iter().all(|x| x.is_finite()), "{how}: non-finite");
        assert_eq!(t.read.len(), 1, "{how}: one state read");
    }
}

/// An audit gap for real plugins ("a real third-party plugin under these ops is still untested"):
/// each description in `SUMMONER_REAL_DESC`, in the shipped child, through the host application's own sequences on a
/// running chain (as the host application's park / unpark and depth-change null does on the lab fixture): a live raise
/// to depth 2 and back, the hub switch (`chain.unpark {group 2}`), the node detached, `chain.park`, `chain.unpark
/// {group 0}`, the node back. No second implementation loads these here (only `--plugin-root` pins are compared), so
/// the judgment is health: hosted, every block acked at once (no thread left on the wrong address), the region's group
/// moved, finite output, sound before the park and again after the unpark, the state read back.
#[test]
#[ignore = "real installed plugins: by hand, under fs_usage"]
fn real_plugins_survive_park_unpark_hub_switch_and_depth_changes() {
    let file = std::env::var("SUMMONER_REAL_DESC")
        .expect("SUMMONER_REAL_DESC: the lab file of scan classes");
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let child = shipped_child();
    for (i, v) in list.iter().enumerate() {
        let d = class_slot(v).expect("SUMMONER_REAL_DESC: a scan's classes (protocol 4.0)");
        let how = if d.au {
            "AU by path and codes"
        } else {
            "VST3 by path"
        };
        let xml = d.slot.clone();
        let mut s = fx(&format!("real-ops-{i}"), &[&xml]);
        s.blocks = 120;
        s.chain_ops = vec![
            (16, ChainOp::Depth(2)),
            (32, ChainOp::Depth(1)),
            (44, ChainOp::Unpark(2)),
            (56, ChainOp::Detach),
            (66, ChainOp::Park),
            (76, ChainOp::Unpark(0)),
            (77, ChainOp::Attach),
        ];
        let t = run_judged(&child, &s, &[], |rig, name| {
            let recs = fswatch_v2::parse(&fswatch_v2::read_log(&rig.log));
            eprintln!("{name}: {} records", recs.len());
        });
        let b = B;
        let block_loud = |k: usize| {
            t.out[k * b..(k + 1) * b]
                .iter()
                .any(|x| f32::from_bits(*x).abs() > 1e-4)
        };
        let before = (8..56).filter(|k| block_loud(*k)).count();
        let parked = (60..77).filter(|k| block_loud(*k)).count();
        let after = (90..120).filter(|k| block_loud(*k)).count();
        let hosted = t
            .ready
            .first()
            .is_some_and(|r| r.contains("[(true, false,"));
        println!(
            "REAL-OPS {i} {how}: hosted {hosted}; sound blocks before {before}/48, detached+parked {parked}/17, after {after}/30; events {:?}",
            t.events
        );
        assert!(hosted, "{how}: not hosted");
        assert!(
            t.out.iter().all(|x| f32::from_bits(*x).is_finite()),
            "{how}: non-finite"
        );
        assert!(
            t.events.iter().any(|e| e == "blocks slower than 100 ms: 0"),
            "{how}: a block was slow: {:?}",
            t.events
        );
        assert!(
            t.events.iter().any(|e| e == "unparked 2, region group 2"),
            "{how}: the hub switch: {:?}",
            t.events
        );
        assert!(
            !t.events.iter().any(|e| e.starts_with("stuck")),
            "{how}: {:?}",
            t.events
        );
        assert!(
            before > 0 && after > 0,
            "{how}: sound before the park and after the unpark"
        );
        assert_eq!(t.read.len(), 1, "{how}: one state read");
    }
}

/// Stability first: the soak of the installed plugins. Every description in `SUMMONER_REAL_DESC`, in ONE chain (in
/// the file's order) of the shipped child, at real time (64-frame blocks paced to 48 kHz) for `SUMMONER_SOAK_SECS` (default
/// 60), noise in, a state read of every slot every 5 s. As real_vst3.rs's soak of the pinned copies: the chain never
/// goes down and is never quarantined; every block is acked within 600 ms; every sample is finite; resident memory
/// grows at most 16 MiB and the fds at most 4 between the warm-up (10 s) and the end; `leaks` printed, not asserted (a
/// real plugin's own leaks cannot be told from ours here).
#[test]
#[ignore = "real installed plugins, wall-clock: by hand, under fs_usage"]
fn real_installed_plugins_soak_in_one_chain() {
    use crate::common::drive::{Bridged, Drive};
    use std::time::{Duration, Instant};
    use summoner::supervisor::{Command, Event};
    let secs: u64 = std::env::var("SUMMONER_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let file = std::env::var("SUMMONER_REAL_DESC")
        .expect("SUMMONER_REAL_DESC: the lab file of scan classes");
    let list: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let xmls: Vec<String> = list
        .iter()
        .filter_map(|v| class_slot(v).map(|c| c.slot))
        .collect();
    let mut rig = Rig::start_child("real-installed-soak", &shipped_child(), vec![]);
    let pid = rig.child_pid;
    let mut sp = spec(&xmls.iter().map(String::as_str).collect::<Vec<_>>());
    sp.geometry = summoner_proto::layout::Geometry {
        ch_in: 2,
        ..summoner_proto::layout::Geometry::stereo(B as u32)
    };
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states: vec![],
    });
    let link = match rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    }) {
        Event::ChainReady { link, slots, .. } => {
            assert!(slots.iter().all(|s| s.ok), "every slot loads: {slots:?}");
            link
        }
        e => panic!("the chain: {e:?}"),
    };
    let mut eng = Drive::new(
        noise_repeating(48_000 * 4, 48_000 * (secs as usize + 1)),
        vec![Bridged::fx(&link)],
        B,
    );
    let period = Duration::from_secs_f64(B as f64 / 48_000.0);
    let start = Instant::now();
    let (mut blocks, mut reads, mut bad, mut loud) = (0u64, 0u64, 0u64, 0u64);
    let mut slowest = Duration::ZERO;
    let mut warm: Option<(u64, usize)> = None;
    let mut next_read = Duration::from_secs(5);
    let mut events = 0usize;
    while start.elapsed() < Duration::from_secs(secs) {
        let y = eng.block();
        bad += y.iter().filter(|x| !x.is_finite()).count() as u64;
        loud += y.iter().any(|x| x.abs() > 1e-4) as u64;
        let t0 = Instant::now();
        assert!(
            null_harness::settle(&link, Duration::from_millis(600)),
            "block {blocks}: not acked in 600 ms"
        );
        slowest = slowest.max(t0.elapsed());
        blocks += 1;
        let due = start + period * blocks as u32;
        if let Some(w) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(w);
        }
        rig.advance(period);
        while let Ok(e) = rig.rx.try_recv() {
            assert!(
                !matches!(e, Event::ChainDown { .. } | Event::Quarantined { .. }),
                "after {:?}, {blocks} blocks: {e:?}",
                start.elapsed()
            );
            events += 1;
        }
        if warm.is_none() && start.elapsed() >= Duration::from_secs(10.min(secs / 2)) {
            warm = Some(rss_fds(pid));
        }
        if start.elapsed() >= next_read {
            next_read += Duration::from_secs(5);
            for slot in 0..xmls.len() as u32 {
                rig.send(Command::GetState { key: KEY, slot });
                reads += 1;
            }
        }
    }
    let end = rss_fds(pid);
    let warm = warm.expect("a warm-up sample");
    let leaks = std::process::Command::new("/usr/bin/leaks")
        .arg(pid.to_string())
        .output()
        .expect("leaks");
    // SUMMONER_LEAKS_OUT: the whole `leaks` report into that (lab) file (with MallocStackLogging=1 in the env, each leak's
    // allocation stack).
    if let Ok(f) = std::env::var("SUMMONER_LEAKS_OUT") {
        std::fs::write(f, &leaks.stdout).unwrap();
    }
    let leaks = String::from_utf8_lossy(&leaks.stdout)
        .lines()
        .find(|l| l.contains(" leaks for "))
        .unwrap_or("no leaks summary")
        .to_string();
    println!(
        "REAL-SOAK {secs} s, {} slots: {blocks} blocks ({loud} with sound), slowest ack {slowest:?}, {reads} state reads; \
         rss {} -> {} KiB ({:+}), fds {} -> {}; non-finite {bad}; leaks: {leaks}; events {events}",
        xmls.len(),
        warm.0,
        end.0,
        end.0 as i64 - warm.0 as i64,
        warm.1,
        end.1
    );
    assert_eq!(bad, 0, "non-finite output samples");
    assert!(
        end.0 <= warm.0 + 16 * 1024,
        "resident memory grew {} KiB",
        end.0 as i64 - warm.0 as i64
    );
    assert!(end.1 <= warm.1 + 4, "open fds grew {} -> {}", warm.1, end.1);
}
