#!/usr/bin/env bash
set -euo pipefail

# Bundle Homebrew GDAL runtime into src-tauri/resources for portable distribution.
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "[bundle-gdal] only macOS is supported by this script" >&2
  exit 1
fi

BREW_PREFIX="${BREW_PREFIX:-/opt/homebrew}"
RES_GDAL_DIR="$ROOT_DIR/src-tauri/resources/gdal"
BIN_DIR="$RES_GDAL_DIR/bin"
LIB_DIR="$RES_GDAL_DIR/lib"
SHARE_DIR="$RES_GDAL_DIR/share"
# 临时目录与历史「离线打包」目录解耦，避免与已废弃的 .offline/ 命名混淆
TMP_DIR="$ROOT_DIR/.tmp/bundle-gdal-macos"

mkdir -p "$BIN_DIR" "$LIB_DIR" "$SHARE_DIR" "$TMP_DIR"
chmod -R u+w "$RES_GDAL_DIR" 2>/dev/null || true
rm -rf "$BIN_DIR" "$LIB_DIR" "$SHARE_DIR"
mkdir -p "$BIN_DIR" "$LIB_DIR" "$SHARE_DIR"
rm -rf "$TMP_DIR"
mkdir -p "$TMP_DIR"

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "[bundle-gdal] missing command: $1" >&2
    exit 1
  fi
}

need_cmd otool
need_cmd install_name_tool
need_cmd rsync

copy_tool() {
  local tool="$1"
  local src_real
  src_real="$(python3 - <<PY
import os
print(os.path.realpath("$BREW_PREFIX/bin/$tool"))
PY
)"
  if [[ ! -f "$src_real" ]]; then
    echo "[bundle-gdal] tool not found: $tool" >&2
    exit 1
  fi
  cp "$src_real" "$BIN_DIR/$tool"
  chmod +x "$BIN_DIR/$tool"
}

# 固定一组最小可用工具，覆盖检查、重投影、打瓦片三类场景。
copy_tool "gdalinfo"
copy_tool "gdalwarp"
copy_tool "gdal_translate"
copy_tool "gdal2tiles"

# 保留 py 版本做兼容兜底；即使缺 python，也不影响 gdal2tiles 二进制路线。
if [[ -f "$BREW_PREFIX/bin/gdal2tiles.py" ]]; then
  cp "$(python3 - <<PY
import os
print(os.path.realpath("$BREW_PREFIX/bin/gdal2tiles.py"))
PY
)" "$BIN_DIR/gdal2tiles.py"
  chmod +x "$BIN_DIR/gdal2tiles.py"
fi

# 递归收集依赖：仅打包 Homebrew 前缀内的 dylib，系统库保留系统加载。
touch "$TMP_DIR/seen.txt"
queue_file() {
  local f="$1"
  if ! /usr/bin/grep -Fqx "$f" "$TMP_DIR/seen.txt"; then
    echo "$f" >> "$TMP_DIR/seen.txt"
    echo "$f" >> "$TMP_DIR/queue.txt"
  fi
}

> "$TMP_DIR/queue.txt"
for f in "$BIN_DIR/gdalinfo" "$BIN_DIR/gdalwarp" "$BIN_DIR/gdal_translate" "$BIN_DIR/gdal2tiles"; do
  queue_file "$f"
done

# gdal 可执行文件通常只指向 @rpath/libgdal，因此显式把核心库加入递归入口。
core_gdal_lib="$(python3 - <<PY
import glob, os
libs = sorted(glob.glob("$BREW_PREFIX/opt/gdal/lib/libgdal*.dylib"))
print(os.path.realpath(libs[0]) if libs else "")
PY
)"
if [[ -n "$core_gdal_lib" ]] && [[ -f "$core_gdal_lib" ]]; then
  cp -f "$core_gdal_lib" "$LIB_DIR/$(basename "$core_gdal_lib")"
  queue_file "$LIB_DIR/$(basename "$core_gdal_lib")"
fi

idx=1
while true; do
  target="$(sed -n "${idx}p" "$TMP_DIR/queue.txt")"
  [[ -z "$target" ]] && break
  while IFS= read -r dep; do
    [[ -z "$dep" ]] && continue
    if [[ "$dep" == "$BREW_PREFIX/"* ]] && [[ -f "$dep" ]]; then
      local_name="$(basename "$dep")"
      cp -f "$dep" "$LIB_DIR/$local_name"
      queue_file "$LIB_DIR/$local_name"
    fi
  done < <(otool -L "$target" | awk 'NR>1 {print $1}')
  idx=$((idx + 1))
done

# 将绝对依赖改写为 @rpath，避免用户机器必须存在 /opt/homebrew。
rewrite_refs() {
  local f="$1"
  local deps
  deps="$(otool -L "$f" | awk 'NR>1 {print $1}')"
  while IFS= read -r dep; do
    [[ -z "$dep" ]] && continue
    if [[ "$dep" == "$BREW_PREFIX/"* ]]; then
      install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$f" || true
    fi
  done <<< "$deps"
}

for f in "$LIB_DIR"/*.dylib; do
  [[ -f "$f" ]] || continue
  install_name_tool -id "@rpath/$(basename "$f")" "$f" || true
  install_name_tool -add_rpath "@loader_path" "$f" || true
  rewrite_refs "$f"
done

for f in "$BIN_DIR/gdalinfo" "$BIN_DIR/gdalwarp" "$BIN_DIR/gdal_translate" "$BIN_DIR/gdal2tiles"; do
  [[ -f "$f" ]] || continue
  file_desc="$(file "$f")"
  if [[ "$file_desc" != *"Mach-O"* ]]; then
    continue
  fi
  install_name_tool -add_rpath "@executable_path/../lib" "$f" || true
  rewrite_refs "$f"
done

# 复制数据目录，保证投影与坐标转换规则可离线读取。
rsync -a --delete "$BREW_PREFIX/share/gdal/" "$SHARE_DIR/gdal/"
rsync -a --delete "$BREW_PREFIX/share/proj/" "$SHARE_DIR/proj/"

echo "[bundle-gdal] bundled runtime at $RES_GDAL_DIR"
