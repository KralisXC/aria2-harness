<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { storeToRefs } from "pinia";
import {
  NCheckbox,
  NDropdown,
  NModal,
  NVirtualList,
  type DropdownOption,
} from "naive-ui";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import NodeProgressBar from "../components/NodeProgressBar.vue";
import ThreadMatrixCard from "../components/ThreadMatrixCard.vue";
import { errText, invoke } from "../ipc";
import { isRealFailure, translateErrorCode } from "../utils/errorCode";
import { PENDING_TASK_NAME, nameFromUri, taskDisplayName } from "../utils/taskName";
import { readDeleteBehavior, writeDeleteBehavior } from "../utils/deleteBehavior";
import { useDownloadsStore, type Task } from "../stores/downloads";
import { useInterceptStore } from "../stores/intercept";

const store = useDownloadsStore();
const { list: tasks, composeNonce, composeSeed } = storeToRefs(store);

/** 浏览器扩展投递的待确认任务（宿主 `PENDING_INTERCEPT` 的前端镜像）。 */
const intercept = useInterceptStore();
const intercepting = computed(() => intercept.pending !== null);
/**
 * 本次关闭弹窗是否保留用户原有草稿。
 *
 * 接管流程不读写草稿，关闭时若照常 `clearDrafts()` 会把用户自己打到一半的
 * 地址清掉 —— 用一个一次性开关区分"用户取消 / 确认"与普通新建弹窗。
 */
let preserveDrafts = false;

const url = ref("");
const dir = ref("");
const message = ref("");
const loading = ref(false);

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

const composeOpen = ref(false);
const searchOpen = ref(false);
const keyword = ref("");
const twoCol = ref(false);
const showInfo = ref(true);
const menuOpen = ref(false);

/** 分类 Tab：对 store 列表做纯 computed 过滤，不维护多份数组。 */
type Category = "all" | "active" | "complete" | "error";
const category = ref<Category>("all");
const CATEGORY_TABS: { key: Category; label: string }[] = [
  { key: "all", label: "全部" },
  { key: "active", label: "下载中" },
  { key: "complete", label: "已完成" },
  { key: "error", label: "错误" },
];

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
  return tasks.value.filter((task) => inCategory(task, key)).length;
}

const filtered = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  return tasks.value.filter((task) => {
    if (!inCategory(task, category.value)) return false;
    return !key || fileName(task).toLowerCase().includes(key);
  });
});

function toggleSelect(gid: string) {
  const next = new Set(selected.value);
  if (next.has(gid)) next.delete(gid);
  else next.add(gid);
  selected.value = next;
}

function clearSelection() {
  selected.value = new Set();
}

const allSelected = computed(
  () => filtered.value.length > 0 && filtered.value.every((task) => selected.value.has(task.gid)),
);

function toggleSelectAll() {
  selected.value = allSelected.value
    ? new Set()
    : new Set(filtered.value.map((task) => task.gid));
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
// 虚拟滚动（增量 46）
// ---------------------------------------------------------------------------

/** 虚拟列表的一"行"：单列模式一行一张卡，分栏模式一行两张卡。 */
interface TaskRow {
  key: string;
  tasks: Task[];
}

/**
 * 把任务列表按当前分栏状态切成行。
 *
 * 为什么连分栏也算进来：`n-virtual-list` 只有一条纵向轨道，双列布局必须先把两张卡
 * 打包成"一行"，否则卡片会串行堆叠。行高因此天然一致（两列同高）。
 */
const rows = computed<TaskRow[]>(() => {
  const source = filtered.value;
  if (!twoCol.value) return source.map((task) => ({ key: task.gid, tasks: [task] }));
  const packed: TaskRow[] = [];
  for (let i = 0; i < source.length; i += 2) {
    const pair = source.slice(i, i + 2);
    packed.push({ key: pair.map((task) => task.gid).join("|"), tasks: pair });
  }
  return packed;
});

/**
 * 单行高度（含行间距），与 CSS 里 `.task-card` 的固定高度**必须同步**：
 * - 显示详细信息：156 + 12；
 * - 隐藏详细信息：132 + 12；
 * - 失败卡片多一行错误提示：+24（由 `item-resizable` 实测修正）。
 */
const itemSize = computed(() => (showInfo.value ? 156 : 132) + 12);

/**
 * 列表可视高度：由外层面板顶部到窗口底部的剩余空间算出。
 *
 * 预留 46px = workspace 底部 14px + page-scroll 底部内边距 32px：
 * 留少了会让整页多出一条无意义的滚动条，虚拟列表反而被撑出视口。
 */
const listHeight = ref(360);
const listBox = ref<HTMLElement | null>(null);

function measureList() {
  const el = listBox.value;
  if (!el) return;
  const top = el.getBoundingClientRect().top;
  listHeight.value = Math.max(220, Math.floor(window.innerHeight - top - 46));
}

/** 搜索 / 分栏 / 信息开关变化后，行的构成或高度会变，重新量一次。 */
watch([twoCol, showInfo, () => rows.value.length], () => {
  void nextTick(measureList);
});

const activeCount = computed(
  () => tasks.value.filter((t) => t.status === "active").length,
);

const totalCount = computed(() => tasks.value.length);

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
    writeDraft(DRAFT_URL_KEY, url.value);
  }, DRAFT_DEBOUNCE_MS);
}

