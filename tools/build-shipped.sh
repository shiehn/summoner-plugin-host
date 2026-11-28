#!/bin/bash
# The shipped child, built and signed as it would ship, into target.noindex/shipped/dist/:
#   summoner-host            built WITHOUT SUMMONER_DEV_PLUGIN_ROOTS (no dev fence, no compiled-in build path),
#                            `release-loading`;
#   libsummoner_editor.dylib   beside it (the child loads it from there; a shipped build has no build-path fallback);
# both signed with the hardened runtime, the child with crates/summoner-host/summoner-host.entitlements (exactly two:
# allow-unsigned-executable-memory, disable-library-validation; the plist holds no comment, AMFI refuses them).
# Identity: SUMMONER_SIGN_IDENTITY, default "-" (ad hoc: the lab never uses a real identity; notarisation is the app's
# release). Run from the repo in the lab env (SUMMONER_VST3_SDK_CACHE names the verified SDK cache).
# The build goes to target.noindex/shipped (the shipped tests and the real-plugin drivers read it there);
# SUMMONER_SHIPPED_TARGET_DIR moves it, for a proof that must not share a build dir (one per toolchain).
# The editor dylib and the binary are the ones cargo reports for THIS build (its JSON messages: summoner-sys's
# build-script out_dir, summoner-host's executable), never a path guessed from the build-dir layout: cargo 1.100 moved
# it (target/<profile>/build/<pkg>/<hash>/out/, was .../build/<pkg>-<hash>/out/), and a glob over it can pick a stale
# hash. Missing, or more than one: the script stops.
set -euo pipefail
: "${SUMMONER_DEV_PLUGIN_ROOTS:?set SUMMONER_DEV_PLUGIN_ROOTS (the dev fence: checked absent from the binary)}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
cache="${SUMMONER_VST3_SDK_CACHE:?set SUMMONER_VST3_SDK_CACHE to the VST3 SDK fetch cache}"
identity="${SUMMONER_SIGN_IDENTITY:--}"
target="${SUMMONER_SHIPPED_TARGET_DIR:-$repo/target.noindex/shipped}"
mkdir -p "$target"
target="$(cd "$target" && pwd -P)"
msgs="$target/build-shipped.cargo.json"
# stdout is cargo's JSON messages only, one per line (diagnostics are rendered to stderr as before). A fresh (cached)
# unit reports its artifacts and its build script's out_dir too.
# Every build-machine path is remapped (tools/release-remap.sh), so the binary names no home directory
# (tools/no-home-paths.sh, below).
remap=$("$repo/tools/release-remap.sh" | paste -sd $'\x1f' -)
(cd "$repo" && env -u SUMMONER_DEV_PLUGIN_ROOTS SUMMONER_VST3_SDK_CACHE="$cache" CARGO_TARGET_DIR="$target" \
  CARGO_ENCODED_RUSTFLAGS="$remap" \
  nice -n 15 cargo build -q --release -p summoner-host --no-default-features --features release-loading \
  --message-format=json-render-diagnostics) > "$msgs"
