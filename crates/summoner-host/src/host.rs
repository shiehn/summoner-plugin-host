//! The shared state and the message thread: every non-inline op, in arrival order, on one thread (the main
//! thread, which an editor's window will need later). Each op that touches a plugin writes the child page's status
//! record first and clears it after, so a crash inside it is attributed.
use crate::Opts;
use crate::chain::{Chain, ChainArc, PendingTake, SlotCell, SlotEntry};
use crate::slots;
use crate::wire::{self, Writer, die, empty};
use runecraft_ipc::bridge::{MachReceive, MachSend};
use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use summoner_plugin::handover;
use summoner_proc::{Chunks, EditorAttach, Notice, Plugin, SetOutcome, StateKind};
use summoner_proto::codec::{self, MAX_BLOB, kind};
use summoner_proto::layout::Geometry;
use summoner_proto::layout::*;
use summoner_proto::state_blob::{self, Blob};
use summoner_rt::region::{ChildPage, Region};
use summoner_sys::editor::{Editor, Window};

pub enum Op {
    /// The meta, the region, and whether the meta names an `instanceId` at all.
    Load(codec::ChainLoad, OwnedFd, bool),
    Unload(u64),
    Prepare(u64),
    /// Protocol 3.8: a render set's processors released and made again.
    PrepareFresh(u64),
    SetState(codec::StateSet, Vec<u8>),
    GetState(u64, codec::StateGet),
    EditorOpen(codec::EditorRef),
    EditorClose(codec::EditorRef),
    Workgroup(codec::WorkgroupAttach),
    ParamList(u64, codec::ParamList),
    ParamSet(u64, codec::ParamSet),
    SlotEnabled(u64, codec::SlotEnabled),
    Shutdown(u64),
}

pub struct Shared {
    pub opts: Opts,
    pub w: Writer,
    page: Mutex<Option<Arc<ChildPage>>>,
    chains: Mutex<BTreeMap<u64, ChainArc>>,
    /// `state.set`s of the both-absent vst3 blob, the counted no-op (the host application's `stateEmptySets`).
    pub empty_sets: AtomicU64,
    /// The editor windows: opened and closed so far, and on screen at the last close (the dylib's
    /// count), for `--counters`.
    pub editor_opened: AtomicU64,
    pub editor_closed: AtomicU64,
    pub editor_on_screen: AtomicU64,
    /// AU editors that were CoreAudioKit's generic view (no Cocoa UI).
    pub editor_generic: AtomicU64,
    /// Re-opens that brought an open window to front, counted when it then was, and windows
    /// the test hook minimised: `--counters`.
    pub editor_refronted: AtomicU64,
    pub editor_minimized: AtomicU64,
    /// The editor-reset guard's arms and applied restores: `--counters`.
    pub editor_guard_armed: AtomicU64,
    /// `editor.closed` events sent (every close the child makes, an unload's included): `--counters`.
    pub editor_closed_sent: AtomicU64,
    pub editor_guard_restored: AtomicU64,
    /// The child page's bytes a refused wire left unwired (`--counters`).
    pub page_unwired: AtomicU64,
    /// Request `corr` keys ignored (not the log's grammar), and `log.set`s applied and refused (`--counters`).
    pub corr_ignored: AtomicU64,
    pub log_set_applied: AtomicU64,
    pub log_set_refused: AtomicU64,
}

impl Shared {
    pub fn new(opts: Opts, w: Writer) -> Shared {
        Shared {
            opts,
            w,
            page: Mutex::new(None),
            chains: Mutex::new(BTreeMap::new()),
            empty_sets: AtomicU64::new(0),
            page_unwired: AtomicU64::new(0),
            corr_ignored: AtomicU64::new(0),
            log_set_applied: AtomicU64::new(0),
            log_set_refused: AtomicU64::new(0),
            editor_opened: AtomicU64::new(0),
            editor_closed: AtomicU64::new(0),
            editor_on_screen: AtomicU64::new(0),
            editor_generic: AtomicU64::new(0),
            editor_refronted: AtomicU64::new(0),
            editor_minimized: AtomicU64::new(0),
            editor_guard_armed: AtomicU64::new(0),
            editor_closed_sent: AtomicU64::new(0),
            editor_guard_restored: AtomicU64::new(0),
        }
    }

    pub fn page(&self) -> Option<Arc<ChildPage>> {
        self.page.lock().unwrap().clone()
    }

    pub fn set_page(&self, p: Arc<ChildPage>) {
        *self.page.lock().unwrap() = Some(p);
    }

    pub fn chain(&self, key: u64) -> Option<ChainArc> {
        self.chains.lock().unwrap().get(&key).cloned()
    }

    pub fn chain_count(&self) -> usize {
        self.chains.lock().unwrap().len()
    }

    pub fn all_chains(&self) -> Vec<ChainArc> {
        self.chains.lock().unwrap().values().cloned().collect()
    }

    /// `state.generations` (inline): every slot's child generation and editor flag.
    pub fn generations(&self) -> codec::Generations {
        let mut slots = Vec::new();
        for (key, c) in self.chains.lock().unwrap().iter() {
            for (i, s) in c.slots.iter().enumerate() {
                slots.push(codec::GenEntry::new(
                    *key,
                    i as u32,
                    s.child_gen.load(Ordering::SeqCst),
                    s.editor_open.load(Ordering::SeqCst),
                ));
            }
        }
        codec::Generations::new(slots)
    }

    /// The op in flight, for the host application (the child page's status record) and for the crash log (the
    /// breadcrumb).
    fn status(&self, op: u32, key: u64, slot: u32, name: &str) {
        if let Some(p) = self.page() {
            p.write_status(
                op,
                key,
                slot,
                name,
                runecraft_ipc::bridge::host_ticks().max(1),
            );
        }
        summoner_sys::crash::set_breadcrumb(op_name(op), name);
    }

    fn clear_status(&self) {
        if let Some(p) = self.page() {
            p.clear_status();
        }
        summoner_sys::crash::clear_breadcrumb();
    }
}

/// The slot `slot` of chain `key`, with its chain kept alive.
fn slot_of(sh: &Shared, key: u64, slot: u32) -> Option<(ChainArc, usize)> {
    let c = sh.chain(key)?;
    ((slot as usize) < c.slots.len()).then_some((c, slot as usize))
}

/// How often the message thread looks at what plugins raised on their own ([`summoner_proc::Plugin::poll`]).
const POLL: Duration = Duration::from_millis(50);

/// Every hosted slot's notices, then the reactivation handover's turn: a chain whose latency moved sends
/// `latency.changed`.
fn poll_plugins(sh: &Shared) {
    // One Turn per main-loop turn; at most one take of the turn may block.
    let mut turn = handover::Turn::new();
    for c in sh.all_chains() {
        // Every slot is polled (each takes its own notices), then the chain's latency is checked once.
        let mut any = false;
        for (k, s) in c.slots.iter().enumerate() {
            let Some(p) = &s.plugin else { continue };
            // An edit by the plugin's own controller moved its state, so the slot's childGen moves (the
            // host application's generations poll then reads it).
            let edits = p.edits();
            if s.edits_seen.swap(edits, Ordering::SeqCst) != edits {
                s.child_gen.fetch_add(1, Ordering::SeqCst);
            }
            // The processor took the host's values: the state may have moved again (a VST3 whose
            // processor keeps its own copy), so the slot's childGen moves once more.
            if s.host_set_waiting.load(Ordering::SeqCst) && !p.host_set_pending() {
                s.host_set_waiting.store(false, Ordering::SeqCst);
                s.child_gen.fetch_add(1, Ordering::SeqCst);
            }
            match p.poll() {
                Notice::None => {}
                Notice::Latency => any = true,
                Notice::Reactivate => s.restart_pending.store(true, Ordering::SeqCst),
                // A notice a later slot raises: nothing this host acts on.
                _ => {}
            }
            any |= reactivation(sh, &c, k, &mut turn);
        }
        if any {
            check_latency(sh, &c);
        }
    }
}

/// The reactivation handover on the message thread, for one slot, one turn (the handover's API for a child,
/// `summoner_plugin::handover`):
/// - a raised restart announces (`HOSTED -> DRAINING`); a word not `HOSTED` (a cycle still returning) keeps the flag;
/// - the turn's one blocking take (`take_blocking`, at most `TAKE_GIVE_UP`) while `Turn::may_wait()`, else one
///   `try_take` per turn (forcing past `take_bound`), until it succeeds or `take_horizon` passes (`abandoned`);
/// - with the processor taken: the cycle ([`summoner_proc::Dsp::reactivate`], the SDK's), the give-back, then
///   `slot.reactivated` (the counts since the last one), before the latency check's `latency.changed`.
///
/// The chain thread never waits on any of it; the slot passes its bypass signal through while its processor is out.
/// Returns whether a cycle ran (the latency may have moved).
fn reactivation(sh: &Shared, c: &Chain, k: usize, turn: &mut handover::Turn) -> bool {
    let s = &c.slots[k];
    let Some(cells) = c.cells.lock().unwrap().clone() else {
        return false; // not prepared: nothing to re-activate
    };
    let cell = &cells[k];
    let bound = handover::take_bound(c.geo.max_frames, c.rate);
    let horizon = handover::take_horizon(c.render);
    let mut pend = s.pending.lock().unwrap();
    if pend.is_none() {
        if !s.restart_pending.swap(false, Ordering::SeqCst) {
            return false;
        }
        if !cell.announce() {
            s.restart_pending.store(true, Ordering::SeqCst);
            return false;
        }
        let pt = PendingTake {
            announced: Instant::now(),
            forced: false,
        };
        if turn.may_wait() {
            sh.status(OP_PREPARE, c.key, k as u32, &s.name);
            let r = cell.take_blocking(bound, Duration::from_millis(1), handover::TAKE_GIVE_UP);
            sh.clear_status();
            match r {
                Ok(t) => {
                    drop(pend);
                    cycle(sh, c, k, cell, t.guard, t.forced, false);
                    return true;
                }
                Err(e) => {
                    turn.record_waits(1);
                    eprintln!(
                        "[summoner-host] chain {} slot {k}: the take gave up at its bound ({e:?}); re-attempted each turn",
                        c.key
                    );
                    *pend = Some(PendingTake {
                        forced: cell.state() == handover::OUT,
                        ..pt
                    });
                    return false;
                }
            }
        }
        // This turn's one wait is spent: the re-attempt path, starting now.
        *pend = Some(pt);
    }
    let Some(mut pt) = *pend else { return false };
    let attempt = cell.try_take(pt.announced.elapsed() >= bound);
    pt.forced |= attempt.forced;
    turn.record_waits(attempt.waits);
    match attempt.guard {
        Some(g) => {
            *pend = None;
            drop(pend);
            cycle(sh, c, k, cell, g, pt.forced, true);
            true
        }
        None if pt.announced.elapsed() >= horizon => {
            *pend = None;
            eprintln!(
                "[summoner-host] chain {} slot {k}: the take did not complete in {horizon:?}; abandoned",
                c.key
            );
            reactivated(sh, c, k, 0, pt.forced, true, true);
            false
        }
        None => {
            *pend = Some(pt);
            false
        }
    }
}

