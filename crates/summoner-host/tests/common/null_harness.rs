//! The null harness: a scenario rendered by one child through the supervisor and the tests' realtime driver
//! (`common::drive`) in lockstep, normalised to a trace two runs can be compared by (`vst3_self_null.rs` runs every
//! scenario twice through this child). Only the shared half lives here: `settle`, `run`, `Scenario`, `fx` and
//! `note`, which the other tests use too.
#![allow(dead_code)]

use super::drive::{Bridged, ClockDesc, Drive, Input};
use super::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use summoner::supervisor::{Command, Event, SlotState};
use summoner_proto::layout::Geometry;
use summoner_rt::rt::MidiEvent;

pub const B: usize = 64;
/// What one scenario does.
#[derive(Clone, Default)]
pub struct Scenario {
    pub name: String,
    pub slots: Vec<String>,
    /// An instrument chain (no input) when false; else the noise source feeds it.
    pub fx: bool,
    /// Region parameters (slot << 24 | index) and their initial node values.
    pub params: Vec<u32>,
    pub values: Vec<f64>,
    pub events: Vec<MidiEvent>,
    /// States applied at load (before any block): (slot, text).
    pub states: Vec<(u32, String)>,
    pub blocks: usize,
    /// (block, node parameter, value): set before that block.
    pub changes: Vec<(usize, u16, f64)>,
    /// Open then close slot 0's editor after the blocks (the test double's "editor" moves its state).
    pub editor: bool,
    /// After the blocks stop acking: advance the clock this many times by 2.1 s, collecting down / quarantine.
    pub advances: usize,
    /// Which of those advances are the child's respawns (by index, in order: respawn #1, #2, …). Each waits for the
    /// supervisor to have ARMED that respawn before advancing (`Rig::advance_to_respawn`, the same 2.1 s), so an
    /// advance never lands between a death's ChainDown and its arming (the manual-clock race). Measured
    /// in both runs: crash-in-process #0, crash-in-set-state #0; the wedge's two are the detector's (none).
    pub respawns: Vec<usize>,
    /// State blobs applied at load after `states` (slot, the whole blob: a `vst3` blob's chunks, say).
    pub blobs: Vec<(u32, Vec<u8>)>,
    /// Also record the right channel (`Trace::out_r`; the hostile TRANSPORT reporter's tempo, flags and samples
    /// are on it). Off for the older scenarios, whose traces are exactly as before.
    pub stereo: bool,
    /// Clock A's description (loop length, tempo, meter), the Bridge's request line's transport. The default
    /// is no loop, 120 bpm, 4/4, so every scenario without one renders as before.
    pub clock: ClockDesc,
    /// (block, op) on clock A, applied before that block (the driver's [`Drive::clock_run`] /
    /// [`Drive::clock_reposition`], as the host application's `Control::clock_run` / `clock_reposition`). Clock A runs
    /// from frame 0.
    pub clock_ops: Vec<(usize, ClockOp)>,
    /// Wait this long after the chain is ready, before the first block, so what a plugin raised during
    /// its load-time state (a `restartComponent`) is taken by the child's poll first (this child polls every 50 ms).
    /// 0: no wait, as before.
    pub pause_ms: u64,
    /// After the blocks, ask for slot 0's parameter list (`Command::ParamList`, offset 0, 256) and put the
    /// reply into the trace's events (`params <total> [(id, name)…]`).
    pub param_list: bool,
    /// Record the `childGen` each final state read carries (`state <slot> childGen <n>` in the events).
    pub child_gen: bool,
    /// The sample rate (the supervisor's and the engine's); `None` is 48 kHz, as before.
    pub rate: Option<f64>,
    /// The block size (the chain's max frames and the engine's block); `None` is [`B`], as before.
    pub block: Option<usize>,
    /// Each block's length, cycled (a device's callbacks shorter than the maximum, so the child gets
    /// partial blocks); empty: every block full, as before.
    pub frames: Vec<usize>,
    /// The chain's (inputs, outputs); `None`: stereo out, stereo in for an effect (as before).
    pub channels: Option<(u32, u32)>,
    /// Blocks after which the harness does NOT wait for the child's ack (so a slow block makes the next
    /// ones miss, and the host application's breaker opens); empty: every block settles, as before.
    pub unsettled: Vec<usize>,
    /// (block, ms) sleep before that block, after its changes (so a busy child finishes and the host application's
    /// breaker closes on that block, with `BF_DISCONTINUITY`); empty: none, as before.
    pub sleeps: Vec<(usize, u64)>,
    /// The chain's pipeline depth at load
    /// (`ChainSpec::depth`); 0 is the spec's own (1), as before.
    pub depth: u8,
    /// (block, op) on the chain, before that block's clock ops and changes, in order (the host application's
    /// park / unpark / hub switch / depth change, as its e2e_audio and e2e_recovery drive them); empty: none.
    pub chain_ops: Vec<(usize, ChainOp)>,
    /// The chain is a render set (`role: render`), driven as the host application's render worker drives one: after
    /// each block the harness wakes the chain's `seq`, then settles on its `ack`. false: a live chain, as before.
    pub render: bool,
}

