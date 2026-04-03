use crate::oss_upload::{
    new_upload_run_id, upload_root_prefix_preview, upload_tiles_to_oss, OssUploadConfig,
    OssUploadSummary, UploadFailure,
};
use crate::progress_event::ProgressEvent;
use crate::tif_inspector::{build_command, detect_gdal_bin_dir, inspect_tif};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcjBounds {
    pub ulx: f64,
    pub uly: f64,
    pub lrx: f64,
    pub lry: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TileJobRequest {
    pub job_id: String,
    pub input_path: String,
    pub output_dir: String,
    pub working_dir: Option<String>,
    pub zoom_min: u8,
    pub zoom_max: u8,
    pub resampling: Option<String>,
    pub target_tile_crs: String,
    #[serde(default)]
    pub skip_disk_space_check: bool,
    pub gcj_bounds: Option<GcjBounds>,
    pub gdal_bin_dir: Option<String>,
    pub gdal2tiles_cmd: Option<String>,
    #[serde(default)]
    pub upload_config: Option<OssUploadConfig>,
    #[serde(default)]
    pub upload_context: Option<UploadContext>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadContext {
    pub tenant_id: String,
    pub ts: String,
    pub platform: String,
    pub layer_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TileJobResult {
    pub job_id: String,
    pub output_dir: String,
    pub tile_template: String,
    pub generated_intermediate_files: Vec<String>,
    pub upload_summary: Option<OssUploadSummary>,
}

#[derive(Default)]
pub struct JobState {
    pub jobs: Mutex<HashMap<String, Arc<JobControl>>>,
}

pub struct JobControl {
    /// 与 OSS 上传等后处理共享：切图结束后任务仍注册在表中，直到上传完成，便于取消上传。
    pub cancelled: Arc<AtomicBool>,
    pub current_pid: Mutex<Option<u32>>,
    pub working_dir: Option<String>,
    pub output_dir: Option<String>,
}

impl Default for JobControl {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            current_pid: Mutex::new(None),
            working_dir: None,
            output_dir: None,
        }
    }
}

#[derive(Clone)]
struct StageSpec {
    stage: &'static str,
    progress_start: u8,
    progress_span: u8,
    command: String,
    args: Vec<String>,
}

#[derive(Debug, Clone)]
struct TileOutputFormat {
    extension: &'static str,
    tile_driver: &'static str,
}

// 管道主入口：按坐标系策略构建阶段并串行执行，确保每一步失败都可追踪。
pub fn run_tile_pipeline(
    app: AppHandle,
    state: Arc<JobState>,
    mut req: TileJobRequest,
) -> Result<TileJobResult, String> {
    // 输出目录为空时回退到系统临时目录下的唯一子目录，与前端「默认系统临时」策略一致。
    let auto_output_dir = req.output_dir.trim().is_empty();
    if auto_output_dir {
        req.output_dir = allocate_default_output_dir()?;
    }

    validate_request(&req)?;
    ensure_dir_exists(&req.output_dir)?;

    let mut control = JobControl::default();
    control.working_dir = req.working_dir.clone();
    control.output_dir = Some(req.output_dir.clone());
    let control = Arc::new(control);
    {
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|_| "任务状态锁已损坏".to_string())?;
        if jobs.contains_key(&req.job_id) {
            return Err(format!("任务ID已存在: {}", req.job_id));
        }
        jobs.insert(req.job_id.clone(), control.clone());
    }

    let mut intermediates = Vec::<String>::new();
    emit_event(
        &app,
        ProgressEvent::new(&req.job_id, "prepare", "info", "开始执行切图任务", 0),
    )?;

    let (stages, tile_format) = build_stages(&req, &mut intermediates)?;
    let result = stages.iter().try_for_each(|spec| {
        run_stage(
            &app,
            &req.job_id,
            control.clone(),
            spec.clone(),
            req.working_dir.as_deref(),
            req.gdal_bin_dir.as_deref(),
        )
    });

    if let Err(e) = result {
        let mut jobs = state
            .jobs
            .lock()
            .map_err(|_| "任务状态锁已损坏".to_string())?;
        jobs.remove(&req.job_id);
        return Err(e);
    }

    // 仅在 GDAL 阶段成功后清理中间文件；失败场景在上面已返回并保留现场。
    let mut retained_intermediates = cleanup_intermediates(&app, &req.job_id, &intermediates);

    // 上传阶段仍持有任务注册，便于 cancel_job 终止上传；无论上传成败都要 remove，避免任务 ID 泄漏。
    let upload_result = maybe_upload_tiles(&app, &req, Some(control.cancelled.clone()));
    let mut jobs = state
        .jobs
        .lock()
        .map_err(|_| "任务状态锁已损坏".to_string())?;
    jobs.remove(&req.job_id);
    drop(jobs);
    let upload_summary = upload_result?;

    // 仅在 OSS 上传开启且本次输出目录由系统自动分配时清理整包瓦片，避免误删用户自定义目录。
    if auto_output_dir {
        retained_intermediates.extend(cleanup_output_tiles(
            &app,
            &req.job_id,
            &req.output_dir,
            req.upload_config.as_ref(),
        ));
    }

    emit_event(
        &app,
        ProgressEvent::new(&req.job_id, "done", "success", "瓦片生成完成", 100),
    )?;

    Ok(TileJobResult {
        job_id: req.job_id,
        output_dir: req.output_dir.clone(),
        tile_template: format!(
            "{}/{{z}}/{{x}}/{{y}}.{}",
            req.output_dir, tile_format.extension
        ),
        // 返回最终仍保留的中间文件（通常为空；清理失败时用于提示用户手动处理）。
        generated_intermediate_files: retained_intermediates,
        upload_summary,
    })
}

