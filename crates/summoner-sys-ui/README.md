![Summoner](../../docs/images/plugin-host.png)

# summoner-sys-ui

**The scan probe's AppKit and CoreGraphics surfaces, kept out of the host program.**

`summoner-sys-ui` is the crate that holds the few window-server calls a plugin scan may need in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A plugin scan loads untrusted code, and some plugins open windows or make themselves a regular application while
they load. The scan probe ([`summoner-probe-bin`](../summoner-probe-bin/README.md)) follows a no-UI rule: it never
makes an `NSApplication` and never lists windows, because each of those connects the process to the window server.
This crate holds the calls that would check those rules, apart from [`summoner-sys`](../summoner-sys/README.md), so
that nothing the host program links pulls AppKit in (a feature of `summoner-sys` would be unified into every build of
the workspace).

| Feature | Module | What it holds |
|---|---|---|
| `appkit` | `appkit` | the activation policy (`prohibit_activation`, `own_activation_policy`, `activation_policy_of`) and `frontmost_pid` |
| `nsapp` | `nsapp` | `existing_app_policy`: the policy of an `NSApplication` a plugin made, found without linking AppKit (draft) |
| `windows` | `windows` | `windows_of`: the windows a process owns (owner pid, layer, bounds only) |

Every feature is off by default, and nothing in the workspace enables them today: with all three off the crate has no
public API and links nothing, which is how the scan probe keeps its no-window-server rule.

## How it fits

Only the scan probe depends on it; the host program must never link it (the host program's release-build test
checks its link map). Below it: AppKit and CoreGraphics, loaded only when a feature asks for them.

## Quick start

The surfaces are opt-in by feature. In a crate of this workspace:

```toml
[dependencies]
summoner-sys-ui = { workspace = true, features = ["windows"] }
```

Then `summoner_sys_ui::windows::windows_of(pid)` lists the windows a process owns. Check what is linked with:

```sh
cargo build -p summoner-probe-bin && otool -L target/debug/summoner-scan-probe | grep -i appkit   # nothing
```

## Realtime and safety

- One of the two crates of the repository that hold `unsafe` code (with [`summoner-sys`](../summoner-sys/README.md)),
  under the same rules: one module per FFI surface, every `unsafe` block with a `// SAFETY:` comment. Every other
  Summoner crate is `#![forbid(unsafe_code)]`.
- Nothing here is realtime: it is for a scan process, never for a chain thread or your audio callback.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. This crate is macOS-only by nature (AppKit and CoreGraphics); other platforms will have their own equivalents if a scan needs them.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-probe-bin`](../summoner-probe-bin/README.md): the scan probe, its only user.
- [`summoner-sys`](../summoner-sys/README.md): the host program's system calls.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
