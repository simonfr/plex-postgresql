#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_PATH="${1:-$ROOT_DIR/include/plex_pg_core_ffi.h}"
ABI_CRATE_DIR="$ROOT_DIR/rust/plex-pg-abi"
CBINDGEN_BIN="${CBINDGEN:-}"

if [[ -z "$CBINDGEN_BIN" ]]; then
  CBINDGEN_BIN="$(command -v cbindgen 2>/dev/null || true)"
fi

if [[ -z "$CBINDGEN_BIN" && -x "$HOME/.cargo/bin/cbindgen" ]]; then
  CBINDGEN_BIN="$HOME/.cargo/bin/cbindgen"
fi

if [[ -z "$CBINDGEN_BIN" ]]; then
  echo "cbindgen is required to generate Rust FFI headers" >&2
  exit 1
fi

mkdir -p "$(dirname "$OUT_PATH")"

run_cbindgen() {
  "$CBINDGEN_BIN" \
    "$ABI_CRATE_DIR" \
    --config "$ROOT_DIR/cbindgen.toml" \
    --output "$OUT_PATH"
}

if ! run_cbindgen; then
  if [[ -z "${CARGO_NET_OFFLINE:-}" ]]; then
    echo "cbindgen metadata failed; retrying with CARGO_NET_OFFLINE=true" >&2
    CARGO_NET_OFFLINE=true run_cbindgen
  else
    echo "hint: run 'cargo metadata --manifest-path rust/plex-pg-abi/Cargo.toml --format-version 1' to inspect crate resolution" >&2
    exit 1
  fi
fi
