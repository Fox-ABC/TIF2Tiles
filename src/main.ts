import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import {
  OSS_ACCESS_KEY_ID as FILE_OSS_ACCESS_KEY_ID,
  OSS_ACCESS_KEY_SECRET as FILE_OSS_ACCESS_KEY_SECRET,
} from "./config/oss-keys.local";

type TargetCrs = "gcj02" | "webmercator";
type PreviewCrs = "gcj02" | "wgs84";

interface GdalCheckItem {
  tool: string;
  ok: boolean;
  detail: string;
}

interface TifInfo {
  path: string;
  size: [number, number];
  bandCount: number;
  coordinateSystem?: string;
  upperLeft?: [number, number];
  lowerRight?: [number, number];
  pixelSize?: [number, number];
}

interface ProgressEventPayload {
  jobId: string;
  stage: string;
  level: "info" | "warn" | "success" | "error";
  message: string;
  percent: number;
}

interface TileJobResult {
  jobId: string;
  outputDir: string;
  tileTemplate: string;
  generatedIntermediateFiles: string[];
  uploadSummary?: OssUploadSummary;
}

interface BoundsPreview {
  targetCrs: PreviewCrs;
  upperLeft: [number, number];
  lowerRight: [number, number];
  sourceSrs: string;
  note?: string;
}

interface JobRequest {
  jobId: string;
  inputPath: string;
  outputDir: string;
  zoomMin: number;
  zoomMax: number;
  skipDiskSpaceCheck: boolean;
  resampling?: string;
  targetTileCrs: TargetCrs;
  gdalBinDir?: string;
  gcjBounds?: {
    ulx: number;
    uly: number;
    lrx: number;
    lry: number;
  };
  uploadConfig?: OssUploadConfig;
  uploadContext?: UploadContext;
}

interface UploadContext {
  tenantId: string;
  ts: string;
  platform: "win" | "mac";
  layerName: string;
}

interface DeepLinkParseResult {
  context: UploadContext | null;
  reason?: string;
}

interface OssUploadConfig {
  enabled: boolean;
  bucket?: string;
  prefix?: string;
  endpoint?: string;
  region?: string;
  failTaskOnError: boolean;
  publicBaseUrl?: string;
  /** 与后端 serde camelCase 一致；非空时优先于环境变量 OSS_ACCESS_KEY_ID */
  accessKeyId?: string;
  /** 非空时优先于环境变量 OSS_ACCESS_KEY_SECRET */
  accessKeySecret?: string;
}

interface UploadFailure {
  filePath: string;
  error: string;
}

interface OssUploadSummary {
  uploadedCount: number;
  failedCount: number;
  bucket: string;
  prefix: string;
  publicBaseUrl: string;
  sampleTileUrl?: string;
  failures: UploadFailure[];
}

let unlistenProgress: UnlistenFn | null = null;
let currentJobId = "";
let isRunning = false;
let hasInspectedInfo = false;
let lastProgressPercent = -1;
let logFlushTimer: number | null = null;
let deepLinkPollTimer: number | null = null;
const logBuffer: string[] = [];
let renderedLogLines: string[] = [];
const MAX_LOG_LINES = 1000;
let currentBoundsPreview: BoundsPreview | null = null;
let gdalCheckResults: GdalCheckItem[] = [];
let detectedGdalBinDir: string | null = null;
let envReady = false;
let pendingUploadContext: UploadContext | null = null;
/** 为 true 时请求体 outputDir 留空，由后端在系统临时目录下分配子目录（便于上传后自动清理）。 */
let usingDefaultSystemTemp = true;
/** 展示用或用户自选的基础目录；默认模式下与系统临时目录一致，仅用于只读展示。 */
let outputBaseDir: string | null = null;

/** 固定上传目标（与后端一致）；对象键含每次上传生成的毫秒唯一数：xuntian/map/{唯一数}/{z}/{x}/{y}.png */
const OSS_FIXED_BUCKET = "xuntian-pro-public";
const OSS_FIXED_ENDPOINT = "oss-cn-beijing.aliyuncs.com";
const OSS_FIXED_REGION = "cn-beijing";
const OSS_FIXED_PREFIX = "xuntian/map";

