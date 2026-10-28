<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { storeToRefs } from "pinia";
import { invoke, listen, errText, type UnlistenFn } from "./ipc";
import { useRoute, useRouter } from "vue-router";
import {
  darkTheme,
  lightTheme,
  NConfigProvider,
  NNotificationProvider,
  type GlobalThemeOverrides,
} from "naive-ui";
import Sidebar from "./components/Sidebar.vue";
import TopBar from "./components/TopBar.vue";
import AppIcon from "./components/AppIcon.vue";
import SettingsView from "./views/SettingsView.vue";
import UpdateNotifier from "./components/UpdateNotifier.vue";
import { useDownloadsStore, type PluginTask } from "./stores/downloads";
import { useInterceptStore, type InterceptPayload } from "./stores/intercept";
import { usePluginsStore, resolveExecConsent } from "./stores/plugins";
import { useThemeStore } from "./stores/theme";
import { useNotificationsStore } from "./stores/notifications";
import { useSystemStore } from "./stores/system";
import { taskDisplayName } from "./utils/taskName";
import {
  installPluginFiles,
  installPluginZip,
  readAsBase64,
} from "./utils/pluginInstall";

interface Aria2Event {
  type: "connected" | "disconnected" | "aria2" | "ws_connected" | "ws_closed";
  event?: string;
  gid?: string;
}

let unlisten: UnlistenFn | null = null;
let unlistenBreaker: UnlistenFn | null = null;
let unlistenReload: UnlistenFn | null = null;
let unlistenIntercept: UnlistenFn | null = null;
let unlistenPluginTask: UnlistenFn | null = null;

const store = useDownloadsStore();
const interceptStore = useInterceptStore();
const { connection } = storeToRefs(store);
const pluginsStore = usePluginsStore();
const themeStore = useThemeStore();
const notices = useNotificationsStore();
const system = useSystemStore();
const route = useRoute();
const router = useRouter();

const currentTheme = computed(() => (themeStore.isDark ? darkTheme : lightTheme));

/**
 * beta12.1 冷启动宽限：应用启动 15 秒内连接失败只在 UI 显示「连接中...」，
 * 绝不推错误通知；成功连过一次之后再断开才允许通知。
 */
const COLD_START_GRACE_MS = 15_000;
const bootAt = Date.now();
let everConnected = false;

/** 引擎可用：记录「成功连过一次」，这是后续断开通知的唯一前提。 */
function markEngineConnected() {
  everConnected = true;
  connection.value = "connected";
}

/** 引擎不可用：宽限期内保持 connecting；只有当「连过再断」时才推通知。 */
function markEngineOffline(notify: boolean) {
  const withinGrace = !everConnected && Date.now() - bootAt < COLD_START_GRACE_MS;
  connection.value = withinGrace ? "connecting" : "disconnected";
  if (!notify || !everConnected || withinGrace) return;
  notices.push("warn", "引擎连接断开", "正在尝试自动重连，请检查核心服务状态", {
    dedupeKey: "engine-disconnected",
  });
}

const commonFonts = {
  // 优先 Inter（未安装时回退系统无衬线）。
  fontFamily:
    'Inter, "Segoe UI", "Microsoft YaHei", "PingFang SC", "Hiragino Sans GB", system-ui, sans-serif',
  fontFamilyMono:
    '"Cascadia Mono", "Consolas", "SFMono-Regular", ui-monospace, monospace',
};

const darkOverrides: GlobalThemeOverrides = {
  common: {
    ...commonFonts,
    primaryColor: "#2dd4bf",
    primaryColorHover: "#5eead4",
    primaryColorPressed: "#14b8a6",
    primaryColorSuppl: "#2dd4bf",
    bodyColor: "#0d1517",
    cardColor: "#131d1f",
    modalColor: "#131d1f",
    popoverColor: "#131d1f",
    borderColor: "#243336",
    dividerColor: "#243336",
    textColorBase: "#e4efed",
    textColor1: "#e4efed",
    textColor2: "#b3c6c3",
    textColor3: "#7f9391",
  },
};

