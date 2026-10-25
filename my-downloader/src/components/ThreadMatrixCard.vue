<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { storeToRefs } from "pinia";
import { invoke } from "../ipc";
import { useDownloadsStore } from "../stores/downloads";
import AppIcon from "./AppIcon.vue";

/**
 * 线程速度热力矩阵（ThreadMatrixCard）。
 *
 * 数据：
 * - 格子数量 = 用户在设置里配置的**单服务器最大连接数**
 *   （宿主 `config.json` 的 `max_connections_per_server`，与分片数 `split` 解耦）。
 *   读取来源必须是 `get_host_config`：`max-connection-per-server` 不是 aria2 的
 *   可运行时变更全局选项，`get_global_option` 读不到用户保存值。
 * - 活跃格子数 = 当前活跃任务的 `connections` 之和（无活跃任务则全暗）；
 * - aria2 不暴露「单连接速度」，故按总速度在各活跃槽位间做相对分配
 *   （确定性伪随机 + 秒级慢漂移），只用于表达线程负载的相对高低。
 *
 * 视觉：纯灰阶四级 + 活跃格子快速闪烁，不含任何青色。
 */

/** 读取失败时的兜底：单服连接数默认 16（保守值，避免 CDN 反爬）。 */
const DEFAULT_THREADS = 16;
/** 方块数量硬上限（与解封后的引擎一致）。 */
const MAX_THREADS = 128;
/** 负载重算间隔：1s，与任务轮询同频。 */
const TICK_MS = 1000;
/** 全局配置轮询间隔：2s，保证设置页保存后矩阵及时跟随。 */
const CONFIG_POLL_MS = 2000;
/** 列表快照节流：WS 进度事件远比 1s 频繁，矩阵只按 500ms 采样重算。 */
const SNAPSHOT_THROTTLE_MS = 500;

const store = useDownloadsStore();
const { list } = storeToRefs(store);

/** 顶部全局矩阵默认折叠：展开才渲染网格并启动计时器。 */
const collapsed = ref(true);

function toggleCollapsed() {
  collapsed.value = !collapsed.value;
}

const threadCount = ref(DEFAULT_THREADS);
const tick = ref(0);

let timer = 0;
let pollTimer = 0;
let snapshotTimer = 0;
/** 防止 2s 轮询与上一次尚未返回的 RPC 叠罗汉。 */
let loading = false;

/**
 * 节流后的任务快照。
 *
 * 直接对 `list` 建 computed 会在每个 WS 事件上重算整块矩阵；改由 500ms 尾随采样
 * 驱动，既保持视觉跟手，又把重绘次数压到每秒最多两次。
 */
const listSnapshot = ref(list.value);

watch(
  list,
  () => {
    if (snapshotTimer !== 0) return;
    snapshotTimer = window.setTimeout(() => {
      snapshotTimer = 0;
      listSnapshot.value = list.value;
    }, SNAPSHOT_THROTTLE_MS);
  },
  { deep: true },
);

/** 解析单服务器最大连接数：只认宿主配置里的显式值。 */
function parseThreads(config: { max_connections_per_server?: number }): number {
  const parsed = Number(config.max_connections_per_server);
  if (Number.isFinite(parsed) && parsed > 0) {
    return Math.min(MAX_THREADS, Math.round(parsed));
  }
  return DEFAULT_THREADS;
}

/** 实时拉取宿主配置；不做任何缓存，设置页保存后 2s 内必然同步。 */
async function loadConcurrency() {
  if (loading) return;
  loading = true;
  try {
    const config = await invoke<{ max_connections_per_server?: number }>("get_host_config");
    threadCount.value = parseThreads(config);
  } catch {
    // 引擎未就绪时保留上一次的值，下一轮再试。
  } finally {
    loading = false;
  }
}

/** 当前占用的并发槽位：活跃任务连接数之和，封顶到配置值。 */
const activeSlots = computed(() => {
  let total = 0;
  for (const task of listSnapshot.value) {
    if (task.status !== "active") continue;
    if (Number(task.downloadSpeed || 0) <= 0) continue;
    total += Number(task.connections || 0);
  }
  return Math.max(0, Math.min(threadCount.value, total));
});