/// The cycle with the processor taken (no lock held but the cell's, which the chain thread only `try_lock`s), then
/// the give-back and `slot.reactivated`.
fn cycle(
    sh: &Shared,
    c: &Chain,
    k: usize,
    cell: &SlotCell,
    mut guard: runecraft_rt::rt_lock::WiredLockGuard<'_, Option<Box<dyn summoner_proc::Dsp>>>,
    forced: bool,
    given_up: bool,
) {
    let s = &c.slots[k];
    let t0 = Instant::now();
    let r = match guard.as_mut() {
        Some(d) => {
            sh.status(OP_PREPARE, c.key, k as u32, &s.name);
            let r = d.reactivate();
            sh.clear_status();
            r
        }
        None => Err("not prepared".into()),
    };
    let took = t0.elapsed();
    cell.give_back(guard);
    s.reactivations.fetch_add(1, Ordering::Relaxed);
    if took > handover::cycle_budget(c.geo.max_frames, c.rate) {
        s.stalls.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "[summoner-host] chain {} slot {k} ({}): the cycle took {took:?}, over its budget",
            c.key, s.name
        );
    }
    if let Err(why) = r {
        eprintln!(
            "[summoner-host] chain {} slot {k} ({}): re-activation failed: {why}",
            c.key, s.name
        );
    }
    reactivated(
        sh,
        c,
        k,
        took.as_nanos().min(u128::from(u64::MAX)) as u64,
        forced,
        given_up,
        false,
    );
}

/// `slot.reactivated` (kind 60): the counts since the last one, the slot's latency after the cycle.
fn reactivated(
    sh: &Shared,
    c: &Chain,
    k: usize,
    cycle_ns: u64,
    forced: bool,
    given_up: bool,
    abandoned: bool,
) {
    let s = &c.slots[k];
    let r = {
        let mut msg = codec::SlotReactivated::default();
        msg.chain_key = c.key;
        msg.slot = k as u32;
        msg.flags = handover::K_LATENCY_CHANGED;
        msg.bypassed_blocks = s
            .bypassed
            .swap(0, Ordering::Relaxed)
            .min(u64::from(u32::MAX)) as u32;
        msg.cycle_ns = cycle_ns;
        msg.latency = s.plugin.as_ref().map_or(0, |p| p.latency());
        msg.forced_take = forced;
        msg.misaligned = s
            .misaligned
            .swap(0, Ordering::Relaxed)
            .min(u64::from(u32::MAX)) as u32;
        msg.take_given_up = given_up;
        msg.abandoned = abandoned;
        msg
    };
    wire::event(&sh.w, kind::SLOT_REACTIVATED, &r, &[]);
}

/// The editor windows, main thread only (the editor dylib is not `Send`): the dylib, loaded on the
/// first real `editor.open` (never in scan mode), and each open window by (chain, slot).
#[derive(Default)]
struct Ui {
    ed: Option<Editor>,
    windows: BTreeMap<(u64, u32), Window>,
    /// When each window opened, and whether its close button was pressed (`--test-press-close`).
    opened_at: BTreeMap<(u64, u32), (Instant, bool)>,
    /// Windows the minimise hook already pressed (`--test-minimize`), and the click hook (`--test-click`).
    minimized: std::collections::BTreeSet<(u64, u32)>,
    clicked: std::collections::BTreeSet<(u64, u32)>,
    /// `--test-no-activate` (test-hooks): editor windows do not activate the process (activation is the default).
    no_activate: bool,
    /// The editor-reset guards in progress, by window.
    guards: BTreeMap<(u64, u32), ResetGuard>,
    /// Why the editor dylib could not be loaded, once it could not (it is not tried again).
    load_failed: Option<String>,
    /// `--test-editor-dylib` (test-hooks): the dylib's path, overriding the lookup.
    dylib_override: Option<String>,
}

/// How long the message thread waits for an op while an editor window is open, before it pumps the run loop again:
/// the windows' events are served at least this often (the pump between ops).
const UI_TICK: Duration = Duration::from_millis(10);
/// The plugins' poll while an editor window is open: the editors' 60 Hz main-thread tick (audio -> GUI values).
/// Without a window, [`POLL`].
const UI_POLL: Duration = Duration::from_millis(16);

/// `editor.availability` (a plugin with no view the host can show).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Availability {
    chain_key: u64,
    slot: u32,
    available: bool,
}

/// An op as the reader posts it, with its request's `corr` (absent when the request had none).
pub struct Posted {
    pub corr: Option<String>,
    pub op: Op,
}

