import { ref } from "vue";
import { defineStore } from "pinia";
import { errText, invoke } from "../ipc";

/** 插件设置项（manifest.settings schema + 当前值）。 */
export interface PluginSettingInfo {
  key: string;
  label: string;
  /** 控件类型：boolean / string / textarea / select。 */
  type: string;
  options: string[];
  default: unknown;
  value: unknown;
  /** 仅前端使用的草稿值（不参与持久化）。 */
  draft?: unknown;
}

/** 插件右键动作（已完成任务上的动态菜单项）。 */
export interface PluginActionInfo {
  id: string;
  label: string;
}

/** 插件视图数据（来自 list_plugins 命令）。 */
export interface PluginInfo {
  id: string;
  name: string;
  version: string;
  description: string;
  /** 作者（manifest.author，缺省为空串）。 */
  author: string;
  /** 图标 data URI；无图标时为 null（前端回退首字母色块）。 */
  icon: string | null;
  /** 入口脚本（相对插件目录）。 */
  entry: string;
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
  /** manifest 声明的右键动作（无动作时为空数组）。 */
  actions: PluginActionInfo[];
}

/** 共享组件数据（来自 list_shared_plugins 命令，目录 data/plugins/.shared/<id>）。 */
export interface SharedInfo {
  id: string;
  name: string;
  version: string;
  /** 描述（manifest.description，缺省为空串）。 */
  description: string;
  /** 作者（manifest.author，缺省为空串）。 */
  author: string;
  /** 共享包目录（绝对路径）。 */
  dir: string;
  /** 目录内文件数（宿主统计）。 */
  fileCount: number;
}

/** 单条插件日志（get_plugin_logs / plugin://log 共用）。 */
export interface PluginLogEntry {
  /** Unix 时间戳（秒；兼容毫秒由展示层归一化）。 */
  ts: number;
  level: "debug" | "info" | "warn" | "error";
  msg: string;
}

/** 批量操作的逐条结果（不新增批量后端命令，前端聚合）。 */
export interface BatchOutcome {
  /** 成功条数。 */
  ok: number;
  /** 失败项名称 / id。 */
  failed: string[];
  /** 被跳过的项（如缺少授权，需单独启用）。 */
  skipped: string[];
}

/** 能力令牌 -> 中文说明（让用户看懂"授权了什么"）。 */
const PERM_LABELS: Record<string, string> = {
  notify: "发送系统通知",
  "downloads:read": "读取下载任务信息",
  "downloads:write": "修改下载任务状态",
  "fs:write:downloads": "在下载目录中创建/移动文件",
  "downloads:intercept": "拦截并改写下载链接",
  "net:http": "发起 HTTP 网络请求",
  "exec:ffmpeg": "调用 FFmpeg 处理文件",
  "exec:local": "执行插件自带的可执行文件",
};

/** 权限令牌转中文；未收录的令牌原样展示（兜底）。 */
export function permLabel(perm: string): string {
  return PERM_LABELS[perm] ?? perm;
}

/**
 * 安装安全确认：待确认插件的提示文案（非 null 即展示红色安全警告弹窗）。
 *
 * 放在模块级而非 Store 内部：安装流程（pluginInstall.ts）与宿主拒绝后的重试
 * 都不在 Store 实例内，模块级 Promise 最直接，也无需为一次性确认引入队列。
 */
export const consentTarget = ref<string | null>(null);

/** 待确认请求的结算函数（同一时刻只保留一个）。 */
let consentResolver: ((ok: boolean) => void) | null = null;

/**
 * 请求用户确认安装含 `exec:local` 权限的插件，返回用户是否信任。
 *
 * 同一时刻已有待确认请求时，先以「拒绝」结算旧的，避免 Promise 悬挂。
 */
export function requestExecConsent(message: string): Promise<boolean> {
  if (consentResolver) {
    consentResolver(false);
    consentResolver = null;
  }
  consentTarget.value = message;
  return new Promise<boolean>((resolve) => {
    consentResolver = resolve;
  });
}

