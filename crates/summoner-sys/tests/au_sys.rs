//! `sys::au` against the lab's own AUs (target.noindex/plugins/au, `tools/build-plugins.sh`): the STOP rules, the
//! output list, the listeners, the input callback's pull rules, state, and the image list. Every scenario runs in a
//! child run of this test binary, because a STOP ends plugin loading for the rest of its process.
//!
//! **Not gated** (the first AU load was proven clean): every AU here comes through `Bundle::check` (the lab's own
//! bundles), every scenario asserts `images_clean`, and nothing asserts a wall-clock bound.
#![cfg(feature = "au")]
#[path = "../../../testkit/plugins/lab_plugins.rs"]
mod lab_plugins;

use std::path::PathBuf;
use std::process::Command;
use summoner_sys::au::{self, Component, Error, Instance};
use summoner_sys::bundle::{Bundle, Format, allowed_root, stops};

// The hostile AU's AU-only parameter ids (testkit/plugins/au/lab_au.cpp, `hx`): after the host application's 11.
const RENDER_ERROR: u32 = 11;
const PULL_OUTSIDE: u32 = 12;
const PULL_TWICE: u32 = 13;
const PULL_WRONG_N: u32 = 14;
const SWAP_OUT: u32 = 15;
const OUTSIDE_STATUS: u32 = 16;
const OUTSIDE_SAW_DATA: u32 = 17;
const TWICE_EQUAL: u32 = 18;
const WRONG_N_STATUS: u32 = 19;

fn bundle(name: &str) -> Bundle {
    let p = allowed_root(Format::Au)
        .unwrap()
        .join(format!("{name}.component"));
    Bundle::check(&p, Format::Au)
        .unwrap_or_else(|r| panic!("{}: {r} (tools/build-plugins.sh)", p.display()))
}

fn component(name: &str, sub: &[u8; 4]) -> Component {
    let reg = au::register(&bundle(name)).expect("register");
    reg.components
        .into_iter()
        .map(|(c, _)| c)
        .find(|c| c.desc.sub == u32::from_be_bytes(*sub))
        .expect("the component")
}

/// Every image in the process is the test binary, a lab bundle, the system's own, or an allowed recorder.
fn images_clean() {
    if let Err(bad) = summoner_sys::bundle::process_images_clean() {
        panic!("non-lab, non-system images in the process: {bad:?}");
    }
}

/// A render's result, its inputs and its outputs.
type Rendered = (Result<(), Error>, Vec<Vec<f32>>, Vec<Vec<f32>>);

fn ramp(n: usize, k: f32) -> Vec<f32> {
    (0..n).map(|i| (i as f32 / n as f32 - 0.5) * k).collect()
}

fn render(p: &mut au::Processor, frames: usize) -> Rendered {
    let ins = vec![ramp(frames, 1.0), ramp(frames, -0.5)];
    let mut outs = vec![vec![9.0f32; frames]; 2];
    let r = {
        let i: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut o: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        p.render(&i, &mut o, frames, None)
    };
    (r, ins, outs)
}

fn prepared(inst: &mut Instance) -> au::Processor {
    inst.prepare(48000.0, 512, 2, 2).expect("prepare")
}

