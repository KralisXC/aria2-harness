<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { storeToRefs } from "pinia";
import {
  NCheckbox,
  NDropdown,
  NModal,
  NPagination,
  type DropdownOption,
} from "naive-ui";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import NodeProgressBar from "../components/NodeProgressBar.vue";
import ThreadMatrixCard from "../components/ThreadMatrixCard.vue";
import { errText, invoke, listen, type UnlistenFn } from "../ipc";
import { isRealFailure, translateErrorCode } from "../utils/errorCode";
import { PENDING_TASK_NAME, nameFromUri, taskDisplayName } from "../utils/taskName";
import { readDeleteBehavior, writeDeleteBehavior } from "../utils/deleteBehavior";
import { useDownloadsStore, type Task } from "../stores/downloads";
import { useInterceptStore } from "../stores/intercept";
import { usePluginsStore } from "../stores/plugins";
import { PAGE_SIZE_EVENT, readPageSize } from "../utils/pageSize";

const store = useDownloadsStore();
const { list: tasks, displayTasks, composeNonce, composeSeed } = storeToRefs(store);

/** 浏览器扩展投递的待确认任务（宿主 `PENDING_INTERCEPT` 的前端镜像）。 */
const intercept = useInterceptStore();
const intercepting = computed(() => intercept.pending !== null);

/** 插件状态：右键「插件动作」菜单项来自这里（仅启用且声明 actions 的插件）。 */
const pluginsStore = usePluginsStore();
/**
 * 本次关闭弹窗是否保留用户原有草稿。
 *
 * 接管流程不读写草稿，关闭时若照常 `clearDrafts()` 会把用户自己打到一半的
 * 地址清掉 —— 用一个一次性开关区分"用户取消 / 确认"与普通新建弹窗。
 */
let preserveDrafts = false;

/**
 * 下载地址输入（beta11 · F-1）：一个 textarea 承载多行，每行分别创建一个任务。
 *
 * 与旧的多输入框相比不再有镜像/备用链接的"同一任务多 URI"语义：
 * 每行都是独立任务，因此也没有条数上限。
 */
const urlLines = ref("");
const dir = ref("");
const message = ref("");
const loading = ref(false);

/** 新建任务高级选项：仅本次任务生效，不写任何持久化。 */
const advancedOut = ref("");
const advancedUa = ref("");
const advancedReferer = ref("");
/** 自定义 Headers：动态 Key / Value 行，提交时拼成 `Key: Value` 数组。 */
const headerRows = ref<{ key: string; value: string }[]>([{ key: "", value: "" }]);
/** beta11 · F-4：本次任务的代理（`options["all-proxy"]`），留空则沿用全局设置。 */
const advancedProxy = ref("");

/**
 * 未提交输入的草稿键（增量 46）。
 *
 * 增量 45 起「关闭窗口 = 销毁 WebView」，Vue 实例连同所有 ref 一并被回收 —— 用户
 * 正在粘贴地址时若顺手点了关闭，回来就白打一遍。草稿只存**未提交的地址**。
 *
 * 保存路径不做任何持久化：它只是本次任务的单次覆盖，默认值统一由全局设置裁决。
 */
const DRAFT_URL_KEY = "aria2-harness-draft-url";
/** 写草稿的防抖窗口：连续敲键只在停顿后落一次 localStorage。 */
const DRAFT_DEBOUNCE_MS = 500;

// ---------------------------------------------------------------------------
// 插件虚拟任务（beta10.1：映射成 Task 后与普通任务统一渲染）
// ---------------------------------------------------------------------------

const PLUGIN_PHASE_TEXT: Record<string, string> = {
  detecting: "解析中",
  downloading: "下载分片",
  merging: "合并中",
  complete: "已完成",
  error: "已失败",
  cancelled: "已取消",
};

function pluginPhaseLabel(task: Task): string {
  const phase = task.plugin?.phase ?? "";
  return PLUGIN_PHASE_TEXT[phase] ?? phase;
}

/** 虚拟任务是否仍在流转（只有这种状态才提供「取消」）。 */
function isPluginTaskActive(task: Task): boolean {
  const phase = task.plugin?.phase ?? "";
  return phase !== "" && !["complete", "error", "cancelled"].includes(phase);
}

async function cancelPluginTask(task: Task) {
  try {
    await store.cancelPluginTask(task.gid);
    message.value = `已取消：${fileName(task)}`;
  } catch (error) {
    message.value = `取消失败：${errText(error)}`;
  }
}

/** beta11 · FF-7：无确定进度的插件任务（直播录制等）改用脉冲条。 */
function isIndeterminate(task: Task): boolean {
  return task.plugin?.mode === "indeterminate";
}

// ---------------------------------------------------------------------------
// beta11 · FF-8：`plugin://exec` 推送的插件 stderr 最新一行
// ---------------------------------------------------------------------------

/** pluginId -> 最近一行执行输出（只做轻量展示，不落任何持久化）。 */
const pluginExecLines = ref<Record<string, string>>({});
let unlistenExec: UnlistenFn | null = null;

/** 从虚拟任务 id（`plugin:<pluginId>:<seq>`）还原插件 id。 */
function pluginIdOf(task: Task): string {
  return /^plugin:([^:]+):/.exec(task.gid)?.[1] ?? "";
}

/** 活跃插件任务对应的最新执行行；无则返回空串（不展示）。 */
function pluginExecLine(task: Task): string {
  if (!task.plugin || !isPluginTaskActive(task)) return "";
  return pluginExecLines.value[pluginIdOf(task)] ?? "";
}

/** 分类 Tab：对 store 列表做纯 computed 过滤，不维护多份数组。 */
type Category = "all" | "active" | "complete" | "error";
const CATEGORY_TABS: { key: Category; label: string }[] = [
  { key: "all", label: "全部" },
  { key: "active", label: "下载中" },
  { key: "complete", label: "已完成" },
  { key: "error", label: "错误" },
];

/**
 * beta9：视图偏好持久化（分栏 / 详细信息 / 分类 Tab）。
 *
 * 与删除偏好同样落在 localStorage：升级或重启后仍保持上次的浏览状态，
 * 不再每次回到「单列 + 显示详情 + 全部」。搜索关键字刻意不持久化 ——
 * 一次性检索若被记住，下次进入会莫名看到空列表。
 */
const UI_PREF_KEY = "aria2-harness-downloads-ui";
interface UiPrefs {
  twoCol: boolean;
  showInfo: boolean;
  category: Category;
}

function readUiPrefs(): Partial<UiPrefs> {
  try {
    const raw = localStorage.getItem(UI_PREF_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw) as Partial<UiPrefs>;
    const prefs: Partial<UiPrefs> = {};
    if (typeof parsed.twoCol === "boolean") prefs.twoCol = parsed.twoCol;
    if (typeof parsed.showInfo === "boolean") prefs.showInfo = parsed.showInfo;
    // 旧版本写入的非法分类直接丢弃，回退到「全部」。
    if (CATEGORY_TABS.some((tab) => tab.key === parsed.category)) {
      prefs.category = parsed.category;
    }
    return prefs;
  } catch {
    return {};
  }
}

function writeUiPrefs(patch: Partial<UiPrefs>) {
  try {
    localStorage.setItem(UI_PREF_KEY, JSON.stringify({ ...readUiPrefs(), ...patch }));
  } catch {
    // 隐私模式 / 配额满：偏好降级为「本次会话有效」。
  }
}

const uiPrefs = readUiPrefs();

const composeOpen = ref(false);
const searchOpen = ref(false);
const keyword = ref("");
const twoCol = ref(uiPrefs.twoCol ?? false);
const showInfo = ref(uiPrefs.showInfo ?? true);
const menuOpen = ref(false);

const category = ref<Category>(uiPrefs.category ?? "all");

/** 三项视图偏好任一变化即落盘（不监听搜索关键字）。 */
watch([twoCol, showInfo, category], () => {
  writeUiPrefs({
    twoCol: twoCol.value,
    showInfo: showInfo.value,
    category: category.value,
  });
});

/** 批量选择（gid 集合）；切分类时保留，便于跨 Tab 连续勾选。 */
const selected = ref<Set<string>>(new Set());

/** 卡片展开状态：展开后显示活跃连接数与任务独立切片矩阵。 */
const expanded = ref<Set<string>>(new Set());

const STATUS_TEXT: Record<string, string> = {
  active: "下载中",
  waiting: "等待中",
  paused: "已暂停",
  error: "已失败",
  complete: "已完成",
  removed: "已移除",
};

/** 分类命中判断。 */
function inCategory(task: Task, key: Category): boolean {
  if (key === "all") return true;
  if (key === "active") return task.status === "active" || task.status === "waiting";
  if (key === "complete") return task.status === "complete";
  return task.status === "error";
}

function categoryCount(key: Category): number {
  return displayTasks.value.filter((task) => inCategory(task, key)).length;
}

const filtered = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  return displayTasks.value.filter((task) => {
    if (!inCategory(task, category.value)) return false;
    return !key || fileName(task).toLowerCase().includes(key);
  });
});

// ---------------------------------------------------------------------------
// 分页（beta10.1：传统页码，替代无限滚动的虚拟列表）
// ---------------------------------------------------------------------------

const pageSize = ref(readPageSize());
const page = ref(1);

const pageCount = computed(() => Math.max(1, Math.ceil(filtered.value.length / pageSize.value)));

/** 当前页的任务切片：分栏 / 单列都基于它继续切行。 */
const pageTasks = computed(() => {
  const start = (page.value - 1) * pageSize.value;
  return filtered.value.slice(start, start + pageSize.value);
});

/** 页码越界（清空 / 切分类 / 改每页数量）时收敛到最后一页。 */
watch([filtered, pageSize], () => {
  if (page.value > pageCount.value) page.value = pageCount.value;
  if (page.value < 1) page.value = 1;
});

// 分类 / 搜索变化回到第一页：否则用户会落在新结果的空页上。
watch([category, keyword], () => {
  page.value = 1;
});

/** 设置页修改每页数量后，同窗口通过自定义事件通知本页即时生效。 */
function syncPageSize() {
  const next = readPageSize();
  if (next === pageSize.value) return;
  pageSize.value = next;
  page.value = 1;
}

