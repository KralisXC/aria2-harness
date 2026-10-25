import { computed, ref } from "vue";
import { defineStore } from "pinia";

export type NoticeKind = "error" | "warn" | "info" | "success";

export interface NoticeItem {
  id: string;
  kind: NoticeKind;
  title: string;
  desc: string;
  time: number;
  read: boolean;
  taskName?: string;
}

const STORAGE_KEY = "aria2-harness-notices";
const MAX_ITEMS = 100;

function loadInitial(): NoticeItem[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as NoticeItem[];
    return Array.isArray(parsed) ? parsed.slice(0, MAX_ITEMS) : [];
  } catch {
    return [];
  }
}

/**
 * 通知中心（纯前端聚合）。
 *
 * 数据源：任务终态错误、引擎断连、插件熔断 / 热重载。
 * 去重：调用方传 `dedupeKey`，同一 key 只记一次，避免 WS 事件风暴刷屏。
 */
export const useNotificationsStore = defineStore("notifications", () => {
  const items = ref<NoticeItem[]>(loadInitial());
  const seen = new Set<string>();

  const unreadCount = computed(
    () => items.value.filter((item) => !item.read).length,
  );

  function persist() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(items.value.slice(0, MAX_ITEMS)));
    } catch {
      // 存储不可用时仅保留内存态。
    }
  }

  function push(
    kind: NoticeKind,
    title: string,
    desc: string,
    options: { dedupeKey?: string; taskName?: string } = {},
  ) {
    const key = options.dedupeKey;
    if (key) {
      if (seen.has(key)) return;
      seen.add(key);
    }
    const now = Date.now();
    items.value.unshift({
      id: `${now}-${Math.random().toString(36).slice(2, 8)}`,
      kind,
      title,
      desc,
      time: now,
      read: false,
      taskName: options.taskName,
    });
    if (items.value.length > MAX_ITEMS) items.value.length = MAX_ITEMS;
    persist();
  }

  function markAllRead() {
    for (const item of items.value) item.read = true;
    persist();
  }

  function clear() {
    items.value = [];
    seen.clear();
    persist();
  }

  return { items, unreadCount, push, markAllRead, clear };
});
