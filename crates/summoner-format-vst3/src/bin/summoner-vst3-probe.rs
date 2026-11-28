//! `summoner-vst3-probe`: loads one lab-built VST3 bundle by absolute path and exercises it, in its own process, so a
//! test can run it under the file-access recorder (`target.noindex/plugins/libsummoner_lab_fswatch.dylib`) and a single
//! `fs_usage` run can watch the first real load.
//!
//! ```text
//! summoner-vst3-probe <bundle.vst3> [--hold <marker>] [--pid-file <path>] [--dry] [--check gain|synth]
//!                [--noalloc gain|synth]
//! ```
//!
//! - `--hold <marker>`: print `pid <n>` (and write it to `--pid-file`), then wait for `<marker>` to exist before
//!   anything touches the bundle (`--hold-secs`, default 3600, at most; then exit 2 having loaded nothing). The marker
//!   is looked at every 250 ms;
//! - `--dry`: stat the bundle and stop; nothing is loaded (the recorder's positive control);
//! - `--check gain`: the gain checks (classes, the class match, identity at the default, × 0.5 after a
//!   parameter, both chunks, latency 0). Each check prints `check <name> ok` or `check <name> FAIL <why>`; exit 1 on
//!   any FAIL;
//! - `--check state`: the raw chunks round-trip byte for byte and play;
//! - `--check liar`: the hostile bundle's lying parameter count, reported but never indexed past;
//! - `--check quad [--io <in>/<out>]`: main buses wider than or unlike stereo that refuse the stereo request
//!   (4/4 by default), hosted with a real buffer for every channel;
//! - `--check synth`: buses (no audio input, an event input), parameters, a note that sounds and is released, and
//!   two instances rendering the same bytes;
//! - `--noalloc gain|synth`: with the allocation counter injected (`libsummoner_lab_malloccount.dylib`), 64 blocks with
//!   parameter changes (and, for the synth, notes and mapped controllers, more than five points on one parameter in
//!   a block) allocate nothing on the processing thread. Prepare and release are counted too and reported, not
//!   asserted ("blocks: 0, release: n"; prepare is where the preallocation happens).
#![deny(missing_docs)]
#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::time::{Duration, Instant};
use summoner_format_vst3::desc::Description;
use summoner_proc::{Block, MidiEvent, Plugin, Transport};
use summoner_sys::bundle::{Bundle, Format};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

struct Checks {
    failed: bool,
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
}

fn block(plugin: &mut dyn summoner_proc::Dsp, chans: &mut [Vec<f32>], frames: usize) {
    block_midi(plugin, chans, frames, &[]);
}

fn block_midi(
    plugin: &mut dyn summoner_proc::Dsp,
    chans: &mut [Vec<f32>],
    frames: usize,
    midi: &[MidiEvent],
) {
    let t = Transport::default();
    summoner_proc::with_channels(chans, |channels| {
        plugin.process(&mut Block {
            frames,
            channels,
            midi,
            transport: &t,
        })
    });
}

/// The Audio Module class of `bundle` called `name`, as a slot names it (its class id).
fn description(bundle: &str, name: &str) -> Option<Description> {
    let b = Bundle::check(std::path::Path::new(bundle), Format::Vst3).ok()?;
    let m = summoner_sys::vst3::Module::open(&b).ok()?;
    let k = m
        .classes()
        .into_iter()
        .find(|k| k.category == "Audio Module Class" && k.name == name)?;
    Some(Description::new(name.into(), bundle.into(), k.cid))
}

