#!/usr/bin/env bash
# Refuses (exit 1) any file whose printable strings name a home directory (the macOS, Linux and Windows home roots): a
# release binary must not carry a build machine's paths (tools/release-remap.sh rewrites them). Prints each hit, cut to 200 columns.
#   tools/no-home-paths.sh <file>...
set -euo pipefail
[ $# -gt 0 ] || { echo "usage: tools/no-home-paths.sh <file>..." >&2; exit 2; }
# The home roots, assembled here so this script names none of them literally.
u=Users; h=home
pat="/$u/|/$h/|C:\\\\$u\\\\"
status=0
for f in "$@"; do
  [ -f "$f" ] || { echo "no-home-paths: $f is not a file" >&2; exit 2; }
  strs=$(strings -a "$f")
  if grep -qE "$pat" <<<"$strs"; then  # guard:home
    echo "no-home-paths: $f names a home directory:" >&2
    grep -E "$pat" <<<"$strs" | cut -c1-200 >&2
    status=1  # guard:home
  fi
done
exit $status