onMounted(() => window.addEventListener(PAGE_SIZE_EVENT, syncPageSize));
onBeforeUnmount(() => window.removeEventListener(PAGE_SIZE_EVENT, syncPageSize));

function toggleSelect(gid: string) {
  const next = new Set(selected.value);
  if (next.has(gid)) next.delete(gid);
  else next.add(gid);
  selected.value = next;
}

function clearSelection() {
  selected.value = new Set();
}

// 「全选」只作用于当前页：跨页全选会让批量操作的目标不可见。
const allSelected = computed(
  () =>
    pageTasks.value.length > 0 &&
    pageTasks.value.every((task) => selected.value.has(task.gid)),
);

function toggleSelectAll() {
  selected.value = allSelected.value
    ? new Set()
    : new Set(pageTasks.value.map((task) => task.gid));
}

function toggleExpand(gid: string) {
  const next = new Set(expanded.value);
  if (next.has(gid)) next.delete(gid);
  else next.add(gid);
  expanded.value = next;
}

/** 当前分类 / 搜索命中的已选任务。 */
function selectedTasks(): Task[] {
  return filtered.value.filter((task) => selected.value.has(task.gid));
}

const selectionCount = computed(() => selectedTasks().length);

/** 批量暂停 / 继续：逐条复用既有 IPC，跳过状态不匹配的任务。 */
async function batchControl(action: "pause" | "resume") {
  const targets = selectedTasks().filter((task) =>
    action === "pause" ? canPause(task) : canResume(task),
  );
  if (targets.length === 0) {
    message.value = action === "pause" ? "所选任务均不可暂停" : "所选任务均不可继续";
    return;
  }
  for (const task of targets) {
    await invoke(action === "pause" ? "pause_download" : "resume_download", {
      gid: task.gid,
    }).catch(() => undefined);
  }
  await Promise.all(targets.map((task) => store.updateOne(task.gid).catch(() => undefined)));
  message.value =
    action === "pause" ? `已暂停 ${targets.length} 个任务` : `已继续 ${targets.length} 个任务`;
}

/** 分片百分比 → 矩阵色阶（0 空闲 / 未过半 / 过半 / 完成）。 */
function pieceLevel(percent: number): string {
  if (percent >= 100) return "done";
  if (percent <= 0) return "idle";
  return percent < 50 ? "low" : "mid";
}

/** 卡片内切片矩阵的格数上限：超过按组取平均，避免单卡塞进几百个 DOM 节点。 */
const CARD_PIECE_CELLS = 64;

function pieceCells(t: Task): number[] {
  const raw = t.pieceProgress ?? [];
  if (raw.length === 0) return [];
  if (raw.length <= CARD_PIECE_CELLS) {
    return raw.map((value) => Math.min(100, Math.max(0, Math.round(Number(value) || 0))));
  }
  const group = Math.ceil(raw.length / CARD_PIECE_CELLS);
  const out: number[] = [];
  for (let i = 0; i < raw.length; i += group) {
    const slice = raw.slice(i, i + group);
    const average = slice.reduce((sum, value) => sum + (Number(value) || 0), 0) / slice.length;
    out.push(Math.min(100, Math.max(0, Math.round(average))));
  }
  return out;
}

/** ETA 展示：秒 → 中文短文案；0 / 非下载中返回空串。 */
function etaLabel(t: Task): string {
  const seconds = Number(t.eta || 0);
  if (!seconds || seconds <= 0) return "";
  if (seconds < 60) return `${seconds} 秒`;
  if (seconds < 3600) {
    const min = Math.floor(seconds / 60);
    const sec = seconds % 60;
    return sec > 0 ? `${min} 分 ${sec} 秒` : `${min} 分`;
  }
  const hour = Math.floor(seconds / 3600);
  const min = Math.floor((seconds % 3600) / 60);
  return min > 0 ? `${hour} 小时 ${min} 分` : `${hour} 小时`;
}

// ---------------------------------------------------------------------------
// 分页切行（beta10.1：每页 tasks 再按单列 / 双列打包）
// ---------------------------------------------------------------------------

/** 列表的一"行"：单列模式一行一张卡，分栏模式一行两张卡。 */
interface TaskRow {
  key: string;
  tasks: Task[];
}

/**
 * 把当前页任务按分栏状态切成行。
 *
 * 双列布局必须先把两张卡打包成"一行"，否则两列会串行堆叠。
 */
const rows = computed<TaskRow[]>(() => {
  const source = pageTasks.value;
  if (!twoCol.value) return source.map((task) => ({ key: task.gid, tasks: [task] }));
  const packed: TaskRow[] = [];
  for (let i = 0; i < source.length; i += 2) {
    const pair = source.slice(i, i + 2);
    packed.push({ key: pair.map((task) => task.gid).join("|"), tasks: pair });
  }
  return packed;
});

const activeCount = computed(
  () => displayTasks.value.filter((t) => t.status === "active").length,
);

const totalCount = computed(() => displayTasks.value.length);

// -- 未提交草稿 --------------------------------------------------------------

function readDraft(key: string): string {
  try {
    return localStorage.getItem(key)?.trim() ?? "";
  } catch {
    return "";
  }
}

function writeDraft(key: string, value: string) {
  try {
    if (value.trim()) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
    // 隐私模式 / 配额满：草稿降级为"不保存"，不影响下载主链路。
  }
}

function clearDrafts() {
  try {
    localStorage.removeItem(DRAFT_URL_KEY);
  } catch {
    // 同上。
  }
}

let draftTimer: ReturnType<typeof setTimeout> | undefined;

/** 地址变化 → 500ms 防抖落盘（每次敲键都同步写会拖慢输入）。 */
function scheduleDraftSave() {
  if (draftTimer) clearTimeout(draftTimer);
  draftTimer = setTimeout(() => {
    writeDraft(DRAFT_URL_KEY, draftUrlText());
  }, DRAFT_DEBOUNCE_MS);
}

/** 草稿文本：直接就是多行输入的原文（空行 / 缩进交给 `writeDraft` 与提交时统一处理）。 */
function draftUrlText(): string {
  return urlLines.value;
}

// 接管预填的是浏览器地址，不属于用户草稿：绝不写进 localStorage。
watch(urlLines, () => {
  if (!intercept.pending) scheduleDraftSave();
});

/** 弹窗关闭（取消 / 提交后）立即清草稿；窗口被销毁时组件直接卸载，不会触发这里。 */
watch(composeOpen, (open) => {
  if (!open) {
    if (draftTimer) clearTimeout(draftTimer);
    if (intercept.pending) {
      // 遮罩 / Esc 关闭等同「取消」：宿主暂存必须一起清掉，否则下次挂载又会弹出来。
      preserveDrafts = true;
      void invoke("cancel_intercept").catch(() => undefined);
      intercept.clear();
    }
    if (preserveDrafts) preserveDrafts = false;
    else clearDrafts();
  }
});

onBeforeUnmount(() => {
  // 窗口被销毁（增量 45 的托盘关闭）时组件直接卸载，防抖定时器很可能还没触发 ——
  // 这里立刻补写一次，否则"打完字就点关闭"仍会丢草稿。
  if (draftTimer) clearTimeout(draftTimer);
  if (composeOpen.value && !intercept.pending) {
    writeDraft(DRAFT_URL_KEY, draftUrlText());
  }
  // FF-8：退订插件执行日志事件，避免视图销毁后回调仍持有旧状态。
  unlistenExec?.();
  unlistenExec = null;
});

/** 打开新建弹窗：拖拽预填 > 未提交草稿。保存路径每次重置为全局默认。 */
function openCompose() {
  const seed = composeSeed.value.trim();
  const draftUrl = readDraft(DRAFT_URL_KEY);
  if (seed) {
    // 拖入的地址属于本次操作，必须覆盖草稿；消费一次即清空。
    urlLines.value = seed;
    composeSeed.value = "";
  } else if (draftUrl) {
    // 草稿代表"最近一次真实输入"，比上次成功地址更新，优先级更高。
    urlLines.value = draftUrl;
  } else {
    urlLines.value = "";
  }
  // 路径只是单次覆盖：每次打开都重新回填全局设置里的默认目录。
  dir.value = "";
  advancedOut.value = "";
  advancedUa.value = "";
  advancedReferer.value = "";
  resetHeaderRows();
  advancedProxy.value = "";
  void loadDefaultDir();
  composeOpen.value = true;
}

/** 回填引擎默认下载目录（设置 → 通用）。用户手填后让位给手填值。 */
async function loadDefaultDir() {
  if (dir.value.trim()) return;
  try {
    const options = await invoke<Record<string, string>>("get_global_option");
    // 异步返回期间用户可能已经手填，避免覆盖用户输入。
    if (options.dir && !dir.value.trim()) dir.value = options.dir;
  } catch {
    // 引擎未连接：保持空值，add_download 会回退到默认目录。
  }
}

/** 原生文件夹选择器（复用设置页同一 IPC，不额外引入 rfd）。 */
async function browseDir() {
  try {
    const picked = await invoke<string | null>("pick_directory");
    if (picked) dir.value = picked;
  } catch (error) {
    message.value = `选择目录失败：${errText(error)}`;
  }
}

/** 侧栏「+」：store 自增 nonce，本页据此打开新建弹窗。 */
watch(
  composeNonce,
  (nonce) => {
    if (nonce > 0) openCompose();
  },
  { immediate: true },
);

/**
 * 浏览器接管：宿主暂存后把整份数据送进来，直接预填并常驻弹窗。
 *
 * 无倒计时 —— 用户可能挂机很久才回来点确认，弹窗与宿主暂存都一直保留。
 */
watch(
  () => intercept.pending,
  (pending) => {
    if (!pending) return;
    // 接管极简模式（F-3）：地址只读展示浏览器给的链接；高级区只留 UA/Referer/代理。
    urlLines.value = pending.url;
    // UA / Referer 预填成浏览器上下文值，用户可按需改写（清空则回退到 pending 值）。
    advancedUa.value = pending.user_agent ?? "";
    advancedReferer.value = pending.referer ?? "";
    // 其余选项在接管模式下不展示，清掉可能残留的输入，避免误用。
    advancedOut.value = "";
    resetHeaderRows();
    advancedProxy.value = "";
    if (pending.dir) dir.value = pending.dir;
    else {
      dir.value = "";
      void loadDefaultDir();
    }
    composeOpen.value = true;
  },
  { immediate: true },
);

