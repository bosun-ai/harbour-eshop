#!/bin/sh
# eshop serves HTTPS only; create a throwaway self-signed certificate on first start.
set -eu
if [ ! -f private.key ] || [ ! -f certificate.crt ]; then
  openssl req -x509 -newkey rsa:2048 -nodes -days 730 \
    -subj "/CN=localhost" -keyout private.key -out certificate.crt 2>/dev/null
fi
exec ./eshop "$@"
