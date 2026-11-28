# Changelog

Summoner is pre-1.0: a minor release may change the public API. Each such change is listed here.

## 0.4.0

### Added

- `summoner_rt::ChildPage::receive_hub_endpoints(slot, objs)`: the child's waiter endpoints on every ring group's
  hub in one call, on every platform, as `summoner_rt::HubEndpoints` (`#[non_exhaustive]`; `get(group)`, `len`),
  taking the chain load's objects (none on unix; on Windows the slot's event). The thread waits on the endpoint of
  the group it reads, so an unpark into another group needs nothing new.
- `summoner_rt::ChildPage::hub_waiter_objects(slot)`: the host side's objects for a chain's waiter slot (none on
  unix), for the chain load message.
- Windows: the host gives each chain a waiter slot unique on its child page (`chain.load`'s `fanSlot`) and refuses
  a chain past the page's 64 slots, loudly. On unix nothing changes: the hello and `chain.load` bytes are the same.

### Fixed

- The plugin host's latency handover (a VST3 that changes its latency at runtime and asks to be re-activated): the
  bypass signal now keeps the latency the processor had when the handover began, through the drain, the time out and
  the return fade. It used to move to the new latency at the return fade's first frame, where the bypass is at full
  level: an audible click, up to the signal's whole swing, on every such change.
- The same handover, twice in a row (a second latency change while the first handover's processor is out): the
  second handover's bypass plays at the latency the first one left, not at the first one's starting latency.
- The same handover on a disabled slot: its return now crossfades into the disabled slot's bypass at the new
  latency. It used to fade to the plugin's own (fresh, silent) output, after which the bypass cut in at once.

### Known issues

- A disabled AU slot whose latency changes moves its bypass to the new latency without a fade (AU latency changes
  run no handover). To be fixed in 0.5.

## 0.3.0

### Changed (breaking, for C users of a header generated from the field tables)

- `summoner_proto::layout::CHILD_PAGE_FIELDS`: the child page's reserved block at +512 (64 `u64` words) is named
  `reserved_host` (it was `reserved_core`). A rename only: its offset, its size, the page's layout and the wire bytes
  are unchanged, and no Rust item changed. C code that names the field must use the new name.

### Added

- `fuzz/`: a cargo-fuzz harness with two targets, `bridge_frame` (the frame codec, the admission gate, the typed
  metas and the scan's form B) and `state_blob` (the stored state-blob parser). It needs a nightly toolchain and
  `cargo-fuzz`; it is not a workspace member.

### Changed

- Documentation: `summoner_proto::codec::form_b` now says what the decoder does with a form B line holding an unknown
  key: it is accepted (the field dropped and noted, the protocol's additive rule) and only such a line skips the
  canonical-bytes check. The old text said it was refused. No behaviour change.

### Fixed

- `summoner-rt`: a render set's RAM admission works on Linux: it reads the child's resident size and the available
  memory there (Runecraft 0.3's Linux bodies), where every render set used to be refused.

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
