<script setup lang="ts">
defineProps<{
  title: string;
  subtitle?: string;
}>();
</script>

<template>
  <header class="page-head">
    <div class="head-left">
      <h1>{{ title }}</h1>
      <p v-if="subtitle" class="sub">{{ subtitle }}</p>
      <slot name="meta" />
    </div>
    <div class="head-actions">
      <slot name="actions" />
    </div>
  </header>
</template>

<style scoped>
.page-head {
  position: sticky;
  top: 0;
  /* 吸顶时必须压住滚动内容（z-50 等价），并让模糊背景铺满 page-scroll 的左右内边距。 */
  z-index: 50;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  /* 负外边距只抵消 page-scroll 的左右 30px 内边距：标题栏横向铺满、纵向留在流内。
     纵向**不能再有负边距** —— 否则吸顶时布局位与视觉位错位，首卡片会被标题压住。 */
  margin: 0 -30px 20px;
  padding: 12px 30px 10px;
  min-height: 40px;
  background: color-mix(in srgb, var(--surface) 82%, transparent);
  backdrop-filter: blur(14px) saturate(160%);
  -webkit-backdrop-filter: blur(14px) saturate(160%);
  border-bottom: 1px solid var(--line);
}

/*
 * 吸顶说明：page-scroll 顶部不再留内边距（padding-top: 0），标题栏的布局位置就是
 * 视觉位置，`top: 0` 吸附时下方内容自然从标题栏底边开始，无需任何缝隙补丁。
 */

.head-left {
  display: flex;
  align-items: center;
  gap: 12px;
  min-width: 0;
}

h1 {
  margin: 0;
  font-size: 22px;
  font-weight: 700;
  letter-spacing: 0.2px;
  color: var(--text);
  /* 标题绝不允许逐字换行：窄窗口下让副标题/操作区换行，标题保持一行。 */
  white-space: nowrap;
  flex: none;
}

.sub {
  margin: 0;
  font-size: 12px;
  color: var(--muted);
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.head-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex: none;
}

/* 窄窗口：操作区整体换到标题下方，避免与标题互相挤压。 */
@media (max-width: 860px) {
  .page-head {
    flex-wrap: wrap;
    align-items: flex-start;
    row-gap: 10px;
  }

  .head-actions {
    flex: 1 1 100%;
    flex-wrap: wrap;
    justify-content: flex-start;
  }
}
</style>
