use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TifInfo {
    pub path: String,
    pub size: [u64; 2],
    pub band_count: u64,
    pub coordinate_system: Option<String>,
    pub upper_left: Option<[f64; 2]>,
    pub lower_right: Option<[f64; 2]>,
    pub pixel_size: Option<[f64; 2]>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GdalCheckResult {
    pub tool: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundsPreview {
    pub target_crs: String,
    pub upper_left: [f64; 2],
    pub lower_right: [f64; 2],
    pub source_srs: String,
    pub note: Option<String>,
}

// 检测核心 GDAL 命令是否可执行，便于在真正切图前给出可读错误。
pub fn check_gdal_tools(gdal_bin_dir: Option<&str>, gdal2tiles_cmd: Option<&str>) -> Vec<GdalCheckResult> {
    let mut results = Vec::new();
    results.push(run_probe("gdalinfo", &["--version"], gdal_bin_dir));
    results.push(run_probe("gdalwarp", &["--version"], gdal_bin_dir));
    results.push(run_probe("gdal_translate", &["--version"], gdal_bin_dir));

    // gdal2tiles 在不同发行中可能叫 gdal2tiles 或 gdal2tiles.py，这里自动兜底。
    if let Some(custom_cmd) = gdal2tiles_cmd {
        results.push(run_probe(custom_cmd, &["--help"], gdal_bin_dir));
        return results;
    }

    let mut last_failure = run_probe("gdal2tiles", &["--help"], gdal_bin_dir);
    if last_failure.ok {
        results.push(last_failure);
        return results;
    }

    let py_variant = run_probe("gdal2tiles.py", &["--help"], gdal_bin_dir);
    if py_variant.ok {
        results.push(py_variant);
    } else {
        last_failure.tool = "gdal2tiles|gdal2tiles.py".to_string();
        last_failure.detail = format!(
            "{}; fallback gdal2tiles.py: {}",
            last_failure.detail, py_variant.detail
        );
        results.push(last_failure);
    }

    results
}

// 通过 gdalinfo -json 读取影像元信息，避免依赖脆弱的文本解析。
pub fn inspect_tif(path: &str, gdal_bin_dir: Option<&str>) -> Result<TifInfo, String> {
    if !Path::new(path).exists() {
        return Err(format!("输入文件不存在: {path}"));
    }

    let output = build_command("gdalinfo", gdal_bin_dir)
        .arg("-json")
        .arg(path)
        .output()
        .map_err(|err| format!("执行 gdalinfo 失败: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gdalinfo 执行失败: {stderr}"));
    }

    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|err| format!("解析 gdalinfo JSON 失败: {err}"))?;

    let size = parse_size(&value);
    let band_count = value
        .get("bands")
        .and_then(Value::as_array)
        .map(|arr| arr.len() as u64)
        .unwrap_or(0);

    let coordinate_system = value
        .get("coordinateSystem")
        .and_then(|cs| cs.get("wkt"))
        .and_then(Value::as_str)
        .map(|v| v.to_string());

    let upper_left = parse_corner(&value, "upperLeft");
    let lower_right = parse_corner(&value, "lowerRight");
    let pixel_size = parse_pixel_size(&value);

    Ok(TifInfo {
        path: path.to_string(),
        size,
        band_count,
        coordinate_system,
        upper_left,
        lower_right,
        pixel_size,
    })
}

