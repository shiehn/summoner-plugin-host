//! `summoner-au-probe`: loads one lab-built AU bundle by absolute path (the lab gate, then in-process registration)
//! and exercises it in its own process, so a test can run it under the recorders and a single `fs_usage` run can
//! watch the first AU load.
//!
//! ```text
//! summoner-au-probe <bundle.component> [--hold <marker> [--hold-secs <n>]] [--pid-file <path>] [--dry]
//!              [--check gain|synth|state] [--noalloc gain|synth]
//! ```
//!
//! - `--hold <marker>`: print `pid <n>` (and write it to `--pid-file`), then wait for `<marker>` to exist before
//!   anything touches the bundle (at most `--hold-secs`, default 3600; then exit 2 having loaded nothing);
//! - `--dry`: the lab gate on the bundle and stop (`dry: gate ok <path>` / `dry: gate refused <why>`); no bundle is
//!   created and no AudioToolbox call is made;
//! - `--check gain`: the load by path and codes, the codes and the name, the parameter, identity at the default and
//!   × 0.5 bit-exact after a parameter, the load again, codes the bundle does not have refused,
//!   latency 0, the image list clean, no STOP;
//! - `--check synth`: a note that sounds at its frame and is released, and two instances rendering the same bytes;
//! - `--check state`: the ClassInfo round trip, the host application's bytes inside it, a refused state;
//! - `--noalloc gain|synth`: under the injected allocation counter, 64 blocks with parameter changes (and notes for
//!   the synth) allocate nothing; prepare and release are reported.
//!
//! Each step (load, prepare, the first render, …) runs under a watchdog with the hostile wedge's bound (2 × 2.1 s):
//! a step past it is `check wedge FAIL <step>` and exit 1. With no run loop or event loop in this process, an AU or
//! CoreFoundation call that waits on the main queue would hang there. Every step's time is printed (`step <name>
//! <µs>`). Each check prints `check <name> ok` or `check <name> FAIL <why>`; exit 1 on any FAIL.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use summoner_format_au::desc::{self, Description};
use summoner_proc::{Block, Dsp, MidiEvent, Plugin, Transport};
use summoner_sys::bundle::{Format, stops};

/// The hostile wedge's hang bound: the host application's detector, two periods of 2.1 s.
const STEP_BOUND: Duration = Duration::from_millis(4200);
const RATE: f64 = 48000.0;
const MAX_FRAMES: usize = 512;

struct Checks {
    failed: bool,
    step: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Checks {
    fn check(&mut self, name: &str, ok: bool, why: impl FnOnce() -> String) {
        if ok {
            println!("check {name} ok");
        } else {
            println!("check {name} FAIL {}", why());
            self.failed = true;
        }
    }

    /// Runs one step under the watchdog and prints its time.
    fn step<T>(&mut self, name: &str, f: impl FnOnce() -> T) -> T {
        *self.step.lock().unwrap() = Some((name.to_string(), Instant::now()));
        let t = Instant::now();
        let r = f();
        let us = t.elapsed().as_micros();
        *self.step.lock().unwrap() = None;
        println!("step {name} {us}");
        self.check(&format!("bound_{name}"), t.elapsed() < STEP_BOUND, || {
            format!("{us} µs")
        });
        r
    }
}

fn watchdog(step: Arc<Mutex<Option<(String, Instant)>>>) {
    std::thread::Builder::new()
        .name("au-probe-watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(50));
                if let Some((name, t)) = step.lock().unwrap().as_ref()
                    && t.elapsed() > STEP_BOUND
                {
                    println!(
                        "check wedge FAIL {name} (over {} ms)",
                        STEP_BOUND.as_millis()
                    );
                    println!("done FAIL");
                    std::process::exit(1);
                }
            }
        })
        .expect("the watchdog");
}

fn description(bundle: &str, sub: &[u8; 4], kind: &[u8; 4]) -> Description {
    let d = summoner_sys::au::Desc {
        kind: u32::from_be_bytes(*kind),
        sub: u32::from_be_bytes(*sub),
        manu: u32::from_be_bytes(*b"SaSL"),
        flags: 0,
        mask: 0,
    };
    Description::new(String::new(), bundle.into(), desc::codes_of(&d))
}

