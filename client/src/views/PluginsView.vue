<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from "vue";
import { storeToRefs } from "pinia";
import {
  NCheckbox,
  NDropdown,
  NInput,
  NInputNumber,
  NModal,
  NSelect,
  NSwitch,
  type DropdownOption,
} from "naive-ui";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import { errText, listen, type UnlistenFn } from "../ipc";
import {
  permLabel,
  usePluginsStore,
  type PluginInfo,
  type PluginLogEntry,
  type SharedInfo,
} from "../stores/plugins";
import { useNotificationsStore } from "../stores/notifications";
import {
  installPluginFiles,
  installPluginZip,
  readAsBase64,
} from "../utils/pluginInstall";

const store = usePluginsStore();
const notices = useNotificationsStore();
const { plugins, shared, loading: pluginLoading, sharedLoading, message: pluginMessage, grantTarget } =
  storeToRefs(store);

/** 当前 Tab：插件 / 共享组件（beta12.2）。 */
const activeTab = ref<"plugins" | "shared">("plugins");

const searchOpen = ref(false);
const keyword = ref("");

/** 已安装插件按名称 / id / 描述 / 作者过滤（顶部搜索）。 */
const filtered = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  if (!key) return plugins.value;
  return plugins.value.filter(
    (p) =>
      p.name.toLowerCase().includes(key) ||
      p.id.toLowerCase().includes(key) ||
      p.description.toLowerCase().includes(key) ||
      (p.author ?? "").toLowerCase().includes(key),
  );
});

/** 共享组件同样支持顶部搜索。 */
const filteredShared = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  if (!key) return shared.value;
  return shared.value.filter(
    (s) =>
      s.name.toLowerCase().includes(key) ||
      s.id.toLowerCase().includes(key) ||
      (s.description ?? "").toLowerCase().includes(key) ||
      (s.author ?? "").toLowerCase().includes(key),
  );
});

/** 顶栏刷新：只刷新当前 Tab，避免无谓请求。 */
function refreshActive() {
  if (activeTab.value === "shared") void store.loadShared();
  else void store.load();
}

const PALETTE = ["#14b8a6", "#8b5cf6", "#f59e0b", "#3b82f6", "#ec4899", "#22c55e"];

/** 插件彩色图标底色：按 id 稳定散列，避免每次刷新变色（共享组件复用）。 */
function tintOfId(id: string): string {
  let hash = 0;
  for (const ch of id) hash = (hash * 31 + ch.charCodeAt(0)) % 997;
  return PALETTE[hash % PALETTE.length];
}

function tintOf(p: PluginInfo): string {
  return tintOfId(p.id);
}

function initialOf(p: PluginInfo): string {
  return (p.name || p.id).slice(0, 1).toUpperCase();
}

function initialOfShared(s: SharedInfo): string {
  return (s.name || s.id).slice(0, 1).toUpperCase();
}

interface Badge {
  text: string;
  tone: "ok" | "off" | "warn" | "danger";
}

/** 状态徽标：启用绿 / 禁用灰 / 熔断红 / 缺授权橙。 */
function badgesOf(p: PluginInfo): Badge[] {
  const badges: Badge[] = [];
  if (p.enabled) badges.push({ text: "已启用", tone: "ok" });
  else if (!p.circuitBroken) badges.push({ text: "已禁用", tone: "off" });
  if (p.circuitBroken) badges.push({ text: "已熔断", tone: "danger" });
  if (p.pendingPermissions.length > 0) {
    badges.push({ text: `缺少授权 ${p.pendingPermissions.length} 项`, tone: "warn" });
  }
  return badges;
}

// ---------------------------------------------------------------------------
// 勾选与批量操作
// ---------------------------------------------------------------------------

const selected = ref<Set<string>>(new Set());
const batchBusy = ref(false);
const uninstallOpen = ref(false);
/** 本次批量卸载的目标（二次确认期间保持快照）。 */
const uninstallIds = ref<string[]>([]);
/** 目标是否来自勾选（右键单条卸载不清空用户的勾选）。 */
const uninstallFromSelection = ref(false);

const selectedCount = computed(
  () => plugins.value.filter((p) => selected.value.has(p.id)).length,
);

const allSelected = computed(
  () =>
    filtered.value.length > 0 &&
    filtered.value.every((p) => selected.value.has(p.id)),
);

function toggleSelect(id: string) {
  const next = new Set(selected.value);
  if (next.has(id)) next.delete(id);
  else next.add(id);
  selected.value = next;
}

function toggleSelectAll() {
  selected.value = allSelected.value ? new Set() : new Set(filtered.value.map((p) => p.id));
}

function clearSelection() {
  selected.value = new Set();
}

// ---------------------------------------------------------------------------
// 卡片右键菜单（beta12.1）：查看详情 / 查看日志 / 启用禁用 / 卸载
// ---------------------------------------------------------------------------

const ctxMenu = ref<{ show: boolean; x: number; y: number; target: PluginInfo | null }>({
  show: false,
  x: 0,
  y: 0,
  target: null,
});

function openCardContext(event: MouseEvent, p: PluginInfo) {
  event.preventDefault();
  ctxMenu.value = { show: true, x: event.clientX, y: event.clientY, target: p };
}

const ctxOptions = computed<DropdownOption[]>(() => {
  const p = ctxMenu.value.target;
  if (!p) return [];
  return [
    { label: "查看详情", key: "detail" },
    { label: "查看日志", key: "logs" },
    { label: p.enabled ? "禁用" : "启用", key: "toggle" },
    { type: "divider", key: "divider" },
    {
      label: "卸载",
      key: "uninstall",
      props: { style: "color: var(--danger);" },
    },
  ];
});

function closeCtxMenu() {
  ctxMenu.value.show = false;
}

function onCtxSelect(key: string) {
  const p = ctxMenu.value.target;
  closeCtxMenu();
  if (!p) return;
  if (key === "detail") openDetail(p);
  else if (key === "logs") void openLogs(p);
  else if (key === "toggle") void store.toggle(p);
  else if (key === "uninstall") {
    // 复用已有的二次确认弹窗：单条卸载只放一个目标。
    uninstallFromSelection.value = false;
    uninstallIds.value = [p.id];
    uninstallOpen.value = true;
  }
}

