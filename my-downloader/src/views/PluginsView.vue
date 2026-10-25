<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { storeToRefs } from "pinia";
import { NInput, NModal, NSelect, NSwitch } from "naive-ui";
import PageHeader from "../components/PageHeader.vue";
import AppIcon from "../components/AppIcon.vue";
import { errText, invoke } from "../ipc";
import { permLabel, usePluginsStore, type PluginInfo } from "../stores/plugins";

const store = usePluginsStore();
const { plugins, loading: pluginLoading, message: pluginMessage, grantTarget } =
  storeToRefs(store);

const searchOpen = ref(false);
const keyword = ref("");
const helpOpen = ref(false);

/** 插件设置弹窗：草稿与已保存值隔离，取消即丢弃。 */
const settingsTarget = ref<PluginInfo | null>(null);
const settingsDraft = ref<Record<string, unknown>>({});
const settingsSaving = ref(false);
const settingsError = ref("");
const settingsOpen = ref(false);

function openSettings(p: PluginInfo) {
  const draft: Record<string, unknown> = {};
  for (const item of p.settings) {
    draft[item.key] = item.value ?? item.default;
  }
  settingsDraft.value = draft;
  settingsTarget.value = p;
  settingsError.value = "";
  settingsOpen.value = true;
}

function closeSettings() {
  settingsOpen.value = false;
  settingsTarget.value = null;
  settingsDraft.value = {};
  settingsError.value = "";
}

async function savePluginSettings() {
  const target = settingsTarget.value;
  if (!target) return;
  settingsSaving.value = true;
  settingsError.value = "";
  try {
    await store.saveSettings(target, { ...settingsDraft.value });
    closeSettings();
  } catch (error) {
    settingsError.value = `保存失败：${errText(error)}`;
  } finally {
    settingsSaving.value = false;
  }
}

/** 草稿读写：模板里避免 TS 断言，统一走显式取值函数。 */
function boolOf(key: string): boolean {
  return settingsDraft.value[key] === true;
}
function setBool(key: string, value: boolean) {
  settingsDraft.value[key] = value;
}
function strOf(key: string): string {
  const value = settingsDraft.value[key];
  return typeof value === "string" ? value : "";
}
function setStr(key: string, value: string) {
  settingsDraft.value[key] = value;
}
function selectOptions(options: string[]) {
  return options.map((option) => ({ label: option, value: option }));
}

/** n-modal 关闭（遮罩 / ESC / 右上角）统一丢弃草稿。 */
function onSettingsShow(value: boolean) {
  if (!value) closeSettings();
}

/** 已安装插件按名称/描述过滤（顶部搜索）。 */
const filtered = computed(() => {
  const key = keyword.value.trim().toLowerCase();
  if (!key) return plugins.value;
  return plugins.value.filter(
    (p) =>
      p.name.toLowerCase().includes(key) ||
      p.id.toLowerCase().includes(key) ||
      p.description.toLowerCase().includes(key),
  );
});

const PALETTE = ["#14b8a6", "#8b5cf6", "#f59e0b", "#3b82f6", "#ec4899", "#22c55e"];

/** 插件彩色图标底色：按 id 稳定散列，避免每次刷新变色。 */
function tintOf(p: PluginInfo): string {
  let hash = 0;
  for (const ch of p.id) hash = (hash * 31 + ch.charCodeAt(0)) % 997;
  return PALETTE[hash % PALETTE.length];
}

function initialOf(p: PluginInfo): string {
  return (p.name || p.id).slice(0, 1).toUpperCase();
}

function statusOf(p: PluginInfo): { text: string; tone: string } {
  if (p.circuitBroken) return { text: "已熔断", tone: "danger" };
  if (p.pendingPermissions.length > 0 && !p.enabled) {
    return { text: `待授权 ${p.pendingPermissions.length} 项`, tone: "warn" };
  }
  if (p.enabled) return { text: "运行正常", tone: "ok" };
  return { text: "已禁用", tone: "off" };
}

/** 插件目录：由宿主解析（便携版 / 安装版位置不同），前端不写死绝对路径。 */
const pluginDir = ref("");

onMounted(() => {
  void store.load();
  void invoke<{ plugins?: string }>("get_app_paths")
    .then((paths) => {
      pluginDir.value = paths.plugins ?? "";
    })
    .catch(() => {
      // 宿主未就绪：模板回退占位符。
    });
});
</script>