/// One operation on the scenario's chain ([`Scenario::chain_ops`]). Each records a line in the trace's events.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChainOp {
    /// Take the bridge node out of the graph (the engine stops publishing; its Retire entry fades out).
    Detach,
    /// A recompile (the Retire entry's release, e2e_audio's `apply(|_| {})`), then `chain.park`, waiting for
    /// `chain.parked`.
    Park,
    /// `chain.unpark {group}`, waiting for `chain.unparked`; the link then carries the new group (the hub switch).
    Unpark(u8),
    /// Put the bridge node back (with the current link), wired as at the start.
    Attach,
    /// The host application's depth change (`Event::DepthChange`): the same region at depth d, adopted by the engine.
    Depth(u8),
}

/// One command to clock A ([`Scenario::clock_ops`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockOp {
    /// `deck.play` (true) / `deck.stop` (false).
    Run(bool),
    /// `transport.setPosition`, in frames.
    Reposition(u64),
}

/// What a child made of a scenario.
#[derive(Debug, PartialEq)]
pub struct Trace {
    pub ready: Vec<String>,
    pub events: Vec<String>,
    pub out: Vec<u32>,
    pub read: Vec<Vec<u8>>,
    /// The right channel's bits, only when the scenario asks (`Scenario::stereo`).
    pub out_r: Vec<u32>,
}

pub fn normalize(e: &Event) -> Option<String> {
    Some(match e {
        Event::ChainReady {
            names, slots, link, ..
        } => format!(
            "ready {names:?} {:?} latency {}",
            slots
                .iter()
                .map(|s| (s.ok, s.quarantined, s.latency, s.param_count))
                .collect::<Vec<_>>(),
            link.plugin_latency
        ),
        Event::ChainDown { reason, .. } => format!("down {reason:?}"),
        Event::StateApplied {
            slot,
            generation,
            ok,
            ..
        } => format!("applied {slot} {generation} {ok}"),
        Event::Quarantined {
            slot, plugin, op, ..
        } => format!("quarantined {slot} {plugin} {op}"),
        Event::LatencyChanged { link, .. } => format!("latency {}", link.plugin_latency),
        Event::Exhausted => "exhausted".into(),
        _ => return None,
    })
}

pub fn settle(link: &Arc<summoner_rt::rt::ChainLink>, d: Duration) -> bool {
    let until = Instant::now() + d;
    let r = &link.region;
    while r.ack().load(Ordering::Acquire) < r.seq().load(Ordering::Acquire) {
        if Instant::now() > until {
            return false;
        }
        std::thread::yield_now();
    }
    true
}

pub const KEY: u64 = 41;

pub fn run(child: &Path, s: &Scenario) -> Trace {
    run_with(child, s, &[])
}

/// As [`run`], with the child's command-line `args` (`--plugin-root <pin>`, say). With args, the run's fswatch log is
/// judged too (the shared v3 STOP rules, and no plugin folder in any line): a real bundle's load, either child.
pub fn run_with(child: &Path, s: &Scenario, args: &[&str]) -> Trace {
    if args.is_empty() {
        run_judged(child, s, args, |_, _| {})
    } else {
        run_judged(child, s, args, judge_fswatch)
    }
}