/**
 * 取消：接管任务额外要清宿主暂存（`PENDING_INTERCEPT`），
 * 普通新建则维持原语义 —— 关闭即丢弃本次输入。
 */
async function cancelCompose() {
  if (intercept.pending) {
    preserveDrafts = true;
    await invoke("cancel_intercept").catch(() => undefined);
    intercept.clear();
  }
  composeOpen.value = false;
}

function progress(t: Task): number {
  const total = Number(t.totalLength || 0);
  if (!total) return 0;
  return Math.min(100, (Number(t.completedLength || 0) / total) * 100);
}

// Chunked / close-delimited transfers report totalLength "0" while running.
// Only active states mean "unknown"; a finished zero-byte file is genuinely 0 B.
function isUnknownSize(t: Task): boolean {
  return (
    Number(t.totalLength || 0) === 0 &&
    (t.status === "active" || t.status === "waiting" || t.status === "paused")
  );
}

function sizeLabel(t: Task): string {
  return isUnknownSize(t) ? "未知大小" : human(t.totalLength);
}

function progressLabel(t: Task): string {
  return isUnknownSize(t) ? "--" : `${progress(t).toFixed(1)}%`;
}

function human(bytes: string | number | undefined): string {
  const n = Number(bytes || 0);
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v.toFixed(1)} ${units[i]}`;
}

/** 拼绝对路径：aria2 对 BT 任务可能返回相对 `dir` 的 files[].path。 */
function toAbsolutePath(path: string, dir?: string): string {
  if (/^[a-zA-Z]:[\\/]/.test(path) || /^\\\\/.test(path)) return path;
  const base = (dir ?? "").trim().replace(/[\\/]+$/, "");
  if (!base) return path;
  const sep = base.includes("\\") ? "\\" : "/";
  return `${base}${sep}${path.replace(/^[\\/]+/, "")}`;
}

function filePath(t: Task): string {
  // 虚拟任务的输出路径由插件在上报事件里给出；未完成时为空。
  if (t.plugin) return t.plugin.outputPath ?? "";
  const raw = (t.files?.[0]?.path ?? "").trim();
  if (!raw) return "";
  return toAbsolutePath(raw, t.dir);
}

/**
 * 「打开所在文件夹」的目标路径。
 *
 * 优先任务真实文件路径；等待中 / BT 元数据未就绪时 `files[].path` 为空，
 * 退回建任务时下发的 `dir`，绝不使用写死的默认目录。
 */
function locatePath(t: Task): string {
  return filePath(t) || (t.dir ?? "").trim();
}

/**
 * 任务显示名。
 *
 * 刚入队时 aria2 还没解析出 `files[].path`，绝不能把 GID 当名字显示；
 * 依次回退到「URL 尾段」与「解析文件名中...」，等 `Event::Start` / 轮询把
 * 真实文件名写进 `files[].path` 后自动替换（见 store.updateOne）。
 */
function fileName(t: Task): string {
  // 插件任务使用插件上报的标题（解析阶段文件名可能还没确定）。
  return t.plugin?.title || taskDisplayName(t);
}

function canPause(t: Task): boolean {
  if (t.plugin) return false;
  return t.status === "active" || t.status === "waiting";
}

function canResume(t: Task): boolean {
  if (t.plugin) return false;
  return t.status === "paused";
}

function statusText(t: Task): string {
  if (t.plugin) return pluginPhaseLabel(t);
  return STATUS_TEXT[t.status] ?? t.status;
}

/** 卡片是否按失败配色：引擎错误码 / 插件终态错误统一命中。 */
function isTaskFailed(t: Task): boolean {
  if (t.plugin) return t.plugin.phase === "error";
  return isRealFailure(t.status, t.errorCode);
}

/** 单线程降级的悬浮说明（与 Rust 侧 mark_single_thread 的语义一致）。 */
const SINGLE_THREAD_HINT =
  "该服务器限制了多线程/防盗链，已自动降级为单线程以保证下载成功";

/** 并发自动降级的悬浮说明（与 Rust 侧 degradation 模块语义一致）。 */
const DEGRADED_HINT =
  "服务器拒绝了高并发请求，已自动为您降低连接数以保证下载成功";

/** 续传能力：errorCode 8 = CANNOT_RESUME；已下载 >0 的 paused/active 视为可续传。 */
function resumeState(t: Task): "supported" | "unsupported" | "unknown" {
  if (Number(t.errorCode) === 8) return "unsupported";
  if ((t.status === "paused" || t.status === "active") && Number(t.completedLength) > 0) {
    return "supported";
  }
  return "unknown";
}

/**
 * 磁盘错误原文 → 人类可读的数字补充。
 *
 * 引擎原文是三个裸字节数（need / reserve / available），直接展示用户读不懂，
 * 这里统一换算成 GB / MB 并拼进提示。
 */
function diskDetail(message: string | undefined): string {
  const match =
    /need (\d+) bytes plus a (\d+) bytes safety margin, but only (\d+) bytes/.exec(
      message ?? "",
    );
  if (!match) return "";
  return `（需 ${human(match[1])} 文件 + ${human(match[2])} 预留，当前剩余 ${human(match[3])}）`;
}

/** 失败文案：磁盘不足补具体数字；HTTP 响应异常（常见 403）补浏览器下载降级提示。 */
function failureText(t: Task): string {
  const base = translateErrorCode(t.errorCode);
  const code = Number(t.errorCode);
  if (code === 9 || code === 34) {
    return `${base}${diskDetail(t.errorMessage)}`;
  }
  const blocked = code === 22 || /(^|\D)403(\D|$)/.test(t.errorMessage ?? "");
  return blocked
    ? `${base} · 该服务器可能限制了下载器，请尝试使用浏览器直接下载`
    : base;
}

/** beta8.3：403/429 限流由引擎降级为暂停，UI 用黄色温柔提示而不是红色错误。 */
function isRateLimited(t: Task): boolean {
  return t.status === "paused" && t.pauseReason === "rate-limit";
}

/** beta9：网络/代理断开同样由引擎降级为暂停，提示用户检查代理后继续。 */
function isProxyPaused(t: Task): boolean {
  return t.status === "paused" && t.pauseReason === "proxy_disconnected";
}

/** 两类降级暂停共用的卡片高度与琥珀色提示。 */
function isDegradedPause(t: Task): boolean {
  return isRateLimited(t) || isProxyPaused(t);
}

/** 节点进度条语义色：失败红 / 暂停·等待琥珀 / 已移除灰 / 其余青蓝。 */
function progressTone(t: Task): "ok" | "warn" | "danger" | "idle" {
  if (t.status === "error") return "danger";
  if (t.status === "paused" || t.status === "waiting") return "warn";
  if (t.status === "removed") return "idle";
  return "ok";
}

async function control(action: "pause" | "resume", t: Task) {
  const command = action === "pause" ? "pause_download" : "resume_download";
  try {
    await invoke(command, { gid: t.gid });
    message.value = action === "pause" ? "已暂停" : "已继续";
    // 立即反映状态变化，不等下一次事件
    await store.updateOne(t.gid);
  } catch (error) {
    message.value = `操作失败：${errText(error)}`;
  }
}

// ---------------------------------------------------------------------------
// 删除任务：偏好记忆 + 确认弹窗（增量 49）
// ---------------------------------------------------------------------------

const removeOpen = ref(false);
/** 本次待删除的任务集合：单条删除与批量删除共用同一个弹窗。 */
const removeTargets = ref<Task[]>([]);
const removeWithFiles = ref(false);
const removeRemember = ref(false);
/** 强制二次确认：下载中 / 等待中的任务与批量删除不允许"记住选择"直接跳过。 */
const removeRequired = ref(false);

/** 仍在传输链路上的任务：删除会造成数据丢失，必须二次确认。 */
function isLive(t: Task): boolean {
  return t.status === "active" || t.status === "waiting";
}

function openRemoveDialog(targets: Task[], required: boolean) {
  if (targets.length === 0) return;
  removeTargets.value = targets;
  removeRequired.value = required;
  // beta9：按上次记住的删除偏好回填，重启/更新后不再丢失勾选状态。
  const behavior = readDeleteBehavior();
  removeWithFiles.value = behavior === "task-and-file";
  removeRemember.value = behavior !== "ask";
  removeOpen.value = true;
}

/**
 * 统一删除入口。
 *
 * 下载中 / 等待中的任务永远弹窗确认；其余任务才尊重用户已记住的删除偏好。
 */
async function requestRemove(t: Task) {
  // 虚拟任务同样尊重「记住选择」；withFiles 一并交给宿主处理。
  const behavior = readDeleteBehavior();
  const live = isLive(t);
  if (!live && behavior === "task-only") {
    await performRemove(t, false);
    return;
  }
  if (!live && behavior === "task-and-file") {
    await performRemove(t, true);
    return;
  }
  openRemoveDialog([t], live);
}

/** 批量删除：多选后统一确认，不复用"记住选择"（批量误触代价太大）。 */
function requestBatchRemove() {
  openRemoveDialog(selectedTasks(), true);
}

async function confirmRemove() {
  const targets = [...removeTargets.value];
  if (targets.length === 0) return;
  const withFiles = removeWithFiles.value;
  if (!removeRequired.value && removeRemember.value) {
    writeDeleteBehavior(withFiles ? "task-and-file" : "task-only");
  }
  removeOpen.value = false;
  removeTargets.value = [];
  for (const task of targets) {
    await performRemove(task, withFiles);
  }
  if (targets.length > 1) message.value = `已删除 ${targets.length} 个任务`;
}

/** 弹窗里始终展示「同时删除本地文件」：插件任务同样按 withFiles 交给宿主处理。 */
const removeCanDeleteFiles = computed(() => removeTargets.value.length > 0);

/** 真正执行删除：虚拟任务走插件 IPC，其余按是否带文件分流。 */
async function performRemove(t: Task, withFiles: boolean) {
  try {
    if (t.plugin) {
      // beta12.1：运行中的虚拟任务必须先取消（宿主会强杀 FFmpeg 并 forceRemove
      // 底层分片），否则直接删注册表会留下"孤儿下载"在后台偷跑。
      if (isPluginTaskActive(t)) {
        await store.cancelPluginTask(t.gid).catch(() => undefined);
      }
      await invoke("remove_plugin_task", { id: t.gid, withFiles });
      store.removePluginTask(t.gid);
      message.value = withFiles
        ? `已删除任务及本地文件：${fileName(t)}`
        : `已删除任务：${fileName(t)}`;
      return;
    }
    if (withFiles) {
      await invoke("remove_task_and_files", { gid: t.gid });
    } else {
      await invoke("remove_download", { gid: t.gid });
    }
    store.remove(t.gid);
    message.value = withFiles
      ? `已删除任务及本地文件：${fileName(t)}`
      : `已删除任务：${fileName(t)}`;
  } catch (error) {
    message.value = `删除失败：${errText(error)}`;
  }
}

/** 弹窗被遮罩 / Esc 关闭时清掉目标，避免下次打开残留旧任务。 */
watch(removeOpen, (open) => {
  if (!open) removeTargets.value = [];
});

async function openFolder(t: Task) {
  const path = locatePath(t);
  // 排查用：确认前端给出的真实路径（配合日志 data\logs 里的 "[open]" 行）。
  console.log("[open] 构造的目标路径:", path);
  if (!path) {
    message.value = "该任务还没有可定位的文件路径";
    return;
  }
  try {
    await invoke("reveal_in_dir", { path });
  } catch (error) {
    message.value = `打开文件夹失败：${errText(error)}`;
  }
}

// ---------------------------------------------------------------------------
// 任务卡片右键菜单（复制链接 / 打开目录 / 复制路径 / 暂停继续 / 强制删除）
// ---------------------------------------------------------------------------

/** 菜单锚点：坐标取自 `contextmenu` 事件的 clientX/clientY（视口坐标）。 */
const ctx = ref<{ show: boolean; x: number; y: number; task: Task | null }>({
  show: false,
  x: 0,
  y: 0,
  task: null,
});

function openCtx(event: MouseEvent, task: Task) {
  ctx.value = { show: true, x: event.clientX, y: event.clientY, task };
}

/** 下载源：优先 `files[0].uris[0]`（BT 任务此时可能是 magnet 或 tracker 源）。 */
function downloadLink(t: Task | null): string {
  return t?.files?.[0]?.uris?.[0]?.uri ?? "";
}

/** 复制文本：Clipboard API 不可用时退回隐藏 textarea + execCommand。 */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    document.body.removeChild(area);
    return ok;
  }
}

/**
 * beta11 · FF-6：已完成任务可用的插件动作。
 *
 * 只取启用且声明了 actions 的插件；无动作插件不产生任何菜单项。
 */
const ctxPluginActions = computed(() => {
  const task = ctx.value.task;
  if (!task || task.status !== "complete") return [];
  return pluginsStore.plugins
    .filter((plugin) => plugin.enabled && plugin.actions.length > 0)
    .flatMap((plugin) =>
      plugin.actions.map((action) => ({
        pluginId: plugin.id,
        actionId: action.id,
        label: `插件动作：${action.label}`,
      })),
    );
});

const ctxOptions = computed<DropdownOption[]>(() => {
  const task = ctx.value.task;
  const link = downloadLink(task);
  const path = task ? filePath(task) : "";
  const revealPath = task ? locatePath(task) : "";
  const canToggle = Boolean(task && (canPause(task) || canResume(task)));
  const options: DropdownOption[] = [
    { label: "复制下载链接", key: "copy_url", disabled: !link },
    { label: "打开所在文件夹", key: "reveal", disabled: !revealPath },
    { label: "复制文件路径", key: "copy_path", disabled: !path },
    { type: "divider", key: "divider-1" },
    {
      label: task && canResume(task) ? "继续下载" : "暂停任务",
      key: "toggle",
      disabled: !canToggle,
    },
    { type: "divider", key: "divider-2" },
    { label: "删除任务", key: "remove" },
    { label: "强制删除", key: "force_remove", disabled: Boolean(task?.plugin) },
  ];
  if (ctxPluginActions.value.length > 0) {
    options.push({ type: "divider", key: "divider-plugin" });
    for (const action of ctxPluginActions.value) {
      options.push({
        label: action.label,
        key: `plugin-action:${action.pluginId}:${action.actionId}`,
      });
    }
  }
  return options;
});

/** 把已完成任务交给插件 `onAction` 处理（FF-6）。 */
async function runPluginAction(pluginId: string, actionId: string, task: Task) {
  try {
    await invoke("run_plugin_action", {
      pluginId,
      actionId,
      task: {
        gid: task.gid,
        name: fileName(task),
        path: filePath(task),
        dir: (task.dir ?? "").trim(),
      },
    });
    message.value = "已提交插件处理";
  } catch (error) {
    message.value = `插件处理失败：${errText(error)}`;
  }
}

async function onCtxSelect(key: string) {
  const task = ctx.value.task;
  ctx.value.show = false;
  if (!task) return;

  // FF-6：插件动作的 key 形如 `plugin-action:<pluginId>:<actionId>`，
  // 用当前菜单项回查，避免插件 id / 动作 id 里出现冒号时切分错误。
  const pluginAction = ctxPluginActions.value.find(
    (item) => `plugin-action:${item.pluginId}:${item.actionId}` === key,
  );
  if (pluginAction) {
    await runPluginAction(pluginAction.pluginId, pluginAction.actionId, task);
    return;
  }

  switch (key) {
    case "copy_url": {
      const link = downloadLink(task);
      if (!link) {
        message.value = "该任务没有可复制的下载链接";
        return;
      }
      const ok = await copyText(link);
      message.value = ok ? "已复制下载链接" : "复制失败（剪贴板不可用）";
      return;
    }
    case "copy_path": {
      const path = filePath(task);
      if (!path) {
        message.value = "该任务还没有文件路径";
        return;
      }
      const ok = await copyText(path);
      message.value = ok ? "已复制文件路径" : "复制失败（剪贴板不可用）";
      return;
    }
    case "reveal":
      await openFolder(task);
      return;
    case "toggle":
      await control(canResume(task) ? "resume" : "pause", task);
      return;
    case "remove":
      await requestRemove(task);
      return;
    case "force_remove":
      await forceRemove(task);
      return;
    default:
      return;
  }
}

/** 强制删除：走专用 IPC（Rust 侧 `forceRemove` 优先），而不是普通「删除」。 */
async function forceRemove(t: Task) {
  try {
    await invoke("force_remove", { gid: t.gid });
    store.remove(t.gid);
    message.value = `已强制删除：${fileName(t)}`;
  } catch (error) {
    message.value = `强制删除失败：${errText(error)}`;
  }
}

/** 读取引擎全局 Referer（设置 → 网络 → 自定义 Referer）；未设置或读取失败返回空串。 */
async function globalReferer(): Promise<string> {
  try {
    const options = await invoke<Record<string, string>>("get_global_option");
    return (options.referer ?? "").trim();
  } catch {
    return "";
  }
}

/** Headers 动态行：重置为一行空输入（打开弹窗 / 接管预填时调用）。 */
function resetHeaderRows() {
  headerRows.value = [{ key: "", value: "" }];
}

/** 追加一行 Header 输入。 */
function addHeaderRow() {
  headerRows.value.push({ key: "", value: "" });
}

/** 删除一行；至少保留一行，删空后自动补一个空行。 */
function removeHeaderRow(index: number) {
  headerRows.value.splice(index, 1);
  if (headerRows.value.length === 0) resetHeaderRows();
}

/** 网盘防盗链预设：百度网盘常见的 UA + Referer 组合。 */
function fillNetdiskPreset() {
  advancedUa.value = "netdisk;P2SP;3.0.20.128";
  advancedReferer.value = "https://pan.baidu.com/";
}

/**
 * 提交前收集多行输入里的全部地址。
 *
 * 按行拆分、trim、去空、保序去重：每行是一个独立任务，重复地址只建一次。
 */
function collectUrls(): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const line of urlLines.value.split(/\r?\n/)) {
    const text = line.trim();
    if (!text || seen.has(text)) continue;
    seen.add(text);
    out.push(text);
  }
  return out;
}

/** 实时地址条数：>1 时提示"将分别创建任务"。 */
const urlCount = computed(() => collectUrls().length);

async function addDownload() {
  const targets = collectUrls();
  if (targets.length === 0) return;
  // 快照：await 期间宿主可能又收到新一单，不能读实时值做归属判断。
  const pending = intercept.pending;
  loading.value = true;
  try {
    const options: Record<string, string | string[]> = {};
    // 手动重命名优先于引擎的自动识别；空值不提交，交给后端贪婪取名。
    const outName = advancedOut.value.trim();
    if (outName) options.out = outName;
    // 手填的高级选项优先：这是用户对本次任务的显式意图。
    const ua = advancedUa.value.trim();
    const referer = advancedReferer.value.trim();
    if (ua) options["user-agent"] = ua;
    if (referer) options.referer = referer;
    // F-4：代理按 aria2 的 all-proxy 下发；空值不写，沿用全局设置。
    const proxy = advancedProxy.value.trim();
    if (proxy) options["all-proxy"] = proxy;

    // 自定义 Headers 按行拼成 `Key: Value`：Key 为空整行跳过，Value 允许为空（输出 `Key:`）。
    const headers = headerRows.value
      .map((row) => {
        const key = row.key.trim();
        if (!key) return "";
        const value = row.value.trim();
        return value ? `${key}: ${value}` : `${key}:`;
      })
      .filter(Boolean);

    if (pending) {
      // 接管任务：浏览器上下文补齐用户未手填的部分。
      if (pending.referer && !options.referer) options.referer = pending.referer;
      if (pending.user_agent && !options["user-agent"]) {
        options["user-agent"] = pending.user_agent;
      }
      // beta8.3：扩展嗅探到的完整浏览器请求头优先合并（整行去重，手填行优先）。
      for (const line of pending.headers ?? []) {
        const trimmed = line.trim();
        if (!trimmed) continue;
        if (!headers.some((exists) => exists.toLowerCase() === trimmed.toLowerCase())) {
          headers.push(trimmed);
        }
      }
      if (pending.cookie) {
        // aria2 的 `header` 要求完整的 `Name: Value` 行；兼容扩展传裸 Cookie 串。
        // 嗅探头里已带 Cookie 时不再重复追加。
        const hasCookie = headers.some((line) => /^cookie:/i.test(line));
        if (!hasCookie) {
          headers.push(
            /^cookie:/i.test(pending.cookie) ? pending.cookie : `Cookie: ${pending.cookie}`,
          );
        }
      }
    } else if (!options.referer) {
      // 普通新建：优先下发设置页的全局自定义 Referer；
      // 未设置时留给宿主按内置防盗链规则无感注入。
      const global = await globalReferer();
      if (global) options.referer = global;
    }
    if (headers.length > 0) options.header = headers;

    // F-2：每条链接单独建任务；单条失败不中断其余条，结束后统一汇总。
    let ok = 0;
    let lastGid = "";
    let lastError = "";
    const failed: string[] = [];
    for (const target of targets) {
      try {
        const gid = await invoke<string>("add_download", {
          urls: [target],
          dir: dir.value.trim() || null,
          options: Object.keys(options).length > 0 ? options : null,
        });
        ok += 1;
        lastGid = gid;
      } catch (error) {
        failed.push(target);
        lastError = errText(error);
      }
    }

    if (ok === 0) {
      // 全部失败：保留输入与弹窗，用户可直接改地址重试。
      message.value = `添加失败：${lastError || `共 ${targets.length} 个地址均未成功`}`;
      return;
    }

    // 提示里绝不出现 GID：优先 URL 尾段，取不到就显示"解析中"。
    if (failed.length > 0) {
      message.value = `已添加任务：成功 ${ok} 个 / 失败 ${failed.length} 个`;
    } else if (targets.length > 1) {
      message.value = `已添加任务：共 ${ok} 个`;
    } else {
      message.value = `已添加任务：${nameFromUri(targets[0]) || PENDING_TASK_NAME}`;
    }
    urlLines.value = "";
    if (pending) {
      // 确认成功：清宿主暂存（失败也不阻断入队，下次取消会再清一次）。
      preserveDrafts = true;
      await invoke("cancel_intercept").catch(() => undefined);
      intercept.clear();
    }
    composeOpen.value = false;
    // 插件虚拟任务不在引擎里，不能走 get_task（会报 Invalid GID）；
    // 它的卡片由 `plugin://task` 事件推入统一下载列表。
    if (lastGid && !lastGid.startsWith("plugin:")) {
      // 立刻把最后一个成功的任务放进 Map，不必等 WS 事件；其余交给事件/轮询。
      await store.updateOne(lastGid);
      store.startPolling();
    }
  } catch (error) {
    message.value = `添加失败：${errText(error)}`;
  } finally {
    loading.value = false;
  }
}