<template>
  <div class="view-plugins">
    <PageHeader title="插件" subtitle="扩展宿主能力（QuickJS / Wasm）">
      <template #actions>
        <div class="head-search" :class="{ open: searchOpen }">
          <input
            v-if="searchOpen"
            v-model="keyword"
            type="search"
            placeholder="搜索插件"
            class="head-search-input"
          />
          <button
            class="icon-btn"
            :class="{ active: searchOpen }"
            title="搜索"
            @click="searchOpen = !searchOpen"
          >
            <AppIcon name="search" :size="17" />
          </button>
        </div>
        <button class="icon-btn" title="刷新" @click="store.load()">
          <AppIcon name="refresh" :size="17" />
        </button>
        <button class="icon-btn" title="导入插件" @click="helpOpen = true">
          <AppIcon name="import" :size="17" />
        </button>
        <button class="btn-primary add-btn" @click="helpOpen = true">
          <AppIcon name="plus" :size="16" />
          <span>添加</span>
        </button>
      </template>
    </PageHeader>

    <p v-if="pluginMessage" class="status">{{ pluginMessage }}</p>

    <!-- 添加可信插件 -->
    <button class="trust-bar" @click="helpOpen = true">
      <span class="trust-icon">
        <AppIcon name="shield" :size="18" />
      </span>
      <span class="trust-text">
        <b>添加可信插件</b>
        <small>插件需通过 SHA-256 完整性校验后才会加载</small>
      </span>
      <AppIcon name="plus" :size="16" class="trust-plus" />
    </button>

    <section class="plugins">
      <p v-if="pluginLoading" class="empty">加载中...</p>
      <p v-else-if="filtered.length === 0" class="empty">未发现任何插件</p>

      <article
        v-for="p in filtered"
        :key="p.id"
        class="plugin-card"
        :class="{ off: !p.enabled }"
      >
        <div class="plugin-top">
          <span class="plugin-icon" :style="{ background: tintOf(p) }">{{ initialOf(p) }}</span>

          <div class="plugin-info">
            <div class="name-row">
              <span class="name" :title="p.id">{{ p.name }}</span>
              <span class="version">v{{ p.version }}</span>
              <span class="chip" :class="statusOf(p).tone">{{ statusOf(p).text }}</span>
            </div>
            <p class="plugin-desc">{{ p.description || "（无描述）" }}</p>
            <div class="plugin-tags">
              <span v-for="perm in p.permissions" :key="perm" class="tag" :title="permLabel(perm)">
                {{ perm }}
              </span>
              <span v-if="p.permissions.length === 0" class="tag muted">无额外权限</span>
            </div>
          </div>

          <div class="plugin-actions">
            <button
              v-if="p.settings.length > 0"
              class="settings-btn"
              title="插件设置"
              @click="openSettings(p)"
            >
              <AppIcon name="settings" :size="14" />
              <span>设置</span>
            </button>
            <n-switch
              :value="p.enabled"
              :disabled="pluginLoading"
              @update:value="store.toggle(p)"
            />
          </div>
        </div>

        <div class="plugin-foot">
          <span class="hook-line">
            Hooks：{{ p.hooks.length > 0 ? p.hooks.join("、") : "未注册" }}
          </span>
          <span class="plugin-id">优先级 {{ p.priority }} · {{ p.id }}</span>
        </div>
      </article>
    </section>

    <!-- 运行时保护：填充底部留白，同时说明宿主的安全边界 -->
    <section class="runtime-strip">
      <div class="runtime-item">
        <AppIcon name="shield" :size="16" class="runtime-icon ok" />
        <div>
          <b>SHA-256 校验</b>
          <small>加载前比对 manifest 摘要，不匹配直接拒绝</small>
        </div>
      </div>
      <div class="runtime-item">
        <AppIcon name="gauge" :size="16" class="runtime-icon purple" />
        <div>
          <b>Wasm 内存限额</b>
          <small>按 limits.memory_mb 强制限制线性内存</small>
        </div>
      </div>
      <div class="runtime-item">
        <AppIcon name="warning" :size="16" class="runtime-icon warn" />
        <div>
          <b>连续 3 次失败熔断</b>
          <small>Hook 异常自动挂起，不影响宿主运行</small>
        </div>
      </div>
    </section>

    <!-- 权限确认弹窗（逻辑保持不变，仅换肤） -->
    <div v-if="grantTarget" class="modal-mask" @click.self="store.cancelGrant()">
      <div class="modal" role="dialog" aria-modal="true">
        <header class="modal-head">
          <h2>授权确认</h2>
          <button class="icon-btn" title="关闭" @click="store.cancelGrant()">
            <AppIcon name="close" :size="16" />
          </button>
        </header>
        <p class="modal-body">
          插件 <strong>{{ grantTarget.name }}</strong> 请求新增以下权限。授权后，该插件将获得这些能力的访问权。
        </p>
        <ul class="perm-list">
          <li v-for="perm in grantTarget.pendingPermissions" :key="perm">
            <span class="perm-code">{{ perm }}</span>
            <span class="perm-label">{{ permLabel(perm) }}</span>
          </li>
        </ul>
        <div class="modal-actions">
          <button class="btn-primary" @click="store.confirmGrant()">同意并启用</button>
        </div>
      </div>
    </div>

    <!-- 插件设置弹窗：按 manifest.settings 的 type 动态渲染 -->
    <n-modal
      :show="settingsOpen"
      preset="card"
      class="plugin-settings-modal"
      :title="settingsTarget ? `${settingsTarget.name} · 设置` : '插件设置'"
      :style="{ width: '480px', maxWidth: '92vw' }"
      :mask-closable="!settingsSaving"
      @update:show="onSettingsShow"
    >
      <div v-if="settingsTarget" class="settings-form">
        <div v-for="item in settingsTarget.settings" :key="item.key" class="settings-row">
          <div class="settings-copy">
            <span>{{ item.label }}</span>
            <small>{{ item.key }}</small>
          </div>
          <n-switch
            v-if="item.type === 'boolean'"
            :value="boolOf(item.key)"
            @update:value="setBool(item.key, $event)"
          />
          <n-select
            v-else-if="item.type === 'select'"
            :value="strOf(item.key)"
            :options="selectOptions(item.options)"
            class="settings-select"
            :consistent-menu-width="false"
            @update:value="setStr(item.key, $event)"
          />
          <n-input
            v-else
            :value="strOf(item.key)"
            class="settings-input"
            placeholder="请输入"
            @update:value="setStr(item.key, $event)"
          />
        </div>
      </div>
      <p v-if="settingsError" class="settings-error">{{ settingsError }}</p>
      <template #footer>
        <div class="settings-actions">
          <button class="btn-primary" :disabled="settingsSaving" @click="savePluginSettings">
            {{ settingsSaving ? "保存中..." : "保存" }}
          </button>
        </div>
      </template>
    </n-modal>

    <!-- 安装说明 -->
    <div v-if="helpOpen" class="modal-mask" @click.self="helpOpen = false">
      <div class="modal" role="dialog" aria-modal="true">
        <header class="modal-head">
          <h2>添加可信插件</h2>
          <button class="icon-btn" title="关闭" @click="helpOpen = false">
            <AppIcon name="close" :size="16" />
          </button>
        </header>
        <p class="modal-body">
          将插件目录放入 <code>{{ pluginDir || "宿主插件目录" }}</code>，确保 <code>manifest.json</code>
          中声明了正确的 SHA-256 摘要，然后点击刷新即可加载。
        </p>
        <div class="modal-actions">
          <button class="btn-primary" @click="helpOpen = false; store.load()">刷新列表</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.head-search {
  display: flex;
  align-items: center;
  gap: 6px;
}
.head-search-input {
  width: 168px;
  height: 34px;
  padding: 0 12px;
  border-radius: var(--r-btn);
}

