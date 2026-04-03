#!/usr/bin/env bash
set -euo pipefail

# Build macOS bundles for both Apple Silicon and Intel.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "[build-macos-dual-arch] This script must run on macOS."
  exit 1
fi

if ! command -v rustup >/dev/null 2>&1; then
  echo "[build-macos-dual-arch] rustup is required to add cross targets."
  exit 1
fi

echo "[build-macos-dual-arch] ensure frontend dependencies"
npm ci

echo "[build-macos-dual-arch] install rust targets"
rustup target add aarch64-apple-darwin x86_64-apple-darwin

build_target() {
  local target="$1"
  echo "[build-macos-dual-arch] building target: ${target}"
  npm run tauri:build -- --target "${target}"
}

build_target "aarch64-apple-darwin"
build_target "x86_64-apple-darwin"

RELEASE_DIR="$ROOT_DIR/release/macos-dual-arch"
rm -rf "$RELEASE_DIR"
mkdir -p "$RELEASE_DIR/aarch64" "$RELEASE_DIR/x86_64"

cp -R "src-tauri/target/aarch64-apple-darwin/release/bundle/." "$RELEASE_DIR/aarch64/"
cp -R "src-tauri/target/x86_64-apple-darwin/release/bundle/." "$RELEASE_DIR/x86_64/"

echo "[build-macos-dual-arch] done: $RELEASE_DIR"
