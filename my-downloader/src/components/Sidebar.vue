<script setup lang="ts">
import { useRoute, useRouter } from "vue-router";
import AppIcon from "./AppIcon.vue";

defineProps<{
  collapsed: boolean;
  unread: number;
  /** 设置弹窗是否打开（仅用于高亮，不再走路由）。 */
  settingsOpen?: boolean;
}>();

const emit = defineEmits<{ "open-settings": [] }>();

const route = useRoute();
const router = useRouter();

const navItems = [
  { path: "/dashboard", label: "仪表盘", icon: "dashboard" },
  { path: "/downloads", label: "下载", icon: "download" },
  { path: "/trackers", label: "Trackers", icon: "tracker" },
  { path: "/plugins", label: "插件", icon: "plugin" },
];

const isActive = (path: string) => route.path === path;

function go(path: string) {
  if (route.path !== path) void router.push(path);
}
</script>

<template>
  <aside class="sider" :class="{ collapsed }">
    <nav class="nav">
      <p v-show="!collapsed" class="nav-caption">MAIN</p>
      <button
        v-for="item in navItems"
        :key="item.path"
        class="nav-item"
        :class="{ active: isActive(item.path) }"
        :title="item.label"
        @click="go(item.path)"
      >
        <AppIcon :name="item.icon" :size="18" />
        <span v-show="!collapsed" class="nav-label">{{ item.label }}</span>
      </button>
    </nav>

    <div class="nav bottom">
      <button
        class="nav-item"
        :class="{ active: isActive('/notifications') }"
        title="通知"
        @click="go('/notifications')"
      >
        <span class="icon-wrap">
          <AppIcon name="bell" :size="18" />
          <span v-if="unread > 0" class="badge">{{ unread > 99 ? "99+" : unread }}</span>
        </span>
        <span v-show="!collapsed" class="nav-label">通知</span>
      </button>
      <button
        class="nav-item"
        :class="{ active: settingsOpen }"
        title="设置"
        @click="emit('open-settings')"
      >
        <AppIcon name="settings" :size="18" />
        <span v-show="!collapsed" class="nav-label">设置</span>
      </button>
    </div>
  </aside>
</template>

<style scoped>
.sider {
  flex: none;
  width: var(--sidebar-w);
  height: 100%;
  display: flex;
  flex-direction: column;
  padding: 2px 12px 14px;
  background: var(--bg);
  transition: width 0.22s ease;
}
.sider.collapsed {
  width: var(--sidebar-w-collapsed);
  padding: 2px 10px 14px;
}

.nav {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding-top: 10px;
}
.nav.bottom {
  margin-top: auto;
  padding-top: 12px;
  border-top: 1px solid var(--line);
}

.nav-caption {
  margin: 0 10px 4px;
  font-size: 10px;
  letter-spacing: 1.8px;
  color: var(--muted);
}

.nav-item {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  height: 48px;
  padding: 0 14px;
  border: 0;
  border-radius: var(--r-nav);
  background: transparent;
  color: var(--text-2);
  font-size: 14px;
  font-weight: 500;
  transition:
    background 0.15s ease,
    color 0.15s ease;
}
.nav-item:hover:not(.active) {
  background: var(--surface-3);
  color: var(--text);
}
.nav-item.active {
  background: var(--surface-3);
  color: var(--accent);
  font-weight: 600;
}
.nav-label {
  overflow: hidden;
  white-space: nowrap;
}

.collapsed .nav-item {
  justify-content: center;
  padding: 0;
  gap: 0;
}
.collapsed .nav-caption {
  display: none;
}

.icon-wrap {
  position: relative;
  display: inline-flex;
}
.badge {
  position: absolute;
  top: -6px;
  right: -8px;
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  display: grid;
  place-items: center;
  border-radius: 999px;
  background: var(--danger);
  color: #fff;
  font-size: 9px;
  font-weight: 700;
  line-height: 1;
}
</style>