fn synth(bundle: &str, c: &mut Checks) {
    let Some(d) = description(bundle, "Summoner Lab Synth") else {
        c.check("load", false, || "no Audio Module class".into());
        return;
    };
    let render = || -> Option<Vec<f32>> {
        let p = summoner_format_vst3::load(&d, 0, 2).ok()?;
        p.set_param(1, 0.002); // release: 96 frames at 48 kHz
        let mut dsp = p.prepare(48_000.0, 64);
        let mut out = Vec::new();
        for k in 0..8 {
            let midi = match k {
                0 => vec![MidiEvent::short(10, [0x90, 69, 127])],
                2 => vec![MidiEvent::short(5, [0x80, 69, 0])],
                _ => vec![],
            };
            let mut ch = vec![vec![0.0; 64], vec![0.0; 64]];
            block_midi(dsp.as_mut(), &mut ch, 64, &midi);
            out.extend_from_slice(&ch[0]);
        }
        Some(out)
    };
    let p = summoner_format_vst3::load(&d, 0, 2);
    c.check("load", p.is_ok(), || format!("{:?}", p.as_ref().err()));
    let Ok(p) = p else { return };
    let b = p.buses();
    c.check("buses", b.inputs == 0 && b.outputs == 2 && b.events, || {
        format!("{b:?}")
    });
    // The prepared processor reports the arranged buses, also when the chain offers it stereo input (a 0-input
    // instrument at the head of an effect chain).
    let in_fx = summoner_format_vst3::load(&d, 2, 2).map(|q| q.prepare(48_000.0, 64).io());
    let io = p.prepare(48_000.0, 64).io();
    let want = summoner_proc::Io::new(0, 2);
    c.check(
        "io",
        io == want && in_fx.as_ref().is_ok_and(|x| *x == want),
        || format!("{io:?} {in_fx:?}"),
    );
    c.check(
        "params",
        p.param_count() == 5 && p.param_name(1) == "Release",
        || format!("{} {:?}", p.param_count(), p.param_name(1)),
    );
    let (a, z) = (render(), render());
    c.check("renders", a.is_some(), || "render failed".into());
    let a = a.unwrap_or_default();
    c.check(
        "silent_before_the_note",
        a[..10].iter().all(|x| *x == 0.0),
        || format!("{:?}", &a[..10]),
    );
    c.check("sounds_after_the_note", a[11] != 0.0, || {
        format!("{:?}", &a[10..14])
    });
    c.check("released_to_silence", a[64 * 7 + 63] == 0.0, || {
        format!("{}", a[64 * 7 + 63])
    });
    c.check("deterministic", z.as_deref() == Some(&a[..]), || {
        "two instances differ".into()
    });
}

/// `--check quad [--io <in>/<out>]`: the quad fixture (4/4 by default; the wide fixtures 1/4, 4/1, 2/6) has main buses
/// that refuse the stereo request, so they keep their own arrangement, and its process dereferences every channel of
/// each bus. Hosted with every channel a real buffer (a null one is a SIGSEGV here, the probe's death), inputs past the
/// block zero, outputs past it dropped. Blocks of 2, 2, 4 and 2 channels, each against [`wide_model`]: the last stereo
/// block after the 4-wide one proves the inputs past the block are zeroed again (not left from the wide block).
fn quad(bundle: &str, io: (u32, u32), c: &mut Checks) {
    let Some(d) = description(bundle, "Summoner Lab Gain") else {
        c.check("load", false, || "no Audio Module class".into());
        return;
    };
    let p = summoner_format_vst3::load(&d, 2, 2);
    c.check("load", p.is_ok(), || format!("{:?}", p.as_ref().err()));
    let Ok(p) = p else { return };
    c.check(
        "buses_kept_wide",
        p.buses().inputs == io.0 && p.buses().outputs == io.1,
        || format!("{:?}", p.buses()),
    );
    let mut dsp = p.prepare(48_000.0, 64);
    let want = summoner_proc::Io::new(io.0, io.1);
    c.check("io", dsp.io() == want, || format!("{:?}", dsp.io()));
    let l: Vec<f32> = (0..64).map(|i| i as f32 / 64.0 - 0.5).collect();
    let r: Vec<f32> = l.iter().map(|x| -0.5 * x).collect();
    let stereo = vec![l.clone(), r.clone()];
    let wide = vec![l.clone(), r.clone(), vec![0.125; 64], vec![0.25; 64]];
    for (k, input) in [&stereo, &stereo, &wide, &stereo].into_iter().enumerate() {
        let mut ch = input.clone();
        block(dsp.as_mut(), &mut ch, 64);
        let want = wide_model(io, input);
        c.check(&format!("block_{k}_{}ch", input.len()), ch == want, || {
            format!(
                "{:?} want {:?}",
                ch.iter().map(|v| v[0]).collect::<Vec<_>>(),
                want.iter().map(|v| v[0]).collect::<Vec<_>>()
            )
        });
    }
    // A block narrower than the host's 2 (never sent) has too few lines for a bus wider than 2: THREW, never a null
    // channel.
    if io.1 > 2 {
        let t = Transport::default();
        let mut ch = vec![l.clone()];
        let ok = summoner_proc::with_channels(&mut ch, |channels| {
            dsp.process(&mut Block {
                frames: 64,
                channels,
                midi: &[],
                transport: &t,
            })
        });
        c.check("narrow_block_threw", !ok, || "processed".into());
    }
}