/** 每个格子的相对负载 0~1；未占用槽位恒为 0。 */
const weights = computed<number[]>(() => {
  const slots = threadCount.value;
  const active = activeSlots.value;
  const phase = tick.value * 0.45;
  const out = new Array<number>(slots).fill(0);
  if (active === 0) return out;

  let max = 1e-6;
  for (let i = 0; i < active; i += 1) {
    // 每槽固定的相位差 + 秒级慢漂移，避免整排同步呼吸。
    const drift = Math.sin(i * 0.6) * 1.8;
    const w = 0.25 + 0.75 * Math.abs(Math.sin(i * 1.7 + phase + drift));
    out[i] = w;
    if (w > max) max = w;
  }
  // 归一化：当前最快线程为 100%，其余按比例落到暗灰 / 中灰。
  for (let i = 0; i < active; i += 1) out[i] /= max;
  return out;
});

/** 四级灰阶映射。 */
function level(ratio: number): string {
  if (ratio <= 0) return "idle";
  if (ratio < 0.5) return "slow";
  if (ratio < 0.8) return "mid";
  return "fast";
}

/** 固定 16 列机架：64 → 4 行，128 → 8 行。 */
const COLUMNS = 16;
const GAP = 4;

const columns = computed(() => Math.min(COLUMNS, threadCount.value));

/**
 * 不足 16 个时给网格限宽并整体居中，保证格子尺寸与满行时完全一致：
 * 16 列总宽 W → 单格宽 = (W - 15×gap) / 16，n 列宽 = n × 单格宽 + (n-1)×gap。
 */
const gridStyle = computed(() => {
  const count = columns.value;
  const style: Record<string, string> = {
    gridTemplateColumns: `repeat(${count}, minmax(0, 1fr))`,
  };
  if (threadCount.value < COLUMNS) {
    style.maxWidth = `calc((100% - ${(COLUMNS - 1) * GAP}px) / ${COLUMNS} * ${count} + ${(count - 1) * GAP}px)`;
  }
  return style;
});

/** 是否有活跃槽位：无活跃任务时矩阵恒为全暗，刷新毫无意义。 */
const running = computed(() => activeSlots.value > 0);

/** 界面是否真的在用户眼前：不可见 / 失焦时不跑任何刷新（后台零 DOM 更新）。 */
function uiVisible(): boolean {
  return document.visibilityState === "visible" && document.hasFocus();
}

function startTicking() {
  if (timer === 0) {
    timer = window.setInterval(() => {
      tick.value += 1;
    }, TICK_MS);
  }
  if (pollTimer === 0) {
    void loadConcurrency();
    pollTimer = window.setInterval(() => {
      void loadConcurrency();
    }, CONFIG_POLL_MS);
  }
}

function stopTicking() {
  if (timer !== 0) {
    window.clearInterval(timer);
    timer = 0;
  }
  if (pollTimer !== 0) {
    window.clearInterval(pollTimer);
    pollTimer = 0;
  }
}

/** 统一入口：只有「展开 且 有活跃任务 且 界面可见」时才跑 1s tick + 2s 配置轮询。 */
function syncTicking() {
  if (!collapsed.value && running.value && uiVisible()) startTicking();
  else stopTicking();
}

watch([running, collapsed], syncTicking);

onMounted(() => {
  // 首帧必须读一次真实配置值（之后仅在活跃期轮询，设置页保存值会在恢复时同步）。
  void loadConcurrency();
  document.addEventListener("visibilitychange", syncTicking);
  window.addEventListener("focus", syncTicking);
  window.addEventListener("blur", syncTicking);
  syncTicking();
});

onBeforeUnmount(() => {
  document.removeEventListener("visibilitychange", syncTicking);
  window.removeEventListener("focus", syncTicking);
  window.removeEventListener("blur", syncTicking);
  if (snapshotTimer !== 0) {
    window.clearTimeout(snapshotTimer);
    snapshotTimer = 0;
  }
  stopTicking();
});
</script>

