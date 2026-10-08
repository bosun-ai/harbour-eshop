#!/bin/sh
# Finite local-only checks; Python owns disposable resources and cleanup.
set -eu
cd "$(dirname "$0")/.."
umask 077
cargo build --locked --manifest-path gateway/Cargo.toml
cargo fmt --manifest-path gateway/Cargo.toml -- --check
cargo clippy --locked --manifest-path gateway/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path gateway/Cargo.toml
docker build -t harbour-eshop:boundary .
docker build -f gateway/Dockerfile -t eshop-gateway:bootstrap gateway
exec python3 scripts/check_boundary.py
