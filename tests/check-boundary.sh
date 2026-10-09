#!/bin/sh
# Temporary local containers only. Requires Docker, curl, timeout, and cmp.
set -eu
legacy=${1:-eshop-legacy-check}
optional=${2:-eshop-slices-check}
fixture=${3:-eshop-boundary-test}
prefix="eshop-check-$$"
scratch=$(mktemp -d)
containers=""
cleanup() {
  for container in $containers; do docker rm -f "$container" >/dev/null 2>&1 || true; done
  rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

native="$prefix-native"
containers="$containers $native"
timeout 45 docker run --name "$native" --entrypoint native-test "$fixture"

start() {
  name="$prefix-$1"
  containers="$containers $name"
  docker run -d --name "$name" -p 127.0.0.1::8002 "$2" >/dev/null
  port=$(docker port "$name" 8002/tcp | sed 's/.*://')
  base="https://127.0.0.1:$port"
  ready=false
  for attempt in $(seq 1 30); do
    if curl -skf --max-time 2 "$base/hello" > /dev/null; then ready=true; break; fi
    sleep 1
  done
  if [ "$ready" != true ]; then docker logs "$name"; exit 1; fi
}

capture() {
  label=$1
  method=$2
  path=$3
  curl -sk --max-time 4 -X "$method" -D "$scratch/headers" -o "$scratch/$label.body" "$base$path"
  # Ignore only listener-dependent headers. All other HTTP metadata must match.
  tr -d '\r' < "$scratch/headers" | grep -viE '^(Date:|Server:)' > "$scratch/$label.headers"
}

cases='GET:/hello POST:/hello GET:/hello?q=1 POST:/hello?q=1 GET:/hello/ GET:/hellox PUT:/hello HEAD:/hello GET:/ GET:/app/login GET:/files/main.css GET:/unknown'
start legacy "$legacy"
index=0
for item in $cases; do
  capture "baseline-$index" "${item%%:*}" "${item#*:}"
  index=$((index + 1))
done
printf 'Hello!' > "$scratch/legacy-body"
cmp "$scratch/baseline-0.body" "$scratch/legacy-body"
! grep -qi '^Set-Cookie:' "$scratch/baseline-0.headers"

for variant in optional disabled selected rollback partial hang stdout stderr missing; do
  image=$fixture
  [ "$variant" != optional ] || image=$optional
  start "$variant" "$image"
  if [ "$variant" = selected ] || [ "$variant" = rollback ] || [ "$variant" = partial ] || [ "$variant" = hang ] || [ "$variant" = stdout ] || [ "$variant" = stderr ] || [ "$variant" = missing ]; then
    # Restart via the same entrypoint with explicit configuration, never embedded in an image.
    docker rm -f "$name" >/dev/null
    mode=$variant
    [ "$variant" != selected ] || mode=''
    docker run -d --name "$name" -p "127.0.0.1:$port:8002" -e ESHOP_ENABLED_SLICE=hello -e "ESHOP_TEST_MODE=$mode" "$image" >/dev/null
    ready=false
    for attempt in $(seq 1 30); do
      if curl -skf --max-time 3 "$base/hello" >/dev/null; then ready=true; break; fi
      sleep 1
    done
    [ "$ready" = true ] || { docker logs "$name"; exit 1; }
    if [ "$variant" = rollback ]; then
      printf 'Rust fixture!' > "$scratch/rollback-before"
      curl -skf --max-time 3 "$base/hello" > "$scratch/rollback-active"
      cmp "$scratch/rollback-before" "$scratch/rollback-active"
    fi
    if [ "$variant" = missing ]; then docker exec "$name" rm /opt/eshop-slices/hello; fi
  fi
  if [ "$variant" = rollback ]; then
    docker rm -f "$name" >/dev/null
    docker run -d --name "$name" -p "127.0.0.1:$port:8002" -e ESHOP_ENABLED_SLICE= "$image" >/dev/null
    sleep 2
  fi
  index=0
  for item in $cases; do
    capture current "${item%%:*}" "${item#*:}"
    if [ "$variant" = selected ] && [ "$index" -lt 4 ]; then
      printf 'Rust fixture!' > "$scratch/selected-body"
      cmp "$scratch/current.body" "$scratch/selected-body"
      sed 's/^Content-Length:.*/Content-Length: BODY/' "$scratch/current.headers" > "$scratch/selected.headers"
      sed 's/^Content-Length:.*/Content-Length: BODY/' "$scratch/baseline-$index.headers" > "$scratch/reference.headers"
      cmp "$scratch/selected.headers" "$scratch/reference.headers"
    else
      cmp "$scratch/current.body" "$scratch/baseline-$index.body"
      cmp "$scratch/current.headers" "$scratch/baseline-$index.headers"
    fi
    index=$((index + 1))
  done
  echo "HTTP checks passed: $variant"
done

for variant in unknown unregistered unpackaged; do
  image=$fixture
  enabled=unknown
  if [ "$variant" = unregistered ]; then image=$optional; enabled=hello; fi
  name="$prefix-invalid-$variant"
  containers="$containers $name"
  docker create --name "$name" -e "ESHOP_ENABLED_SLICE=$enabled" "$image" >/dev/null
  if [ "$variant" = unpackaged ]; then
    # Preserve ownership but remove the packaged executable before startup.
    docker rm "$name" >/dev/null
    docker create --name "$name" -e ESHOP_ENABLED_SLICE=hello --entrypoint sh "$image" -c 'rm /opt/eshop-slices/hello; exec docker-entrypoint.sh' >/dev/null
  fi
  docker start "$name" >/dev/null
  status=$(timeout 10 docker wait "$name")
  [ "$status" = 1 ]
  docker logs "$name" | grep -q 'Slice configuration error:'
  echo "Startup rejection passed: $variant"
done