<template>
  <section class="thread-matrix panel" :class="{ collapsed }" aria-label="线程速度矩阵">
    <button class="tm-head" type="button" :title="collapsed ? '展开线程矩阵' : '收起线程矩阵'" @click="toggleCollapsed">
      <span class="tm-label">并发线程矩阵</span>
      <span class="tm-side">
        <span class="tm-limit">单服上限 {{ threadCount }} · 活跃 {{ activeSlots }}</span>
        <AppIcon name="chevron" :size="14" class="tm-chev" :class="{ flipped: collapsed }" />
      </span>
    </button>
    <div v-if="!collapsed" class="tm-grid-wrap">
      <div class="tm-grid" :style="gridStyle">
        <span
          v-for="(ratio, index) in weights"
          :key="index"
          class="tm-cell"
          :class="level(ratio)"
          :style="{ animationDelay: `${-((index % 8) * 0.09)}s` }"
        />
      </div>
    </div>
  </section>
</template>

<style scoped>
.thread-matrix {
  margin-bottom: 14px;
  padding: 12px 14px 14px;
}

.tm-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  width: 100%;
  margin-bottom: 10px;
  padding: 0;
  border: 0;
  background: transparent;
  color: inherit;
  font: inherit;
  cursor: pointer;
  text-align: left;
}

.thread-matrix.collapsed .tm-head {
  margin-bottom: 0;
}

.tm-side {
  display: inline-flex;
  align-items: center;
  gap: 8px;
}

.tm-chev {
  flex: none;
  color: var(--muted);
  transition: transform 0.18s ease;
}

.tm-chev.flipped {
  transform: rotate(-90deg);
}

/* 极小灰字：说明"格子数 = 单服上限"，避免用户误以为线程没跑满。 */
.tm-limit {
  font-size: 10px;
  line-height: 1;
  color: var(--muted);
  opacity: 0.72;
  white-space: nowrap;
  font-variant-numeric: tabular-nums;
}

.tm-label {
  font-family: inherit;
  font-size: 12px;
  font-weight: 600;
  line-height: 1;
  letter-spacing: 0.02em;
  color: var(--muted);
  white-space: nowrap;
}

/* 外层居中：不足 16 个时整个机架块居中，不靠左。 */
.tm-grid-wrap {
  display: flex;
  justify-content: center;
  width: 100%;
}

/*
 * 固定 16 列车架网格（列数由内联 gridTemplateColumns 注入）：
 * 64 → 4 行，128 → 8 行；不足 16 个由内联 max-width + 外层 flex 居中。
 */
.tm-grid {
  display: grid;
  gap: 4px;
  width: 100%;
  align-items: start;
}

.tm-cell {
  width: 100%;
  /* 略扁平的服务器刀片比例，宽度越大也不会膨胀成巨型方块。 */
  aspect-ratio: 3 / 2;
  max-height: 40px;
  border-radius: 0;
  background: #111619;
  transition: background-color 0.24s steps(3, end);
}

.tm-cell.slow {
  background: #4a4a4a;
}

.tm-cell.mid {
  background: #999999;
}

.tm-cell.fast {
  background: #ffffff;
}

/* 有速度的格子快速呼吸；负延迟让各列错峰，避免整块同步闪。
 * 下限保留 0.6 不透明度：闪烁不能把满速纯白「闪成中灰」，否则灰阶语义被破坏。 */
.tm-cell.slow,
.tm-cell.mid,
.tm-cell.fast {
  animation: tm-flicker 0.72s steps(2, end) infinite alternate;
}

@keyframes tm-flicker {
  from {
    opacity: 1;
  }
  to {
    opacity: 0.6;
  }
}

/* ------------------------------------------------------------------
 * 亮色常规主题：灰阶反转（白底上用深色表达负载），并压低闪烁幅度。
 * 暗色常规主题继续沿用上面的深色底灰阶。
 * ------------------------------------------------------------------ */
html:not(.dark) .tm-cell {
  background: #e0e0e0;
}

html:not(.dark) .tm-cell.slow {
  background: #c6c6c6;
}

html:not(.dark) .tm-cell.mid {
  background: #8f8f8f;
}

html:not(.dark) .tm-cell.fast {
  background: #1a1a1a;
}

html:not(.dark) .tm-cell.slow,
html:not(.dark) .tm-cell.mid,
html:not(.dark) .tm-cell.fast {
  animation: tm-flicker-light 1.1s ease-in-out infinite alternate;
}

@keyframes tm-flicker-light {
  from {
    opacity: 1;
  }
  to {
    opacity: 0.78;
  }
}
</style>
