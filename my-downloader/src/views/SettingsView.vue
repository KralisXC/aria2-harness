<script setup lang="ts">
/**
 * 设置弹窗（beta7）：由 `App.vue` 常驻渲染，`open` 控制显示。
 *
 * 数据流红线：
 * - 所有输入只写 `draft`（草稿），绝不直接绑定 Store 或立刻下发引擎；
 * - 切换左侧分类**绝不清空草稿**：右侧已填写的值跨分类保留（draft 只在这里重建）；
 * - 只有点击「保存」，才把所有改动一次性下发并关闭弹窗（没有自动保存）；
 * - 点右上角 X / 遮罩 / Esc 时若有未保存改动，弹「是 / 否」确认框：
 *   「是」= 只关掉提示框、留在设置里；「否」= 放弃改动并关闭整个设置弹窗。
 */
import { computed, onMounted, reactive, ref, watch } from "vue";
import { NAlert, NModal, NSelect, NSwitch } from "naive-ui";
import AppIcon from "../components/AppIcon.vue";
import { errText, invoke } from "../ipc";
import {
  readDeleteBehavior,
  writeDeleteBehavior,
  type DeleteBehavior,
} from "../utils/deleteBehavior";
import { useSystemStore } from "../stores/system";
import { useThemeStore, type ThemeMode } from "../stores/theme";

const props = defineProps<{ open: boolean }>();
const emit = defineEmits<{ close: [] }>();

const themeStore = useThemeStore();
const system = useSystemStore();

type ProxyType = "none" | "http";
type Slicing = "fast" | "balanced" | "eco";
type CloseBehavior = "tray" | "quit";

interface Category {
  key: string;
  title: string;
  subtitle: string;
  icon: string;
  color: string;
  soft: string;
}

const CATEGORIES: Category[] = [
  {
    key: "general",
    title: "通用",
    subtitle: "下载目录、启动行为",
    icon: "settings",
    color: "#14b8a6",
    soft: "#e6f7f5",
  },
  {
    key: "download",
    title: "下载",
    subtitle: "并发、限速",
    icon: "download",
    color: "#3b82f6",
    soft: "#eaf1fe",
  },
  {
    key: "appearance",
    title: "外观",
    subtitle: "主题、关闭行为",
    icon: "dashboard",
    color: "#8b5cf6",
    soft: "#f1ecfe",
  },
  {
    key: "bt",
    title: "BitTorrent",
    subtitle: "Tracker、本地发现",
    icon: "seed",
    color: "#f59e0b",
    soft: "#fdf3e2",
  },
  {
    key: "network",
    title: "网络",
    subtitle: "代理、RPC 端口",
    icon: "link",
    color: "#06b6d4",
    soft: "#e6f8fb",
  },
  {
    key: "advanced",
    title: "高级",
    subtitle: "切割策略、请求头",
    icon: "shield",
    color: "#ec4899",
    soft: "#fdecf5",
  },
  {
    key: "integration",
    title: "集成",
    subtitle: "插件、审计日志",
    icon: "plugin",
    color: "#10b981",
    soft: "#e8f8f1",
  },
  {
    key: "about",
    title: "关于",
    subtitle: "版本、开源组件",
    icon: "info",
    color: "#64748b",
    soft: "#eef2f4",
  },
];

/** 并发数固定预设档位：避免手输 256 这类危险值。 */
const CONCURRENCY_OPTIONS = [1, 2, 4, 8, 16, 32, 64, 128].map((value) => ({
  label: String(value),
  value,
}));

/**
 * 默认 UA 伪装：与 aria2_service.rs 的启动参数保持一致（Chrome 122）。
 * 必须是**纯浏览器 UA** —— WAF 会按 `aria2` / `Harness` 等关键词拦截下载器。
 */
const DEFAULT_USER_AGENT =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

const UA_PRESETS: { label: string; value: string }[] = [
  { label: "Chrome 122（默认）", value: DEFAULT_USER_AGENT },
  {
    label: "Chrome 120",
    value:
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
  },
  {
    label: "Firefox 121",
    value: "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:121.0) Gecko/20100101 Firefox/121.0",
  },
  { label: "自定义 / 留空", value: "" },
];

const PROXY_OPTIONS: { label: string; value: ProxyType }[] = [
  { label: "不使用代理", value: "none" },
  // aria2 只支持 HTTP 代理（源码无 SOCKS 实现），这里不提供 SOCKS5 以免误选。
  { label: "HTTP", value: "http" },
];

const SLICING_OPTIONS: { label: string; value: Slicing }[] = [
  { label: "极速（积极抢占）", value: "fast" },
  { label: "均衡（默认）", value: "balanced" },
  { label: "省流（少抢占）", value: "eco" },
];

const THEME_OPTIONS: { label: string; value: ThemeMode }[] = [
  { label: "亮色", value: "light" },
  { label: "暗色", value: "dark" },
  { label: "跟随系统", value: "system" },
];

/** 切割策略 → 引擎真实参数（dynamic-slicing-*）。 */
const SLICING_PRESETS: Record<Slicing, { stall: number; preempts: number }> = {
  fast: { stall: 6, preempts: 4 },
  balanced: { stall: 12, preempts: 2 },
  eco: { stall: 24, preempts: 1 },
};

/** 删除任务时的行为（与下载页确认弹窗共用 `aria2-harness-delete-behavior`）。 */
const DELETE_BEHAVIOR_OPTIONS: { label: string; value: DeleteBehavior }[] = [
  { label: "每次询问", value: "ask" },
  { label: "仅删除任务记录", value: "task-only" },
  { label: "同时删除本地文件", value: "task-and-file" },
];

const deleteBehavior = ref<DeleteBehavior>(readDeleteBehavior());

/** 下拉框回调：只接受三个合法值，防止 Naive UI 的宽泛类型污染偏好。 */
function onDeleteBehaviorChange(value: string) {
  if (value === "ask" || value === "task-only" || value === "task-and-file") {
    deleteBehavior.value = value;
  }
}

// 下拉框改动即时落盘：下载页下次点删除就会按新偏好执行（不必等「保存」）。
watch(deleteBehavior, (value) => writeDeleteBehavior(value));

/** 已确认生效的值：保存成功后才更新，取消绝不触碰。 */
const saved = reactive({
  // 默认空：真实值由宿主 `config.json` 回填（后端缺省为系统「下载」文件夹），
  // 不写死任何构建机路径。
  dir: "",
  resumeLast: true,
  autostart: false,
  splitSize: 64,
  maxConnPerServer: 16,
  globalDownLimit: "0",
  perDownLimit: "0",
  closeBehavior: "tray" as CloseBehavior,
  proxyType: "none" as ProxyType,
  proxyAddr: "",
  showAdvanced: false,
  slicing: "balanced" as Slicing,
  userAgent: DEFAULT_USER_AGENT,
  referer: "",
  headers: "",
  // HTTP/1.1 兼容模式：存宿主本地配置，默认关闭。
  http1Compat: false,
});

interface DraftSettings {
  dir: string;
  resumeLast: boolean;
  autostart: boolean;
  splitSize: number;
  maxConnPerServer: number;
  globalDownLimit: string;
  perDownLimit: string;
  theme: ThemeMode;
  closeBehavior: CloseBehavior;
  proxyType: ProxyType;
  proxyAddr: string;
  showAdvanced: boolean;
  slicing: Slicing;
  userAgent: string;
  referer: string;
  headers: string;
  trackerInject: boolean;
  http1Compat: boolean;
}