/// As [`run_with`], with the caller's own judgment of the run's rig (its recorders) at the end (a run
/// whose plugin reads inside `/System/Library/Components/CoreAudio.component`, which [`judge_fswatch`] forbids
/// outright).
pub fn run_judged(child: &Path, s: &Scenario, args: &[&str], judge: impl Fn(&Rig, &str)) -> Trace {
    eprintln!("null: {} on {} {args:?}", s.name, child.display());
    let rate = s.rate.unwrap_or(SR);
    let mut rig = Rig::start_child_at(
        &format!("null-{}", s.name),
        child,
        args.iter().map(|a| (*a).into()).collect(),
        rate,
    );
    let mut sp = spec(&s.slots.iter().map(String::as_str).collect::<Vec<_>>());
    sp.geometry = Geometry {
        ch_in: if s.fx { 2 } else { 0 },
        ..Geometry::stereo(s.block.unwrap_or(B) as u32)
    };
    if let Some((ch_in, ch_out)) = s.channels {
        sp.geometry.ch_in = ch_in;
        sp.geometry.ch_out = ch_out;
    }
    sp.param_ids = Arc::from(s.params.clone());
    if s.depth != 0 {
        sp.depth = s.depth;
    }
    if s.render {
        sp.role = summoner_proto::codec::Role::Render;
    }
    let states = s
        .states
        .iter()
        .enumerate()
        .map(|(i, (slot, text))| {
            SlotState::new(
                *slot,
                i as u64 + 1,
                Arc::from(summoner_proto::state_blob::builtin(text.as_bytes())),
            )
        })
        .chain(s.blobs.iter().enumerate().map(|(i, (slot, blob))| {
            SlotState::new(
                *slot,
                (s.states.len() + i) as u64 + 1,
                Arc::from(blob.clone()),
            )
        }))
        .collect();
    rig.send(Command::Load {
        key: KEY,
        spec: sp,
        states,
    });
    let mut trace = Trace {
        ready: Vec::new(),
        events: Vec::new(),
        out: Vec::new(),
        read: Vec::new(),
        out_r: Vec::new(),
    };
    let first = rig.wait(|e| {
        matches!(
            e,
            Event::ChainReady { key: KEY, .. } | Event::ChainDown { key: KEY, .. }
        )
    });
    trace.ready.extend(normalize(&first));
    for e in rig.seen.drain(..) {
        trace.events.extend(normalize(&e));
    }
    if let Event::ChainReady {
        mut link, slots, ..
    } = first
    {
        let mut eng = Drive::new_at(
            if s.fx { noise(48_000) } else { Input::None },
            vec![Bridged {
                link: link.clone(),
                events: Arc::from(s.events.clone()),
                fx: s.fx,
                params: s.values.clone(),
            }],
            s.block.unwrap_or(B),
            rate,
        );
        eng.set_clock(s.clock);
        std::thread::sleep(Duration::from_millis(s.pause_ms));
        let mut slow = 0usize;
        for k in 0..s.blocks {
            for (_, op) in s.chain_ops.iter().filter(|(b, _)| *b == k) {
                let relink = |l: &Arc<summoner_rt::rt::ChainLink>, group: u8, depth: u8| {
                    Arc::new({
                        let mut msg = summoner_rt::rt::ChainLink::new(
                            l.region.clone(),
                            l.page.clone(),
                            group,
                            depth,
                            l.monitor,
                            l.plugin_latency,
                            l.param_ids.clone(),
                        );
                        // A live link (only a render job's own link carries one).
                        msg.render = None;
                        msg
                    })
                };
                match *op {
                    ChainOp::Detach => eng.detach(0),
                    ChainOp::Park => {
                        rig.send(Command::Park { key: KEY });
                        rig.wait(|e| matches!(e, Event::Parked { key: KEY }));
                        // As e2e_audio: the parked thread has moved to its own seq before the next block.
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    ChainOp::Unpark(group) => {
                        rig.send(Command::Unpark { key: KEY, group });
                        if let Event::Unparked { group: g, .. } =
                            rig.wait(|e| matches!(e, Event::Unparked { key: KEY, .. }))
                        {
                            trace.events.push(format!(
                                "unparked {g}, region group {}",
                                link.region.group().load(Ordering::Acquire)
                            ));
                        }
                        link = relink(&link, group, link.depth);
                        eng.relink(0, link.clone());
                    }
                    ChainOp::Attach => {
                        eng.relink(0, link.clone());
                        eng.attach(0);
                    }
                    ChainOp::Depth(depth) => {
                        link = relink(&link, link.group, depth);
                        eng.relink(0, link.clone());
                    }
                }
                trace.events.push(format!(
                    "block {k} {op:?}: schedule latency {}",
                    eng.latency()
                ));
            }
            for (_, op) in s.clock_ops.iter().filter(|(b, _)| *b == k) {
                match *op {
                    ClockOp::Run(run) => eng.clock_run(run),
                    ClockOp::Reposition(at) => eng.clock_reposition(at),
                }
            }
            for (_, p, v) in s.changes.iter().filter(|(b, _, _)| *b == k) {
                eng.set_param(0, usize::from(*p), *v);
            }
            for (_, ms) in s.sleeps.iter().filter(|(b, _)| *b == k) {
                std::thread::sleep(Duration::from_millis(*ms));
            }
            let full = s.block.unwrap_or(B);
            let n = if s.frames.is_empty() {
                full
            } else {
                s.frames[k % s.frames.len()].min(full)
            };
            trace
                .out
                .extend(eng.block_frames(n).iter().map(|x| x.to_bits()));
            if s.stereo {
                trace.out_r.extend(
                    eng.out[..2 * n]
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|x| x.to_bits()),
                );
            }
            if s.render {
                // The render worker: publish (the block), wake seq, wait for ack (the settle below).
                let _ = link.region.seq_waker().wake();
            }
            let t0 = Instant::now();
            if !s.unsettled.contains(&k) && !settle(&link, Duration::from_millis(600)) {
                trace.events.push(format!("stuck after block {k}"));
                break;
            }
            if t0.elapsed() > Duration::from_millis(100) {
                slow += 1;
            }
        }
        if !s.chain_ops.is_empty() {
            // A chain thread left waiting on the wrong address (its own seq after an unpark, the
            // old hub after a switch) still acks within its 250 ms wait timeout, which settle's 600 ms allows; a
            // healthy one acks at once. Both children: 0.
            trace
                .events
                .push(format!("blocks slower than 100 ms: {slow}"));
        }
        if !s.unsettled.is_empty() {
            // What the host application's request line did: each breaker close sets BF_DISCONTINUITY on the
            // next request (the bridge's rt.rs close).
            let b = eng.counters();
            trace.events.push(format!(
                "breaker opens {} late {}",
                b.breaker_opens.load(Ordering::Relaxed),
                b.late.load(Ordering::Relaxed)
            ));
        }
        if s.editor {
            rig.send(Command::EditorOpen {
                key: KEY,
                slot: 0,
                guard_reset: false,
            });
            rig.send(Command::EditorClose {
                key: KEY,
                slot: Some(0),
            });
        }
        if s.param_list {
            rig.send(Command::ParamList {
                key: KEY,
                slot: 0,
                offset: 0,
                count: 256,
            });
            if let Event::ParamList {
                ok, total, params, ..
            } = rig.wait(|e| {
                matches!(
                    e,
                    Event::ParamList {
                        key: KEY,
                        slot: 0,
                        ..
                    }
                )
            }) {
                // The null compares `{id, name}` only: the 3.7 fields (`pluginId`, `automatable`, `default`, `value`,
                // `stepCount`, `label`, `order`) are compared in tests/param_set.rs.
                let params: Vec<(u32, String)> =
                    params.into_iter().map(|p| (p.id, p.name)).collect();
                trace.events.push(format!("params {ok} {total} {params:?}"));
            }
        }
        if s.advances == 0 {
            // Only hosted slots have a state (a quarantined or unhosted slot answers "no such slot", no event).
            for slot in (0..s.slots.len() as u32).filter(|i| slots[*i as usize].ok) {
                rig.send(Command::GetState { key: KEY, slot });
                if let Event::StateRead {
                    blob, child_gen, ..
                } = rig.wait(|e| matches!(e, Event::StateRead { slot: k, .. } if *k == slot))
                {
                    trace.read.push(blob.to_vec());
                    if s.child_gen {
                        trace
                            .events
                            .push(format!("state {slot} childGen {child_gen}"));
                    }
                }
            }
        }
    }
    for i in 0..s.advances {
        match s.respawns.iter().position(|&r| r == i) {
            Some(k) => rig.advance_to_respawn(k as u32 + 1),
            None => rig.advance(Duration::from_millis(2100)),
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    std::thread::sleep(Duration::from_millis(100));
    while let Ok(e) = rig.rx.try_recv() {
        rig.seen.push(e);
    }
    for e in rig.seen.drain(..) {
        trace.events.extend(normalize(&e));
    }
    judge(&rig, &s.name);
    trace
}

/// A run's fswatch log, judged: the shared v3 STOP rules, and no plugin folder in any line (a real bundle's load,
/// either child).
pub fn judge_fswatch(rig: &Rig, name: &str) {
    let text = fswatch_v2::read_log(&rig.log);
    assert_fswatch_v2(&text);
    for bad in ["/Library/Audio/Plug-Ins", "/System/Library/Components"] {
        assert!(
            !text.lines().any(|l| l.contains(bad)),
            "{name}: {bad} touched:\n{text}"
        );
    }
}

pub fn fx(name: &str, slots: &[&str]) -> Scenario {
    Scenario {
        name: name.into(),
        slots: slots.iter().map(|s| s.to_string()).collect(),
        fx: true,
        blocks: 24,
        ..Default::default()
    }
}

pub fn note(frame: u64, b: [u8; 3]) -> MidiEvent {
    MidiEvent::new(frame, b)
}
