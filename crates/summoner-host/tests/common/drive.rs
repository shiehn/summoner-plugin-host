//! The tests' realtime driver: the host application side of the bridge exchange, run from the test thread, with
//! the bridge crate alone (the child depends on the bridge and FFI crates only).
//!
//! A [`Drive`] is one device callback's worth of the host application's audio path for these tests: an [`Input`] (a
//! mono sample played into both channels from frame 0), zero or more bridged chains, each one
//! [`summoner_rt::rt::ChainState`] visited with that input (an effect) or with silence (an instrument), and their sum
//! as the output pair. Each visit is the same [`summoner_rt::rt::ChainState::visit`] call a host application's graph
//! node makes, with a [`Visit`] built as a host application builds it (the block's end frame as the hub value, the
//! clock's position, tempo, loop and run state, the release at a stop edge or a running reposition). After every chain
//! is visited, each (page, group) whose chain published is rung once ([`summoner_rt::rt::ring_group`]), as the host
//! application's ring plan does for chains on one level. A render link is exchanged through
//! [`summoner_rt::render::render_visit`].
//!
//! It is not the host application's graph: no fader, no pan law, no PDC between parallel chains, no crossfade at a
//! change. The tests compare a child with itself, with another child binary through the same driver, or with the dry
//! input, so none of that matters to them; what the child sees on the wire is the host application's.
#![allow(dead_code)]

use std::sync::Arc;
use summoner_rt::rt::{AudioCounters, ChainLink, ChainState, Chase, MidiEvent, Tempo, Visit};

/// The source: a mono sample into both channels of an effect chain's input.
#[derive(Clone, Default)]
pub enum Input {
    /// No source (an instrument's tests).
    #[default]
    None,
    /// The sample once from frame 0, then silence.
    Once(Arc<[f32]>),
    /// The sample again every `len` frames, for `total` frames, then silence.
    Repeat(Arc<[f32]>, usize),
    /// Two channels: `.0` into the left, `.1` scaled by `.2` into the right (a source whose channels
    /// differ, for the channel-rule tests).
    Stereo(Box<Input>, f32),
}

impl Input {
    /// The input at timeline frame `t`.
    fn at(&self, t: u64) -> f32 {
        match self {
            Input::None => 0.0,
            Input::Once(d) => d.get(t as usize).copied().unwrap_or(0.0),
            Input::Repeat(d, total) if !d.is_empty() && (t as usize) < *total => {
                d[t as usize % d.len()]
            }
            Input::Repeat(..) => 0.0,
            Input::Stereo(l, _) => l.at(t),
        }
    }

    /// The right channel's input at timeline frame `t` (the left's but for [`Input::Stereo`]).
    fn at_r(&self, t: u64) -> f32 {
        match self {
            Input::Stereo(l, k) => l.at(t) * k,
            i => i.at(t),
        }
    }
}

/// One bridged chain: its link (from `ChainReady`), its timeline MIDI, effect or instrument, and its model
/// parameters' values (the host application's `Kind::Bridge` node and its parameter values).
#[derive(Clone)]
pub struct Bridged {
    pub link: Arc<ChainLink>,
    pub events: Arc<[MidiEvent]>,
    pub fx: bool,
    pub params: Vec<f64>,
}

impl Bridged {
    /// An effect with no MIDI and no parameter values.
    pub fn fx(link: &Arc<ChainLink>) -> Bridged {
        Bridged {
            link: link.clone(),
            events: Arc::from(Vec::new()),
            fx: true,
            params: Vec::new(),
        }
    }
}

/// Clock A: loop, tempo, run state and position (the host application's clock node; running from frame 0 by default).
#[derive(Clone, Copy, Debug, Default)]
pub struct ClockDesc {
    pub loop_frames: u64,
    pub loop_start: u64,
    pub tempo: Tempo,
}

struct Clock {
    desc: ClockDesc,
    running: bool,
    pos: u64,
    stop: bool,
    reposition: bool,
}

struct Slot {
    chain: Bridged,
    /// None while detached.
    state: Option<ChainState>,
}