// 取消任务时优先设置取消标记，再尝试杀掉正在运行的子进程，然后清理相关文件。
pub fn cancel_job(state: Arc<JobState>, job_id: &str) -> Result<bool, String> {
    let (control, working_dir, output_dir) = {
        let jobs = state
            .jobs
            .lock()
            .map_err(|_| "任务状态锁已损坏".to_string())?;
        let control = match jobs.get(job_id) {
            Some(job) => job.clone(),
            None => return Ok(false),
        };
        let working_dir = control.working_dir.clone();
        let output_dir = control.output_dir.clone();
        (control, working_dir, output_dir)
    };

    control.cancelled.store(true, Ordering::Relaxed);

    let current_pid = {
        let pid_guard = control
            .current_pid
            .lock()
            .map_err(|_| "任务进程锁已损坏".to_string())?;
        *pid_guard
    };

    if let Some(pid) = current_pid {
        let _ = kill_process(pid);
    }

    // 清理工作目录和输出目录
    if let Some(dir) = working_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }
    if let Some(dir) = output_dir {
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 从任务列表中移除
    let mut jobs = state
        .jobs
        .lock()
        .map_err(|_| "任务状态锁已损坏".to_string())?;
    jobs.remove(job_id);

    Ok(true)
}

fn build_stages(
    req: &TileJobRequest,
    intermediates: &mut Vec<String>,
) -> Result<(Vec<StageSpec>, TileOutputFormat), String> {
    // 根据目标坐标系动态拼装命令链，保证每种策略都可复用统一执行器。
    let input_name = Path::new(&req.input_path)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("input");

    let work_dir = req
        .working_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&req.output_dir));
    ensure_dir_exists(work_dir.to_string_lossy().as_ref())?;

    // 中间重投影/改地理参考一律输出 VRT：只保存 XML 与源路径引用，不落盘整幅 materialized GeoTIFF，显著降低本地磁盘占用。
    let wgs_path = work_dir.join(format!("{input_name}_wgs84.vrt"));
    let mercator_path = work_dir.join(format!("{input_name}_3857.vrt"));
    let gcj_path = work_dir.join(format!("{input_name}_gcj_tagged.vrt"));

    let wgs_path_str = wgs_path.to_string_lossy().to_string();
    let mercator_path_str = mercator_path.to_string_lossy().to_string();
    let gcj_path_str = gcj_path.to_string_lossy().to_string();

    let mut stages = Vec::<StageSpec>::new();
    // 默认优先使用 gdal2tiles 二进制，找不到时回退 gdal2tiles.py。
    let gdal2tiles = req.gdal2tiles_cmd.clone().unwrap_or_else(|| {
        let detected = req.gdal_bin_dir.clone().or_else(detect_gdal_bin_dir);
        let mut probe = build_command("gdal2tiles", detected.as_deref());
        match probe.arg("--help").output() {
            Ok(output) if output.status.success() => "gdal2tiles".to_string(),
            _ => "gdal2tiles.py".to_string(),
        }
    });
    let resampling = req
        .resampling
        .clone()
        .unwrap_or_else(|| "average".to_string());
    let tile_format = resolve_tile_output_format(&gdal2tiles, req.gdal_bin_dir.as_deref())?;

    match req.target_tile_crs.as_str() {
        "wgs84" => {
            // 当输入本身已是 WGS84 时直接切片，避免生成额外中间 tif 占用磁盘。
            let source_is_wgs84 = is_source_wgs84(req)?;
            let tile_input = if source_is_wgs84 {
                req.input_path.clone()
            } else {
                intermediates.push(wgs_path_str.clone());
                stages.push(StageSpec {
                    stage: "warp_wgs84",
                    progress_start: 5,
                    progress_span: 35,
                    command: "gdalwarp".to_string(),
                    args: build_warp_args(
                        req,
                        "EPSG:4326",
                        req.input_path.clone(),
                        wgs_path_str.clone(),
                    ),
                });
                wgs_path_str.clone()
            };
            stages.push(StageSpec {
                stage: "tiles_wgs84",
                progress_start: 40,
                progress_span: 60,
                command: gdal2tiles,
                args: vec![
                    "-p".to_string(),
                    "geodetic".to_string(),
                    "--xyz".to_string(),
                    // 显式禁用 KML 产物，避免输出目录出现与业务无关文件。
                    "--no-kml".to_string(),
                    "--tiledriver".to_string(),
                    tile_format.tile_driver.to_string(),
                    "-z".to_string(),
                    format!("{}-{}", req.zoom_min, req.zoom_max),
                    "-r".to_string(),
                    resampling,
                    "--processes=4".to_string(),
                    "-w".to_string(),
                    "none".to_string(),
                    tile_input,
                    req.output_dir.clone(),
                ],
            });
        }
        "webmercator" => {
            // 输入已是 Web 墨卡顿时直接喂 gdal2tiles，与 wgs84 分支同理省去 warp。
            let source_is_3857 = is_source_web_mercator(req)?;
            let tile_input = if source_is_3857 {
                req.input_path.clone()
            } else {
                intermediates.push(mercator_path_str.clone());
                stages.push(StageSpec {
                    stage: "warp_3857",
                    progress_start: 5,
                    progress_span: 35,
                    command: "gdalwarp".to_string(),
                    args: build_warp_args(
                        req,
                        "EPSG:3857",
                        req.input_path.clone(),
                        mercator_path_str.clone(),
                    ),
                });
                mercator_path_str.clone()
            };
            stages.push(StageSpec {
                stage: "tiles_mercator",
                progress_start: 40,
                progress_span: 60,
                command: gdal2tiles,
                args: vec![
                    "-p".to_string(),
                    "mercator".to_string(),
                    "--xyz".to_string(),
                    // 显式禁用 KML 产物，避免输出目录出现与业务无关文件。
                    "--no-kml".to_string(),
                    "--tiledriver".to_string(),
                    tile_format.tile_driver.to_string(),
                    "-z".to_string(),
                    format!("{}-{}", req.zoom_min, req.zoom_max),
                    "-r".to_string(),
                    resampling,
                    "--processes=4".to_string(),
                    "-w".to_string(),
                    "none".to_string(),
                    tile_input,
                    req.output_dir.clone(),
                ],
            });
        }
        "gcj02" => {
            let bounds = req
                .gcj_bounds
                .clone()
                .ok_or_else(|| "GCJ-02 模式必须提供左上右下坐标".to_string())?;
            intermediates.push(wgs_path_str.clone());
            intermediates.push(gcj_path_str.clone());
            stages.push(StageSpec {
                stage: "warp_wgs84",
                progress_start: 5,
                progress_span: 25,
                command: "gdalwarp".to_string(),
                args: build_warp_args(
                    req,
                    "EPSG:4326",
                    req.input_path.clone(),
                    wgs_path_str.clone(),
                ),
            });
            stages.push(StageSpec {
                stage: "tag_gcj_bounds",
                progress_start: 30,
                progress_span: 25,
                command: "gdal_translate".to_string(),
                args: vec![
                    "-of".to_string(),
                    "VRT".to_string(),
                    "-a_srs".to_string(),
                    "EPSG:4326".to_string(),
                    "-a_ullr".to_string(),
                    bounds.ulx.to_string(),
                    bounds.uly.to_string(),
                    bounds.lrx.to_string(),
                    bounds.lry.to_string(),
                    wgs_path_str,
                    gcj_path_str.clone(),
                ],
            });
            stages.push(StageSpec {
                stage: "tiles_gaode_xyz",
                progress_start: 55,
                progress_span: 45,
                command: gdal2tiles,
                args: vec![
                    "-p".to_string(),
                    "mercator".to_string(),
                    "--xyz".to_string(),
                    // 显式禁用 KML 产物，避免输出目录出现与业务无关文件。
                    "--no-kml".to_string(),
                    "--tiledriver".to_string(),
                    tile_format.tile_driver.to_string(),
                    "-z".to_string(),
                    format!("{}-{}", req.zoom_min, req.zoom_max),
                    "-r".to_string(),
                    resampling,
                    "--processes=4".to_string(),
                    "-w".to_string(),
                    "none".to_string(),
                    gcj_path_str,
                    req.output_dir.clone(),
                ],
            });
        }
        other => return Err(format!("不支持的目标坐标系: {other}")),
    }

    Ok((stages, tile_format))
}

