/**
 * 删除任务时的行为偏好（localStorage 持久化）。
 *
 * - `ask`：每次弹出确认框；
 * - `task-only`：只移除任务记录，保留本地文件；
 * - `task-and-file`：移除任务并删除本地文件。
 *
 * 设置页「删除任务时」与下载页的确认弹窗共用同一个键，保证两处语义一致。
 */
export type DeleteBehavior = "ask" | "task-only" | "task-and-file";

export const DELETE_BEHAVIOR_KEY = "aria2-harness-delete-behavior";

/** 读取偏好；未设置 / 数据损坏 / 存储不可用时一律回退到「每次询问」。 */
export function readDeleteBehavior(): DeleteBehavior {
  try {
    const raw = localStorage.getItem(DELETE_BEHAVIOR_KEY);
    if (raw === "task-only" || raw === "task-and-file") return raw;
    return "ask";
  } catch {
    return "ask";
  }
}

/** 写入偏好；隐私模式 / 配额满时静默降级为「不记忆」，不影响删除主链路。 */
export function writeDeleteBehavior(value: DeleteBehavior): void {
  try {
    localStorage.setItem(DELETE_BEHAVIOR_KEY, value);
  } catch {
    // 忽略。
  }
}