/** 弹窗草稿：打开时深拷贝当前状态，关闭即整体丢弃。 */
const draft = reactive<DraftSettings>({
  dir: "",
  resumeLast: true,
  autostart: false,
  splitSize: 64,
  maxConnPerServer: 16,
  globalDownLimit: "0",
  perDownLimit: "0",
  theme: "light",
  closeBehavior: "tray",
  proxyType: "none",
  proxyAddr: "",
  showAdvanced: false,
  slicing: "balanced",
  userAgent: DEFAULT_USER_AGENT,
  referer: "",
  headers: "",
  trackerInject: false,
  http1Compat: false,
});

/** UA 预设选择：命中预设即显示预设名，否则落到「自定义」。 */
const uaPreset = computed<string>({
  get: () => UA_PRESETS.find((p) => p.value === draft.userAgent)?.value ?? "",
  set: (value) => {
    if (value !== "") draft.userAgent = value;
    else if (UA_PRESETS.some((p) => p.value === "")) draft.userAgent = "";
  },
});

const activeKey = ref<string>("general");
const settingsSaving = ref(false);
const settingsMessage = ref("");

/** 打开弹窗那一刻的草稿快照：与当前草稿不同即「有未保存的改动」。 */
const baseline = ref("");
/** 未保存拦截确认框（是=留在原地，否=放弃并关闭设置）。 */
const confirmCloseOpen = ref(false);

function draftSignature(): string {
  return JSON.stringify({ ...draft });
}

const isDirty = computed(
  () => baseline.value !== "" && draftSignature() !== baseline.value,
);

interface ApiInfo {
  port: number;
  url: string;
}

/** 本机接管 API 的真实端口（设置 → 网络展示；密钥由扩展自动握手，UI 不再暴露）。 */
const apiInfo = reactive<ApiInfo>({ port: 0, url: "" });

/** 浏览器扩展安装引导（设置 → 网络：「安装浏览器扩展」按钮 + 图文弹窗）。 */
const extensionOpen = ref(false);
const extensionInfo = reactive({ path: "", exists: false });
const extensionCopied = ref(false);
let extensionCopyTimer: number | undefined;

const activeCategory = computed(
  () => CATEGORIES.find((item) => item.key === activeKey.value) ?? null,
);

/** 纯展示分类：没有输入框 / 开关，底部不允许出现「保存」。 */
const isReadOnlyCategory = computed(
  () => activeKey.value === "about" || activeKey.value === "integration",
);

/** 用已确认值 + 当前主题 / Tracker 状态填充草稿。 */
function resetDraft() {
  Object.assign(draft, {
    dir: saved.dir,
    resumeLast: saved.resumeLast,
    autostart: saved.autostart,
    splitSize: saved.splitSize,
    maxConnPerServer: saved.maxConnPerServer,
    globalDownLimit: saved.globalDownLimit,
    perDownLimit: saved.perDownLimit,
    theme: themeStore.mode,
    closeBehavior: saved.closeBehavior,
    proxyType: saved.proxyType,
    proxyAddr: saved.proxyAddr,
    showAdvanced: saved.showAdvanced,
    slicing: saved.slicing,
    userAgent: saved.userAgent,
    referer: saved.referer,
    headers: saved.headers,
    trackerInject: system.trackers?.injecting ?? false,
    http1Compat: saved.http1Compat,
  });
}

/**
 * 切换分类：**只切视图，草稿原样保留**。
 *
 * 这是 beta7 的硬性要求：左侧菜单来回切换时，右侧已填写的表单数据绝不能重置；
 * 草稿只由 `hydrate()`（打开弹窗）与「重置」重建。
 */
function selectCategory(key: string) {
  if (activeKey.value === key) return;
  activeKey.value = key;
  settingsMessage.value = "";
  // 打开与运行态强相关的分类时刷新一次，避免展示启动瞬间的空值。
  if (key === "network") void loadApiInfo();
}

/** 重置：只丢弃草稿，不触碰已保存值。 */
function discardDraft() {
  resetDraft();
  settingsMessage.value = "";
}

/** 限速值：允许 0 / 500K / 5M / 1.5M 这类写法。 */
const SPEED_PATTERN = /^\d+(\.\d+)?[KMG]?$/i;

function normalizeLimit(raw: string): string | null {
  const value = raw.trim();
  if (value === "") return "0";
  return SPEED_PATTERN.test(value) ? value.toUpperCase() : null;
}

/** 代理地址：host:port（主机名允许字母数字、点、横线、下划线）。 */
const PROXY_ADDR_PATTERN = /^[\w.-]+:\d{1,5}$/;

function buildProxyValue(type: ProxyType, addr: string): string | null {
  if (type === "none") return "";
  if (!PROXY_ADDR_PATTERN.test(addr.trim())) return null;
  return `${type}://${addr.trim()}`;
}

/** 请求头：逐行清洗，只保留 `Key: Value` 形式。 */
function normalizeHeaders(raw: string): string {
  return raw
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.includes(":") && line.indexOf(":") > 0)
    .join("\n");
}

/** 下发一次 changeGlobalOption（失败保留弹窗并展示原因）。 */
async function pushOptions(options: Record<string, string>): Promise<boolean> {
  settingsSaving.value = true;
  try {
    await invoke("change_global_option", { options });
    return true;
  } catch (error) {
    settingsMessage.value = `保存失败：${errText(error)}`;
    return false;
  } finally {
    settingsSaving.value = false;
  }
}

/** 写宿主本地配置（`<HOME>/config.json`），只合并 patch 中的字段。 */
async function pushHostConfig(patch: Record<string, unknown>): Promise<boolean> {
  settingsSaving.value = true;
  try {
    await invoke("set_host_config", { config: patch });
    return true;
  } catch (error) {
    settingsMessage.value = `保存失败：${errText(error)}`;
    return false;
  } finally {
    settingsSaving.value = false;
  }
}

/** `config.json` 的结构（只列设置页关心的字段；未知字段由后端原样保留）。 */
interface HostConfig {
  download_dir?: string;
  resume_last?: boolean;
  close_behavior?: string;
  split_size?: number;
  max_connections_per_server?: number;
  max_overall_download_limit?: string;
  max_download_limit?: string;
  proxy?: string;
  user_agent?: string;
  referer?: string;
  header?: string;
  dynamic_slicing_stall?: number;
  show_advanced?: boolean;
  enable_http1_compat?: boolean;
}

/** 运行期关键目录（插件 / 数据）：由宿主解析，前端不写死任何绝对路径。 */
const appPaths = ref<{ plugins?: string; data?: string }>({});

async function loadAppPaths() {
  try {
    appPaths.value = await invoke("get_app_paths");
  } catch {
    // 宿主未就绪：保持空串，模板回退占位符。
  }
}

/**
 * 设置页唯一的回显数据源：宿主 `config.json`。
 *
 * 不再读 localStorage：安装包升级会重建 WebView 存储目录，而 `%LOCALAPPDATA%`
 * 下的 config.json 由安装器明确保留 —— 这才是能跨版本存活的持久层。
 * 引擎的 `getGlobalOption` 也不可用于回显：它只反映当前 aria2c 进程。
 */
