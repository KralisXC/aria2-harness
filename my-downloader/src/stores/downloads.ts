import { computed, reactive, ref } from "vue";
import { defineStore } from "pinia";
import { invoke } from "../ipc";

export interface TaskFile {
  index: string;
  path: string;
  length: string;
  completedLength: string;
  /** 下载源（aria2 `files[].uris[]`）：右键「复制下载链接」与 BT 多源回退使用。 */
  uris?: { uri: string; status?: string }[];
}

export interface Task {
  gid: string;
  status: string;
  totalLength: string;
  completedLength: string;
  downloadSpeed: string;
  connections: string;
  errorCode?: string;
  errorMessage?: string;
  dir: string;
  files?: TaskFile[];
  /**
   * 宿主是否把这单任务降级为单连接（命中防盗链 / 老旧 CDN 规则）。
   * aria2 的 `tellStatus` 没有这个语义，由 Rust 侧按 GID 记账后注入。
   */
  isSingleThread?: boolean;
  /**
   * 宿主是否对该任务执行过并发自动降级（服务器 403/429 等拒绝后按档位重试）。
   * 记录按 GID 保留到任务被删除，所以降到 1 连接后成功完成也仍会为 true。
   */
  isDegraded?: boolean;
  /** 降级后的当前并发档位（split 与单服连接数同档），供标签展示。 */
  currentConcurrency?: number;
  /** 分片完成百分比（0~100，最多 256 段）；空数组表示引擎暂未上报。 */
  pieceProgress?: number[];
  /** 当前真正在传数据的 worker 数（并发连接数快照）。 */
  activeWorkers?: number;
  /** 剩余秒数（后端按 `remaining / speed` 估算，0 表示无法估算）。 */
  eta?: string;
}

/** 进度轮询间隔（毫秒）。 */
const POLL_INTERVAL = 1000;

export const useDownloadsStore = defineStore("downloads", () => {
  // 以 gid 为键的任务表。Vue 3 的 reactive(Map) 原生支持响应式，
  // 因此单条 upsert 只会触发依赖该条数据的视图更新，不会整表重渲染。
  const tasks = reactive(new Map<string, Task>());
  const connection = ref<"connecting" | "connected" | "disconnected">("connecting");

  /**
   * 新建任务弹窗的触发器：每次自增即请求打开一次。
   * 侧栏「+」与全局快捷键只需调用 `openCompose()`，弹窗归属下载页统一管理。
   */
  const composeNonce = ref(0);

  /** 待预填的下载地址（拖拽 http/magnet 时由 App.vue 写入，打开弹窗后消费一次即清空）。 */
  const composeSeed = ref("");

  /** 请求打开「新建任务」弹窗（值变化即触发，下载页 watch 它）。 */
  function openCompose(seed = "") {
    composeSeed.value = seed;
    composeNonce.value += 1;
  }

  /** 内部任务文件名特征（Tracker 自拉）：前端兜底过滤，绝不展示给用户。 */
  const INTERNAL_FILE_MARKERS = ["aria2-trackers"];

  function isInternalTask(task: Task): boolean {
    return (task.files ?? []).some((file) =>
      INTERNAL_FILE_MARKERS.some((marker) => (file.path ?? "").includes(marker)),
    );
  }

  /** 模板遍历用的数组视图（由 Map 派生；过滤内部任务）。 */
  const list = computed(() => Array.from(tasks.values()).filter((task) => !isInternalTask(task)));

  /** 插入或覆盖单条任务。 */
  function upsert(task: Task) {
    tasks.set(task.gid, task);
    // 有任务进入 active：确保轮询器在跑（空闲时它已自行停表，见 `startPolling`）。
    // initAll 也走这里，因此启动时有活跃任务同样会自动恢复轮询。
    if (task.status === "active") startPolling();
  }

  /** 从表中移除单条任务。 */
  function remove(gid: string) {
    tasks.delete(gid);
  }

  /** 全量初始化：组件挂载 / WS 连接成功时调用，解决"挂载早于引擎加载 session"的时序问题。 */
  async function initAll() {
    const all = await invoke<Task[]>("get_tasks");
    tasks.clear();
    for (const task of all) upsert(task);
  }

  /**
   * 强制全量刷新（焦点恢复 / 后台切前台）。
   *
   * 绕过 1s 轮询节流直接拉一次完整快照并覆盖本地状态，失败时静默保留旧数据 ——
   * 引擎未就绪不该把已展示的任务清空。
   */
  async function refreshNow() {
    try {
      const all = await invoke<Task[]>("get_tasks");
      tasks.clear();
      for (const task of all) upsert(task);
    } catch {
      // 引擎暂不可用：保留当前快照，等 WS 恢复后再覆盖。
    }
  }

  /** 单任务增量更新：由 WS 状态事件（start/pause/stop/complete/error）触发。 */
  async function updateOne(gid: string) {
    const task = await invoke<Task>("get_task", { gid });
    upsert(task);
  }

  /** 拉取活跃任务并 upsert（进度更新）。 */
  async function pollActive() {
    const active = await invoke<Task[]>("get_active_tasks");
    for (const task of active) upsert(task);
  }

  /** 表中是否存在 active 状态的任务。 */
  function hasActive(): boolean {
    for (const task of tasks.values()) {
      if (task.status === "active") return true;
    }
    return false;
  }

  // 轮询器：只在有 active 任务时存在。无任务时下一 tick 直接 `clearInterval` 停表，
  // 空闲期一次 IPC 都不发（不是"tick 里跳过请求"，那是空转）。
  let timer: ReturnType<typeof setInterval> | null = null;

  function startPolling() {
    if (timer !== null) return;
    timer = setInterval(() => {
      if (!hasActive()) {
        stopPolling();
        return;
      }
      void pollActive().catch(() => {
        // 引擎未就绪时静默失败，下一轮再试
      });
    }, POLL_INTERVAL);
  }

  function stopPolling() {
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  return {
    tasks,
    list,
    connection,
    composeNonce,
    composeSeed,
    openCompose,
    upsert,
    remove,
    initAll,
    refreshNow,
    updateOne,
    pollActive,
    hasActive,
    startPolling,
    stopPolling,
  };
});