/** 「更多」菜单：对当前列表批量操作（逐条走既有 IPC，不新增后端接口）。 */
async function pauseAll() {
  menuOpen.value = false;
  for (const task of tasks.value) {
    if (canPause(task)) {
      await invoke("pause_download", { gid: task.gid }).catch(() => undefined);
    }
  }
  message.value = "已暂停全部进行中的任务";
}

async function resumeAll() {
  menuOpen.value = false;
  for (const task of tasks.value) {
    if (canResume(task)) {
      await invoke("resume_download", { gid: task.gid }).catch(() => undefined);
    }
  }
  message.value = "已继续全部暂停的任务";
}

async function clearCompleted() {
  menuOpen.value = false;
  // 合并列表里的「已完成」既可能来自引擎，也可能是插件虚拟任务，两条链路都要清。
  for (const task of displayTasks.value.filter((t) => t.status === "complete")) {
    if (task.plugin) {
      await invoke("remove_plugin_task", { id: task.gid }).catch(() => undefined);
      store.removePluginTask(task.gid);
      continue;
    }
    await invoke("remove_download", { gid: task.gid }).catch(() => undefined);
    store.remove(task.gid);
  }
  message.value = "已清理已完成任务";
}

onMounted(() => {
  void store.initAll().catch(() => {
    // 引擎未就绪时静默失败，WS 连接成功后会重新初始化
  });
  void store.initPluginTasks();
  // FF-6：右键「插件动作」依赖插件清单；未加载过（没进过插件页）时补一次。
  if (pluginsStore.plugins.length === 0) {
    void pluginsStore.load().catch(() => undefined);
  }
  // FF-8：插件执行输出（stderr 逐行）按 pluginId 保存最新一行，供活跃任务卡轻量展示。
  void listen<{ pluginId: string; program: string; line: string }>(
    "plugin://exec",
    (ev) => {
      const { pluginId, line } = ev.payload;
      const text = (line ?? "").trim();
      if (!pluginId || !text) return;
      pluginExecLines.value = { ...pluginExecLines.value, [pluginId]: text };
    },
  ).then((off) => {
    unlistenExec = off;
  });
});
</script>