/** 结算当前确认请求（true = 信任并安装，false = 取消）；无待确认时安全空操作。 */
export function resolveExecConsent(ok: boolean) {
  const resolve = consentResolver;
  consentResolver = null;
  consentTarget.value = null;
  resolve?.(ok);
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
  /** 共享组件列表（beta12.2：.shared/<id>）。 */
  const shared = ref<SharedInfo[]>([]);
  const loading = ref(false);
  const sharedLoading = ref(false);
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

  /** 从后端拉取共享组件列表（beta12.2，幂等）。 */
  async function loadShared() {
    sharedLoading.value = true;
    try {
      shared.value = await invoke<SharedInfo[]>("list_shared_plugins");
    } catch (error) {
      message.value = `❌ 读取共享组件失败：${errText(error)}`;
    } finally {
      sharedLoading.value = false;
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

  /** 卸载单个插件（后端会停止实例、删除目录与配置）。 */
  async function uninstall(id: string) {
    const name = plugins.value.find((p) => p.id === id)?.name ?? id;
    await invoke("uninstall_plugin", { id });
    message.value = `✅ 已卸载 ${name}`;
    await load();
  }

  /** 卸载共享组件（后端删除 .shared/<id> 目录），成功后刷新共享与插件列表。 */
  async function uninstallShared(id: string) {
    const name = shared.value.find((s) => s.id === id)?.name ?? id;
    await invoke("uninstall_shared_plugin", { id });
    message.value = `✅ 已卸载共享组件 ${name}`;
    await Promise.all([loadShared(), load()]);
  }

  /**
   * 批量启停：前端逐条调用既有 toggle_plugin，失败项汇总。
   * 启用时遇到"待授权"插件直接跳过，避免批量操作静默提权。
   */
  async function batchToggle(ids: string[], enabled: boolean): Promise<BatchOutcome> {
    const outcome: BatchOutcome = { ok: 0, failed: [], skipped: [] };
    for (const id of ids) {
      const p = plugins.value.find((item) => item.id === id);
      if (!p) continue;
      if (p.enabled === enabled) {
        outcome.ok += 1;
        continue;
      }
      if (enabled && p.pendingPermissions.length > 0) {
        outcome.skipped.push(p.name);
        continue;
      }
      try {
        await invoke("toggle_plugin", { id, enabled });
        outcome.ok += 1;
      } catch {
        outcome.failed.push(p.name);
      }
    }
    const verb = enabled ? "启用" : "禁用";
    const parts = [`✅ 已${verb} ${outcome.ok} 个插件`];
    if (outcome.skipped.length > 0) {
      parts.push(`跳过 ${outcome.skipped.length} 个（缺少授权，请单独启用）`);
    }
    if (outcome.failed.length > 0) {
      parts.push(`失败 ${outcome.failed.length} 个：${outcome.failed.join("、")}`);
    }
    message.value = parts.join("，");
    await load();
    return outcome;
  }

  /** 批量卸载：前端逐条调用 uninstall_plugin，失败项汇总。 */
  async function batchUninstall(ids: string[]): Promise<BatchOutcome> {
    const outcome: BatchOutcome = { ok: 0, failed: [], skipped: [] };
    for (const id of ids) {
      const name = plugins.value.find((item) => item.id === id)?.name ?? id;
      try {
        await invoke("uninstall_plugin", { id });
        outcome.ok += 1;
      } catch {
        outcome.failed.push(name);
      }
    }
    const parts = [`✅ 已卸载 ${outcome.ok} 个插件`];
    if (outcome.failed.length > 0) {
      parts.push(`失败 ${outcome.failed.length} 个：${outcome.failed.join("、")}`);
    }
    message.value = parts.join("，");
    await load();
    return outcome;
  }

  /** 读取插件日志历史（默认最近 500 条）。 */
  async function loadLogs(id: string, limit = 500): Promise<PluginLogEntry[]> {
    return invoke<PluginLogEntry[]>("get_plugin_logs", { id, limit });
  }

  /** 清空指定插件的日志缓冲。 */
  async function clearLogs(id: string) {
    await invoke("clear_plugin_logs", { id });
  }

  /** 由 shell 的事件监听写入提示（熔断 / 热重载）。 */
  function notify(text: string) {
    message.value = text;
  }

  return {
    plugins,
    shared,
    loading,
    sharedLoading,
    message,
    grantTarget,
    /** 安装安全确认目标（模块级 ref，Store 实例上自动解包为 string | null）。 */
    consentTarget,
    load,
    loadShared,
    toggle,
    confirmGrant,
    cancelGrant,
    saveSettings,
    uninstall,
    uninstallShared,
    batchToggle,
    batchUninstall,
    loadLogs,
    clearLogs,
    notify,
  };
});
