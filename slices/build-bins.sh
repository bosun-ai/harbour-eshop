#!/bin/sh
# Cargo auto-discovers future src/bin/<id>.rs; the empty bootstrap has none.
set -eu
mkdir -p /packaged-slices
if [ -d src/bin ] && find src/bin -name '*.rs' -print | grep -q .; then
  cargo build --locked --release --bins
  for source in src/bin/*.rs; do
    [ -f "$source" ] || continue
    name=$(basename "$source" .rs)
    case "$name" in *[!a-z0-9_]*|'') echo "Invalid slice binary ID: $name" >&2; exit 1;; esac
    cp "target/release/$name" "/packaged-slices/$name"
  done
fi
