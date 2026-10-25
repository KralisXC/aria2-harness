<script setup lang="ts">
import { storeToRefs } from "pinia";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import { useNotificationsStore, type NoticeKind } from "../stores/notifications";

const store = useNotificationsStore();
const { items, unreadCount } = storeToRefs(store);

const ICON: Record<NoticeKind, string> = {
  error: "error",
  warn: "warning",
  info: "info",
  success: "check",
};

function timeLabel(time: number): string {
  const diff = Date.now() - time;
  const minute = 60_000;
  if (diff < minute) return "刚刚";
  if (diff < 60 * minute) return `${Math.floor(diff / minute)} 分钟前`;
  if (diff < 24 * 60 * minute) return `${Math.floor(diff / (60 * minute))} 小时前`;
  const date = new Date(time);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}
</script>

<template>
  <div class="view-notifications">
    <PageHeader title="通知" :subtitle="`${unreadCount} 条未读 · 共 ${items.length} 条`">
      <template #actions>
        <button class="btn-ghost action" :disabled="items.length === 0" @click="store.markAllRead()">
          <AppIcon name="check" :size="15" />
          全部已读
        </button>
        <button class="btn-ghost action" :disabled="items.length === 0" @click="store.clear()">
          <AppIcon name="trash" :size="15" />
          清空
        </button>
      </template>
    </PageHeader>

    <section v-if="items.length > 0" class="notice-list">
      <article
        v-for="item in items"
        :key="item.id"
        class="notice-card"
        :class="{ unread: !item.read }"
      >
        <span class="unread-dot" :class="{ show: !item.read }"></span>
        <span class="notice-icon" :class="item.kind">
          <AppIcon :name="ICON[item.kind]" :size="16" />
        </span>
        <div class="notice-body">
          <div class="notice-head">
            <b class="notice-title">{{ item.title }}</b>
            <span class="notice-time">{{ timeLabel(item.time) }}</span>
          </div>
          <p class="notice-desc">{{ item.desc }}</p>
          <span v-if="item.taskName" class="notice-task">
            <AppIcon name="download" :size="12" />
            {{ item.taskName }}
          </span>
        </div>
      </article>
    </section>

    <section v-else class="empty-state">
      <span class="empty-icon">
        <AppIcon name="bell" :size="26" />
      </span>
      <p>暂无通知</p>
      <small>任务失败、连接断开与插件异常会显示在这里</small>
    </section>
  </div>
</template>

<style scoped>
.action {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 32px;
  padding: 0 12px;
  border-radius: var(--r-btn);
  font-size: 12px;
}

.notice-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.notice-card {
  position: relative;
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 14px 16px 14px 20px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
}
.notice-card.unread {
  background: color-mix(in srgb, var(--accent) 4%, var(--surface));
  border-color: color-mix(in srgb, var(--accent) 22%, var(--line));
}

.unread-dot {
  position: absolute;
  left: 8px;
  top: 20px;
  width: 6px;
  height: 6px;
  border-radius: 999px;
  background: #3b82f6;
  opacity: 0;
}
.unread-dot.show {
  opacity: 1;
}

.notice-icon {
  flex: none;
  width: 34px;
  height: 34px;
  display: grid;
  place-items: center;
  border-radius: 11px;
  background: var(--surface-3);
  color: var(--muted);
}
.notice-icon.error {
  background: var(--danger-soft);
  color: var(--danger);
}
.notice-icon.warn {
  background: var(--warn-soft);
  color: var(--warn);
}
.notice-icon.success {
  background: var(--success-soft);
  color: var(--success);
}
.notice-icon.info {
  background: var(--accent-soft);
  color: var(--accent);
}

.notice-body {
  flex: 1;
  min-width: 0;
}

.notice-head {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 12px;
}

.notice-title {
  font-size: 13px;
  font-weight: 650;
  color: var(--text);
}

.notice-time {
  flex: none;
  font-size: 11px;
  color: var(--muted);
}

.notice-desc {
  margin: 4px 0 0;
  font-size: 12px;
  line-height: 1.5;
  color: var(--text-2);
  word-break: break-all;
}

.notice-task {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  margin-top: 7px;
  padding: 2px 9px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--muted);
  font-size: 10.5px;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.empty-state {
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  min-height: 52vh;
  color: var(--muted);
}
.empty-icon {
  width: 62px;
  height: 62px;
  display: grid;
  place-items: center;
  border-radius: 20px;
  background: var(--surface-2);
  color: var(--line-strong);
}
.empty-state p {
  margin: 4px 0 0;
  font-size: 14px;
  color: var(--text-2);
}
.empty-state small {
  font-size: 11.5px;
}
</style>
