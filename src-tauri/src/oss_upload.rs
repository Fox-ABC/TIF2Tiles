use base64::Engine;
use chrono::Utc;
use hmac::{Hmac, Mac};
use percent_encoding::utf8_percent_encode;
use percent_encoding::NON_ALPHANUMERIC;
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use std::path::{Path, PathBuf};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

type HmacSha1 = Hmac<Sha1>;

/// 前端 / 任务请求中的 OSS 参数；密钥建议仅通过环境变量注入，避免写入前端构建产物。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OssUploadConfig {
    pub enabled: bool,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    /// 反序列化保留；实际上传目标由 `resolve_config` 固定为北京区 xuntian-pro-public / xuntian/map。
    #[allow(dead_code)]
    pub endpoint: Option<String>,
    #[allow(dead_code)]
    pub region: Option<String>,
    #[serde(default)]
    pub fail_task_on_error: bool,
    pub access_key_id: Option<String>,
    pub access_key_secret: Option<String>,
    pub security_token: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub public_base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadFailure {
    pub file_path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OssUploadSummary {
    pub uploaded_count: usize,
    pub failed_count: usize,
    pub bucket: String,
    pub prefix: String,
    pub public_base_url: String,
    pub sample_tile_url: Option<String>,
    pub failures: Vec<UploadFailure>,
}

#[derive(Clone)]
struct OssResolvedConfig {
    bucket: String,
    prefix: String,
    endpoint: String,
    access_key_id: String,
    access_key_secret: String,
    security_token: Option<String>,
    public_base_url: Option<String>,
}

/// 并行上传工作线程数：在吞吐与限流之间折中。
fn upload_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4)
}

/// 生成上传批次唯一数字（Unix 毫秒），用于路径 `.../{id}/`。
pub fn new_upload_run_id() -> Result<String, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .map_err(|e| format!("生成上传批次 ID 失败: {e}"))
}

/// 对象键：`{prefix}/{upload_run_id}/{输出目录相对路径}`，例如 `xuntian/map/1730123456789/12/3456/7890.png`。
pub fn upload_tiles_to_oss<F>(
    output_dir: &str,
    upload_run_id: &str,
    config: &OssUploadConfig,
    cancel: Option<Arc<AtomicBool>>,
    on_progress: F,
) -> Result<OssUploadSummary, String>
where
    F: FnMut(usize, usize) + Send,
{
    let resolved = resolve_config(config)?;
    let all_files = collect_upload_files(output_dir)?;
    if all_files.is_empty() {
        return Err("输出目录为空，没有可上传文件".to_string());
    }
    let files_by_z = group_files_by_z(output_dir, &all_files);
    let files = files_by_z
        .values()
        .flat_map(|group| group.iter().cloned())
        .collect::<Vec<_>>();

    let total = files.len();
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(upload_worker_count())
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败: {e}"))?;

    let uploaded_count = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(Mutex::new(Vec::<UploadFailure>::new()));
    let sample_tile_url = Arc::new(Mutex::new(None::<String>));
    let on_progress = Arc::new(Mutex::new(on_progress));

    let pool = ThreadPoolBuilder::new()
        .num_threads(upload_worker_count())
        .build()
        .map_err(|e| format!("初始化上传线程池失败: {e}"))?;

    let cancel_ref = cancel.clone();
    let resolved_arc = Arc::new(resolved.clone());
    let run_segment = upload_run_id.to_string();
    let output_dir_owned = output_dir.to_string();

    for group in files_by_z.values() {
        pool.install(|| {
            group.par_iter().for_each(|file_path| {
                if cancel_ref
                    .as_ref()
                    .map(|f| f.load(Ordering::Relaxed))
                    .unwrap_or(false)
                {
                    return;
                }

                let rel = Path::new(file_path)
                    .strip_prefix(&output_dir_owned)
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| PathBuf::from(file_path));
                let rel_key = normalize_relative_key(&rel);
                let object_key = build_object_key(&resolved_arc.prefix, &run_segment, &rel_key);

                let upload_result = upload_single_file_with_retry(
                    &client,
                    &resolved_arc,
                    &object_key,
                    file_path,
                );

                match upload_result {
                    Ok(url) => {
                        let n = uploaded_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if rel_key.ends_with(".webp")
                            || rel_key.ends_with(".jpg")
                            || rel_key.ends_with(".jpeg")
                        {
                            let mut slot = sample_tile_url.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(url);
                            }
                        }
                        if let Ok(mut cb) = on_progress.lock() {
                            (*cb)(n, total);
                        }
                    }
                    Err(err) => failures
                        .lock()
                        .unwrap()
                        .push(UploadFailure {
                            file_path: rel_key,
                            error: err,
                        }),
                }
            });
        });
    }

    if cancel
        .as_ref()
        .map(|f| f.load(Ordering::Relaxed))
        .unwrap_or(false)
    {
        return Err("OSS 上传已被取消".to_string());
    }

    let uploaded_count = uploaded_count.load(Ordering::SeqCst);
    let failures = failures.lock().unwrap().clone();
    let sample_tile_url = sample_tile_url.lock().unwrap().clone();

    let public_base_url = build_public_base_url(&resolved, upload_run_id);

    Ok(OssUploadSummary {
        uploaded_count,
        failed_count: failures.len(),
        bucket: resolved.bucket,
        prefix: resolved.prefix,
        public_base_url,
        sample_tile_url,
        failures,
    })
}

