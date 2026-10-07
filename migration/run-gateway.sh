#!/bin/sh
# Explicit activation only; never initializes or copies application state.
set -eu
if { [ "$#" -ne 6 ] || [ "$1" != "--activate-ingress" ]; } &&
   { [ "$#" -ne 5 ] || [ "$1" != "--activate-ingress-volumes" ]; }; then
  echo "usage: $0 --activate-ingress LEGACY_IMAGE GATEWAY_IMAGE RUNTIME CONFIG TLS_DIRECTORY" >&2
  echo "   or: $0 --activate-ingress-volumes LEGACY_IMAGE GATEWAY_IMAGE RUNTIME_VOLUME INPUT_VOLUME" >&2
  exit 2
fi
legacy_image=$2
gateway_image=$3
docker image inspect "$legacy_image" "$gateway_image" >/dev/null
if [ "$1" = "--activate-ingress-volumes" ]; then
  runtime=$4
  inputs=$5
  docker volume inspect "$runtime" "$inputs" >/dev/null
  runtime_mount="type=volume,src=$runtime,dst=/app"
  config_path=/inputs/gateway.toml
  docker run --rm --network none --read-only --mount "$runtime_mount,readonly" \
    --entrypoint /bin/sh "$legacy_image" -ec '
      for file in eshop users.dbf carts.dbf items.dbf private.key certificate.crt; do test -s /app/$file; done
      test -x /app/eshop && test -d /app/tpl && test -s /app/files/main.css'
else
  runtime=$(realpath "$4")
  config=$(realpath "$5")
  tls=$(realpath "$6")
  for file in eshop users.dbf carts.dbf items.dbf private.key certificate.crt; do
    test -s "$runtime/$file" || { echo "prepared runtime is incomplete" >&2; exit 2; }
  done
  test -x "$runtime/eshop"
  test -d "$runtime/tpl" && test -s "$runtime/files/main.css"
  test -f "$config" && test -d "$tls"
  runtime_mount="type=bind,src=$runtime,dst=/app"
  config_path=/gateway.toml
fi
# Refuse another Docker writer even if its container is currently stopped.
for container in $(docker ps -aq); do
  if docker inspect --format '{{range .Mounts}}{{println .Source}}{{println .Name}}{{end}}' "$container" | grep -Fxq "$runtime"; then
    echo "runtime already belongs to a container; quiesce and remove it first" >&2
    exit 2
  fi
done
prefix="eshop-ingress-$$"
network="$prefix-private"
ingress_network="$prefix-public"
legacy="$prefix-legacy"
gateway="$prefix-gateway"
cleanup() {
  docker stop -t 305 "$gateway" >/dev/null 2>&1 || true
  docker rm "$gateway" >/dev/null 2>&1 || true
  if docker inspect "$legacy" >/dev/null 2>&1; then
    docker exec "$legacy" ./eshop //stop >/dev/null 2>&1 || true
    docker stop -t 35 "$legacy" >/dev/null 2>&1 || true
    docker rm "$legacy" >/dev/null 2>&1 || true
  fi
  docker network rm "$network" >/dev/null 2>&1 || true
  docker network rm "$ingress_network" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT TERM
docker network create --internal "$network" >/dev/null
docker network create "$ingress_network" >/dev/null
docker run -d --name "$legacy" --network "$network" --network-alias legacy \
  --mount "$runtime_mount" "$legacy_image" >/dev/null
# Gateway gets only public TLS and upstream trust, never the Harbour runtime/key.
if [ "$1" = "--activate-ingress-volumes" ]; then
  set -- --mount "type=volume,src=$inputs,dst=/inputs,readonly"
else
  set -- --mount "type=bind,src=$config,dst=/gateway.toml,readonly" \
    --mount "type=bind,src=$tls,dst=/tls,readonly"
fi
docker run -d --name "$gateway" --network "$network" -p 8002:8002 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  "$@" "$gateway_image" --config "$config_path" >/dev/null
docker network connect "$ingress_network" "$gateway"
echo "Activated local ingress: $gateway; private writer: $legacy"
echo "Keep this command running. Interrupt to drain ingress and stop Harbour; runtime is retained."
code=$(docker wait "$gateway")
test "$code" = 0
