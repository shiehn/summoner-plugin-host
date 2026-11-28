![Summoner](../../docs/images/plugin-host.png)

# summoner-sys

**The host program's system calls, behind a safe API.**

`summoner-sys` is the crate that holds the host program's operating-system and plugin-SDK calls in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

Summoner runs third-party plugins in a helper process, the host program (`summoner-host`). Almost all of the host
program is safe Rust. The calls that cannot be (the kernel's process-exit note, the fatal-signal handler, the VST3 SDK
shim, AudioToolbox, the editor window glue, the audio workgroup) live here, one module per surface, behind owned
handles and plain values.

| Module | What it holds |
|---|---|
| `exit_watch` | `ExitWatch`: a process's exit from the kernel (kqueue `EVFILT_PROC` / `NOTE_EXIT`), registered once, so a reused pid is never mistaken for it; the host program's orphan watch |
| `crash` | the fatal-signal handler: a crash log, and a breadcrumb naming the plugin call in flight (`set_breadcrumb`) |
| `bundle` | the development fence (no `unsafe`): `Bundle::check` admits only bundles under the build's `SUMMONER_DEV_PLUGIN_ROOTS`, before anything of the bundle is opened and again after it is loaded |
| `images` / `dyld` | the loaded-image list the post-load check reads, and its system-image rule |
| `vst3` (feature `vst3`) | the VST3 host shim over the pinned SDK (fetched at build time, never copied in): `Module`, `Instance`, `Processor` |
| `au` (feature `au`) | Audio Unit v2 hosting through AudioToolbox, for bundles registered in this process only |
| `editor` (feature `editor`) | the plugin editor windows, in a dylib of their own loaded on first use |
| `qos` | the calling thread's QoS class, read back |
| `workgroup` | the audio workgroup (`os_workgroup` create, join, leave); groundwork, not wired yet |
| `proc_exit` | the immediate exit (`_exit`): no `atexit` handlers, no plugin's static destructors |
| `num`, `sha256`, `alloc_count` | strict `strtof`, SHA-256, and the allocation counter the no-allocation tests arm |

## How it fits

Below it: [`runecraft-rt`](https://signalsandsorcery.com/components/runecraft-audio-core/) (the realtime floor), the
C library and the macOS frameworks. Above it: the format crates ([`summoner-format-vst3`](../summoner-format-vst3/README.md),
[`summoner-format-au`](../summoner-format-au/README.md)), the scan probe ([`summoner-probe`](../summoner-probe/README.md))
and the host program ([`summoner-host`](../summoner-host/README.md)). Your application never links this crate: it
talks to the host program through the [`summoner`](../summoner/README.md) supervisor.

What it must not do: it decides nothing. Which plugin loads, when a block runs and what a failure means are the
callers' rules; this crate only makes the calls safe to make.

## Quick start

```rust
//! The safe API over the host program's system calls: watch a child process exit through the kernel (kqueue
//! `NOTE_EXIT`, so a reused pid is never mistaken for it), read the calling thread's QoS class, and take a SHA-256.
//! Run it with `cargo run -p summoner-sys --example summoner_sys_quickstart`.
use std::process::Command;
use std::time::Duration;

use summoner_sys::exit_watch::ExitWatch;

fn main() {
    let mut child = Command::new("/bin/sleep")
        .arg("0.2")
        .spawn()
        .expect("spawn /bin/sleep");
    // Registered once on this pid: the watch fires at the exit, before the child is reaped.
    let watch = ExitWatch::new(child.id()).expect("the exit watch");
    let gone = watch.wait(Some(Duration::from_secs(10))).expect("the wait");
    assert!(gone, "the child exits within the timeout");
    child.wait().expect("reap the child");

    let (qos, priority) = summoner_sys::qos::current().expect("this thread's QoS class");
    println!("the child exited; this thread runs at {qos:?} (relative priority {priority})");

    let digest = summoner_sys::sha256::hex(b"abc");
    // The well-known test vector: 64 hex digits, starting `ba7816bf`.
    assert!(digest.len() == 64 && digest.starts_with("ba7816bf"));
    println!("sha256(\"abc\") = {digest}");
}

#[test]
fn the_example_runs() {
    main();
}
```

```sh
cargo run -p summoner-sys --example summoner_sys_quickstart
```

## Realtime and safety

- This is one of the two crates of the repository that hold `unsafe` code (the other is
  [`summoner-sys-ui`](../summoner-sys-ui/README.md)); every other Summoner crate is `#![forbid(unsafe_code)]`.
  Each module is one FFI surface, and every `unsafe` block carries a `// SAFETY:` comment saying why it holds.
- Nothing here is called from your audio callback. The block call (`vst3::Processor`, the AU render) runs on the host
  program's own chain threads; a C++ exception thrown by a plugin's `process` is caught in the shim and returned as a
  failed block, never unwound through Rust.
- Plugin bundles are untrusted: only development bundles under the build's fence load at all, and the loaded-image
  list is checked after every load.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. This crate is macOS-only today (kqueue, mach, AudioToolbox, AppKit); its Windows and Linux bodies come with the host program's ports.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-sys-ui`](../summoner-sys-ui/README.md): the scan probe's AppKit surfaces, the other crate with `unsafe` code.
- [`summoner-host`](../summoner-host/README.md): the host program, the main user of this crate.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