async function loadHostConfig() {
  try {
    const config = await invoke<HostConfig>("get_host_config");
    if (typeof config.download_dir === "string" && config.download_dir.trim()) {
      saved.dir = config.download_dir;
    }
    if (typeof config.resume_last === "boolean") saved.resumeLast = config.resume_last;
    if (config.close_behavior === "tray" || config.close_behavior === "quit") {
      saved.closeBehavior = config.close_behavior;
    }
    if (typeof config.show_advanced === "boolean") saved.showAdvanced = config.show_advanced;
    if (typeof config.max_overall_download_limit === "string") {
      saved.globalDownLimit = config.max_overall_download_limit || "0";
    }
    if (typeof config.max_download_limit === "string") {
      saved.perDownLimit = config.max_download_limit || "0";
    }
    if (config.proxy !== undefined) {
      const proxy = parseProxy(config.proxy);
      if (proxy.type === "none" || proxy.type === "http") {
        saved.proxyType = proxy.type;
        saved.proxyAddr = proxy.addr;
      }
    }
    if (typeof config.user_agent === "string" && config.user_agent.trim()) {
      saved.userAgent = config.user_agent;
    }
    if (typeof config.referer === "string") saved.referer = config.referer;
    if (typeof config.header === "string") saved.headers = config.header;
    const stall = Number(config.dynamic_slicing_stall);
    if (Number.isFinite(stall) && stall > 0) saved.slicing = slicingFrom(stall);
    // 后端已完成旧配置迁移：split_size 缺失时按旧字段语义回填，这里直接信任返回值。
    const split = Number(config.split_size);
    if (Number.isFinite(split) && split > 0) saved.splitSize = split;
    const connections = Number(config.max_connections_per_server);
    if (Number.isFinite(connections) && connections > 0) saved.maxConnPerServer = connections;
    saved.http1Compat = config.enable_http1_compat === true;
  } catch {
    // 宿主未就绪：保留默认值，不阻塞页面；保存时仍会写盘。
  }
}

/** 调起系统文件夹选择器（原生 IFileOpenDialog）。 */
async function browseDir() {
  try {
    const picked = await invoke<string | null>("pick_directory");
    if (picked) draft.dir = picked;
  } catch (error) {
    settingsMessage.value = `打开目录选择器失败：${errText(error)}`;
  }
}

/** Tracker 注入：仅当草稿与当前状态不同才真实下发。 */
async function applyTrackerInject(next: boolean): Promise<boolean> {
  const current = system.trackers?.injecting ?? false;
  if (next === current) return true;
  settingsSaving.value = true;
  try {
    if (next) {
      await system.refreshTrackers();
    } else {
      await invoke("change_global_option", { options: { "bt-tracker": "" } });
      await system.loadTrackers();
    }
    return true;
  } catch (error) {
    settingsMessage.value = `保存失败：${errText(error)}`;
    return false;
  } finally {
    settingsSaving.value = false;
  }
}

/** 开机自启：真实写 HKCU Run 键，失败时保留弹窗并展示原因。 */
async function applyAutostart(next: boolean): Promise<boolean> {
  settingsSaving.value = true;
  try {
    await invoke("set_autostart", { enabled: next });
    return true;
  } catch (error) {
    settingsMessage.value = `保存失败：${errText(error)}`;
    return false;
  } finally {
    settingsSaving.value = false;
  }
}

/** 每个分类「草稿 vs 已确认值」是否有改动（未改的分类不重复下发）。 */
const changed = computed(() => {
  const draftProxy = buildProxyValue(draft.proxyType, draft.proxyAddr) ?? "";
  const savedProxy = buildProxyValue(saved.proxyType, saved.proxyAddr) ?? "";
  return {
    general:
      draft.dir.trim() !== saved.dir ||
      draft.resumeLast !== saved.resumeLast ||
      draft.autostart !== saved.autostart,
    download:
      draft.splitSize !== saved.splitSize ||
      draft.maxConnPerServer !== saved.maxConnPerServer ||
      normalizeLimit(draft.globalDownLimit) !== saved.globalDownLimit ||
      normalizeLimit(draft.perDownLimit) !== saved.perDownLimit,
    appearance:
      draft.theme !== themeStore.mode || draft.closeBehavior !== saved.closeBehavior,
    bt: draft.trackerInject !== (system.trackers?.injecting ?? false),
    network:
      draftProxy !== savedProxy ||
      draft.userAgent.trim() !== saved.userAgent.trim() ||
      draft.referer.trim() !== saved.referer ||
      draft.http1Compat !== saved.http1Compat,
    advanced:
      draft.slicing !== saved.slicing ||
      normalizeHeaders(draft.headers) !== saved.headers ||
      draft.showAdvanced !== saved.showAdvanced,
  };
});

/**
 * 保存（beta7）：一次性提交**所有分类**的改动，成功后关闭弹窗。
 *
 * 只对真正与已确认值不同的分类下发（未改的分类不重复打扰引擎）；
 * 任一步失败都跳到对应分类并保留弹窗，绝不静默丢弃用户输入。
 */
async function save() {
  if (settingsSaving.value) return;
  settingsMessage.value = "";
  const diff = changed.value;
  const savedKeys: string[] = [];

  if (diff.general) {
    const dir = draft.dir.trim();
    if (!dir) {
      activeKey.value = "general";
      settingsMessage.value = "下载目录不能为空";
      return;
    }
    if (!(await pushOptions({ dir }))) {
      activeKey.value = "general";
      return;
    }
    if (draft.autostart !== saved.autostart && !(await applyAutostart(draft.autostart))) {
      activeKey.value = "general";
      return;
    }
    saved.dir = dir;
    saved.resumeLast = draft.resumeLast;
    saved.autostart = draft.autostart;
    savedKeys.push("general");
  }

  if (diff.download) {
    const globalLimit = normalizeLimit(draft.globalDownLimit);
    const perLimit = normalizeLimit(draft.perDownLimit);
    if (globalLimit === null || perLimit === null) {
      activeKey.value = "download";
      settingsMessage.value = "限速格式无效，示例：5M / 500K / 0（0 表示不限速）";
      return;
    }
    const ok = await pushOptions({
      // 两个参数完全独立：`split` 是预先切块数，`max-connection-per-server` 是单服并发。
      // 引擎已解封到 128，这里不再做 16 的夹取 —— 用户选多少就下发多少。
      split: String(draft.splitSize),
      "max-connection-per-server": String(draft.maxConnPerServer),
      "max-overall-download-limit": globalLimit,
      "max-download-limit": perLimit,
    });
    if (!ok) {
      activeKey.value = "download";
      return;
    }
    saved.splitSize = draft.splitSize;
    saved.maxConnPerServer = draft.maxConnPerServer;
    saved.globalDownLimit = globalLimit;
    saved.perDownLimit = perLimit;
    savedKeys.push("download");
  }

  if (diff.appearance) {
    // 关闭行为必须真正下发到宿主主循环，否则点 X 仍会直接退出。
    settingsSaving.value = true;
    try {
      await invoke("set_close_behavior", { mode: draft.closeBehavior });
    } catch (error) {
      activeKey.value = "appearance";
      settingsMessage.value = `保存失败：${errText(error)}`;
      return;
    } finally {
      settingsSaving.value = false;
    }
    themeStore.setMode(draft.theme);
    saved.closeBehavior = draft.closeBehavior;
    savedKeys.push("appearance");
  }

  if (diff.bt) {
    if (!(await applyTrackerInject(draft.trackerInject))) {
      activeKey.value = "bt";
      return;
    }
    savedKeys.push("bt");
  }

  if (diff.network) {
    const proxy = buildProxyValue(draft.proxyType, draft.proxyAddr);
    if (proxy === null) {
      activeKey.value = "network";
      settingsMessage.value = "代理地址格式无效，示例：127.0.0.1:7890";
      return;
    }
    const options: Record<string, string> = { "all-proxy": proxy };
    const userAgent = draft.userAgent.trim();
    const referer = draft.referer.trim();
    // 留空表示不下发（保留引擎当前值），避免把空 UA 写进请求头。
    if (userAgent) options["user-agent"] = userAgent;
    // Referer 允许清空：空串写回引擎即关闭全局兜底，内置智能匹配随即可用。
    options.referer = referer;
    if (!(await pushOptions(options))) {
      activeKey.value = "network";
      return;
    }
    saved.proxyType = draft.proxyType;
    saved.proxyAddr = draft.proxyAddr.trim();
    if (userAgent) saved.userAgent = userAgent;
    saved.referer = referer;
    saved.http1Compat = draft.http1Compat;
    savedKeys.push("network");
  }

  if (diff.advanced) {
    const preset = SLICING_PRESETS[draft.slicing];
    const headers = normalizeHeaders(draft.headers);
    const ok = await pushOptions({
      "dynamic-slicing-stall": String(preset.stall),
      "dynamic-slicing-max-preempts": String(preset.preempts),
      header: headers,
    });
    if (!ok) {
      activeKey.value = "advanced";
      return;
    }
    saved.slicing = draft.slicing;
    saved.headers = headers;
    saved.showAdvanced = draft.showAdvanced;
    savedKeys.push("advanced");
  }

  // 引擎只保留「当前进程」的全局选项 → 必须同步落盘，覆盖安装 / 引擎重启后才能重放。
  const patch = savedKeys.reduce<Record<string, unknown>>(
    (acc, key) => Object.assign(acc, buildConfigPatch(key)),
    {},
  );
  if (Object.keys(patch).length > 0 && !(await pushHostConfig(patch))) return;

  // 显式保存成功 → 关闭弹窗（没有自动保存，也没有"保存后留在原地"）。
  resetDraft();
  baseline.value = draftSignature();
  emit("close");
}

