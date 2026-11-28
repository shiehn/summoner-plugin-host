#!/usr/bin/env bash
# Verify the fetched VST3 SDK trees against testkit/plugins/vst3-sdk.pins and (re)make the include root the SDK's own
# #includes expect ("pluginterfaces/…", "base/…", "public.sdk/…"): $SUMMONER_VST3_SDK_CACHE/root/, three symlinks to
# the verified fetch directories. Prints the root. Refuses (exit 1) on a missing or unreadable pins file, a pins file
# that does not pin exactly the four trees (vst3sdk, pluginterfaces, base, public.sdk) once each, any missing tree, a
# HEAD that is not its pin, or a licence file whose sha256 is not its pin: it fails closed, never printing a root it
# did not verify. Reads files only: no git command, nothing executed in the trees. Run by the build before it uses
# the SDK.
# On Windows (Git Bash: MINGW/MSYS/CYGWIN), where `ln -s` silently copies without Developer Mode, the links are
# directory junctions (`mklink /J`), anything at a link's path that is not a link is refused (never deleted), and the
# root is printed as a Windows path (cygpath -w), which cl.exe can read. Elsewhere nothing changes. A Windows cargo
# build does not run this script: crates/summoner-sys/build_support/vst3_sdk.rs makes the same checks.
set -euo pipefail

case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) win=1 ;; *) win=0 ;; esac

root_dir="$(cd "$(dirname "$0")/.." && pwd)"
# The fetch cache (tools/fetch-vst3-subs.sh fills it): SUMMONER_VST3_SDK_CACHE, required. Every tree in it is
# verified against the pins below.
cache="${SUMMONER_VST3_SDK_CACHE:?set SUMMONER_VST3_SDK_CACHE to the VST3 SDK fetch cache (tools/fetch-vst3-subs.sh)}"
pins="$root_dir/testkit/plugins/vst3-sdk.pins"
# Fail closed: no pins file, or one that does not pin the four trees once each, is a refusal, never an empty loop.
[ -f "$pins" ] && [ -r "$pins" ] || { echo "vst3-sdk: no pins file $pins" >&2; exit 1; }  # guard:pins
want=$(printf '%s\n' base pluginterfaces public.sdk vst3sdk)
got=$(awk '$1 != "" && substr($1, 1, 1) != "#" { print $1 }' "$pins" | LC_ALL=C sort)
[ "$got" = "$want" ] || { echo "vst3-sdk: $pins must pin vst3sdk, pluginterfaces, base and public.sdk once each" >&2; exit 1; }  # guard:pins
out="$cache/root"
mkdir -p "$out"
while read -r name repo sha lic; do
  case "$name" in ''|'#'*) continue ;; esac
  [ -n "$sha" ] && [ -n "$lic" ] || { echo "vst3-sdk: a pins line without a commit and a licence hash: $name" >&2; exit 1; }
  [ "$name" = vst3sdk ] && suffix="" || suffix="-$name"
  dir=""
  for d in "$cache"/.fetch-*"$suffix"; do
    [ -d "$d/.git" ] || continue
    [ "$suffix" = "" ] && [[ "$d" =~ -(pluginterfaces|base|public\.sdk)$ ]] && continue
    [ "$(cat "$d/.git/HEAD")" = "$sha" ] && dir="$d"
  done
  [ -n "$dir" ] || { echo "vst3-sdk: no fetched $name at $sha under $cache (tools/fetch-vst3-subs.sh)" >&2; exit 1; }
  got=$(shasum -a 256 "$dir/LICENSE.txt" | cut -d' ' -f1)
  [ "$got" = "$lic" ] || { echo "vst3-sdk: $name LICENSE.txt is $got, pinned $lic" >&2; exit 1; }
  if [ "$win" = 1 ]; then
    [ "$name" = vst3sdk ] && continue
    link="$out/$name"
    if [ -L "$link" ]; then
      # A junction to another fetch: remove the junction itself (rmdir never touches its target).
      [ "$(cd "$link" && pwd -P)" = "$(cd "$dir" && pwd -P)" ] || cmd //c rmdir "$(cygpath -w "$link")"
    elif [ -e "$link" ]; then
      echo "vst3-sdk: $link exists and is not a link (a copied tree?); remove it by hand, it is not removed here" >&2
      exit 1
    fi
    [ -L "$link" ] || cmd //c mklink //J "$(cygpath -w "$link")" "$(cygpath -w "$dir")" > /dev/null
    [ -L "$link" ] && [ "$(cd "$link" && pwd -P)" = "$(cd "$dir" && pwd -P)" ] \
      || { echo "vst3-sdk: cannot make the junction $link -> $dir" >&2; exit 1; }
    continue
  fi
  [ "$name" = vst3sdk ] || ln -sfn "$dir" "$out/$name"
done < "$pins"
if [ "$win" = 1 ]; then cygpath -w "$out"; else echo "$out"; fi
