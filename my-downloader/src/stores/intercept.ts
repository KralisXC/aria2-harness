import { ref } from "vue";
import { defineStore } from "pinia";

/**
 * 浏览器扩展投递、等待用户确认的下载任务（宿主 `PENDING_INTERCEPT` 的镜像）。
 *
 * 生命周期刻意与弹窗解耦：
 * - 宿主收到 `POST /add` 后立刻回 200 并存下这份数据，**不等待用户**；
 * - 前端恢复/重建时通过 `get_pending_intercept` 把它补进来；
 * - 只有用户点「开始下载」或「取消」，才会 `cancel_intercept` 清空宿主侧。
 *
 * 因此这里**没有**任何倒计时/过期逻辑：挂机多久都保持。
 */
export interface InterceptPayload {
  url: string;
  referer: string;
  cookie: string;
  user_agent: string;
  dir: string;
  /** 宿主接收时刻（Unix 毫秒），仅用于展示。 */
  received_at: number;
}

export const useInterceptStore = defineStore("intercept", () => {
  /** 当前待确认的浏览器任务；null = 无。 */
  const pending = ref<InterceptPayload | null>(null);

  function setPending(value: InterceptPayload | null) {
    pending.value = value;
  }

  function clear() {
    pending.value = null;
  }

  return { pending, setPending, clear };
});
