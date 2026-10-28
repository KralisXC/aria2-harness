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
   * beta10.1：非空表示这是插件接管的虚拟任务（HLS 等）。
   *
   * 虚拟任务会先映射成 Task 形状再进入统一下载列表，因此卡片、进度条、
   * 分栏与分页逻辑完全复用普通任务；这里只保存渲染所需的插件态元数据。
   */
  plugin?: PluginTaskInfo;
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
  /** 暂停原因：`"rate-limit"` = 被 403/429 限流降级暂停（beta8.3）。 */
  pauseReason?: string;
}

/** 插件接管的虚拟任务（beta10：HLS 等，与 aria2 GID 命名空间隔离）。 */
export interface PluginTask {
  id: string;
  pluginId: string;
  title: string;
  url: string;
  /** detecting / downloading / merging / complete / error / cancelled */
  phase: string;
  /** beta11 · FF-7：进度展示模式（缺省 progress；indeterminate = 直播录制等）。 */
  mode?: "progress" | "indeterminate";
  progress: number;
  detail: string;
  outputPath?: string;
  error?: string;
  createdAt: number;
  updatedAt: number;
}

/** 虚拟任务映射成 Task 后保留的渲染元数据（beta10.1）。 */
export interface PluginTaskInfo {
  phase: string;
  /** beta11 · FF-7：`indeterminate` 时卡片改用脉冲条而非百分比进度。 */
  mode: "progress" | "indeterminate";
  progress: number;
  detail: string;
  title: string;
  outputPath?: string;
  error?: string;
}

/** 插件 phase → 普通任务 status（决定卡片配色 / 进度条语义色）。 */
const PLUGIN_STATUS: Record<string, string> = {
  detecting: "active",
  downloading: "active",
  merging: "active",
  complete: "complete",
  error: "error",
  cancelled: "removed",
};

/**
 * 把插件虚拟任务转换成标准 Task。
 *
 * 进度用 0~100 的伪长度表示，`progress` 百分比因此与原逻辑完全兼容；
 * 文件名 / 路径 / 链接分别落到 files[0] 上，右键菜单与「打开文件夹」无需特判。
 */
export function pluginTaskToTask(task: PluginTask): Task {
  const percent = Math.min(100, Math.max(0, Number(task.progress) || 0));
  const output = task.outputPath?.trim() ?? "";
  const dir = output.includes("\\")
    ? output.slice(0, output.lastIndexOf("\\"))
    : output.slice(0, output.lastIndexOf("/"));
  return {
    gid: task.id,
    status: PLUGIN_STATUS[task.phase] ?? "active",
    totalLength: "100",
    completedLength: String(percent),
    downloadSpeed: "0",
    connections: "0",
    dir,
    files: [
      {
        index: "1",
        path: output || task.title,
        length: "100",
        completedLength: String(percent),
        uris: [{ uri: task.url }],
      },
    ],
    plugin: {
      phase: task.phase,
      mode: task.mode === "indeterminate" ? "indeterminate" : "progress",
      progress: percent,
      detail: task.detail,
      title: task.title,
      outputPath: task.outputPath,
      error: task.error,
    },
  };
}

/** 进度轮询间隔（毫秒）。 */
const POLL_INTERVAL = 1000;

export const useDownloadsStore = defineStore("downloads", () => {
  // 以 gid 为键的任务表。Vue 3 的 reactive(Map) 原生支持响应式，
  // 因此单条 upsert 只会触发依赖该条数据的视图更新，不会整表重渲染。
  const tasks = reactive(new Map<string, Task>());
  const connection = ref<"connecting" | "connected" | "disconnected">("connecting");
  /** 插件虚拟任务（宿主 `get_plugin_tasks` 快照 + `plugin://task` 增量事件）。 */
  const pluginTasks = ref<PluginTask[]>([]);

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

  /**
   * 统一下载列表：插件虚拟任务映射成 Task 后与引擎任务合并（beta10.1）。
   *
   * 虚拟任务排在最前（它们通常是最新发起的），其余保持引擎返回顺序。
   * 下载页只消费这个列表，不再有独立的「插件任务」区块。
   */
  const displayTasks = computed<Task[]>(() => [
    ...pluginTasks.value.map(pluginTaskToTask),
    ...list.value,
  ]);

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

  /** 全量拉取插件虚拟任务（挂载 / 重连时调用）。 */
  async function initPluginTasks() {
    try {
      pluginTasks.value = await invoke<PluginTask[]>("get_plugin_tasks");
    } catch {
      // 引擎/插件系统未就绪：保留现有快照，等事件到达。
    }
  }

  /** 单条虚拟任务增量更新（按 id 覆盖，新任务插入并按更新时间倒序）。 */
  function upsertPluginTask(task: PluginTask) {
    const index = pluginTasks.value.findIndex((item) => item.id === task.id);
    if (index >= 0) {
      pluginTasks.value[index] = task;
    } else {
      pluginTasks.value = [task, ...pluginTasks.value];
    }
    pluginTasks.value.sort((a, b) => b.updatedAt - a.updatedAt);
  }

  /** 取消插件虚拟任务（宿主会中断分片批量下载并通知插件清理）。 */
  async function cancelPluginTask(id: string) {
    await invoke("cancel_plugin_task", { id });
  }

  /** 从本地视图移除已结束的虚拟任务（宿主侧由 `remove_plugin_task` 清理）。 */
  function removePluginTask(id: string) {
    pluginTasks.value = pluginTasks.value.filter((item) => item.id !== id);
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
    // 插件虚拟任务的 id 不在 aria2 的 GID 命名空间里，get_task 会报 Invalid GID；
    // 它的状态只由 `plugin://task` 事件 / `get_plugin_tasks` 快照维护。
    if (gid.startsWith("plugin:")) return;
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
    displayTasks,
    connection,
    pluginTasks,
    initPluginTasks,
    upsertPluginTask,
    cancelPluginTask,
    removePluginTask,
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