// 凭证见 src/config/oss-keys.local.ts（从 oss-keys.local.example.ts 复制，已 gitignore）

// 统一获取输入控件并做空值保护，避免 DOM 结构变化导致运行时崩溃。
function byId<T extends HTMLElement>(id: string): T {
  const element = document.getElementById(id) as T | null;
  if (!element) {
    throw new Error(`缺少页面元素: #${id}`);
  }
  return element;
}

/** 切图完成后固定上传到 xuntian-pro-public / oss-cn-beijing / xuntian/map（后端再次强制，界面无入口）。 */
function buildFixedOssUploadConfig(): OssUploadConfig {
  return {
    enabled: true,
    failTaskOnError: false,
    bucket: OSS_FIXED_BUCKET,
    prefix: OSS_FIXED_PREFIX,
    endpoint: OSS_FIXED_ENDPOINT,
    region: OSS_FIXED_REGION,
    ...(FILE_OSS_ACCESS_KEY_ID ? { accessKeyId: FILE_OSS_ACCESS_KEY_ID } : {}),
    ...(FILE_OSS_ACCESS_KEY_SECRET ? { accessKeySecret: FILE_OSS_ACCESS_KEY_SECRET } : {}),
  };
}

function appendLog(line: string) {
  // 事件频繁时先进入缓冲区，避免每条日志都触发整块文本重排造成卡顿。
  logBuffer.push(line);
}

function flushLogBuffer() {
  if (logBuffer.length === 0) return;

  const logOutput = byId<HTMLPreElement>("log-output");
  renderedLogLines = renderedLogLines.concat(logBuffer.splice(0));

  // 限制最大日志行数，防止超长任务导致文本无限增长拖慢 UI。
  if (renderedLogLines.length > MAX_LOG_LINES) {
    renderedLogLines = renderedLogLines.slice(-MAX_LOG_LINES);
  }

  logOutput.textContent = renderedLogLines.join("\n");
  logOutput.scrollTop = logOutput.scrollHeight;
}

function setStatus(message: string) {
  // 状态栏用于给出当前阶段的人类可读描述。
  byId<HTMLElement>("status-text").textContent = message;
}

function popupNotice(message: string) {
  // 关键节点通过弹框直达用户，避免只看状态栏时错过重要提示。
  window.alert(message);
}

function setProgress(percent: number) {
  // 统一进度裁剪，避免后端噪声导致进度条越界。
  const clamped = Math.max(0, Math.min(100, Math.round(percent)));
  if (clamped === lastProgressPercent) return;
  lastProgressPercent = clamped;
  byId<HTMLProgressElement>("progress-bar").value = clamped;
  byId<HTMLElement>("progress-text").textContent = `${clamped}%`;
}

function setRunning(next: boolean) {
  // 任务运行中禁用关键按钮，防止重复提交造成并发冲突。
  isRunning = next;
  byId<HTMLButtonElement>("run-btn").disabled = next || !envReady;
  byId<HTMLButtonElement>("pick-input-btn").disabled = next;
  byId<HTMLButtonElement>("pick-output-btn").disabled = next;
  byId<HTMLButtonElement>("reset-output-temp-btn").disabled = next;
  byId<HTMLButtonElement>("env-recheck-btn").disabled = next;
  byId<HTMLButtonElement>("cancel-btn").style.display = next ? "inline-block" : "none";
}

function syncPageStage() {
  // 按环境就绪状态在“环境页/切图页”之间切换，避免用户在缺依赖时误操作。
  const setup = byId<HTMLElement>("setup-page");
  const work = byId<HTMLElement>("work-page");
  setup.style.display = envReady ? "none" : "block";
  work.style.display = envReady ? "block" : "none";
}