/// The scenarios, each in its own process.
fn scenario(name: &str) {
    match name {
        // The gain: identity at its default, bit-exact at 0.5; state; listeners; no STOP; clean images.
        "gain" => {
            let mut inst = component("summoner-lab-gain", b"SLgn")
                .instantiate()
                .unwrap();
            assert!(inst.listening());
            let params = inst.params();
            assert_eq!(params[0].name, "Gain");
            let mut p = prepared(&mut inst);
            let (r, ins, outs) = render(&mut p, 256);
            r.unwrap();
            assert_eq!(outs, ins, "identity at the default gain");
            p.set_param(0, 0.5);
            let (r, ins, outs) = render(&mut p, 256);
            r.unwrap();
            for (o, i) in outs.iter().zip(&ins) {
                assert!(
                    o.iter()
                        .zip(i)
                        .all(|(o, i)| o.to_bits() == (i * 0.5).to_bits()),
                    "x * 0.5, bit-exact"
                );
            }
            let state = inst.class_info().unwrap();
            let core = au::plist_data(&state, "summoner-lab-state").expect("the core's bytes");
            assert_eq!(&core[..4], b"SASL");
            assert!(
                inst.set_class_info(b"not a plist").is_err(),
                "a parse failure is reported"
            );
            inst.set_class_info(&state).unwrap();
            assert_eq!(inst.latency_seconds(), 0.0);
            drop(p);
            images_clean();
        }
        // A load that brings in a foreign image is a STOP; the same path again is refused, and so is any other.
        "retry" => {
            let drag = bundle("summoner-lab-drag");
            assert!(
                matches!(au::register(&drag), Err(Error::Refused(_))),
                "the first load is a STOP"
            );
            assert_eq!(stops(), 1);
            assert!(
                matches!(au::register(&drag), Err(Error::Refused(_))),
                "the retry is refused"
            );
            assert!(
                matches!(
                    au::register(&bundle("summoner-lab-gain")),
                    Err(Error::Refused(_))
                ),
                "so is any other"
            );
            assert_eq!(stops(), 1, "a refusal is not a second STOP");
        }
        // A foreign image loaded during construction / Initialize is a STOP.
        "ctor" => {
            assert!(matches!(
                component("summoner-lab-hostile", b"SLh8").instantiate(),
                Err(Error::Refused(_))
            ));
            assert_eq!(stops(), 1);
        }
        "init" => {
            let mut inst = component("summoner-lab-hostile", b"SLh9")
                .instantiate()
                .unwrap();
            assert!(matches!(
                inst.prepare(48000.0, 512, 2, 2),
                Err(Error::Refused(_))
            ));
            assert_eq!(stops(), 1);
        }
        // A rewritten output list is a failed block; a plain one is not.
        "swap" => {
            let mut inst = component("summoner-lab-hostile", b"SLho")
                .instantiate()
                .unwrap();
            let mut p = prepared(&mut inst);
            render(&mut p, 128).0.unwrap();
            p.set_param(SWAP_OUT, 1.0);
            assert!(
                matches!(render(&mut p, 128).0, Err(Error::Failed(_))),
                "a swapped-out output buffer is a failure"
            );
            p.set_param(SWAP_OUT, 0.0);
            render(&mut p, 128).0.unwrap();
            p.set_param(RENDER_ERROR, 1.0);
            assert!(
                matches!(render(&mut p, 128).0, Err(Error::Status(_))),
                "a render status is a failure"
            );
            assert_eq!(stops(), 0);
        }
        // An AU that refuses listeners is known as such.
        "deaf" => {
            assert!(
                !component("summoner-lab-hostile", b"SLh5")
                    .instantiate()
                    .unwrap()
                    .listening()
            );
            assert!(
                component("summoner-lab-hostile", b"SLho")
                    .instantiate()
                    .unwrap()
                    .listening()
            );
        }
        // A second pull reads the same input; a short pull and a pull outside any render get NoConnection and no
        // data.
        "pulls" => {
            let mut inst = component("summoner-lab-hostile", b"SLho")
                .instantiate()
                .unwrap();
            let mut p = prepared(&mut inst);
            p.set_param(PULL_TWICE, 1.0);
            p.set_param(PULL_WRONG_N, 1.0);
            for _ in 0..4 {
                render(&mut p, 256).0.unwrap();
            }
            assert_eq!(
                inst.param(TWICE_EQUAL).unwrap(),
                1.0,
                "the second pull matched the first"
            );
            assert_eq!(
                inst.param(WRONG_N_STATUS).unwrap(),
                au::ERR_NO_CONNECTION as f32
            );
            p.set_param(PULL_OUTSIDE, 1.0);
            // No timing assumption: render, then wait (bounded, generously) until the AU's
            // own thread has made at least one outside pull and reported it, however busy the machine is.
            render(&mut p, 256).0.unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while inst.param(OUTSIDE_STATUS).unwrap() == 0.0 {
                assert!(
                    std::time::Instant::now() < until,
                    "the AU's thread never pulled outside a render"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert_eq!(
                inst.param(OUTSIDE_STATUS).unwrap(),
                au::ERR_NO_CONNECTION as f32,
                "an outside pull"
            );
            assert_eq!(
                inst.param(OUTSIDE_SAW_DATA).unwrap(),
                0.0,
                "no host memory read outside a render"
            );
            p.set_param(PULL_OUTSIDE, 0.0);
            drop(p);
            images_clean();
        }
        // This process's ClassInfo bytes for the gain at 0.25, in hex, for the parent to compare
        // across processes.
        "state-bytes" => {
            let mut inst = component("summoner-lab-gain", b"SLgn")
                .instantiate()
                .unwrap();
            let mut p = prepared(&mut inst);
            p.set_param(0, 0.25);
            render(&mut p, 64).0.unwrap();
            drop(p);
            let b = inst.class_info().unwrap();
            println!(
                "STATE-HEX {}",
                b.iter().map(|x| format!("{x:02x}")).collect::<String>()
            );
            images_clean();
        }
        // The paramlist fixture, the positive control: its ParameterList is [0, N) until its 8th block, then [N, 0..N),
        // announced; its right channel is id N's value.
        // Which name path the lab params AU takes. Every parameter's info
        // has kAudioUnitParameterFlag_HasCFNameString (1 << 27) and kAudioUnitParameterFlag_CFNameRelease (1 << 4):
        // the CF-string name path with the caller's release; the fixed `name` array path is not exercised.
        // The fixed `name` array path, on the lab AU whose parameters carry no CF
        // names (no HasCFNameString, no CFNameRelease): the bytes up to the first NUL (all 52 when there is none),
        // decoded as UTF-8 with an invalid byte replaced by U+FFFD; the custom unit's name still read.
        "c04fixed" => {
            let inst = component("summoner-lab-paramsfixed", b"SLpf")
                .instantiate()
                .unwrap();
            let ps = inst.params();
            for p in ps.iter().skip(1) {
                assert_eq!(
                    p.flags & ((1 << 27) | (1 << 4)),
                    0,
                    "id {}: no CF name flags",
                    p.id
                );
            }
            let names: Vec<&str> = ps.iter().skip(1).map(|p| p.name.as_str()).collect();
            let d52 = "D".repeat(52);
            assert_eq!(
                names,
                [
                    "Cutoff",
                    "Mode",
                    "Offline",
                    "Cutoff",
                    "",
                    d52.as_str(),
                    "R\u{FFFD}s"
                ]
            );
            assert_eq!(ps[6].unit_name, "drv");
            images_clean();
        }
        // The parameter list's size edges (no AudioUnitSDK unit produces them): the lab params AU's list under the size
        // interposer (testkit/plugins/plsize, mode SUMMONER_LAB_PLSIZE); the ids the list yields are printed for the parent.
        "plsize" => {
            let inst = component("summoner-lab-params", b"SLpm")
                .instantiate()
                .unwrap();
            let ids: Vec<u32> = inst.params().iter().map(|p| p.id).collect();
            println!("PLSIZE-IDS {ids:?}");
        }
        "c04names" => {
            let inst = component("summoner-lab-params", b"SLpm")
                .instantiate()
                .unwrap();
            let ps = inst.params();
            for p in &ps {
                println!(
                    "c04names id {} flags {:#010x} unit {} name {:?} unit_name {:?}",
                    p.id, p.flags, p.unit, p.name, p.unit_name
                );
                assert_ne!(p.flags & (1 << 27), 0, "id {}: HasCFNameString", p.id);
                assert_ne!(p.flags & (1 << 4), 0, "id {}: CFNameRelease", p.id);
            }
            let names: Vec<&str> = ps.iter().skip(1).map(|p| p.name.as_str()).collect();
            assert_eq!(
                names,
                ["Cutoff", "Mode", "Offline", "Cutoff", "", "Drive", "Res"]
            );
            assert_eq!(ps[6].unit_name, "drv");
            assert!(
                ps.iter()
                    .filter(|p| p.unit != 26)
                    .all(|p| p.unit_name.is_empty())
            );
            // "Offline" (id 3) carries kAudioUnitParameterFlag_NonRealTime (the binding's
            // FLAG_NON_REAL_TIME, 1 << 24 in AudioToolbox/AudioUnitProperties.h), and no other parameter does.
            let nrt: Vec<bool> = ps
                .iter()
                .map(|p| p.flags & au::FLAG_NON_REAL_TIME != 0)
                .collect();
            assert_eq!(nrt, [false, false, false, true, false, false, false, false]);
            images_clean();
        }
        "paramlist" => {
            let mut inst = component("summoner-lab-paramlist", b"SLpl")
                .instantiate()
                .unwrap();
            let before: Vec<u32> = inst.params().iter().map(|p| p.id).collect();
            let n = before.len() as u32;
            assert_eq!(before, (0..n).collect::<Vec<_>>(), "the core's ids first");
            let mut p = prepared(&mut inst);
            for k in 0..7 {
                let (r, _, outs) = render(&mut p, 64);
                r.unwrap();
                assert!(
                    outs[1].iter().all(|x| *x == 0.0),
                    "block {k}: id N at its default"
                );
            }
            assert!(!inst.take_notices().1, "no notice before the 8th block");
            render(&mut p, 64).0.unwrap();
            assert!(inst.take_notices().1, "the 8th block announced the change");
            p.set_param(n, 0.75);
            let (r, _, outs) = render(&mut p, 64);
            r.unwrap();
            assert!(
                outs[1].iter().all(|x| *x == 0.75),
                "the right channel is id N's value"
            );
            drop(p);
            let after: Vec<u32> = inst.params().iter().map(|p| p.id).collect();
            let mut want = vec![n];
            want.extend(0..n);
            assert_eq!(after, want, "id N inserted at index 0");
            images_clean();
        }
        // The hostcb fixture under sys::au, which registers the HostCallbacks: with nothing published, the default
        // answers; then exactly what the block published (and its fallbacks for what it lacks).
        "hostcb" => {
            let mut inst = component("summoner-lab-hostcb", b"SLhc")
                .instantiate()
                .unwrap();
            assert!(
                inst.host_callbacks(),
                "the AU accepted kAudioUnitProperty_HostCallbacks"
            );
            let mut p = prepared(&mut inst);
            let (r, _, outs) = render(&mut p, 64);
            r.unwrap();
            assert!(
                outs[0].iter().all(|x| *x == 0.0),
                "beat 0: {:?}",
                &outs[0][..4]
            );
            assert_eq!(
                &outs[1][..8],
                &[120.0, 7.0, 0.0, 0.0, 4.0, 4.0, 0.0, 0.0],
                "the expected defaults"
            );
            p.publish_transport(&au::HostTransport {
                ppq: 7.5,
                bpm: 96.0,
                time_sig_num: 3,
                time_sig_den: 8,
                bar_start_ppq: 6.0,
                bar_valid: true,
                playing: true,
                time_samples: 48_000,
                ..au::HostTransport::default()
            });
            let (r, _, outs) = render(&mut p, 64);
            r.unwrap();
            assert!(outs[0].iter().all(|x| *x == 7.5), "the published beat");
            assert_eq!(
                &outs[1][..8],
                &[96.0, 7.0, 1.0, 48_000.0, 3.0, 8.0, 6.0, 0.0],
                "the published block"
            );
            assert!(outs[1][8..].iter().all(|x| *x == 0.0));
            p.publish_transport(&au::HostTransport {
                ppq: 2.0,
                time_samples: 96_000,
                ..au::HostTransport::default()
            });
            let (r, _, outs) = render(&mut p, 64);
            r.unwrap();
            assert_eq!(
                &outs[1][..8],
                &[120.0, 7.0, 0.0, 96_000.0, 4.0, 4.0, 0.0, 0.0],
                "the fallbacks; stopped"
            );
            drop(p);
            drop(inst);
            assert_eq!(au::host_callbacks_clear_failures(), 0, "cleared at drop");
            images_clean();
        }
        // A render set's slot (`prepare_mode(…, offline: true)`) never gets kAudioUnitProperty_OfflineRender (a bounce
        // renders an AU as live). The offline fixture stores the property (the AudioUnitSDK's AUBase does not), so a
        // request would read back: it reads 0 after an offline prepare as after a live one, and renders the same bits.
        // The lab gain (no such property) still prepares.
        "offline" => {
            let mut live = component("summoner-lab-offline", b"SLof")
                .instantiate()
                .unwrap();
            assert_eq!(
                live.offline_render(),
                Some(false),
                "the fixture stores it: 0 to start"
            );
            let mut p = prepared(&mut live);
            assert_eq!(live.offline_render(), Some(false), "a live slot: not set");
            let (r, _, live_out) = render(&mut p, 256);
            r.unwrap();
            drop(p);
            let mut off = component("summoner-lab-offline", b"SLof")
                .instantiate()
                .unwrap();
            let mut q = off
                .prepare_mode(48000.0, 512, 2, 2, true)
                .expect("prepare for a render set");
            assert_eq!(
                off.offline_render(),
                Some(false),
                "a render set's AU slot: OfflineRender NOT set (a C++ host never sets it either)"
            );
            let (r, _, off_out) = render(&mut q, 256);
            r.unwrap();
            assert_eq!(off_out, live_out, "the same bits as live");
            drop(q);
            let mut gain = component("summoner-lab-gain", b"SLgn")
                .instantiate()
                .unwrap();
            let q = gain
                .prepare_mode(48000.0, 512, 2, 2, true)
                .expect("an AU without the property prepares for a render set");
            assert_eq!(
                gain.offline_render(),
                None,
                "the lab gain has no OfflineRender property"
            );
            drop(q);
            images_clean();
        }
        other => panic!("no scenario {other}"),
    }
    assert_eq!(
        au::wrong_thread_drops(),
        0,
        "no parameter write dropped as off its thread"
    );
    println!("AU-SCENARIO-OK {name}");
}

/// The child half: one scenario, chosen by the parent.
#[test]
fn au_child() {
    if let Ok(name) = std::env::var("SUMMONER_AU_SCENARIO") {
        scenario(&name);
    }
}

fn run(name: &str) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "au_child", "--nocapture", "--test-threads=1"])
        .env("SUMMONER_AU_SCENARIO", name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("AU-SCENARIO-OK {name}")),
        "{name}:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Is `CFPropertyListWrite`'s binary plist of the same state the same bytes in two processes? Measured, and the case