// 按目标坐标系动态计算展示坐标，确保前端展示与后端能力同源。
pub fn preview_bounds_by_crs(
    path: &str,
    target_crs: &str,
    gdal_bin_dir: Option<&str>,
) -> Result<BoundsPreview, String> {
    if !Path::new(path).exists() {
        return Err(format!("输入文件不存在: {path}"));
    }

    let (target_srs, mut note) = match target_crs {
        "wgs84" => ("EPSG:4326", None),
        "webmercator" => ("EPSG:3857", None),
        // GCJ-02 先通过 GDAL 得到 WGS84 边界，再做国测局偏移换算，保持显示值可用于当前流程。
        "gcj02" => (
            "EPSG:4326",
            Some("GCJ-02 坐标由 WGS84 边界经偏移模型换算得到".to_string()),
        ),
        other => return Err(format!("不支持的目标坐标系: {other}")),
    };

    let temp_path = std::env::temp_dir().join(format!(
        "giff-preview-{}-{}.vrt",
        target_crs,
        Uuid::new_v4()
    ));
    let temp_path_str = temp_path.to_string_lossy().to_string();

    let warp_output = build_command("gdalwarp", gdal_bin_dir)
        // 预览只需要坐标范围，VRT 足够且体积很小，避免生成超大中间栅格。
        .arg("-of")
        .arg("VRT")
        // 某些大图会触发磁盘空间估算误杀，这里仅对预览流程关闭该检查。
        .arg("--config")
        .arg("CHECK_DISK_FREE_SPACE")
        .arg("FALSE")
        .arg("-t_srs")
        .arg(target_srs)
        .arg(path)
        .arg(&temp_path_str)
        .output()
        .map_err(|err| format!("执行 gdalwarp 失败: {err}"))?;
    if !warp_output.status.success() {
        let stderr = String::from_utf8_lossy(&warp_output.stderr);
        return Err(format!("gdalwarp 执行失败: {stderr}"));
    }

    let warped_info = inspect_tif(&temp_path_str, gdal_bin_dir);
    let _ = std::fs::remove_file(&temp_path);
    let info = warped_info?;

    let mut upper_left = info
        .upper_left
        .ok_or_else(|| "未能读取左上坐标".to_string())?;
    let mut lower_right = info
        .lower_right
        .ok_or_else(|| "未能读取右下坐标".to_string())?;

    if target_crs == "gcj02" {
        upper_left = wgs84_to_gcj02(upper_left[0], upper_left[1]);
        lower_right = wgs84_to_gcj02(lower_right[0], lower_right[1]);
    }

    let source_srs = if target_crs == "gcj02" {
        "GCJ-02".to_string()
    } else {
        target_srs.to_string()
    };
    if target_crs != "gcj02" {
        note = None;
    }

    Ok(BoundsPreview {
        target_crs: target_crs.to_string(),
        upper_left,
        lower_right,
        source_srs,
        note,
    })
}

// 构建带可选 GDAL bin 目录前缀的命令，统一处理跨平台工具路径。
pub fn build_command(tool: &str, gdal_bin_dir: Option<&str>) -> Command {
    let tool_path = resolve_tool_path(tool, gdal_bin_dir);
    let is_python_script = tool_path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("py"))
        .unwrap_or(false);

    // gdal2tiles.py 在 Windows 上常常无法直接执行，这里统一用 python 启动脚本。
    let mut cmd = if is_python_script {
        let mut py_cmd = Command::new(resolve_python_executable());
        py_cmd.arg(&tool_path);
        py_cmd
    } else {
        Command::new(&tool_path)
    };

    // 当命中应用内置 gdal/bin 时，注入运行时环境，确保 dylib 与坐标数据可解析。
    if let Some(bin_dir) = tool_path.parent() {
        let root = bin_dir.parent().unwrap_or(bin_dir);
        let lib_dir = root.join("lib");
        let gdal_data = root.join("share/gdal");
        let proj_data = root.join("share/proj");

        if cfg!(target_os = "macos") && lib_dir.exists() {
            cmd.env("DYLD_LIBRARY_PATH", prepend_env_path("DYLD_LIBRARY_PATH", &lib_dir));
        }
        if gdal_data.exists() {
            cmd.env("GDAL_DATA", gdal_data);
        }
        if proj_data.exists() {
            cmd.env("PROJ_LIB", proj_data);
        }
        cmd.env("PATH", prepend_env_path("PATH", bin_dir));
    }

    cmd
}

// 自动探测 GDAL bin 目录，供前端默认填充，减少 Finder 启动时 PATH 缺失问题。
pub fn detect_gdal_bin_dir() -> Option<String> {
    let required = ["gdalinfo", "gdalwarp", "gdal_translate"];
    candidate_bin_dirs()
        .into_iter()
        .find(|dir| required.iter().all(|tool| find_tool_in_dir(dir, tool).is_some()))
        .map(|dir| dir.to_string_lossy().to_string())
}

