#!/usr/bin/env bash
set -euo pipefail

# Install project dependencies in offline environments from pre-bundled caches.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

NPM_CACHE_DIR="${NPM_CACHE_DIR:-$ROOT_DIR/.offline/npm-cache}"

if [[ ! -d "$NPM_CACHE_DIR" ]]; then
  echo "[offline-install] missing npm cache: $NPM_CACHE_DIR" >&2
  exit 1
fi

if [[ ! -d "$ROOT_DIR/vendor" ]]; then
  echo "[offline-install] missing Rust vendor directory: $ROOT_DIR/vendor" >&2
  exit 1
fi

# Force npm to use local cache and avoid network fallback in offline sites.
echo "[offline-install] npm ci --offline"
npm ci --offline --cache "$NPM_CACHE_DIR"

# Cargo reads src-tauri/.cargo/config.toml and resolves crates from vendor/.
echo "[offline-install] cargo check --offline"
cargo check --manifest-path "src-tauri/Cargo.toml" --offline

echo "[offline-install] dependencies restored from local offline assets"