const lightOverrides: GlobalThemeOverrides = {
  common: {
    ...commonFonts,
    primaryColor: "#14b8a6",
    primaryColorHover: "#0d9c8c",
    primaryColorPressed: "#0b8074",
    primaryColorSuppl: "#14b8a6",
    bodyColor: "#f0f7f7",
    cardColor: "#ffffff",
    modalColor: "#ffffff",
    popoverColor: "#ffffff",
    borderColor: "#e7eeee",
    dividerColor: "#e7eeee",
    textColorBase: "#1f2d2c",
    textColor1: "#1f2d2c",
    textColor2: "#4c5e5d",
    textColor3: "#8a9b9a",
  },
};

const currentOverrides = computed(() =>
  themeStore.isDark ? darkOverrides : lightOverrides,
);

const collapsed = ref(readCollapsed());

/** beta9：下载页由虚拟列表独立滚动，外层容器不再参与滚动。 */
const downloadsPage = computed(() => route.name === "downloads");

/** 设置弹窗（beta7）：不再走路由页面，主界面背景保留，居中弹出。 */
const settingsOpen = ref(false);

function readCollapsed(): boolean {
  try {
    return localStorage.getItem("aria2-harness-sidebar") === "1";
  } catch {
    return false;
  }
}

watch(collapsed, (value) => {
  try {
    localStorage.setItem("aria2-harness-sidebar", value ? "1" : "0");
  } catch {
    // 忽略：折叠状态无法持久化不影响本次会话。
  }
});

watch(
  () => themeStore.isDark,
  (dark) => document.documentElement.classList.toggle("dark", dark),
  { immediate: true },
);

/** 全量初始化 + 启动轮询（挂载时、以及每次 WS 连接成功时调用）。 */
async function bootstrap() {
  // beta8：UI 连上后端后强制全量下发一次本地真实配置（dir / split / 单服并发 /
  // 限速 / 代理…）。覆盖安装与引擎重启都会丢掉 changeGlobalOption 的运行时值，
  // 这一步保证底层引擎行为与设置页显示绝对一致。失败静默，等下次 connected 重试。
  await invoke("sync_engine_config").catch(() => {});
  try {
    await store.initAll();
  } catch {
    // 引擎尚未就绪时静默失败，等 WS connected 事件触发重试
  }
  // 代理 / 下载目录等全局设置由宿主在 WS 连上后从 config.json 重放（见 lib.rs
  // 的 `apply_saved_globals`），前端不再持有或回写这类运行时偏好。
  store.startPolling();
}

function toggleSidebar() {
  collapsed.value = !collapsed.value;
}

function newTask() {
  void router.push("/downloads");
  store.openCompose();
}

/**
 * 通知栏里的任务名：绝不再回退到 GID。
 *
 * 解析中显示「解析文件名中...」，任务已删除 / 取不到信息显示「未知文件」。
 */
function taskLabel(gid: string): string {
  return taskDisplayName(store.tasks.get(gid));
}

/** 去浏览器化：屏蔽右键菜单。 */
function blockContextMenu(e: MouseEvent) {
  e.preventDefault();
}

/** 去浏览器化：拦截 F5 / Ctrl+R / Ctrl+Shift+R 刷新。 */
function blockRefreshShortcuts(e: KeyboardEvent) {
  const refresh =
    e.key === "F5" ||
    ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "r");
  if (refresh) e.preventDefault();
}

/**
 * 强制一次布局：窗口最小化/隐藏到托盘期间，WebView2 会暂停合成；
 * 恢复可见后仅靠 `set_visible(true)` 往往只重绘局部（右侧一条），其余留白。
 * 这里主动派发 resize，让 Vue 组件与 WebView2 重新计算视口并整屏重绘。
 */
function forceRelayout() {
  window.dispatchEvent(new Event("resize"));
}

