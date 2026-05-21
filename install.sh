#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")"

CONFIG="everscale_global_config_builder.json"
EXAMPLE_CONFIG="everscale_global_config_builder.example.json"
BINARY="target/release/everscale_global_config_builder"

if [ -f "${HOME}/.cargo/env" ]; then
  # shellcheck disable=SC1090
  . "${HOME}/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo not found. Install Rust first: https://rustup.rs/" >&2
  exit 1
fi

mkdir -p out

echo "rust: $(rustc --version)"
echo "building release binary"
cargo build --release

if [ ! -f "${CONFIG}" ]; then
  cp "${EXAMPLE_CONFIG}" "${CONFIG}"
  echo "created config: ${CONFIG}"
else
  echo "keeping existing config: ${CONFIG}"
fi

echo "installed"
echo "binary: ${PWD}/${BINARY}"
echo "config: ${PWD}/${CONFIG}"
echo
echo "edit seed_global_config_path if needed, then run:"
echo "${BINARY} build --config ${CONFIG}"