<template>
  <div class="view-downloads" @click="menuOpen = false">
    <PageHeader title="全部下载" subtitle="管理当前会话中的下载任务">
      <template #meta>
        <span class="count-pill num">{{ activeCount }}/{{ totalCount }}</span>
      </template>
      <template #actions>
        <div class="head-search" :class="{ open: searchOpen }">
          <input
            v-if="searchOpen"
            v-model="keyword"
            type="search"
            placeholder="搜索任务名"
            class="head-search-input"
            @click.stop
          />
          <button
            class="icon-btn"
            :class="{ active: searchOpen }"
            title="搜索"
            @click.stop="searchOpen = !searchOpen"
          >
            <AppIcon name="search" :size="17" />
          </button>
        </div>
        <button
          class="icon-btn"
          :class="{ active: twoCol }"
          title="分栏显示"
          @click.stop="twoCol = !twoCol"
        >
          <AppIcon name="split" :size="17" />
        </button>
        <button
          class="icon-btn"
          :class="{ active: showInfo }"
          title="显示详细信息"
          @click.stop="showInfo = !showInfo"
        >
          <AppIcon name="info" :size="17" />
        </button>
        <div class="menu-wrap">
          <button
            class="icon-btn"
            :class="{ active: menuOpen }"
            title="更多"
            @click.stop="menuOpen = !menuOpen"
          >
            <AppIcon name="more" :size="17" />
          </button>
          <div v-if="menuOpen" class="menu" @click.stop>
            <button class="menu-item" @click="pauseAll">全部暂停</button>
            <button class="menu-item" @click="resumeAll">全部继续</button>
            <button class="menu-item danger" @click="clearCompleted">清理已完成</button>
          </div>
        </div>
        <button class="btn-primary add-btn" @click.stop="openCompose()">
          <AppIcon name="plus" :size="16" />
          <span>新建</span>
        </button>
      </template>
    </PageHeader>

    <ThreadMatrixCard />

    <!-- 分类 Tab + 全选：纯前端过滤，不额外请求引擎 -->
    <div class="cat-bar">
      <div class="cat-tabs" role="tablist">
        <button
          v-for="tab in CATEGORY_TABS"
          :key="tab.key"
          class="cat-tab"
          :class="{ active: category === tab.key }"
          role="tab"
          :aria-selected="category === tab.key"
          @click.stop="category = tab.key"
        >
          {{ tab.label }}
          <span class="cat-count num">{{ categoryCount(tab.key) }}</span>
        </button>
      </div>
      <label class="cat-select" @click.stop>
        <n-checkbox
          :checked="allSelected"
          :indeterminate="selectionCount > 0 && !allSelected"
          @update:checked="toggleSelectAll"
        />
        <span>全选</span>
      </label>
    </div>

    <div v-if="selectionCount > 0" class="batch-bar" @click.stop>
      <span class="batch-count">已选 {{ selectionCount }} 项</span>
      <button class="btn-ghost" @click="batchControl('pause')">
        <AppIcon name="pause" :size="13" /> 暂停
      </button>
      <button class="btn-ghost" @click="batchControl('resume')">
        <AppIcon name="play" :size="13" /> 继续
      </button>
      <button class="btn-ghost danger" @click="requestBatchRemove">
        <AppIcon name="trash" :size="13" /> 删除
      </button>
      <button class="btn-ghost batch-clear" @click="clearSelection">取消选择</button>
    </div>

    <p v-if="message" class="status">{{ message }}</p>

    <section v-if="filtered.length > 0" class="tasks">
      <div
        v-for="item in rows"
        :key="item.key"
        class="task-row"
        :class="{ 'two-col': item.tasks.length > 1 }"
      >
        <article
          v-for="task in item.tasks"
          :key="task.gid"
          class="task-card"
          :class="{
            'no-info': !showInfo,
            'has-error': isTaskFailed(task),
            'has-rate-limit': isRateLimited(task),
            'has-degraded-pause': isDegradedPause(task),
          }"
          @contextmenu.prevent="openCtx($event, task)"
            >
              <div class="task-main">
                <label class="task-check" title="选择任务" @click.stop>
                  <n-checkbox
                    :checked="selected.has(task.gid)"
                    @update:checked="toggleSelect(task.gid)"
                  />
                </label>

                <div class="task-icon" :class="task.status">
                  <AppIcon
                    :name="task.status === 'complete' ? 'check' : task.status === 'error' ? 'error' : 'download'"
                    :size="18"
                  />
                </div>

                <div class="task-body">
                  <div class="task-head">
                    <div class="title-line">
                      <span class="name" :title="fileName(task)">{{ fileName(task) }}</span>
                      <!-- 续传能力：只在能给出确定结论时展示，避免误导 -->
                      <span
                        v-if="resumeState(task) === 'supported'"
                        class="tag-resume ok"
                        title="已下载部分数据，支持断点续传"
                      >↻ 支持续传</span>
                      <span
                        v-else-if="resumeState(task) === 'unsupported'"
                        class="tag-resume no"
                        title="服务器不支持断点续传，中断后需要重新下载"
                      >× 不支持续传</span>
                      <span
                        v-if="task.isSingleThread"
                        class="tag-single"
                        :title="SINGLE_THREAD_HINT"
                      >单线程降级</span>
                      <span
                        v-if="task.isDegraded"
                        class="tag-degraded"
                        :title="DEGRADED_HINT"
                      >↘ 自动降级至 {{ task.currentConcurrency ?? 1 }} 连接</span>
                    </div>
                    <div class="state-line">
                      <span class="state" :class="task.status">{{ statusText(task) }}</span>
                      <button
                        v-if="!task.plugin"
                        class="expand-btn"
                        :class="{ open: expanded.has(task.gid) }"
                        :title="expanded.has(task.gid) ? '收起详情' : '展开详情'"
                        @click.stop="toggleExpand(task.gid)"
                      >
                        <AppIcon
                          name="chevron"
                          :size="13"
                          class="chev"
                          :class="{ flipped: !expanded.has(task.gid) }"
                        />
                      </button>
                    </div>
                  </div>

                  <!-- FF-7：无确定进度的任务（直播录制等）用脉冲条代替百分比进度 -->
                  <div
                    v-if="isIndeterminate(task)"
                    class="pulse-bar"
                    :class="{ done: task.status !== 'active' }"
                  >
                    <span class="pulse-thumb"></span>
                  </div>
                  <NodeProgressBar
                    v-else
                    class="task-progress"
                    :progress="progress(task)"
                    :pieces="task.pieceProgress"
                    :active="task.status === 'active'"
                    :tone="progressTone(task)"
                  />

                  <div class="meta">
                    <span v-if="!isIndeterminate(task)" class="num">{{ progressLabel(task) }}</span>
                    <template v-if="task.plugin">
                      <span class="num">{{ task.plugin.detail || pluginPhaseLabel(task) }}</span>
                    </template>
                    <template v-else>
                      <span class="num">{{ human(task.completedLength) }} / {{ sizeLabel(task) }}</span>
                      <span v-if="task.status === 'active'" class="num speed">
                        {{ human(task.downloadSpeed) }}/s
                      </span>
                      <span v-if="task.status === 'active'" class="num eta">
                        剩余 {{ etaLabel(task) || "计算中" }}
                      </span>
                      <span>{{ task.activeWorkers ?? task.connections }} 连接</span>
                    </template>
                  </div>

                  <!-- FF-8：活跃插件任务下方展示该插件最新一行执行输出（轻量、可缺省） -->
                  <p v-if="pluginExecLine(task)" class="exec-line" :title="pluginExecLine(task)">
                    {{ pluginExecLine(task) }}
                  </p>

                  <div v-if="showInfo" class="path" :title="filePath(task) || task.plugin?.detail">
                    {{ filePath(task) || task.plugin?.detail || "（路径尚未确定）" }}
                  </div>
                </div>
              </div>

              <div class="actions">
                <button v-if="canPause(task)" class="btn-ghost" @click="control('pause', task)">
                  <AppIcon name="pause" :size="14" /> 暂停
                </button>
                <button v-if="canResume(task)" class="btn-ghost" @click="control('resume', task)">
                  <AppIcon name="play" :size="14" /> 继续
                </button>
                <!-- 虚拟任务只有「取消」：插件阶段没有暂停/继续语义 -->
                <button
                  v-if="isPluginTaskActive(task)"
                  class="btn-ghost"
                  @click="cancelPluginTask(task)"
                >
                  <AppIcon name="close" :size="14" /> 取消
                </button>
                <button
                  v-if="!task.plugin || filePath(task)"
                  class="btn-ghost"
                  @click="openFolder(task)"
                >
                  <AppIcon name="folder" :size="14" /> 打开
                </button>
                <button class="btn-ghost danger" @click="requestRemove(task)">
                  <AppIcon name="trash" :size="14" /> 删除
                </button>
              </div>

              <!-- 展开面板：活跃连接 + 该任务独立的切片矩阵 -->
              <div v-if="expanded.has(task.gid)" class="task-expand">
                <div class="expand-head">
                  <span class="expand-metric">
                    <AppIcon name="gauge" :size="13" />
                    活跃连接 <b class="num">{{ task.activeWorkers ?? 0 }}</b>
                  </span>
                  <span class="expand-metric">
                    <AppIcon name="clock" :size="13" />
                    剩余 <b class="num">{{ etaLabel(task) || "计算中" }}</b>
                  </span>
                  <span class="expand-metric">
                    <AppIcon name="split" :size="13" />
                    分片 <b class="num">{{ task.pieceProgress?.length ?? 0 }}</b>
                  </span>
                </div>
                <div v-if="pieceCells(task).length > 0" class="piece-matrix">
                  <span
                    v-for="(percent, index) in pieceCells(task)"
                    :key="index"
                    class="pm-cell"
                    :class="pieceLevel(percent)"
                    :title="`分片 ${index + 1} · ${percent}%`"
                  />
                </div>
                <p v-else class="expand-note">暂无分片数据（单线程或任务刚入队）</p>
              </div>

              <!-- 失败信息：单行浅色文字，错误码统一翻译，不展示英文原文 -->
              <p v-if="isRealFailure(task.status, task.errorCode)" class="err-inline">
                <AppIcon name="warning" :size="13" />
                <span>下载失败 · {{ failureText(task) }}</span>
              </p>
              <!-- 虚拟任务失败：插件回传的错误原文 -->
              <p v-if="task.plugin?.phase === 'error'" class="err-inline">
                <AppIcon name="warning" :size="13" />
                <span>{{ task.plugin.error || "插件任务执行失败" }}</span>
              </p>
              <!-- beta8.3：限流降级暂停的温柔提示（黄色），不显示为错误 -->
              <p v-if="isRateLimited(task)" class="warn-inline">
                <AppIcon name="warning" :size="13" />
                <span>服务器拒绝了连接（可能触发了限流）。建议在设置中降低并发数，或稍后点击继续。</span>
              </p>
              <!-- beta9：代理/网络断开降级暂停的黄色提示 -->
              <p v-else-if="isProxyPaused(task)" class="warn-inline">
                <AppIcon name="warning" :size="13" />
                <span>网络或代理已断开，请检查后点击继续。</span>
              </p>
            </article>
      </div>
    </section>

    <!-- 分页：仅在多页时出现；单页时保持干净的列表 -->
    <div v-if="pageCount > 1" class="pager" @click.stop>
      <n-pagination
        v-model:page="page"
        :page-count="pageCount"
        :page-slot="7"
        :show-quick-jumper="pageCount > 7"
      />
      <span class="pager-total num">共 {{ filtered.length }} 项</span>
    </div>

    <section v-if="filtered.length === 0" class="empty-state">
      <!-- 极简插画：空列表时给一个明确的视觉落点，而不是只有一行字 -->
      <svg class="empty-art" viewBox="0 0 96 72" aria-hidden="true">
        <rect x="14" y="10" width="68" height="50" rx="10" class="art-frame" />
        <path d="M48 24v20" class="art-line" />
        <path d="M39 36l9 9 9-9" class="art-line" />
        <path d="M34 56h28" class="art-line" />
        <circle cx="76" cy="14" r="2.5" class="art-dot" />
        <circle cx="20" cy="14" r="2.5" class="art-dot" />
      </svg>
      <h2 class="empty-title">暂无下载任务</h2>
      <p class="empty-hint">
        <template v-if="keyword">没有匹配「{{ keyword }}」的任务</template>
        <template v-else-if="category !== 'all'">当前分类下没有任务</template>
        <template v-else>把链接或 .torrent 文件拖进窗口即可开始</template>
      </p>
      <button class="btn-primary empty-btn" @click="openCompose()">
        <AppIcon name="plus" :size="16" /> 新建任务
      </button>
    </section>

    <!-- 任务卡片右键菜单：manual 模式，坐标来自 contextmenu 事件的视口坐标 -->
    <n-dropdown
      trigger="manual"
      placement="bottom-start"
      :show="ctx.show"
      :x="ctx.x"
      :y="ctx.y"
      :options="ctxOptions"
      @select="onCtxSelect"
      @clickoutside="ctx.show = false"
    />

    <!-- 新建任务弹窗：使用 naive-ui card 预设，避免自定义 transform 导致首帧尺寸抖动 -->
    <n-modal
      v-model:show="composeOpen"
      preset="card"
      :title="intercepting ? '接管浏览器下载' : '新建下载任务'"
      style="width: 780px; max-width: 94vw"
      :bordered="false"
      :segmented="{ content: 'soft', footer: 'soft' }"
    >
      <p v-if="intercepting" class="compose-desc intercept-desc">
        <AppIcon name="download" :size="14" />
        接管下载：浏览器发来的请求已自动填入地址与 Cookie，可调整 User-Agent / Referer / 代理后开始下载。
      </p>
      <p v-else class="compose-desc">
        支持 http / https / ftp / magnet；每行一个链接，将分别创建任务（Ctrl+Enter 开始）。
      </p>
      <form class="compose-grid" @submit.prevent="addDownload">
        <!-- 左栏：本次任务的基础信息 -->
        <div class="compose-col">
          <div class="compose-field">
            <span>
              下载地址
              <em v-if="!intercepting && urlCount > 1">
                已识别 {{ urlCount }} 个链接，将分别创建任务
              </em>
            </span>
            <!-- F-3：接管模式只读展示浏览器地址 -->
            <input
              v-if="intercepting"
              :value="intercept.pending?.url ?? ''"
              type="text"
              readonly
              spellcheck="false"
              class="readonly-url"
            />
            <textarea
              v-else
              v-model="urlLines"
              rows="4"
              placeholder="每行一个链接，将分别创建任务"
              spellcheck="false"
              @keydown.ctrl.enter.prevent="addDownload"
              @keydown.meta.enter.prevent="addDownload"
            ></textarea>
          </div>
          <label v-if="!intercepting" class="compose-field">
            <span>保存路径</span>
            <div class="compose-row">
              <input
                v-model="dir"
                type="text"
                placeholder="默认下载目录"
                spellcheck="false"
              />
              <button type="button" class="btn-ghost" @click="browseDir">浏览</button>
            </div>
          </label>
        </div>

        <!-- 右栏：仅本次任务生效的高级选项 -->
        <div class="compose-col compose-advanced">
          <div class="compose-col-title">
            高级选项
            <em>仅本次任务生效</em>
          </div>
          <label v-if="!intercepting" class="compose-field">
            <span>重命名 <em>可选</em></span>
            <input
              v-model="advancedOut"
              type="text"
              placeholder="留空则自动识别文件名"
              spellcheck="false"
            />
          </label>
          <label class="compose-field">
            <span>User-Agent <em>可选</em></span>
            <input
              v-model="advancedUa"
              type="text"
              placeholder="留空则沿用全局设置"
              spellcheck="false"
            />
          </label>
          <label class="compose-field">
            <span>Referer <em>可选</em></span>
            <input
              v-model="advancedReferer"
              type="text"
              placeholder="留空则沿用全局设置"
              spellcheck="false"
            />
          </label>
          <label class="compose-field">
            <span>代理 <em>可选</em></span>
            <input
              v-model="advancedProxy"
              type="text"
              placeholder="http://127.0.0.1:7890"
              spellcheck="false"
            />
          </label>
          <template v-if="!intercepting">
            <div class="compose-field">
              <span>自定义 Headers <em>每行一个 Key / Value</em></span>
              <div class="header-rows">
                <div v-for="(row, index) in headerRows" :key="index" class="header-row">
                  <input
                    v-model="row.key"
                    type="text"
                    class="header-key"
                    placeholder="Key"
                    spellcheck="false"
                  />
                  <input
                    v-model="row.value"
                    type="text"
                    class="header-value"
                    placeholder="Value"
                    spellcheck="false"
                  />
                  <button
                    type="button"
                    class="icon-btn header-del"
                    title="删除这一行"
                    @click="removeHeaderRow(index)"
                  >
                    <AppIcon name="close" :size="14" />
                  </button>
                </div>
              </div>
              <button type="button" class="btn-ghost header-add" @click="addHeaderRow">
                <AppIcon name="plus" :size="14" /> 添加 Header
              </button>
            </div>
            <button type="button" class="btn-ghost preset-btn" @click="fillNetdiskPreset">
              填入网盘预设
            </button>
          </template>
        </div>
      </form>

      <template #footer>
        <div class="compose-actions">
          <button type="button" class="btn-ghost" @click="cancelCompose">取消</button>
          <button
            type="button"
            class="btn-primary"
            :disabled="loading || urlCount === 0"
            @click="addDownload"
          >
            {{ loading ? "添加中..." : "开始下载" }}
          </button>
        </div>
      </template>
    </n-modal>

    <!-- 删除确认：下载中任务与批量删除强制出现（见 requestRemove） -->
    <n-modal
      v-model:show="removeOpen"
      preset="card"
      :title="removeTargets.length > 1 ? `删除 ${removeTargets.length} 个任务` : '删除任务'"
      style="width: 440px; max-width: 90vw"
      :bordered="false"
      :segmented="{ content: 'soft', footer: 'soft' }"
    >
      <p class="compose-desc">
        <template v-if="removeTargets.length > 1">
          确定要删除所选 {{ removeTargets.length }} 个下载任务吗？
        </template>
        <template v-else>确定要删除该下载任务吗？</template>
        <template v-if="removeRequired">任务仍在下载中，删除后未完成的数据将丢失。</template>
      </p>
      <p v-if="removeTargets.length === 1" class="delete-name">
        {{ fileName(removeTargets[0]) }}
      </p>
      <div class="delete-options">
        <n-checkbox v-if="removeCanDeleteFiles" v-model:checked="removeWithFiles">
          同时删除本地文件（不可恢复）
        </n-checkbox>
        <n-checkbox v-if="!removeRequired" v-model:checked="removeRemember">
          记住我的选择，下次不再询问
        </n-checkbox>
      </div>

      <template #footer>
        <div class="compose-actions">
          <button type="button" class="btn-ghost" @click="removeOpen = false">取消</button>
          <button type="button" class="btn-danger" @click="confirmRemove">确定删除</button>
        </div>
      </template>
    </n-modal>
  </div>