const appRoot = ref<HTMLElement | null>(null);
let relayoutTimer = 0;

/**
 * 强制重建合成层：改一次 `transform` 会迫使 Chromium 重新分配图层，
 * 能清掉 WebView2 从隐藏态恢复后残留的白底。80ms 后撤销，避免影响布局。
 */
function forceRerender() {
  const el = appRoot.value;
  if (!el) return;
  el.style.transform = "translateZ(0)";
  void el.offsetHeight;
  window.clearTimeout(relayoutTimer);
  relayoutTimer = window.setTimeout(() => {
    el.style.transform = "";
  }, 80);
}

/**
 * 焦点恢复后的**延迟**重排。
 *
 * WebView2 从隐藏/最小化恢复时，合成器醒来晚于 `focus` 事件；立刻派发 resize
 * 会被旧视口吞掉。50ms 后再来一次，配合 Rust 侧跨 tick 的尺寸抖动一起生效。
 */
let focusTimer = 0;

/**
 * 焦点恢复：无视任何节流立刻拉一份全量快照覆盖本地状态，并检查 WS 连接。
 *
 * 后台期间进度被 WebView2 与轮询双重点停，仅恢复轮询会让首帧仍是旧数据；
 * 同时若 WS 已断，切回前台没必要再等最长 30s 的退避。
 */
async function syncOnFocus() {
  await store.refreshNow();
  const online = await invoke<boolean>("get_engine_status").catch(() => false);
  if (online) {
    markEngineConnected();
  } else {
    markEngineOffline(false);
    await invoke<boolean>("reconnect_engine").catch(() => false);
  }
}

function onWindowFocus() {
  // 回到前台立刻恢复轮询（无活跃任务时 store 会在下一 tick 自行停表）。
  resumeBackgroundWork();
  void syncOnFocus();
  window.clearTimeout(focusTimer);
  focusTimer = window.setTimeout(() => {
    forceRelayout();
    forceRerender();
  }, 50);
}

/** 后台隐藏：停掉所有轮询，避免 1s 一次的 IPC 在托盘里继续烧 CPU。 */
function pauseBackgroundWork() {
  store.stopPolling();
  system.stopStatPolling();
}

function resumeBackgroundWork() {
  store.startPolling();
  system.startStatPolling();
}

function onDocumentVisibility() {
  if (document.visibilityState === "visible") {
    resumeBackgroundWork();
    forceRelayout();
    onWindowFocus();
  } else {
    pauseBackgroundWork();
  }
}

/** 宿主（Rust 托盘唤回 / 隐藏）派发的可见性事件：与 document 事件互补。 */
function onHarnessVisibility(event: Event) {
  const hidden = (event as CustomEvent<{ hidden?: boolean }>).detail?.hidden;
  if (hidden === false) {
    resumeBackgroundWork();
    forceRelayout();
    onWindowFocus();
  } else {
    pauseBackgroundWork();
  }
}

// ---------------------------------------------------------------------------
// 拖拽接管（全局）：链接 → 新建弹窗预填；.torrent → 直接入队
// ---------------------------------------------------------------------------

/** 遮罩可见性。用「进入/离开计数」抵消子元素反复触发的 dragenter/dragleave。 */
const dragActive = ref(false);
let dragDepth = 0;

/** 拖入内容是否值得接管（文件 / 链接文本）。 */
function hasDragPayload(e: DragEvent): boolean {
  const types = Array.from(e.dataTransfer?.types ?? []);
  return (
    types.includes("Files") ||
    types.includes("text/uri-list") ||
    types.includes("text/plain")
  );
}

function onDragEnter(e: DragEvent) {
  if (!hasDragPayload(e)) return;
  e.preventDefault();
  dragDepth += 1;
  dragActive.value = true;
}

function onDragOver(e: DragEvent) {
  // 必须无条件 preventDefault：否则 WebView2 会把拖入的文件当成导航目标直接打开。
  e.preventDefault();
  if (e.dataTransfer) e.dataTransfer.dropEffect = "copy";
}