// ---------------------------------------------------------------------------
// 未保存拦截：X / 遮罩 / Esc 统一走这里
// ---------------------------------------------------------------------------

/** naive-ui 的关闭请求（X / 遮罩 / Esc）统一在此拦截。 */
function onShowChange(show: boolean) {
  if (show) return;
  requestClose();
}

function requestClose() {
  // 保存进行中不接受关闭请求，避免半途中断留下不一致的下发状态。
  if (settingsSaving.value) return;
  if (!isDirty.value) {
    confirmCloseOpen.value = false;
    emit("close");
    return;
  }
  confirmCloseOpen.value = true;
}

/** 「是」：只关掉提示框，不保存、不关闭设置弹窗（留在原地手动保存）。 */
function keepEditing() {
  confirmCloseOpen.value = false;
}

/** 「否」：关掉提示框，不保存，直接关闭整个设置弹窗。 */
function discardAndClose() {
  confirmCloseOpen.value = false;
  emit("close");
}

/** 已确认值 → `config.json` 补丁（键名与 src-host/src/lib.rs 的读取逻辑一一对应）。 */
function buildConfigPatch(key: string | undefined): Record<string, unknown> {
  switch (key) {
    case "general":
      return { download_dir: saved.dir, resume_last: saved.resumeLast };
    case "download":
      return {
        split_size: saved.splitSize,
        max_connections_per_server: saved.maxConnPerServer,
        max_overall_download_limit: saved.globalDownLimit,
        max_download_limit: saved.perDownLimit,
      };
    case "appearance":
      return { close_behavior: saved.closeBehavior };
    case "network": {
      const patch: Record<string, unknown> = {
        // 空串是显式意图：重启后清掉服务自动探测的系统代理。
        proxy: buildProxyValue(saved.proxyType, saved.proxyAddr) ?? "",
        referer: saved.referer,
        enable_http1_compat: saved.http1Compat,
      };
      // 空 UA 不下发也不落盘：表示沿用引擎内置的浏览器伪装。
      if (saved.userAgent.trim()) patch.user_agent = saved.userAgent.trim();
      return patch;
    }
    case "advanced": {
      const preset = SLICING_PRESETS[saved.slicing];
      return {
        dynamic_slicing_stall: preset.stall,
        dynamic_slicing_max_preempts: preset.preempts,
        header: saved.headers,
        show_advanced: saved.showAdvanced,
      };
    }
    default:
      return {};
  }
}

function parseProxy(raw: string): { type: ProxyType; addr: string } {
  if (!raw) return { type: "none", addr: "" };
  const match = /^http:\/\/(.+)$/i.exec(raw);
  if (match) return { type: match[1].toLowerCase() as ProxyType, addr: match[2] };
  return { type: "http", addr: raw };
}

function slicingFrom(stall: number): Slicing {
  if (stall <= 8) return "fast";
  if (stall >= 20) return "eco";
  return "balanced";
}

/** 读取本机接管 API 的运行态（仅监听地址）。宿主未就绪时保留占位。 */
async function loadApiInfo() {
  try {
    const info = await invoke<ApiInfo>("get_api_info");
    apiInfo.port = Number(info.port ?? 0);
    apiInfo.url = info.url ?? "";
  } catch {
    // 引擎/宿主尚未就绪：界面上继续显示"启动中"。
  }
}

// ---------------------------------------------------------------------------
// 代理连通性测试
// ---------------------------------------------------------------------------

const proxyTesting = ref(false);
const proxyTestOpen = ref(false);
const proxyTestOk = ref(false);
const proxyTestInfo = ref("");

/**
 * 测试代理连通性：由宿主发起 TCP 握手（3s 超时），结果用弹窗明确告知。
 *
 * 只测端点可达性，不携带业务请求 —— 用户只需知道"地址能不能连上"。
 */
async function testProxy() {
  if (draft.proxyType === "none") {
    proxyTestOk.value = false;
    proxyTestInfo.value = "当前未启用代理，无需测试。";
    proxyTestOpen.value = true;
    return;
  }
  const addr = draft.proxyAddr.trim();
  if (!PROXY_ADDR_PATTERN.test(addr)) {
    proxyTestOk.value = false;
    proxyTestInfo.value = "代理地址格式无效，示例：127.0.0.1:7890";
    proxyTestOpen.value = true;
    return;
  }
  const split = addr.lastIndexOf(":");
  const host = addr.slice(0, split);
  const port = Number(addr.slice(split + 1));
  proxyTesting.value = true;
  try {
    const latency = await invoke<number>("test_proxy", { host, port });
    proxyTestOk.value = true;
    proxyTestInfo.value = `代理 ${addr} 可连通，TCP 握手耗时 ${latency} ms。`;
  } catch (error) {
    proxyTestOk.value = false;
    proxyTestInfo.value = `无法连接 ${addr}：${errText(error)}`;
  } finally {
    proxyTesting.value = false;
    proxyTestOpen.value = true;
  }
}

/** 复制文本（剪贴板 API 不可用时退化为 execCommand）。 */
async function writeClipboard(text: string) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    try {
      document.execCommand("copy");
    } catch {
      // 复制失败不阻断交互。
    }
    area.remove();
  }
}

/**
 * 打开扩展安装引导：先取扩展的真实物理目录。
 *
 * 目录随 exe 走（安装版在 Program Files、便携版在解压目录），因此必须由宿主
 * 解析，前端不能写死路径。
 */
async function openExtensionGuide() {
  extensionOpen.value = true;
  extensionCopied.value = false;
  try {
    const info = await invoke<{ path: string; exists: boolean }>(
      "get_browser_extension_info",
    );
    extensionInfo.path = info.path ?? "";
    extensionInfo.exists = info.exists === true;
  } catch {
    extensionInfo.path = "";
    extensionInfo.exists = false;
  }
}

/** 一键复制扩展目录（用户在「加载已解压的扩展程序」里粘贴）。 */
async function copyExtensionPath() {
  if (!extensionInfo.path) return;
  await writeClipboard(extensionInfo.path);
  extensionCopied.value = true;
  window.clearTimeout(extensionCopyTimer);
  extensionCopyTimer = window.setTimeout(() => {
    extensionCopied.value = false;
  }, 1600);
}

/** 读取开机自启真实状态（非 Windows 平台恒为 false）。 */
async function loadAutostart() {
  try {
    saved.autostart = await invoke<boolean>("get_autostart");
  } catch {
    saved.autostart = false;
  }
}

/**
 * 打开弹窗前重新水合（hydrate）：宿主 `config.json` 是唯一回显来源。
 *
 * 必须先等 `loadTrackers()` 落地再重建草稿：Tracker 注入开关的真值来自运行态，
 * 否则首次打开会把「已注入」误判成「关闭」，保存时反而清空 Tracker。
 */