pub fn message_thread(ops: Receiver<Posted>, sh: Arc<Shared>, own: Option<Arc<MachReceive>>) {
    // Chains whose thread is wedged inside a plugin: kept alive, their processors never touched again.
    let mut abandoned: Vec<ChainArc> = Vec::new();
    let mut ui = Ui::default();
    #[cfg(feature = "test-hooks")]
    {
        ui.dylib_override = sh.opts.editor_dylib.clone();
        ui.no_activate = sh.opts.no_activate;
    }
    let mut polled = Instant::now();
    loop {
        let wait = if ui.windows.is_empty() { POLL } else { UI_TICK };
        let next = match ops.recv_timeout(wait) {
            Ok(op) => Some(op),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // The run loop, between ops: the windows' events, then any close button pressed.
        if let Some(ed) = &ui.ed {
            #[cfg(feature = "test-hooks")]
            if let Some(ms) = sh.opts.press_close_ms {
                for (k, w) in &ui.windows {
                    if let Some((at, pressed)) = ui.opened_at.get_mut(k)
                        && !*pressed
                        && at.elapsed() >= Duration::from_millis(ms)
                    {
                        *pressed = true;
                        let _ = ed.perform_close(w);
                    }
                }
            }
            #[cfg(feature = "test-hooks")]
            if let Some(ms) = sh.opts.minimize_ms {
                for (k, w) in &ui.windows {
                    if !ui.minimized.contains(k)
                        && ui
                            .opened_at
                            .get(k)
                            .is_some_and(|(at, _)| at.elapsed() >= Duration::from_millis(ms))
                    {
                        ui.minimized.insert(*k);
                        if ed.minimize(w) {
                            sh.editor_minimized.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                }
            }
            #[cfg(feature = "test-hooks")]
            if let Some(ms) = sh.opts.click_ms {
                for (k, w) in &ui.windows {
                    if !ui.clicked.contains(k)
                        && ui
                            .opened_at
                            .get(k)
                            .is_some_and(|(at, _)| at.elapsed() >= Duration::from_millis(ms))
                    {
                        ui.clicked.insert(*k);
                        let _ = ed.post_click(w, sh.opts.click_title_bar);
                    }
                }
            }
            // The run loop reaches the views' code (drawRect, events): with one window open the status
            // record names its plugin, so a view that hangs here is attributed (with several, the op stays idle).
            let named = (ui.windows.len() == 1)
                .then(|| ui.windows.keys().next().copied())
                .flatten()
                .and_then(|(ck, sl)| {
                    slot_of(&sh, ck, sl).map(|(c, i)| (ck, sl, c.slots[i].name.clone()))
                });
            if let Some((ck, sl, name)) = &named {
                sh.status(OP_EDITOR, *ck, *sl, name);
            }
            let _ = ed.pump(0.0);
            if named.is_some() {
                sh.clear_status();
            }
            let asked: Vec<(u64, u32)> = ui
                .windows
                .iter()
                .filter(|(_, w)| ed.take_close(w))
                .map(|(k, _)| *k)
                .collect();
            for (chain_key, slot) in asked {
                editor_close(
                    &sh,
                    &mut ui,
                    codec::EditorRef::new(chain_key, Some(slot), false),
                );
            }
            // The editor-reset guards' due ticks, after the run loop delivered the windows' events.
            run_guards(&sh, &mut ui);
        }
        let Some(Posted { corr: req_corr, op }) = next else {
            let every = if ui.windows.is_empty() { POLL } else { UI_POLL };
            if polled.elapsed() >= every {
                poll_plugins(&sh);
                // The arena's control-tick work, here on the message thread: blocks a chain thread dropped (an
                // unwind) freed, and a reserve chunk mapped ahead of the next chain's need.
                let arena = runecraft_rt::arena::RtArena::global();
                arena.drain_orphans();
                arena.grow_ahead();
                polled = Instant::now();
            }
            continue;
        };
        // Every record the op writes on this thread carries the host application's id for the request.
        let _corr = req_corr
            .as_deref()
            .map(|c| crate::diag::CorrScope::enter(Some(c)));
        match op {
            Op::Load(m, fd, given) => load(&sh, m, fd, given),
            Op::Unload(key) => {
                crate::diag::log(
                    crate::diag::Level::Info,
                    crate::diag::area::HOST,
                    corr(key),
                    "chain.unload",
                    vec![("chain", crate::diag::Value::U64(key))],
                );
                // An unload closes the chain's windows SILENTLY (no `editor.closed`): the host application reports
                // those itself (`Event::EditorGone` before its own reload's unload; a user unload drops the chain at
                // once). A child event here would arrive after a same-key reload began and carry the dying chain's
                // state read onto the new chain (editor-windows.md, the 3.4 section).
                close_windows(&sh, &mut ui, Some(key));
                let c = sh.chains.lock().unwrap().remove(&key);
                if let Some(c) = c
                    && release(&sh, &c).is_none()
                {
                    abandoned.push(c);
                }
                let r = codec::ChainRef::new(key, true);
                wire::answer_event(
                    &sh.w,
                    kind::CHAIN_UNLOADED,
                    &r,
                    &[],
                    wire::op_corr().as_deref(),
                );
            }
            Op::Prepare(key) => prepare(&sh, key),
            Op::PrepareFresh(key) => prepare_fresh(&sh, key),
            Op::SetState(m, blob) => set_state(&sh, m, &blob),
            Op::GetState(id, m) => get_state(&sh, id, m),
            Op::EditorOpen(m) => editor_open(&sh, &mut ui, m),
            Op::EditorClose(m) => editor_close(&sh, &mut ui, m),
            Op::Workgroup(m) => {
                // Route A: look the one-shot name up and send the channel hello there. A workgroup port
                // for a non-zero token arrives on the mach channel thread.
                if let (Some(name), Some(own)) = (m.bootstrap_name, own.as_ref())
                    && let Ok(s) = MachSend::look_up(&name)
                {
                    let _ = s.send_making(std::process::id() as u64, own, 1000);
                }
            }
            Op::ParamList(id, m) => param_list(&sh, id, m),
            Op::ParamSet(id, m) => param_set(&sh, id, m),
            Op::SlotEnabled(id, m) => slot_set_enabled(&sh, id, m),
            Op::Shutdown(id) => {
                close_windows(&sh, &mut ui, None);
                shutdown(&sh, id)
            }
        }
        poll_plugins(&sh);
        polled = Instant::now();
    }
    close_windows(&sh, &mut ui, None);
    summoner_sys::proc_exit::exit_now(0);
}

/// `editor.open`, main thread. A real plugin's view goes into a window of its own: step 1 the
/// view and its size (none: `editor.availability {available: false}`), step 2 the window (titled, closable, resizable
/// only if the view is, floating), step 3 setFrame and attached, then the window ordered front; the slot counts as
/// having an editor open and its childGen moves (as today). A test double's "editor" is its user edit. An editor
/// already open stays as it is.
fn editor_open(sh: &Shared, ui: &mut Ui, m: codec::EditorRef) {
    let slot = m.slot.unwrap_or(0);
    let Some((c, i)) = slot_of(sh, m.chain_key, slot) else {
        return;
    };
    let Some(p) = c.slots[i].plugin.clone() else {
        return;
    };
    let s = &c.slots[i];
    // A render set never opens an editor (the host application refuses a render's FX editor with RENDER_IN_PROGRESS):
    // no window, `editor.availability {available: false}`.
    if c.render {
        let r = Availability {
            chain_key: m.chain_key,
            slot,
            available: false,
        };
        return wire::event(&sh.w, kind::EDITOR_AVAILABILITY, &r, &[]);
    }
    // Already open: brought to the front again (de-miniaturized if it was); not a second window, and its childGen does
    // not move.
    if let Some(win) = ui.windows.get(&(m.chain_key, slot)) {
        // De-miniaturize / make key can reach the plugin's view code: the status record names it.
        sh.status(OP_EDITOR, m.chain_key, slot, &s.name);
        if let Some(ed) = &ui.ed
            && ed.front(win).is_ok()
            && ed.is_front(win)
        {
            sh.editor_refronted.fetch_add(1, Ordering::SeqCst);
        }
        sh.clear_status();
        return;
    }
    // The editor-reset guard: the state before the view is created, and the deliberate-write epoch; armed
    // below only once the window exists, and only with a state.
    // Protocol 3.4: `editor.open.guardReset` asks for it (the host application sends it for instruments only).
    let guard_asked = m.guard_reset;
    let guard = if guard_asked {
        sh.status(OP_GET_STATE, m.chain_key, slot, &s.name);
        // A snapshot with nothing to apply (a VST3 both-absent state blob) arms nothing.
        let snapshot = framed(p.as_ref()).ok().filter(|b| {
            !b.is_empty()
                && !matches!(
                    state_blob::decode(b),
                    Ok(Blob::Vst3 {
                        component: None,
                        controller: None
                    })
                )
        });
        sh.clear_status();
        snapshot.map(|snapshot| ResetGuard {
            snapshot,
            epoch: s.deliberate_epoch.load(Ordering::SeqCst),
            param_writes: s.param_writes.load(Ordering::Relaxed),
            plugin: Arc::as_ptr(&p) as *const () as usize,
            opened: Instant::now(),
            tick: 0,
            prev: None,
        })
    } else {
        None
    };
    sh.status(OP_EDITOR, m.chain_key, slot, &s.name);
    let opened = match p.editor_create() {
        Some((w, h, resizable)) => {
            let shown = open_window(ui, p.as_ref(), (w, h, resizable), &s.name);
            match shown {
                Ok(win) => {
                    ui.windows.insert((m.chain_key, slot), win);
                    arm_guard(sh, ui, (m.chain_key, slot), guard);
                    ui.opened_at
                        .insert((m.chain_key, slot), (Instant::now(), false));
                    sh.editor_opened.fetch_add(1, Ordering::SeqCst);
                    true
                }
                Err(why) => {
                    eprintln!("[summoner-host] editor.open slot {slot}: {why}");
                    p.editor_close();
                    false
                }
            }
        }
        None => match p.editor_source() {
            // AU v2: the AU's Cocoa UI, else the generic view, as the window's content.
            summoner_proc::EditorSource::AuUnit(unit) => {
                match open_au_window(ui, unit.raw(), &s.name) {
                    Ok((win, generic)) => {
                        if generic {
                            sh.editor_generic.fetch_add(1, Ordering::SeqCst);
                        }
                        ui.opened_at
                            .insert((m.chain_key, slot), (Instant::now(), false));
                        ui.windows.insert((m.chain_key, slot), win);
                        // An AU's view can reset its patch as any other's (every format is guarded).
                        arm_guard(sh, ui, (m.chain_key, slot), guard);
                        sh.editor_opened.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                    Err(why) => {
                        eprintln!("[summoner-host] editor.open slot {slot}: {why}");
                        false
                    }
                }
            }
            _ => p.open_editor(),
        },
    };
    if opened {
        s.editor_open.store(true, Ordering::SeqCst);
        s.child_gen.fetch_add(1, Ordering::SeqCst);
    } else {
        let r = Availability {
            chain_key: m.chain_key,
            slot,
            available: false,
        };
        wire::event(&sh.w, kind::EDITOR_AVAILABILITY, &r, &[]);
    }
    sh.clear_status();
    check_latency(sh, &c);
}

/// The editor dylib, loaded on the first real editor (never in scan mode), from the first of: the test hooks'
/// `--test-editor-dylib`; `libsummoner_editor.dylib` beside this executable, if it is a regular file (not a link),
/// this user's, and writable by no one else (a shipped child's place for it); the build's own path. A failure is
/// remembered: no window, `editor.availability` false, and the child goes on.
fn load_editor(ui: &mut Ui) -> Result<(), String> {
    if ui.ed.is_some() {
        return Ok(());
    }
    if let Some(why) = &ui.load_failed {
        return Err(why.clone());
    }
    let path = ui.dylib_override.clone().unwrap_or_else(editor_dylib_path);
    match Editor::load_from(&path) {
        Ok(ed) => {
            ed.set_activate(!ui.no_activate);
            ui.ed = Some(ed);
            Ok(())
        }
        Err(e) => {
            let why = format!("the editor dylib {path}: {e:?}");
            ui.load_failed = Some(why.clone());
            Err(why)
        }
    }
}

pub(crate) fn editor_dylib_path() -> String {
    use std::os::unix::fs::MetadataExt;
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("libsummoner_editor.dylib")));
    if let Some(b) = beside
        && let Ok(m) = std::fs::symlink_metadata(&b)
        && m.file_type().is_file()
        && m.uid() == summoner_sys::editor::current_uid()
        && m.mode() & 0o022 == 0
    {
        return b.to_string_lossy().into_owned();
    }
    // A shipped build (no lab root) never falls back to the build machine's path; only the dylib beside the
    // executable (tools/build-shipped.sh puts it there), else no editor (`editor.availability` false).
    if summoner_sys::bundle::shipped_build() {
        return std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("libsummoner_editor.dylib")))
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    summoner_sys::editor::DYLIB.to_string()
}

/// Steps 2-3 for a view `create`d at (w, h, resizable): the dylib (loaded on first use), the window, the resize hook,
/// the view attached, the window shown.
fn open_window(
    ui: &mut Ui,
    p: &dyn Plugin,
    (w, h, resizable): (u32, u32, bool),
    title: &str,
) -> Result<Window, String> {
    load_editor(ui)?;
    let ed = ui.ed.as_ref().expect("loaded above");
    let win = ed
        .open(f64::from(w), f64::from(h), resizable, title)
        .map_err(|e| format!("the window: {e:?}"))?;
    if let Some((hook, ctx)) = p.editor_resize_hook() {
        ed.set_hook(&win, Some(hook), ctx);
    }
    let a = EditorAttach::new(
        summoner_proc::ParentWindow::NsView(win.content_view(ed)),
        summoner_proc::HostWindow::new(
            ed.set_size_fn() as usize,
            ed.get_size_fn() as usize,
            win.handle(),
        ),
    );
    if let Err(why) = p.editor_attach(a) {
        ed.set_hook(&win, None, 0);
        let _ = ed.close(win);
        return Err(format!("attach: {why}"));
    }
    if let Err(e) = ed.show(&win) {
        // Never left open (its input monitor installed) on a failed show.
        p.editor_close();
        ed.set_hook(&win, None, 0);
        let _ = ed.close(win);
        return Err(format!("show: {e:?}"));
    }
    Ok(win)
}

/// AU v2: the window, its content the AU's own view, then shown.
fn open_au_window(ui: &mut Ui, unit: usize, title: &str) -> Result<(Window, bool), String> {
    load_editor(ui)?;
    let ed = ui.ed.as_ref().expect("loaded above");
    let (win, generic) = ed
        .open_au(unit, title)
        .map_err(|e| format!("the AU window: {e:?}"))?;
    if let Err(e) = ed.show(&win) {
        let _ = ed.close(win);
        return Err(format!("show: {e:?}"));
    }
    Ok((win, generic))
}

/// The editor-reset guard of one open window: if the plugin resets its state when its view is created, the state from
/// before the view is restored. It stands down once the user sent the window any mouse, key or scroll event, and it
/// restores at most once per open.
struct ResetGuard {
    /// The state-blob-framed state before the view was created.
    snapshot: Vec<u8>,
    /// The slot's deliberate-write epoch and host parameter writes at the arm.
    epoch: u64,
    param_writes: u64,
    /// The plugin the guard was armed for (`Arc::as_ptr`): another plugin at the slot stands it down.
    plugin: usize,
    /// When the window was on screen (the ticks count from the open, not from before the view existed).
    opened: Instant,
    /// The next tick of [`GUARD_TICKS_MS`].
    tick: usize,
    /// The state the previous tick read, if any.
    prev: Option<Vec<u8>>,
}

/// The guard's ticks after the open (100, 300, 700, 1500 ms).
const GUARD_TICKS_MS: [u64; 4] = [100, 300, 700, 1500];

/// A guard armed for an open window: its clock starts now, with the window on screen.
fn arm_guard(sh: &Shared, ui: &mut Ui, key: (u64, u32), guard: Option<ResetGuard>) {
    if let Some(mut g) = guard {
        g.opened = Instant::now();
        ui.guards.insert(key, g);
        sh.editor_guard_armed.fetch_add(1, Ordering::SeqCst);
    }
}

/// The due ticks of every guard (main loop, after the pump). At each tick, in this order: the slot gone, a
/// deliberate write since the arm, or (divergence 1) the user's input: the guard is done, nothing restored. Else the
/// state now equal to the snapshot: nothing; changed but still moving (not equal to the previous tick's) and not the
/// last tick: wait; changed and settled, or the last tick: the snapshot applied, once (divergence 2), and the guard is
/// done.
fn run_guards(sh: &Shared, ui: &mut Ui) {
    let keys: Vec<(u64, u32)> = ui.guards.keys().copied().collect();
    for k in keys {
        let Some(g) = ui.guards.get_mut(&k) else {
            continue;
        };
        if g.tick >= GUARD_TICKS_MS.len() {
            ui.guards.remove(&k);
            continue;
        }
        // The ticks whose deadline has passed; a loop that came late runs only the latest of them, so
        // two reads that would decide "settled" are never one loop turn apart.
        let elapsed = g.opened.elapsed();
        let due = GUARD_TICKS_MS
            .iter()
            .take_while(|&&ms| elapsed >= Duration::from_millis(ms))
            .count();
        if due <= g.tick {
            continue;
        }
        g.tick = due;
        let last = g.tick == GUARD_TICKS_MS.len();
        let slot =
            slot_of(sh, k.0, k.1).and_then(|(c, i)| c.slots[i].plugin.clone().map(|p| (c, i, p)));
        let Some((c, i, p)) = slot else {
            ui.guards.remove(&k);
            continue;
        };
        let s = &c.slots[i];
        // Another plugin at this slot (the guard was armed for the one it snapshotted).
        if Arc::as_ptr(&p) as *const () as usize != g.plugin {
            ui.guards.remove(&k);
            continue;
        }
        let touched = ui
            .windows
            .get(&k)
            .zip(ui.ed.as_ref())
            .is_some_and(|(w, ed)| ed.took_input(w));
        if s.deliberate_epoch.load(Ordering::SeqCst) != g.epoch
            || s.param_writes.load(Ordering::Relaxed) != g.param_writes
            || touched
        {
            ui.guards.remove(&k);
            continue;
        }
        sh.status(OP_GET_STATE, k.0, k.1, &s.name);
        let now = framed(p.as_ref()).ok();
        sh.clear_status();
        let Some(now) = now else {
            if last {
                ui.guards.remove(&k);
            }
            continue;
        };
        if now == g.snapshot {
            g.prev = Some(now);
            continue;
        }
        let settled = g.prev.as_ref() == Some(&now);
        if !settled && !last {
            g.prev = Some(now);
            continue;
        }
        let snapshot = std::mem::take(&mut g.snapshot);
        ui.guards.remove(&k);
        let r = apply_state(p.as_ref(), codec::StateMode::V1, &snapshot, &mut || {
            sh.status(OP_SET_STATE, k.0, k.1, &s.name)
        });
        if r.ran {
            s.child_gen.fetch_add(1, Ordering::SeqCst);
            sh.clear_status();
            check_latency(sh, &c);
        }
        // Counted (and told) only when the snapshot was applied.
        if r.ran && r.ok {
            sh.editor_guard_restored.fetch_add(1, Ordering::SeqCst);
            eprintln!(
                "[summoner-host] chain {} slot {}: editor-open reset detected; restored the pre-editor state ({} bytes)",
                k.0,
                k.1,
                snapshot.len()
            );
        }
        crate::diag::log(
            crate::diag::Level::Info,
            crate::diag::area::HOST,
            None,
            "editor.guard",
            vec![
                ("chain", crate::diag::Value::U64(k.0)),
                ("slot", crate::diag::Value::U64(u64::from(k.1))),
                ("bytes", crate::diag::Value::U64(snapshot.len() as u64)),
                ("ok", crate::diag::Value::Bool(r.ok)),
            ],
        );
    }
}

/// The close's view-and-window half, for one slot's window: the plugin's view removed and released, then the window
/// ordered out and freed.
fn close_window(sh: &Shared, ui: &mut Ui, key: (u64, u32), p: Option<&Arc<dyn Plugin>>) {
    ui.opened_at.remove(&key);
    ui.minimized.remove(&key);
    ui.clicked.remove(&key);
    ui.guards.remove(&key);
    if let Some(win) = ui.windows.remove(&key) {
        if let Some(p) = p {
            p.editor_close();
        }
        if let Some(ed) = &ui.ed {
            ed.set_hook(&win, None, 0);
            let _ = ed.close(win);
            sh.editor_closed.fetch_add(1, Ordering::SeqCst);
            sh.editor_on_screen
                .store(u64::from(ed.count()), Ordering::SeqCst);
        }
    }
}

/// Every window of chain `key` (or of every chain), closed without `editor.closed`: the chain is going (an unload),
/// or the child is (a shutdown, the end of the loop).
fn close_windows(sh: &Shared, ui: &mut Ui, key: Option<u64>) {
    let keys: Vec<(u64, u32)> = ui
        .windows
        .keys()
        .filter(|(k, _)| key.is_none_or(|x| x == *k))
        .copied()
        .collect();
    for (k, slot) in keys {
        let p = slot_of(sh, k, slot).and_then(|(c, i)| c.slots[i].plugin.clone());
        close_window(sh, ui, (k, slot), p.as_ref());
        if let Some((c, i)) = slot_of(sh, k, slot) {
            c.slots[i].editor_open.store(false, Ordering::SeqCst);
        }
    }
}

/// A chain's `corr` in the child's records: null (the child does not know the app's id without a protocol change);
/// the chain and slot are in `fields`.
pub fn corr(_key: u64) -> Option<String> {
    None
}

fn fail_load(sh: &Shared, key: u64, why: &str) {
    eprintln!("[summoner-host] chain {key}: {why}");
    use crate::diag::{Level, Value, area, log};
    log(
        Level::Info,
        area::HOST,
        corr(key),
        "chain.load",
        vec![
            ("chain", Value::U64(key)),
            ("ok", Value::Bool(false)),
            ("reason", Value::Str(why.into())),
        ],
    );
    let r = {
        let mut msg = codec::ChainLoaded::default();
        msg.chain_key = key;
        msg
    };
    wire::answer_event(
        &sh.w,
        kind::CHAIN_LOADED,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

/// Whether a `chain.load` is a render set. Flags 0 (a host application that writes none): the role decides.
/// Any flag set: FLAG_RENDER must equal `role == Render` and FLAG_ACK_WAKE must equal FLAG_RENDER (only render
/// sets are ack-woken, and every one is), and no other bit may be set; else the load is refused. Never an OR: a stray
/// flag must not drop a live chain to user-initiated QoS on the audio path.
fn render_mode(role: codec::Role, flags: u32) -> Result<bool, &'static str> {
    use summoner_proto::layout::{FLAG_ACK_WAKE, FLAG_RENDER};
    let render = role == codec::Role::Render;
    if flags == 0 {
        return Ok(render);
    }
    if flags & !(FLAG_RENDER | FLAG_ACK_WAKE) != 0 {
        return Err("the region's flags carry an unknown bit");
    }
    let (fr, fa) = (flags & FLAG_RENDER != 0, flags & FLAG_ACK_WAKE != 0);
    if fr != render || fa != fr {
        return Err("the region's flags disagree with the chain's role");
    }
    Ok(render)
}

fn load(sh: &Shared, m: codec::ChainLoad, fd: OwnedFd, instance_given: bool) {
    let key = m.chain_key;
    // A `chain.load` on a key that is live is refused, the live chain untouched. Replacing it
    // in place would leave its editor windows (their views on the old plugin) and reset guards keyed to the slot:
    // a use-after-free on the next pump, and another plugin's snapshot restored into the new one. The host application
    // unloads before it reloads (its make-before-break uses a new key).
    if sh.chain(key).is_some() {
        drop(fd);
        return fail_load(sh, key, "chain.load on a live chain key (unload it first)");
    }
    let Some(page) = sh.page() else {
        die(5, "chain.load before hello");
    };
    let geo = Geometry {
        max_frames: m.max_frames,
        ch_in: m.ch_in,
        ch_out: m.ch_out,
        max_midi: m.max_midi,
        max_params: m.max_params,
        tap_slot: m.tap_slot,
    };
    if geo.validate().is_err() || m.rate.is_nan() || m.rate <= 0.0 {
        return fail_load(sh, key, "chain.load names an invalid geometry");
    }
    // The region, then its doorbells' objects and this chain's FanOut endpoint (on unix the frame carries the one
    // descriptor and no `fanSlot`: slot 0, the shared hub word).
    let mut bells = Vec::new();
    let fan = match runecraft_ipc::bridge::FanOutEndpoint::from_transferables(
        m.fan_slot.unwrap_or(0),
        &mut bells,
    ) {
        Ok(f) => f,
        Err(e) => return fail_load(sh, key, &format!("fan-out endpoint: {e}")),
    };
    let region = match Region::open(crate::reader::received(fd), &mut bells, geo, key) {
        Ok(r) => r,
        Err(e) => return fail_load(sh, key, &format!("region: {e}")),
    };
    // (The descriptor closed once mapped, inside `Region::open`, where the explicit drop of before was.)
    // This process's view of the region wired in place, counted (each side wires its own mapping).
    let region_unwired = match region.wire() {
        Ok(_) => 0,
        Err(e) => {
            eprintln!(
                "[summoner-host] chain {key}: the region was not wired ({e:?}); it runs unwired"
            );
            e.bytes as u64
        }
    };
    // What this load adds to the arena and the locked regions (the message thread alone allocates them), for
    // `chain.loaded`'s wiredBytes / unwiredBytes.
    let mem0 = rt_mem();
    // Any instanceId the meta carries is checked, 0 included; none, nothing to check.
    if instance_given && m.instance_id != region.instance_id() {
        return fail_load(sh, key, "region instanceId does not match chain.load");
    }
    if m.slots.len() > MAX_SLOTS {
        return fail_load(sh, key, "more than 32 slots");
    }
    region
        .group()
        .store(m.group.min(RING_GROUPS as u32 - 1), Ordering::Release);
    // the role decides; the region's flags, once the host application writes them, must agree.
    let render = match render_mode(m.role, region.flags()) {
        Ok(r) => r,
        Err(why) => return fail_load(sh, key, why),
    };
    let mut entries = Vec::with_capacity(m.slots.len());
    for (i, d) in m.slots.iter().enumerate() {
        // A quarantined slot arrives empty (no plugin): nothing is loaded; it passes audio through and keeps its
        // number.
        let quarantined = d.plugin.is_none();
        let described = d.plugin.as_ref().and_then(|r| slots::describe(r, i));
        let name = match &d.plugin {
            None => format!("fake{i}"),
            Some(r) => slots::slot_name(r, i, described.as_ref()),
        };
        sh.status(OP_LOAD, key, i as u32, &name);
        let ctx = slots::Context {
            ch_in: geo.ch_in,
            ch_out: geo.ch_out,
        };
        let plugin = match described.map(|d| (d.make)(ctx)) {
            Some(Ok(p)) => Some(p),
            Some(Err(why)) => {
                eprintln!("[summoner-host] chain {key} slot {i} ({name}): {why}");
                None
            }
            None => {
                if !quarantined {
                    eprintln!(
                        "[summoner-host] chain {key} slot {i}: no hosting for this description in this build"
                    );
                }
                None
            }
        };
        let lie = quarantined && sh.opts.lie_quarantined;
        let (reported_latency, reported_params) = match &plugin {
            Some(p) => (p.latency(), p.param_count()),
            None if lie => (999, 77),
            None => (0, 0),
        };
        entries.push(SlotEntry {
            name,
            plugin,
            reported_latency,
            reported_params,
            // 1, as the host application's test double starts: the host application's slot starts at 0, so its first
            // generations poll reads every freshly loaded slot once (at 0 the host application would never read a fresh
            // slot until something moved).
            child_gen: AtomicU64::new(1),
            edits_seen: AtomicU64::new(0),
            editor_open: AtomicBool::new(false),
            reactivations: AtomicU64::new(0),
            bypassed: AtomicU64::new(0),
            bypassed_total: AtomicU64::new(0),
            stalls: AtomicU64::new(0),
            misaligned: AtomicU64::new(0),
            restart_pending: AtomicBool::new(false),
            host_set_waiting: AtomicBool::new(false),
            pending: Mutex::new(None),
            enabled: AtomicBool::new(initially_enabled(i, d.enabled)),
            calls: AtomicU64::new(0),
            deliberate_epoch: AtomicU64::new(0),
            param_writes: AtomicU64::new(0),
        });
    }
    sh.clear_status();
    let infos: Vec<codec::SlotInfo> = entries
        .iter()
        .map(|s| {
            codec::SlotInfo::new(
                s.plugin.is_some(),
                s.name.clone(),
                geo.ch_out,
                s.reported_latency,
                s.reported_params,
                // Protocol 3.4: a hosted slot has an editor, answered without creating a view (a VST3 with none says so at
                // `editor.open`: `editor.availability`); a failed or quarantined slot has none.
                s.plugin.is_some(),
                // Protocol 3.9: the slot's notices at load (an AU names `inputsRefused` only once prepared).
                s.plugin.as_ref().map_or_else(Vec::new, |p| p.notices()),
            )
        })
        .collect();
    let latency = entries.iter().map(|s| s.reported_latency).sum();
    let param_count = entries.iter().map(|s| s.reported_params).sum();
    let c = ChainArc::new(Chain::new(
        key,
        geo,
        m.rate,
        region,
        page,
        entries,
        sh.opts.tc_us,
        sh.opts.stats.is_some(),
        sh.opts.noalloc_blocks,
        render,
        fan,
    ));
    c.reported_latency.store(latency, Ordering::Relaxed);
    {
        use crate::diag::{Level, Value, area, hashed, log};
        log(
            Level::Info,
            area::HOST,
            corr(key),
            "chain.load",
            vec![
                ("chain", Value::U64(key)),
                ("ok", Value::Bool(true)),
                ("slots", Value::U64(c.slots.len() as u64)),
                ("render", Value::Bool(render)),
                ("latency", Value::U64(latency as u64)),
                ("params", Value::U64(param_count as u64)),
                ("rate", Value::F64(m.rate)),
                ("maxFrames", Value::U64(m.max_frames as u64)),
            ],
        );
        for (i, s) in c.slots.iter().enumerate() {
            // the plugin's name hashed (its description's path never logged).
            log(
                Level::Info,
                area::PLUGIN,
                None,
                "slot.load",
                vec![
                    ("chain", Value::U64(key)),
                    ("slot", Value::U64(i as u64)),
                    ("name", Value::Str(hashed(&s.name))),
                    ("hosted", Value::Bool(s.plugin.is_some())),
                    ("latency", Value::U64(s.reported_latency as u64)),
                    ("params", Value::U64(s.reported_params as u64)),
                ],
            );
        }
    }
    Chain::start(&c);
    // The chain's wired memory (its region, its arena blocks and locked regions, its thread's stack top) and
    // what could not be wired.
    let mem = rt_mem();
    let arena_live = mem.0.saturating_sub(mem0.0);
    let arena_unwired = mem.1.saturating_sub(mem0.1).min(arena_live);
    let wired_bytes = c.region.wired_bytes()
        + (arena_live - arena_unwired)
        + mem.2.saturating_sub(mem0.2)
        + c.stack_wired.load(Ordering::Relaxed);
    let unwired_bytes = region_unwired + arena_unwired + c.stack_unwired.load(Ordering::Relaxed);
    c.reported_wired.store(wired_bytes, Ordering::Relaxed);
    c.reported_unwired.store(unwired_bytes, Ordering::Relaxed);
    sh.chains.lock().unwrap().insert(key, c);
    let r = codec::ChainLoaded::new(
        key,
        true,
        infos,
        latency,
        param_count,
        wired_bytes,
        unwired_bytes,
    );
    wire::answer_event(
        &sh.w,
        kind::CHAIN_LOADED,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

/// This child's arena blocks (live, and live on unwired chunks) and its locked regions' wired bytes, now.
fn rt_mem() -> (u64, u64, u64) {
    let a = runecraft_rt::arena::RtArena::global().stats();
    (
        a.live_by_owner[crate::chain::RT_OWNER as usize],
        a.unwired_bytes,
        // Locked regions only: wired ranges (the region, the stack) are counted on their own.
        (runecraft_rt::os::LockedRegion::live_wired_bytes() - runecraft_rt::os::wired_range_bytes())
            as u64,
    )
}

/// After an op that ran plugin code: a hosted slot whose latency moved changes the chain's PDC, so the host application
/// hears `latency.changed` with the new sum (the response line no longer carries a latency).
fn check_latency(sh: &Shared, c: &Chain) {
    let now = c.latency();
    if c.reported_latency.swap(now, Ordering::Relaxed) != now {
        let r = codec::LatencyChanged::new(c.key, now);
        wire::event(&sh.w, kind::LATENCY_CHANGED, &r, &[]);
    }
}

/// Stop the chain's thread, then drop its processors and plugins on this thread under the status record. `None`:
/// the thread is wedged and the chain must be kept (abandoned).
fn release(sh: &Shared, c: &ChainArc) -> Option<Vec<u64>> {
    let stats = c.stop_and_join(500)?;
    let mut p = c.procs.lock();
    // The processors are dropped here, on this thread: the message thread's clone of the cells first, then the
    // gate's, so the last reference goes under the status record.
    *c.cells.lock().unwrap() = None;
    if let Some(cells) = p.cells.take() {
        for (i, s) in c.slots.iter().enumerate() {
            if s.plugin.is_some() {
                sh.status(OP_UNLOAD, c.key, i as u32, &s.name);
            }
        }
        drop(cells);
    }
    p.lines = runecraft_rt::arena::ArenaSlice::from_fn(crate::chain::RT_OWNER, 0, |_| None);
    p.sides = runecraft_rt::arena::ArenaSlice::from_fn(crate::chain::RT_OWNER, 0, |_| {
        handover::ChainSide::default()
    });
    p.ios = runecraft_rt::arena::ArenaSlice::from_fn(crate::chain::RT_OWNER, 0, |_| {
        summoner_proc::Io::STEREO
    });
    p.prepared = false;
    sh.clear_status();
    Some(stats)
}

/// The slots' processors released and made again on their kept plugins at the chain's rate, `maxFrames` and offline
/// flag (a VST3 processor releases on drop; the plugin instance, and any first-use state it keeps, stays). `prepare`'s
/// body, shared with `chain.prepareFresh`.
fn rebuild(sh: &Shared, key: u64, c: &ChainArc) {
    let mut p = c.procs.lock();
    // Release the old processors before the new prepare (a VST3 one releases on drop): both clones of the cells.
    p.cells = None;
    *c.cells.lock().unwrap() = None;
    let mf = c.geo.max_frames as usize;
    let nbuf = 2.max(c.geo.ch_in).max(c.geo.ch_out) as usize;
    let mut cells = Vec::with_capacity(c.slots.len());
    let mut lines = Vec::with_capacity(c.slots.len());
    let mut ios = Vec::with_capacity(c.slots.len());
    for (i, s) in c.slots.iter().enumerate() {
        let d = s.plugin.as_ref().map(|pl| {
            sh.status(OP_PREPARE, key, i as u32, &s.name);
            pl.prepare_mode(c.rate, mf, c.render)
        });
        // The processor's buses, read once here (fixed from its prepare on), for the chain thread's
        // channel rule.
        ios.push(d.as_ref().map_or(summoner_proc::Io::STEREO, |d| d.io()));
        // a hosted effect slot's bypass delay line, C = max(L at prepare, 16,384); an instrument (slot 0 of a
        // chain without inputs) bypasses to silence.
        let effect = !(i == 0 && c.geo.ch_in == 0);
        lines.push(
            s.plugin
                .as_ref()
                .filter(|_| effect)
                .map(|pl| handover::DelayLine::new(nbuf, pl.latency() as usize, mf)),
        );
        *s.pending.lock().unwrap() = None;
        s.restart_pending.store(false, Ordering::SeqCst);
        cells.push(SlotCell::new(d));
    }
    // The cells, the lines' table and the sides in the wired arena (message thread).
    let cells = {
        let n = cells.len();
        let mut it = cells.into_iter();
        runecraft_rt::arena::ArenaArc::from_fn(crate::chain::RT_OWNER, n, |_| {
            it.next().expect("n cells")
        })
    };
    p.sides =
        runecraft_rt::arena::ArenaSlice::from_fn(crate::chain::RT_OWNER, c.slots.len(), |_| {
            handover::ChainSide::default()
        });
    p.lines = {
        let n = lines.len();
        let mut it = lines.into_iter();
        runecraft_rt::arena::ArenaSlice::from_fn(crate::chain::RT_OWNER, n, |_| {
            it.next().expect("n lines")
        })
    };
    p.ios = runecraft_rt::arena::ArenaSlice::from_slice(crate::chain::RT_OWNER, &ios);
    // The inventory (debug builds): the three tables the chain thread reads per block, checked as `Chain::start`
    // checks its working set.
    #[cfg(debug_assertions)]
    {
        let arena = runecraft_rt::arena::RtArena::global();
        for (ptr, len) in [
            (cells.as_ptr().cast::<u8>(), std::mem::size_of_val(&*cells)),
            (p.sides.as_ptr().cast(), std::mem::size_of_val(&*p.sides)),
            (p.lines.as_ptr().cast(), std::mem::size_of_val(&*p.lines)),
            (p.ios.as_ptr().cast(), std::mem::size_of_val(&*p.ios)),
        ] {
            if len == 0 {
                continue;
            }
            c.rt_checked.fetch_add(1, Ordering::Relaxed);
            if arena.contains(ptr, len) != Some(true) {
                c.rt_unwired.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    p.cells = Some(cells.clone());
    *c.cells.lock().unwrap() = Some(cells);
    sh.clear_status();
    p.prepared = true;
}

/// Protocol 3.8: `chain.prepareFresh` on a render set (the processors released and prepared again under the process
/// lock, answered after it ran): every slot's processor released and made again on the kept plugin ([`rebuild`]), then
/// `latency.changed` if a slot's latency moved, then `chain.preparedFresh {chainKey, ok, ms, latency}`. The host
/// application sends it only between a render's passes, so no block is in flight. A live chain, an unknown key or a
/// chain not yet prepared is refused (`ok: false`, `error`), nothing changed. `--cfg
/// mutant_prepare_fresh_keeps_processor` (never in a normal build): answered ok, nothing rebuilt (the warm pass's tails
/// stay).
fn prepare_fresh(sh: &Shared, key: u64) {
    let t0 = Instant::now();
    let c = sh.chain(key);
    let mut r = {
        let mut msg = codec::ChainPreparedFresh::default();
        msg.chain_key = key;
        msg
    };
    match &c {
        None => r.error = Some("unknown chain".into()),
        Some(c) if !c.render => r.error = Some("not a render set".into()),
        Some(c) if !c.procs.lock().prepared => r.error = Some("not prepared".into()),
        Some(c) => {
            if !cfg!(mutant_prepare_fresh_keeps_processor) {
                rebuild(sh, key, c);
            }
            check_latency(sh, c);
            r.ok = true;
            r.latency = c.latency();
            if !cfg!(mutant_fresh_notices_omitted) {
                r.slot_notices = Some(slot_notices(c));
            }
        }
    }
    r.ms = t0.elapsed().as_secs_f64() * 1000.0;
    wire::answer_event(
        &sh.w,
        kind::CHAIN_PREPARED_FRESH,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

/// Complete snapshot of the plugins whose processors were just rebuilt, on the message thread.
fn slot_notices(c: &ChainArc) -> Vec<Vec<String>> {
    let mut notices: Vec<Vec<String>> = c
        .slots
        .iter()
        .map(|s| s.plugin.as_ref().map_or_else(Vec::new, |p| p.notices()))
        .collect();
    if notices.iter().all(Vec::is_empty) {
        notices.clear();
    }
    notices
}

fn prepare(sh: &Shared, key: u64) {
    let c = sh.chain(key);
    // The chain's memory moves at a prepare (its processors and the bypass delay lines rebuilt): measured across it,
    // as `chain.loaded`'s arena share is across the load (the message thread alone allocates in this owner).
    let mem0 = rt_mem();
    if let Some(c) = &c {
        use crate::diag::{Level, Value, area, log};
        log(
            Level::Info,
            area::HOST,
            corr(key),
            "chain.prepare",
            vec![
                ("chain", Value::U64(key)),
                ("rate", Value::F64(c.rate)),
                ("maxFrames", Value::U64(c.geo.max_frames as u64)),
                ("offline", Value::Bool(c.render)),
            ],
        );
    }
    if let Some(c) = &c {
        rebuild(sh, key, c);
    }
    // The latency the prepare gave (a plugin sizes its buffers there: CHOWTapeModel reads 37 at load, 39 once prepared
    // at 48 kHz), to the host application before chain.prepared, so ChainReady carries the prepared latency, not the
    // load-time sum until the next poll.
    if let Some(c) = &c {
        check_latency(sh, c);
    }
    // The chain's pair after the prepare, replacing the one the host application holds: the last reported one plus the
    // change across the prepare (arena blocks freed and made, locked regions), never below 0.
    let pair = c.as_ref().map(|c| {
        let mem = rt_mem();
        let d = |a: u64, b: u64| i128::from(a) - i128::from(b);
        let d_unwired = d(mem.1, mem0.1);
        let d_wired = d(mem.0, mem0.0) - d_unwired + d(mem.2, mem0.2);
        let clamp = |v: i128| u64::try_from(v.max(0)).unwrap_or(u64::MAX);
        let w = clamp(i128::from(c.reported_wired.load(Ordering::Relaxed)) + d_wired);
        let u = clamp(i128::from(c.reported_unwired.load(Ordering::Relaxed)) + d_unwired);
        c.reported_wired.store(w, Ordering::Relaxed);
        c.reported_unwired.store(u, Ordering::Relaxed);
        (w, u)
    });
    // Protocol 3.9: every slot's whole notice set after the prepare (an AU instrument that refused its inputs), sent
    // only when a slot has one, so a chain without any writes 3.8's bytes.
    let slot_notices = c.as_ref().map_or_else(Vec::new, slot_notices);
    // The chain is ready on chain.prepared: sent after the state.sets queued before it were handled.
    let r = codec::ChainPrepared::new(
        key,
        c.is_some(),
        pair.map(|p| p.0),
        pair.map(|p| p.1),
        slot_notices,
    );
    wire::answer_event(
        &sh.w,
        kind::CHAIN_PREPARED,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

/// What a `state.set` asks of the slot, decided before any plugin code runs: the both-absent vst3
/// no-op, or the chunks to apply.
enum Wanted<'a> {
    Nothing,
    Chunks(Option<&'a [u8]>, Option<&'a [u8]>),
}

/// The child's frame check: only `mode: sasp`, only a blob the state blob's one form accepts, and only the slot's own
/// format. `Err`: refused, and nothing reaches the plugin (`state.applied {ok: false}`). The
/// both-absent vst3 blob is accepted before the format is compared.
fn wanted<'a>(mode: codec::StateMode, blob: &'a [u8], k: StateKind) -> Result<Wanted<'a>, String> {
    if mode != codec::StateMode::V1 {
        return Err(format!("mode {mode:?}: only sasp is applied"));
    }
    let b = state_blob::decode(blob).map_err(|r| r.to_string())?;
    if b.both_absent() {
        return Ok(Wanted::Nothing);
    }
    match (b, k) {
        (Blob::Builtin { body }, StateKind::Builtin) => Ok(Wanted::Chunks(Some(body), None)),
        (
            Blob::Vst3 {
                component,
                controller,
            },
            StateKind::Vst3,
        ) => Ok(Wanted::Chunks(component, controller)),
        (Blob::Au { plist }, StateKind::Au) => Ok(Wanted::Chunks(Some(plist), None)),
        (b, k) => Err(format!(
            "a {} state blob for a {k:?} slot",
            b.format().name()
        )),
    }
}

/// A slot's state as the state blob it answers with (`state.get`, `editor.closed`).
/// Why a slot's state could not be framed: the text, and with `too_large` the framed size the state blob measured (over
/// `MAX_BLOB`; protocol 3.3's `state.get` tooLarge reports it).
#[derive(Debug)]
struct NotFramed {
    why: String,
    too_large: Option<u64>,
}

impl std::fmt::Display for NotFramed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

fn framed(p: &dyn Plugin) -> Result<Vec<u8>, NotFramed> {
    // The test double's BAREGET (the state.set refusal test): the bare body, which the host application must refuse.
    // Test builds only.
    #[cfg(feature = "test-hooks")]
    if p.test_hooks().is_some_and(|t| t.answers_bare()) {
        return Ok(p.get_state());
    }
    let c: Chunks = p.get_state_chunks().map_err(|why| NotFramed {
        why: why.into(),
        too_large: None,
    })?;
    let out = match p.state_kind() {
        StateKind::Builtin => state_blob::encode(&Blob::Builtin {
            body: c.first.as_deref().unwrap_or_default(),
        }),
        StateKind::Vst3 => state_blob::encode(&Blob::Vst3 {
            component: c.first.as_deref(),
            controller: c.second.as_deref(),
        }),
        StateKind::Au => state_blob::encode(&Blob::Au {
            plist: c.first.as_deref().unwrap_or_default(),
        }),
        // A state kind a later slot names: this host has no frame for it.
        _ => {
            return Err(NotFramed {
                why: "a state kind this host does not frame".to_string(),
                too_large: None,
            });
        }
    };
    out.map_err(|r| match r {
        state_blob::Refusal::TooLarge(n) => NotFramed {
            why: "state larger than 64 MiB".to_string(),
            too_large: Some(n as u64),
        },
        r => NotFramed {
            why: r.to_string(),
            too_large: None,
        },
    })
}

/// What one `state.set` did to a slot: the receipt's `ok` and `applied`, whether plugin code ran, and whether it
/// was the counted both-absent no-op.
#[derive(Debug, Default, PartialEq)]
struct Receipt {
    ok: bool,
    applied: Option<codec::Applied>,
    ran: bool,
    empty: bool,
}

/// The state-set decision for one slot, apart from the wire: the frame check first (nothing reaches the plugin unless
/// the mode is `sasp`, the blob is the state blob's one form and of the slot's format), then the chunks. `before` runs
/// once, just before plugin code (the status record).
fn apply_state(
    p: &dyn Plugin,
    mode: codec::StateMode,
    blob: &[u8],
    before: &mut dyn FnMut(),
) -> Receipt {
    let mut r = Receipt::default();
    match wanted(mode, blob, p.state_kind()) {
        Err(why) => eprintln!("[summoner-host] state.set refused, nothing applied: {why}"),
        Ok(Wanted::Nothing) => {
            r.ok = true; // the both-absent vst3 blob: a counted no-op (the host application counts it too)
            r.empty = true;
        }
        Ok(Wanted::Chunks(first, second)) => {
            before();
            r.ran = true;
            match p.set_state_chunks(first, second) {
                SetOutcome::Applied => r.ok = true,
                SetOutcome::Refused(_) => {} // ok false: nothing (more) reached the plugin
                SetOutcome::FirstOnly(why) => {
                    // N1: the component applied, then the controller refused; the plugin changed and cannot be rolled
                    // back, so the receipt says which part applied (ok false, exactly one true).
                    eprintln!("[summoner-host] a partial vst3 set: the component applied, {why}");
                    r.applied = Some(codec::Applied::new(true, false));
                }
                // An outcome a later slot names: ok false, nothing reported applied.
                _ => {}
            }
        }
    }
    r
}

fn set_state(sh: &Shared, m: codec::StateSet, blob: &[u8]) {
    // the blob's size only, never its bytes.
    crate::diag::log(
        crate::diag::Level::Debug,
        crate::diag::area::HOST,
        None,
        "state.set",
        vec![
            ("chain", crate::diag::Value::U64(m.chain_key)),
            ("slot", crate::diag::Value::U64(m.slot as u64)),
            ("bytes", crate::diag::Value::U64(blob.len() as u64)),
        ],
    );
    let mut r = Receipt::default();
    // A deliberate write (the editor-reset guard keeps it: a host set moves the deliberate-write epoch).
    // Protocol 3.3: the slot's own generation once the set took effect, the host application's capture baseline
    // (absent for a slot that does not exist).
    let mut child_generation = None;
    if let Some((c, i)) = slot_of(sh, m.chain_key, m.slot) {
        c.slots[i].deliberate_epoch.fetch_add(1, Ordering::SeqCst);
        child_generation = Some(c.slots[i].child_gen.load(Ordering::SeqCst));
    }
    if let Some((c, i)) = slot_of(sh, m.chain_key, m.slot)
        && let Some(p) = &c.slots[i].plugin
    {
        let s = &c.slots[i];
        r = apply_state(p.as_ref(), m.mode, blob, &mut || {
            sh.status(OP_SET_STATE, m.chain_key, m.slot, &s.name)
        });
        if r.empty {
            sh.empty_sets.fetch_add(1, Ordering::Relaxed);
        }
        if r.ran {
            child_generation = Some(s.child_gen.fetch_add(1, Ordering::SeqCst) + 1);
            sh.clear_status();
            check_latency(sh, &c);
        }
    }
    let reply = codec::StateApplied::new(
        m.chain_key,
        m.slot,
        m.generation,
        r.ok,
        r.applied,
        child_generation,
    );
    wire::answer_event(
        &sh.w,
        kind::STATE_APPLIED,
        &reply,
        &[],
        wire::op_corr().as_deref(),
    );
}

fn get_state(sh: &Shared, id: u64, m: codec::StateGet) {
    crate::diag::log(
        crate::diag::Level::Debug,
        crate::diag::area::HOST,
        None,
        "state.get",
        vec![
            ("chain", crate::diag::Value::U64(m.chain_key)),
            ("slot", crate::diag::Value::U64(m.slot as u64)),
        ],
    );
    let Some((c, i)) = slot_of(sh, m.chain_key, m.slot) else {
        return wire::answer_error(
            &sh.w,
            kind::STATE_GET,
            id,
            -32602,
            "no such slot",
            wire::op_corr().as_deref(),
        );
    };
    let s = &c.slots[i];
    let Some(p) = &s.plugin else {
        return wire::answer_error(
            &sh.w,
            kind::STATE_GET,
            id,
            -32602,
            "no such slot",
            wire::op_corr().as_deref(),
        );
    };
    sh.status(OP_GET_STATE, m.chain_key, m.slot, &s.name);
    let blob = framed(p.as_ref());
    sh.clear_status();
    let child_gen = s.child_gen.load(Ordering::SeqCst);
    // The answer is a state blob; no state is the error reply. Protocol 3.3: a state over
    // MAX_BLOB can never be framed, so it is measured whole here and answered `{ok: false, reason: "tooLarge", bytes}`
    // (the framed size) with no blob; nothing of it is sent.
    // The framed size over the cap: the state blob's own measure, or (a test build's bare body) the body's length.
    let over = match &blob {
        Ok(b) if b.len() > MAX_BLOB => Some(b.len() as u64),
        Err(NotFramed {
            too_large: Some(n), ..
        }) => Some(*n),
        _ => None,
    };
    let blob = match blob {
        _ if over.is_some() => {
            let bytes = over.unwrap_or_default();
            let r = codec::StateGetReply::new(
                m.chain_key,
                m.slot,
                child_gen,
                false,
                Some(codec::StateGetReply::TOO_LARGE.into()),
                Some(bytes),
            );
            crate::diag::log(
                crate::diag::Level::Warn,
                crate::diag::area::HOST,
                None,
                "state.tooLarge",
                vec![
                    ("chain", crate::diag::Value::U64(m.chain_key)),
                    ("slot", crate::diag::Value::U64(u64::from(m.slot))),
                    ("bytes", crate::diag::Value::U64(bytes)),
                ],
            );
            return wire::answer_reply(
                &sh.w,
                kind::STATE_GET,
                id,
                &r,
                &[],
                wire::op_corr().as_deref(),
            );
        }
        Ok(b) => b,
        Err(why) => {
            return wire::answer_error(
                &sh.w,
                kind::STATE_GET,
                id,
                -32603,
                &format!("no state blob: {why}"),
                wire::op_corr().as_deref(),
            );
        }
    };
    let r = {
        let mut msg = codec::StateGetReply::default();
        msg.chain_key = m.chain_key;
        msg.slot = m.slot;
        msg.child_gen = child_gen;
        msg
    };
    wire::answer_reply(
        &sh.w,
        kind::STATE_GET,
        id,
        &r,
        &blob,
        wire::op_corr().as_deref(),
    );
}

/// `editor.close` (one slot, or every slot without one): each open editor's state goes up with `editor.closed`, the
/// authoritative state.
fn editor_close(sh: &Shared, ui: &mut Ui, m: codec::EditorRef) {
    let Some(c) = sh.chain(m.chain_key) else {
        return;
    };
    for (i, s) in c.slots.iter().enumerate() {
        let Some(p) = &s.plugin else { continue };
        if m.slot.is_some_and(|x| x as usize != i) || !s.editor_open.swap(false, Ordering::SeqCst) {
            continue;
        }
        sh.status(OP_EDITOR, m.chain_key, i as u32, &s.name);
        // The close: the view removed and released, the window closed; then the authoritative state.
        close_window(sh, ui, (m.chain_key, i as u32), Some(p));
        let blob = framed(p.as_ref());
        sh.clear_status();
        // A state blob, as state.get answers. Every window the child closes is
        // reported, so a state that cannot be framed (unreadable, or over MAX_BLOB) still sends `editor.closed`, with
        // no blob (the host application refuses an empty read and keeps the stored state).
        let blob = match blob {
            Ok(b) if b.len() <= MAX_BLOB => b,
            Ok(b) => {
                eprintln!(
                    "[summoner-host] editor.closed slot {i}: state of {} bytes, over MAX_BLOB: sent without it",
                    b.len()
                );
                Vec::new()
            }
            Err(why) => {
                eprintln!("[summoner-host] editor.closed slot {i}: {why}: sent without the state");
                Vec::new()
            }
        };
        let r = codec::EditorClosed::new(m.chain_key, i as u32, s.child_gen.load(Ordering::SeqCst));
        // This one answers `editor.close` (a close the user makes is sent elsewhere, bare).
        wire::answer_event(
            &sh.w,
            kind::EDITOR_CLOSED,
            &r,
            &blob,
            wire::op_corr().as_deref(),
        );
        sh.editor_closed_sent.fetch_add(1, Ordering::SeqCst);
    }
}

fn param_list(sh: &Shared, id: u64, m: codec::ParamList) {
    let plugin = slot_of(sh, m.chain_key, m.slot).and_then(|(c, i)| c.slots[i].plugin.clone());
    let total = plugin.as_ref().map_or(0, |p| p.param_count());
    let end = (u64::from(m.offset) + u64::from(m.count.min(256))).min(u64::from(total)) as u32;
    // Protocol 3.7: every entry with its 3.7 fields, names cut at 1024 characters, the page within
    // 48 KiB of meta (the host application pages by the entries it receives).
    let params = match &plugin {
        Some(p) => (m.offset..end)
            .map(|i| {
                let meta = p.param_meta(i);
                {
                    let mut msg = codec::ParamEntry::default();
                    msg.id = m.slot << 24 | i;
                    msg.name = summoner_plugin::params::cut_chars(
                        p.param_name(i),
                        summoner_plugin::params::NAME_CHARS,
                    );
                    msg.plugin_id = meta.plugin_id;
                    msg.automatable = meta.automatable;
                    msg.default = meta.default;
                    msg.value = p.param_value(i);
                    msg.step_count = meta.step_count;
                    msg.label = meta.label;
                    msg.order = meta.order;
                    msg
                }
            })
            .collect(),
        None => Vec::new(),
    };
    let params = codec::page_within(params, summoner_plugin::params::PAGE_BYTES);
    let r = codec::ParamListReply::new(m.chain_key, m.slot, total, params);
    wire::answer_reply(
        &sh.w,
        kind::PARAM_LIST,
        id,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

/// `param.set` (protocol 3.7, kind 35, MainThread): the plugin's parameter set by its own id, then the slot's childGen
/// moves (the value is the plugin's state now: the controller's or the unit's at once; a VST3 processor's at its next
/// block, when the poll moves it again), then the reply with the read-back. A refusal (no such chain, slot or
/// parameter, a quarantined slot, a non-finite value) is an error reply, `-32602`.
fn param_set(sh: &Shared, id: u64, m: codec::ParamSet) {
    let refuse = |why: &str| {
        wire::error(
            &sh.w,
            kind::PARAM_SET,
            id,
            -32602,
            &format!("param.set: {why}"),
        );
    };
    if !m.value.is_finite() {
        return refuse("the value is not finite");
    }
    let Some((c, i)) = slot_of(sh, m.chain_key, m.slot) else {
        return refuse("no such chain or slot");
    };
    let s = &c.slots[i];
    let Some(p) = &s.plugin else {
        return refuse("the slot hosts no plugin");
    };
    let v = m.value.clamp(0.0, 1.0);
    // `--cfg mutant_param_gen_before_apply` (never in a normal build): no second move once the processor took
    // it; host/tests/param_set.rs's VST3 test fails.
    match p.host_set(m.plugin_id, v) {
        Ok(back) => {
            s.deliberate_epoch.fetch_add(1, Ordering::SeqCst);
            s.child_gen.fetch_add(1, Ordering::SeqCst);
            if p.host_set_pending() && !cfg!(mutant_param_gen_before_apply) {
                s.host_set_waiting.store(true, Ordering::SeqCst);
            }
            let r = codec::ParamSetReply::new(m.chain_key, m.slot, m.plugin_id, back);
            wire::answer_reply(
                &sh.w,
                kind::PARAM_SET,
                id,
                &r,
                &[],
                wire::op_corr().as_deref(),
            );
        }
        Err(why) => refuse(why.as_str()),
    }
}

/// Whether slot `i` starts enabled: `chain.load`'s `SlotDesc.enabled` (protocol 3.2; absent = true), unless a
/// test's `--test-slot-enabled <i>:<on|off>@0` says otherwise.
fn initially_enabled(i: usize, wire: bool) -> bool {
    #[cfg(feature = "test-hooks")]
    if let Some(t) = crate::chain::TEST_SLOT_ENABLED.get() {
        let mut on = wire;
        for &(slot, state, at) in t {
            if at == 0 && slot as usize == i {
                on = state;
            }
        }
        return on;
    }
    let _ = i;
    wire
}

/// `slot.setEnabled` (protocol 3.2, kind 34, MainThread): the slot's flag stored (the chain thread
/// fades at its next block), then the reply, once applied. `ok: false` for no such chain or slot (no error frame); a
/// quarantined or unhosted slot: `ok: true`, the flag stored, nothing changes (it passes its audio through already).
fn slot_set_enabled(sh: &Shared, id: u64, m: codec::SlotEnabled) {
    let ok = match slot_of(sh, m.chain_key, m.slot) {
        Some((c, i)) => {
            c.slots[i].enabled.store(m.enabled, Ordering::Release);
            true
        }
        None => false,
    };
    crate::diag::log(
        crate::diag::Level::Info,
        crate::diag::area::HOST,
        None,
        "slot.enabled",
        vec![
            ("chain", crate::diag::Value::U64(m.chain_key)),
            ("slot", crate::diag::Value::U64(u64::from(m.slot))),
            ("enabled", crate::diag::Value::Bool(m.enabled)),
            ("ok", crate::diag::Value::Bool(ok)),
        ],
    );
    let r = codec::SlotEnabledReply::new(m.chain_key, m.slot, m.enabled, ok);
    wire::answer_reply(
        &sh.w,
        kind::SLOT_SET_ENABLED,
        id,
        &r,
        &[],
        wire::op_corr().as_deref(),
    );
}

fn shutdown(sh: &Shared, id: u64) -> ! {
    let all = std::mem::take(&mut *sh.chains.lock().unwrap());
    let mut text = String::new();
    let mut counters = String::new();
    // Before any chain is released: what the live chains hold.
    {
        // The wired arena's accounting (the inventory test reads it).
        let a = runecraft_rt::arena::RtArena::global().stats();
        counters.push_str(&format!(
            "rt_arena wired_bytes {} live_bytes {} unwired_bytes {} orphaned {} lock_refused {} chunks {} large {} host_live {}\n",
            a.wired_bytes,
            a.live_bytes,
            a.unwired_bytes,
            a.orphaned,
            a.lock_refused,
            a.chunks,
            a.large_blocks,
            a.live_by_owner[crate::chain::RT_OWNER as usize]
        ));
        // The wired regions outside the arena (a VST3 shim's placed process state, the store-like buffers).
        counters.push_str(&format!(
            "locked_regions mapped_bytes {} wired_bytes {} ranges_wired_bytes {}\n",
            runecraft_rt::os::LockedRegion::live_mapped_bytes(),
            runecraft_rt::os::LockedRegion::live_wired_bytes()
                - runecraft_rt::os::wired_range_bytes(),
            runecraft_rt::os::wired_range_bytes()
        ));
        // The child page.
        counters.push_str(&format!(
            "child_page wired_bytes {} unwired_bytes {}\n",
            sh.page().map_or(0, |p| p.wired_bytes()),
            sh.page_unwired.load(Ordering::Relaxed)
        ));
        // Corr keys ignored, log.set applied and refused.
        counters.push_str(&format!(
            "diag corr_ignored {} log_set_applied {} log_set_refused {}\n",
            sh.corr_ignored.load(Ordering::Relaxed),
            sh.log_set_applied.load(Ordering::Relaxed),
            sh.log_set_refused.load(Ordering::Relaxed)
        ));
    }

    for (key, c) in &all {
        for (i, s) in c.slots.iter().enumerate() {
            counters.push_str(&format!(
                "chain {key} slot {i} reactivations {} bypassed {} stalls {} dropped_params {} misaligned {} calls {}\n",
                s.reactivations.load(Ordering::Relaxed),
                s.bypassed_total.load(Ordering::Relaxed),
                s.stalls.load(Ordering::Relaxed),
                s.plugin.as_ref().map_or(0, |p| p.dropped_params()),
                s.misaligned.load(Ordering::Relaxed),
                s.calls.load(Ordering::Relaxed)
            ));
        }
        counters.push_str(&format!(
            "chain {key} rt_scratch checked {} unwired {}\n",
            c.rt_checked.load(Ordering::Relaxed),
            c.rt_unwired.load(Ordering::Relaxed)
        ));
        if sh.opts.noalloc_blocks > 0 {
            counters.push_str(&format!(
                "chain {key} noalloc_blocks {} allocations {}\n",
                c.noalloc_counted.load(Ordering::Relaxed),
                c.noalloc_allocs.load(Ordering::Relaxed)
            ));
        }
        // A wedged chain is left as it is: the process exits under it.
        let released = release(sh, c);
        // The region (this side's view) and the chain thread's stack top (its depth is known once the
        // thread has ended).
        counters.push_str(&format!(
            "chain {key} wired region_bytes {} stack_bytes {} stack_unwired_bytes {} stack_used {}\n",
            c.region.wired_bytes(),
            c.stack_wired.load(Ordering::Relaxed),
            c.stack_unwired.load(Ordering::Relaxed),
            c.stack_used.load(Ordering::Relaxed)
        ));
        if let Some(v) = released {
            for p in v.as_chunks::<2>().0 {
                text.push_str(&format!(
                    "{key} {} {}\n",
                    p[0],
                    runecraft_ipc::bridge::ticks_to_ns(p[1])
                ));
            }
        }
    }
    if let Some(path) = &sh.opts.stats {
        let _ = std::fs::write(path, text);
    }
    counters.push_str(&format!(
        "state_empty_sets {}\n",
        sh.empty_sets.load(Ordering::Relaxed)
    ));
    counters.push_str(&format!(
        "foreign_images {}\n",
        summoner_sys::bundle::foreign_images()
    ));
    counters.push_str(&format!(
        "editor_windows opened {} closed {} on_screen {} generic {}\n",
        sh.editor_opened.load(Ordering::SeqCst),
        sh.editor_closed.load(Ordering::SeqCst),
        sh.editor_on_screen.load(Ordering::SeqCst),
        sh.editor_generic.load(Ordering::SeqCst)
    ));
    counters.push_str(&format!(
        "editor_closed sent {}\n",
        sh.editor_closed_sent.load(Ordering::SeqCst)
    ));
    counters.push_str(&format!(
        "editor_guard restored {} armed {}\n",
        sh.editor_guard_restored.load(Ordering::SeqCst),
        sh.editor_guard_armed.load(Ordering::SeqCst)
    ));
    counters.push_str(&format!(
        "editor_windows refronted {} minimized {}\n",
        sh.editor_refronted.load(Ordering::SeqCst),
        sh.editor_minimized.load(Ordering::SeqCst)
    ));
    if let Some(path) = &sh.opts.counters {
        let _ = std::fs::write(path, counters);
    }
    wire::reply(&sh.w, kind::SHUTDOWN, id, &empty(), &[]);
    // `_exit`: the files above are written and closed, the reply is
    // written synchronously; no plugin's static destructors run.
    summoner_sys::proc_exit::exit_now(0);
}

#[cfg(test)]
mod render_mode_tests {
    use super::render_mode;
    use summoner_proto::codec::Role;
    use summoner_proto::layout::{FLAG_ACK_WAKE, FLAG_RENDER};

    /// The role decides while the host application writes flags 0; once it writes them they must agree with it, and an
    /// unknown bit is refused. Never an OR.
    #[test]
    fn the_role_decides_and_the_flags_must_agree() {
        let both = FLAG_RENDER | FLAG_ACK_WAKE;
        assert_eq!(render_mode(Role::Live, 0), Ok(false));
        assert_eq!(render_mode(Role::Render, 0), Ok(true));
        assert_eq!(render_mode(Role::Render, both), Ok(true));
        assert!(
            render_mode(Role::Live, both).is_err(),
            "a stray render flag on a live chain"
        );
        assert!(render_mode(Role::Live, FLAG_RENDER).is_err());
        assert!(
            render_mode(Role::Render, FLAG_RENDER).is_err(),
            "render without ack wake"
        );
        assert!(
            render_mode(Role::Render, FLAG_ACK_WAKE).is_err(),
            "ack wake without render"
        );
        assert!(render_mode(Role::Live, FLAG_ACK_WAKE).is_err());
        assert!(
            render_mode(Role::Render, both | 1 << 7).is_err(),
            "an unknown bit"
        );
        assert!(render_mode(Role::Live, 1 << 2).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use summoner_proc::{Dsp, Notice};

    /// A slot of `kind` that counts every call that reaches it, and refuses a controller chunk "nope".
    struct Counting {
        kind: StateKind,
        calls: AtomicU32,
    }

    impl Plugin for Counting {
        fn name(&self) -> &str {
            "counting"
        }
        fn latency(&self) -> u32 {
            0
        }
        fn param_count(&self) -> u32 {
            0
        }
        fn param_name(&self, _: u32) -> String {
            String::new()
        }
        fn set_param(&self, _: u32, _: f32) {}
        fn set_state(&self, _: &[u8]) -> Result<(), summoner_proc::Error> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn get_state(&self) -> Vec<u8> {
            Vec::new()
        }
        fn state_kind(&self) -> StateKind {
            self.kind
        }
        fn set_state_chunks(&self, first: Option<&[u8]>, second: Option<&[u8]>) -> SetOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match (first, second) {
                (Some(_), Some(b"nope")) => SetOutcome::FirstOnly("controller refused".into()),
                _ => SetOutcome::Applied,
            }
        }
        fn poll(&self) -> Notice {
            Notice::None
        }
        fn prepare(&self, _: f64, _: usize) -> Box<dyn Dsp> {
            unimplemented!()
        }
    }

    fn slot(kind: StateKind) -> Counting {
        Counting {
            kind,
            calls: AtomicU32::new(0),
        }
    }

    fn apply(p: &Counting, mode: codec::StateMode, blob: &[u8]) -> Receipt {
        let mut before = 0;
        let r = apply_state(p, mode, blob, &mut || before += 1);
        assert_eq!(
            before,
            u32::from(r.ran),
            "the status record exactly when plugin code runs"
        );
        r
    }

    fn vst3(component: Option<&[u8]>, controller: Option<&[u8]>) -> Vec<u8> {
        state_blob::encode(&Blob::Vst3 {
            component,
            controller,
        })
        .unwrap()
    }

    /// A child refusing a set that is not a state blob. A raw blob, the
    /// retired 3.x mode token, an Unknown mode, a state blob of another format, and a malformed one: ok false, and
    /// nothing reaches the plugin.
    #[test]
    fn a_non_sasp_set_is_ok_false_and_nothing_reaches_the_plugin() {
        let builtin = state_blob::builtin(b"gain=0.5");
        for (what, kind, mode, blob) in [
            (
                "a raw blob",
                StateKind::Builtin,
                codec::StateMode::V1,
                b"gain=0.5".to_vec(),
            ),
            (
                // The retired 3.x mode token decodes as Unknown in protocol 4.0, refused as any other word.
                "the retired 3.x mode token",
                StateKind::Builtin,
                serde_json::from_str::<codec::StateMode>(r#""vendor""#).unwrap(),
                builtin.clone(),
            ),
            (
                "mode Unknown (3.0's raw)",
                StateKind::Builtin,
                codec::StateMode::Unknown,
                builtin.clone(),
            ),
            (
                "a builtin blob for a vst3 slot",
                StateKind::Vst3,
                codec::StateMode::V1,
                builtin.clone(),
            ),
            (
                "a vst3 blob for an au slot",
                StateKind::Au,
                codec::StateMode::V1,
                vst3(Some(b"x"), None),
            ),
            (
                "trailing bytes",
                StateKind::Builtin,
                codec::StateMode::V1,
                [builtin.as_slice(), b"!"].concat(),
            ),
            (
                "empty",
                StateKind::Builtin,
                codec::StateMode::V1,
                Vec::new(),
            ),
        ] {
            let p = slot(kind);
            let r = apply(&p, mode, &blob);
            assert_eq!(
                r,
                Receipt::default(),
                "{what}: ok false, nothing applied, no plugin code"
            );
            assert_eq!(
                p.calls.load(Ordering::SeqCst),
                0,
                "{what}: nothing reached the plugin"
            );
        }
    }

    #[test]
    fn the_both_absent_vst3_blob_is_a_counted_no_op_that_reaches_no_plugin() {
        let p = slot(StateKind::Vst3);
        let r = apply(&p, codec::StateMode::V1, &vst3(None, None));
        assert_eq!((r.ok, r.empty, r.ran, r.applied), (true, true, false, None));
        assert_eq!(p.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn applied_is_sent_only_on_a_partial_set_with_exactly_one_part_true() {
        let p = slot(StateKind::Vst3);
        let partial = apply(&p, codec::StateMode::V1, &vst3(Some(b"c"), Some(b"nope")));
        assert_eq!(
            (partial.ok, partial.applied),
            (false, Some(codec::Applied::new(true, false)))
        );
        let whole = apply(&p, codec::StateMode::V1, &vst3(Some(b"c"), Some(b"ok")));
        assert_eq!(
            (whole.ok, whole.applied),
            (true, None),
            "a full success sends no applied"
        );
        let refused = apply(
            &p,
            codec::StateMode::Unknown,
            &vst3(Some(b"c"), Some(b"nope")),
        );
        assert_eq!(
            (refused.ok, refused.applied),
            (false, None),
            "a full refusal sends no applied"
        );
        let builtin = slot(StateKind::Builtin);
        let b = apply(
            &builtin,
            codec::StateMode::V1,
            &state_blob::builtin(b"gain=1"),
        );
        assert_eq!((b.ok, b.applied, b.ran), (true, None, true));
    }
}
