#!/bin/sh
# Native counter coverage using existing Rust instrumentation and Python only.
set -eu
cd "$(dirname "$0")/.."
directory="$PWD/gateway/coverage"
mkdir -p "$directory"
rm -f "$directory"/*.profraw
export RUSTFLAGS='-C instrument-coverage'
export CARGO_TARGET_DIR="$PWD/gateway/target/coverage"
export LLVM_PROFILE_FILE="$directory/unit-%p-%m.profraw"
cargo test --locked --manifest-path gateway/Cargo.toml
cargo build --locked --manifest-path gateway/Cargo.toml
export GATEWAY_BINARY="$CARGO_TARGET_DIR/debug/eshop-gateway"
export LLVM_PROFILE_FILE="$directory/live-%p-%m.profraw"
unset RUSTFLAGS CARGO_TARGET_DIR
sh verification/gateway.sh
python3 verification/coverage.py "$directory"