/// The driver (see the module doc).
pub struct Drive {
    input: Input,
    slots: Vec<Slot>,
    clock: Clock,
    counters: AudioCounters,
    rate: f64,
    block: usize,
    frame: u64,
    l: Vec<f32>,
    r: Vec<f32>,
    sum_l: Vec<f32>,
    sum_r: Vec<f32>,
    /// The last block, interleaved stereo (`2 × frames`; the first `2 × n` after a short block).
    pub out: Vec<f32>,
}

impl Drive {
    /// `chains` fed by `input`, at most `block` frames a callback, at 48 kHz.
    pub fn new(input: Input, chains: Vec<Bridged>, block: usize) -> Drive {
        Drive::new_at(input, chains, block, super::SR)
    }

    /// As [`Drive::new`], at sample rate `rate`.
    pub fn new_at(input: Input, chains: Vec<Bridged>, block: usize, rate: f64) -> Drive {
        let declick = (0.003 * rate).round() as u32;
        let slots = chains
            .into_iter()
            .map(|mut chain| {
                // The host application's table: one value per `link.param_ids`, 0 where the description gives none.
                chain.params.resize(chain.link.param_ids.len(), 0.0);
                let state = fresh(&chain, block, declick);
                Slot {
                    chain,
                    state: Some(state),
                }
            })
            .collect();
        Drive {
            input,
            slots,
            clock: Clock {
                desc: ClockDesc::default(),
                running: true,
                pos: 0,
                stop: false,
                reposition: false,
            },
            counters: AudioCounters::default(),
            rate,
            block,
            frame: 0,
            l: vec![0.0; block],
            r: vec![0.0; block],
            sum_l: vec![0.0; block],
            sum_r: vec![0.0; block],
            out: vec![0.0; 2 * block],
        }
    }

    /// The input alone, straight to the output (the tests' dry reference).
    pub fn dry(input: Input, block: usize) -> Drive {
        Drive::new(input, Vec::new(), block)
    }

    /// Clock A's description (before the first block).
    pub fn set_clock(&mut self, desc: ClockDesc) {
        self.clock.desc = desc;
    }

    /// The host application's clock run command, applied at the next block's first frame.
    pub fn clock_run(&mut self, run: bool) {
        if self.clock.running && !run {
            self.clock.stop = true;
        }
        self.clock.running = run;
    }

    /// The host application's clock reposition command, applied at the next block's first frame.
    pub fn clock_reposition(&mut self, at: u64) {
        self.clock.pos = at;
        self.clock.reposition = true;
    }

    /// Chain `i`'s model parameter `p`, from the next block (none past the chain's `param_ids`, as in the host
    /// application).
    pub fn set_param(&mut self, i: usize, p: usize, value: f64) {
        if let Some(v) = self.slots[i].chain.params.get_mut(p) {
            *v = value;
        }
    }

    /// Chain `i` exchanged through `link` from the next block (an unpark's group, a depth change): the same state
    /// when the region is the same (a depth change is in place), a fresh one otherwise.
    pub fn relink(&mut self, i: usize, link: Arc<ChainLink>) {
        let declick = self.declick();
        let block = self.block;
        let s = &mut self.slots[i];
        let same = s.chain.link.region.instance_id() == link.region.instance_id();
        s.chain.params.resize(link.param_ids.len(), 0.0);
        s.chain.link = link;
        if !same && s.state.is_some() {
            s.state = Some(fresh(&s.chain, block, declick));
        }
    }

    /// Chain `i` no longer visited (the host application's node removed).
    pub fn detach(&mut self, i: usize) {
        self.slots[i].state = None;
    }

    /// Chain `i` visited again, with a fresh state (the host application's node added again).
    pub fn attach(&mut self, i: usize) {
        let (block, declick) = (self.block, self.declick());
        let s = &mut self.slots[i];
        s.state = Some(fresh(&s.chain, block, declick));
    }

    /// The path's latency the host application's schedule would report: the largest attached chain's plugin latency
    /// plus its depth × B, 0 with none.
    pub fn latency(&self) -> u32 {
        self.slots
            .iter()
            .filter(|s| s.state.is_some())
            .map(|s| s.chain.link.pdc_latency(self.block))
            .max()
            .unwrap_or(0)
    }

    /// The exchange's counters (the host application's `counters().bridge`).
    pub fn counters(&self) -> &AudioCounters {
        &self.counters
    }