// 优先使用可执行文件旁路、资源目录和常见安装目录，最后回退系统 PATH。
fn resolve_tool_path(tool: &str, gdal_bin_dir: Option<&str>) -> PathBuf {
    if let Some(dir) = gdal_bin_dir {
        return Path::new(dir).join(tool);
    }

    if tool.contains(std::path::MAIN_SEPARATOR) {
        return PathBuf::from(tool);
    }

    for dir in candidate_bin_dirs() {
        if let Some(candidate) = find_tool_in_dir(&dir, tool) {
            return candidate;
        }
    }

    PathBuf::from(tool)
}

fn candidate_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::<PathBuf>::new();
    let mut seen = HashSet::<String>::new();

    let mut push_unique = |path: PathBuf| {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            dirs.push(path);
        }
    };

    if let Ok(dir) = std::env::var("GDAL_BIN_DIR") {
        push_unique(PathBuf::from(dir));
    }

    if let Ok(cwd) = std::env::current_dir() {
        push_unique(cwd.join("gdal/bin"));
    }

    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            push_unique(exe_dir.to_path_buf());
            // macOS .app 内置资源目录：Contents/Resources/gdal/bin
            push_unique(exe_dir.join("../Resources/gdal/bin"));
            // Linux/Windows 常见资源目录
            push_unique(exe_dir.join("../resources/gdal/bin"));
            push_unique(exe_dir.join("resources/gdal/bin"));
        }
    }

    // 常见 macOS 包管理器路径。
    push_unique(PathBuf::from("/opt/homebrew/bin"));
    push_unique(PathBuf::from("/usr/local/bin"));
    push_unique(PathBuf::from("/opt/local/bin"));

    if cfg!(target_os = "windows") {
        // Windows 常见 GDAL 目录（OSGeo4W/QGIS/Conda）。
        push_unique(PathBuf::from("C:/OSGeo4W64/bin"));
        push_unique(PathBuf::from("C:/OSGeo4W/bin"));
        push_unique(PathBuf::from("C:/Program Files/QGIS/bin"));
        push_unique(PathBuf::from("C:/Program Files/QGIS 3.34.0/bin"));

        if let Ok(user_profile) = std::env::var("USERPROFILE") {
            push_unique(PathBuf::from(&user_profile).join("miniconda3/Library/bin"));
            push_unique(PathBuf::from(&user_profile).join("Miniconda3/Library/bin"));
            push_unique(PathBuf::from(&user_profile).join("miniconda3/Scripts"));
            push_unique(PathBuf::from(&user_profile).join("Miniconda3/Scripts"));
            push_unique(PathBuf::from(&user_profile).join("miniconda3/envs/gdal-mini/Library/bin"));
            push_unique(PathBuf::from(&user_profile).join("Miniconda3/envs/gdal-mini/Library/bin"));
            push_unique(PathBuf::from(&user_profile).join("miniconda3/envs/gdal-mini/Scripts"));
            push_unique(PathBuf::from(&user_profile).join("Miniconda3/envs/gdal-mini/Scripts"));
            push_unique(PathBuf::from(&user_profile).join("AppData/Local/conda/conda/envs/gdal-mini/Library/bin"));
            push_unique(PathBuf::from(&user_profile).join("AppData/Local/conda/conda/envs/gdal-mini/Scripts"));
        }
    }

    dirs
}

fn prepend_env_path(key: &str, value: &Path) -> String {
    // 以“新路径优先，兼容已有值”的策略拼 PATH，避免污染系统环境语义。
    let new_path = value.to_string_lossy().to_string();
    let separator = if cfg!(target_os = "windows") { ";" } else { ":" };
    match std::env::var(key) {
        Ok(old) if !old.trim().is_empty() => format!("{new_path}{separator}{old}"),
        _ => new_path,
    }
}

