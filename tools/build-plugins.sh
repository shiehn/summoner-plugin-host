#!/usr/bin/env bash
# The one command that builds the testkit's lab plugins (testkit/plugins) from source, used by CI and by the export
# proof alike: cmake + ninja at nice 15 into target.noindex/plugins, then their tests (ctest).
#
#   tools/build-plugins.sh           # the SDKs must already be fetched; they are verified against their pins
#   tools/build-plugins.sh --fetch   # first fetch a missing SDK by its pin (tools/fetch-vst3-subs.sh --superproject,
#                                    # tools/fetch-au-sdk.sh), then verify it the same way
#
# Needs SUMMONER_VST3_SDK_CACHE and SUMMONER_AU_SDK_CACHE (the fetch caches). Fails closed: an SDK that is missing (and
# not fetched), or not its pin, stops the build (tools/vst3-sdk.sh, tools/au-sdk.sh); nothing is skipped.
# Never installed anywhere: the hosts load the built bundles by absolute path from target.noindex/plugins (never a
# system plugin folder). The VST3 bundles land in target.noindex/plugins/vst3/, the AU v2 bundles in
# target.noindex/plugins/au/ (registered in-process by the host from there).
set -euo pipefail

fetch=0
case "${1:-}" in
  '') ;;
  --fetch) fetch=1 ;;
  *) echo "usage: tools/build-plugins.sh [--fetch]" >&2; exit 2 ;;
esac
root="$(cd "$(dirname "$0")/.." && pwd)"
build="$root/target.noindex/plugins"
for t in cmake ninja; do
  command -v "$t" >/dev/null || { echo "build-plugins: $t not found" >&2; exit 2; }
done
# The VST3 SDK, verified against testkit/plugins/vst3-sdk.pins (tools/vst3-sdk.sh refuses a tree that is not its pin);
# with --fetch, fetched by those pins first when the verification finds none.
if [ "$fetch" = 1 ] && ! "$root/tools/vst3-sdk.sh" >/dev/null 2>&1; then
  "$root/tools/fetch-vst3-subs.sh" --superproject
fi
sdk=$("$root/tools/vst3-sdk.sh")
# The AudioUnitSDK, verified against its pin (tools/au-sdk.sh refuses anything else); --fetch as above.
if [ "$fetch" = 1 ] && [ ! -e "${SUMMONER_AU_SDK_CACHE:?set SUMMONER_AU_SDK_CACHE}/AudioUnitSDK-1.4.0" ]; then
  "$root/tools/fetch-au-sdk.sh"
fi
ausdk=$("$root/tools/au-sdk.sh")
nice -n 15 cmake -S "$root/testkit/plugins" -B "$build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DSUMMONER_VST3_SDK_ROOT="$sdk" \
  -DSUMMONER_AU_SDK_ROOT="$ausdk" >/dev/null
nice -n 15 cmake --build "$build"
rm -f "$build/fswatch-selftest.log"  # the recorder appends; each ctest run checks a fresh log
(cd "$build" && nice -n 15 ctest --output-on-failure)
