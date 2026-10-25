import { ref } from "vue";
import { defineStore } from "pinia";
import { errText, invoke } from "../ipc";

/** 插件设置项（manifest.settings schema + 当前值）。 */
export interface PluginSettingInfo {
  key: string;
  label: string;
  /** 控件类型：boolean / string / select。 */
  type: string;
  options: string[];
  default: unknown;
  value: unknown;
  /** 仅前端使用的草稿值（不参与持久化）。 */
  draft?: unknown;
}

/** 插件视图数据（来自 list_plugins 命令）。 */
export interface PluginInfo {
  id: string;
  name: string;
  version: string;
  description: string;
  enabled: boolean;
  priority: number;
  hooks: string[];
  permissions: string[];
  /** manifest 申请但尚未授权的权限（非空时启用需弹窗确认）。 */
  pendingPermissions: string[];
  /** 是否已被熔断（连续失败自动禁用）。 */
  circuitBroken: boolean;
  /** 设置 schema（空数组表示插件未声明设置）。 */
  settings: PluginSettingInfo[];
}

/** 能力令牌 -> 中文说明（让用户看懂"授权了什么"）。 */
const PERM_LABELS: Record<string, string> = {
  notify: "发送系统通知",
  "downloads:read": "读取下载任务信息",
  "fs:write:downloads": "在下载目录中创建/移动文件",
  "downloads:intercept": "拦截并改写下载链接",
};

/** 权限令牌转中文；未收录的令牌原样展示（兜底）。 */
export function permLabel(perm: string): string {
  return PERM_LABELS[perm] ?? perm;
}

/**
 * 插件状态 Store（增量 24 从 App.vue 抽出）。
 *
 * 抽出的动机：全局 `plugin://` 事件监听留守 shell，若插件状态留在视图内，
 * 切到"下载任务"页时视图被销毁 → 后台事件（热重载完成 / 熔断）到达时状态无处落。
 * 因此状态统一放在 Store：shell 监听事件 → 调 Store action 刷新；视图只读渲染。
 */
export const usePluginsStore = defineStore("plugins", () => {
  const plugins = ref<PluginInfo[]>([]);
  const loading = ref(false);
  const message = ref("");
  /** 权限确认弹窗的目标插件（非 null 即展示弹窗）。 */
  const grantTarget = ref<PluginInfo | null>(null);

  /** 从后端拉取插件列表（幂等）。 */
  async function load() {
    loading.value = true;
    try {
      plugins.value = await invoke<PluginInfo[]>("list_plugins");
    } catch (error) {
      message.value = `❌ 读取插件失败：${errText(error)}`;
    } finally {
      loading.value = false;
    }
  }

  /**
   * 切换启用状态；禁用为挂起语义（宿主不再向该插件派发事件）。
   * 传 `ev` 是为了后端拒绝时把复选框回弹到原状态。
   */
  async function toggle(p: PluginInfo, ev?: Event) {
    const next = !p.enabled;
    // 授权可解释性（P0-A 闭环）：启用且存在"待授权权限"时，先弹窗确认，不直接静默提权。
    if (next && p.pendingPermissions.length > 0) {
      revertCheckbox(ev, p.enabled);
      grantTarget.value = p;
      return;
    }
    try {
      await invoke("toggle_plugin", { id: p.id, enabled: next });
      message.value = `${p.name} 已${next ? "启用" : "禁用"}`;
    } catch (error) {
      message.value = `❌ 切换失败：${errText(error)}`;
      revertCheckbox(ev, p.enabled);
    } finally {
      // 后端可能拒绝并改写真实状态：统一以 list_plugins 的结果为准。
      await load();
    }
  }

  /** 后端拒绝切换时把复选框回弹到原状态（`:checked` 绑定不随点击变化，需手动回滚 DOM）。 */
  function revertCheckbox(ev: Event | undefined, prev: boolean) {
    const input = ev?.target as HTMLInputElement | null;
    if (input) input.checked = prev;
  }

  /** 弹窗"同意并启用"：强制刷新授权快照并启用。 */
  async function confirmGrant() {
    const p = grantTarget.value;
    if (!p) return;
    try {
      await invoke("grant_plugin_permissions", { id: p.id });
      message.value = `✅ ${p.name} 已确认授权并启用`;
      grantTarget.value = null;
      await load();
    } catch (error) {
      message.value = `❌ 授权失败：${errText(error)}`;
    }
  }

  /** 取消授权：关闭弹窗，插件保持未启用。 */
  function cancelGrant() {
    grantTarget.value = null;
  }

  /** 保存插件设置：后端校验 + 持久化 + 运行时热更新，成功后刷新列表。 */
  async function saveSettings(p: PluginInfo, values: Record<string, unknown>) {
    await invoke("update_plugin_settings", { id: p.id, values });
    message.value = `✅ ${p.name} 的设置已保存`;
    await load();
  }

  /** 由 shell 的事件监听写入提示（熔断 / 热重载）。 */
  function notify(text: string) {
    message.value = text;
  }

  return {
    plugins,
    loading,
    message,
    grantTarget,
    load,
    toggle,
    confirmGrant,
    cancelGrant,
    saveSettings,
    notify,
  };
});