/// recorded (the `Q3: …` line). If not, the fallback must hold: the parsed dictionaries are equal (CFEqual) and the
/// `summoner-lab-state` bytes are identical, so state comparisons use those instead of the raw plist.
#[test]
fn q3_the_class_info_bytes_across_two_processes() {
    if lab_plugins::absent("q3_the_class_info_bytes_across_two_processes") {
        return;
    }
    let hex = |name: &str| -> Vec<u8> {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "au_child", "--nocapture", "--test-threads=1"])
            .env("SUMMONER_AU_SCENARIO", name)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let h = stdout
            .lines()
            .find_map(|l| l.find("STATE-HEX ").map(|i| l[i + 10..].trim().to_string()))
            .unwrap_or_else(|| panic!("{stdout}"));
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    };
    let (a, b) = (hex("state-bytes"), hex("state-bytes"));
    if a == b {
        println!(
            "Q3: the binary plist bytes are IDENTICAL across two processes ({} bytes)",
            a.len()
        );
    } else {
        println!(
            "Q3: the bytes DIFFER across processes ({} vs {} bytes): the fallback applies",
            a.len(),
            b.len()
        );
        assert_eq!(
            au::plist_equal(&a, &b),
            Some(true),
            "the fallback: equal dictionaries"
        );
        assert_eq!(
            au::plist_data(&a, "summoner-lab-state"),
            au::plist_data(&b, "summoner-lab-state"),
            "the fallback: the exact summoner-lab-state bytes"
        );
    }
    assert!(au::plist_data(&a, "summoner-lab-state").is_some_and(|c| c.starts_with(b"SASL")));
}

