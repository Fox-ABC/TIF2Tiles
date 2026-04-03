#!/usr/bin/env bash
set -euo pipefail

# Assemble a redistributable kit for offline build machines.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

OFFLINE_NPM_CACHE="$ROOT_DIR/.offline/npm-cache"
OFFLINE_KIT_DIR="$ROOT_DIR/offline-dev-kit"
OFFLINE_PROJECT_DIR="$OFFLINE_KIT_DIR/project"

if [[ ! -d "$OFFLINE_NPM_CACHE" ]]; then
  echo "[prepare-offline-kit] npm cache not found at $OFFLINE_NPM_CACHE" >&2
  echo "[prepare-offline-kit] run: npm ci --cache .offline/npm-cache --prefer-offline" >&2
  exit 1
fi

if [[ ! -d "$ROOT_DIR/vendor" ]]; then
  echo "[prepare-offline-kit] Rust vendor directory not found at $ROOT_DIR/vendor" >&2
  echo "[prepare-offline-kit] run: cargo vendor vendor --manifest-path src-tauri/Cargo.toml" >&2
  echo "[prepare-offline-kit] and keep src-tauri/.cargo/config.toml directory=\"../vendor\"" >&2
  exit 1
fi

rm -rf "$OFFLINE_KIT_DIR"
mkdir -p "$OFFLINE_PROJECT_DIR"

# Copy source tree while excluding heavyweight or machine-local outputs.
rsync -a \
  --exclude ".git/" \
  --exclude "node_modules/" \
  --exclude "dist/" \
  --exclude "release/" \
  --exclude "offline-dev-kit/" \
  --exclude "src-tauri/target/" \
  "$ROOT_DIR/" "$OFFLINE_PROJECT_DIR/"

# Ensure offline caches are packaged with predictable paths.
mkdir -p "$OFFLINE_PROJECT_DIR/.offline"
mkdir -p "$OFFLINE_PROJECT_DIR/.offline/npm-cache"
cp -R "$OFFLINE_NPM_CACHE/." "$OFFLINE_PROJECT_DIR/.offline/npm-cache/"

# Include build artifacts for users that only need installation.
mkdir -p "$OFFLINE_KIT_DIR/release/macos" "$OFFLINE_KIT_DIR/release/dmg"
if [[ -d "$ROOT_DIR/src-tauri/target/release/bundle/macos" ]]; then
  cp -R "$ROOT_DIR/src-tauri/target/release/bundle/macos/." "$OFFLINE_KIT_DIR/release/macos/"
fi
if [[ -d "$ROOT_DIR/src-tauri/target/release/bundle/dmg" ]]; then
  cp -R "$ROOT_DIR/src-tauri/target/release/bundle/dmg/." "$OFFLINE_KIT_DIR/release/dmg/"
fi

# Write an entrypoint script so offline users have one stable command.
cat > "$OFFLINE_KIT_DIR/run-offline-build.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/project"
bash "./scripts/offline-install.sh"
npm run tauri:build
EOF
chmod +x "$OFFLINE_KIT_DIR/run-offline-build.sh"

echo "[prepare-offline-kit] done: $OFFLINE_KIT_DIR"