    /// One full block; the left channel.
    pub fn block(&mut self) -> Vec<f32> {
        self.block_frames(self.block)
    }

    /// One block of `frames` (at most the driver's block: a callback shorter than its maximum); the left channel. The
    /// right channel is `self.out[1..2 * frames]` by twos.
    pub fn block_frames(&mut self, frames: usize) -> Vec<f32> {
        let n = frames.min(self.block);
        let t0 = self.frame;
        let input: Vec<f32> = (0..n as u64).map(|i| self.input.at(t0 + i)).collect();
        let input_r: Vec<f32> = (0..n as u64).map(|i| self.input.at_r(t0 + i)).collect();
        let c = &self.clock;
        let release_edge = c.stop || (c.reposition && c.running);
        let visit = |release: bool| Visit {
            block: t0 + n as u64,
            offset: 0,
            len: n,
            total: n,
            pos: c.pos,
            block_pos: c.pos,
            loop_frames: c.desc.loop_frames,
            loop_start: c.desc.loop_start,
            sample_rate: self.rate,
            tempo: c.desc.tempo,
            playing: c.running,
            release,
        };
        let mut rings: Vec<(Arc<summoner_rt::region::ChildPage>, usize)> = Vec::new();
        self.sum_l[..n].fill(0.0);
        self.sum_r[..n].fill(0.0);
        let any = self.slots.iter().any(|s| s.state.is_some());
        for s in &mut self.slots {
            let Some(st) = s.state.as_mut() else {
                continue;
            };
            let (l, r) = (&mut self.l[..n], &mut self.r[..n]);
            if s.chain.fx {
                l.copy_from_slice(&input);
                r.copy_from_slice(&input_r);
            } else {
                l.fill(0.0);
                r.fill(0.0);
            }
            // The host application's tag rule: a fresh state records its clock's tag with no release; then the stop
            // edge or a running reposition releases.
            let release = if st.clock_tag() == ChainState::FRESH_TAG {
                st.set_clock_tag(1); // the host application's clock A
                false
            } else {
                release_edge
            };
            let v = visit(release);
            let lk = &s.chain.link;
            if let Some(job) = &lk.render {
                summoner_rt::render::render_visit(
                    job,
                    lk,
                    &v,
                    [l, r],
                    &s.chain.events,
                    &s.chain.params,
                );
            } else if st.visit_chased(
                lk,
                &v,
                [l, r],
                &s.chain.events,
                &s.chain.params,
                &self.counters,
                &Chase::NONE,
            ) {
                let g = lk.group.min(3) as usize;
                if !rings
                    .iter()
                    .any(|(p, q)| Arc::ptr_eq(p, &lk.page) && *q == g)
                {
                    rings.push((lk.page.clone(), g));
                }
            }
            for i in 0..n {
                self.sum_l[i] += self.l[i];
                self.sum_r[i] += self.r[i];
            }
        }
        for (page, g) in &rings {
            summoner_rt::rt::ring_group(page, *g, t0 + n as u64, &self.counters);
        }
        if !any && self.slots.is_empty() {
            self.sum_l[..n].copy_from_slice(&input);
            self.sum_r[..n].copy_from_slice(&input_r);
        }
        for i in 0..n {
            self.out[2 * i] = self.sum_l[i];
            self.out[2 * i + 1] = self.sum_r[i];
        }
        // The clock after the block: a running clock advances (and wraps at its loop); the edges are consumed.
        let c = &mut self.clock;
        if c.running {
            c.pos += n as u64;
            if c.desc.loop_frames > c.desc.loop_start && c.pos >= c.desc.loop_frames {
                let len = c.desc.loop_frames - c.desc.loop_start;
                c.pos = c.desc.loop_start + (c.pos - c.desc.loop_start) % len;
            }
        }
        c.stop = false;
        c.reposition = false;
        self.frame += n as u64;
        self.out[..2 * n].iter().step_by(2).copied().collect()
    }

    fn declick(&self) -> u32 {
        (0.003 * self.rate).round() as u32
    }
}

fn fresh(chain: &Bridged, block: usize, declick: u32) -> ChainState {
    ChainState::new(
        chain.link.region.instance_id(),
        chain.link.depth,
        chain.link.param_ids.len(),
        block,
        declick,
    )
}