// ---------------------------------------------------------------------------
// 共享组件（beta12.2）：右键仅「卸载」，确认后 Toast + 刷新
// ---------------------------------------------------------------------------

const sharedCtx = ref<{
  show: boolean;
  x: number;
  y: number;
  target: SharedInfo | null;
}>({ show: false, x: 0, y: 0, target: null });

function openSharedContext(event: MouseEvent, s: SharedInfo) {
  event.preventDefault();
  sharedCtx.value = { show: true, x: event.clientX, y: event.clientY, target: s };
}

const sharedCtxOptions = computed<DropdownOption[]>(() =>
  sharedCtx.value.target
    ? [
        {
          label: "卸载",
          key: "uninstall",
          props: { style: "color: var(--danger);" },
        },
      ]
    : [],
);

function closeSharedCtxMenu() {
  sharedCtx.value.show = false;
}

function onSharedCtxSelect(key: string) {
  const s = sharedCtx.value.target;
  closeSharedCtxMenu();
  if (!s || key !== "uninstall") return;
  sharedUninstallTarget.value = s;
  sharedUninstallOpen.value = true;
}

const sharedUninstallOpen = ref(false);
const sharedUninstallTarget = ref<SharedInfo | null>(null);
const sharedBusy = ref(false);

function onSharedUninstallShow(value: boolean) {
  sharedUninstallOpen.value = value;
  if (!value && !sharedBusy.value) sharedUninstallTarget.value = null;
}

async function confirmSharedUninstall() {
  const target = sharedUninstallTarget.value;
  if (!target || sharedBusy.value) return;
  sharedUninstallOpen.value = false;
  sharedBusy.value = true;
  try {
    await store.uninstallShared(target.id);
    showToast(`✅ 已卸载共享组件 ${target.name || target.id}`);
  } catch (error) {
    showToast(`❌ 卸载失败：${errText(error)}`, "error");
  } finally {
    sharedBusy.value = false;
    sharedUninstallTarget.value = null;
  }
}

/** 页内轻量 Toast（2.6s 淡出），不依赖全局 alert。 */
const toastText = ref("");
const toastTone = ref<"ok" | "error">("ok");
let toastTimer: number | undefined;

function showToast(text: string, tone: "ok" | "error" = "ok") {
  toastText.value = text;
  toastTone.value = tone;
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastText.value = "";
    toastTimer = undefined;
  }, 2600);
}

async function batchEnable(enabled: boolean) {
  const ids = [...selected.value];
  if (ids.length === 0 || batchBusy.value) return;
  batchBusy.value = true;
  try {
    await store.batchToggle(ids, enabled);
    clearSelection();
  } finally {
    batchBusy.value = false;
  }
}

function requestBatchUninstall() {
  if (selected.value.size === 0) return;
  uninstallFromSelection.value = true;
  uninstallIds.value = [...selected.value];
  uninstallOpen.value = true;
}

function onUninstallShow(value: boolean) {
  uninstallOpen.value = value;
}

async function confirmBatchUninstall() {
  uninstallOpen.value = false;
  batchBusy.value = true;
  try {
    await store.batchUninstall(uninstallIds.value);
    uninstallIds.value = [];
    if (uninstallFromSelection.value) clearSelection();
  } finally {
    batchBusy.value = false;
  }
}

const uninstallNames = computed(() =>
  plugins.value.filter((p) => uninstallIds.value.includes(p.id)).map((p) => p.name),
);

// ---------------------------------------------------------------------------
// 导入插件（beta12.1）：多选 .zip + 整个文件夹（文件夹内可含多个插件）
// ---------------------------------------------------------------------------

const zipInput = ref<HTMLInputElement | null>(null);
const folderInput = ref<HTMLInputElement | null>(null);
const importBusy = ref(false);

function triggerZipPick() {
  if (!importBusy.value) zipInput.value?.click();
}

function triggerFolderPick() {
  if (!importBusy.value) folderInput.value?.click();
}

/** 多选 zip：逐个上传，单个失败不影响其余包。 */
async function onZipPick(ev: Event) {
  const input = ev.target as HTMLInputElement;
  const list = Array.from(input.files ?? []);
  input.value = "";
  if (list.length === 0) return;
  importBusy.value = true;
  try {
    for (const file of list) await installPluginZip(file);
  } finally {
    importBusy.value = false;
  }
}

/**
 * 文件夹导入：保留 `webkitRelativePath` 作为相对路径，
 * 后端会递归识别其中的每个 `manifest.json` 目录并逐个安装。
 */
async function onFolderPick(ev: Event) {
  const input = ev.target as HTMLInputElement;
  const list = Array.from(input.files ?? []);
  input.value = "";
  if (list.length === 0) return;
  importBusy.value = true;
  try {
    const files = await Promise.all(
      list.map(async (file) => ({
        path:
          (file as File & { webkitRelativePath?: string }).webkitRelativePath ||
          file.name,
        data: await readAsBase64(file),
      })),
    );
    const source = files[0]?.path.split("/")[0] || "文件夹";
    await installPluginFiles(files, source);
  } catch (error) {
    notices.push("error", "插件安装失败", `读取文件夹失败：${errText(error)}`);
  } finally {
    importBusy.value = false;
  }
}

// ---------------------------------------------------------------------------
// 详情弹窗：左 manifest / 右动态表单
// ---------------------------------------------------------------------------

const detailOpen = ref(false);
const detailTarget = ref<PluginInfo | null>(null);
const detailDraft = ref<Record<string, unknown>>({});
const detailSaving = ref(false);
const detailError = ref("");
const detailNotice = ref("");
const importInput = ref<HTMLInputElement | null>(null);

function openDetail(p: PluginInfo) {
  const draft: Record<string, unknown> = {};
  for (const item of p.settings) draft[item.key] = item.value ?? item.default;
  detailTarget.value = p;
  detailDraft.value = draft;
  detailError.value = "";
  detailNotice.value = "";
  detailOpen.value = true;
}

/** n-modal 关闭（遮罩 / ESC / 右上角）统一丢弃草稿。 */
function onDetailShow(value: boolean) {
  detailOpen.value = value;
  if (value) return;
  detailTarget.value = null;
  detailDraft.value = {};
  detailError.value = "";
  detailNotice.value = "";
}