fn resolve_tile_output_format(
    gdal2tiles_cmd: &str,
    gdal_bin_dir: Option<&str>,
) -> Result<TileOutputFormat, String> {
    // 默认优先 WebP；若工具链不支持则回退 JPEG（不回退 PNG）。
    let output = build_command(gdal2tiles_cmd, gdal_bin_dir)
        .arg("--help")
        .output()
        .map_err(|e| format!("检测 gdal2tiles 输出格式失败: {e}"))?;
    let help_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_uppercase();
    if help_text.contains("WEBP") {
        return Ok(TileOutputFormat {
            extension: "webp",
            tile_driver: "WEBP",
        });
    }
    if help_text.contains("JPEG") || help_text.contains("JPG") {
        return Ok(TileOutputFormat {
            extension: "jpg",
            tile_driver: "JPEG",
        });
    }
    Err("当前 gdal2tiles 不支持 WEBP/JPEG tiledriver，无法按要求切图".to_string())
}

fn is_source_wgs84(req: &TileJobRequest) -> Result<bool, String> {
    // 仅当顶层为地理 CRS 时才跳过 warp；不能用「含 WGS 84」判断，否则 UTM 等投影的 BASEGEOGCRS 会误判。
    let info = inspect_tif(&req.input_path, req.gdal_bin_dir.as_deref())?;
    let wkt = info.coordinate_system.unwrap_or_default();
    let head = wkt.trim_start().to_ascii_uppercase();
    Ok(head.starts_with("GEOGCRS[") || head.starts_with("GEOGCS["))
}