/// What the quad / wide fixture leaves in a block of `input`, hosted with a real buffer per bus channel: its inputs
/// are the block's channels (zero past the block), its outputs land on the block's channels (past the block:
/// dropped); a block channel past its outputs keeps the input. Out c (c < 2) = in c (0 past the inputs) plus every
/// input from 2 on; outs from 2 on are 0.75.
fn wide_model((n_in, n_out): (u32, u32), input: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let frames = input[0].len();
    let ins: Vec<Vec<f32>> = (0..n_in as usize)
        .map(|k| input.get(k).cloned().unwrap_or_else(|| vec![0.0; frames]))
        .collect();
    (0..input.len())
        .map(|c| {
            if c >= n_out as usize {
                return input[c].clone();
            }
            if c >= 2 {
                return vec![0.75; frames];
            }
            (0..frames)
                .map(|i| {
                    let mut v = ins.get(c).map_or(0.0, |x| x[i]);
                    for x in ins.iter().skip(2) {
                        v += x[i];
                    }
                    v
                })
                .collect()
        })
        .collect()
}

/// `--check liar`: the hostile bundle's lying controller (claims 77 parameters; 5 real, 5 "ghosts" with a colliding id,
/// the rest failing). Reported as claimed (77; titles, or empty); never indexed past what it has: values for a ghost
/// (7) and a failed index (20) are dropped and counted, and the output stays at gain 1 (they never drive the plugin's
/// parameter 0, the gain).
fn liar(bundle: &str, c: &mut Checks) {
    let Some(d) = description(bundle, "Summoner Lab Hostile (77 parameters)") else {
        c.check("load", false, || "no class".into());
        return;
    };
    let Ok(p) = summoner_format_vst3::load(&d, 2, 2) else {
        c.check("load", false, || "load failed".into());
        return;
    };
    c.check("reported_count", p.param_count() == 77, || {
        p.param_count().to_string()
    });
    let names: Vec<String> = (0..77).map(|i| p.param_name(i)).collect();
    c.check(
        "names_in_the_expected_order",
        names[..5] == ["Gain", "Abort", "SIGSEGV", "Throw", "Wedge"]
            && names[5..10].iter().all(|n| n == "ghost")
            && names[10..].iter().all(String::is_empty),
        || format!("{:?}", &names[..12]),
    );
    let mut dsp = p.prepare(48_000.0, 64);
    p.set_param(7, 0.0); // a ghost: id 0 collides with the gain
    p.set_param(20, 0.0); // a failed index: a zeroed info, id 0 again
    c.check("dropped_and_counted", p.dropped_params() == 2, || {
        p.dropped_params().to_string()
    });
    let ramp: Vec<f32> = (0..64).map(|i| i as f32 / 64.0 - 0.5).collect();
    let mut ch = vec![ramp.clone(), ramp.clone()];
    block(dsp.as_mut(), &mut ch, 64);
    c.check("the_gain_is_untouched", ch[0] == ramp, || {
        format!("{:?}", &ch[0][..4])
    });
}

