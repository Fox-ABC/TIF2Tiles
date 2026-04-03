<!--
 * @Author: 王干
 * @Date: 2026-03-20 08:45:07
 * @LastEditors: 王干
 * @LastEditTime: 2026-03-20 08:46:22
 * @Description: file content
 * @FilePath: /GIFF地图切片工具/OFFLINE_GUIDE.md
-->
# OFFLINE GUIDE

本文档提供两种分发方式：

- 最终用户安装：直接使用 `release/` 产物，无需 Node/Rust/npm。
- 离线开发构建：使用 `offline-dev-kit/`，在无公网环境完成依赖恢复和打包。

## 1) 发布机（联网）准备

在项目根目录执行：

```bash
npm run tauri:build
npm ci --cache .offline/npm-cache --prefer-offline
cargo vendor vendor --manifest-path src-tauri/Cargo.toml
# 将 src-tauri/.cargo/config.toml 的 directory 改成 "../vendor"
npm run offline:prepare
```

执行完成后会得到：

- `offline-dev-kit/release/`：已构建好的 `.app/.dmg`（给最终用户）。
- `offline-dev-kit/project/`：可离线二次构建的完整工程（含 npm cache + vendor）。
- `offline-dev-kit/run-offline-build.sh`：离线构建入口脚本。

## 2) 最终用户（无需开发环境）

只分发 `release/` 或 `offline-dev-kit/release/` 即可：

- macOS：优先分发 `.dmg`。
- 用户安装后直接运行，不需要手动下载前端/Rust 依赖。

## 3) 离线开发机（需要重新构建）

把 `offline-dev-kit` 整体拷贝到离线机器，在其目录执行：

```bash
bash ./run-offline-build.sh
```

脚本会执行：

1. `project/scripts/offline-install.sh`
2. `npm run tauri:build`

其中 `offline-install.sh` 会强制使用本地缓存：

- npm：`npm ci --offline --cache .offline/npm-cache`
- cargo：`cargo check --offline`（由 `src-tauri/.cargo/config.toml` 指向 `vendor/`）

## 4) 关键目录说明

- `.offline/npm-cache/`：Node 依赖离线缓存。
- `vendor/`：Rust crates 本地镜像。
- `src-tauri/.cargo/config.toml`：Cargo 源替换配置（crates-io -> vendor）。

## 5) 注意事项

- `vendor/` 和 npm 缓存体积较大，建议压缩后分发。
- `src-tauri/target/` 不建议打包进离线包（体积大且强平台相关）。
- 最终用户机器上仍需 **自行安装 GDAL**（与联网开发机相同要求）；安装完成后在应用内点击「重新检测环境」确认。
- 自行安装 OSGeo4W、安装包类软件时，Windows 可能出现 UAC；使用 `brew install gdal` 时，macOS 可能提示输入密码，均属系统安装程序行为，与应用无关。