function onDragLeave() {
  // leave 事件的 dataTransfer.types 可能已为空，不能再用 hasDragPayload 过滤，
  // 否则计数永远减不到 0、遮罩会卡死。
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) dragActive.value = false;
}

// ---------------------------------------------------------------------------
// 插件安装（beta12.1）：拖入 .zip 或文件夹，批量结果由公共模块统一提示
// ---------------------------------------------------------------------------

/** 递归读取拖入的文件夹（分批读取，目录项数量没有单次上限）。 */
async function readDirectoryEntries(
  entry: FileSystemDirectoryEntry,
  prefix = "",
): Promise<Array<{ path: string; data: string }>> {
  const reader = entry.createReader();
  const children: FileSystemEntry[] = [];
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
      reader.readEntries(resolve, reject),
    );
    if (batch.length === 0) break;
    children.push(...batch);
  }
  const files: Array<{ path: string; data: string }> = [];
  for (const child of children) {
    const path = `${prefix}${child.name}`;
    if (child.isDirectory) {
      files.push(
        ...(await readDirectoryEntries(child as FileSystemDirectoryEntry, `${path}/`)),
      );
      continue;
    }
    const file = await new Promise<File>((resolve, reject) =>
      (child as FileSystemFileEntry).file(resolve, reject),
    );
    files.push({ path, data: await readAsBase64(file) });
  }
  return files;
}

async function installPluginFolder(
  entry: FileSystemDirectoryEntry,
  source: string,
) {
  try {
    const files = await readDirectoryEntries(entry);
    await installPluginFiles(files, source);
  } catch (error) {
    notices.push("error", "插件安装失败", `${source}：${errText(error)}`);
  }
}

async function addTorrentFile(file: File) {
  try {
    const data = await readAsBase64(file);
    const gid = await invoke<string>("add_torrent", { data, name: file.name });
    // 立即入表 + 启动轮询，不必等 WS 事件（与弹窗添加保持一致）。
    await store.updateOne(gid).catch(() => undefined);
    store.startPolling();
    notices.push("success", "种子已加入下载", file.name);
    void router.push("/downloads");
  } catch (error) {
    notices.push("error", "添加种子失败", `${file.name}：${errText(error)}`);
  }
}

async function onDrop(e: DragEvent) {
  e.preventDefault();
  dragDepth = 0;
  dragActive.value = false;

  // 文件夹只能通过 items 的 entry 拿到（files 里不含目录项），故先处理目录。
  const entries = Array.from(e.dataTransfer?.items ?? [])
    .filter((item) => item.kind === "file")
    .map((item) => item.webkitGetAsEntry?.() ?? null)
    .filter((entry): entry is FileSystemEntry => entry !== null);

  const folder = entries.find((entry) => entry.isDirectory);
  if (folder) {
    await installPluginFolder(folder as FileSystemDirectoryEntry, folder.name);
    return;
  }

  const files = Array.from(e.dataTransfer?.files ?? []);
  // 允许多个 zip 一次拖入（beta12.1 批量安装），逐个交给后端。
  const pluginZips = files.filter((file) => file.name.toLowerCase().endsWith(".zip"));
  if (pluginZips.length > 0) {
    for (const zip of pluginZips) await installPluginZip(zip);
    return;
  }
  const torrent = files.find((file) => file.name.toLowerCase().endsWith(".torrent"));
  if (torrent) {
    await addTorrentFile(torrent);
    return;
  }
  if (files.length > 0 && files[0].size > 0) {
    notices.push(
      "warn",
      "不支持的文件类型",
      `支持 .torrent 种子与 .zip 插件包（或直接拖入插件文件夹），收到：${files[0].name}`,
    );
    return;
  }

  // 文本拖拽：`text/uri-list` 可能包含多行与 `#` 注释，取第一条有效行。
  const raw =
    e.dataTransfer?.getData("text/uri-list") ||
    e.dataTransfer?.getData("text/plain") ||
    "";
  const link =
    raw
      .split(/\r?\n/)
      .map((line) => line.trim())
      .find((line) => line.length > 0 && !line.startsWith("#")) ?? "";

  if (/^(https?|magnet):/i.test(link)) {
    void router.push("/downloads");
    // 预填到「新建任务」弹窗，由用户确认后入队（Referer 由宿主智能匹配）。
    store.openCompose(link);
    return;
  }

  notices.push(
    "warn",
    "无法识别拖入内容",
    "支持 http / https / magnet 链接、.torrent 种子与 .zip 插件包（或插件文件夹）",
  );
}