async function hydrate() {
  settingsMessage.value = "";
  confirmCloseOpen.value = false;
  // 唯一回显来源：宿主 config.json（IPC）。不再读 localStorage，也不回读引擎全局值。
  await loadHostConfig();
  await Promise.all([loadAutostart(), system.loadTrackers()]);
  void loadAppPaths();
  void loadApiInfo();
  resetDraft();
  baseline.value = draftSignature();
}

onMounted(async () => {
  await hydrate();
  // 把已保存的关闭行为同步给宿主主循环：否则上次选择「直接退出」在重启后会丢失。
  void invoke("set_close_behavior", { mode: saved.closeBehavior }).catch(() => {});
});

// 每次打开都重新读一遍宿主配置：外部（另一个进程 / 手动改 config.json）的变更
// 不会在弹窗里显示成旧值；关闭时顺手收起残留的确认框。
watch(
  () => props.open,
  (open) => {
    if (!open) {
      confirmCloseOpen.value = false;
      return;
    }
    void hydrate();
  },
);
</script>

<template>
  <!-- 设置弹窗：主界面保留在背后，居中弹出宽大卡片；X / 遮罩 / Esc 统一走关闭拦截。 -->
  <n-modal
    :show="props.open"
    preset="card"
    class="settings-modal"
    title="设置"
    style="width: min(1040px, 94vw)"
    :bordered="false"
    :closable="true"
    :mask-closable="true"
    :close-on-esc="true"
    @update:show="onShowChange"
  >
    <!-- 左菜单 + 右内容：右侧内容区独立滚动 -->
    <section class="settings">
      <section class="settings-layout">
      <!-- 左侧分类菜单：固定，不跟随右侧内容滚动 -->
      <aside class="settings-nav" aria-label="设置分类">
        <button
          v-for="item in CATEGORIES"
          :key="item.key"
          class="nav-item"
          :class="{ active: activeKey === item.key }"
          @click="selectCategory(item.key)"
        >
          <span class="cat-icon" :style="{ background: item.soft, color: item.color }">
            <AppIcon :name="item.icon" :size="17" />
          </span>
          <span class="nav-text">
            <b>{{ item.title }}</b>
            <small>{{ item.subtitle }}</small>
          </span>
          <AppIcon name="chevron" :size="13" class="nav-arrow" />
        </button>
      </aside>

      <div v-if="activeCategory" class="settings-panel">
        <div class="panel-body">
          <!-- 通用 -->
          <div v-if="activeCategory.key === 'general'" class="field-group">
            <label class="field">
              <span><AppIcon name="folder" :size="13" class="field-icon" /> 默认下载目录</span>
              <div class="path-row">
                <input v-model="draft.dir" type="text" spellcheck="false" />
                <button class="btn-ghost browse" type="button" @click="browseDir">
                  <AppIcon name="folder" :size="14" /> 浏览
                </button>
              </div>
              <small class="hint">新任务默认保存到此目录，保存后下发引擎 dir 选项</small>
            </label>

            <div class="field switch-row">
              <div class="switch-copy">
                <span><AppIcon name="refresh" :size="13" class="field-icon" /> 启动时恢复上次未完成的任务</span>
                <small>下次启动 Harness Core 时继续上次队列（本地偏好）</small>
              </div>
              <n-switch :value="draft.resumeLast" @update:value="draft.resumeLast = $event" />
            </div>

            <div class="field switch-row">
              <div class="switch-copy">
                <span><AppIcon name="timer" :size="13" class="field-icon" /> 开机自动启动</span>
                <small>写入当前用户注册表 Run 项，登录 Windows 后自动拉起 Harness</small>
              </div>
              <n-switch
                :value="draft.autostart"
                @update:value="draft.autostart = $event"
              />
            </div>
          </div>

          <!-- 下载 -->
          <div v-else-if="activeCategory.key === 'download'" class="field-group">
            <div class="field">
              <span><AppIcon name="split" :size="13" class="field-icon" /> 文件分片数（Split）</span>
              <n-select
                v-model:value="draft.splitSize"
                class="concurrency-select"
                :options="CONCURRENCY_OPTIONS"
                :consistent-menu-width="false"
              />
              <small class="hint">决定文件被预先切成多少块，建议保持 64</small>
            </div>
            <div class="field">
              <span><AppIcon name="gauge" :size="13" class="field-icon" /> 单服务器最大连接数</span>
              <n-select
                v-model:value="draft.maxConnPerServer"
                class="concurrency-select"
                :options="CONCURRENCY_OPTIONS"
                :consistent-menu-width="false"
              />
              <small class="hint">决定同时对同一个服务器发起多少个请求</small>
              <n-alert
                class="danger-alert"
                type="warning"
                title="高危操作警告"
                :bordered="false"
              >
                <strong>⚠️ 将单服务器连接数设置高于 16，极易触发 CDN 反爬机制（如腾讯、阿里云等），导致您的 IP 被暂时封禁或下载失败。日常使用建议保持默认 16。</strong>
              </n-alert>
            </div>
            <div class="field">
              <span><AppIcon name="trash" :size="13" class="field-icon" /> 删除任务时</span>
              <n-select
                :value="deleteBehavior"
                class="concurrency-select"
                :options="DELETE_BEHAVIOR_OPTIONS"
                :consistent-menu-width="false"
                @update:value="onDeleteBehaviorChange"
              />
              <small class="hint">
                每次询问会在下载页弹窗确认；「同时删除本地文件」连同 .aria2 进度文件一起清理，不可恢复
              </small>
            </div>
            <label class="field">
              <span><AppIcon name="download" :size="13" class="field-icon" /> 全局下载限速</span>
              <input v-model="draft.globalDownLimit" type="text" maxlength="8" placeholder="0" />
              <small class="hint">所有任务的总下载限速，如 5M / 500K；0 表示不限速</small>
            </label>
            <label class="field">
              <span><AppIcon name="download" :size="13" class="field-icon" /> 单任务下载限速</span>
              <input v-model="draft.perDownLimit" type="text" maxlength="8" placeholder="0" />
              <small class="hint">单个任务的下载限速，如 2M；0 表示不限速</small>
            </label>
          </div>

          <!-- 外观 -->
          <div v-else-if="activeCategory.key === 'appearance'" class="field-group">
            <div class="field">
              <span><AppIcon name="settings" :size="13" class="field-icon" /> 主题模式</span>
              <div class="segmented">
                <button
                  v-for="option in THEME_OPTIONS"
                  :key="option.value"
                  type="button"
                  :class="{ active: draft.theme === option.value }"
                  @click="draft.theme = option.value"
                >
                  {{ option.label }}
                </button>
              </div>
              <small class="hint">常规皮肤下生效；跟随系统会响应 Windows 深色模式</small>
            </div>

            <div class="field">
              <span><AppIcon name="close" :size="13" class="field-icon" /> 关闭窗口时</span>
              <div class="segmented">
                <button
                  type="button"
                  :class="{ active: draft.closeBehavior === 'tray' }"
                  @click="draft.closeBehavior = 'tray'"
                >
                  最小化到托盘
                </button>
                <button
                  type="button"
                  :class="{ active: draft.closeBehavior === 'quit' }"
                  @click="draft.closeBehavior = 'quit'"
                >
                  直接退出
                </button>
              </div>
              <small class="hint">
                点击标题栏 X 时隐藏到系统托盘，下载与引擎继续运行；托盘右键菜单可随时唤回或退出
              </small>
            </div>
          </div>

          <!-- BitTorrent -->
          <div v-else-if="activeCategory.key === 'bt'" class="field-group">
            <div class="field switch-row">
              <div class="switch-copy">
                <span><AppIcon name="seed" :size="13" class="field-icon" /> 注入精选 Tracker</span>
                <small>保存后才会同步注入或清空，取消保持引擎现状</small>
              </div>
              <n-switch
                :value="draft.trackerInject"
                @update:value="draft.trackerInject = $event"
              />
            </div>
            <div class="field readonly-row">
              <span><AppIcon name="link" :size="13" class="field-icon" /> 本地发现（LPD）</span>
              <code>--bt-enable-lpd=true</code>
            </div>
          </div>

          <!-- 网络 -->
          <div v-else-if="activeCategory.key === 'network'" class="field-group">
            <div class="field">
              <span><AppIcon name="shield" :size="13" class="field-icon" /> User-Agent 预设</span>
              <n-select
                v-model:value="uaPreset"
                :options="UA_PRESETS"
                :consistent-menu-width="false"
              />
              <input
                v-model="draft.userAgent"
                type="text"
                maxlength="200"
                placeholder="留空则保留引擎当前 UA"
              />
              <small class="hint">
                默认伪装为 Chrome 122（纯浏览器 UA），对后续新任务生效，用于绕过站点 UA 校验
              </small>
            </div>
            <label class="field">
              <span><AppIcon name="link" :size="13" class="field-icon" /> 自定义 Referer（可选）</span>
              <input
                v-model="draft.referer"
                type="text"
                maxlength="200"
                placeholder="留空 = 由内置规则自动匹配"
              />
              <small class="hint">
                优先级：浏览器扩展传入 &gt; 此处设置 &gt; 内置智能匹配（qq / bilibili / weibo / github）
              </small>
            </label>

            <div class="field switch-row">
              <div class="switch-copy">
                <span><AppIcon name="shield" :size="13" class="field-icon" /> 启用 HTTP/1.1 兼容模式（针对老旧 CDN）</span>
                <small>
                  部分服务器（如腾讯 CDN）可能限制 HTTP/2 连接，若下载失败请开启此选项
                </small>
              </div>
              <n-switch
                :value="draft.http1Compat"
                @update:value="draft.http1Compat = $event"
              />
            </div>

            <div class="field">
              <span><AppIcon name="link" :size="13" class="field-icon" /> 代理设置</span>
              <div class="proxy-row">
                <n-select
                  v-model:value="draft.proxyType"
                  class="proxy-type"
                  :options="PROXY_OPTIONS"
                  :consistent-menu-width="false"
                />
                <input
                  v-if="draft.proxyType !== 'none'"
                  v-model="draft.proxyAddr"
                  type="text"
                  maxlength="48"
                  placeholder="127.0.0.1:7890"
                />
                <button
                  class="btn-ghost proxy-test"
                  type="button"
                  :disabled="proxyTesting"
                  @click="testProxy"
                >
                  <AppIcon name="gauge" :size="14" />
                  {{ proxyTesting ? "测试中..." : "测试连通性" }}
                </button>
              </div>
              <small class="hint">
                映射到引擎 all-proxy（仅 HTTP 代理）。Clash 一般填 7890，V2Ray 的
                HTTP 入站一般填 10809；SOCKS 端口无法使用
              </small>
            </div>
            <div class="field readonly-row">
              <span><AppIcon name="gauge" :size="13" class="field-icon" /> RPC 监听端口</span>
              <code>127.0.0.1:6800</code>
            </div>
            <small class="hint">端口由宿主启动参数固定，出于安全考虑不可在 UI 修改</small>

            <div class="field">
              <span><AppIcon name="plugin" :size="13" class="field-icon" /> 浏览器接管 API</span>
              <div class="readonly-row">
                <span>监听地址</span>
                <code>{{ apiInfo.url || "启动中…" }}</code>
              </div>
              <small class="hint">扩展将自动握手获取密钥，无需手动配置。</small>
            </div>

            <div class="field">
              <span><AppIcon name="download" :size="13" class="field-icon" /> 浏览器扩展</span>
              <button class="btn-primary ext-install" type="button" @click="openExtensionGuide">
                <AppIcon name="download" :size="15" />
                安装浏览器扩展
              </button>
              <small class="hint">
                装好后浏览器里点下载会自动交给本机，弹窗确认后才开始，挂机多久都不会丢
              </small>
            </div>
          </div>

          <!-- 高级 -->
          <div v-else-if="activeCategory.key === 'advanced'" class="field-group">
            <div class="field switch-row">
              <div class="switch-copy">
                <span><AppIcon name="settings" :size="13" class="field-icon" /> 显示高级 / 开发者选项</span>
                <small>开启后展示动态切割策略与自定义请求头</small>
              </div>
              <n-switch
                :value="draft.showAdvanced"
                @update:value="draft.showAdvanced = $event"
              />
            </div>

            <template v-if="draft.showAdvanced">
              <div class="field">
                <span><AppIcon name="split" :size="13" class="field-icon" /> 动态切割策略</span>
                <n-select
                  v-model:value="draft.slicing"
                  class="concurrency-select"
                  :options="SLICING_OPTIONS"
                  :consistent-menu-width="false"
                />
                <small class="hint">
                  映射 dynamic-slicing-stall / max-preempts：极速 6s·4，均衡 12s·2，省流 24s·1
                </small>
              </div>
              <label class="field">
                <span><AppIcon name="copy" :size="13" class="field-icon" /> 自定义请求头</span>
                <textarea
                  v-model="draft.headers"
                  rows="4"
                  spellcheck="false"
                  placeholder="X-My-Header: 123&#10;Accept-Language: zh-CN"
                />
                <small class="hint">每行一条，格式 Key: Value；保存时自动清洗非法行</small>
              </label>
            </template>
          </div>

          <!-- 集成 -->
          <div v-else-if="activeCategory.key === 'integration'" class="field-group">
            <div class="field readonly-row">
              <span><AppIcon name="folder" :size="13" class="field-icon" /> 插件目录</span>
              <code>{{ appPaths.plugins || "由宿主解析" }}</code>
            </div>
            <div class="field readonly-row">
              <span><AppIcon name="info" :size="13" class="field-icon" /> 审计日志</span>
              <code>data\audit\plugins.log</code>
            </div>
            <div class="field readonly-row">
              <span><AppIcon name="timer" :size="13" class="field-icon" /> 进度持久化</span>
              <code>SQLite（WAL）</code>
            </div>
            <small class="hint">插件安装、启停与权限授权请前往「插件」页面操作</small>
          </div>

          <!-- 关于：上下排版，突出"纯血 Rust 引擎" -->
          <div v-else-if="activeCategory.key === 'about'" class="field-group about-page">
            <div class="about-hero">
              <span class="brand-mark">A2</span>
              <div>
                <b>Aria2-Harness</b>
                <small>v1.0.0-beta8 · 纯血 Rust 下载引擎</small>
              </div>
            </div>

            <div class="stack-list">
              <div class="stack-row">
                <span class="stack-label">HTTP 网络引擎</span>
                <b class="stack-name">Harness-Net</b>
                <small class="stack-deps">tokio · hyper · rustls · hickory-dns</small>
              </div>
              <div class="stack-row">
                <span class="stack-label">BT 下载引擎</span>
                <b class="stack-name">aria2 魔改核心</b>
                <small class="stack-deps">C++ · nghttp2 · sqlite3</small>
              </div>
              <div class="stack-row">
                <span class="stack-label">UI 外壳</span>
                <b class="stack-name">tao · wry · Vue 3</b>
              </div>
            </div>
          </div>

        </div>

        <footer class="panel-foot">
          <p v-if="settingsMessage" class="panel-status">{{ settingsMessage }}</p>
          <small v-else-if="isReadOnlyCategory" class="panel-foot-note">
            该分类仅展示信息，改动在其他分类填写后仍需点击保存
          </small>
          <small v-else-if="isDirty" class="panel-foot-note unsaved">有未保存的修改</small>
          <button
            class="btn-ghost"
            type="button"
            :disabled="settingsSaving"
            @click="discardDraft"
          >
            重置
          </button>
          <button
            class="btn-primary save"
            type="button"
            :disabled="settingsSaving"
            @click="save"
          >
            {{ settingsSaving ? "保存中..." : "保存" }}
          </button>
        </footer>
      </div>
    </section>
    </section>
  </n-modal>

    <!-- 未保存拦截：是 = 留在设置里；否 = 放弃改动并关闭设置 -->
    <n-modal
      :show="confirmCloseOpen"
      preset="dialog"
      title="有未保存的修改"
      class="settings-confirm"
      :mask-closable="false"
      :close-on-esc="false"
      :closable="false"
      @update:show="(value: boolean) => (confirmCloseOpen = value)"
    >
      <p class="confirm-text">
        当前设置还没有保存，直接关闭会丢失这些修改。
      </p>
      <p class="confirm-hint">
        「是」：留在设置里继续编辑（不保存）　「否」：放弃修改并关闭设置
      </p>
      <template #action>
        <button class="btn-ghost" type="button" @click="discardAndClose">否</button>
        <button class="btn-primary" type="button" @click="keepEditing">是</button>
      </template>
    </n-modal>

    <!-- 浏览器扩展图文引导：复制真实目录 → 浏览器加载已解压扩展 -->
    <n-modal
      v-model:show="extensionOpen"
      preset="card"
      title="安装浏览器扩展"
      style="width: 560px; max-width: 92vw"
      :bordered="false"
      :segmented="{ content: 'soft', footer: 'soft' }"
    >
      <ol class="ext-steps">
        <li>
          打开 Chrome / Edge 的扩展页：地址栏输入
          <code>chrome://extensions</code>（Edge 为 <code>edge://extensions</code>）
        </li>
        <li>右上角开启「开发者模式」</li>
        <li>点击「加载已解压的扩展程序」</li>
        <li>粘贴下面复制好的扩展目录并确认</li>
      </ol>

      <div class="ext-path">
        <code class="ext-path-text" :title="extensionInfo.path">
          {{ extensionInfo.path || "读取中…" }}
        </code>
        <button
          class="btn-ghost"
          type="button"
          :disabled="!extensionInfo.path"
          @click="copyExtensionPath"
        >
          <AppIcon :name="extensionCopied ? 'check' : 'copy'" :size="14" />
          {{ extensionCopied ? "已复制" : "复制路径" }}
        </button>
      </div>

      <small v-if="extensionInfo.path && !extensionInfo.exists" class="hint ext-missing">
        当前安装目录里没有 browser-extension 文件夹，请重新安装完整包，或把扩展文件夹放到
        上述位置。
      </small>
      <small v-else class="hint">
        安装后无需填端口或 Token（扩展会自动握手）；扩展目录里的「如何安装.txt」也有同样的步骤。
      </small>

      <template #footer>
        <div class="ext-foot">
          <button class="btn-primary" type="button" @click="extensionOpen = false">我知道了</button>
        </div>
      </template>
    </n-modal>

    <!-- 代理连通性测试结果：成功 / 失败都用明确弹窗告知 -->
    <n-modal
      v-model:show="proxyTestOpen"
      preset="card"
      :title="proxyTestOk ? '代理连接正常' : '代理连接失败'"
      style="width: 420px; max-width: 90vw"
      :bordered="false"
      :segmented="{ content: 'soft', footer: 'soft' }"
    >
      <p class="proxy-test-result" :class="{ ok: proxyTestOk }">
        <AppIcon :name="proxyTestOk ? 'check' : 'warning'" :size="16" />
        <span>{{ proxyTestInfo }}</span>
      </p>
      <template #footer>
        <div class="ext-foot">
          <button class="btn-primary" type="button" @click="proxyTestOpen = false">
            知道了
          </button>
        </div>
      </template>
    </n-modal>