fn plugins() -> PathBuf {
    allowed_root(Format::Au).unwrap()
}

#[test]
fn a_render_sets_au_slot_never_gets_offline_render_and_renders_the_same_bits() {
    if lab_plugins::absent(
        "a_render_sets_au_slot_never_gets_offline_render_and_renders_the_same_bits",
    ) {
        return;
    }
    run("offline");
}

#[test]
fn the_lab_gain_renders_bit_exactly_with_state_and_clean_images() {
    if lab_plugins::absent("the_lab_gain_renders_bit_exactly_with_state_and_clean_images") {
        return;
    }
    assert!(plugins().join("summoner-lab-gain.component").is_dir());
    run("gain");
}

#[test]
fn a_stop_refuses_the_retry_and_every_later_load() {
    if lab_plugins::absent("a_stop_refuses_the_retry_and_every_later_load") {
        return;
    }
    run("retry");
}

#[test]
fn a_foreign_image_in_construction_or_initialize_is_a_stop() {
    if lab_plugins::absent("a_foreign_image_in_construction_or_initialize_is_a_stop") {
        return;
    }
    run("ctor");
    run("init");
}

#[test]
fn a_rewritten_output_list_or_a_render_status_is_a_failed_block() {
    if lab_plugins::absent("a_rewritten_output_list_or_a_render_status_is_a_failed_block") {
        return;
    }
    run("swap");
}

