![Summoner](../../docs/images/plugin-host.png)

# summoner-probe-bin

**`summoner-scan-probe`: the scan process for one plugin bundle.**

`summoner-probe-bin` is the package of the scan process a plugin scan runs for each bundle in [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), and is part of **Signals & Sorcery**, an ecosystem of Rust audio
building blocks for building high-performance audio applications and DAWs
([signalsandsorcery.com](https://signalsandsorcery.com)).

## What it is

A small program that scans one bundle with [`summoner-probe`](../summoner-probe/README.md) and prints the report as
JSON on stdout: the protocol's `Scanned` (`format`, `path`, `outcome`, and a reason and detail when it failed), plus
the probe's extras. Its argument is checked exactly as the protocol checks a scan request.

| Exit | Meaning |
|---|---|
| 0 | a report was printed (`ok` or `failed`: the outcome is in the report) |
| 64 | not a scan the protocol could send (not an absolute `.vst3` / `.component` bundle path): nothing scanned |
| 75 | a step passed its deadline (`--step-ms`, default 25 000); stderr names the step |
| killed by a signal | the plugin crashed; the supervisor records it |

It keeps a no-UI rule: it never makes an `NSApplication` and never lists windows, so a scan makes no window-server
contact at all. It is a package of its own so that only it could link the AppKit surfaces of
[`summoner-sys-ui`](../summoner-sys-ui/README.md), never the host program.

## How it fits

The [`summoner-scan`](../summoner-scan/README.md) supervisor runs it, one process per bundle, in its own process
group and with a time budget; a crash or a timeout is the supervisor's verdict. The host program's `--scan` mode
will take its place once that mode speaks the full protocol.

## Quick start

```sh
tools/build-plugins.sh                                       # the testkit's plugins, once
cargo run -p summoner-probe-bin -- "$PWD/target.noindex/plugins/vst3/summoner-lab-gain.vst3"
```

```text
summoner-scan-probe <bundle.vst3|bundle.component> [--step-ms <n>]
```

Only development bundles (under the build's `SUMMONER_DEV_PLUGIN_ROOTS`) are scanned.

## Realtime and safety

- `#![forbid(unsafe_code)]`; the system calls are in [`summoner-sys`](../summoner-sys/README.md).
- A scan loads untrusted code, so it runs in a process of its own; nothing it does can reach your application's
  memory.

## Platform support

macOS (Apple silicon) today; Windows and Linux are in progress. This program is macOS-only today.

## Licence

MIT. See [LICENSE](../../LICENSE).

## Related

- [The Summoner repository README](../../README.md): the supervisor, the host program and how they fit.
- [`summoner-probe`](../summoner-probe/README.md): the scan itself.
- [`summoner-scan`](../summoner-scan/README.md): the supervisor that runs it.
- [Summoner](https://signalsandsorcery.com/components/summoner-plugin-host/), [Runecraft](https://signalsandsorcery.com/components/runecraft-audio-core/) and [Orrery](https://signalsandsorcery.com/components/orrery-musical-time/), the three code repositories, and [all components](https://signalsandsorcery.com/components/).
