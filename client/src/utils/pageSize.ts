/**
 * 下载列表每页条数（localStorage 持久化，beta10.1）。
 *
 * 设置页「每页显示数量」与下载页分页器共用同一个键：
 * 默认 20，只接受 10 / 20 / 50 三个合法值。
 */
export const PAGE_SIZE_KEY = "aria2-harness-downloads-page-size";

export const PAGE_SIZE_OPTIONS = [10, 20, 50] as const;
export const DEFAULT_PAGE_SIZE = 20;

/** 读取每页条数；未设置 / 数据损坏 / 存储不可用时回退默认 20。 */
export function readPageSize(): number {
  try {
    const value = Number(localStorage.getItem(PAGE_SIZE_KEY));
    return (PAGE_SIZE_OPTIONS as readonly number[]).includes(value) ? value : DEFAULT_PAGE_SIZE;
  } catch {
    return DEFAULT_PAGE_SIZE;
  }
}

/** 写入每页条数；非法值直接忽略。 */
export function writePageSize(value: number): void {
  if (!(PAGE_SIZE_OPTIONS as readonly number[]).includes(value)) return;
  try {
    localStorage.setItem(PAGE_SIZE_KEY, String(value));
  } catch {
    // 隐私模式 / 配额满：降级为「本次会话有效」。
  }
}

/** 把设置页下拉框的数值切换回传给下载页（storage 事件只跨窗口，不跨组件）。 */
export const PAGE_SIZE_EVENT = "aria2-harness-page-size-change";
