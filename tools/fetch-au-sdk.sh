#!/usr/bin/env bash
# Fetch Apple's AudioUnitSDK at the exact commit tools/au-sdk.sh pins, into $SUMMONER_AU_SDK_CACHE/AudioUnitSDK-1.4.0,
# as its own repository: no submodules, no hooks, no file:// transport; nothing in the tree is executed. Refuses a
# directory that already exists (tools/au-sdk.sh verifies what is there). Then runs tools/au-sdk.sh, so a fetch that
# is not the pin fails here.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cache="${SUMMONER_AU_SDK_CACHE:?set SUMMONER_AU_SDK_CACHE to the AudioUnitSDK fetch cache}"
# The pin, read from the verifier itself, so the two can never disagree.
sha=$(awk -F= '$1 == "sha" { print $2; exit }' "$here/au-sdk.sh")
[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || { echo "fetch-au-sdk: no 40-hex sha= line in tools/au-sdk.sh" >&2; exit 1; }
dir="$cache/AudioUnitSDK-1.4.0"
[ ! -e "$dir" ] || { echo "fetch-au-sdk: $dir exists" >&2; exit 1; }
mkdir -p "$dir"
g=(git -C "$dir" -c core.hooksPath=/dev/null -c protocol.file.allow=never -c advice.detachedHead=false)
"${g[@]}" init -q
"${g[@]}" remote add origin https://github.com/apple/AudioUnitSDK.git
nice -n 15 "${g[@]}" fetch -q --depth 1 --no-recurse-submodules origin "$sha"
"${g[@]}" checkout -q --detach "$sha"
"$here/au-sdk.sh" >/dev/null
echo "fetch-au-sdk: AudioUnitSDK $sha MATCH $dir"
