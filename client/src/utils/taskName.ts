/**
 * 任务显示名解析。
 *
 * aria2 在 `addUri` 返回 GID 的那一刻还没有解析文件名：HTTP 任务要等
 * `Event::Start`，BT 任务要等元数据下载完成，`files[].path` 之前都是空串。
 * 前端若直接回退到 `gid`，用户就会看到"已添加任务：4adb426d43f908f8"这种鬼东西。
 *
 * 统一规则：真实文件名 → 下载地址推导出的临时名 → 「解析文件名中...」。
 * GID 永远只作为内部键，不参与任何用户可见文案。
 */

/** 文件名尚未解析出来时的统一占位文案。 */
export const PENDING_TASK_NAME = "解析文件名中...";

/** 无任务信息时的兜底名称（通知栏里的历史事件可能指向已删除任务）。 */
export const UNKNOWN_TASK_NAME = "未知文件";

/** 判断名称所需的最小字段集合（下载页 `Task` 与历史行都可直接传入）。 */
export interface NameSource {
  gid?: string;
  files?: { path?: string; uris?: { uri?: string }[] }[];
}

function safeDecode(text: string): string {
  try {
    return decodeURIComponent(text);
  } catch {
    return text;
  }
}

/**
 * 从下载地址推导临时文件名。
 *
 * - HTTP(S)：取路径最后一段，丢弃 query / hash；
 * - magnet：取 `dn=` 参数（种子自带名字，元数据未到也能显示）。
 */
export function nameFromUri(uri: string | undefined): string {
  const raw = (uri ?? "").trim();
  if (!raw) return "";
  if (/^magnet:/i.test(raw)) {
    const match = /[?&]dn=([^&]+)/i.exec(raw);
    return match ? safeDecode(match[1]).trim() : "";
  }
  const tail = raw.split(/[?#]/)[0].split(/[\\/]/).pop() ?? "";
  return safeDecode(tail).trim();
}

/** 任务显示名：真实文件名 → URL 尾段 → 解析中占位（绝不返回 GID）。 */
export function taskDisplayName(task: NameSource | undefined | null): string {
  if (!task) return UNKNOWN_TASK_NAME;
  const full = (task.files?.[0]?.path ?? "").trim();
  const base = full ? (full.split(/[\\/]/).pop() ?? "").trim() : "";
  if (base && base !== task.gid) return base;
  const uri = task.files?.[0]?.uris?.find((item) => (item.uri ?? "").trim())?.uri;
  return nameFromUri(uri) || PENDING_TASK_NAME;
}
