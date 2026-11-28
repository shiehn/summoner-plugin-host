#!/usr/bin/env bash
# Verify the fetched AudioUnitSDK tree against its pin (VENDORED.md) and print its root. Refuses (exit 1) when the tree
# is missing, its HEAD is not the pinned commit, or its LICENSE.txt is not the pinned sha256. Reads files only: no git
# command, nothing in the tree executed. Only its include/ and src/ are compiled (testkit/plugins/CMakeLists.txt).
set -euo pipefail

tree="${SUMMONER_AU_SDK_CACHE:?set SUMMONER_AU_SDK_CACHE to the AudioUnitSDK fetch cache}/AudioUnitSDK-1.4.0"
sha=bd98b31feff57a15989fcfab4cd86dc63382b1ac
lic=cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30

[ -f "$tree/.git/HEAD" ] || { echo "au-sdk: no fetched AudioUnitSDK-1.4.0 at $tree" >&2; exit 1; }
[ "$(cat "$tree/.git/HEAD")" = "$sha" ] || { echo "au-sdk: $tree HEAD is not $sha" >&2; exit 1; }
got=$(shasum -a 256 "$tree/LICENSE.txt" | cut -d' ' -f1)
[ "$got" = "$lic" ] || { echo "au-sdk: LICENSE.txt is $got, pinned $lic" >&2; exit 1; }
echo "$tree"
