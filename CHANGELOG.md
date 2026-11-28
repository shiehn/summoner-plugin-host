# Changelog

Summoner is pre-1.0: a minor release may change the public API. Each such change is listed here.

## 0.2.0

### Changed (breaking)

The host side of the protocol is now called "host" in every public name (it was "core"):

| 0.1 | 0.2 |
|---|---|
| `summoner_proto::codec::Direction::ToCore` | `Direction::ToHost` |
| `summoner_proto::codec::check_to_core` | `check_to_host` |
| `summoner_scan::scan::ScanEnd::CoreFault` | `ScanEnd::HostFault` |
| `summoner_scan::scan::ScanCounters::core_faults` | `ScanCounters::host_faults` |
| `summoner::supervisor::DownReason::Incompatible::core_protocol` | `Incompatible::host_protocol` |

- `summoner_rt::render::Fault::WaitFailed` and `RenderError::WaitFailed`: the render worker's wait for a block's ack
  failed at the OS level (`Waited::Failed`, Runecraft 0.2). The block fails and the exchange is spent, as for a
  timeout.

- `summoner_proto::codec::MAX_FRAME_FDS` is 4 on Windows (the hub set's section and a waiter's event ride with a
  frame there); 2 elsewhere, as before.

The renames are renames only: the wire bytes, the shared-memory layout and the protocol version (4.x) are unchanged, so a 0.1 child
and a 0.2 host (or the reverse) still speak to each other.

### Changed

- Messages: a few refusal and log texts now say "the host" and plain words where they cited internal design labels
  (the scan's over-limit, count and line limits; the `log.set` expiry rule; the AU code refusals).
- Documentation: comments and docs explain the design in words instead of citing internal design labels and
  documents; the realtime rules N1 to N12 are defined in Runecraft's `docs/realtime-rules.md`.
- The `summoner-proto` description no longer claims a C header generator: the crate ships the field tables a header
  can be generated from.

## 0.1.0

The first public release.