// 接管预填的是浏览器地址，不属于用户草稿：绝不写进 localStorage。
watch(url, () => {
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
    writeDraft(DRAFT_URL_KEY, url.value);
  }
});

/** 打开新建弹窗：拖拽预填 > 未提交草稿。保存路径每次重置为全局默认。 */
function openCompose() {
  const seed = composeSeed.value.trim();
  const draftUrl = readDraft(DRAFT_URL_KEY);
  if (seed) {
    // 拖入的地址属于本次操作，必须覆盖草稿；消费一次即清空。
    url.value = seed;
    composeSeed.value = "";
  } else if (draftUrl) {
    // 草稿代表"最近一次真实输入"，比上次成功地址更新，优先级更高。
    url.value = draftUrl;
  }
  // 路径只是单次覆盖：每次打开都重新回填全局设置里的默认目录。
  dir.value = "";
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
    url.value = pending.url;
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
  return taskDisplayName(t);
}

function canPause(t: Task): boolean {
  return t.status === "active" || t.status === "waiting";
}

function canResume(t: Task): boolean {
  return t.status === "paused";
}

function statusText(t: Task): string {
  return STATUS_TEXT[t.status] ?? t.status;
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
  removeWithFiles.value = false;
  removeRemember.value = false;
  removeOpen.value = true;
}

/**
 * 统一删除入口。
 *
 * 下载中 / 等待中的任务永远弹窗确认；其余任务才尊重用户已记住的删除偏好。
 */