/// 固定上传桶与前缀，忽略请求体中的 bucket/endpoint/prefix/region/public_base_url，防止前端或 invoke 参数被篡改。
fn resolve_config(config: &OssUploadConfig) -> Result<OssResolvedConfig, String> {
    let bucket = "xuntian-pro-public".to_string();
    let prefix = normalize_prefix("xuntian/map");
    let endpoint = normalize_endpoint("oss-cn-beijing.aliyuncs.com".to_string());

    let access_key_id = config
        .access_key_id
        .clone()
        .or_else(|| std::env::var("OSS_ACCESS_KEY_ID").ok())
        .ok_or_else(|| "缺少 OSS AccessKeyId（uploadConfig 或 OSS_ACCESS_KEY_ID）".to_string())?;
    let access_key_secret = config
        .access_key_secret
        .clone()
        .or_else(|| std::env::var("OSS_ACCESS_KEY_SECRET").ok())
        .ok_or_else(|| "缺少 OSS AccessKeySecret（uploadConfig 或 OSS_ACCESS_KEY_SECRET）".to_string())?;
    let security_token = config
        .security_token
        .clone()
        .or_else(|| std::env::var("OSS_SECURITY_TOKEN").ok());

    Ok(OssResolvedConfig {
        bucket,
        prefix: normalize_prefix(&prefix),
        endpoint,
        access_key_id,
        access_key_secret,
        security_token,
        public_base_url: None,
    })
}

fn normalize_endpoint(raw: String) -> String {
    raw.trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// 排除 gdal2tiles 生成的预览页与无关文件，减少误传。
fn should_upload_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("html") | Some("htm") | Some("kml") | Some("txt") | Some("log") => false,
        _ => true,
    }
}

fn collect_upload_files(output_dir: &str) -> Result<Vec<String>, String> {
    let mut files = Vec::<String>::new();
    for entry in WalkDir::new(output_dir).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && should_upload_file(path) {
            files.push(path.to_string_lossy().to_string());
        }
    }
    files.sort();
    Ok(files)
}

fn group_files_by_z(output_dir: &str, files: &[String]) -> BTreeMap<String, Vec<String>> {
    // 按 z 层分批上传，避免深层级大批量一次性提交导致峰值拥塞。
    let mut grouped = BTreeMap::<String, Vec<String>>::new();
    for file in files {
        let rel = Path::new(file)
            .strip_prefix(output_dir)
            .map(|p| normalize_relative_key(p))
            .unwrap_or_else(|_| normalize_relative_key(Path::new(file)));
        let z = rel
            .split('/')
            .next()
            .filter(|seg| !seg.is_empty())
            .unwrap_or("_misc")
            .to_string();
        grouped.entry(z).or_default().push(file.clone());
    }
    grouped
}

fn upload_single_file_with_retry(
    client: &Client,
    config: &OssResolvedConfig,
    object_key: &str,
    local_path: &str,
) -> Result<String, String> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(200 * u64::from(attempt)));
        }
        match upload_single_file(client, config, object_key, local_path) {
            Ok(url) => return Ok(url),
            Err(e) => {
                last_err = e.clone();
                if attempt + 1 < MAX_ATTEMPTS && is_retryable_error(&e) {
                    continue;
                }
                return Err(e);
            }
        }
    }
    Err(last_err)
}

fn is_retryable_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("timeout")
        || m.contains("timed out")
        || m.contains("connection")
        || m.contains("502")
        || m.contains("503")
        || m.contains("504")
        || m.contains("429")
}