async function saveDetail() {
  const target = detailTarget.value;
  if (!target || detailSaving.value) return;
  detailSaving.value = true;
  detailError.value = "";
  detailNotice.value = "";
  try {
    await store.saveSettings(target, normalizedDraft(target));
    onDetailShow(false);
  } catch (error) {
    detailError.value = `保存失败：${errText(error)}`;
  } finally {
    detailSaving.value = false;
  }
}

/** 草稿读写：模板里避免 TS 断言，统一走显式取值函数。 */
function boolOf(key: string): boolean {
  return detailDraft.value[key] === true;
}
function setBool(key: string, value: boolean) {
  detailDraft.value[key] = value;
}
function strOf(key: string): string {
  const value = detailDraft.value[key];
  return typeof value === "string" ? value : "";
}
function setStr(key: string, value: string) {
  detailDraft.value[key] = value;
}
function setSelect(key: string, value: unknown) {
  detailDraft.value[key] =
    value === null || value === undefined ? "" : String(value);
}
/** 数字控件：草稿非数字（含空值 / NaN）时返回 null，交给 NInputNumber 显示占位。 */
function numOf(key: string): number | null {
  const value = detailDraft.value[key];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
function setNum(key: string, value: number | null) {
  detailDraft.value[key] = value;
}
function selectOptions(options: string[]) {
  return options.map((option) => ({ label: option, value: option }));
}
/** 整行布局：多行文本 / 数字输入标签在上、控件占满宽度，其余保持两列。 */
function isFullRow(type: string): boolean {
  return type === "textarea" || type === "text" || type === "number";
}

/** 保存前归一化：number 项必须是 JSON 数字，null 回落 schema default（非数字则 0）。 */
function normalizedDraft(target: PluginInfo): Record<string, unknown> {
  const payload: Record<string, unknown> = { ...detailDraft.value };
  for (const item of target.settings) {
    if (item.type !== "number") continue;
    const value = payload[item.key];
    if (typeof value === "number" && Number.isFinite(value)) continue;
    payload[item.key] = typeof item.default === "number" ? item.default : 0;
  }
  return payload;
}

// -- 导出 / 导入配置（纯前端，不新增 IPC） -----------------------------------

function exportConfig() {
  const target = detailTarget.value;
  if (!target) return;
  const payload = {
    id: target.id,
    version: target.version,
    settings: { ...detailDraft.value },
  };
  const blob = new Blob([JSON.stringify(payload, null, 2)], {
    type: "application/json",
  });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `plugin-config-${target.id}.json`;
  link.click();
  URL.revokeObjectURL(url);
}

function triggerImport() {
  importInput.value?.click();
}

/** 导入只填充草稿，点「保存」才落盘（后端仍做 schema / 类型校验）。 */
async function onImportFile(ev: Event) {
  const input = ev.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = "";
  const target = detailTarget.value;
  if (!file || !target) return;
  try {
    const raw = JSON.parse(await file.text()) as { id?: unknown; settings?: unknown };
    if (typeof raw.id === "string" && raw.id !== target.id) {
      throw new Error(`配置属于 ${raw.id}，与当前插件 ${target.id} 不匹配`);
    }
    if (!raw.settings || typeof raw.settings !== "object" || Array.isArray(raw.settings)) {
      throw new Error("缺少 settings 对象");
    }
    const incoming = raw.settings as Record<string, unknown>;
    const next = { ...detailDraft.value };
    for (const item of target.settings) {
      if (Object.prototype.hasOwnProperty.call(incoming, item.key)) {
        next[item.key] = incoming[item.key];
      }
    }
    detailDraft.value = next;
    detailError.value = "";
    detailNotice.value = "已导入配置草稿，点击「保存」后生效";
  } catch (error) {
    detailNotice.value = "";
    detailError.value = `导入失败：${errText(error)}`;
  }
}

// ---------------------------------------------------------------------------
// 日志终端
// ---------------------------------------------------------------------------

interface PluginLogPayload {
  pluginId: string;
  entry: PluginLogEntry;
}

const logsOpen = ref(false);
const logTarget = ref<PluginInfo | null>(null);
const logEntries = ref<PluginLogEntry[]>([]);
const logBox = ref<HTMLElement | null>(null);
const logError = ref("");
const logHint = ref("");
/** 用户上滚后暂停自动滚底，回到底部附近自动恢复。 */
const logAutoScroll = ref(true);
let unlistenLog: UnlistenFn | undefined;
/** 打开序号：快速切换插件时丢弃过期请求，避免订阅串台 / 泄漏。 */
let logRequest = 0;

function stopLogListen() {
  if (!unlistenLog) return;
  unlistenLog();
  unlistenLog = undefined;
}

async function openLogs(p: PluginInfo) {
  const token = ++logRequest;
  stopLogListen();
  logTarget.value = p;
  logEntries.value = [];
  logError.value = "";
  logHint.value = "";
  logAutoScroll.value = true;
  logsOpen.value = true;
  let history: PluginLogEntry[] = [];
  try {
    history = await store.loadLogs(p.id, 500);
  } catch (error) {
    if (token !== logRequest) return;
    logError.value = `日志加载失败：${errText(error)}`;
  }
  if (token !== logRequest || !logsOpen.value) return;
  logEntries.value = history;
  await nextTick();
  scrollLogToBottom();
  const off = await listen<PluginLogPayload>("plugin://log", (ev) => {
    if (token !== logRequest) return;
    const payload = ev.payload;
    if (!payload || payload.pluginId !== p.id || !payload.entry) return;
    // 环形缓冲上限 2000 行：长期挂着日志弹窗也不会无限增长 DOM。
    logEntries.value = [...logEntries.value.slice(-1999), payload.entry];
    if (logAutoScroll.value) void nextTick(scrollLogToBottom);
  });
  // 异步订阅期间用户已关闭 / 切换弹窗：立刻退订，避免泄漏。
  if (token !== logRequest || !logsOpen.value) {
    off();
    return;
  }
  unlistenLog = off;
}

function onLogsShow(value: boolean) {
  logsOpen.value = value;
  if (value) return;
  logRequest += 1;
  stopLogListen();
  logTarget.value = null;
  logEntries.value = [];
  logError.value = "";
  logHint.value = "";
}

function closeLogs() {
  onLogsShow(false);
}

function scrollLogToBottom() {
  const el = logBox.value;
  if (el) el.scrollTop = el.scrollHeight;
}

function onLogScroll() {
  const el = logBox.value;
  if (!el) return;
  logAutoScroll.value = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
}

function timeLabel(ts: number): string {
  // 后端 ts 为 Unix 秒；兼容毫秒值，避免出现 1970 年。
  const date = new Date(ts > 1e12 ? ts : ts * 1000);
  if (Number.isNaN(date.getTime())) return "--:--:--";
  return [date.getHours(), date.getMinutes(), date.getSeconds()]
    .map((value) => String(value).padStart(2, "0"))
    .join(":");
}

function formatLog(entry: PluginLogEntry): string {
  return `${timeLabel(entry.ts)} [${entry.level.toUpperCase()}] ${entry.msg}`;
}

async function clearLog() {
  const target = logTarget.value;
  if (!target) return;
  try {
    await store.clearLogs(target.id);
    logEntries.value = [];
    logError.value = "";
    logHint.value = "日志已清空";
  } catch (error) {
    logError.value = `清空失败：${errText(error)}`;
  }
}

async function copyLog() {
  if (logEntries.value.length === 0) {
    logHint.value = "暂无日志";
    return;
  }
  try {
    await navigator.clipboard.writeText(logEntries.value.map(formatLog).join("\n"));
    logHint.value = `已复制 ${logEntries.value.length} 行`;
  } catch {
    logHint.value = "复制失败";
  }
}

onMounted(() => {
  void store.load();
  void store.loadShared();
});

onBeforeUnmount(() => {
  stopLogListen();
  if (toastTimer !== undefined) window.clearTimeout(toastTimer);
});
</script>

<template>
  <div class="view-plugins">
    <PageHeader title="插件" subtitle="扩展宿主能力（QuickJS / Wasm）">
      <template #actions>
        <div class="head-search" :class="{ open: searchOpen }">
          <input
            v-if="searchOpen"
            v-model="keyword"
            type="search"
            placeholder="搜索插件"
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
        <button class="icon-btn" title="刷新" @click="refreshActive">
          <AppIcon name="refresh" :size="17" />
        </button>
      </template>
    </PageHeader>

    <!-- Tab：插件 / 共享组件（beta12.2） -->
    <div class="plugin-tabs" role="tablist">
      <button
        class="plugin-tab"
        :class="{ active: activeTab === 'plugins' }"
        role="tab"
        :aria-selected="activeTab === 'plugins'"
        @click="activeTab = 'plugins'"
      >
        插件
        <span class="tab-count num">{{ plugins.length }}</span>
      </button>
      <button
        class="plugin-tab"
        :class="{ active: activeTab === 'shared' }"
        role="tab"
        :aria-selected="activeTab === 'shared'"
        @click="activeTab = 'shared'"
      >
        共享组件
        <span class="tab-count num">{{ shared.length }}</span>
      </button>
    </div>

    <!-- 安装说明：拖拽入口在 App.vue，这里补文件选择入口（beta12.1 多选） -->
    <div class="install-hint">
      <span class="hint-icon">
        <AppIcon name="upload" :size="17" />
      </span>
      <div class="hint-text">
        <b>安装插件</b>
        <small>拖入窗口，或选择 .zip / 文件夹（支持一次多选、包内多插件）</small>
      </div>
      <div class="hint-actions">
        <button class="batch-btn" :disabled="importBusy" @click="triggerZipPick">
          <AppIcon name="import" :size="13" />
          <span>{{ importBusy ? "安装中..." : "选择 .zip" }}</span>
        </button>
        <button class="batch-btn" :disabled="importBusy" @click="triggerFolderPick">
          <AppIcon name="folder" :size="13" />
          <span>选择文件夹</span>
        </button>
      </div>
      <input
        ref="zipInput"
        type="file"
        accept=".zip,application/zip"
        multiple
        class="file-input"
        @change="onZipPick"
      />
      <input
        ref="folderInput"
        type="file"
        webkitdirectory
        multiple
        class="file-input"
        @change="onFolderPick"
      />
    </div>

    <p v-if="pluginMessage" class="status">{{ pluginMessage }}</p>

    <!-- 批量控制栏：逐条 invoke，不新增批量后端命令（仅插件 Tab） -->
    <div v-if="activeTab === 'plugins'" class="batch-bar">
      <label class="select-all">
        <n-checkbox
          :checked="allSelected"
          :indeterminate="selectedCount > 0 && !allSelected"
          @update:checked="toggleSelectAll"
        />
        <span>全选</span>
      </label>
      <span class="batch-count num">已选 {{ selectedCount }} / 共 {{ plugins.length }}</span>
      <div class="batch-actions">
        <button
          class="batch-btn"
          :disabled="selectedCount === 0 || batchBusy"
          @click="batchEnable(true)"
        >
          <AppIcon name="play" :size="13" />
          <span>批量启用</span>
        </button>
        <button
          class="batch-btn danger-outline"
          :disabled="selectedCount === 0 || batchBusy"
          @click="batchEnable(false)"
        >
          <AppIcon name="pause" :size="13" />
          <span>批量禁用</span>
        </button>
        <button
          class="batch-btn danger-text"
          :disabled="selectedCount === 0 || batchBusy"
          @click="requestBatchUninstall"
        >
          <AppIcon name="trash" :size="13" />
          <span>批量卸载</span>
        </button>
      </div>
    </div>

    <section v-if="activeTab === 'plugins'" class="plugin-grid">
      <p v-if="pluginLoading && plugins.length === 0" class="empty">加载中...</p>
      <p v-else-if="filtered.length === 0" class="empty">未发现任何插件</p>

      <article
        v-for="p in filtered"
        :key="p.id"
        class="plugin-card"
        :class="{ off: !p.enabled, selected: selected.has(p.id) }"
        @click="openDetail(p)"
        @contextmenu.prevent="openCardContext($event, p)"
      >
        <div class="card-top">
          <span
            class="plugin-icon"
            :style="p.icon ? undefined : { background: tintOf(p) }"
          >
            <img v-if="p.icon" :src="p.icon" alt="" />
            <template v-else>{{ initialOf(p) }}</template>
          </span>

          <div class="card-meta">
            <div class="name-row">
              <span class="name" :title="p.id">{{ p.name }}</span>
              <span class="version">v{{ p.version }}</span>
            </div>
            <span class="author">{{ p.author ? `作者 ${p.author}` : "未知作者" }}</span>
          </div>

          <div class="card-tools" @click.stop>
            <label class="card-check" title="选择插件">
              <n-checkbox
                :checked="selected.has(p.id)"
                @update:checked="toggleSelect(p.id)"
              />
            </label>
            <button class="log-btn" title="查看日志" @click.stop="openLogs(p)">
              <AppIcon name="terminal" :size="15" />
            </button>
            <n-switch size="small" :value="p.enabled" @update:value="store.toggle(p)" />
          </div>
        </div>

        <p class="card-desc">{{ p.description || "（无描述）" }}</p>

        <div class="badges">
          <span v-for="badge in badgesOf(p)" :key="badge.text" class="chip" :class="badge.tone">
            {{ badge.text }}
          </span>
        </div>

        <div class="card-foot">
          <span class="hook-line" :title="p.hooks.join('、')">
            Hooks：{{ p.hooks.length > 0 ? p.hooks.join("、") : "未注册" }}
          </span>
          <span class="priority">优先级 {{ p.priority }}</span>
        </div>
      </article>
    </section>

    <!-- 共享组件：卡片展示名称/版本/作者/文件数，右键仅「卸载」 -->
    <section v-else class="plugin-grid">
      <p v-if="sharedLoading && shared.length === 0" class="empty">加载中...</p>
      <p v-else-if="filteredShared.length === 0" class="empty">
        未安装共享组件（如 FFmpeg 共享包）
      </p>

      <article
        v-for="s in filteredShared"
        :key="s.id"
        class="plugin-card shared-card"
        @contextmenu.prevent="openSharedContext($event, s)"
      >
        <div class="card-top">
          <span class="plugin-icon" :style="{ background: tintOfId(s.id) }">
            {{ initialOfShared(s) }}
          </span>

          <div class="card-meta">
            <div class="name-row">
              <span class="name" :title="s.id">{{ s.name || s.id }}</span>
              <span class="version">v{{ s.version }}</span>
            </div>
            <span class="author">{{ s.author ? `作者 ${s.author}` : "未知作者" }}</span>
          </div>
        </div>

        <p class="card-desc">{{ s.description || "（共享依赖包，供插件调用）" }}</p>

        <div class="badges">
          <span class="chip ok">共享组件</span>
        </div>

        <div class="card-foot">
          <span class="hook-line mono" :title="s.dir">{{ s.dir }}</span>
          <span class="priority">{{ s.fileCount }} 个文件</span>
        </div>
      </article>
    </section>

    <!-- 卡片右键菜单（manual 模式，坐标取自 contextmenu 事件） -->
    <n-dropdown
      trigger="manual"
      placement="bottom-start"
      :show="ctxMenu.show"
      :x="ctxMenu.x"
      :y="ctxMenu.y"
      :options="ctxOptions"
      @select="onCtxSelect"
      @clickoutside="closeCtxMenu"
    />

    <!-- 共享组件右键菜单：仅「卸载」 -->
    <n-dropdown
      trigger="manual"
      placement="bottom-start"
      :show="sharedCtx.show"
      :x="sharedCtx.x"
      :y="sharedCtx.y"
      :options="sharedCtxOptions"
      @select="onSharedCtxSelect"
      @clickoutside="closeSharedCtxMenu"
    />

    <!-- 权限确认弹窗（逻辑保持不变，仅换肤） -->
    <div v-if="grantTarget" class="modal-mask" @click.self="store.cancelGrant()">
      <div class="modal" role="dialog" aria-modal="true">
        <header class="modal-head">
          <h2>授权确认</h2>
          <button class="icon-btn" title="关闭" @click="store.cancelGrant()">
            <AppIcon name="close" :size="16" />
          </button>
        </header>
        <p class="modal-body">
          插件 <strong>{{ grantTarget.name }}</strong> 请求新增以下权限。授权后，该插件将获得这些能力的访问权。
        </p>
        <ul class="perm-list">
          <li v-for="perm in grantTarget.pendingPermissions" :key="perm">
            <span class="perm-code">{{ perm }}</span>
            <span class="perm-label">{{ permLabel(perm) }}</span>
          </li>
        </ul>
        <div class="modal-actions">
          <button class="btn-primary" @click="store.confirmGrant()">同意并启用</button>
        </div>
      </div>
    </div>

    <!-- 详情弹窗：左 manifest / 右动态表单 -->
    <n-modal
      :show="detailOpen"
      preset="card"
      class="plugin-detail-modal"
      :title="detailTarget ? detailTarget.name : '插件详情'"
      :style="{ width: '860px', maxWidth: '94vw' }"
      :mask-closable="!detailSaving"
      @update:show="onDetailShow"
    >
      <div v-if="detailTarget" class="detail-body">
        <div class="detail-left">
          <div class="meta-row">
            <span class="meta-key">ID</span>
            <span class="meta-val mono">{{ detailTarget.id }}</span>
          </div>
          <div class="meta-row">
            <span class="meta-key">版本</span>
            <span class="meta-val">v{{ detailTarget.version }}</span>
          </div>
          <div class="meta-row">
            <span class="meta-key">作者</span>
            <span class="meta-val">{{ detailTarget.author || "未知" }}</span>
          </div>
          <div class="meta-row">
            <span class="meta-key">优先级</span>
            <span class="meta-val">{{ detailTarget.priority }}</span>
          </div>
          <div class="meta-row">
            <span class="meta-key">入口</span>
            <span class="meta-val mono">{{ detailTarget.entry || "—" }}</span>
          </div>

          <div class="meta-block">
            <span class="meta-key">描述</span>
            <p>{{ detailTarget.description || "（无描述）" }}</p>
          </div>

          <div class="meta-block">
            <span class="meta-key">Hooks</span>
            <div v-if="detailTarget.hooks.length > 0" class="tag-list">
              <span v-for="hook in detailTarget.hooks" :key="hook" class="tag">{{ hook }}</span>
            </div>
            <p v-else>未注册</p>
          </div>

          <div class="meta-block">
            <span class="meta-key">权限</span>
            <div v-if="detailTarget.permissions.length > 0" class="tag-list">
              <span
                v-for="perm in detailTarget.permissions"
                :key="perm"
                class="tag perm-tag"
                :title="perm"
              >
                {{ permLabel(perm) }}
              </span>
            </div>
            <p v-else>无额外权限</p>
          </div>
        </div>

        <div class="detail-right">
          <h4 class="section-title">配置项</h4>
          <p v-if="detailTarget.settings.length === 0" class="empty-settings">
            该插件未声明配置项
          </p>
          <div v-else class="settings-form">
            <div
              v-for="item in detailTarget.settings"
              :key="item.key"
              class="settings-row"
              :class="{ full: isFullRow(item.type) }"
            >
              <div class="settings-copy">
                <span>{{ item.label }}</span>
                <small>{{ item.key }}</small>
              </div>
              <n-switch
                v-if="item.type === 'boolean'"
                :value="boolOf(item.key)"
                @update:value="setBool(item.key, $event)"
              />
              <n-select
                v-else-if="item.type === 'select'"
                :value="strOf(item.key)"
                :options="selectOptions(item.options)"
                class="settings-control"
                :consistent-menu-width="false"
                @update:value="setSelect(item.key, $event)"
              />
              <n-input
                v-else-if="item.type === 'textarea' || item.type === 'text'"
                :value="strOf(item.key)"
                type="textarea"
                :rows="4"
                :autosize="false"
                class="settings-control"
                placeholder="请输入"
                @update:value="setStr(item.key, $event)"
              />
              <n-input-number
                v-else-if="item.type === 'number'"
                :value="numOf(item.key)"
                class="settings-control"
                placeholder="请输入数字"
                @update:value="setNum(item.key, $event)"
              />
              <n-input
                v-else
                :value="strOf(item.key)"
                class="settings-control"
                placeholder="请输入"
                @update:value="setStr(item.key, $event)"
              />
            </div>
          </div>
        </div>
      </div>

      <p v-if="detailError" class="form-error">{{ detailError }}</p>
      <p v-if="detailNotice" class="form-notice">{{ detailNotice }}</p>

      <template #footer>
        <div class="detail-actions">
          <input
            ref="importInput"
            type="file"
            accept=".json,application/json"
            class="file-input"
            @change="onImportFile"
          />
          <button class="btn-ghost" :disabled="detailSaving" @click="triggerImport">
            <AppIcon name="import" :size="14" />
            <span>导入配置</span>
          </button>
          <button class="btn-ghost" :disabled="detailSaving" @click="exportConfig">
            <AppIcon name="upload" :size="14" />
            <span>导出配置</span>
          </button>
          <button class="btn-primary" :disabled="detailSaving" @click="saveDetail">
            {{ detailSaving ? "保存中..." : "保存" }}
          </button>
        </div>
      </template>
    </n-modal>

    <!-- 日志终端：黑底绿字，打开回填历史并订阅增量 -->
    <n-modal
      :show="logsOpen"
      preset="card"
      class="log-modal"
      :bordered="false"
      :closable="false"
      :style="{ width: '860px', maxWidth: '94vw' }"
      @update:show="onLogsShow"
    >
      <template #header>
        <div class="log-title">
          <AppIcon name="terminal" :size="16" />
          <span>{{ logTarget ? `${logTarget.name} · 运行日志` : "运行日志" }}</span>
          <span v-if="logHint" class="log-hint">{{ logHint }}</span>
        </div>
      </template>
      <template #header-extra>
        <div class="log-tools">
          <button class="btn-ghost" @click="clearLog">清空日志</button>
          <button class="btn-ghost" @click="copyLog">复制全部</button>
          <button class="icon-btn close-btn" title="关闭" @click="closeLogs">
            <AppIcon name="close" :size="16" />
          </button>
        </div>
      </template>

      <p v-if="logError" class="log-error">{{ logError }}</p>
      <div ref="logBox" class="log-console" @scroll="onLogScroll">
        <p v-if="logEntries.length === 0" class="log-empty">暂无日志</p>
        <div
          v-for="(entry, index) in logEntries"
          :key="index"
          class="log-line"
          :class="entry.level"
        >
          <span class="log-time">{{ timeLabel(entry.ts) }}</span>
          <span class="log-level">[{{ entry.level.toUpperCase() }}]</span>
          <span class="log-msg">{{ entry.msg }}</span>
        </div>
      </div>
    </n-modal>

    <!-- 批量卸载二次确认 -->
    <n-modal
      :show="uninstallOpen"
      preset="dialog"
      type="error"
      title="卸载插件"
      positive-text="确认卸载"
      negative-text="取消"
      :positive-button-props="{ loading: batchBusy, disabled: batchBusy }"
      @positive-click="confirmBatchUninstall"
      @update:show="onUninstallShow"
    >
      <p v-if="uninstallIds.length === 1" class="confirm-copy">
        确定卸载插件「{{ uninstallNames[0] || uninstallIds[0] }}」？插件目录与已保存配置将被删除，且不可恢复。
      </p>
      <p v-else class="confirm-copy">
        确定卸载已选的 {{ uninstallIds.length }} 个插件？插件目录与已保存配置将被删除，且不可恢复。
      </p>
      <p v-if="uninstallNames.length > 1" class="confirm-list">
        {{ uninstallNames.join("、") }}
      </p>
    </n-modal>

    <!-- 共享组件卸载二次确认 -->
    <n-modal
      :show="sharedUninstallOpen"
      preset="dialog"
      type="error"
      title="卸载共享组件"
      positive-text="确认卸载"
      negative-text="取消"
      :positive-button-props="{ loading: sharedBusy, disabled: sharedBusy }"
      @positive-click="confirmSharedUninstall"
      @update:show="onSharedUninstallShow"
    >
      <p class="confirm-copy">
        确定卸载共享组件「{{ sharedUninstallTarget?.name || sharedUninstallTarget?.id }}」？依赖它的插件可能无法继续运行。
      </p>
      <p v-if="sharedUninstallTarget?.dir" class="confirm-list">
        {{ sharedUninstallTarget.dir }}
      </p>
    </n-modal>

    <!-- 页内 Toast：共享组件卸载结果 -->
    <Teleport to="body">
      <Transition name="page-toast">
        <div v-if="toastText" class="page-toast" :class="toastTone">
          {{ toastText }}
        </div>
      </Transition>
    </Teleport>
  </div>
</template>

<style scoped>
/* ---- Tab：插件 / 共享组件 ---- */
.plugin-tabs {
  display: inline-flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-bottom: 14px;
  padding: 3px;
  border: 1px solid var(--line);
  border-radius: var(--r-btn);
  background: var(--surface-2);
}
.plugin-tab {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 30px;
  padding: 0 14px;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: var(--muted);
  font-size: 12.5px;
}
.plugin-tab:hover:not(:disabled) {
  border-color: transparent;
  color: var(--text);
}
.plugin-tab.active {
  background: var(--accent-soft);
  color: var(--accent);
  font-weight: 600;
}
.tab-count {
  min-width: 16px;
  padding: 0 4px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--line) 45%, transparent);
  color: var(--muted);
  font-size: 10.5px;
  line-height: 16px;
  text-align: center;
}
.plugin-tab.active .tab-count {
  background: color-mix(in srgb, var(--accent) 22%, transparent);
  color: var(--accent);
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
}

