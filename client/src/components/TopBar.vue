<script setup lang="ts">
import { computed } from "vue";
import { storeToRefs } from "pinia";
import { invoke } from "../ipc";
import { useDownloadsStore } from "../stores/downloads";
import { useSystemStore } from "../stores/system";
import AppIcon from "./AppIcon.vue";

const props = defineProps<{ collapsed: boolean }>();

const emit = defineEmits<{
  (e: "toggle"): void;
  (e: "new-task"): void;
}>();

const downloads = useDownloadsStore();
const { list, connection } = storeToRefs(downloads);
const system = useSystemStore();
const { globalStat } = storeToRefs(system);

const tip = computed(() => (props.collapsed ? "展开侧栏" : "折叠侧栏"));

/** 优先用引擎全局统计；未取到时用任务列表求和兜底。 */
const downloadSpeed = computed(() => {
  const stat = globalStat.value;
  if (stat) return Number(stat.downloadSpeed || 0);
  return list.value.reduce((sum, t) => sum + Number(t.downloadSpeed || 0), 0);
});

const uploadSpeed = computed(() => Number(globalStat.value?.uploadSpeed || 0));

const statusText = computed(() => {
  if (connection.value === "connected") return "运行正常";
  if (connection.value === "disconnected") return "未连接";
  return "连接中";
});

/** NAT 状态由引擎连通性推导：引擎在跑才谈得上 NAT 映射。 */
const natReady = computed(() => connection.value === "connected");

/** 悬浮详情：NAT / 引擎 / 端口合并展示，避免顶栏堆两个状态块。 */
const statusDetail = computed(() => {
  const nat = natReady.value ? "NAT 已连接" : "NAT 未连接";
  return `${statusText.value} · ${nat} · 端口 6800`;
});