.add-btn {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  height: 34px;
  padding: 0 14px;
  border-radius: var(--r-btn);
  font-size: 13px;
}

.trust-bar {
  display: flex;
  align-items: center;
  gap: 12px;
  width: 100%;
  margin-bottom: 14px;
  padding: 13px 16px;
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-card);
  background: var(--surface-2);
  text-align: left;
}
.trust-bar:hover {
  border-color: color-mix(in srgb, var(--accent) 45%, var(--line-strong));
  background: var(--accent-soft);
}
.trust-icon {
  flex: none;
  width: 36px;
  height: 36px;
  display: grid;
  place-items: center;
  border-radius: 12px;
  background: var(--surface);
  color: var(--accent);
  box-shadow: var(--shadow-card);
}
.trust-text {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.trust-text b {
  font-size: 13px;
  color: var(--text);
}
.trust-text small {
  font-size: 11.5px;
  color: var(--muted);
}
.trust-plus {
  margin-left: auto;
  color: var(--muted);
}

.plugins {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.runtime-strip {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 14px;
  margin-top: 16px;
  padding: 16px 18px;
  border: 1px dashed var(--line-strong);
  border-radius: var(--r-card);
  background: var(--surface-2);
}
.runtime-item {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  min-width: 0;
}
.runtime-item > div {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.runtime-item b {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
}
.runtime-item small {
  font-size: 11.5px;
  color: var(--muted);
}
.runtime-icon {
  flex: none;
  margin-top: 1px;
}
.runtime-icon.ok {
  color: var(--accent);
}
.runtime-icon.purple {
  color: var(--purple);
}
.runtime-icon.warn {
  color: var(--warn);
}

@media (max-width: 1100px) {
  .runtime-strip {
    grid-template-columns: 1fr;
  }
}

.plugin-card {
  padding: 16px 18px;
  border: 1px solid var(--line);
  border-radius: var(--r-card);
  background: var(--surface);
  box-shadow: var(--shadow-card);
  transition:
    border-color 0.15s ease,
    opacity 0.2s ease;
}
.plugin-card:hover {
  border-color: var(--line-strong);
}
.plugin-card.off {
  opacity: 0.62;
}

.plugin-top {
  display: flex;
  align-items: flex-start;
  gap: 14px;
}

.plugin-icon {
  flex: none;
  width: 44px;
  height: 44px;
  display: grid;
  place-items: center;
  border-radius: 14px;
  color: #ffffff;
  font-size: 17px;
  font-weight: 700;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.08);
}

.plugin-info {
  flex: 1;
  min-width: 0;
}

.name-row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}

.name {
  font-size: 14px;
  font-weight: 650;
  color: var(--text);
}

.version {
  font-size: 11px;
  color: var(--muted);
}

.chip {
  padding: 2px 9px;
  border-radius: 999px;
  font-size: 10.5px;
  font-weight: 500;
}
.chip.ok {
  background: var(--success-soft);
  color: var(--success);
}
.chip.warn {
  background: var(--warn-soft);
  color: var(--warn);
}
.chip.danger {
  background: var(--danger-soft);
  color: var(--danger);
}
.chip.off {
  background: var(--surface-3);
  color: var(--muted);
}

/* ---- 卡片内操作 ---- */
.plugin-actions {
  flex: none;
  display: flex;
  align-items: center;
  gap: 10px;
}
.settings-btn {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  height: 30px;
  padding: 0 10px;
  border: 1px solid var(--line);
  border-radius: var(--r-btn);
  background: var(--surface-2);
  color: var(--text-2);
  font-size: 12px;
}
.settings-btn:hover {
  border-color: var(--accent);
  color: var(--accent);
}

/* ---- 插件设置弹窗 ---- */
.settings-form {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.settings-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 16px;
  padding: 10px 12px;
  border-radius: 10px;
  background: var(--surface-2);
}
.settings-copy {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.settings-copy > span {
  font-size: 13px;
  color: var(--text);
}
.settings-copy > small {
  font-size: 11px;
  color: var(--muted);
  font-family: ui-monospace, Consolas, monospace;
}
.settings-input,
.settings-select {
  width: 190px;
  flex: none;
}
.settings-error {
  margin: 12px 0 0;
  color: var(--danger);
  font-size: 12px;
}
.settings-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.settings-actions button {
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}

.plugin-desc {
  margin: 5px 0 0;
  font-size: 12px;
  color: var(--text-2);
}

.plugin-tags {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 9px;
}

.tag {
  padding: 2px 9px;
  border-radius: 999px;
  background: var(--accent-soft);
  color: var(--accent);
  font-size: 10.5px;
}
.tag.muted {
  background: var(--surface-3);
  color: var(--muted);
}

.plugin-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  margin-top: 12px;
  padding-top: 10px;
  border-top: 1px solid var(--line);
  font-size: 11px;
  color: var(--muted);
}
.hook-line {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.plugin-id {
  flex: none;
}

/* ---- 弹窗 ---- */
.modal-mask {
  position: fixed;
  inset: 0;
  z-index: 1000;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 20px;
  background: rgba(20, 40, 40, 0.28);
  backdrop-filter: blur(2px);
}

.modal {
  width: min(480px, 100%);
  padding: 20px 22px;
  border: 1px solid var(--line);
  border-radius: var(--r-outer);
  background: var(--surface);
  box-shadow: var(--shadow-float);
}

.modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}
.modal-head h2 {
  margin: 0;
  font-size: 16px;
  font-weight: 700;
}

.modal-body {
  margin: 10px 0 14px;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-2);
}
.modal-body code {
  padding: 1px 5px;
  border-radius: 5px;
  background: var(--surface-3);
  color: var(--text);
  font-size: 11.5px;
}

.perm-list {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.perm-list li {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 7px 11px;
  border-radius: 10px;
  background: var(--surface-2);
  font-size: 12px;
}
.perm-code {
  color: var(--accent);
  font-weight: 600;
}
.perm-label {
  color: var(--text-2);
}

.modal-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 16px;
}
.modal-actions button {
  height: 36px;
  padding: 0 16px;
  border-radius: var(--r-btn);
}
</style>
