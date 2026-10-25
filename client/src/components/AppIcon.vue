<script setup lang="ts">
/**
 * 轻量线性图标集（Feather/Lucide 风格）。
 *
 * 不引入额外图标依赖：全部为静态 SVG，随组件 tree-shake。
 */
withDefaults(
  defineProps<{
    name: string;
    size?: number;
    strokeWidth?: number;
  }>(),
  { size: 18, strokeWidth: 1.8 },
);
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    :stroke-width="strokeWidth"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <template v-if="name === 'dashboard'">
      <rect x="3" y="3" width="7" height="7" rx="2" />
      <rect x="14" y="3" width="7" height="7" rx="2" />
      <rect x="3" y="14" width="7" height="7" rx="2" />
      <rect x="14" y="14" width="7" height="7" rx="2" />
    </template>

    <template v-else-if="name === 'download'">
      <path d="M12 3v12" />
      <path d="m7 10 5 5 5-5" />
      <path d="M4 21h16" />
    </template>

    <template v-else-if="name === 'upload'">
      <path d="M12 16V4" />
      <path d="m7 9 5-5 5 5" />
      <path d="M4 21h16" />
    </template>

    <template v-else-if="name === 'tracker'">
      <circle cx="12" cy="12" r="9" />
      <path d="M3 12h18" />
      <path d="M12 3c2.5 2.6 4 5.7 4 9s-1.5 6.4-4 9c-2.5-2.6-4-5.7-4-9s1.5-6.4 4-9Z" />
    </template>

    <template v-else-if="name === 'plugin'">
      <path
        d="M20 13v4a2 2 0 0 1-2 2h-4v-1.5a2.5 2.5 0 0 0-5 0V19H5a2 2 0 0 1-2-2v-4h1.5a2.5 2.5 0 0 0 0-5H3V4a2 2 0 0 1 2-2h4v1.5a2.5 2.5 0 0 0 5 0V2h4a2 2 0 0 1 2 2v4h1.5a2.5 2.5 0 0 0 0 5H20Z"
      />
    </template>

    <template v-else-if="name === 'bell'">
      <path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9" />
      <path d="M13.7 21a2 2 0 0 1-3.4 0" />
    </template>

    <template v-else-if="name === 'settings'">
      <path d="M4 6h16" />
      <path d="M4 12h16" />
      <path d="M4 18h16" />
      <circle cx="9" cy="6" r="2" fill="var(--surface)" />
      <circle cx="15" cy="12" r="2" fill="var(--surface)" />
      <circle cx="8" cy="18" r="2" fill="var(--surface)" />
    </template>

    <template v-else-if="name === 'plus'">
      <path d="M12 5v14" />
      <path d="M5 12h14" />
    </template>

    <template v-else-if="name === 'chevron'">
      <path d="m15 18-6-6 6-6" />
    </template>

    <template v-else-if="name === 'search'">
      <circle cx="11" cy="11" r="7" />
      <path d="m20 20-3.2-3.2" />
    </template>

    <template v-else-if="name === 'refresh'">
      <path d="M21 12a9 9 0 1 1-2.6-6.4" />
      <path d="M21 3v6h-6" />
    </template>

    <template v-else-if="name === 'more'">
      <circle cx="5" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="12" cy="12" r="1.4" fill="currentColor" stroke="none" />
      <circle cx="19" cy="12" r="1.4" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'split'">
      <rect x="3" y="4" width="7.5" height="16" rx="2" />
      <rect x="13.5" y="4" width="7.5" height="16" rx="2" />
    </template>

    <template v-else-if="name === 'info'">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5" />
      <circle cx="12" cy="8" r="0.9" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'import'">
      <path d="M12 15V3" />
      <path d="m7 8 5-5 5 5" />
      <path d="M4 15v4a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-4" />
    </template>

    <template v-else-if="name === 'warning'">
      <path d="M10.3 3.9 2.6 17.2A2 2 0 0 0 4.3 20h15.4a2 2 0 0 0 1.7-2.8L13.7 3.9a2 2 0 0 0-3.4 0Z" />
      <path d="M12 9v4" />
      <circle cx="12" cy="16.5" r="0.9" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'error'">
      <circle cx="12" cy="12" r="9" />
      <path d="m9 9 6 6" />
      <path d="m15 9-6 6" />
    </template>

    <template v-else-if="name === 'check'">
      <circle cx="12" cy="12" r="9" />
      <path d="m8.5 12.5 2.5 2.5 4.5-5" />
    </template>

    <template v-else-if="name === 'folder'">
      <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z" />
    </template>

    <template v-else-if="name === 'trash'">
      <path d="M4 7h16" />
      <path d="M9 7V4h6v3" />
      <path d="M6 7l1 13h10l1-13" />
      <path d="M10 11v5" />
      <path d="M14 11v5" />
    </template>

    <template v-else-if="name === 'pause'">
      <rect x="7" y="5" width="3.5" height="14" rx="1.2" fill="currentColor" stroke="none" />
      <rect x="13.5" y="5" width="3.5" height="14" rx="1.2" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'play'">
      <path d="M8 5.5v13l11-6.5-11-6.5Z" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'link'">
      <path d="M10 13a5 5 0 0 0 7.1 0l2-2a5 5 0 0 0-7.1-7.1l-1.2 1.2" />
      <path d="M14 11a5 5 0 0 0-7.1 0l-2 2A5 5 0 0 0 12 20.1l1.2-1.2" />
    </template>

    <template v-else-if="name === 'copy'">
      <rect x="9" y="9" width="11" height="11" rx="2" />
      <path d="M5 15V5a2 2 0 0 1 2-2h8" />
    </template>

    <template v-else-if="name === 'gauge'">
      <path d="M4 18a9 9 0 1 1 16 0" />
      <path d="m12 14 4-4" />
      <circle cx="12" cy="14" r="1.2" fill="currentColor" stroke="none" />
    </template>

    <template v-else-if="name === 'clock'">
      <circle cx="12" cy="12" r="9" />
      <path d="M12 7v5l3 2" />
    </template>

    <template v-else-if="name === 'seed'">
      <path d="M12 21c4-2 6-5.5 6-10V5l-4 2c-2 1-3 2.5-3 4.5" />
      <path d="M8 11c-1 1-2 2.5-2 4.5 0 2 1.5 4 4 5" />
    </template>

    <template v-else-if="name === 'close'">
      <path d="m6 6 12 12" />
      <path d="m18 6-12 12" />
    </template>

    <template v-else-if="name === 'timer'">
      <circle cx="12" cy="13" r="8" />
      <path d="M12 9v4l2.5 1.5" />
      <path d="M9 2h6" />
    </template>

    <template v-else-if="name === 'shield'">
      <path d="M12 3 4 6v5c0 5 3.4 8.4 8 10 4.6-1.6 8-5 8-10V6l-8-3Z" />
      <path d="m9 12 2 2 4-4" />
    </template>

    <template v-else-if="name === 'minimize'">
      <path d="M5 12h14" />
    </template>

    <template v-else-if="name === 'maximize'">
      <rect x="5" y="5" width="14" height="14" rx="2" />
    </template>
  </svg>
</template>