function speed(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(1)} ${units[index]}`;
}

/**
 * 全顶部拖拽：整条 TopBar 都是拖拽区（除 `.no-drag` 的按钮）。
 * WebView2 不消费 `-webkit-app-region`，由 mousedown 调原生 `drag_window` 兜底；
 * 320ms 内的第二次按下视为双击 → 切换最大化。
 */
let lastPress = 0;
function onDragMouseDown(e: MouseEvent) {
  if (e.button !== 0) return;
  const target = e.target as HTMLElement | null;
  if (target?.closest(".no-drag")) return;

  const now = Date.now();
  if (now - lastPress < 320) {
    lastPress = 0;
    void invoke("window-maximize").catch(() => undefined);
    return;
  }
  lastPress = now;
  void invoke("window-drag").catch(() => undefined);
}

function control(cmd: "window-minimize" | "window-maximize" | "window-close") {
  if (cmd === "window-close") {
    fadeAndClose();
    return;
  }
  void invoke(cmd).catch(() => undefined);
}

/**
 * 关闭窗口：先把页面淡出再发 IPC。
 *
 * 直接销毁时 WebView2 会提前卸载，露出系统白底与粗边框；150ms 的淡出
 * 让"消失"发生在纯色背景上，Rust 侧再隐藏窗口 + 置黑底色兜底。
 */
const CLOSE_FADE_MS = 150;

function fadeAndClose() {
  const app = document.getElementById("app");
  if (!app) {
    void invoke("window-close").catch(() => undefined);
    return;
  }
  app.style.transition = `opacity ${CLOSE_FADE_MS}ms ease-out`;
  app.style.opacity = "0";
  window.setTimeout(() => {
    void invoke("window-close").catch(() => undefined);
  }, CLOSE_FADE_MS + 10);
}
</script>

<template>
  <header class="top-bar" @mousedown="onDragMouseDown">
    <div class="brand">
      <div class="brand-mark no-drag" title="Aria2 Harness" @mousedown.stop>
        A2
      </div>
      <div v-show="!collapsed" class="brand-text">
        <b>Aria2</b>
        <span>Harness</span>
      </div>
    </div>

    <button class="bar-btn no-drag" :title="tip" @mousedown.stop @click="emit('toggle')">
      <AppIcon name="chevron" :size="16" class="chev" :class="{ flipped: collapsed }" />
    </button>

    <button class="bar-btn add no-drag" title="新建任务" @mousedown.stop @click="emit('new-task')">
      <AppIcon name="plus" :size="17" />
    </button>

    <!-- 拖拽空白区 + 轻量状态胶囊（原底部悬浮栏迁移至此，不再遮挡内容） -->
    <div class="drag-zone">
      <div class="stat-pills">
        <span class="pill speed" title="下载速度">
          <AppIcon name="download" :size="13" class="down" />
          <b>{{ speed(downloadSpeed) }}</b>
          <span class="unit">/s</span>
        </span>
        <span class="pill speed" title="上传速度">
          <AppIcon name="upload" :size="13" class="up" />
          <b>{{ speed(uploadSpeed) }}</b>
          <span class="unit">/s</span>
        </span>
        <!-- 单一动态状态胶囊：NAT + 引擎合并，悬浮显示详情（P2 合并）。 -->
        <span class="pill status" :class="connection" :title="statusDetail">
          <span class="dot" :class="connection"></span>
          <span class="pill-text">{{ statusText }}</span>
        </span>
      </div>
    </div>

    <div class="win-controls no-drag">
      <button class="win-btn" title="最小化" @click="control('window-minimize')">
        <AppIcon name="minimize" :size="14" />
      </button>
      <button class="win-btn" title="最大化 / 还原" @click="control('window-maximize')">
        <AppIcon name="maximize" :size="12" />
      </button>
      <button class="win-btn close" title="关闭" @click="control('window-close')">
        <AppIcon name="close" :size="14" />
      </button>
    </div>
  </header>
</template>

<style scoped>
.top-bar {
  flex: none;
  display: flex;
  align-items: center;
  gap: 8px;
  height: 56px;
  padding: 0 10px 0 16px;
  background: var(--bg);
  -webkit-app-region: drag;
}

.no-drag {
  -webkit-app-region: no-drag;
}

.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding-right: 4px;
}

.brand-mark {
  flex: none;
  width: 34px;
  height: 34px;
  display: grid;
  place-items: center;
  border-radius: 11px;
  background: linear-gradient(135deg, var(--accent), #3ddc97);
  color: #fff;
  font-size: 12.5px;
  font-weight: 800;
  letter-spacing: 0.3px;
  box-shadow: 0 4px 12px color-mix(in srgb, var(--accent) 30%, transparent);
}

.brand-text {
  display: flex;
  flex-direction: column;
  min-width: 0;
  line-height: 1.15;
}
.brand-text b {
  font-size: 14px;
  letter-spacing: 0.2px;
  color: var(--text);
}
.brand-text span {
  font-size: 10px;
  letter-spacing: 1.6px;
  color: var(--muted);
  text-transform: uppercase;
}

.bar-btn {
  flex: none;
  width: 32px;
  height: 32px;
  padding: 0;
  display: grid;
  place-items: center;
  border: 0;
  border-radius: 9px;
  background: transparent;
  color: var(--muted);
}
.bar-btn:hover {
  background: var(--surface-3);
  color: var(--text);
}
.bar-btn.add {
  background: var(--surface);
  border: 1px solid var(--line);
  color: var(--accent);
  box-shadow: var(--shadow-card);
}
.bar-btn.add:hover {
  background: var(--accent-soft);
  border-color: color-mix(in srgb, var(--accent) 35%, var(--line));
}

.chev {
  transition: transform 0.22s ease;
}
.chev.flipped {
  transform: rotate(180deg);
}

.drag-zone {
  flex: 1;
  min-width: 24px;
  align-self: stretch;
  display: flex;
  align-items: center;
  justify-content: flex-end;
  padding-right: 8px;
  -webkit-app-region: drag;
}

/* ---- 状态胶囊：极轻的灰底细条，不争抢视觉 ---- */
.stat-pills {
  display: flex;
  align-items: center;
  gap: 6px;
  white-space: nowrap;
}

.pill {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 24px;
  padding: 0 9px;
  border-radius: 999px;
  background: color-mix(in srgb, var(--surface) 70%, transparent);
  font-size: 11px;
  color: var(--text-2);
}

.pill b {
  font-size: 11.5px;
  font-weight: 650;
  color: var(--text);
}

.pill .unit {
  font-size: 9.5px;
  color: var(--muted);
}

.pill .down {
  color: var(--accent);
}
.pill .up {
  color: var(--purple);
}

.pill.nat {
  color: var(--muted);
}

.pill.status {
  color: var(--text-2);
}

.dot {
  width: 6px;
  height: 6px;
  border-radius: 999px;
  background: var(--muted);
}
.dot.connected {
  background: var(--success);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--success) 16%, transparent);
}
.dot.disconnected {
  background: var(--danger);
}
.dot.pending,
.dot.connecting {
  background: var(--warn);
  animation: pulse 1.4s ease-in-out infinite;
}
.dot.nat {
  background: var(--line-strong);
}
.dot.nat.connected {
  background: var(--success);
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
  }
}

@media (max-width: 1180px) {
  .pill.speed:nth-child(2) {
    display: none;
  }
}

.win-controls {
  flex: none;
  display: flex;
  align-items: center;
  gap: 2px;
  padding-left: 6px;
}

.win-btn {
  width: 38px;
  height: 30px;
  padding: 0;
  display: grid;
  place-items: center;
  border: 0;
  border-radius: 8px;
  background: transparent;
  color: var(--muted);
}
.win-btn:hover {
  background: var(--surface-3);
  color: var(--text);
}
.win-btn.close:hover {
  background: #e5484d;
  color: #ffffff;
  border-color: transparent;
}
</style>