fn ramp(frames: usize, k: f32) -> Vec<f32> {
    (0..frames)
        .map(|i| (i as f32 / frames as f32 - 0.5) * k)
        .collect()
}

fn run(dsp: &mut dyn Dsp, chans: &mut [Vec<f32>], frames: usize, midi: &[MidiEvent]) -> bool {
    let t = Transport::default();
    summoner_proc::with_channels(chans, |channels| {
        dsp.process(&mut Block {
            frames,
            channels,
            midi,
            transport: &t,
        })
    })
}

/// Every image in the process is this probe, a lab bundle, the system's own, or one of the two recorders under
/// all four of `process_images_clean`'s conditions.
fn images_clean(c: &mut Checks) {
    let r = summoner_sys::bundle::process_images_clean();
    c.check("images_lab_and_system_only", r.is_ok(), || {
        format!("{:?}", r.err())
    });
    c.check("no_stop", stops() == 0, || format!("{} STOPs", stops()));
}

fn gain(bundle: &str, c: &mut Checks) {
    let d = description(bundle, b"SLgn", b"aufx");
    let p = c.step("load", || summoner_format_au::load(&d, 2, 2));
    c.check("load_by_path", p.is_ok(), || {
        format!("{:?}", p.as_ref().err())
    });
    let Ok(p) = p else { return };
    c.check("name", p.name() == "Summoner Lab Gain", || p.name().into());
    c.check("codes", p.codes() == "61756678-534c676e-5361534c", || {
        p.codes()
    });
    c.check(
        "params",
        p.param_count() == 1 && p.param_name(0) == "Gain",
        || format!("{} {:?}", p.param_count(), p.param_name(0)),
    );
    c.check("listening", p.listening(), String::new);
    let mut dsp = c.step("prepare", || p.prepare(RATE, MAX_FRAMES));
    c.check("latency", p.latency() == 0, || p.latency().to_string());
    let input = [ramp(256, 1.0), ramp(256, -0.5)];
    let mut chans = input.to_vec();
    let ok = c.step("first_render", || run(dsp.as_mut(), &mut chans, 256, &[]));
    c.check("render_ok", ok, String::new);
    c.check("identity_at_default", chans[..] == input[..], String::new);
    p.set_param(0, 0.5);
    let mut chans = input.to_vec();
    let ok = run(dsp.as_mut(), &mut chans, 256, &[]);
    let half = chans.iter().zip(&input).all(|(o, i)| {
        o.iter()
            .zip(i)
            .all(|(o, i)| o.to_bits() == (i * 0.5).to_bits())
    });
    c.check("half_after_param", ok && half, String::new);
    drop(dsp);
    // A second load of the same bundle (registered again in-process), and codes the bundle does not have
    // refused (the identifier form, which resolved against this process's registrations, is gone in protocol 4.0).
    let again = c.step("load_again", || summoner_format_au::load(&d, 2, 2));
    c.check("load_again", again.is_ok(), || {
        format!("{:?}", again.as_ref().err())
    });
    let other = {
        let mut msg = d.clone();
        msg.codes = [d.codes[0], u32::from_be_bytes(*b"Nope"), d.codes[2]];
        msg
    };
    let refused = summoner_format_au::load(&other, 2, 2);
    c.check(
        "unknown_codes_refused",
        refused
            .as_ref()
            .err()
            .is_some_and(|e| e.as_str().contains("no AudioComponents entry")),
        || format!("{:?}", refused.as_ref().err()),
    );
    images_clean(c);
}