</template>

<style scoped>
.count-pill {
  display: inline-flex;
  align-items: center;
  height: 26px;
  padding: 0 12px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--text-2);
  font-size: 12px;
}

/* ---- 分类 Tab + 批量操作条 ---- */

.cat-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 10px;
  margin: 4px 0 10px;
}

.cat-tabs {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  padding: 3px;
  border: 1px solid var(--line);
  border-radius: var(--r-btn);
  background: var(--surface-2);
}

.cat-tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 28px;
  padding: 0 12px;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: var(--muted);
  font-size: 12.5px;
  transition:
    background 0.15s ease,
    color 0.15s ease;
}

.cat-tab:hover {
  color: var(--text);
}

.cat-tab.active {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 600;
}

.cat-count {
  min-width: 16px;
  padding: 0 4px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--line) 45%, transparent);
  color: var(--muted);
  font-size: 10.5px;
  line-height: 16px;
  text-align: center;
}

.cat-tab.active .cat-count {
  background: color-mix(in srgb, var(--accent) 22%, transparent);
  color: var(--accent);
}

.cat-select {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  margin-left: auto;
  font-size: 12px;
  color: var(--muted);
  cursor: pointer;
}

.batch-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
  margin-bottom: 10px;
  padding: 8px 12px;
  border: 1px solid color-mix(in srgb, var(--accent) 30%, var(--line));
  border-radius: var(--r-card);
  background: color-mix(in srgb, var(--accent) 8%, var(--surface));
}