// 与 is_source_wgs84 对称：已是 EPSG:3857 时 webmercator 分支无需再 warp 出中间文件。
fn is_source_web_mercator(req: &TileJobRequest) -> Result<bool, String> {
    let info = inspect_tif(&req.input_path, req.gdal_bin_dir.as_deref())?;
    let wkt = info.coordinate_system.unwrap_or_default().to_ascii_uppercase();
    Ok(wkt.contains("ID[\"EPSG\",3857]"))
}

fn build_warp_args(
    req: &TileJobRequest,
    target_srs: &str,
    input: String,
    output: String,
) -> Vec<String> {
    // Hybrid 策略：默认保留磁盘检查，用户显式勾选后才注入关闭检查参数。
    let mut args = Vec::new();
    if req.skip_disk_space_check {
        args.push("--config".to_string());
        args.push("CHECK_DISK_FREE_SPACE".to_string());
        args.push("FALSE".to_string());
    }
    // 某些 GDAL 版本不支持 -progress，这里只保留兼容性更好的并行参数。
    args.push("-multi".to_string());
    args.push("-wo".to_string());
    args.push("NUM_THREADS=ALL_CPUS".to_string());
    // 默认写出 VRT，由 gdal2tiles 按瓦块读取时再做重投影采样，避免整幅目标 SRS 栅格落盘。
    args.push("-of".to_string());
    args.push("VRT".to_string());
    args.push("-t_srs".to_string());
    args.push(target_srs.to_string());
    args.push(input);
    args.push(output);
    args
}