function renderBoundsPreview(preview: BoundsPreview) {
  // 坐标展示随目标用途变化，且保持只读，避免误改与执行参数不一致。
  const format = (num: number) => num.toFixed(8);
  // 统一按最小/最大经纬度展示，避免不同坐标系方向约定导致“左上/右下”语义混淆。
  const minLon = Math.min(preview.upperLeft[0], preview.lowerRight[0]);
  const maxLon = Math.max(preview.upperLeft[0], preview.lowerRight[0]);
  const minLat = Math.min(preview.upperLeft[1], preview.lowerRight[1]);
  const maxLat = Math.max(preview.upperLeft[1], preview.lowerRight[1]);
  byId<HTMLInputElement>("coord-ulx").value = format(minLon);
  byId<HTMLInputElement>("coord-uly").value = format(minLat);
  byId<HTMLInputElement>("coord-lrx").value = format(maxLon);
  byId<HTMLInputElement>("coord-lry").value = format(maxLat);
  const targetLabel =
    preview.targetCrs === "gcj02"
      ? "高德地图展示（GCJ-02）"
      : "Mapbox + 天地图展示（WGS84）";
  byId<HTMLElement>("coord-title").textContent = `${targetLabel} 最小最大经纬度（只读）`;
  byId<HTMLElement>("coord-note").textContent = preview.note ?? "";
}

function setTileLink(link: string | null) {
  // 结果链接放在坐标区，减少用户在日志中来回查找的操作成本。
  const panel = byId<HTMLDivElement>("tile-link-panel");
  const value = byId<HTMLInputElement>("tile-link-value");
  if (!link) {
    panel.style.display = "none";
    value.value = "";
    return;
  }
  panel.style.display = "block";
  value.value = link;
}

function inferTileExtension(result: TileJobResult): string {
  // 优先从示例 URL 推断扩展名，缺失时回退到 tileTemplate。
  const sample = result.uploadSummary?.sampleTileUrl?.toLowerCase() ?? "";
  if (sample.endsWith(".webp")) return "webp";
  if (sample.endsWith(".jpg") || sample.endsWith(".jpeg")) return "jpg";
  const template = result.tileTemplate.toLowerCase();
  if (template.endsWith(".webp")) return "webp";
  if (template.endsWith(".jpg") || template.endsWith(".jpeg")) return "jpg";
  return "webp";
}

function parseNumberInput(id: string): number {
  // 对数字输入做显式校验，尽早阻断非法参数进入后端。
  const raw = byId<HTMLInputElement>(id).value.trim();
  const value = Number(raw);
  if (!Number.isFinite(value)) {
    throw new Error(`${id} 不是合法数字`);
  }
  return value;
}

/** 首次进入切图页或恢复默认时拉取系统临时目录，只读框与后端 `std::env::temp_dir()` 对齐。 */
async function ensureOutputBaseDirInitialized() {
  if (!usingDefaultSystemTemp && outputBaseDir) {
    byId<HTMLInputElement>("output-dir").value = outputBaseDir;
    return;
  }
  try {
    const temp = await invoke<string>("get_system_temp_dir");
    outputBaseDir = temp;
    byId<HTMLInputElement>("output-dir").value = temp;
  } catch (error) {
    const message = String(error);
    appendLog(`[pick] get_system_temp_dir failed: ${message}`);
  }
}

/** 恢复为系统临时目录，切图输出仍由后端在临时目录下自动建子目录。 */
async function resetOutputBaseToSystemTemp() {
  usingDefaultSystemTemp = true;
  try {
    const temp = await invoke<string>("get_system_temp_dir");
    outputBaseDir = temp;
    byId<HTMLInputElement>("output-dir").value = temp;
    appendLog(`[pick] output base reset to system temp: ${temp}`);
  } catch (error) {
    const message = String(error);
    setStatus(`读取系统临时目录失败: ${message}`);
    appendLog(`[pick] reset temp failed: ${message}`);
  }
}

