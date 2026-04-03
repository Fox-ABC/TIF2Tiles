#!/usr/bin/env bash
set -euo pipefail

# Build signed/unsigned release bundles from a clean dependency state.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "[build-release] install frontend dependencies"
npm ci

echo "[build-release] run tauri release build"
npm run tauri:build

# Keep release artifacts in a stable, shareable path outside Cargo temp dirs.
RELEASE_DIR="$ROOT_DIR/release"
rm -rf "$RELEASE_DIR"
mkdir -p "$RELEASE_DIR/macos" "$RELEASE_DIR/dmg"

cp -R "src-tauri/target/release/bundle/macos/." "$RELEASE_DIR/macos/"
cp -R "src-tauri/target/release/bundle/dmg/." "$RELEASE_DIR/dmg/"

echo "[build-release] done: $RELEASE_DIR"
