#!/bin/sh
# Explicit disposable hosting experiment; never changes the established public port.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
STATE="$ROOT/gateway/.local"
PREFIX=eshop-bootstrap
LEGACY_IMAGE=${LEGACY_IMAGE:-harbour-eshop:legacy}
GATEWAY_IMAGE=${GATEWAY_IMAGE:-eshop-gateway:bootstrap}

down() {
  for name in gateway legacy baseline rollback probe failure; do
    docker rm -f "$PREFIX-$name" >/dev/null 2>&1 || true
  done
  docker network rm "$PREFIX-net" >/dev/null 2>&1 || true
  for name in runtime baseline certs; do
    docker volume rm "$PREFIX-$name" >/dev/null 2>&1 || true
  done
  rm -rf "$STATE"
}

cert() {
  name=$1
  san=$2
  openssl req -new -newkey rsa:2048 -nodes -subj "/CN=$name" -keyout "$STATE/$name.key" -out "$STATE/$name.csr" 2>/dev/null
  printf 'subjectAltName=%s\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n' "$san" > "$STATE/$name.ext"
  openssl x509 -req -in "$STATE/$name.csr" -CA "$STATE/ca.crt" -CAkey "$STATE/ca.key" -CAcreateserial -days 2 -extfile "$STATE/$name.ext" -out "$STATE/$name.crt" 2>/dev/null
}

up() {
  if docker container inspect "$PREFIX-legacy" >/dev/null 2>&1; then
    echo 'Candidate already exists; use down first.' >&2
    exit 1
  fi
  mkdir -p "$STATE"
  chmod 700 "$STATE"
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=bootstrap-test-ca -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign -keyout "$STATE/ca.key" -out "$STATE/ca.crt" 2>/dev/null
  cert legacy 'DNS:legacy,DNS:fault,DNS:localhost,IP:127.0.0.1'
  cert public 'DNS:localhost,IP:127.0.0.1'
  chmod 644 "$STATE/public.key"
  docker network create "$PREFIX-net" >/dev/null
  docker volume create "$PREFIX-runtime" >/dev/null
  docker volume create "$PREFIX-certs" >/dev/null
  docker create --name "$PREFIX-legacy" --network "$PREFIX-net" --network-alias legacy -v "$PREFIX-runtime:/app" "$LEGACY_IMAGE" >/dev/null
  docker cp "$STATE/legacy.crt" "$PREFIX-legacy:/app/certificate.crt"
  docker cp "$STATE/legacy.key" "$PREFIX-legacy:/app/private.key"
  docker create --name "$PREFIX-probe" -v "$PREFIX-certs:/certs" --entrypoint /bin/true "$LEGACY_IMAGE" >/dev/null
  for file in public.crt public.key ca.crt; do docker cp "$STATE/$file" "$PREFIX-probe:/certs/$file"; done
  docker rm "$PREFIX-probe" >/dev/null
  docker start "$PREFIX-legacy" >/dev/null
  docker run -d --name "$PREFIX-gateway" --network "$PREFIX-net" -p 127.0.0.1:18002:8002 --mount "type=volume,src=$PREFIX-certs,dst=/certs,readonly" --env-file "$ROOT/gateway/config/example.env" "$GATEWAY_IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    if docker exec "$PREFIX-gateway" eshop-gateway check-ready >/dev/null 2>&1; then
      echo 'Candidate ready at https://localhost:18002 (test CA in gateway/.local/ca.crt)'
      return
    fi
    sleep 1
  done
  echo 'Candidate readiness failed' >&2
  return 1
}

case "${1:-}" in
  down) down ;;
  up) trap 'down' HUP INT TERM; trap 'down' EXIT; up; trap - EXIT ;;
  verify)
    trap 'down' EXIT HUP INT TERM
    docker build -t "$LEGACY_IMAGE" "$ROOT"
    docker build -t "$GATEWAY_IMAGE" "$ROOT/gateway"
    up
    docker volume create "$PREFIX-baseline" >/dev/null
    docker create --name "$PREFIX-baseline" --network "$PREFIX-net" -p 127.0.0.1:18003:8002 -v "$PREFIX-baseline:/app" "$LEGACY_IMAGE" >/dev/null
    docker cp "$STATE/legacy.crt" "$PREFIX-baseline:/app/certificate.crt"
    docker cp "$STATE/legacy.key" "$PREFIX-baseline:/app/private.key"
    docker start "$PREFIX-baseline" >/dev/null
    python3 "$ROOT/gateway/verification/compat.py" "$STATE/ca.crt" "$PREFIX" "$GATEWAY_IMAGE"
    echo 'Image provenance:'
    docker image inspect "$LEGACY_IMAGE" "$GATEWAY_IMAGE" --format '{{.Id}} {{json .RepoDigests}}'
    ;;
  *) echo 'usage: local.sh up | verify | down' >&2; exit 2 ;;
esac