.batch-count {
  margin-right: 4px;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

.batch-bar button {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 28px;
  padding: 0 10px;
  border: 0;
  border-radius: 8px;
  font-size: 12px;
}

.batch-bar button.danger {
  color: var(--danger);
}
.batch-bar button.danger:hover {
  background: var(--danger-soft);
}

.batch-clear {
  margin-left: auto;
}

.head-search {
  display: flex;
  align-items: center;
  gap: 6px;
}

.head-search-input {
  width: 168px;
  height: 34px;
  padding: 0 12px;
  border-radius: var(--r-btn);
  animation: slide-in 0.18s ease;
}

@keyframes slide-in {
  from {
    width: 0;
    opacity: 0;
  }
  to {
    width: 168px;
    opacity: 1;
  }
}

.menu-wrap {
  position: relative;
}

.menu {
  position: absolute;
  top: 42px;
  right: 0;
  z-index: 30;
  min-width: 148px;
  padding: 6px;
  border: 1px solid var(--line);
  border-radius: 14px;
  background: var(--surface);
  box-shadow: var(--shadow-float);
}

.menu-item {
  display: block;
  width: 100%;
  padding: 8px 10px;
  border: 0;
  border-radius: 9px;
  background: transparent;
  color: var(--text-2);
  font-size: 13px;
  text-align: left;
}
.menu-item:hover {
  background: var(--surface-3);
  color: var(--text);
}
.menu-item.danger:hover {
  background: var(--danger-soft);
  color: var(--danger);
}

.add-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 34px;
  padding: 0 14px;
  border-radius: var(--r-btn);
  font-size: 13px;
}

/*
 * 分页列表（beta10.1）：`.tasks` 是固定高度的滚动视口，只有列表内部滚动，
 * 外层容器保持锁定；页码条固定在列表下方，不随卡片滚动。
 */
.view-downloads {
  display: flex;
  flex-direction: column;
  height: 100%;
  min-height: 0;
}

.tasks {
  margin-top: 4px;
  padding-right: 2px;
  /* 占满剩余高度，超出一页的卡片在容器内部滚动。 */
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
}

/* 页码条：列表下方居中，单页时不出现。 */
.pager {
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 12px;
  flex-wrap: wrap;
  padding: 10px 0 4px;
  flex: none;
}

.pager-total {
  font-size: 11.5px;
  color: var(--muted);
}

.task-row {
  display: grid;
  /* minmax(0,1fr)：允许轨道被压缩到容器宽度，避免长文件名/路径把卡片撑宽。 */
  grid-template-columns: minmax(0, 1fr);
  align-items: start;
  gap: 12px;
  box-sizing: border-box;
  padding-bottom: 12px;
  /* 卡片按容器宽度铺满，不随文字宽度收缩。 */
  width: 100%;
  min-width: 0;
}
.task-row.two-col {
  grid-template-columns: repeat(2, minmax(0, 1fr));
}

.task-card {
  box-sizing: border-box;
  min-height: 156px;
  padding: 14px 16px;
  border: 1px solid var(--line);
  border-radius: 12px;
  background: var(--surface-2);
  transition:
    border-color 0.15s ease,
    background 0.15s ease;
}
.task-card.no-info {
  min-height: 132px;
}
.task-card.has-error {
  min-height: 180px;
}
.task-card.no-info.has-error {
  min-height: 156px;
}
.task-card.has-rate-limit,
.task-card.has-degraded-pause {
  min-height: 180px;
}
.task-card.no-info.has-rate-limit,
.task-card.no-info.has-degraded-pause {
  min-height: 156px;
}
.task-card:hover {
  border-color: var(--line-strong);
  background: var(--surface);
}

/* 多选复选框：常驻左侧，保证卡片内容排版稳定不跳动。 */
.task-check {
  display: inline-flex;
  align-items: center;
  flex: none;
  width: 18px;
  height: 22px;
  cursor: pointer;
}

/* 状态 + 展开按钮：右上角成组出现，不再和标题抢宽度。 */
.state-line {
  display: flex;
  align-items: center;
  gap: 4px;
  flex: none;
}

.expand-btn {
  display: grid;
  place-items: center;
  width: 22px;
  height: 22px;
  padding: 0;
  border: 0;
  border-radius: 7px;
  background: transparent;
  color: var(--muted);
  transition:
    background 0.15s ease,
    color 0.15s ease;
}
.expand-btn:hover,
.expand-btn.open {
  background: var(--surface-3);
  color: var(--text);
}
.expand-btn .chev {
  transition: transform 0.18s ease;
}
.expand-btn .chev.flipped {
  transform: rotate(-90deg);
}

/* 展开面板：活跃连接 / ETA / 分片数 + 该任务独立的切片矩阵。 */
.task-expand {
  margin-top: 10px;
  padding-left: 78px;
  animation: expand-in 0.18s ease;
}