fn synth(bundle: &str, c: &mut Checks) {
    let d = description(bundle, b"SLsy", b"aumu");
    let render = |c: &mut Checks, label: &str| -> Option<Vec<Vec<f32>>> {
        let p = c.step(&format!("load_{label}"), || {
            summoner_format_au::load(&d, 2, 2)
        });
        c.check(&format!("load_{label}"), p.is_ok(), || {
            format!("{:?}", p.as_ref().err())
        });
        let p = p.ok()?;
        let mut dsp = c.step(&format!("prepare_{label}"), || p.prepare(RATE, MAX_FRAMES));
        let mut out = Vec::new();
        let on = [MidiEvent::short(10, [0x90, 69, 100])];
        let off = [MidiEvent::short(0, [0x80, 69, 0])];
        for k in 0..40 {
            let mut chans = vec![vec![0.0; 256]; 2];
            let midi: &[MidiEvent] = match k {
                0 => &on,
                8 => &off,
                _ => &[],
            };
            if !run(dsp.as_mut(), &mut chans, 256, midi) {
                return None;
            }
            out.push(chans[0].clone());
        }
        Some(out)
    };
    let a = render(c, "a");
    let b = render(c, "b");
    let (Some(a), Some(b)) = (a, b) else {
        c.check("renders", false, || "a render failed".into());
        return;
    };
    c.check(
        "silent_before_the_note",
        a[0][..10].iter().all(|x| *x == 0.0),
        String::new,
    );
    c.check(
        "sounds_at_the_note",
        a[0][10..].iter().any(|x| *x != 0.0),
        String::new,
    );
    c.check("released", a[39].iter().all(|x| *x == 0.0), || {
        "still sounding at block 39".into()
    });
    c.check("two_instances_same_bytes", a == b, String::new);
    images_clean(c);
}

fn state(bundle: &str, c: &mut Checks) {
    let d = description(bundle, b"SLgn", b"aufx");
    let p = c.step("load", || summoner_format_au::load(&d, 2, 2));
    let Ok(p) = p else {
        c.check("load", false, || format!("{:?}", p.err()));
        return;
    };
    let mut dsp = c.step("prepare", || p.prepare(RATE, MAX_FRAMES));
    p.set_param(0, 0.25);
    let mut chans = vec![ramp(64, 1.0), ramp(64, 1.0)];
    run(dsp.as_mut(), &mut chans, 64, &[]);
    let s = p.get_state();
    let core = summoner_sys::au::plist_data(&s, "summoner-lab-state");
    c.check(
        "core_bytes_in_class_info",
        core.as_ref().is_some_and(|b| b.starts_with(b"SASL")),
        || format!("{core:?}"),
    );
    let fresh = summoner_format_au::load(&d, 2, 2).expect("a second instance");
    c.check("set_state_ok", fresh.set_state(&s).is_ok(), String::new);
    let s2 = fresh.get_state();
    c.check(
        "round_trip_equal_plist",
        summoner_sys::au::plist_equal(&s, &s2) == Some(true),
        String::new,
    );
    println!("state bytes_equal {}", s == s2);
    let mut dsp2 = fresh.prepare(RATE, MAX_FRAMES);
    let input = ramp(64, 1.0);
    let mut chans = vec![input.clone(), input.clone()];
    run(dsp2.as_mut(), &mut chans, 64, &[]);
    c.check(
        "plays_at_the_restored_gain",
        chans[0]
            .iter()
            .zip(&input)
            .all(|(o, i)| o.to_bits() == (i * 0.25).to_bits()),
        String::new,
    );
    c.check(
        "refused_state_is_err",
        fresh.set_state(b"not a plist").is_err(),
        String::new,
    );
    drop((dsp, dsp2));
    images_clean(c);
}