/**
 * 接收浏览器接管任务：写入 store 并切到下载页，由下载页弹窗自动预填。
 *
 * 不做任何超时判定 —— 宿主那边同样是"存到用户确认或取消为止"。
 */
function acceptIntercept(payload: InterceptPayload | null) {
  if (!payload || !payload.url) return;
  interceptStore.setPending(payload);
  void router.push("/downloads");
}

onMounted(async () => {
  window.addEventListener("contextmenu", blockContextMenu);
  window.addEventListener("keydown", blockRefreshShortcuts);
  // 托盘恢复白屏修复：可见性变化 / 宿主唤回 / 重新获得焦点时强制重排一次。
  document.addEventListener("visibilitychange", onDocumentVisibility);
  window.addEventListener("harness-visibility", onHarnessVisibility);
  window.addEventListener("focus", onWindowFocus);
  // 失焦即休眠：窗口被其他应用遮挡 / 切到后台时停掉全部定时轮询，
  // 重新获得焦点时由 `onWindowFocus` 恢复（两者与 visibilitychange 幂等）。
  window.addEventListener("blur", pauseBackgroundWork);
  // 路由切换 / 窗口尺寸变化时重建合成层，确保 RouterView 内容跟着重绘。
  window.addEventListener("resize", forceRerender);
  // 全局拖拽：捕获阶段监听，避免页面内元素 stopPropagation 导致遮罩不出现。
  window.addEventListener("dragenter", onDragEnter, true);
  window.addEventListener("dragover", onDragOver, true);
  window.addEventListener("dragleave", onDragLeave, true);
  window.addEventListener("drop", onDrop, true);

  system.startStatPolling();

  unlisten = await listen<Aria2Event>("aria2://event", async (ev) => {
    const payload = ev.payload;

    if (payload.type === "connected") {
      // 宿主已用 `aria2.getVersion` 探针确认引擎可用，可直接转绿。
      markEngineConnected();
      await bootstrap();
      return;
    }

    if (payload.type === "ws_connected") {
      // beta9.1：WS 握手成功 ≠ 引擎可用（服务被强杀时回环 TCP 仍可能握手成功）。
      // 必须再经宿主的真实探针确认，超时/失败一律保持断开，绝不提前转绿。
      const online = await invoke<boolean>("get_engine_status").catch(() => false);
      if (!online) {
        markEngineOffline(false);
        return;
      }
      markEngineConnected();
      await bootstrap();
      return;
    }

    if (payload.type === "disconnected" || payload.type === "ws_closed") {
      // beta9.1：固定 dedupeKey —— 断链后每次重连失败都会重复派发 `ws_closed`。
      // beta12.1：冷启动 15 秒宽限内只显示「连接中...」，且从未连上过不通知。
      markEngineOffline(true);
      return;
    }

    if (payload.type === "aria2" && payload.gid) {
      try {
        await store.updateOne(payload.gid);
        const task = store.tasks.get(payload.gid);
        if (task?.status === "error" && task.errorCode) {
          notices.push(
            "error",
            "下载失败",
            task.errorMessage || `错误码 ${task.errorCode}`,
            {
              dedupeKey: `task-error-${payload.gid}-${task.errorCode}`,
              taskName: taskLabel(payload.gid),
            },
          );
        } else if (!task) {
          // 第三方 RPC 推送：事件可能先于本地快照到达，单条查询拿不到就整表刷新兜底。
          await store.refreshNow();
        }
      } catch {
        // 任务可能已被删除，或刚由第三方脚本创建、宿主尚未登记 —— 全量刷新兜底，
        // 保证「外部推送的任务卡片秒级出现」而不是停在旧列表。
        await store.refreshNow();
      } finally {
        // 新任务通常是 active：事件驱动的刷新必须把进度轮询一并拉起。
        store.startPolling();
      }
    }
  });

  unlistenBreaker = await listen<{ pluginId: string; consecutiveFailures: number }>(
    "plugin://circuit-breaker",
    async (ev) => {
      const { pluginId, consecutiveFailures } = ev.payload;
      pluginsStore.notify(
        `⚠️ 插件 ${pluginId} 连续 ${consecutiveFailures} 次异常，已自动禁用（熔断）`,
      );
      notices.push(
        "error",
        "插件已熔断",
        `插件 ${pluginId} 连续 ${consecutiveFailures} 次 Hook 异常，已被自动禁用`,
        { dedupeKey: `breaker-${pluginId}` },
      );
      if (route.path.startsWith("/plugins")) await pluginsStore.load();
    },
  );

  // 浏览器扩展接管：宿主暂存后立即派发；窗口被销毁重建时事件可能丢失，
  // 因此挂载阶段还会主动查一次（见下面的 get_pending_intercept）。
  unlistenIntercept = await listen<InterceptPayload>("app://browser-intercept", (ev) => {
    acceptIntercept(ev.payload);
  });

  const pendingIntercept = await invoke<InterceptPayload | null>("get_pending_intercept").catch(
    () => null,
  );
  if (pendingIntercept?.url) acceptIntercept(pendingIntercept);

  unlistenReload = await listen<{ id: string; status: string; error?: string }>(
    "plugin://reloaded",
    async (ev) => {
      const { id, status, error } = ev.payload;
      pluginsStore.notify(
        status === "failed"
          ? `❌ 插件 ${id} 热重载失败：${error ?? "校验未通过"}（已保留旧版本）`
          : `🔄 插件 ${id} 已热重载（${status}）`,
      );
      // beta12.2：后端热重载只记日志，不再推系统通知（避免安装/更新时通知刷屏）。
      console.info(
        status === "failed"
          ? `[plugin] ${id} 热重载失败：${error ?? "校验未通过"}（已保留旧版本）`
          : `[plugin] ${id} 已重新加载（${status}）`,
      );
      if (route.path.startsWith("/plugins")) await pluginsStore.load();
    },
  );

  // beta10：插件接管的虚拟任务（HLS 等）状态增量。
  unlistenPluginTask = await listen<{ pluginId: string; task: PluginTask }>(
    "plugin://task",
    (ev) => {
      const task = ev.payload.task;
      if (!task) return;
      // beta12.2 首帧兜底：非终态事件说明任务刚启动，插件分片的真实下载仍在引擎里，
      // 立刻拉起轮询让下载页可见（`startPolling` 内部判空，已轮询时幂等）。
      if (task.phase !== "complete" && task.phase !== "error" && task.phase !== "cancelled") {
        store.startPolling();
      }
      const previous = store.pluginTasks.find((item) => item.id === task.id);
      store.upsertPluginTask(task);
      if (previous?.phase === task.phase) return;
      if (task.phase === "complete") {
        notices.push("success", "插件任务完成", `${task.title} 已处理完成`, {
          dedupeKey: `plugin-task-${task.id}`,
        });
      } else if (task.phase === "error") {
        notices.push("error", "插件任务失败", `${task.title}：${task.error || "未知错误"}`, {
          dedupeKey: `plugin-task-${task.id}-error`,
        });
      }
    },
  );

  const online = await invoke<boolean>("get_engine_status").catch(() => false);
  if (online) {
    markEngineConnected();
  } else {
    markEngineOffline(false);
  }

  await bootstrap();
});

