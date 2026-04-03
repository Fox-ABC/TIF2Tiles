mod oss_upload;
mod gdal_pipeline;
mod progress_event;
mod tif_inspector;

use gdal_pipeline::{cancel_job as cancel_pipeline_job, run_tile_pipeline, JobState, TileJobRequest, TileJobResult};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, State};
use tif_inspector::{
    check_gdal_tools as check_tools_impl, detect_gdal_bin_dir as detect_gdal_bin_dir_impl,
    inspect_tif as inspect_tif_impl, preview_bounds_by_crs as preview_bounds_impl, BoundsPreview,
    GdalCheckResult, TifInfo,
};

#[tauri::command]
fn inspect_tif(path: String, gdal_bin_dir: Option<String>) -> Result<TifInfo, String> {
    // 解析 TIF 的关键信息，供前端展示图层边界与坐标系。
    let resolved_bin = gdal_bin_dir.or_else(detect_gdal_bin_dir_impl);
    inspect_tif_impl(&path, resolved_bin.as_deref())
}

#[tauri::command]
fn check_gdal_tools(
    gdal_bin_dir: Option<String>,
    gdal2tiles_cmd: Option<String>,
) -> Result<Vec<GdalCheckResult>, String> {
    // 在任务启动前自检依赖，避免切图执行到中途才失败。
    let resolved_bin = gdal_bin_dir.or_else(detect_gdal_bin_dir_impl);
    Ok(check_tools_impl(
        resolved_bin.as_deref(),
        gdal2tiles_cmd.as_deref(),
    ))
}

#[tauri::command]
fn detect_gdal_bin_dir() -> Option<String> {
    // 返回可用的默认 GDAL bin 路径，供前端自动填充到输入框。
    detect_gdal_bin_dir_impl()
}

#[tauri::command]
fn preview_bounds_by_crs(
    path: String,
    target_crs: String,
    gdal_bin_dir: Option<String>,
) -> Result<BoundsPreview, String> {
    // 坐标预览统一走后端计算，确保展示与生成链路使用同一套地理处理口径。
    let resolved_bin = gdal_bin_dir.or_else(detect_gdal_bin_dir_impl);
    preview_bounds_impl(&path, &target_crs, resolved_bin.as_deref())
}

#[tauri::command]
async fn run_tiling(
    app: AppHandle,
    state: State<'_, Arc<JobState>>,
    mut request: TileJobRequest,
) -> Result<TileJobResult, String> {
    // 将长耗时切图放入阻塞线程池，避免命令线程被持续占用导致界面卡顿。
    if request.gdal_bin_dir.is_none() {
        request.gdal_bin_dir = detect_gdal_bin_dir_impl();
    }
    let state_arc = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || run_tile_pipeline(app, state_arc, request))
        .await
        .map_err(|err| format!("切图任务线程异常: {err}"))?
}

#[tauri::command]
fn cancel_job(state: State<Arc<JobState>>, job_id: String) -> Result<bool, String> {
    // 取消操作支持幂等：任务不存在时返回 false 而不是报错。
    cancel_pipeline_job(state.inner().clone(), &job_id)
}

/// 与 `std::env::temp_dir()` 一致（macOS 多为 TMPDIR 下路径，而非固定 `/tmp`），供前端作为默认输出根目录展示。
#[tauri::command]
fn get_system_temp_dir() -> Result<String, String> {
    Ok(std::env::temp_dir().to_string_lossy().to_string())
}

/// 在 `parent` 下拼接单段子目录名，禁止传入含分隔符或 `..` 的片段，避免路径穿越。
#[tauri::command]
fn join_path(parent: String, child: String) -> Result<String, String> {
    let trimmed = child.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed == ".."
        || trimmed.contains("..")
    {
        return Err("非法子目录名".to_string());
    }
    let joined = PathBuf::from(parent).join(trimmed);
    Ok(joined.to_string_lossy().to_string())
}

#[tauri::command]
fn get_pending_deep_links(state: State<'_, Arc<Mutex<Vec<String>>>>) -> Result<Vec<String>, String> {
    // 前端可能晚于 setup 才开始监听事件；这里提供一次性拉取避免启动竞态丢参。
    let mut guard = state
        .lock()
        .map_err(|_| "读取 deep link 缓存失败：状态锁已损坏".to_string())?;
    Ok(std::mem::take(&mut *guard))
}

fn push_deep_links_to_frontend(
    app: &AppHandle,
    state: &Arc<Mutex<Vec<String>>>,
    urls: Vec<String>,
) -> Result<(), String> {
    // 所有来源（argv/RunEvent::Opened）统一写入缓存并广播，保证行为一致。
    if urls.is_empty() {
        return Ok(());
    }
    {
        let mut guard = state
            .lock()
            .map_err(|_| "写入 deep link 缓存失败：状态锁已损坏".to_string())?;
        guard.extend(urls.iter().cloned());
    }
    app.emit("deep-link-url", urls)
        .map_err(|err| format!("广播 deep link 事件失败: {err}"))?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let pending_deep_links = Arc::new(Mutex::new(Vec::<String>::new()));
    let app = tauri::Builder::default()
        .manage(Arc::new(JobState::default()))
        .manage(pending_deep_links.clone())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // 启动参数格式在不同浏览器/系统实现上可能略有差异，按 scheme 前缀统一放行。
            let urls: Vec<String> = std::env::args()
                .filter(|arg| arg.starts_with("xuntian-uploader:"))
                .collect();
            let app_handle = app.handle().clone();
            let state = app.state::<Arc<Mutex<Vec<String>>>>().inner().clone();
            let _ = push_deep_links_to_frontend(&app_handle, &state, urls);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            inspect_tif,
            check_gdal_tools,
            detect_gdal_bin_dir,
            preview_bounds_by_crs,
            run_tiling,
            cancel_job,
            get_system_temp_dir,
            join_path,
            get_pending_deep_links
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(move |app_handle, event| {
        // macOS 深链接主要通过 Opened 事件进入；这里补齐热唤起链路。
        if let tauri::RunEvent::Opened { urls } = event {
            let normalized_urls: Vec<String> = urls
                .iter()
                .map(|item| item.to_string())
                .filter(|item| item.starts_with("xuntian-uploader:"))
                .collect();
            let _ = push_deep_links_to_frontend(app_handle, &pending_deep_links, normalized_urls);
        }
    });
}