@keyframes expand-in {
  from {
    opacity: 0;
    transform: translateY(-4px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}

.expand-head {
  display: flex;
  flex-wrap: wrap;
  gap: 14px;
  font-size: 11.5px;
  color: var(--muted);
}

.expand-metric {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.expand-metric b {
  color: var(--text-2);
  font-weight: 650;
}

.piece-matrix {
  display: grid;
  grid-template-columns: repeat(16, minmax(0, 1fr));
  gap: 3px;
  margin-top: 8px;
  padding: 6px;
  border-radius: 8px;
  background: var(--surface-3);
}

.pm-cell {
  height: 8px;
  border-radius: 1px;
  background: color-mix(in srgb, var(--line) 60%, transparent);
}
.pm-cell.low {
  background: color-mix(in srgb, var(--accent) 35%, transparent);
}
.pm-cell.mid {
  background: color-mix(in srgb, var(--accent) 65%, transparent);
}
.pm-cell.done {
  background: var(--accent);
}

.expand-note {
  margin: 8px 0 0;
  font-size: 11px;
  color: var(--muted);
  opacity: 0.8;
}

.task-main {
  display: flex;
  align-items: flex-start;
  gap: 12px;
}

.task-icon {
  flex: none;
  width: 38px;
  height: 38px;
  display: grid;
  place-items: center;
  border-radius: 12px;
  background: var(--accent-soft);
  color: var(--accent);
}
.task-icon.complete {
  background: var(--success-soft);
  color: var(--success);
}
.task-icon.error {
  background: var(--danger-soft);
  color: var(--danger);
}
.task-icon.paused,
.task-icon.waiting {
  background: var(--warn-soft);
  color: var(--warn);
}

.task-body {
  flex: 1;
  min-width: 0;
}

.task-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

/* 文件名 + 标签：文件名可收缩省略，标签始终完整可见。 */
.title-line {
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
  flex: 1;
}

.name {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 13.5px;
  font-weight: 600;
  color: var(--text);
}

/* 断点续传指示：极小号，绿色=可续传，灰色=服务器明确拒绝续传。 */
.tag-resume {
  flex: none;
  font-size: 10px;
  line-height: 1;
  letter-spacing: 0.2px;
  cursor: help;
  white-space: nowrap;
}
.tag-resume.ok {
  color: var(--success);
}
.tag-resume.no {
  color: var(--muted);
}

/* 并发自动降级：更亮的黄色，表示"已主动降档重试"而非最终失败。 */
.tag-degraded {
  flex: none;
  padding: 1px 7px;
  border-radius: 999px;
  border: 1px solid rgba(250, 204, 21, 0.45);
  background: rgba(250, 204, 21, 0.16);
  color: #fbbf24;
  font-size: 10px;
  line-height: 1.5;
  cursor: help;
  white-space: nowrap;
}

/* 单线程降级：橙色，提示"不是故障，是主动策略"。 */
.tag-single {
  flex: none;
  padding: 1px 7px;
  border-radius: 999px;
  border: 1px solid rgba(245, 158, 11, 0.35);
  background: rgba(245, 158, 11, 0.12);
  color: #f59e0b;
  font-size: 10px;
  line-height: 1.5;
  cursor: help;
  white-space: nowrap;
}

.state {
  flex: none;
  padding: 2px 9px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--muted);
  font-size: 10.5px;
}
.state.active {
  background: var(--accent-soft);
  color: var(--accent);
}
.state.complete {
  background: var(--success-soft);
  color: var(--success);
}
.state.error {
  background: var(--danger-soft);
  color: var(--danger);
}
.state.paused,
.state.waiting {
  background: var(--warn-soft);
  color: var(--warn);
}

/* 任务进度：单线节点分段式（5 节点 / 4 段），已完成虚线、未完成极细实线。 */
.task-progress {
  margin-top: 10px;
}

/* FF-7：无确定进度的任务用跳动脉冲条代替百分比条。 */
.pulse-bar {
  position: relative;
  height: 8px;
  margin-top: 10px;
  overflow: hidden;
  border-radius: 999px;
  background: color-mix(in srgb, var(--line) 55%, transparent);
}

.pulse-thumb {
  position: absolute;
  top: 0;
  left: 0;
  width: 34%;
  height: 100%;
  border-radius: 999px;
  background: var(--accent);
  opacity: 0.85;
  animation: pulse-slide 1.3s ease-in-out infinite;
}

/* 终态不再"跳动"，只留一条静态满条，避免误导用户以为还在处理。 */
.pulse-bar.done .pulse-thumb {
  width: 100%;
  opacity: 0.4;
  animation: none;
}

@keyframes pulse-slide {
  0% {
    transform: translateX(-100%);
  }
  100% {
    transform: translateX(320%);
  }
}

/* FF-8：活跃插件任务的最新一行执行输出（小字、单行省略）。 */
.exec-line {
  margin: 4px 0 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: var(--mono, ui-monospace, SFMono-Regular, Menlo, monospace);
  font-size: 10.5px;
  color: var(--muted);
  opacity: 0.75;
}

.meta {
  display: flex;
  gap: 14px;
  flex-wrap: wrap;
  margin-top: 8px;
  font-size: 11.5px;
  color: var(--muted);
}
.meta .speed {
  color: var(--accent);
}
.meta .eta {
  color: var(--text-2);
}

.path {
  margin-top: 6px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: rtl;
  text-align: left;
  font-size: 11px;
  color: var(--muted);
  opacity: 0.8;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-top: 10px;
  padding-left: 78px;
}
.actions button {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 28px;
  padding: 0 10px;
  border: 0;
  border-radius: 8px;
  font-size: 11.5px;
}
.actions button.danger {
  color: var(--danger);
}
.actions button.danger:hover {
  background: var(--danger-soft);
  color: var(--danger);
}

/* 失败提示：无背景、无边框，像一行普通注释 */
.err-inline {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  margin: 7px 0 0;
  padding-left: 78px;
  font-size: 11.5px;
  line-height: 1.5;
  color: #d32f2f;
}
.err-inline svg {
  flex: none;
  margin-top: 1px;
}

/* 限流提示：无背景，琥珀色一行注释（beta8.3） */
.warn-inline {
  display: flex;
  align-items: flex-start;
  gap: 6px;
  margin: 7px 0 0;
  padding-left: 78px;
  font-size: 11.5px;
  line-height: 1.5;
  color: #b26a00;
}
.warn-inline svg {
  flex: none;
  margin-top: 1px;
}

/* ---- 空状态（默认纯净文字，故障特效只在 CTOS 皮肤下启用） ---- */
.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 14px;
  min-height: 60vh;
  padding: 30px 0;
}

.empty-art {
  width: 96px;
  height: 72px;
  color: var(--muted);
}

.empty-art .art-frame {
  fill: var(--surface-2);
  stroke: var(--line);
  stroke-width: 1.5;
}

.empty-art .art-line {
  fill: none;
  stroke: var(--muted);
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
  opacity: 0.7;
}

.empty-art .art-dot {
  fill: var(--accent);
  opacity: 0.5;
}

.empty-title {
  margin: 0;
  font-size: 20px;
  font-weight: 600;
  letter-spacing: 0.02em;
  color: var(--text);
}

.empty-hint {
  margin: 0;
  font-size: 13px;
  color: var(--muted);
}

.empty-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
  font-size: 13px;
}

/* ---- 新建任务弹窗（n-modal card 预设内部布局） ---- */
.compose-desc {
  margin: 0 0 14px;
  font-size: 12px;
  color: var(--muted);
}

/* 浏览器接管提示：比普通说明更醒目一点，表明这单不是用户手打的。 */
.intercept-desc {
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 8px 10px;
  border-radius: 8px;
  background: var(--accent-soft, rgba(45, 212, 191, 0.12));
  color: var(--accent, #14b8a6);
}

/* 双栏：左基础信息 / 右高级选项，窄屏自动折叠为单栏。 */
.compose-grid {
  display: grid;
  grid-template-columns: minmax(0, 1.05fr) minmax(0, 0.95fr);
  gap: 20px;
  align-items: start;
}

.compose-col {
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-width: 0;
  /* 高级选项再多也不顶穿底部按钮：栏内独立滚动。 */
  max-height: 70vh;
  overflow-y: auto;
  overscroll-behavior: contain;
}

.compose-field {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.compose-field > span {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

.compose-field em {
  margin-left: 6px;
  font-style: normal;
  font-size: 11px;
  font-weight: 400;
  color: var(--muted);
}

.compose-field input {
  width: 100%;
  height: 40px;
  font-size: 13px;
}

.compose-field textarea {
  width: 100%;
  min-height: 76px;
  padding: 10px 12px;
  font-size: 12.5px;
  font-family: var(--mono, ui-monospace, SFMono-Regular, Menlo, monospace);
  line-height: 1.6;
  resize: vertical;
}

/* 右栏视觉分组：淡底板把"仅本次任务生效"的选项与基础信息区分开。 */
.compose-advanced {
  gap: 12px;
  padding: 14px;
  border-radius: 12px;
  background: var(--surface-2, rgba(127, 127, 127, 0.06));
  overscroll-behavior: contain;
}

/* Headers 动态行：Key 窄、Value 宽，行尾删除按钮固定尺寸。 */
.header-rows {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.header-row {
  display: flex;
  align-items: center;
  gap: 6px;
}

.compose-field .header-row input {
  width: auto;
  height: 34px;
  font-size: 12.5px;
}

.compose-field .header-key {
  flex: 0 1 34%;
  min-width: 0;
}

.compose-field .header-value {
  flex: 1 1 auto;
  min-width: 0;
}

.header-del {
  flex: none;
  width: 30px;
  height: 30px;
}

.header-add {
  align-self: flex-start;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  border-radius: var(--r-btn);
}

.compose-col-title {
  display: flex;
  align-items: baseline;
  gap: 6px;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

.compose-col-title em {
  font-style: normal;
  font-size: 11px;
  font-weight: 400;
  color: var(--muted);
}

/* F-3：接管模式下的只读地址（视觉上明确不可编辑）。 */
.readonly-url {
  color: var(--muted);
  background: var(--surface-3);
  cursor: default;
}

@media (max-width: 720px) {
  .compose-grid {
    grid-template-columns: minmax(0, 1fr);
    gap: 14px;
  }
}

.preset-btn {
  align-self: flex-start;
  height: 32px;
  padding: 0 12px;
  border-radius: var(--r-btn);
}

.compose-row {
  display: flex;
  gap: 8px;
}

.compose-row input {
  flex: 1;
  min-width: 0;
}

.compose-row button {
  flex: none;
  height: 40px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}

.compose-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.compose-actions button {
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}

/* ---- 删除确认弹窗 ---- */
.delete-options {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.delete-name {
  margin: 0 0 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text-2);
}

@media (max-width: 1100px) {
  .task-row.two-col {
    grid-template-columns: minmax(0, 1fr);
  }
}
</style>