fn run_stage(
    app: &AppHandle,
    job_id: &str,
    control: Arc<JobControl>,
    spec: StageSpec,
    working_dir: Option<&str>,
    gdal_bin_dir: Option<&str>,
) -> Result<(), String> {
    // 单阶段执行器负责进程启动、日志转发、进度推断与取消响应。
    if control.cancelled.load(Ordering::Relaxed) {
        return Err("任务已取消".to_string());
    }

    emit_event(
        app,
        ProgressEvent::new(
            job_id,
            spec.stage,
            "info",
            format!("开始阶段 {}", spec.stage),
            spec.progress_start,
        ),
    )?;

    let mut command = build_command(&spec.command, gdal_bin_dir);
    command
        .args(spec.args.clone())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    let mut child = command
        .spawn()
        .map_err(|err| format!("阶段 {} 启动失败: {err}", spec.stage))?;

    {
        // 记录 pid 用于取消时跨命令终止进程。
        let mut pid_guard = control
            .current_pid
            .lock()
            .map_err(|_| "任务进程锁已损坏".to_string())?;
        *pid_guard = Some(child.id());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法读取 stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "无法读取 stderr".to_string())?;

    let (tx, rx) = mpsc::channel::<(String, String)>();
    spawn_log_reader(stdout, "stdout", tx.clone());
    spawn_log_reader(stderr, "stderr", tx.clone());
    drop(tx);

    loop {
        while let Ok((source, line)) = rx.try_recv() {
            let level = if source == "stderr" { "warn" } else { "info" };
            let percent = parse_progress_percent(&line)
                .map(|stage_percent| {
                    spec.progress_start.saturating_add(
                        (spec.progress_span as u16 * stage_percent as u16 / 100) as u8,
                    )
                })
                .unwrap_or(spec.progress_start);
            emit_event(
                app,
                ProgressEvent::new(job_id, spec.stage, level, line, percent),
            )?;
        }

        if control.cancelled.load(Ordering::Relaxed) {
            let _ = child.kill();
            {
                let mut pid_guard = control
                    .current_pid
                    .lock()
                    .map_err(|_| "任务进程锁已损坏".to_string())?;
                *pid_guard = None;
            }
            return Err("任务已取消".to_string());
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                {
                    let mut pid_guard = control
                        .current_pid
                        .lock()
                        .map_err(|_| "任务进程锁已损坏".to_string())?;
                    *pid_guard = None;
                }
                if status.success() {
                    emit_event(
                        app,
                        ProgressEvent::new(
                            job_id,
                            spec.stage,
                            "success",
                            format!("阶段 {} 完成", spec.stage),
                            spec.progress_start.saturating_add(spec.progress_span),
                        ),
                    )?;
                    return Ok(());
                }
                return Err(format!(
                    "阶段 {} 执行失败，退出码: {:?}",
                    spec.stage,
                    status.code()
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(120)),
            Err(err) => return Err(format!("阶段 {} 状态检查失败: {err}", spec.stage)),
        }
    }
}

fn spawn_log_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    source: &'static str,
    tx: mpsc::Sender<(String, String)>,
) {
    thread::spawn(move || {
        // 按行读取日志，避免输出碎片化导致前端难以展示。
        let buf_reader = BufReader::new(reader);
        for line in buf_reader.lines().map_while(Result::ok) {
            let _ = tx.send((source.to_string(), line));
        }
    });
}

fn validate_request(req: &TileJobRequest) -> Result<(), String> {
    // 在启动外部进程前做基础参数检查，避免无效任务浪费时间。
    if req.zoom_min > req.zoom_max {
        return Err("最小缩放级别不能大于最大缩放级别".to_string());
    }
    if req.zoom_max > 24 {
        return Err("最大缩放级别不应超过 24".to_string());
    }
    if !Path::new(&req.input_path).exists() {
        return Err(format!("输入文件不存在: {}", req.input_path));
    }
    Ok(())
}