# From the messages: summoner-sys's package id (its compiler-artifact, by its manifest), that id's ONE build
# script out_dir holding the dylib, and summoner-host's ONE executable, which must be $target/release/summoner-host
# (the path the other tools read). Prints "<dylib>" and "<bin>" on two lines, or names what is wrong and exits 1.
# The two packages' manifests, by package name through `cargo metadata` (never by directory name).
meta="$target/build-shipped.metadata.json"
(cd "$repo" && cargo metadata --offline --no-deps --format-version 1) > "$meta"
found=$(python3 -I - "$msgs" "$repo" "$target" "$meta" <<'PY'
import json, os, sys
msgs, repo, target, meta = sys.argv[1:5]
def die(why):
    sys.exit(f"build-shipped: {why} (cargo's messages: {msgs})")
with open(meta) as f:
    manifests = {p["name"]: os.path.realpath(p["manifest_path"]) for p in json.load(f)["packages"]}
for name in ("summoner-sys", "summoner-host"):
    if name not in manifests:
        die(f"cargo metadata lists no package {name}")
sys_manifest = manifests["summoner-sys"]
host_manifest = manifests["summoner-host"]
with open(msgs) as f:
    lines = [json.loads(l) for l in f if l.strip()]
sys_ids, exes = set(), []
for m in lines:
    if m.get("reason") != "compiler-artifact":
        continue
    mp = os.path.realpath(m.get("manifest_path", ""))
    if mp == sys_manifest:
        sys_ids.add(m["package_id"])
    elif mp == host_manifest and m["target"]["name"] == "summoner-host" and "bin" in m["target"]["kind"]:
        exes.append(m.get("executable"))
if len(sys_ids) != 1:
    die(f"expected one summoner-sys package in the build, found {len(sys_ids)}: {sorted(sys_ids)}")
out_dirs, editor_env = set(), []
for m in lines:
    if m.get("reason") == "build-script-executed" and m.get("package_id") in sys_ids:
        out_dirs.add(str(m.get("out_dir")))
        editor_env += [v for k, v in m.get("env", []) if k == "SUMMONER_EDITOR_DYLIB"]
if len(out_dirs) != 1:
    die(f"expected one build-script out_dir for summoner-sys, found {len(out_dirs)}: {sorted(out_dirs)}")
if editor_env != [""]:
    die(f"summoner-sys set SUMMONER_EDITOR_DYLIB to {editor_env!r}, not one empty value (a build path compiled in?)")
dylib = os.path.join(out_dirs.pop(), "libsummoner_editor.dylib")
if os.path.islink(dylib) or not os.path.isfile(dylib):
    die(f"the editor dylib is missing (or not a regular file): {dylib}")
if len(set(exes)) != 1 or not exes[0]:
    die(f"expected one summoner-host executable, found {sorted(set(map(str, exes)))}")
want = os.path.join(target, "release", "summoner-host")
if os.path.realpath(exes[0]) != os.path.realpath(want):
    die(f"cargo put summoner-host at {exes[0]}, not {want}")
if not (os.path.isfile(want) and os.access(want, os.X_OK)):
    die(f"the executable is missing: {want}")
print(dylib)
print(want)
PY
)
dylib=$(sed -n 1p <<<"$found")
host_manifest=$(python3 -I -c 'import json, sys; print([p["manifest_path"] for p in json.load(open(sys.argv[1]))["packages"] if p["name"] == "summoner-host"][0])' "$meta")
bin=$(sed -n 2p <<<"$found")
echo "build-shipped: editor dylib $dylib"
dist="$target/dist"
rm -rf "$dist"; mkdir -p "$dist"
cp "$bin" "$dist/summoner-host"
cp "$dylib" "$dist/libsummoner_editor.dylib"
chmod 755 "$dist/summoner-host"; chmod 644 "$dist/libsummoner_editor.dylib"
codesign --force --sign "$identity" --options runtime "$dist/libsummoner_editor.dylib"
codesign --force --sign "$identity" --options runtime \
  --entitlements "$(dirname "$host_manifest")/summoner-host.entitlements" "$dist/summoner-host"
# Checks: exactly the two files, both hardened, both entitlements, no lab path compiled in, no registry lookup or
# walk imported, the dylib resolved beside the child.
# Each tool's output captured first: under pipefail a `grep -q` that exits early SIGPIPEs its writer, and the pipe fails.
listing=$(cd "$dist" && ls -A)
[ "$listing" = "$(printf '%s\n' libsummoner_editor.dylib summoner-host)" ] || {
  echo "build-shipped: dist holds '$listing', not exactly the child and its editor dylib" >&2; exit 1; }
for f in summoner-host libsummoner_editor.dylib; do
  sig=$(codesign -dv "$dist/$f" 2>&1)
  grep -qE "flags=0x[0-9a-f]+\([^)]*runtime[^)]*\)" <<<"$sig" || { echo "build-shipped: $f not hardened" >&2; exit 1; }
done
ents=$(codesign -d --entitlements - "$dist/summoner-host" 2>/dev/null)
for e in allow-unsigned-executable-memory disable-library-validation; do
  grep -q "$e" <<<"$ents" || { echo "build-shipped: entitlement $e missing" >&2; exit 1; }
done
strs=$(strings -a "$dist/summoner-host")
if grep -qF "$SUMMONER_DEV_PLUGIN_ROOTS" <<<"$strs"; then
  echo "build-shipped: the dev fence path is in the binary:" >&2
  grep -F "$SUMMONER_DEV_PLUGIN_ROOTS" <<<"$strs" | cut -c1-200 >&2
  exit 1
fi
"$repo/tools/no-home-paths.sh" "$dist/summoner-host" "$dist/libsummoner_editor.dylib"
imports=$(nm -uj "$dist/summoner-host")
# The child has no AU registry lookup, so the shipped child imports none of the registry calls.
if grep -qE "AudioComponentFindNext|AudioComponentCount|AudioComponentCopyName" <<<"$imports"; then
  echo "build-shipped: a registry lookup or walk is imported" >&2; exit 1
fi
# The installed child resolves the dylib beside it (not a build path).
resolved=$("$dist/summoner-host" --editor-dylib-path)
[ "$resolved" = "$dist/libsummoner_editor.dylib" ] || { echo "build-shipped: the child resolves the editor dylib as '$resolved'" >&2; exit 1; }
shasum -a 256 "$dist/summoner-host" "$dist/libsummoner_editor.dylib"
echo "build-shipped: $dist (hardened, entitlements, identity $identity)"