async function requestRemove(t: Task) {
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

/** 真正执行删除：带文件走新 IPC，仅任务走原 IPC。 */
async function performRemove(t: Task, withFiles: boolean) {
  try {
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

const ctxOptions = computed<DropdownOption[]>(() => {
  const task = ctx.value.task;
  const link = downloadLink(task);
  const path = task ? filePath(task) : "";
  const revealPath = task ? locatePath(task) : "";
  const canToggle = Boolean(task && (canPause(task) || canResume(task)));
  return [
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
    { label: "强制删除", key: "force_remove" },
  ];
});

async function onCtxSelect(key: string) {
  const task = ctx.value.task;
  ctx.value.show = false;
  if (!task) return;

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

async function addDownload() {
  const target = url.value.trim();
  if (!target) return;
  // 快照：await 期间宿主可能又收到新一单，不能读实时值做归属判断。
  const pending = intercept.pending;
  loading.value = true;
  try {
    const options: Record<string, string> = {};
    if (pending) {
      // 接管任务：浏览器上下文最高优先，原样交给 aria2 单任务 options。
      if (pending.referer) options.referer = pending.referer;
      if (pending.user_agent) options["user-agent"] = pending.user_agent;
      if (pending.cookie) {
        // aria2 的 `header` 要求完整的 `Name: Value` 行；兼容扩展传裸 Cookie 串。
        options.header = /^cookie:/i.test(pending.cookie)
          ? pending.cookie
          : `Cookie: ${pending.cookie}`;
      }
    } else {
      // 普通新建：优先下发设置页的全局自定义 Referer；
      // 未设置时留给宿主按内置防盗链规则无感注入。
      const referer = await globalReferer();
      if (referer) options.referer = referer;
    }
    const gid = await invoke<string>("add_download", {
      url: target,
      dir: dir.value.trim() || null,
      options: Object.keys(options).length > 0 ? options : null,
    });
    // 提示里绝不出现 GID：优先 URL 尾段，取不到就显示"解析中"。
    message.value = `已添加任务：${nameFromUri(target) || PENDING_TASK_NAME}`;
    url.value = "";
    if (pending) {
      // 确认成功：清宿主暂存（失败也不阻断入队，下次取消会再清一次）。
      preserveDrafts = true;
      await invoke("cancel_intercept").catch(() => undefined);
      intercept.clear();
    }
    composeOpen.value = false;
    // 立刻把新任务放进 Map，不必等 WS 事件
    await store.updateOne(gid);
    store.startPolling();
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
  for (const task of tasks.value.filter((t) => t.status === "complete")) {
    await invoke("remove_download", { gid: task.gid }).catch(() => undefined);
    store.remove(task.gid);
  }
  message.value = "已清理已完成任务";
}

onMounted(() => {
  void store.initAll().catch(() => {
    // 引擎未就绪时静默失败，WS 连接成功后会重新初始化
  });
  // 虚拟列表的可视高度依赖窗口尺寸（跨屏拖动 / 最大化都会变），必须跟随 resize。
  void nextTick(measureList);
  window.addEventListener("resize", measureList);
});

onBeforeUnmount(() => {
  window.removeEventListener("resize", measureList);
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

    <section v-if="rows.length > 0" ref="listBox" class="tasks">
      <!-- naive-ui 2.45 起 NVirtualList 不再接受 height 属性：高度必须落在根节点
           （style 会透传到内部 NScrollbar），虚列表据此裁剪可视区，否则会撑满全部行、
           虚拟滚动完全失效。 -->
      <n-virtual-list
        :items="rows"
        :item-size="itemSize"
        :style="{ height: `${listHeight}px` }"
        key-field="key"
        item-resizable
      >
        <template #default="{ item }">
          <div class="task-row" :class="{ 'two-col': item.tasks.length > 1 }">
            <article
              v-for="task in item.tasks"
              :key="task.gid"
              class="task-card"
              :class="{
                'no-info': !showInfo,
                'has-error': isRealFailure(task.status, task.errorCode),
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

                  <NodeProgressBar
                    class="task-progress"
                    :progress="progress(task)"
                    :pieces="task.pieceProgress"
                    :active="task.status === 'active'"
                    :tone="progressTone(task)"
                  />

                  <div class="meta">
                    <span class="num">{{ progressLabel(task) }}</span>
                    <span class="num">{{ human(task.completedLength) }} / {{ sizeLabel(task) }}</span>
                    <span v-if="task.status === 'active'" class="num speed">
                      {{ human(task.downloadSpeed) }}/s
                    </span>
                    <span v-if="task.status === 'active'" class="num eta">
                      剩余 {{ etaLabel(task) || "计算中" }}
                    </span>
                    <span>{{ task.activeWorkers ?? task.connections }} 连接</span>
                  </div>

                  <div v-if="showInfo" class="path" :title="filePath(task)">
                    {{ filePath(task) || "（路径尚未确定）" }}
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
                <button class="btn-ghost" @click="openFolder(task)">
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
            </article>
          </div>
        </template>
      </n-virtual-list>
    </section>

    <section v-else class="empty-state">
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
      style="width: 500px; max-width: 90vw"
      :bordered="false"
      :segmented="{ content: 'soft', footer: 'soft' }"
    >
      <p v-if="intercepting" class="compose-desc intercept-desc">
        <AppIcon name="download" :size="14" />
        浏览器发来的下载请求，已自动填入地址与 Cookie / Referer，确认后开始下载。
      </p>
      <p v-else class="compose-desc">支持 http / https / ftp / magnet 链接，粘贴后回车即可开始。</p>
      <form class="compose-form" @submit.prevent="addDownload">
        <label class="compose-field">
          <span>下载地址</span>
          <input
            v-model="url"
            type="text"
            placeholder="https://example.com/file.zip"
            spellcheck="false"
            autofocus
          />
        </label>
        <label class="compose-field">
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
      </form>

      <template #footer>
        <div class="compose-actions">
          <button type="button" class="btn-ghost" @click="cancelCompose">取消</button>
          <button
            type="button"
            class="btn-primary"
            :disabled="loading || !url.trim()"
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
        <n-checkbox v-model:checked="removeWithFiles">
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
 * 虚拟滚动（增量 46）：`.tasks` 不再是 grid，而是「固定高度的滚动视口」；
 * 行高由脚本里的 `itemSize` 估算，展开 / 失败行由 `item-resizable` 实测修正。
 * 卡片因此用 min-height 而不是固定 height —— 展开面板才能撑高自身。
 */
.tasks {
  margin-top: 4px;
  padding-right: 2px;
}

.task-row {
  display: grid;
  /* minmax(0,1fr)：允许轨道被压缩到容器宽度，避免长文件名/路径把卡片撑宽。 */
  grid-template-columns: minmax(0, 1fr);
  align-items: start;
  gap: 12px;
  box-sizing: border-box;
  padding-bottom: 12px;
  /* n-virtual-list 的条目轨道宽度由内容决定：显式铺满，卡片才不会按文字宽度收缩。 */
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
  border-radius: var(--r-card);
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

.compose-form {
  display: flex;
  flex-direction: column;
  gap: 14px;
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
  .tasks.two-col {
    grid-template-columns: 1fr;
  }
}
</style>