/// `--noalloc gain|synth`: with the allocation counter injected (`libsummoner_lab_malloccount.dylib`), 64 blocks with
/// parameter changes (and, for the synth, notes and CC 64 in every fourth block) allocate nothing on this thread.
/// Prepare and release are counted and reported, not asserted (prepare is where the preallocation happens; the
/// release is AudioUnitReset on each bus and AudioUnitUninitialize, on the message thread).
fn noalloc(bundle: &str, kind: &str, c: &mut Checks) {
    let d = if kind == "synth" {
        description(bundle, b"SLsy", b"aumu")
    } else {
        description(bundle, b"SLgn", b"aufx")
    };
    let p = match summoner_format_au::load(&d, 2, 2) {
        Ok(p) => p,
        Err(e) => {
            c.check("load", false, || e.into());
            return;
        }
    };
    let Some(counter) = summoner_sys::alloc_count::Counter::resolve() else {
        c.check("counter_injected", false, || {
            "libsummoner_lab_malloccount.dylib is not injected".into()
        });
        return;
    };
    counter.arm();
    let mut dsp = p.prepare(RATE, 64);
    let prepare = counter.disarm();
    // Everything the blocks use is made before the counter is armed for them.
    let mut ch = vec![vec![0.25f32; 64], vec![0.25f32; 64]];
    let busy: Vec<MidiEvent> = (0..8u32)
        .map(|i| MidiEvent::short(i * 7, [0xb0, 64, (i * 16) as u8]))
        .chain([
            MidiEvent::short(3, [0x90, 60, 100]),
            MidiEvent::short(50, [0x80, 60, 0]),
        ])
        .collect();
    let none: Vec<MidiEvent> = Vec::new();
    summoner_proc::warm_channels(ch.len(), 64);
    counter.arm();
    for k in 0..64 {
        p.set_param(0, if k % 2 == 0 { 0.5 } else { 0.75 });
        let midi = if kind == "synth" && k % 4 == 0 {
            &busy
        } else {
            &none
        };
        run(dsp.as_mut(), &mut ch, 64, midi);
    }
    let blocks = counter.disarm();
    counter.arm();
    drop(dsp);
    let release = counter.disarm();
    println!("alloc prepare {prepare} blocks {blocks} release {release}");
    c.check("no_allocation_in_64_blocks", blocks == 0, || {
        format!("{blocks} allocations")
    });
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(bundle) = args.get(1).filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!(
            "usage: summoner-au-probe <bundle.component> [--hold <marker> [--hold-secs <n>]] [--pid-file <path>] [--dry] \
             [--check gain|synth|state] [--noalloc gain|synth]"
        );
        std::process::exit(64);
    };
    if !PathBuf::from(&bundle).is_absolute() {
        eprintln!("summoner-au-probe: {bundle}: not an absolute path");
        std::process::exit(64);
    }
    let pid = std::process::id();
    println!("pid {pid}");
    if let Some(f) = arg(&args, "--pid-file") {
        let _ = std::fs::write(f, pid.to_string());
    }
    if let Some(marker) = arg(&args, "--hold") {
        let secs: u64 = arg(&args, "--hold-secs")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        let until = Instant::now() + Duration::from_secs(secs);
        while !Path::new(&marker).exists() {
            if Instant::now() > until {
                println!("hold: no marker in {secs} s; nothing loaded");
                std::process::exit(2);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        println!("hold: released");
    }
    if args.iter().any(|a| a == "--dry") {
        // The lab gate alone (lexical, then lstat inside the lab): no bundle is created, no AudioToolbox call is made.
        let r = summoner_sys::bundle::Bundle::check(Path::new(&bundle), Format::Au);
        match &r {
            Ok(b) => println!("dry: gate ok {}", b.path().display()),
            Err(why) => println!("dry: gate refused {why}"),
        }
        std::process::exit(i32::from(r.is_err()));
    }
    let step = Arc::new(Mutex::new(None));
    watchdog(step.clone());
    let mut c = Checks {
        failed: false,
        step,
    };
    match (
        arg(&args, "--check").as_deref(),
        arg(&args, "--noalloc").as_deref(),
    ) {
        (Some("gain"), None) => gain(&bundle, &mut c),
        (Some("synth"), None) => synth(&bundle, &mut c),
        (Some("state"), None) => state(&bundle, &mut c),
        (None, Some(k @ ("gain" | "synth"))) => noalloc(&bundle, k, &mut c),
        other => {
            eprintln!("summoner-au-probe: unknown mode {other:?}");
            std::process::exit(64);
        }
    }
    c.check(
        "r4_no_chain_thread_release",
        summoner_sys::au::wrong_thread_drops() == 0,
        String::new,
    );
    println!("done {}", if c.failed { "FAIL" } else { "ok" });
    std::process::exit(i32::from(c.failed));
}