</template>

<style scoped>
/* 弹窗内容区不再有卡片内边距：左菜单要贴边，右内容区自己控制滚动。 */
:global(.settings-modal .n-card__content) {
  padding: 0 !important;
}

.settings {
  display: flex;
  flex-direction: column;
  /* 固定高度：弹窗内左菜单与右侧表单各自滚动，不随内容无限撑高。 */
  height: min(640px, calc(100vh - 180px));
  min-height: 380px;
}

/* ---- 左菜单 + 右内容（仅右侧滚动） ---- */
.settings-layout {
  display: grid;
  grid-template-columns: 236px minmax(0, 1fr);
  gap: 0;
  align-items: stretch;
  flex: 1;
  min-height: 0;
}

.settings-nav {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px;
  min-height: 0;
  overflow-y: auto;
  border-right: 1px solid var(--line);
  background: var(--surface);
}

.nav-item {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 10px;
  border: 0;
  border-radius: 10px;
  background: transparent;
  text-align: left;
  cursor: pointer;
  transition:
    background 0.16s ease,
    color 0.16s ease;
}

.nav-item:hover {
  background: var(--surface-3);
}

.nav-item.active {
  background: var(--accent-soft);
}

.nav-item .cat-icon {
  width: 32px;
  height: 32px;
  border-radius: 10px;
}

