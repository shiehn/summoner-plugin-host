#!/usr/bin/env bash
# The path remapping of a release build: prints the rustc flags (one per line) that rewrite every build-machine
# prefix a release binary could hold (panic locations, file!() and debug paths) to a neutral one: this repo to
# /summoner, CARGO_HOME (registry and git checkouts) to /cargo, RUSTUP_HOME (the toolchain) to /rustup, and the home
# to /home-dir. Used by tools/build-shipped.sh and the release tests; tools/no-home-paths.sh checks the result.
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd -P)"
real_home=$(python3 -c 'import os, pwd; print(pwd.getpwuid(os.getuid()).pw_dir)')
cargo_home="${CARGO_HOME:-$real_home/.cargo}"
rustup_home="${RUSTUP_HOME:-$real_home/.rustup}"
# Longest prefix first: rustc applies the LAST matching flag, so the home goes first and the narrower ones after it.
for pair in "$real_home=/home-dir" "$cargo_home=/cargo" "$rustup_home=/rustup" "$repo=/summoner"; do
  echo "--remap-path-prefix=$pair"
done
