// 插件安装前端公共逻辑（beta12.1）：拖拽入口（App.vue）与导入按钮（PluginsView）
// 共用同一套批量结果解析与提示，避免两处逻辑分叉。
//
// 后端契约：`install_plugin_zip` / `install_plugin_dir` 返回
// `{ installed: InstallOutcome[], failed: InstallFailure[] }`；全部失败时才抛错。

import { IpcError, errText, invoke } from "../ipc";
import { useNotificationsStore } from "../stores/notifications";
import { requestExecConsent, usePluginsStore } from "../stores/plugins";

/** 单个插件安装成功项。 */
export interface InstallOutcome {
  id: string;
  name: string;
  version: string;
  dir: string;
  replaced: boolean;
  /** beta12.2：安装类型（普通插件 / 共享组件）。 */
  kind: "plugin" | "shared";
}

/** 单个插件安装失败项（批量安装时其余插件继续）。 */
export interface InstallFailure {
  plugin: string;
  error: string;
}

/** 批量安装结果。 */
export interface InstallBatchOutcome {
  installed: InstallOutcome[];
  failed: InstallFailure[];
}

/** 文件夹安装通道的逐文件载荷：相对路径 + base64 内容。 */
export interface InstallFilePayload {
  path: string;
  data: string;
}

/**
 * 把 File / Blob 读成 base64。
 *
 * 分块拼接 binary 字符串：大文件一次性 `String.fromCharCode(...bytes)` 会触发
 * 参数数量上限（RangeError）。
 */
export function readAsBase64(file: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const bytes = new Uint8Array(reader.result as ArrayBuffer);
      const chunk = 0x8000;
      let binary = "";
      for (let offset = 0; offset < bytes.length; offset += chunk) {
        binary += String.fromCharCode(...bytes.subarray(offset, offset + chunk));
      }
      resolve(btoa(binary));
    };
    reader.onerror = () => reject(reader.error ?? new Error("读取文件失败"));
    reader.readAsArrayBuffer(file);
  });
}

/**
 * 安装结果提示：刷新插件 / 共享组件列表，并**只推一条**合并通知（beta12.2）。
 *
 * - 全部成功：`成功安装 X 个插件`（含共享组件时 `成功安装 X 个插件、Y 个共享组件`）；
 * - 有失败：同一行追加 `，Z 个失败：名称：原因；…`（最多列 3 条，超出用「等」）；
 * - 等级：无失败 success；部分失败 warn；全失败 error。
 */
export async function reportInstallOutcome(
  outcome: InstallBatchOutcome,
  source: string,
) {
  const pluginsStore = usePluginsStore();
  const notices = useNotificationsStore();
  await pluginsStore.load().catch(() => undefined);
  // 批量包内可能含共享组件：同步刷新 .shared 列表（失败不影响安装结果提示）。
  await pluginsStore.loadShared().catch(() => undefined);

  const installed = outcome.installed;
  const failed = outcome.failed;
  if (installed.length === 0 && failed.length === 0) return;

  const sharedCount = installed.filter((item) => item.kind === "shared").length;
  const pluginCount = installed.length - sharedCount;

  const parts: string[] = [];
  if (installed.length > 0) {
    parts.push(
      sharedCount > 0
        ? `成功安装 ${pluginCount} 个插件、${sharedCount} 个共享组件`
        : `成功安装 ${pluginCount} 个插件`,
    );
  }
  if (failed.length > 0) {
    const detail = failed
      .slice(0, 3)
      .map((item) => `${item.plugin}：${item.error}`)
      .join("；");
    parts.push(
      `${failed.length} 个失败：${detail}${failed.length > 3 ? "；等" : ""}`,
    );
  }

  const kind =
    failed.length === 0 ? "success" : installed.length === 0 ? "error" : "warn";
  const installedNames = installed
    .map((item) => `${item.name || item.id} v${item.version}`)
    .join("、");
  const desc = installedNames ? `${installedNames} · 来自 ${source}` : `来自 ${source}`;

  notices.push(kind, parts.join("，"), desc);
}

/**
 * 带安全确认的安装调用（v1.0.0-rc1）。
 *
 * 宿主在移动任何目录前就会因 `exec:local` 权限返回 `PLUGIN_CONSENT_REQUIRED`
 * （staging 已清理），因此这里只需：弹窗询问 → 拒绝即停止；同意则带
 * `allowExecLocal: true` 重试**一次**。重试不再弹窗，避免死循环。
 */
async function invokeInstall(
  cmd: "install_plugin_zip" | "install_plugin_dir",
  args: Record<string, unknown>,
  source: string,
): Promise<InstallBatchOutcome | null> {
  try {
    return await invoke<InstallBatchOutcome>(cmd, args);
  } catch (error) {
    if (!(error instanceof IpcError) || error.code !== "PLUGIN_CONSENT_REQUIRED") {
      throw error;
    }
    const ok = await requestExecConsent(errText(error));
    if (!ok) {
      useNotificationsStore().push("warn", "已取消安装", source);
      return null;
    }
    return invoke<InstallBatchOutcome>(cmd, { ...args, allowExecLocal: true });
  }
}

/** 安装单个 .zip 插件包（包内可含多个插件）。 */
export async function installPluginZip(file: File) {
  try {
    const data = await readAsBase64(file);
    const outcome = await invokeInstall("install_plugin_zip", { data }, file.name);
    if (!outcome) return null;
    await reportInstallOutcome(outcome, file.name);
    return outcome;
  } catch (error) {
    useNotificationsStore().push(
      "error",
      "插件安装失败",
      `${file.name}：${errText(error)}`,
    );
    return null;
  }
}

/** 安装一组文件夹文件（支持一个文件夹里放多个插件目录）。 */
export async function installPluginFiles(
  files: InstallFilePayload[],
  source: string,
) {
  try {
    const outcome = await invokeInstall("install_plugin_dir", { files }, source);
    if (!outcome) return null;
    await reportInstallOutcome(outcome, source);
    return outcome;
  } catch (error) {
    useNotificationsStore().push(
      "error",
      "插件安装失败",
      `${source}：${errText(error)}`,
    );
    return null;
  }
}
