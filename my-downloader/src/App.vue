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
import { useDownloadsStore } from "./stores/downloads";
import { useInterceptStore, type InterceptPayload } from "./stores/intercept";
import { usePluginsStore } from "./stores/plugins";
import { useThemeStore } from "./stores/theme";
import { useNotificationsStore } from "./stores/notifications";
import { useSystemStore } from "./stores/system";
import { taskDisplayName } from "./utils/taskName";

interface Aria2Event {
  type: "connected" | "disconnected" | "aria2" | "ws_connected" | "ws_closed";
  event?: string;
  gid?: string;
}

let unlisten: UnlistenFn | null = null;
let unlistenBreaker: UnlistenFn | null = null;
let unlistenReload: UnlistenFn | null = null;
let unlistenIntercept: UnlistenFn | null = null;

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
  connection.value = online ? "connected" : "disconnected";
  if (!online) {
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

/** 把 File 读成 base64（分块拼接，避免大种子触发 `String.fromCharCode` 参数上限）。 */
function readAsBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const bytes = new Uint8Array(reader.result as ArrayBuffer);
      const chunk = 0x8000;
      let binary = "";
      for (let offset = 0; offset < bytes.length; offset += chunk) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + chunk));
      }
      resolve(btoa(binary));
    };
    reader.onerror = () => reject(reader.error ?? new Error("读取种子文件失败"));
    reader.readAsArrayBuffer(file);
  });
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

  const files = Array.from(e.dataTransfer?.files ?? []);
  const torrent = files.find((file) => file.name.toLowerCase().endsWith(".torrent"));
  if (torrent) {
    await addTorrentFile(torrent);
    return;
  }
  if (files.length > 0) {
    notices.push(
      "warn",
      "不支持的文件类型",
      `目前只支持 .torrent 种子文件，收到：${files[0].name}`,
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
    "支持 http / https / magnet 链接与 .torrent 种子文件",
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

    if (payload.type === "connected" || payload.type === "ws_connected") {
      connection.value = "connected";
      await bootstrap();
      return;
    }

    if (payload.type === "disconnected" || payload.type === "ws_closed") {
      connection.value = "disconnected";
      notices.push("warn", "引擎连接断开", "正在尝试自动重连，请检查核心服务状态", {
        dedupeKey: `disconnected-${Math.floor(Date.now() / 60_000)}`,
      });
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
        }
      } catch {
        // 任务可能已被删除，忽略
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
      notices.push(
        status === "failed" ? "warn" : "success",
        status === "failed" ? "插件热重载失败" : "插件已热重载",
        status === "failed"
          ? `插件 ${id} 重载失败：${error ?? "校验未通过"}，已保留旧版本`
          : `插件 ${id} 已重新加载（${status}）`,
      );
      if (route.path.startsWith("/plugins")) await pluginsStore.load();
    },
  );

  const online = await invoke<boolean>("get_engine_status").catch(() => false);
  connection.value = online ? "connected" : "connecting";

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
            <div class="page-scroll">
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

      <!-- 拖拽遮罩：覆盖整个窗口，提示可投放的内容类型 -->
      <div v-if="dragActive" class="drop-overlay">
        <div class="drop-frame">
          <AppIcon name="download" :size="38" />
          <p class="drop-title">松开以添加下载任务</p>
          <p class="drop-hint">支持 http / https / magnet 链接与 .torrent 种子文件</p>
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