fn find_tool_in_dir(dir: &Path, tool: &str) -> Option<PathBuf> {
    // 兼容 Windows 下 .exe/.bat/.cmd 后缀，避免“文件存在但探测失败”。
    let plain = dir.join(tool);
    if plain.exists() {
        return Some(plain);
    }
    if cfg!(target_os = "windows") {
        for ext in [".exe", ".bat", ".cmd", ".py"] {
            let candidate = dir.join(format!("{tool}{ext}"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn resolve_python_executable() -> &'static str {
    // 运行时优先使用系统可用解释器：Windows 偏向 py，其他平台偏向 python3。
    if cfg!(target_os = "windows") {
        "py"
    } else {
        "python3"
    }
}

fn run_probe(tool: &str, args: &[&str], gdal_bin_dir: Option<&str>) -> GdalCheckResult {
    // 使用轻量探测命令判断工具可用性，避免真正任务才暴露环境缺失。
    let result = build_command(tool, gdal_bin_dir).args(args).output();

    match result {
        Ok(output) if output.status.success() => GdalCheckResult {
            tool: tool.to_string(),
            ok: true,
            detail: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        },
        Ok(output) => GdalCheckResult {
            tool: tool.to_string(),
            ok: false,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(err) => GdalCheckResult {
            tool: tool.to_string(),
            ok: false,
            detail: err.to_string(),
        },
    }
}

fn parse_size(value: &Value) -> [u64; 2] {
    // gdalinfo 的 size 固定是 [width, height]，缺失时返回 0 便于前端识别异常。
    let mut size = [0_u64, 0_u64];
    if let Some(arr) = value.get("size").and_then(Value::as_array) {
        size[0] = arr.first().and_then(Value::as_u64).unwrap_or(0);
        size[1] = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
    }
    size
}

fn parse_corner(value: &Value, key: &str) -> Option<[f64; 2]> {
    // 角点坐标用于展示左上右下边界，读取失败时返回 None 而不强制报错。
    let arr = value
        .get("cornerCoordinates")
        .and_then(|corners| corners.get(key))
        .and_then(Value::as_array)?;

    let x = arr.first().and_then(Value::as_f64)?;
    let y = arr.get(1).and_then(Value::as_f64)?;
    Some([x, y])
}

fn parse_pixel_size(value: &Value) -> Option<[f64; 2]> {
    // geoTransform 第 1 和第 5 位是像素分辨率，保留原符号用于方向判断。
    let transform = value.get("geoTransform").and_then(Value::as_array)?;
    let px = transform.get(1).and_then(Value::as_f64)?;
    let py = transform.get(5).and_then(Value::as_f64)?;
    Some([px, py])
}

const PI: f64 = std::f64::consts::PI;
const A: f64 = 6378245.0;
const EE: f64 = 0.00669342162296594323;

fn wgs84_to_gcj02(lng: f64, lat: f64) -> [f64; 2] {
    // 中国大陆外不做偏移，避免对海外数据引入无意义误差。
    if out_of_china(lng, lat) {
        return [lng, lat];
    }
    let dlat = transform_lat(lng - 105.0, lat - 35.0);
    let dlng = transform_lng(lng - 105.0, lat - 35.0);
    let radlat = lat / 180.0 * PI;
    let magic = 1.0 - EE * radlat.sin() * radlat.sin();
    let sqrt_magic = magic.sqrt();
    let mg_lat = lat + (dlat * 180.0) / ((A * (1.0 - EE)) / (magic * sqrt_magic) * PI);
    let mg_lng = lng + (dlng * 180.0) / (A / sqrt_magic * radlat.cos() * PI);
    [mg_lng, mg_lat]
}

fn out_of_china(lng: f64, lat: f64) -> bool {
    lng < 72.004 || lng > 137.8347 || lat < 0.8293 || lat > 55.8271
}

fn transform_lat(x: f64, y: f64) -> f64 {
    let mut ret = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * PI).sin() + 20.0 * (2.0 * x * PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (y * PI).sin() + 40.0 * (y / 3.0 * PI).sin()) * 2.0 / 3.0;
    ret += (160.0 * (y / 12.0 * PI).sin() + 320.0 * (y * PI / 30.0).sin()) * 2.0 / 3.0;
    ret
}

fn transform_lng(x: f64, y: f64) -> f64 {
    let mut ret = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    ret += (20.0 * (6.0 * x * PI).sin() + 20.0 * (2.0 * x * PI).sin()) * 2.0 / 3.0;
    ret += (20.0 * (x * PI).sin() + 40.0 * (x / 3.0 * PI).sin()) * 2.0 / 3.0;
    ret += (150.0 * (x / 12.0 * PI).sin() + 300.0 * (x / 30.0 * PI).sin()) * 2.0 / 3.0;
    ret
}