.nav-text {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  flex: 1;
}

.nav-text b {
  font-size: 13px;
  font-weight: 650;
  color: var(--text);
}

.nav-text small {
  font-size: 10.5px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

.nav-arrow {
  flex: none;
  color: var(--muted);
  opacity: 0;
  transform: rotate(180deg);
  transition: opacity 0.16s ease;
}

.nav-item.active .nav-arrow {
  opacity: 1;
  color: var(--accent);
}

.cat-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
  width: 42px;
  height: 42px;
  border-radius: 13px;
}

/* ---- 右侧内容面板（整页：底部操作栏固定，仅 body 滚动） ---- */
.settings-panel {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  height: 100%;
  overflow: hidden;
}

/* 底部操作栏左侧的状态文案：错误时红字，成功时关闭弹窗（不再展示）。 */
.panel-status {
  margin: 0 auto 0 0;
  font-size: 11.5px;
  line-height: 1.4;
  color: var(--danger);
}

.panel-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overflow-x: hidden;
  padding: 18px 18px 20px;
}

.panel-foot {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 10px;
  padding: 12px 18px;
  border-top: 1px solid var(--line);
  background: var(--surface);
}

.panel-foot .save {
  min-width: 96px;
}

.panel-foot .save:disabled {
  opacity: 0.6;
  cursor: default;
}

.panel-foot-note {
  margin-right: auto;
  font-size: 11.5px;
  color: var(--muted);
}

.panel-foot-note.unsaved {
  color: var(--accent);
  font-weight: 600;
}

/* ---- 未保存确认框 ---- */
.confirm-text {
  margin: 0 0 8px;
  font-size: 13px;
  line-height: 1.6;
  color: var(--text);
}

.confirm-hint {
  margin: 0;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--muted);
}

.field-group {
  display: flex;
  flex-direction: column;
  gap: 18px;
}

.field {
  display: flex;
  flex-direction: column;
  gap: 7px;
  min-width: 0;
}

.field > span {
  display: inline-flex;
  align-items: center;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

/* 设置项小图标：统一灰阶，只有分类图标带语义色。 */
.field-icon {
  flex: none;
  margin-right: 5px;
  color: var(--muted);
}

.field input[type="text"],
.field textarea {
  width: 100%;
  padding: 9px 12px;
  border: 1px solid var(--line);
  border-radius: var(--r-btn);
  background: var(--surface-2);
  font-family: inherit;
  font-size: 13px;
  color: var(--text);
  outline: none;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease,
    background 0.15s ease;
}

.field textarea {
  resize: vertical;
  min-height: 86px;
  line-height: 1.55;
  font-family: inherit;
  font-size: 12.5px;
}

.field input[type="text"]:focus,
.field textarea:focus {
  background: var(--surface);
  border-color: color-mix(in srgb, var(--accent) 55%, transparent);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent);
}