.install-hint {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 14px;
  padding: 11px 16px;
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-card);
  background: var(--surface-2);
}
.hint-icon {
  flex: none;
  display: grid;
  place-items: center;
  width: 34px;
  height: 34px;
  border-radius: 12px;
  background: var(--surface);
  color: var(--accent);
  box-shadow: var(--shadow-card);
}
.hint-text {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.hint-text b {
  font-size: 13px;
  color: var(--text);
}
.hint-text small {
  font-size: 11.5px;
  color: var(--muted);
}
.hint-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-left: auto;
}

/* ---- 批量控制栏 ---- */
.batch-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  margin-bottom: 12px;
  padding: 8px 12px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface-2);
}
.select-all {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  color: var(--muted);
  cursor: pointer;
}
.batch-count {
  font-size: 12px;
  color: var(--text-2);
}
.batch-actions {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-left: auto;
}
.batch-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 30px;
  padding: 0 12px;
  border: 1px solid var(--line);
  border-radius: 8px;
  background: var(--surface);
  color: var(--text-2);
  font-size: 12px;
}
.batch-btn:hover:not(:disabled) {
  border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
  color: var(--accent);
}
.batch-btn.danger-outline {
  color: var(--danger);
  border-color: color-mix(in srgb, var(--danger) 38%, var(--line));
}
.batch-btn.danger-outline:hover:not(:disabled) {
  background: var(--danger-soft);
  border-color: var(--danger);
  color: var(--danger);
}
.batch-btn.danger-text {
  color: var(--danger);
}
.batch-btn.danger-text:hover:not(:disabled) {
  background: var(--danger-soft);
  border-color: var(--danger);
  color: var(--danger);
}

