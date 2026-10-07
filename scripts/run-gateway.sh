#!/bin/sh
# Explicit opt-in bootstrap. Existing docker run instructions are unaffected.
set -eu
if [ "$#" -ne 6 ] || [ "$1" != --activate ]; then
  echo 'usage: run-gateway.sh --activate LEGACY_IMAGE GATEWAY_IMAGE CONFIG CREDENTIAL_DIR RUNTIME_DIR' >&2
  exit 2
fi
legacy_image=$2 gateway_image=$3 config=$4 credentials=$5 runtime=$6
for image in "$legacy_image" "$gateway_image"; do
  case "$image" in sha256:*|*@sha256:*) ;; *) echo 'immutable image references required' >&2; exit 2;; esac
done
for path in "$config" "$credentials" "$runtime"; do
  case "$path" in /*) ;; *) echo 'absolute paths required' >&2; exit 2;; esac
done
test -f "$config"
for file in public.crt public.key upstream-ca.crt; do test -f "$credentials/$file"; done
for file in eshop private.key certificate.crt users.dbf items.dbf carts.dbf; do test -f "$runtime/$file"; done
test -x "$runtime/eshop"
# This host lock also prevents concurrent launchers; do not bypass it for rollback.
mkdir "$runtime/.gateway-writer-lock" || { echo 'runtime already reserved' >&2; exit 1; }
network="eshop-private-$$" legacy="eshop-legacy-$$" gateway="eshop-gateway-$$"
ingress="eshop-ingress-$$"
cleanup() {
  docker stop -t 12 "$gateway" "$legacy" >/dev/null 2>&1 || true
  docker rm -f "$gateway" "$legacy" >/dev/null 2>&1 || true
  docker network rm "$network" >/dev/null 2>&1 || true
  docker network rm "$ingress" >/dev/null 2>&1 || true
  if docker inspect "$legacy" >/dev/null 2>&1; then
    echo 'legacy removal not confirmed; runtime writer lock retained' >&2
  elif docker info >/dev/null 2>&1; then
    rmdir "$runtime/.gateway-writer-lock"
  else
    echo 'daemon unavailable; runtime writer lock retained' >&2
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker network create --internal "$network" >/dev/null
docker network create "$ingress" >/dev/null
docker run -d --name "$legacy" --network "$network" --network-alias legacy \
  --mount "type=bind,src=$runtime,dst=/app" "$legacy_image" >/dev/null
# No legacy publication. Gateway gets only config and public key/trust, never /app.
docker run -d --name "$gateway" --network "$network" -p 8002:8002 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --mount "type=bind,src=$config,dst=/config.toml,readonly" \
  --mount "type=bind,src=$credentials,dst=/credentials,readonly" \
  "$gateway_image" --config /config.toml >/dev/null
docker network connect "$ingress" "$gateway"
# Probe from the private namespace using the already selected legacy image.
ready=false
for attempt in $(seq 1 30); do
  if [ "$(docker inspect --format '{{.State.Running}}' "$gateway")" != true ]; then break; fi
  if docker run --rm --network "container:$gateway" --entrypoint bash "$legacy_image" -c \
    'exec 3<>/dev/tcp/127.0.0.1/8003; printf "GET /readyz HTTP/1.1\r\nHost: management\r\nConnection: close\r\n\r\n" >&3; IFS= read -r line <&3; [[ "$line" == *" 200 "* ]]' 2>/dev/null; then
    ready=true; break
  fi
  sleep 1
done
if [ "$ready" != true ]; then echo 'gateway readiness failed (TLS/reachability/configuration)' >&2; exit 1; fi
echo "ready: $gateway (Ctrl-C stops both containers; runtime retained)"
while [ "$(docker inspect --format '{{.State.Running}}' "$gateway")" = true ]; do sleep 1; done