/** 通过系统对话框选择输出基础目录，禁止手输以减少非法路径。 */
async function selectOutputBaseFolder() {
  try {
    const selected = await open({ directory: true, multiple: false });
    if (!selected || Array.isArray(selected)) return;
    usingDefaultSystemTemp = false;
    outputBaseDir = selected;
    byId<HTMLInputElement>("output-dir").value = selected;
    appendLog(`[pick] output base: ${selected}`);
  } catch (error) {
    const message = String(error);
    setStatus(`选择输出目录失败: ${message}`);
    appendLog(`[pick] output folder failed: ${message}`);
  }
}

async function buildJobRequest(): Promise<JobRequest> {
  // 默认策略 outputDir 留空由后端分配；自选目录时用 join_path 拼每任务子目录，避免多次任务互相覆盖。
  const inputPath = byId<HTMLInputElement>("input-path").value.trim();
  if (!inputPath) {
    throw new Error("请先选择 TIF 文件");
  }
  if (!pendingUploadContext) {
    // 上传链路依赖网页下发上下文；未携带时禁止启动，避免流量落到错误租户分支。
    throw new Error("请从网页重新打开本软件后再次尝试");
  }

  await ensureOutputBaseDirInitialized();
  const jobId = `job-${Date.now()}`;
  let outputDir = "";
  if (!usingDefaultSystemTemp) {
    const base = outputBaseDir?.trim() ?? "";
    if (!base) {
      throw new Error("请先通过「选择文件夹」指定输出基础目录");
    }
    outputDir = await invoke<string>("join_path", {
      parent: base,
      child: `giff_tiles_${jobId}`,
    });
  }

  const targetTileCrs = byId<HTMLSelectElement>("target-crs").value as TargetCrs;
  const request: JobRequest = {
    jobId,
    inputPath,
    outputDir,
    targetTileCrs,
    gdalBinDir: detectedGdalBinDir ?? undefined,
    zoomMin: parseNumberInput("zoom-min"),
    zoomMax: parseNumberInput("zoom-max"),
    skipDiskSpaceCheck: byId<HTMLInputElement>("skip-disk-check").checked,
    resampling: byId<HTMLSelectElement>("resampling").value,
    uploadConfig: buildFixedOssUploadConfig(),
    uploadContext: pendingUploadContext,
  };

  if (targetTileCrs === "gcj02") {
    if (!hasInspectedInfo || !currentBoundsPreview || currentBoundsPreview.targetCrs !== "gcj02") {
      throw new Error("请先选择 TIF 文件并等待坐标计算完成");
    }
    request.gcjBounds = {
      ulx: currentBoundsPreview.upperLeft[0],
      uly: currentBoundsPreview.upperLeft[1],
      lrx: currentBoundsPreview.lowerRight[0],
      lry: currentBoundsPreview.lowerRight[1],
    };
  }

  return request;
}

function currentPlatformTag(): "win" | "mac" {
  // 仅用于协议参数校验与提示，不影响最终任务执行。
  return navigator.userAgent.includes("Windows") ? "win" : "mac";
}