.hint {
  font-size: 11px;
  line-height: 1.5;
  color: var(--muted);
}

/* 单服连接数 >16 的高危警告：红橙高亮，和普通 hint 拉开层级。 */
.danger-alert {
  margin-top: 6px;
  border-left: 3px solid var(--danger, #e5484d);
}

.danger-alert :deep(.n-alert-body__content) {
  font-size: 11.5px;
  line-height: 1.6;
}

.path-row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.path-row input {
  flex: 1;
  min-width: 0;
}

.path-row .browse {
  flex: none;
  height: 38px;
  padding: 0 14px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12.5px;
}

.switch-row {
  flex-direction: row;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
}

.switch-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}

.switch-copy > span {
  display: inline-flex;
  align-items: center;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

.switch-copy small {
  font-size: 11px;
  color: var(--muted);
}

.readonly-row {
  flex-direction: row;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 10px 12px;
  border-radius: var(--r-btn);
  background: var(--surface-2);
}

.readonly-row code {
  font-family: inherit;
  font-size: 11.5px;
  color: var(--text-2);
  word-break: break-all;
  text-align: right;
}

.readonly-row > span {
  display: inline-flex;
  align-items: center;
  flex: none;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

.proxy-row {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 9px;
}

.proxy-type {
  flex: none;
  width: 150px;
}

.proxy-row input {
  flex: 1;
  min-width: 140px;
}

/* 代理连通性测试：与输入框同高，窄窗口自动换行到下一行。 */
.proxy-test {
  flex: none;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 38px;
  padding: 0 13px;
  font-size: 12px;
}

.proxy-test-result {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 0;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--danger);
}

.proxy-test-result svg {
  flex: none;
  margin-top: 2px;
}

.proxy-test-result.ok {
  color: var(--text-2);
}

.proxy-test-result.ok svg {
  color: var(--success, #16a34a);
}

.hint code {
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-size: 10.5px;
  padding: 1px 4px;
  border-radius: 4px;
  background: var(--surface-2);
  color: var(--text-2);
}

/* ---- 分段选择（主题 / 关闭行为） ---- */
.segmented {
  display: inline-flex;
  padding: 3px;
  border-radius: 12px;
  background: var(--surface-2);
  border: 1px solid var(--line);
  width: fit-content;
}

.segmented button {
  min-width: 92px;
  height: 31px;
  padding: 0 14px;
  border: 0;
  border-radius: 9px;
  background: transparent;
  font-size: 12.5px;
  color: var(--muted);
  cursor: pointer;
  transition:
    background 0.16s ease,
    color 0.16s ease,
    box-shadow 0.16s ease;
}

.segmented button.active {
  background: var(--surface);
  color: var(--text);
  font-weight: 600;
  box-shadow: 0 1px 4px rgba(15, 42, 40, 0.12);
}

/* ---- naive-ui 下拉：与输入框同款浅底、圆角与聚焦光晕 ---- */
.concurrency-select {
  width: 100%;
  max-width: 320px;
}
.concurrency-select :deep(.n-base-selection),
.proxy-type :deep(.n-base-selection) {
  --n-height: 38px !important;
  --n-border-radius: var(--r-btn) !important;
  --n-color: var(--surface-2) !important;
  --n-color-active: var(--surface) !important;
  --n-text-color: var(--text) !important;
  --n-placeholder-color: var(--muted) !important;
  --n-arrow-color: var(--muted) !important;
  --n-border: 1px solid var(--line) !important;
  --n-border-hover: 1px solid color-mix(in srgb, var(--accent) 45%, var(--line)) !important;
  --n-border-active: 1px solid color-mix(in srgb, var(--accent) 55%, transparent) !important;
  --n-border-focus: 1px solid color-mix(in srgb, var(--accent) 55%, transparent) !important;
  --n-box-shadow-focus: 0 0 0 3px color-mix(in srgb, var(--accent) 12%, transparent) !important;
  font-size: 13px;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease,
    background 0.15s ease;
}
.concurrency-select :deep(.n-base-selection-label),
.proxy-type :deep(.n-base-selection-label) {
  padding: 0 13px;
}

/* ---- 关于 ---- */
.about-hero {
  display: flex;
  align-items: center;
  gap: 13px;
  padding: 14px;
  border-radius: var(--r-card);
  background: linear-gradient(135deg, #e6f7f5, #f4fbfa);
  border: 1px solid color-mix(in srgb, var(--accent) 22%, transparent);
}

.about-hero .brand-mark {
  width: 44px;
  height: 44px;
}

.about-hero > div {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.about-hero b {
  font-size: 15px;
  font-weight: 700;
  color: var(--text);
}

.about-hero small {
  font-size: 11.5px;
  color: var(--muted);
}

/* 技术栈：上下排版 —— 每行一个组件，名称大字、依赖小字在下方。 */
.stack-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.stack-row {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 12px 14px;
  border-radius: var(--r-card);
  background: var(--surface-2);
  border: 1px solid var(--line);
}

.stack-label {
  font-size: 11.5px;
  font-weight: 600;
  letter-spacing: 0.04em;
  color: var(--muted);
  text-transform: uppercase;
}

.stack-name {
  font-size: 19px;
  font-weight: 700;
  line-height: 1.25;
  color: var(--text);
}

.stack-deps {
  font-size: 11.5px;
  color: var(--muted);
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
}

/* ---- 浏览器扩展安装引导 ---- */
.ext-install {
  width: fit-content;
  height: 38px;
  padding: 0 16px;
  display: inline-flex;
  align-items: center;
  gap: 7px;
  font-size: 12.5px;
  font-weight: 600;
  /* 高亮：比同面板普通按钮多一圈柔和光晕，引导用户点它。 */
  box-shadow: 0 4px 14px color-mix(in srgb, var(--accent) 28%, transparent);
}

.ext-steps {
  margin: 0 0 14px;
  padding-left: 20px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-2);
}

.ext-steps code {
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-size: 11.5px;
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--surface-2);
  color: var(--text);
}

.ext-path {
  display: flex;
  align-items: center;
  gap: 9px;
}

.ext-path-text {
  flex: 1;
  min-width: 0;
  padding: 10px 12px;
  border-radius: var(--r-btn);
  background: var(--surface-2);
  border: 1px dashed var(--line-strong);
  font-family: ui-monospace, SFMono-Regular, Consolas, monospace;
  font-size: 11.5px;
  line-height: 1.5;
  color: var(--text-2);
  word-break: break-all;
}

.ext-path button {
  flex: none;
  height: 38px;
  padding: 0 14px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12.5px;
}

.ext-path button:disabled {
  opacity: 0.55;
  cursor: default;
}

.ext-missing {
  display: block;
  margin-top: 10px;
  color: var(--danger, #e5484d);
}

.ext-foot {
  display: flex;
  justify-content: flex-end;
  padding: 13px 18px;
  border-top: 1px solid var(--line);
  background: var(--surface);
}

/* 窄窗口：左菜单折成顶部横向分类条（可横向滚动），内容区独占整行。 */
@media (max-width: 860px) {
  .settings-layout {
    grid-template-columns: minmax(0, 1fr);
    grid-template-rows: auto minmax(0, 1fr);
  }
  .settings-nav {
    flex-direction: row;
    flex-wrap: nowrap;
    overflow-x: auto;
    overflow-y: hidden;
    border-right: 0;
    border-bottom: 1px solid var(--line);
    padding: 6px;
  }
  .nav-item {
    flex: none;
    padding: 6px 9px;
  }
  .nav-text small,
  .nav-arrow {
    display: none;
  }
}
</style>
