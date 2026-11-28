# Third-party code

This repository vendors no third-party source. Two SDKs are fetched at build time, each at a pinned commit, and
verified before anything is compiled from them; the Rust dependencies come from crates.io through `Cargo.lock`.

## SDKs fetched at build time

| SDK | Repository | Pin | Licence | Used for |
|---|---|---|---|---|
| Steinberg VST 3 SDK 3.8.1 (build 84) | `github.com/steinbergmedia/vst3sdk` | `3cdf9ca5d1f5b1b21e0a86832aa4abe55607bd96` | MIT (`LICENSE.txt`, sha256 `e86d79e19e4a33ebc442d2e0d083887dfc75a2a8de9fe17eeddedb84810ad74e`) | VST3 hosting (the shim in `crates/summoner-sys/vst3`) and the testkit's VST3 plugins |
| Apple AudioUnitSDK 1.4.0 | `github.com/apple/AudioUnitSDK` | `bd98b31feff57a15989fcfab4cd86dc63382b1ac` | Apache-2.0 (`LICENSE.txt`, sha256 `cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30`) | the testkit's AU v2 plugins only (hosting uses AudioToolbox, a system framework) |

The VST3 SDK's submodules, fetched each as its own repository at the commit the superproject records:

| Path | Repository | Commit | Licence |
|---|---|---|---|
| `pluginterfaces` | `vst3_pluginterfaces` | `4f547e8e102b47de4a8b8aaf343c73b700786372` | MIT (the same `LICENSE.txt`) |
| `base` | `vst3_base` | `fcf9da0bd27a16f7f03773a3a39822f28f5c8477` | MIT (the same `LICENSE.txt`) |
| `public.sdk` | `vst3_public_sdk` | `586dc5e6c8012c3e4b01c79389375cbe96bdb1da` | MIT (the same `LICENSE.txt`) |

The other submodules (`vstgui4`, `cmake`, `doc`, `tutorials`) are never fetched.

**How.** `tools/build-plugins.sh --fetch` (and `tools/fetch-vst3-subs.sh --superproject`, `tools/fetch-au-sdk.sh`)
fetch each tree with `git fetch --depth 1` of the pinned commit into the caches named by `SUMMONER_VST3_SDK_CACHE` and
`SUMMONER_AU_SDK_CACHE`, with hooks off, no submodule recursion and no `file://` transport. `tools/vst3-sdk.sh` and
`tools/au-sdk.sh` (and, on Windows, `crates/summoner-sys/build_support/vst3_sdk.rs`) refuse any tree whose `HEAD` is not its
pin or whose licence file is not the pinned sha256, and refuse a pins file (`testkit/plugins/vst3-sdk.pins`) that does
not pin the four VST3 trees once each. Nothing in a fetched tree is executed: neither SDK's own build files are run,
and only the files named below are compiled.

**What of the VST3 SDK is compiled.**
- The host shim (`crates/summoner-sys/build.rs` `SDK_FILES`): `pluginterfaces/base/{coreiids,funknown,ustring}.cpp`;
  `public.sdk/source/common/{commoniids,commonstringconvert,memorystream}.cpp`; `public.sdk/source/vst/vstinitiids.cpp`;
  `public.sdk/source/vst/hosting/{eventlist,hostclasses,pluginterfacesupport}.cpp`;
  `public.sdk/source/vst/utility/stringconvert.cpp`. The SDK's macOS module loader (`module_mac.mm`, which walks the
  system plugin folders) is not compiled; the shim loads a bundle by absolute path only.
- The testkit's plugins (`testkit/plugins/CMakeLists.txt`): `pluginterfaces/base/{coreiids,funknown,ustring,
  conststringtable}.cpp`; `base/source/{baseiids,fobject,fstring,fdebug,fbuffer,fstreamer,updatehandler}.cpp`;
  `base/thread/source/{flock,fcondition}.cpp`; `public.sdk/source/common/{commoniids,pluginview}.cpp`;
  `public.sdk/source/main/{pluginfactory,moduleinit,macmain}.cpp`;
  `public.sdk/source/vst/{vstinitiids,vstaudioeffect,vstcomponent,vstcomponentbase,vsteditcontroller,vstbus,vstparameters}.cpp`.

In the compiled directories (`pluginterfaces/`, `base/`, `public.sdk/source/`) one file has another licence:
`public.sdk/source/vst/moduleinfo/json.h` (sheredom's json.h, Unlicense). It is used only by `moduleinfo/`, which is
not compiled. `public.sdk/samples/` holds BSD, zlib and public-domain files; it is never compiled.

**What of the AudioUnitSDK is compiled:** its `include/` and `src/AudioUnitSDK/` sources, into the testkit's AU
plugins only.

## Rust dependencies

From crates.io, every version pinned by `Cargo.lock`: the direct ones are `serde` and `serde_json` (MIT OR
Apache-2.0), `libc` (MIT OR Apache-2.0) and, build-only for the VST3 shim, `cc` (MIT OR Apache-2.0). The
`summoner-*`, `runecraft-*` and `orrery-*` crates are this project's own (MIT in their published repositories).

## The testkit's plugin identities

The testkit's lab plugins (`testkit/plugins`) are this project's own (MIT). They build as `summoner-lab-*.vst3` and
`summoner-lab-*.component`, named `Summoner Lab …`, vendor "Summoner Lab", with the recorders
`libsummoner_lab_{fswatch,lookwatch,malloccount,plsize}.dylib`; their VST3 class ids and AU codes (manufacturer
`SaSL`) are unchanged. Read-only, sha-pinned copies of these plugins made outside this repository before the names
changed keep the identities they were built with; the development gate accepts them only by their pinned bytes.