/// `--check state`: the raw chunks (the product path). A gain set through a parameter reaches the
/// component chunk after a block; both chunks set on a fresh instance come back byte for byte and play at that gain.
fn state(bundle: &str, c: &mut Checks) {
    let Some(d) = description(bundle, "Summoner Lab Gain") else {
        c.check("load", false, || "no class".into());
        return;
    };
    let fresh = || summoner_format_vst3::load(&d, 2, 2).ok();
    let Some(a) = fresh() else {
        c.check("load", false, || "load failed".into());
        return;
    };
    let mut dsp = a.prepare(48_000.0, 64);
    a.set_param(0, 0.25);
    let mut ch = vec![vec![0.5f32; 64], vec![0.5f32; 64]];
    block(dsp.as_mut(), &mut ch, 64);
    drop(dsp);
    let (comp, ctrl) = (
        a.chunk(0).unwrap_or_default(),
        a.chunk(1).unwrap_or_default(),
    );
    c.check(
        "component_chunk_holds_the_gain",
        comp.len() == 20 && comp[16..20] == 0.25f32.to_le_bytes(),
        || format!("{comp:?}"),
    );
    let ramp: Vec<f32> = (0..64).map(|i| i as f32 / 64.0 - 0.5).collect();
    let plays = |p: &summoner_format_vst3::Vst3Plugin| {
        let mut dsp = p.prepare(48_000.0, 64);
        let mut ch = vec![ramp.clone(), ramp.clone()];
        block(dsp.as_mut(), &mut ch, 64);
        ch[0].clone()
    };
    let quarter: Vec<f32> = ramp.iter().map(|x| x * 0.25).collect();
    if let Some(b) = fresh() {
        let set = b.set_chunk(0, &comp).and(b.set_chunk(1, &ctrl));
        c.check("raw_chunks_set", set.is_ok(), || format!("{set:?}"));
        c.check(
            "raw_chunks_round_trip",
            b.chunk(0).ok().as_ref() == Some(&comp) && b.chunk(1).ok().as_ref() == Some(&ctrl),
            || "the chunks differ".into(),
        );
        c.check("raw_chunks_play_the_gain", plays(&b) == quarter, || {
            "not at 0.25".into()
        });
    }
    // A chunk the plugin refuses is refused (and the instance keeps its own).
    if let Some(f) = fresh() {
        let r = f.set_chunk(0, b"not a SASL state");
        c.check("a_refused_chunk_is_refused", r.is_err(), || {
            format!("{r:?}")
        });
    }
}

/// `--noalloc`: 64 blocks under the armed counter.
fn noalloc(bundle: &str, kind: &str, c: &mut Checks) {
    let (name, ch_in) = if kind == "synth" {
        ("Summoner Lab Synth", 0)
    } else {
        ("Summoner Lab Gain", 2)
    };
    let Some(d) = description(bundle, name) else {
        c.check("load", false, || "no Audio Module class".into());
        return;
    };
    let Ok(p) = summoner_format_vst3::load(&d, ch_in, 2) else {
        c.check("load", false, || "load failed".into());
        return;
    };
    if !summoner_sys::alloc_count::arm() {
        c.check("counter_injected", false, || {
            "libsummoner_lab_malloccount.dylib is not injected".into()
        });
        return;
    }
    let mut dsp = p.prepare(48_000.0, 64);
    let prepare = summoner_sys::alloc_count::disarm().unwrap_or(u64::MAX);
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
    summoner_sys::alloc_count::arm();
    for k in 0..64 {
        p.set_param(0, if k % 2 == 0 { 0.5 } else { 0.75 });
        let midi = if kind == "synth" && k % 4 == 0 {
            &busy
        } else {
            &none
        };
        block_midi(dsp.as_mut(), &mut ch, 64, midi);
    }
    let blocks = summoner_sys::alloc_count::disarm().unwrap_or(u64::MAX);
    summoner_sys::alloc_count::arm();
    drop(dsp); // the processor's release (setProcessing (false), setActive (false))
    let release = summoner_sys::alloc_count::disarm().unwrap_or(u64::MAX);
    println!("alloc prepare {prepare} blocks {blocks} release {release}");
    c.check("no_allocation_in_64_blocks", blocks == 0, || {
        format!("{blocks} allocations")
    });
}