#[test]
fn an_au_refusing_listeners_is_known() {
    if lab_plugins::absent("an_au_refusing_listeners_is_known") {
        return;
    }
    run("deaf");
}

#[test]
fn input_pulls_follow_c3() {
    if lab_plugins::absent("input_pulls_follow_c3") {
        return;
    }
    run("pulls");
}

/// The parameter list's size edges: under the size interposer a size of 0 or 3 gives no parameter, 9 gives the two
/// whole ids, a returned size below the buffer keeps only the ids returned, and one above it keeps the buffer's;
/// without the interposer the params AU's whole list, ids 0 to 7 (the control).
#[test]
fn the_parameter_list_size_edges() {
    if lab_plugins::absent("the_parameter_list_size_edges") {
        return;
    }
    let lib = allowed_root(Format::Au)
        .unwrap()
        .parent()
        .unwrap()
        .join("libsummoner_lab_plsize.dylib");
    assert!(lib.is_file(), "{} (tools/build-plugins.sh)", lib.display());
    let ids = |mode: Option<&str>| -> String {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", "au_child", "--nocapture", "--test-threads=1"])
            .env("SUMMONER_AU_SCENARIO", "plsize");
        if let Some(m) = mode {
            c.env("DYLD_INSERT_LIBRARIES", &lib)
                .env("SUMMONER_LAB_PLSIZE", m);
        }
        let out = c.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            text.contains("AU-SCENARIO-OK plsize"),
            "{mode:?}:\n{text}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        text.lines()
            .find_map(|l| l.split_once("PLSIZE-IDS ").map(|(_, ids)| ids.to_string()))
            .unwrap_or_default()
    };
    let all = ids(None);
    assert_eq!(all, "[0, 1, 2, 3, 4, 5, 6, 7]", "the control");
    for (mode, want) in [
        ("zero", "[]"),
        ("three", "[]"),
        ("nine", "[0, 1]"),
        ("short", "[0]"),
        ("long", "[0, 1]"),
    ] {
        assert_eq!(ids(Some(mode)), want, "{mode}");
    }
}