fn emit_event(app: &AppHandle, event: ProgressEvent) -> Result<(), String> {
    // 事件发送统一封装，便于未来替换事件名或补充审计逻辑。
    app.emit("tiling-progress", event)
        .map_err(|err| format!("发送进度事件失败: {err}"))
}

fn ensure_dir_exists(path: &str) -> Result<(), String> {
    // 输出目录在任务前创建，避免 gdal2tiles 中途因目录问题失败。
    std::fs::create_dir_all(path).map_err(|err| format!("无法创建目录 {path}: {err}"))
}

fn allocate_default_output_dir() -> Result<String, String> {
    // 使用 std::env::temp_dir()（macOS 多为 TMPDIR，Windows 多为用户 Local\Temp），避免与输入文件同盘耦合。
    let base = std::env::temp_dir();
    let dir = base.join(format!("giff_tiles_{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("无法创建默认输出目录 {}: {err}", dir.display()))?;
    Ok(dir.to_string_lossy().to_string())
}

fn cleanup_output_tiles(
    app: &AppHandle,
    job_id: &str,
    output_dir: &str,
    upload_config: Option<&OssUploadConfig>,
) -> Vec<String> {
    // 只在启用上传后清理临时瓦片目录，保留“仅本地切图”模式的历史行为。
    let Some(config) = upload_config else {
        return Vec::new();
    };
    if !config.enabled {
        return Vec::new();
    }

    match std::fs::remove_dir_all(output_dir) {
        Ok(_) => {
            let _ = emit_event(
                app,
                ProgressEvent::new(
                    job_id,
                    "cleanup_output",
                    "info",
                    format!("上传完成，已删除本地临时瓦片目录: {output_dir}"),
                    100,
                ),
            );
            Vec::new()
        }
        Err(err) => {
            let _ = emit_event(
                app,
                ProgressEvent::new(
                    job_id,
                    "cleanup_output",
                    "warn",
                    format!("本地临时瓦片目录清理失败，已保留: {output_dir}; {err}"),
                    100,
                ),
            );
            vec![output_dir.to_string()]
        }
    }
}

fn cleanup_intermediates(app: &AppHandle, job_id: &str, intermediates: &[String]) -> Vec<String> {
    // 清理失败不应影响主任务成功结果，因此仅记录告警并返回残留文件列表。
    let mut retained = Vec::<String>::new();
    for path in intermediates {
        let delete_result = if Path::new(path).is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        if let Err(err) = delete_result {
            retained.push(path.clone());
            let _ = emit_event(
                app,
                ProgressEvent::new(
                    job_id,
                    "cleanup_intermediates",
                    "warn",
                    format!("中间文件清理失败，已保留: {path}; {err}"),
                    100,
                ),
            );
        }
    }
    retained
}

fn maybe_upload_tiles(
    app: &AppHandle,
    req: &TileJobRequest,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<Option<OssUploadSummary>, String> {
    // 上传配置缺失或关闭时直接跳过，保持历史任务行为不变。
    let Some(config) = req.upload_config.as_ref() else {
        return Ok(None);
    };
    if !config.enabled {
        return Ok(None);
    }

    let upload_run_id = build_upload_run_segment(req)?;
    let prepare_msg = match upload_root_prefix_preview(config, &upload_run_id) {
        Ok(root_url) => format!("切图完成，准备上传 OSS。对象根路径（PUT 前缀）: {root_url}"),
        Err(_) => "切图完成，准备上传 OSS（尚未解析根路径，若缺密钥将在随后失败）".to_string(),
    };
    emit_event(
        app,
        ProgressEvent::new(&req.job_id, "upload_prepare", "info", prepare_msg, 90),
    )?;

    let upload_result = upload_tiles_to_oss(
        &req.output_dir,
        &upload_run_id,
        config,
        cancel,
        |done, total| {
            // 上传进度压缩到 90~99 区间，保留 100 给最终 done 事件。
            let percent = if total == 0 {
                99
            } else {
                90u8.saturating_add(((done as f64 / total as f64) * 9.0).round() as u8)
            };
            let _ = emit_event(
                app,
                ProgressEvent::new(
                    &req.job_id,
                    "uploading",
                    "info",
                    format!("OSS 上传进度: {done}/{total}"),
                    percent.min(99),
                ),
            );
        },
    );

    match upload_result {
        Ok(summary) => {
            emit_event(
                app,
                ProgressEvent::new(
                    &req.job_id,
                    "upload_done",
                    if summary.failed_count == 0 {
                        "success"
                    } else {
                        "warn"
                    },
                    format!(
                        "OSS 上传完成：成功 {}，失败 {}",
                        summary.uploaded_count, summary.failed_count
                    ),
                    99,
                ),
            )?;
            Ok(Some(summary))
        }
        Err(err) => {
            emit_event(
                app,
                ProgressEvent::new(
                    &req.job_id,
                    "upload_done",
                    "warn",
                    format!("OSS 上传失败: {err}"),
                    99,
                ),
            )?;
            // 用户主动取消上传：不应吞掉为「部分成功」，整体任务按失败返回。
            if err.contains("上传已被取消") {
                return Err(err);
            }
            if config.fail_task_on_error {
                Err(format!("OSS 上传失败且配置为强失败: {err}"))
            } else {
                Ok(Some(OssUploadSummary {
                    uploaded_count: 0,
                    failed_count: 1,
                    bucket: config.bucket.clone().unwrap_or_default(),
                    prefix: config.prefix.clone().unwrap_or_default(),
                    public_base_url: String::new(),
                    sample_tile_url: None,
                    failures: vec![UploadFailure {
                        file_path: req.output_dir.clone(),
                        error: err,
                    }],
                }))
            }
        }
    }
}

fn build_upload_run_segment(req: &TileJobRequest) -> Result<String, String> {
    // 网页拉起场景按租户前缀组织对象目录，便于按租户批量筛选与治理。
    let Some(ctx) = req.upload_context.as_ref() else {
        return new_upload_run_id();
    };
    let tenant_id = sanitize_folder_segment(&ctx.tenant_id);
    let layer_name = sanitize_folder_segment(&ctx.layer_name);
    let ts = ctx.ts.trim();
    let platform = ctx.platform.trim().to_ascii_lowercase();
    let platform_ok = matches!(platform.as_str(), "win" | "mac");
    if tenant_id.is_empty()
        || layer_name.is_empty()
        || !ts.chars().all(|c| c.is_ascii_digit())
        || !platform_ok
    {
        return new_upload_run_id();
    }
    // 目录按租户分层：tenantId/layerName_ts
    Ok(format!("{tenant_id}/{layer_name}_{ts}"))
}

fn sanitize_folder_segment(input: &str) -> String {
    // 仅清理路径敏感字符，保留常见中英文与数字，避免业务名可读性损失。
    let cleaned = input
        .trim()
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ if ch.is_control() => '_',
            _ => ch,
        })
        .collect::<String>();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("_")
        .trim_matches('_')
        .chars()
        .take(80)
        .collect()
}

fn kill_process(pid: u32) -> Result<(), String> {
    // 平台分支统一封装进程终止逻辑，避免取消流程分散在业务代码中。
    #[cfg(target_os = "windows")]
    {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .map_err(|err| format!("终止进程失败: {err}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("taskkill 返回失败".to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let status = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .map_err(|err| format!("终止进程失败: {err}"))?;
        if status.success() {
            return Ok(());
        }
        Err("kill 返回失败".to_string())
    }
}

fn parse_progress_percent(line: &str) -> Option<u8> {
    // 兼容两类 GDAL 进度：`42%` 和 `0...10...20...`。
    if let Some(pos) = line.find('%') {
        let bytes = line.as_bytes();
        let mut start = pos;
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start != pos {
            return line[start..pos].parse::<u8>().ok();
        }
    }

    if line.contains("...") {
        let mut last: Option<u8> = None;
        for segment in line.split("...") {
            if let Ok(v) = segment.trim().parse::<u8>() {
                if v <= 100 {
                    last = Some(v);
                }
            }
        }
        return last;
    }

    None
}