fn upload_single_file(
    client: &Client,
    config: &OssResolvedConfig,
    object_key: &str,
    local_path: &str,
) -> Result<String, String> {
    let body = std::fs::read(local_path)
        .map_err(|e| format!("读取本地文件失败 {local_path}: {e}"))?;
    let content_type = guess_content_type(local_path);
    let date = rfc_1123_date();
    let canonical_headers = canonicalized_oss_headers(config.security_token.as_deref());
    let canonical_resource = format!("/{}/{}", config.bucket, object_key);
    let string_to_sign = format!(
        "PUT\n\n{content_type}\n{date}\n{canonical_headers}{canonical_resource}"
    );
    let signature = sign_hmac_sha1_base64(&config.access_key_secret, &string_to_sign)?;
    let authorization = format!("OSS {}:{}", config.access_key_id, signature);
    let url = build_put_url(&config.bucket, &config.endpoint, object_key)?;

    let mut req = client
        .put(&url)
        .header("Date", &date)
        .header("Content-Type", content_type)
        .header("Authorization", &authorization)
        .body(body);

    if let Some(token) = &config.security_token {
        if !token.trim().is_empty() {
            req = req.header("x-oss-security-token", token);
        }
    }

    let response = req
        .send()
        .map_err(|e| format!("OSS 请求失败 {local_path}: {e}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(url);
    }

    let body_text = response.text().unwrap_or_default();
    if status == StatusCode::FORBIDDEN {
        return Err(format!("OSS 拒绝访问(403): {}", shorten(&body_text, 240)));
    }
    Err(format!(
        "OSS 上传失败({}): {}",
        status.as_u16(),
        shorten(&body_text, 240)
    ))
}

/// 瓦片路径多为安全 ASCII；含中文等字符时对各段做百分号编码，且保留 `.` 不参与 NON_ALPHANUMERIC 的全量编码。
fn build_put_url(bucket: &str, endpoint: &str, object_key: &str) -> Result<String, String> {
    let path = if object_key.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~')
    }) {
        object_key.to_string()
    } else {
        // 路径含非 ASCII 等字符时按段编码；瓦片文件名中的 . 需保留，故仅对「非安全段」做全量编码。
        object_key
            .split('/')
            .map(|seg| {
                if seg.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '~')
                }) {
                    seg.to_string()
                } else {
                    utf8_percent_encode(seg, NON_ALPHANUMERIC).to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    };
    Ok(format!("https://{bucket}.{endpoint}/{path}"))
}

fn rfc_1123_date() -> String {
    Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

fn canonicalized_oss_headers(security_token: Option<&str>) -> String {
    match security_token {
        Some(token) if !token.trim().is_empty() => format!("x-oss-security-token:{token}\n"),
        _ => String::new(),
    }
}

fn sign_hmac_sha1_base64(secret: &str, payload: &str) -> Result<String, String> {
    let mut mac = HmacSha1::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("构建 HMAC 密钥失败: {e}"))?;
    mac.update(payload.as_bytes());
    let result = mac.finalize().into_bytes();
    Ok(base64::engine::general_purpose::STANDARD.encode(result))
}

fn guess_content_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("pbf") => "application/x-protobuf",
        _ => "application/octet-stream",
    }
}

fn build_object_key(prefix: &str, upload_run_id: &str, rel_key: &str) -> String {
    format!(
        "{}/{}/{}",
        normalize_prefix(prefix),
        upload_run_id.trim(),
        rel_key
    )
}

fn build_base_path(prefix: &str, upload_run_id: &str) -> String {
    format!(
        "/{}/{}/",
        normalize_prefix(prefix),
        upload_run_id.trim()
    )
}

/// 上传开始前解析配置，得到与本次批次一致的对外根 URL（含 `{唯一数}/`）。
pub fn upload_root_prefix_preview(config: &OssUploadConfig, upload_run_id: &str) -> Result<String, String> {
    let resolved = resolve_config(config)?;
    Ok(build_public_base_url(&resolved, upload_run_id))
}

fn build_public_base_url(resolved: &OssResolvedConfig, upload_run_id: &str) -> String {
    let id = upload_run_id.trim();
    if let Some(custom) = resolved
        .public_base_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let root = custom.trim_end_matches('/');
        format!("{}/{}/{}/", root, normalize_prefix(&resolved.prefix), id)
    } else {
        format!(
            "https://{}.{}{}",
            resolved.bucket,
            resolved.endpoint,
            build_base_path(&resolved.prefix, id)
        )
    }
}

fn normalize_prefix(prefix: &str) -> String {
    prefix.trim_matches('/').to_string()
}

fn normalize_relative_key(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches('/')
        .to_string()
}

fn shorten(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    text.chars().take(max_len).collect::<String>() + "..."
}