onUnmounted(() => {
  window.removeEventListener("contextmenu", blockContextMenu);
  window.removeEventListener("keydown", blockRefreshShortcuts);
  document.removeEventListener("visibilitychange", onDocumentVisibility);
  window.removeEventListener("harness-visibility", onHarnessVisibility);
  window.removeEventListener("focus", onWindowFocus);
  window.removeEventListener("blur", pauseBackgroundWork);
  window.removeEventListener("resize", forceRerender);
  window.clearTimeout(relayoutTimer);
  window.clearTimeout(focusTimer);
  window.removeEventListener("dragenter", onDragEnter, true);
  window.removeEventListener("dragover", onDragOver, true);
  window.removeEventListener("dragleave", onDragLeave, true);
  window.removeEventListener("drop", onDrop, true);
  unlisten?.();
  unlistenBreaker?.();
  unlistenReload?.();
  unlistenIntercept?.();
  unlistenPluginTask?.();
  store.stopPolling();
  system.stopStatPolling();
});
</script>

<template>
  <n-config-provider :theme="currentTheme" :theme-overrides="currentOverrides">
    <!-- 通知 Provider 包在最外层（其他页面仍在用 useNotification）；beta7 的
         UpdateNotifier 自己就是确认弹窗，挂在这里是为了与 App 同生命周期、
         且能拿到 ConfigProvider 的主题上下文。 -->
    <n-notification-provider placement="bottom-right" :max="3">
      <UpdateNotifier />
      <div class="app" ref="appRoot">
      <TopBar :collapsed="collapsed" @toggle="toggleSidebar" @new-task="newTask" />

      <div class="app-body">
        <Sidebar
          :collapsed="collapsed"
          :unread="notices.unreadCount"
          :settings-open="settingsOpen"
          @open-settings="settingsOpen = true"
        />

        <main class="workspace">
          <div class="content-card">
            <div class="page-scroll" :class="{ 'page-scroll--locked': downloadsPage }">
              <RouterView v-slot="{ Component }">
                <Transition name="page" mode="out-in">
                  <component :is="Component" />
                </Transition>
              </RouterView>
            </div>
          </div>
        </main>
      </div>

      <!-- 设置弹窗：常驻渲染，open 控制显示（内容 teleport 到 body）。 -->
      <SettingsView :open="settingsOpen" @close="settingsOpen = false" />

      <!-- 安装安全确认：插件含 exec:local 权限时，装前必须显式信任 -->
      <div
        v-if="pluginsStore.consentTarget"
        class="modal-mask consent-mask"
        @click.self="resolveExecConsent(false)"
      >
        <div class="modal consent-modal" role="alertdialog" aria-modal="true">
          <header class="modal-head">
            <h2 class="consent-title">安全警告</h2>
          </header>
          <p class="modal-body">
            此插件包含外部可执行文件，是否信任并安装？
          </p>
          <p class="consent-detail">{{ pluginsStore.consentTarget }}</p>
          <div class="modal-actions">
            <button class="btn-ghost" @click="resolveExecConsent(false)">取消</button>
            <button class="consent-accept" @click="resolveExecConsent(true)">
              信任并安装
            </button>
          </div>
        </div>
      </div>

      <!-- 拖拽遮罩：覆盖整个窗口，提示可投放的内容类型 -->
      <div v-if="dragActive" class="drop-overlay">
        <div class="drop-frame">
          <AppIcon name="download" :size="38" />
          <p class="drop-title">松开以添加下载任务 / 安装插件</p>
          <p class="drop-hint">
            支持 http / https / magnet 链接、.torrent 种子与 .zip 插件包（或插件文件夹）
          </p>
        </div>
      </div>
      </div>
    </n-notification-provider>
  </n-config-provider>