/* ---- 网格卡片 ---- */
.plugin-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
  gap: 14px;
}

.plugin-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-width: 0;
  padding: 16px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease,
    opacity 0.2s ease;
}
.plugin-card:hover {
  border-color: color-mix(in srgb, var(--accent) 38%, var(--line));
  box-shadow: var(--shadow-float);
}
.plugin-card.off {
  opacity: 0.68;
}
.plugin-card.selected {
  border-color: color-mix(in srgb, var(--accent) 70%, var(--line));
}
/* 共享组件没有详情弹窗，仅右键操作，避免误导为可点击。 */
.shared-card {
  cursor: default;
}

.card-top {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  min-width: 0;
}

.plugin-icon {
  flex: none;
  display: grid;
  place-items: center;
  width: 42px;
  height: 42px;
  overflow: hidden;
  border-radius: 13px;
  background: var(--surface-3);
  color: #ffffff;
  font-size: 16px;
  font-weight: 700;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.08);
}
.plugin-icon img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}

.card-meta {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.name-row {
  display: flex;
  align-items: baseline;
  gap: 7px;
  min-width: 0;
}
.name {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 14px;
  font-weight: 650;
  color: var(--text);
}
.version {
  flex: none;
  font-size: 11px;
  color: var(--muted);
}
.author {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-size: 11.5px;
  color: var(--muted);
}

.card-tools {
  flex: none;
  display: flex;
  align-items: center;
  gap: 4px;
}
.card-check {
  display: inline-flex;
  align-items: center;
  cursor: pointer;
}
.log-btn {
  display: grid;
  place-items: center;
  width: 28px;
  height: 28px;
  padding: 0;
  border-color: transparent;
  border-radius: 8px;
  background: transparent;
  color: var(--muted);
}
.log-btn:hover:not(:disabled) {
  border-color: transparent;
  background: var(--surface-3);
  color: var(--accent);
}

.card-desc {
  margin: 0;
  font-size: 12px;
  line-height: 1.55;
  color: var(--text-2);
  display: -webkit-box;
  overflow: hidden;
  -webkit-box-orient: vertical;
  -webkit-line-clamp: 2;
}

.badges {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.chip {
  padding: 2px 9px;
  border-radius: 999px;
  font-size: 10.5px;
  font-weight: 500;
}
.chip.ok {
  background: var(--success-soft);
  color: var(--success);
}
.chip.warn {
  background: var(--warn-soft);
  color: var(--warn);
}
.chip.danger {
  background: var(--danger-soft);
  color: var(--danger);
}
.chip.off {
  background: var(--surface-3);
  color: var(--muted);
}

.card-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  margin-top: auto;
  padding-top: 9px;
  border-top: 1px solid var(--line);
  font-size: 11px;
  color: var(--muted);
}
.hook-line {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.priority {
  flex: none;
}

/* ---- 详情弹窗 ---- */
/* 弹窗限高 80vh：头部/底栏固定，左右两栏同处内容区滚动。 */
.plugin-detail-modal {
  max-height: 80vh;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.plugin-detail-modal :deep(.n-card__header) {
  flex-shrink: 0;
}
.plugin-detail-modal :deep(.n-card__content) {
  padding: 16px 20px;
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
}
.plugin-detail-modal :deep(.n-card__footer) {
  flex-shrink: 0;
}

.detail-body {
  display: grid;
  grid-template-columns: 320px minmax(0, 1fr);
  gap: 22px;
}

.detail-left {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-width: 0;
  padding-right: 18px;
  border-right: 1px solid var(--line);
}

.meta-row {
  display: flex;
  align-items: baseline;
  gap: 10px;
  font-size: 12.5px;
}
.meta-key {
  flex: none;
  width: 52px;
  font-size: 11.5px;
  color: var(--muted);
}
.meta-val {
  min-width: 0;
  color: var(--text);
  word-break: break-all;
}
.mono {
  font-family: ui-monospace, Consolas, "Cascadia Mono", monospace;
  font-size: 11.5px;
}

.meta-block {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12.5px;
}
.meta-block p {
  margin: 0;
  line-height: 1.6;
  color: var(--text-2);
}
.tag-list {
  display: flex;
  flex-wrap: wrap;
  gap: 5px;
}
.tag {
  padding: 2px 8px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--text-2);
  font-size: 10.5px;
}
.perm-tag {
  background: var(--accent-soft);
  color: var(--accent);
}

.detail-right {
  min-width: 0;
}
.section-title {
  margin: 0 0 10px;
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.3px;
  color: var(--muted);
}
.empty-settings {
  margin: 0;
  padding: 28px 16px;
  border-radius: 12px;
  background: var(--surface-2);
  color: var(--muted);
  font-size: 12.5px;
  text-align: center;
}

.settings-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.settings-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 14px;
  padding: 10px 12px;
  border-radius: 10px;
  background: var(--surface-2);
}
.settings-row.full {
  flex-direction: column;
  align-items: stretch;
  gap: 8px;
}
.settings-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.settings-copy > span {
  font-size: 13px;
  color: var(--text);
}
.settings-copy > small {
  font-size: 11px;
  color: var(--muted);
  font-family: ui-monospace, Consolas, monospace;
}
.settings-control {
  flex: none;
  width: 220px;
}
.settings-row.full .settings-control {
  width: 100%;
}

.form-error {
  margin: 10px 0 0;
  color: var(--danger);
  font-size: 12px;
}
.form-notice {
  margin: 10px 0 0;
  color: var(--accent);
  font-size: 12px;
}

.detail-actions {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 8px;
}
.detail-actions button {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}
.file-input {
  display: none;
}

/* ---- 日志终端 ---- */
.log-modal :deep(.n-card__content) {
  padding: 4px 14px 14px;
}
.log-title {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  font-size: 14px;
  font-weight: 600;
  color: var(--text);
}
.log-hint {
  font-size: 11.5px;
  font-weight: 400;
  color: var(--muted);
}
.log-tools {
  display: flex;
  align-items: center;
  gap: 6px;
}
.log-tools button {
  height: 30px;
  padding: 0 10px;
  border-radius: 8px;
  font-size: 12px;
}
.close-btn {
  width: 30px;
  height: 30px;
}
.log-error {
  margin: 0 0 8px;
  color: var(--danger);
  font-size: 12px;
}
.log-console {
  height: 420px;
  overflow: auto;
  padding: 12px 14px;
  border-radius: 12px;
  background: #0b1014;
  color: #7ee787;
  font-family: ui-monospace, Consolas, "Cascadia Mono", monospace;
  font-size: 12px;
  line-height: 1.65;
  user-select: text;
}
.log-line {
  display: flex;
  gap: 10px;
  white-space: pre-wrap;
  word-break: break-word;
}
.log-time {
  flex: none;
  color: #5b6b73;
}
.log-level {
  flex: none;
}
.log-msg {
  min-width: 0;
  white-space: pre-wrap;
  word-break: break-word;
}
.log-line.warn {
  color: #fbbf24;
}
.log-line.error {
  color: #f87171;
}
.log-line.debug {
  color: #8b949e;
}
.log-empty {
  margin: 0;
  color: #5b6b73;
}

.confirm-copy {
  margin: 0;
  color: var(--text-2);
  font-size: 13px;
  line-height: 1.6;
}
.confirm-list {
  margin: 8px 0 0;
  color: var(--muted);
  font-size: 12px;
  word-break: break-all;
}

@media (max-width: 900px) {
  .detail-body {
    grid-template-columns: 1fr;
  }
  .detail-left {
    padding-right: 0;
    padding-bottom: 14px;
    border-right: 0;
    border-bottom: 1px solid var(--line);
  }
}

/* ---- 授权弹窗 ---- */
.modal-mask {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 20px;
  background: rgba(20, 40, 40, 0.28);
  backdrop-filter: blur(2px);
}

.modal {
  width: min(480px, 100%);
  padding: 20px 22px;
  border: 1px solid var(--line);
  border-radius: 12px;
  background: var(--surface);
  box-shadow: var(--shadow-float);
}

.modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.modal-head h2 {
  margin: 0;
  font-size: 16px;
  font-weight: 700;
}

.modal-body {
  margin: 10px 0 14px;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-2);
}

.perm-list {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.perm-list li {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 7px 11px;
  border-radius: 10px;
  background: var(--surface-2);
  font-size: 12px;
}
.perm-code {
  color: var(--accent);
  font-weight: 600;
}
.perm-label {
  color: var(--text-2);
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 16px;
}
.modal-actions button {
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}

/* ---- 页内 Toast（共享组件卸载结果） ---- */
.page-toast {
  position: fixed;
  left: 50%;
  bottom: 34px;
  z-index: 2000;
  max-width: 88vw;
  padding: 10px 18px;
  border: 1px solid color-mix(in srgb, var(--success) 35%, var(--line));
  border-radius: 12px;
  background: var(--surface);
  color: var(--success);
  box-shadow: var(--shadow-float);
  font-size: 12.5px;
  transform: translateX(-50%);
}
.page-toast.error {
  border-color: color-mix(in srgb, var(--danger) 35%, var(--line));
  color: var(--danger);
}
.page-toast-enter-active,
.page-toast-leave-active {
  transition:
    opacity 0.18s ease,
    transform 0.18s ease;
}
.page-toast-enter-from,
.page-toast-leave-to {
  opacity: 0;
  transform: translate(-50%, 8px);
}
</style>
