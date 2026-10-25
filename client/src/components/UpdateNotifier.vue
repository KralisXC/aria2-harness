<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";
import { NModal } from "naive-ui";
import { errText, invoke, listen, type UnlistenFn } from "../ipc";

/**
 * 自动更新提示（beta7：由通知条改为「更新确认弹窗」）。
 *
 * 用户红线：检测到新版本时**绝不直接强杀进程**。点「确定」后由宿主
 * `quit_for_update` 完成：先暂停所有任务 → 打开下载页 → 走完整退出流程
 * （saveSession → 引擎 shutdown → 通知服务收尾）。点「取消」只关弹窗，
 * 正在下载的任务完全不受影响。
 *
 * 放在独立组件而不是 `App.vue` 的 setup 里：弹窗需要挂在 NConfigProvider /
 * NNotificationProvider 之下（主题与层级都依赖这层 provide）。
 */

interface UpdatePayload {
  version?: string;
  url?: string;
  changelog?: string;
  /** 当前版本（宿主回传，便于文案里对照）。 */
  current?: string;
}

/** 与 Rust 侧 `update_check::EVENT_UPDATE_AVAILABLE` 完全一致。 */
const EVENT = "app://update-available";

const open = ref(false);
const busy = ref(false);
const version = ref("");
const url = ref("");
const changelog = ref("");
const current = ref("");
const error = ref("");

let unlisten: UnlistenFn | null = null;
/** 本次运行是否已经提示过（宿主每次进程只检查一次，这里再兜一层防重复）。 */
let shown = false;

function present(payload: UpdatePayload) {
  if (shown || !payload?.version || !payload.url) return;
  shown = true;
  version.value = payload.version;
  url.value = payload.url;
  changelog.value = payload.changelog?.trim() ?? "";
  current.value = payload.current?.trim() ?? "";
  busy.value = false;
  error.value = "";
  open.value = true;
}

/** 关闭请求统一走这里：退出过程中不允许被遮罩 / Esc 打断。 */
function onShowChange(show: boolean) {
  if (show) {
    open.value = true;
    return;
  }
  if (busy.value) return;
  open.value = false;
}

/** 「确定」：交给宿主安全退出（暂停任务 → 保存会话 → 拉起安装包）。 */
async function confirm() {
  if (busy.value) return;
  busy.value = true;
  error.value = "";
  try {
    await invoke("quit_for_update", { url: url.value });
    // 宿主会在回传后立即退出进程：保持弹窗停留在"正在安全退出…"。
  } catch (e) {
    busy.value = false;
    error.value = `安全退出失败：${errText(e)}`;
  }
}

/** 「取消」：只关提示，不动任何下载任务。 */
function cancel() {
  if (busy.value) return;
  open.value = false;
}

onMounted(async () => {
  // 宿主在启动 5s 后检查，窗口若那时尚未建好也不会丢：事件订阅常驻，
  // 窗口销毁重建时本组件会随 Vue 一起重挂，宿主只在检查成功时发一次。
  unlisten = await listen<UpdatePayload>(EVENT, (event) => present(event.payload));
});

onBeforeUnmount(() => {
  unlisten?.();
});
</script>

<template>
  <n-modal
    :show="open"
    preset="dialog"
    class="update-modal"
    title="发现新版本"
    style="width: 460px; max-width: 92vw"
    :mask-closable="false"
    :close-on-esc="false"
    :closable="false"
    @update:show="onShowChange"
  >
    <p class="update-text">
      检测到新版本 v{{ version }}，更新需要重启软件，是否继续？
    </p>
    <p v-if="changelog" class="update-changelog">{{ changelog }}</p>
    <p class="update-hint">
      确定后会先暂停任务并保存进度，再退出软件并打开下载页；取消则继续当前下载。
    </p>
    <p v-if="current" class="update-hint">当前版本 v{{ current }}</p>
    <p v-if="error" class="update-error">{{ error }}</p>

    <template #action>
      <button class="btn-ghost" type="button" :disabled="busy" @click="cancel">
        取消
      </button>
      <button class="btn-primary" type="button" :disabled="busy" @click="confirm">
        {{ busy ? "正在安全退出…" : "确定" }}
      </button>
    </template>
  </n-modal>
</template>

<style scoped>
.update-text {
  margin: 0 0 8px;
  font-size: 13.5px;
  font-weight: 600;
  line-height: 1.6;
  color: var(--text);
}

.update-changelog {
  margin: 0 0 8px;
  font-size: 12px;
  line-height: 1.6;
  color: var(--text-2);
}

.update-hint {
  margin: 0;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--muted);
}

.update-error {
  margin: 8px 0 0;
  font-size: 11.5px;
  line-height: 1.6;
  color: var(--danger);
}
</style>