function parseDeepLinkUrl(rawUrl: string): DeepLinkParseResult {
  // 协议参数来自外部 URL，先做格式与必填校验，避免污染任务请求。
  let url: URL;
  try {
    url = new URL(rawUrl);
  } catch {
    return { context: null, reason: "URL 格式非法" };
  }
  if (url.protocol !== "xuntian-uploader:") {
    return { context: null, reason: `协议不匹配: ${url.protocol}` };
  }
  const action = (url.host || url.pathname.replace(/^\//, "")).toLowerCase();
  if (action !== "open") {
    return { context: null, reason: `action 非 open: ${action || "(empty)"}` };
  }
  const tenantId = url.searchParams.get("tenantId")?.trim() ?? "";
  const ts = url.searchParams.get("ts")?.trim() ?? "";
  const platform = (url.searchParams.get("platform")?.trim().toLowerCase() ?? "") as
    | "win"
    | "mac"
    | "";
  const layerName = url.searchParams.get("layerName")?.trim() ?? "";
  if (!tenantId || !ts || !layerName || (platform !== "win" && platform !== "mac")) {
    const missing: string[] = [];
    if (!tenantId) missing.push("tenantId");
    if (!ts) missing.push("ts");
    if (!layerName) missing.push("layerName");
    if (platform !== "win" && platform !== "mac") missing.push("platform(win/mac)");
    return { context: null, reason: `参数不完整或非法: ${missing.join(", ")}` };
  }
  return { context: { tenantId, ts, platform, layerName } };
}

function applyDeepLinkUrls(urls: string[]) {
  // 多 URL 时取最后一个，确保采用最新一次网页拉起参数。
  let parsed: UploadContext | null = null;
  for (const rawUrl of urls) {
    const result = parseDeepLinkUrl(rawUrl);
    if (result.context) {
      parsed = result.context;
      continue;
    }
    // 记录丢弃原因，便于区分“没收到”与“收到但校验失败”。
    appendLog(`[deeplink] 已忽略无效链接: ${rawUrl}，原因: ${result.reason ?? "未知"}`);
  }
  if (!parsed) {
    appendLog("[deeplink] 当前批次未发现可用参数");
    return;
  }

  pendingUploadContext = parsed;
  const localPlatform = currentPlatformTag();
  if (parsed.platform !== localPlatform) {
    appendLog(
      `[deeplink] 平台参数不匹配: url=${parsed.platform}, local=${localPlatform}（仅提示，不阻断）`,
    );
  }
  appendLog(
    `[deeplink] 已接收参数 tenantId=${parsed.tenantId}, layerName=${parsed.layerName}, ts=${parsed.ts}`,
  );
  setStatus("已接收网页拉起参数，请确认输入后手动点击开始切图");
}

async function consumePendingDeepLinks() {
  // 启动早期可能先收到后端 emit，这里在前端就绪后主动补拉一次。
  try {
    const pendingUrls = await invoke<string[]>("get_pending_deep_links");
    if (pendingUrls.length === 0) {
      return;
    }
    appendLog(`[deeplink] 启动补偿拉取到 ${pendingUrls.length} 条链接`);
    applyDeepLinkUrls(pendingUrls);
  } catch (error) {
    appendLog(`[deeplink] 读取待处理参数失败: ${String(error)}`);
  }
}

async function pollPendingDeepLinks() {
  // 有些系统场景里实时事件可能不稳定，定时补捞可降低“收不到参数”的概率。
  await consumePendingDeepLinks();
}

function isDiskSpaceCheckError(message: string): boolean {
  // 兼容 GDAL 不同版本输出，匹配磁盘检查相关关键片段。
  const normalized = message.toLowerCase();
  return (
    normalized.includes("check_disk_free_space") ||
    normalized.includes("free disk space available") ||
    normalized.includes("at least necessary")
  );
}

async function ensureProgressListener() {
  if (unlistenProgress) return;

  // 监听统一的进度事件流，按 jobId 过滤当前任务，避免多任务串台。
  unlistenProgress = await listen<ProgressEventPayload>("tiling-progress", (event) => {
    const payload = event.payload;
    if (!payload || payload.jobId !== currentJobId) {
      return;
    }
    setProgress(payload.percent);
    appendLog(`[${payload.stage}] ${payload.message}`);
    if (payload.level === "warn" || payload.level === "error") {
      setStatus(`阶段 ${payload.stage}: ${payload.message}`);
    }
  });
}

async function inspectSelectedTif(path: string) {
  // 每次重选文件都重新读取图层信息，确保坐标编辑基于最新输入。
  try {
    setStatus("正在读取图层信息...");
    await invoke<TifInfo>("inspect_tif", {
      path,
      gdalBinDir: detectedGdalBinDir,
    });
    hasInspectedInfo = true;
    await refreshBoundsPreview();
    toggleGcjFields();
    setStatus("图层信息读取成功");
  } catch (error) {
    const message = String(error);
    hasInspectedInfo = false;
    currentBoundsPreview = null;
    toggleGcjFields();
    setStatus(`读取坐标失败: ${message}`);
    appendLog(`[inspect] ${message}`);
  }
}

async function selectInputFile() {
  // 使用系统文件选择器限定 TIF，减少手输路径出错概率。
  try {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [{ name: "TIF", extensions: ["tif", "tiff"] }],
    });
    if (!selected || Array.isArray(selected)) return;

    byId<HTMLInputElement>("input-path").value = selected;
    hasInspectedInfo = false;
    currentBoundsPreview = null;
    toggleGcjFields();
    appendLog(`[pick] input: ${selected}`);
    await inspectSelectedTif(selected);
  } catch (error) {
    const message = String(error);
    setStatus(`选择输入文件失败: ${message}`);
    appendLog(`[pick] input failed: ${message}`);
  }
}

