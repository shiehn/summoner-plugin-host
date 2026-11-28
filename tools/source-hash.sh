#!/usr/bin/env bash
# The child's source hash (a host application's end-to-end tests can refuse a child built from other sources than the
# tree under test). For this child the tree is this repo. One definition, used on both sides:
#   - crates/summoner-host/build.rs runs it on every build and embeds it; `summoner-host --source-hash` prints it;
#   - a host application's tests can name it in SUMMONER_HOST_SOURCE_HASH_CMD, so their stale-child guard compares
#     the child against this repo.
#
#   tools/source-hash.sh          # print the hash (64 hex digits)
#
# What is hashed: every file the binary is compiled from, by its path relative to the repo root and its sha256:
# Cargo.toml, Cargo.lock (it pins the host application's sha, so moving the pin changes the hash), rust-toolchain.toml,
# crates/ and testkit/plugins/vst3-sdk.pins (it pins the SDK the shim is compiled from); and the value of
# SUMMONER_DEV_PLUGIN_ROOTS, the fence summoner-sys compiles in (empty in a shipped build). Editor and Finder
# droppings are skipped. Not hashed: docs, tools other than this one, target/.
# Each listing line is `<hex>  <name>`, as macOS's shasum prints it (text mode). Git Bash's shasum on Windows prints
# binary mode, `<hex> *<name>`; the sed below rewrites that one form back, and is a no-op where shasum never prints it.
# crates/summoner-host/build_support/source_hash.rs is this script in Rust (Windows builds use it);
# tests/source_hash_rs.rs.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

hash=$(
  {
    {
      printf '%s\n' Cargo.toml Cargo.lock rust-toolchain.toml tools/source-hash.sh testkit/plugins/vst3-sdk.pins
      find crates -type f ! -name '.DS_Store' ! -name '*.swp' ! -name '*~'
    } | LC_ALL=C sort | tr '\n' '\0' | xargs -0 shasum -a 256 | sed 's/^\([0-9a-f]\{64\}\) \*/\1  /'
    printf 'SUMMONER_DEV_PLUGIN_ROOTS=%s\n' "${SUMMONER_DEV_PLUGIN_ROOTS:-}"
  } | shasum -a 256 | cut -d' ' -f1
)
[ ${#hash} -eq 64 ] || { echo "source-hash: no hash" >&2; exit 1; }
echo "$hash"