/// A parameter name in the fixed `name` array (no CF names) decodes as its bytes up to
/// the first NUL, lossy UTF-8.
#[test]
fn the_lab_params_fixed_au_names_come_through_the_fixed_array() {
    if lab_plugins::absent("the_lab_params_fixed_au_names_come_through_the_fixed_array") {
        return;
    }
    run("c04fixed");
}

/// The lab params AU's names come through the CF-string path, caller-released.
#[test]
fn the_lab_params_au_names_come_through_cf_strings() {
    if lab_plugins::absent("the_lab_params_au_names_come_through_cf_strings") {
        return;
    }
    run("c04names");
}

/// The paramlist lab fixture: the ParameterList change it makes at its 8th block, and its encoding.
#[test]
fn the_paramlist_fixture_changes_its_list_at_block_8_and_says_so() {
    if lab_plugins::absent("the_paramlist_fixture_changes_its_list_at_block_8_and_says_so") {
        return;
    }
    run("paramlist");
}

/// The hostcb lab fixture through sys::au's HostCallbacks: the answers from the published block.
#[test]
fn the_hostcb_fixture_reads_the_host_callbacks_answers() {
    if lab_plugins::absent("the_hostcb_fixture_reads_the_host_callbacks_answers") {
        return;
    }
    run("hostcb");
}