async function onRunTiling() {
  // 启动切图前先建立进度监听，避免错过早期事件。
  if (isRunning) {
    return;
  }

  try {
    setRunning(true);
    if (!envReady) {
      throw new Error("请先安装 GDAL 后点击「重新检测环境」");
    }
    await ensureProgressListener();
    const request = await buildJobRequest();
    currentJobId = request.jobId;
    if (currentBoundsPreview) {
      appendLog(
        `[coord] 展示坐标系=${currentBoundsPreview.targetCrs} UL(${currentBoundsPreview.upperLeft[0]}, ${currentBoundsPreview.upperLeft[1]}) LR(${currentBoundsPreview.lowerRight[0]}, ${currentBoundsPreview.lowerRight[1]})`,
      );
    }
    appendLog(`[job] 任务分支坐标系=${request.targetTileCrs}`);
    appendLog(`[job] skipDiskSpaceCheck=${request.skipDiskSpaceCheck}`);
    setStatus("任务已启动");
    setTileLink(null);
    setProgress(0);
    appendLog(`[job] start ${request.jobId}`);
    toggleGcjFields();
    void refreshBoundsPreview();

    const result = await invoke<TileJobResult>("run_tiling", { request });
    appendLog(`[job] done: ${result.outputDir}`);
    appendLog(`[job] tile URL: ${result.tileTemplate}`);
    if (result.uploadSummary) {
      appendLog(
        `[upload] 成功=${result.uploadSummary.uploadedCount}, 失败=${result.uploadSummary.failedCount}, 前缀=${result.uploadSummary.publicBaseUrl}`,
      );
      if (result.uploadSummary.sampleTileUrl) {
        appendLog(`[upload] 示例瓦片URL: ${result.uploadSummary.sampleTileUrl}`);
      }
      if (result.uploadSummary.uploadedCount > 0) {
        const ext = inferTileExtension(result);
        const tilePattern = result.uploadSummary.publicBaseUrl + `{z}/{x}/{y}.${ext}`;
        appendLog(`[upload] 瓦片访问模板: ${tilePattern}`);
        setTileLink(tilePattern);
      }
      if (result.uploadSummary.failedCount > 0) {
        appendLog(
          `[upload] 失败文件示例:\n${result.uploadSummary.failures
            .slice(0, 5)
            .map((item) => `${item.filePath}: ${item.error}`)
            .join("\n")}`,
        );
      }
    }
    if (result.generatedIntermediateFiles.length === 0) {
      // 成功任务默认已执行清理；空列表表示没有残留中间文件。
      appendLog("[job] 中间文件已自动清理");
    } else {
      appendLog(`[job] 保留的中间文件（清理失败，可手动删除）:\n${result.generatedIntermediateFiles.join("\n")}`);
    }
    appendLog(`[job] branch=${request.targetTileCrs}`);
    if (result.uploadSummary && result.uploadSummary.failedCount > 0) {
      setStatus("瓦片生成完成，部分文件上传失败");
    } else if (result.uploadSummary && result.uploadSummary.uploadedCount > 0) {
      const ext = inferTileExtension(result);
      setStatus("瓦片生成并上传成功");
      setTileLink(result.uploadSummary.publicBaseUrl + `{z}/{x}/{y}.${ext}`);
    } else {
      setStatus("瓦片生成完成");
    }
    pendingUploadContext = null;
    popupNotice("切图任务已完成！请复制上方的瓦片链接使用。");
    setProgress(100);
  } catch (error) {
    const message = String(error);
    if (isDiskSpaceCheckError(message)) {
      const diskHint = "磁盘空间不足，请降低缩放级别或减小范围后重试";
      setStatus(diskHint);
      appendLog("[hint] 磁盘空间检查失败");
      popupNotice(diskHint);
    } else {
      const failureNotice = "切图失败：" + message;
      setStatus("切图失败");
      popupNotice(failureNotice);
    }
    appendLog(`[job] failed: ${message}`);
    setTileLink(null);
  } finally {
    setRunning(false);
  }
}

