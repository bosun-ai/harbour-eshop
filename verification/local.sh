#!/usr/bin/env bash
# Plain Docker only; all resources belong to this disposable run.
set -euo pipefail
export PYTHONDONTWRITEBYTECODE=1
cd "$(dirname "$0")/.."
prefix="eshop-boundary-$$"
temp=$(mktemp -d)
containers=()
volumes=()
networks=()
cleanup() {
  result=$?
  if (( result != 0 )); then echo "Boundary verification failed; resources will be removed" >&2; fi
  for name in "${containers[@]}"; do docker rm -f "$name" >/dev/null 2>&1 || true; done
  for name in "${volumes[@]}"; do docker volume rm "$name" >/dev/null 2>&1 || true; done
  for name in "${networks[@]}"; do docker network rm "$name" >/dev/null 2>&1 || true; done
  docker image rm "$prefix-legacy" "$prefix-gateway" >/dev/null 2>&1 || true
  rm -rf "$temp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
docker build -t "$prefix-legacy" .
docker build -t "$prefix-gateway" gateway/
for network in private ingress; do
  networks+=("$prefix-$network")
  options=(); [[ $network != private ]] || options+=(--internal)
  docker network create "${options[@]}" "$prefix-$network" >/dev/null
done
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=BoundaryCA -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' -keyout "$temp/ca.key" -out "$temp/ca.crt" >/dev/null 2>&1
for identity in legacy public; do
  openssl req -new -newkey rsa:2048 -nodes -subj "/CN=$identity" -keyout "$temp/$identity.key" -out "$temp/$identity.csr" >/dev/null 2>&1
  [[ $identity != legacy ]] && san='DNS:localhost,IP:127.0.0.1' || san='DNS:legacy,DNS:localhost,IP:127.0.0.1'
  printf 'subjectAltName=%s\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' "$san" > "$temp/extensions"
  openssl x509 -req -in "$temp/$identity.csr" -CA "$temp/ca.crt" -CAkey "$temp/ca.key" -CAcreateserial -days 1 -extfile "$temp/extensions" -out "$temp/$identity.crt" >/dev/null 2>&1
done
# Disposable keys only; make them readable by the non-root gateway image.
chmod 755 "$temp"
chmod 644 "$temp"/*.key
python3 verification/native.py --tls "$temp"
for identity in legacy public; do
  volumes+=("$prefix-$identity-tls")
  docker volume create "$prefix-$identity-tls" >/dev/null
  containers+=("$prefix-seed-$identity")
  docker create --name "$prefix-seed-$identity" -v "$prefix-$identity-tls:/tls" "$prefix-legacy" >/dev/null
  if [[ $identity == legacy ]]; then
    docker cp "$temp/legacy.key" "$prefix-seed-$identity:/tls/private.key"
    docker cp "$temp/legacy.crt" "$prefix-seed-$identity:/tls/certificate.crt"
  else
    for file in public.key public.crt ca.crt; do docker cp "$temp/$file" "$prefix-seed-$identity:/tls/$file"; done
  fi
  docker rm "$prefix-seed-$identity" >/dev/null
done
for instance in direct legacy; do
  volumes+=("$prefix-$instance-state")
  docker volume create "$prefix-$instance-state" >/dev/null
  docker run --rm --entrypoint sh -v "$prefix-$instance-state:/app" "$prefix-legacy" -c 'ln -sf /tls/private.key /app/private.key; ln -sf /tls/certificate.crt /app/certificate.crt'
  containers+=("$prefix-$instance")
  if [[ $instance == direct ]]; then
    # Baseline publication exists only for this disposable characterization instance.
    docker create --name "$prefix-direct" -p 127.0.0.1:18002:8002 -v "$prefix-direct-state:/app" -v "$prefix-legacy-tls:/tls:ro" "$prefix-legacy" >/dev/null
    docker start "$prefix-direct" >/dev/null
  else
    docker create --name "$prefix-legacy" --network "$prefix-private" --network-alias legacy -v "$prefix-legacy-state:/app" -v "$prefix-legacy-tls:/tls:ro" "$prefix-legacy" >/dev/null
    docker start "$prefix-legacy" >/dev/null
  fi
done
gateway_env=(-e PUBLIC_CERT_FILE=/tls/public.crt -e PUBLIC_KEY_FILE=/tls/public.key -e LEGACY_CA_FILE=/tls/ca.crt -e LEGACY_ORIGIN=https://legacy:8002 -e CONNECT_TIMEOUT_SECS=2 -e UPLOAD_TIMEOUT_SECS=3 -e RESPONSE_TIMEOUT_SECS=5 -e DRAIN_TIMEOUT_SECS=2)
containers+=("$prefix-gateway")
docker run -d --name "$prefix-gateway" --network "$prefix-ingress" -p 127.0.0.1:8002:8002 -v "$prefix-public-tls:/tls:ro" "${gateway_env[@]}" "$prefix-gateway" >/dev/null
docker network connect "$prefix-private" "$prefix-gateway"
wait_ready() {
  local deadline=$((SECONDS+45))
  until docker exec "$prefix-gateway" eshop-gateway healthcheck >/dev/null 2>&1 && curl --max-time 2 --cacert "$temp/ca.crt" -fs https://localhost:18002/hello >/dev/null; do
    (( SECONDS < deadline )) || { echo 'Readiness deadline exceeded' >&2; docker logs "$prefix-gateway"; docker exec "$prefix-gateway" eshop-gateway healthcheck; exit 1; }
    sleep 0.2
  done
}
wait_ready
docker inspect "$prefix-legacy" "$prefix-gateway" > "$temp/inspect.json"
python3 - "$temp/inspect.json" <<'PY'
import json, sys
legacy, gateway = json.load(open(sys.argv[1]))
assert not legacy['HostConfig']['PortBindings']
assert all(mount['Destination'] != '/app' for mount in gateway['Mounts'])
assert set(gateway['HostConfig']['PortBindings']) == {'8002/tcp'}
print('PASS: private Harbour, unpublished admin, no gateway /app mount')
PY
python3 verification/boundary.py compare --ca "$temp/ca.crt"
python3 verification/boundary.py transport --ca "$temp/ca.crt"
# Startup validation must fail, not start a listener.
if docker run --rm -v "$prefix-public-tls:/tls:ro" "${gateway_env[@]}" -e ENABLED_ROUTE_FAMILIES=unknown "$prefix-gateway"; then exit 1; fi
if docker run --rm "$prefix-gateway"; then exit 1; fi
docker stop -t 5 "$prefix-legacy" >/dev/null
python3 verification/boundary.py failure --ca "$temp/ca.crt"
if docker exec "$prefix-gateway" eshop-gateway healthcheck; then exit 1; fi
docker start "$prefix-legacy" >/dev/null
wait_ready
python3 verification/boundary.py restored --ca "$temp/ca.crt"
started=$SECONDS
docker kill --signal TERM "$prefix-gateway" >/dev/null
deadline=$((SECONDS+5))
until [[ $(docker inspect -f '{{.State.Running}}' "$prefix-gateway") == false ]]; do
  (( SECONDS < deadline )) || exit 1
  sleep 0.1
done
[[ $(docker inspect -f '{{.State.ExitCode}}' "$prefix-gateway") == 0 ]]
echo "PASS: SIGTERM drain $((SECONDS-started))s"
# Exclusive same-state rollback: remove the previous process before recreating.
docker rm "$prefix-gateway" >/dev/null
docker rm -f "$prefix-legacy" >/dev/null
docker run -d --name "$prefix-legacy" -p 127.0.0.1:8002:8002 -v "$prefix-legacy-state:/app" -v "$prefix-legacy-tls:/tls:ro" "$prefix-legacy" >/dev/null
deadline=$((SECONDS+30))
until curl --max-time 2 --cacert "$temp/ca.crt" -fs https://localhost:8002/hello >/dev/null 2>&1; do
  (( SECONDS < deadline )) || exit 1
  sleep 0.2
done
python3 verification/boundary.py rollback --ca "$temp/ca.crt"
echo 'PASS: boundary verification; cleanup follows'
