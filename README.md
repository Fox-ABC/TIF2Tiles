<!--
 * @Author: 王干
 * @Date: 2026-03-19 16:54:20
 * @LastEditors: 王干
 * @LastEditTime: 2026-03-23 14:27:04
 * @Description: file content
 * @FilePath: /TIF2Tiles/README.md
-->
# GIFF TIF 瓦片工具

一个基于 **Tauri 2 + Rust + GDAL** 的桌面应用，用于导入 `.tif/.tiff` 后读取图层信息并生成瓦片目录。

## 已实现能力

- 导入 TIF 路径并读取元信息（坐标系、像素尺寸、左上右下坐标、波段数）
- 通过系统对话框选择输入 TIF 文件与输出目录
- 支持两种展示场景目标：
  - `高德地图展示（GCJ-02）`
  - `Mapbox + 天地图展示（WebMercator / EPSG:3857）`
- 执行三阶段 GDAL 流程并实时展示：
  - 阶段状态
  - 进度条
  - stdout/stderr 日志
- 支持取消正在运行的任务
- 选择 TIF 后自动读取图层信息，并按状态动态展示 GCJ 参数区

## 环境要求

- Node.js 18+
- Rust stable（含 Cargo）
- GDAL 命令行工具（需在本机自行安装；应用仅检测，不执行系统级安装）
- 需可用：`gdalinfo`、`gdalwarp`、`gdal_translate`、`gdal2tiles`（或 `gdal2tiles.py`）
- macOS 常见：`brew install gdal`；Windows 常见：OSGeo4W、QGIS 自带环境，或 conda-forge 安装 `gdal`
- 应用会探测 Homebrew/Conda/OSGeo4W 等常见目录；也可将工具加入 `PATH`

可执行检查：

```bash
node -v
npm -v
rustc -V
cargo -V
gdalinfo --version
```

## 开发运行

```bash
npm install
npm run tauri:dev
```

## 打包

```bash
npm run tauri:build
```

默认产物目录（本地环境）：

- `src-tauri/target/release/bundle/`

## 分发说明

- **最终用户**只需安装你打包好的应用（如 `.dmg` / `.msi` / `.exe`），**不需要**在本机安装 Node.js 或 Rust。
- **构建方**在联网环境执行 `npm install` 与 `npm run tauri:build` 即可；产物见 `src-tauri/target/release/bundle/`。

## GDAL 运行环境（需自行安装）

应用内「第一步：准备运行环境」会探测 GDAL，并提供：

- **重新检测环境**：检查 `gdalinfo` / `gdalwarp` / `gdal_translate` / `gdal2tiles` 是否可执行，并在界面展示明细

安装与使用顺序：

1. 按本机平台自行安装 GDAL（见上文「环境要求」）
2. 打开应用后点击 **重新检测环境**，全部通过后再进入切图流程

说明：应用不再内置「一键安装」或调用 `winget` / `brew` / `conda` 代为安装，以避免不同环境下安装失败或权限问题；若检测未通过，请根据界面中的错误明细排查路径与依赖。

## GCJ-02 切图说明

当目标坐标系选择 `GCJ-02（高德）` 时，应用会执行类似流程：

1. `gdalwarp -t_srs EPSG:4326`
2. `gdal_translate -a_srs EPSG:4326 -a_ullr ...`
3. `gdal2tiles.py -p mercator --xyz ...`

`a_ullr` 的左上右下坐标由界面输入，建议使用经过校核的范围值，避免与高德底图偏移。

左上右下坐标参数区会在以下条件满足时显示：

1. 已选择输入 TIF 文件
2. 图层信息自动读取成功

坐标区为只读展示，且会随 `目标瓦片用途` 切换动态计算：

- `高德地图展示`：展示按当前项目口径计算的 GCJ-02 边界（用于高德场景核对）
- `Mapbox + 天地图展示`：展示 WGS84（EPSG:4326）边界（用于经纬度定位核对）

场景选项与后端分支映射：

- `高德地图展示` -> `gcj02` 分支
- `Mapbox + 天地图展示` -> `webmercator` 分支

注意：`Mapbox + 天地图展示` 虽然任务分支走 `webmercator`，但界面展示坐标为 WGS84 经纬度，便于直接用于定位与人工校验。

当目标用途为 `高德地图展示` 时，任务会使用当前展示的边界参数执行切片流程。

## 磁盘空间检查策略（Hybrid）

- 默认开启 GDAL 磁盘空间检查，优先避免误写爆磁盘。
- 若出现 `Free disk space available ... at least necessary` 报错：
  1. 建议先降低缩放级别或缩小范围；
  2. 确认风险可控后，可勾选“本次任务强制继续（忽略磁盘空间检查）”重试。
- 日志会记录 `skipDiskSpaceCheck=true/false`，便于复核本次执行策略。

## KML 与中间文件策略

- 默认对 `gdal2tiles` 显式传入 `--no-kml`，不生成 KML 文件。
- 中间文件（如 `*_wgs84.tif`、`*_3857.tif`、`*_gcj_tagged.tif`）在任务成功后自动清理。
- 若任务失败，则保留中间文件用于排查；若成功但清理失败，日志会提示残留路径，可手动删除。
