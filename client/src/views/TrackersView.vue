<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { storeToRefs } from "pinia";
import { NSwitch } from "naive-ui";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import { errText, invoke } from "../ipc";
import { useSystemStore } from "../stores/system";

const system = useSystemStore();
const { trackers, trackerLoading, trackerRefreshing, trackerError } =
  storeToRefs(system);

const tab = ref<"active" | "blocklist">("active");
const localError = ref("");

const items = computed(() => trackers.value?.items ?? []);
const injecting = computed(() => trackers.value?.injecting ?? false);

/** 源地址 -> 可读标签（snapshot 回传的是完整 URL）。 */
const sourceLabel = computed(() => {
  const source = trackers.value?.source ?? "";
  if (source.includes("ngosang")) return "ngosang/trackerslist (best)";
  if (!source) return "未知来源";
  return source.split("/").slice(-2).join("/") || source;
});

function formatTime(seconds: number | null | undefined): string {
  if (!seconds) return "尚未同步";
  const date = new Date(seconds * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}`;
}

const lastProbe = computed(() => formatTime(trackers.value?.lastUpdate));

/** 健康度：已注入的 tracker 视为可用（无后端探测数据，不伪造延迟）。 */
function healthOf(): { label: string; level: "ok" | "idle" } {
  return injecting.value
    ? { label: "可用", level: "ok" }
    : { label: "未注入", level: "idle" };
}

async function sync() {
  localError.value = "";
  await system.refreshTrackers();
}

/**
 * 开关切换：UI 立即乐观更新，RPC 全部丢到后台静默执行。
 *
 * 刻意不给 n-switch 绑 loading：naive-ui 的 loading 会在方块里画一个
 * 圆形 spinner，与 CTOS 方块拨动样式冲突，也会让开关「卡住」不可再点。
 * 失败时把引擎真实状态拉回来覆盖乐观值，错误提示落在页脚。
 */
function onToggle(value: boolean) {
  localError.value = "";
  if (value) {
    if (trackers.value) trackers.value = { ...trackers.value, injecting: true };
    void system.refreshTrackers().then(() => {
      if (!trackerError.value) return;
      localError.value = `注入失败：${trackerError.value}`;
      void system.loadTrackers();
    });
    return;
  }

  if (trackers.value) {
    trackers.value = { ...trackers.value, items: [], count: 0, injecting: false };
  }
  void invoke("change_global_option", { options: { "bt-tracker": "" } }).catch(
    (error) => {
      localError.value = `关闭注入失败：${errText(error)}`;
      void system.loadTrackers();
    },
  );
}

function removeSource() {
  onToggle(false);
}

onMounted(() => {
  void system.loadTrackers();
});
</script>

<template>
  <div class="view-trackers">
    <PageHeader title="Trackers" subtitle="BT 任务使用的公共 Tracker 池">
      <template #actions>
        <span class="inject-label">注入精选 Tracker</span>
        <n-switch :value="injecting" @update:value="onToggle" />
      </template>
    </PageHeader>

    <div class="tab-bar">
      <button class="tab" :class="{ on: tab === 'active' }" @click="tab = 'active'">
        生效
        <span class="tab-count num">{{ items.length }}</span>
      </button>
      <button class="tab" :class="{ on: tab === 'blocklist' }" @click="tab = 'blocklist'">
        黑名单
        <span class="tab-count num">0</span>
      </button>
    </div>

    <section v-if="tab === 'active'" class="content-card-flat">
      <div v-if="injecting" class="source-row">
        <span class="source-chip">
          <AppIcon name="link" :size="13" />
          <span class="source-name">{{ sourceLabel }}</span>
          <button class="chip-x" title="移除该来源" @click.stop="removeSource">
            <AppIcon name="close" :size="12" />
          </button>
        </span>
      </div>

      <div v-if="trackerLoading" class="placeholder">正在读取 Tracker 列表...</div>

      <div v-else-if="items.length === 0" class="placeholder">
        <AppIcon name="tracker" :size="26" class="placeholder-icon" />
        <p>当前没有生效的 Tracker</p>
        <small>开启右上角开关，即可每日自动拉取并注入精选列表</small>
      </div>

      <div v-else class="table-wrap">
        <table class="tracker-table">
          <thead>
            <tr>
              <th class="col-url">URL</th>
              <th class="col-health">健康度</th>
              <th class="col-time">最后探测</th>
              <th class="col-source">来源</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="(item, index) in items" :key="`${index}-${item}`">
              <td class="col-url">
                <span class="url" :title="item">{{ item }}</span>
              </td>
              <td class="col-health">
                <span class="health-dot" :class="healthOf().level"></span>
                {{ healthOf().label }}
              </td>
              <td class="col-time">{{ lastProbe }}</td>
              <td class="col-source">{{ sourceLabel }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <section v-else class="content-card-flat">
      <div class="placeholder">
        <AppIcon name="shield" :size="26" class="placeholder-icon" />
        <p>黑名单为空</p>
        <small>被屏蔽的 Tracker 会显示在这里</small>
      </div>
    </section>

    <footer class="tracker-footer">
      <div class="footer-info">
        <span class="footer-note">上次同步：{{ lastProbe }}</span>
        <span v-if="trackerError || localError" class="footer-error">
          <AppIcon name="warning" :size="14" />
          同步未完成，请检查网络：{{ localError || trackerError }}
        </span>
      </div>
      <button class="btn-dark" :disabled="trackerRefreshing" @click="sync">
        <AppIcon name="refresh" :size="15" />
        {{ trackerRefreshing ? "同步中..." : "立即同步" }}
      </button>
    </footer>
  </div>
</template>

<style scoped>
.inject-label {
  font-size: 12px;
  color: var(--text-2);
}

.tab-bar {
  display: flex;
  gap: 6px;
  margin-bottom: 14px;
  padding: 4px;
  width: fit-content;
  border-radius: 999px;
  background: var(--surface-3);
}

.tab {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  height: 30px;
  padding: 0 15px;
  border: 0;
  border-radius: 999px;
  background: transparent;
  color: var(--muted);
  font-size: 12.5px;
}
.tab:hover {
  background: transparent;
  color: var(--text-2);
}
.tab.on {
  background: var(--surface);
  color: var(--text);
  font-weight: 600;
  box-shadow: 0 2px 6px rgba(0, 0, 0, 0.06);
}

.tab-count {
  padding: 0 6px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--muted);
  font-size: 10.5px;
  line-height: 16px;
}
.tab.on .tab-count {
  background: var(--accent-soft);
  color: var(--accent);
}

.content-card-flat {
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
  overflow: hidden;
}

.source-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 14px 18px 0;
}

.source-chip {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  height: 28px;
  padding: 0 6px 0 11px;
  border-radius: 999px;
  background: var(--surface-3);
  color: var(--text-2);
  font-size: 11.5px;
}

.source-name {
  max-width: 260px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.chip-x {
  width: 20px;
  height: 20px;
  padding: 0;
  display: grid;
  place-items: center;
  border: 0;
  border-radius: 999px;
  background: transparent;
  color: var(--muted);
}
.chip-x:hover {
  background: var(--danger-soft);
  color: var(--danger);
}

.placeholder {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 56px 20px;
  color: var(--muted);
  font-size: 13px;
}
.placeholder p {
  margin: 0;
  font-size: 14px;
  color: var(--text-2);
}
.placeholder small {
  font-size: 11.5px;
}
.placeholder-icon {
  color: var(--line-strong);
}

.table-wrap {
  overflow-x: auto;
}

.tracker-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12px;
}

.tracker-table th {
  padding: 12px 18px;
  border-bottom: 1px solid var(--line);
  color: var(--muted);
  font-size: 11px;
  font-weight: 500;
  letter-spacing: 0.6px;
  text-align: left;
  white-space: nowrap;
}

.tracker-table td {
  padding: 11px 18px;
  border-bottom: 1px solid color-mix(in srgb, var(--line) 60%, transparent);
  color: var(--text-2);
  vertical-align: middle;
}

.tracker-table tbody tr:nth-child(even) {
  background: color-mix(in srgb, var(--surface-2) 65%, transparent);
}
.tracker-table tbody tr:hover {
  background: var(--surface-2);
}
.tracker-table tbody tr:last-child td {
  border-bottom: 0;
}

.col-url {
  width: 58%;
}
.col-health {
  width: 90px;
  white-space: nowrap;
}
.col-time,
.col-source {
  white-space: nowrap;
  color: var(--muted);
}

.url {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  direction: ltr;
  color: var(--text);
}

.health-dot {
  display: inline-block;
  width: 7px;
  height: 7px;
  margin-right: 6px;
  border-radius: 999px;
  background: var(--muted);
}
.health-dot.ok {
  background: var(--success);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--success) 14%, transparent);
}
.health-dot.idle {
  background: var(--line-strong);
}

.tracker-footer {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  margin-top: 14px;
  padding: 12px 16px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
}

.footer-info {
  display: flex;
  flex-direction: column;
  gap: 3px;
  min-width: 0;
}

.footer-note {
  font-size: 11.5px;
  color: var(--muted);
}

.footer-error {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  font-size: 11.5px;
  color: var(--danger);
}

.btn-dark {
  flex: none;
  display: inline-flex;
  align-items: center;
  gap: 7px;
  height: 34px;
  padding: 0 16px;
  border: 1px solid #1f2d2c;
  border-radius: var(--r-btn);
  background: #1f2d2c;
  color: #ffffff;
  font-size: 12.5px;
  font-weight: 600;
}
.btn-dark:hover:not(:disabled) {
  background: #2c403e;
  border-color: #2c403e;
  color: #ffffff;
}
</style>
