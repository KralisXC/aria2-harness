<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { storeToRefs } from "pinia";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import NodeProgressBar from "../components/NodeProgressBar.vue";
import ThreadMatrixCard from "../components/ThreadMatrixCard.vue";
import { useDownloadsStore } from "../stores/downloads";
import { useSystemStore } from "../stores/system";

const downloads = useDownloadsStore();
const { list } = storeToRefs(downloads);
const system = useSystemStore();
const { globalStat, history } = storeToRefs(system);

const downSpeed = computed(() => Number(globalStat.value?.downloadSpeed || 0));
const upSpeed = computed(() => Number(globalStat.value?.uploadSpeed || 0));

/** 单方向速度占当前总吞吐的比例（0~100），无流量时为 0。 */
function trafficShare(bytes: number): number {
  const total = downSpeed.value + upSpeed.value;
  return total > 0 ? (bytes / total) * 100 : 0;
}

/** 活跃任务分列：下载中 / 等待 / 做种（completed >= total 的 active 视为做种）。 */
const counts = computed(() => {
  let active = 0;
  let waiting = 0;
  let seeding = 0;
  for (const task of list.value) {
    if (task.status === "active") {
      const total = Number(task.totalLength || 0);
      const done = Number(task.completedLength || 0);
      if (total > 0 && done >= total) seeding += 1;
      else active += 1;
    } else if (task.status === "waiting") {
      waiting += 1;
    }
  }
  return { active, waiting, seeding };
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

/** 历史库时间戳为 unix 秒。 */
function timestampOf(seconds: number): number {
  return seconds * 1000;
}

function startOfDay(ms: number): number {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

/**
 * 活动热力图：宽度充足时展示完整 52 周，窄窗口按刻度降档到 26 / 13 周。
 *
 * 红线：**任何宽度下都不允许横向滚动** —— 方块用 fr 等分容器并靠 aspect-ratio
 * 保持正方形，容器实在放不下就减少周数，而不是让内容溢出被滑动查看。
 */
const HEAT_FULL_WEEKS = 52;
/** 周数阶梯：容器宽度低于阈值即降档，保证每个方块始终清晰可辨。 */
const HEAT_WEEK_STEPS: { minWidth: number; weeks: number }[] = [
  // 380px 时 52 周仍有 ~4px 方块（可辨认下限），再窄就放不下了。
  { minWidth: 380, weeks: 52 },
  { minWidth: 210, weeks: 26 },
  { minWidth: 0, weeks: 13 },
];

interface HeatCell {
  key: string;
  level: number;
  count: number;
  label: string;
}

/** 当前展示的周数（由容器实测宽度决定，ResizeObserver 驱动）。 */
const heatWeeks = ref(HEAT_FULL_WEEKS);

const heatmap = computed<HeatCell[]>(() => {
  const buckets = new Map<string, number>();
  for (const item of history.value) {
    const ts = timestampOf(item.finishedAt ?? item.updatedAt);
    if (!ts) continue;
    const key = new Date(startOfDay(ts)).toDateString();
    buckets.set(key, (buckets.get(key) ?? 0) + 1);
  }

  const today = startOfDay(Date.now());
  const days = heatWeeks.value * 7;
  const cells: HeatCell[] = [];
  for (let offset = days - 1; offset >= 0; offset -= 1) {
    const date = new Date(today);
    date.setDate(date.getDate() - offset);
    const count = buckets.get(date.toDateString()) ?? 0;
    const level = count === 0 ? 0 : count <= 1 ? 1 : count <= 3 ? 2 : count <= 6 ? 3 : 4;
    cells.push({
      key: date.toISOString(),
      level,
      count,
      label: `${date.getMonth() + 1}月${date.getDate()}日 · ${count} 个任务`,
    });
  }
  return cells;
});

const heatmapTotal = computed(() =>
  heatmap.value.reduce((sum, cell) => sum + cell.count, 0),
);

/** 热力图容器：只测量宽度用于决定周数，本身不滚动。 */
const heatWrap = ref<HTMLElement | null>(null);

let heatObserver: ResizeObserver | null = null;

function resolveWeeks(width: number): number {
  return HEAT_WEEK_STEPS.find((step) => width >= step.minWidth)?.weeks ?? 13;
}

onMounted(() => {
  void system.loadHistory(400);
  if (heatWrap.value) {
    // 首次 observe 会立刻回调一次，无需额外初始化。
    heatObserver = new ResizeObserver((entries) => {
      const width = entries[0]?.contentRect.width ?? 0;
      if (width > 0) heatWeeks.value = resolveWeeks(width);
    });
    heatObserver.observe(heatWrap.value);
  }
});

onBeforeUnmount(() => {
  heatObserver?.disconnect();
  heatObserver = null;
});
</script>

<template>
  <div class="view-dashboard">
    <PageHeader title="仪表盘" subtitle="引擎状态与传输概览" />

    <ThreadMatrixCard />

    <section class="dash-grid">
      <article class="panel card-tasks">
        <header class="panel-head">
          <span class="panel-title">活跃任务</span>
          <span class="panel-tag">{{ counts.active + counts.waiting + counts.seeding }} 个</span>
        </header>
        <div class="task-columns">
          <div class="task-col">
            <b class="num active">{{ counts.active }}</b>
            <span>下载中</span>
          </div>
          <span class="col-split"></span>
          <div class="task-col">
            <b class="num waiting">{{ counts.waiting }}</b>
            <span>等待</span>
          </div>
          <span class="col-split"></span>
          <div class="task-col">
            <b class="num seeding">{{ counts.seeding }}</b>
            <span>做种</span>
          </div>
        </div>
        <div class="limit-row">
          <span class="limit-label"><AppIcon name="gauge" :size="13" /> 限速</span>
          <b class="limit-value">常规模式</b>
          <span class="limit-note">未启用全局限速</span>
        </div>
      </article>

      <article class="stat-card card-speed">
        <div class="stat-top">
          <span class="stat-label">下载</span>
          <AppIcon name="download" :size="16" class="stat-icon down" />
        </div>
        <strong class="stat-value num">{{ speed(downSpeed) }}<em>/s</em></strong>
        <NodeProgressBar
          class="stat-progress"
          :progress="trafficShare(downSpeed)"
          :active="downSpeed > 0"
        />
      </article>

      <article class="stat-card card-speed">
        <div class="stat-top">
          <span class="stat-label">上传</span>
          <AppIcon name="upload" :size="16" class="stat-icon up" />
        </div>
        <strong class="stat-value num">{{ speed(upSpeed) }}<em>/s</em></strong>
        <NodeProgressBar
          class="stat-progress"
          :progress="trafficShare(upSpeed)"
          :active="upSpeed > 0"
        />
      </article>

      <section class="panel heat-panel card-heat">
      <header class="panel-head">
        <span class="panel-title">活动热力图</span>
        <span class="panel-tag">近 {{ heatWeeks }} 周 · {{ heatmapTotal }} 个任务</span>
      </header>
      <div ref="heatWrap" class="heatmap-scroll">
        <div
          class="heatmap"
          :style="{ gridTemplateColumns: `repeat(${heatWeeks}, minmax(0, 1fr))` }"
        >
          <div
            v-for="cell in heatmap"
            :key="cell.key"
            class="heat-cell"
            :class="`lv-${cell.level}`"
            :title="cell.label"
          ></div>
        </div>
      </div>
      <div class="heat-legend">
        <span>少</span>
        <i class="heat-cell lv-0"></i>
        <i class="heat-cell lv-1"></i>
        <i class="heat-cell lv-2"></i>
        <i class="heat-cell lv-3"></i>
        <i class="heat-cell lv-4"></i>
        <span>多</span>
      </div>
      </section>
    </section>
  </div>
</template>

<style scoped>
/* 活动任务占 2 列、下载/上传各 1 列、热力图整行，杜绝卡片间大片空白。 */
.dash-grid {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 14px;
}

.card-tasks {
  grid-column: span 2;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.card-speed {
  grid-column: span 1;
  min-width: 0;
}

.card-heat {
  grid-column: 1 / -1;
}

.stat-card {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-height: 118px;
  padding: 16px 18px 18px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
  /* 底部进度条贴边铺满，圆角裁切交给卡片自身。 */
  overflow: hidden;
}

.stat-top {
  display: flex;
  align-items: center;
  justify-content: space-between;
}

.stat-label {
  font-size: 11px;
  letter-spacing: 1.4px;
  color: var(--muted);
  text-transform: uppercase;
}

.stat-icon {
  color: var(--muted);
}
.stat-icon.down {
  color: var(--accent);
}
.stat-icon.up {
  color: var(--purple);
}

.stat-value {
  margin-top: auto;
  font-size: 23px;
  font-weight: 700;
  letter-spacing: 0.2px;
  color: var(--text);
}
.stat-value em {
  margin-left: 2px;
  font-size: 12px;
  font-style: normal;
  font-weight: 500;
  color: var(--muted);
}

/* 底部节点分段进度条（下载 / 上传吞吐占比）。
   宽度铺满卡片、贴住底边；圆角归零，由 .stat-card 的 overflow:hidden 裁切。 */
.stat-progress {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 0;
  width: 100%;
}

.stat-progress :deep(.np-smooth),
.stat-progress :deep(.np-smooth-fill) {
  border-radius: 0;
}

.limit-row {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-top: 16px;
  padding-top: 10px;
  border-top: 1px solid var(--line);
}

.limit-label {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  letter-spacing: 1.2px;
  color: var(--muted);
}

.limit-value {
  font-size: 12.5px;
  font-weight: 650;
  color: var(--text);
}

.limit-note {
  margin-left: auto;
  font-size: 11px;
  color: var(--muted);
}

.heat-panel {
  margin-top: 0;
}

.panel {
  padding: 16px 18px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
}

.panel-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-bottom: 14px;
}

.panel-title {
  font-size: 13px;
  font-weight: 650;
  color: var(--text);
}

.panel-tag {
  font-size: 11px;
  color: var(--muted);
}

.task-columns {
  display: flex;
  align-items: center;
  gap: 8px;
}

.task-col {
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding-left: 4px;
}
.task-col b {
  font-size: 26px;
  line-height: 1.1;
}
.task-col b.active {
  color: var(--accent);
}
.task-col b.waiting {
  color: var(--warn);
}
.task-col b.seeding {
  color: var(--purple);
}
.task-col span {
  font-size: 11.5px;
  color: var(--muted);
}

.col-split {
  width: 1px;
  height: 34px;
  background: var(--line);
}

.heatmap-scroll {
  width: 100%;
  /* 红线：绝不横向滚动。放不下时由周数降档 + 方块收缩解决，而不是滑动查看。
     clip 连程序化滚动一并禁止；老内核不认识 clip 时退回 hidden。 */
  overflow: hidden;
  overflow: clip;
}

.heatmap {
  display: grid;
  grid-auto-flow: column;
  /* 7 行等分；列数与周数一致（模板内联），列宽用 fr 等分容器。 */
  grid-template-rows: repeat(7, auto);
  gap: 3px;
  width: 100%;
}

.heat-cell {
  width: 100%;
  /* 正方形由宽高比保证；容器变窄时整块等比缩小，绝不撑破卡片。 */
  aspect-ratio: 1 / 1;
  border-radius: clamp(1px, 20%, 3px);
  background: var(--surface-3);
  display: block;
}
.heat-cell.lv-1 {
  background: color-mix(in srgb, var(--accent) 24%, var(--surface-3));
}
.heat-cell.lv-2 {
  background: color-mix(in srgb, var(--accent) 45%, var(--surface-3));
}
.heat-cell.lv-3 {
  background: color-mix(in srgb, var(--accent) 70%, var(--surface-3));
}
.heat-cell.lv-4 {
  background: var(--accent);
}

.heat-legend {
  display: flex;
  align-items: center;
  justify-content: flex-end;
  gap: 5px;
  margin-top: 12px;
  font-size: 11px;
  color: var(--muted);
}

/* 图例里的色块保持固定小尺寸（不参与热力图的拉伸规则）。 */
.heat-legend .heat-cell {
  width: 11px;
  height: 11px;
  aspect-ratio: auto;
  flex: none;
  border-radius: 3px;
}

@media (max-width: 1200px) {
  .dash-grid {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }
  .card-heat {
    grid-column: 1 / -1;
  }
}

/* 小窗（<800px）：全部单列堆叠，不产生内部横向滚动。 */
@media (max-width: 800px) {
  .dash-grid {
    grid-template-columns: minmax(0, 1fr);
  }
  .card-tasks,
  .card-speed,
  .card-heat {
    grid-column: 1 / -1;
  }
}
</style>