</template>

<style scoped>
.app {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
  background: var(--bg);
}

.app-body {
  flex: 1;
  min-height: 0;
  display: flex;
  overflow: hidden;
}

.workspace {
  position: relative;
  flex: 1;
  min-width: 0;
  height: 100%;
  padding: 2px 16px 14px 4px;
  display: flex;
  flex-direction: column;
}

.content-card {
  position: relative;
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  background: var(--surface);
  border-radius: var(--r-outer);
  box-shadow: var(--shadow-card);
  overflow: hidden;
}

.page-scroll {
  position: relative;
  z-index: 1;
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overflow-x: hidden;
  /* 顶部不留内边距：sticky 标题栏的布局位 = 视觉位，滚动到顶时内容不会钻到标题背后。 */
  padding: 0 30px 32px;
  scroll-behavior: smooth;
}

/* beta9：下载页锁死外层高度，只让内部虚拟列表滚动，消除第二条滚动条。 */
.page-scroll--locked {
  display: flex;
  flex-direction: column;
  height: 100%;
  overflow: hidden;
}

/* ---- 安装安全确认弹窗 ---- */
/* z-index 高于普通弹窗（1000）与拖拽遮罩（3000）：这是阻断式安全询问。 */
.modal-mask {
  position: fixed;
  inset: 0;
  z-index: 3200;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 20px;
  background: rgba(40, 12, 12, 0.4);
  backdrop-filter: blur(2px);
}