fn gain(bundle: &str, c: &mut Checks) {
    let module = Bundle::check(std::path::Path::new(bundle), Format::Vst3)
        .map_err(|r| format!("{r}"))
        .and_then(|b| summoner_sys::vst3::Module::open(&b).map_err(|e| format!("{e:?}")));
    c.check("module_open", module.is_ok(), || {
        format!("{:?}", module.as_ref().err())
    });
    let Ok(module) = module else { return };
    let classes = module.classes();
    let names: Vec<_> = classes
        .iter()
        .map(|k| (k.category.as_str(), k.name.as_str()))
        .collect();
    c.check(
        "classes",
        names
            == [
                ("Audio Module Class", "Summoner Lab Gain"),
                ("Component Controller Class", "Summoner Lab Gain Controller"),
            ],
        || format!("{names:?}"),
    );
    drop(module);
    let Some(proc_class) = classes.iter().find(|k| k.category == "Audio Module Class") else {
        return;
    };
    let d = Description::new("Summoner Lab Gain".into(), bundle.into(), proc_class.cid);
    let p = summoner_format_vst3::load(&d, 2, 2);
    c.check("load_by_class_id", p.is_ok(), || {
        format!("{:?}", p.as_ref().err())
    });
    let Ok(p) = p else { return };
    c.check(
        "buses",
        p.buses().inputs == 2 && p.buses().outputs == 2,
        || format!("{:?}", p.buses()),
    );
    c.check("latency", p.latency() == 0, || p.latency().to_string());
    c.check(
        "params",
        p.param_count() == 1 && p.param_name(0) == "Gain",
        || format!("{} {:?}", p.param_count(), p.param_name(0)),
    );
    let mut dsp = p.prepare(48_000.0, 64);
    // A stereo effect's prepared processor reports two in, two out.
    c.check("io", dsp.io() == summoner_proc::Io::STEREO, || {
        format!("{:?}", dsp.io())
    });
    let ramp: Vec<f32> = (0..64).map(|i| i as f32 / 64.0 - 0.5).collect();
    let mut ch = vec![ramp.clone(), ramp.clone()];
    block(dsp.as_mut(), &mut ch, 64);
    c.check(
        "identity_at_default",
        ch[0] == ramp && ch[1] == ramp,
        || format!("{:?}", &ch[0][..4]),
    );
    p.set_param(0, 0.5);
    let mut ch = vec![ramp.clone(), ramp.clone()];
    block(dsp.as_mut(), &mut ch, 64);
    let want: Vec<f32> = ramp.iter().map(|x| x * 0.5).collect();
    c.check("half_after_param", ch[0] == want && ch[1] == want, || {
        format!("{:?}", &ch[0][..4])
    });
    let comp = p.chunk(0).unwrap_or_default();
    c.check(
        "component_chunk",
        comp.starts_with(b"SASL") && comp.len() == 20,
        || format!("{comp:?}"),
    );
    let ctrl = p.chunk(1).unwrap_or_default();
    c.check(
        "controller_chunk",
        ctrl == [b'S', b'A', b'S', b'C', 1, 0, 0, 0, 1, 0, 0, 0],
        || format!("{ctrl:?}"),
    );
    // The component chunk round-trips: set it on a fresh instance, read it back byte for byte.
    if let Ok(q) = summoner_format_vst3::load(&d, 2, 2) {
        let set = q.set_state(&comp);
        c.check(
            "component_chunk_roundtrip",
            set.is_ok() && q.get_state() == comp,
            || format!("{set:?}"),
        );
    }
    drop(dsp);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(bundle) = args.get(1).filter(|a| !a.starts_with("--")).cloned() else {
        eprintln!(
            "usage: summoner-vst3-probe <bundle.vst3> [--hold <marker> [--hold-secs <n>]] [--pid-file <path>] [--dry] \
             [--check gain|synth] [--noalloc gain|synth]"
        );
        std::process::exit(64);
    };
    if !PathBuf::from(&bundle).is_absolute() {
        eprintln!("summoner-vst3-probe: {bundle}: not an absolute path");
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
        while !std::path::Path::new(&marker).exists() {
            if Instant::now() > until {
                println!("hold: no marker in {secs} s; nothing loaded");
                std::process::exit(2);
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        println!("hold: released");
    }
    if args.iter().any(|a| a == "--dry") {
        let ok = std::fs::metadata(&bundle).is_ok();
        println!("dry: stat {}", if ok { "ok" } else { "missing" });
        std::process::exit(if ok { 0 } else { 1 });
    }
    let mut c = Checks { failed: false };
    match (
        arg(&args, "--check").as_deref(),
        arg(&args, "--noalloc").as_deref(),
    ) {
        (Some("gain"), None) => gain(&bundle, &mut c),
        (Some("synth"), None) => synth(&bundle, &mut c),
        (Some("liar"), None) => liar(&bundle, &mut c),
        (Some("state"), None) => state(&bundle, &mut c),
        (Some("quad"), None) => {
            let io = arg(&args, "--io")
                .and_then(|v| {
                    let (i, o) = v.split_once('/')?;
                    Some((i.parse().ok()?, o.parse().ok()?))
                })
                .unwrap_or((4, 4));
            quad(&bundle, io, &mut c)
        }
        (None, Some(k @ ("gain" | "synth"))) => noalloc(&bundle, k, &mut c),
        other => {
            eprintln!("summoner-vst3-probe: unknown mode {other:?}");
            std::process::exit(64);
        }
    }
    println!("done {}", if c.failed { "FAIL" } else { "ok" });
    std::process::exit(i32::from(c.failed));
}