async function onCancel() {
  if (!currentJobId) {
    setStatus("当前没有可取消的任务");
    return;
  }

  try {
    const cancelled = await invoke<boolean>("cancel_job", { jobId: currentJobId });
    if (cancelled) {
      setStatus("已取消任务并清理文件");
      appendLog(`[job] cancelled ${currentJobId}`);
      setProgress(0);
      currentJobId = "";
    } else {
      setStatus("任务不存在或已结束");
    }
  } catch (error) {
    const message = String(error);
    setStatus(`取消失败: ${message}`);
    appendLog(`[job] cancel failed: ${message}`);
  }
}

function toggleGcjFields() {
  // 始终显示坐标区，任务进行时用户需要看到进度和坐标
  byId<HTMLDivElement>("gcj-fields").style.display = "grid";
  byId<HTMLElement>("gcj-hint").style.display = "none";
}

async function refreshBoundsPreview() {
  // 每次切换目标坐标系都向后端按需计算，确保展示值与实际流程同源。
  if (!hasInspectedInfo) return;
  const inputPath = byId<HTMLInputElement>("input-path").value.trim();
  if (!inputPath) return;

  const targetTileCrs = byId<HTMLSelectElement>("target-crs").value as TargetCrs;
  // Mapbox 场景用于定位核对时优先展示经纬度，因此这里改为请求 WGS84 预览。
  const previewCrs: PreviewCrs = targetTileCrs === "gcj02" ? "gcj02" : "wgs84";
  try {
    const preview = await invoke<BoundsPreview>("preview_bounds_by_crs", {
      path: inputPath,
      targetCrs: previewCrs,
      gdalBinDir: detectedGdalBinDir,
    });
    currentBoundsPreview = preview;
    renderBoundsPreview(preview);
    appendLog(
      `[coord] 预览展示坐标系=${preview.targetCrs} UL(${preview.upperLeft[0]}, ${preview.upperLeft[1]}) LR(${preview.lowerRight[0]}, ${preview.lowerRight[1]})`,
    );
  } catch (error) {
    const message = String(error);
    setStatus(`坐标预览计算失败: ${message}`);
    appendLog(`[coord] preview failed: ${message}`);
  }
}

function renderEnvStatus() {
  // 环境状态卡片统一展示检测结果与下一步动作，减少“为什么不能运行”的心智负担。
  const summary = byId<HTMLElement>("env-summary");
  const details = byId<HTMLPreElement>("env-details");
  if (gdalCheckResults.length === 0) {
    summary.textContent = "尚未检测 GDAL 环境。";
    details.textContent = "";
    envReady = false;
  } else {
    const failed = gdalCheckResults.filter((item) => !item.ok);
    envReady = failed.length === 0;
    summary.textContent = envReady
      ? `环境检测通过，当前 GDAL bin: ${detectedGdalBinDir ?? "已通过 PATH 自动解析"}`
      : `环境缺失：${failed.map((item) => item.tool).join(", ")}。请在本机手动安装 GDAL（含上述命令）后再次检测。`;
    details.textContent = gdalCheckResults
      .map((item) => `${item.ok ? "OK" : "ERR"} ${item.tool} ${item.detail}`)
      .join("\n");
  }
  syncPageStage();
  if (envReady) {
    void ensureOutputBaseDirInitialized();
  }
  setRunning(isRunning);
}

