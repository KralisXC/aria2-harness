<script setup lang="ts">
import { computed } from "vue";

/**
 * 任务进度条：有分片数据时渲染 IDM 风格多段色块，否则退化为单条平滑进度。
 */
const props = withDefaults(
  defineProps<{
    /** 0~100，越界自动收敛。 */
    progress?: number;
    /** 传输中：已完成虚线流动。 */
    active?: boolean;
    /** 语义色：ok 青蓝 / warn 琥珀 / danger 红 / idle 灰。 */
    tone?: "ok" | "warn" | "danger" | "idle";
    /**
     * 分片百分比数组（每项 0~100）。有数据且多于一段时渲染 IDM 风格多段色块，
     * 无数据 / 单段则退化为普通平滑进度条。
     */
    pieces?: number[];
  }>(),
  { progress: 0, active: false, tone: "ok", pieces: () => [] },
);

const value = computed(() => Math.min(100, Math.max(0, Number(props.progress) || 0)));

/** 多段渲染的段数上限：超出按组取平均，避免几百个 DOM 节点拖慢滚动。 */
const MAX_SEGMENTS = 48;

function clampPercent(raw: number): number {
  return Math.min(100, Math.max(0, Math.round(Number(raw) || 0)));
}

/** 展示用的分段百分比；不足两段视为"无切片数据"。 */
const segments = computed<number[]>(() => {
  const raw = props.pieces ?? [];
  if (raw.length < 2) return [];
  if (raw.length <= MAX_SEGMENTS) return raw.map(clampPercent);
  const group = Math.ceil(raw.length / MAX_SEGMENTS);
  const out: number[] = [];
  for (let i = 0; i < raw.length; i += group) {
    const slice = raw.slice(i, i + group);
    out.push(clampPercent(slice.reduce((sum, v) => sum + clampPercent(v), 0) / slice.length));
  }
  return out;
});

const multi = computed(() => segments.value.length >= 2);
</script>

<template>
  <div
    class="node-progress"
    :class="[`tone-${tone}`, { active }]"
    role="progressbar"
    :aria-valuenow="Math.round(value)"
    aria-valuemin="0"
    aria-valuemax="100"
  >
    <span class="np-smooth" :class="{ 'np-multi': multi }" aria-hidden="true">
      <template v-if="multi">
        <!-- IDM 风格：每个分片一段色块，按自身完成度从左向右填充。 -->
        <span v-for="(piece, index) in segments" :key="index" class="np-seg">
          <span class="np-seg-fill" :style="{ width: `${piece}%` }" />
        </span>
      </template>
      <span v-else class="np-smooth-fill" :style="{ width: `${value}%` }" />
    </span>
  </div>
</template>

<style scoped>
.node-progress {
  --np-accent: var(--accent);
  --np-line: var(--line);
  display: flex;
  align-items: center;
  width: 100%;
  height: 10px;
}

.tone-warn {
  --np-accent: var(--warn);
}

.tone-danger {
  --np-accent: var(--danger);
}

.tone-idle {
  --np-accent: var(--line-strong);
}

.np-smooth {
  display: flex;
  position: relative;
  width: 100%;
  height: 6px;
  border-radius: 999px;
  background: var(--surface-3);
  overflow: hidden;
}

/* IDM 多段模式：分片等宽排列，段内按各自完成度填充。 */
.np-smooth.np-multi {
  border-radius: 2px;
  gap: 1px;
}

.np-seg {
  position: relative;
  flex: 1 1 0;
  min-width: 0;
  height: 100%;
  overflow: hidden;
  background: color-mix(in srgb, var(--line) 35%, transparent);
}

.np-seg-fill {
  display: block;
  height: 100%;
  background: var(--accent);
  transition: width 0.3s ease;
}

.tone-warn .np-seg-fill {
  background: var(--warn);
}

.tone-danger .np-seg-fill {
  background: var(--danger);
}

.tone-idle .np-seg-fill {
  background: var(--line-strong);
}

.np-smooth-fill {
  display: block;
  height: 100%;
  border-radius: inherit;
  background: linear-gradient(
    90deg,
    color-mix(in srgb, var(--accent) 45%, #ffffff),
    var(--accent)
  );
  transition: width 0.3s ease;
}

</style>
