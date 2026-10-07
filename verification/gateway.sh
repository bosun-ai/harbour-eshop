#!/bin/sh
# All runtime data is disposable. No bind mounts or captured legacy logs.
set -eu
cd "$(dirname "$0")/.."
LEGACY_IMAGE=${LEGACY_IMAGE:-harbour-eshop:gateway-check}
GATEWAY_IMAGE=${GATEWAY_IMAGE:-eshop-gateway:gateway-check}
export LEGACY_IMAGE GATEWAY_IMAGE
temporary=$(mktemp -d)
export CHECK_TMP="$temporary"
cleanup() {
  if [ -f "$temporary/resources" ]; then
    for category in container volume; do
      while read -r kind name; do
        if [ "$kind" = "$category" ]; then docker "$kind" rm -f "$name" >/dev/null 2>&1 || true; fi
      done < "$temporary/resources"
    done
  fi
  if [ -f "$temporary/network" ]; then docker network rm "$(cat "$temporary/network")" >/dev/null 2>&1 || true; fi
  rm -rf "$temporary"
}
trap cleanup EXIT HUP INT TERM
docker image inspect "$LEGACY_IMAGE" >/dev/null 2>&1 || docker build -t "$LEGACY_IMAGE" .
docker build -f gateway/Dockerfile -t "$GATEWAY_IMAGE" gateway
cargo build --locked --release --manifest-path gateway/Cargo.toml
python3 verification/compatibility.py
python3 verification/operations.py
