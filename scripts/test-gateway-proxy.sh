#!/bin/sh
# Exercises the all-proxy deployment shape without sharing Harbour's runtime directory.
set -eu

legacy_image=${LEGACY_IMAGE:-harbour-eshop:gateway-test}
gateway_image=${GATEWAY_IMAGE:-harbour-eshop-gateway:gateway-test}
network="harbour-gateway-test-$$"
legacy="harbour-gateway-legacy-$$"
gateway="harbour-gateway-public-$$"
secrets=$(mktemp -d)
cookies=$(mktemp)
headers=$(mktemp)

cleanup() {
  docker rm -f "$gateway" "$legacy" >/dev/null 2>&1 || true
  docker network rm "$network" >/dev/null 2>&1 || true
  rm -rf "$secrets" "$cookies" "$headers"
}
trap cleanup EXIT INT TERM

openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=legacy \
  -addext 'subjectAltName=DNS:legacy' -keyout "$secrets/legacy.key" -out "$secrets/legacy.crt" >/dev/null 2>&1
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=localhost \
  -addext 'subjectAltName=DNS:localhost' -keyout "$secrets/public.key" -out "$secrets/public.crt" >/dev/null 2>&1

docker network create "$network" >/dev/null
docker run -d --name "$legacy" --network "$network" --network-alias legacy \
  -v "$secrets/legacy.key:/app/private.key:ro" -v "$secrets/legacy.crt:/app/certificate.crt:ro" \
  "$legacy_image" >/dev/null
docker run -d --name "$gateway" --network "$network" -p 8002:8002 \
  -v "$secrets/public.key:/run/secrets/public-private.key:ro" \
  -v "$secrets/public.crt:/run/secrets/public-certificate.crt:ro" \
  -v "$secrets/legacy.crt:/run/secrets/legacy-certificate.crt:ro" \
  -e GATEWAY_BIND_ADDRESS=0.0.0.0:8002 \
  -e GATEWAY_PUBLIC_CERT_PEM=/run/secrets/public-certificate.crt \
  -e GATEWAY_PUBLIC_KEY_PEM=/run/secrets/public-private.key \
  -e GATEWAY_LEGACY_URL=https://legacy:8002 \
  -e GATEWAY_UPSTREAM_CA_PEM=/run/secrets/legacy-certificate.crt \
  -e GATEWAY_UPSTREAM_TLS_SERVER_NAME=legacy \
  -e GATEWAY_CONNECT_TIMEOUT_SECS=5 -e GATEWAY_REQUEST_TIMEOUT_SECS=30 \
  -e GATEWAY_RESPONSE_IDLE_TIMEOUT_SECS=30 -e GATEWAY_ENABLED_SLICES= \
  "$gateway_image" >/dev/null

for attempt in $(seq 1 45); do
  curl -skf https://localhost:8002/hello >/dev/null && break
  [ "$attempt" -eq 45 ] && { docker logs "$gateway"; docker logs "$legacy"; exit 1; }
  sleep 1
done

assert_equal() { [ "$1" = "$2" ] || { printf 'mismatch for %s\n' "$3" >&2; exit 1; }; }
direct() { docker run --rm --network "$network" curlimages/curl:8.12.1 -sk "https://legacy:8002$1"; }
direct_headers() { docker run --rm --network "$network" curlimages/curl:8.12.1 -skD - -o /dev/null "https://legacy:8002$1"; }
direct_status() { docker run --rm --network "$network" curlimages/curl:8.12.1 -sk -o /dev/null -w '%{http_code}' "https://legacy:8002$1"; }
assert_equal "$(direct /hello)" "$(curl -sk https://localhost:8002/hello)" /hello
assert_equal "$(direct /files/main.css)" "$(curl -sk https://localhost:8002/files/main.css)" /files/main.css
assert_equal "$(direct_status /hello)" "$(curl -sk -o /dev/null -w '%{http_code}' https://localhost:8002/hello)" '/hello status'
direct_location=$(direct_headers / | awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r')
gateway_location=$(curl -skD - -o /dev/null https://localhost:8002/ | awk 'tolower($1) == "location:" { print $2 }' | tr -d '\r')
assert_equal "$direct_location" "$gateway_location" '/ redirect'
assert_equal "$(direct_status /)" "$(curl -sk -o /dev/null -w '%{http_code}' https://localhost:8002/)" '/ status'

base=https://localhost:8002
curl -skD "$headers" -c "$cookies" -b "$cookies" -o /dev/null \
  -d 'user=gatewaytest&name=Gateway+Test&password1=secret&password2=secret&register=1' "$base/app/register"
grep -q '^HTTP/.* 302 ' "$headers"
grep -qi '^set-cookie:.*path=' "$headers"
curl -skf -c "$cookies" -b "$cookies" -o /dev/null -d 'user=gatewaytest&password=secret' "$base/app/login"
curl -skf -c "$cookies" -b "$cookies" -o /dev/null "$base/app/shopping?add=0001"
curl -skf -c "$cookies" -b "$cookies" -o /dev/null "$base/app/shopping?add=0001"
curl -skf -c "$cookies" -b "$cookies" "$base/app/cart" | grep -F '53.34' >/dev/null
printf '%s\n' 'gateway proxy compatibility check passed'
