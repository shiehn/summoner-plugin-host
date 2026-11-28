#!/usr/bin/env bash
# Step 2 of the VST3 SDK's build-time fetch: the three submodules hosting needs,
# each into its own fresh directory under the cache, as its own repository, at the exact sha VENDORED.md pins. No
# `git submodule`, no hooks, no file:// transport, no recursion; nothing in the fetched trees is executed.
# Needs SUMMONER_VST3_SDK_CACHE (the fetch cache). Prints one line per tree: name, sha, HEAD, MATCH / MISMATCH, the directory.
#
#   tools/fetch-vst3-subs.sh                  # the three submodules (unchanged)
#   tools/fetch-vst3-subs.sh --superproject   # first the superproject vst3sdk, at its pin in testkit/plugins/vst3-sdk.pins,
#                                             # the same way (a fresh machine, e.g. Windows in Git Bash, has none)
set -euo pipefail

super=0
case "${1:-}" in
  '') ;;
  --superproject) super=1 ;;
  *) echo "usage: tools/fetch-vst3-subs.sh [--superproject]" >&2; exit 2 ;;
esac
cache="${SUMMONER_VST3_SDK_CACHE:?set SUMMONER_VST3_SDK_CACHE to the VST3 SDK fetch cache}"
mkdir -p "$cache"
ts=$(date -u +%Y%m%dT%H%M%SZ)
status=0
trees='pluginterfaces vst3_pluginterfaces 4f547e8e102b47de4a8b8aaf343c73b700786372
base vst3_base fcf9da0bd27a16f7f03773a3a39822f28f5c8477
public.sdk vst3_public_sdk 586dc5e6c8012c3e4b01c79389375cbe96bdb1da'
if [ "$super" = 1 ]; then
  pins="$(cd "$(dirname "$0")/.." && pwd)/testkit/plugins/vst3-sdk.pins"
  sp=$(awk '$1 == "vst3sdk" { print $1, $2, $3 }' "$pins")
  [ -n "$sp" ] || { echo "fetch-vst3-subs: no vst3sdk line in $pins" >&2; exit 1; }
  trees="$sp
$trees"
fi
while read -r name repo sha; do
  dir="$cache/.fetch-$ts-$name"
  [ ! -e "$dir" ] || { echo "fetch-vst3-subs: $dir exists" >&2; exit 1; }
  mkdir -p "$dir"
  g=(git -C "$dir" -c core.hooksPath=/dev/null -c protocol.file.allow=never -c advice.detachedHead=false)
  # Windows (Git Bash): the superproject's sample trees pass MAX_PATH; this repository's own git may use long paths.
  case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) g+=(-c core.longpaths=true) ;; esac
  "${g[@]}" init -q
  "${g[@]}" remote add origin "https://github.com/steinbergmedia/$repo.git"
  nice -n 15 "${g[@]}" fetch -q --depth 1 --no-recurse-submodules origin "$sha"
  "${g[@]}" checkout -q --detach "$sha"
  head=$("${g[@]}" rev-parse HEAD)
  if [ "$head" = "$sha" ]; then ok=MATCH; else ok=MISMATCH; status=1; fi
  echo "$name $sha $head $ok $dir"
done <<< "$trees"
exit $status