.modal {
  width: min(460px, 100%);
  padding: 20px 22px;
  border: 1px solid color-mix(in srgb, var(--danger) 45%, var(--line));
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

.consent-title {
  margin: 0;
  font-size: 16px;
  font-weight: 700;
  color: var(--danger);
}

.modal-body {
  margin: 10px 0 8px;
  font-size: 13px;
  font-weight: 600;
  line-height: 1.6;
  color: var(--text);
}

.consent-detail {
  margin: 0;
  padding: 9px 12px;
  border-left: 3px solid var(--danger);
  border-radius: 8px;
  background: var(--danger-soft);
  color: var(--danger);
  font-size: 12.5px;
  line-height: 1.6;
  word-break: break-all;
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

/* 主操作是「信任并安装」，用危险色强化风险感知。 */
.consent-accept {
  background: var(--danger);
  border-color: var(--danger);
  color: #ffffff;
  font-weight: 600;
}
.consent-accept:hover:not(:disabled) {
  background: color-mix(in srgb, var(--danger) 82%, #000000);
  border-color: color-mix(in srgb, var(--danger) 82%, #000000);
  color: #ffffff;
}

/* ---- 拖拽遮罩 ---- */
.drop-overlay {
  position: fixed;
  inset: 0;
  z-index: 3000;
  display: grid;
  place-items: center;
  background: color-mix(in srgb, var(--bg) 68%, transparent);
  backdrop-filter: blur(3px);
  pointer-events: none; /* 遮罩不吞 drop 事件，落点仍由捕获阶段处理 */
}

.drop-frame {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  width: min(460px, 76vw);
  padding: 34px 30px 30px;
  border: 2px dashed var(--accent);
  border-radius: var(--r-outer, 12px);
  background: var(--surface);
  box-shadow: var(--shadow-card);
  color: var(--text);
  animation: drop-pulse 1.4s ease-in-out infinite;
}

.drop-title {
  margin: 12px 0 2px;
  font-size: 17px;
  font-weight: 600;
  letter-spacing: 0.02em;
}

.drop-hint {
  margin: 0;
  font-size: 12.5px;
  opacity: 0.72;
}

@keyframes drop-pulse {
  0%,
  100% {
    border-color: var(--accent);
    box-shadow: 0 0 0 0 color-mix(in srgb, var(--accent) 34%, transparent),
      var(--shadow-card);
  }
  50% {
    border-color: var(--accent-hover);
    box-shadow: 0 0 0 9px color-mix(in srgb, var(--accent) 0%, transparent),
      var(--shadow-card);
  }
}
</style>