async function refreshEnvironmentCheck() {
  // 检测时先尝试后端自动探测目录，再按该目录执行工具自检；缺失时仅提示用户自行安装。
  try {
    const autoDetected = await invoke<string | null>("detect_gdal_bin_dir");
    detectedGdalBinDir = autoDetected ?? null;
    gdalCheckResults = await invoke<GdalCheckItem[]>("check_gdal_tools", {
      gdalBinDir: detectedGdalBinDir,
      gdal2tilesCmd: null,
    });
    renderEnvStatus();
    appendLog(
      `[env] checked; bin=${detectedGdalBinDir ?? "PATH"}; ready=${gdalCheckResults.every((item) => item.ok)}`,
    );
  } catch (error) {
    const message = String(error);
    gdalCheckResults = [];
    envReady = false;
    setStatus(`环境检测失败: ${message}`);
    appendLog(`[env] check failed: ${message}`);
    renderEnvStatus();
  }
}

window.addEventListener("DOMContentLoaded", () => {
  byId<HTMLButtonElement>("pick-input-btn").addEventListener("click", () => void selectInputFile());
  byId<HTMLButtonElement>("pick-output-btn").addEventListener("click", () => void selectOutputBaseFolder());
  byId<HTMLButtonElement>("reset-output-temp-btn").addEventListener("click", () => void resetOutputBaseToSystemTemp());
  byId<HTMLButtonElement>("run-btn").addEventListener("click", onRunTiling);
  byId<HTMLButtonElement>("cancel-btn").addEventListener("click", onCancel);
  byId<HTMLButtonElement>("copy-tile-link-btn").addEventListener("click", async () => {
    const value = byId<HTMLInputElement>("tile-link-value").value.trim();
    // 复制动作必须给出显式结果，避免用户误以为已复制成功。
    if (!value) {
      const emptyHint = "当前没有可复制的瓦片链接";
      setStatus(emptyHint);
      popupNotice(emptyHint);
      appendLog("[copy] skipped: empty tile link");
      return;
    }
    try {
      await navigator.clipboard.writeText(value);
      const successHint = "复制成功：线上瓦片链接已写入剪贴板";
      setStatus(successHint);
      popupNotice(successHint);
      appendLog("[copy] success");
    } catch (error) {
      const message = String(error);
      const failHint = "复制失败，请手动复制链接";
      setStatus(failHint);
      popupNotice(failHint);
      appendLog(`[copy] failed: ${message}`);
    }
  });
  byId<HTMLButtonElement>("env-recheck-btn").addEventListener("click", () => void refreshEnvironmentCheck());
  byId<HTMLSelectElement>("target-crs").addEventListener("change", () => {
    void refreshBoundsPreview();
    toggleGcjFields();
  });
  setRunning(false);
  toggleGcjFields();
  syncPageStage();
  void refreshEnvironmentCheck();
  void listen<string[]>("deep-link-url", (event) => {
    const urls = event.payload ?? [];
    if (urls.length > 0) {
      applyDeepLinkUrls(urls);
    }
  });
  void consumePendingDeepLinks();
  deepLinkPollTimer = window.setInterval(() => void pollPendingDeepLinks(), 1500);

  // 固定频率批量刷新日志，降低高频进度事件对主线程的占用。
  logFlushTimer = window.setInterval(flushLogBuffer, 120);
});

window.addEventListener("beforeunload", () => {
  if (logFlushTimer !== null) {
    window.clearInterval(logFlushTimer);
    logFlushTimer = null;
  }
  if (deepLinkPollTimer !== null) {
    window.clearInterval(deepLinkPollTimer);
    deepLinkPollTimer = null;
  }
});
