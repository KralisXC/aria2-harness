import { ref, watch } from "vue";
import { defineStore } from "pinia";
import { errText, invoke } from "../ipc";
import { useDownloadsStore } from "./downloads";

/** aria2.getGlobalStat（所有字段均为字符串）。 */
export interface GlobalStat {
  downloadSpeed: string;
  uploadSpeed: string;
  numActive: string;
  numWaiting: string;
  numStopped: string;
}

/** 历史库行（db::TaskRecord 的 camelCase 序列化）。 */
export interface HistoryTask {
  gid: string;
  url: string;
  dir: string;
  status: string;
  completedLength: number;
  totalLength: number;
  downloadSpeed: number;
  errorCode: number | null;
  errorMessage: string | null;
  createdAt: number;
  updatedAt: number;
  finishedAt: number | null;
}

export interface TaskHistoryPage {
  items: HistoryTask[];
  total: number;
}

/** Tracker 快照（tracker::snapshot）。 */
export interface TrackerSnapshot {
  items: string[];
  count: number;
  lastUpdate: number | null;
  source: string;
  injecting: boolean;
}

/**
 * 全局系统数据：上下行速度、历史任务、Tracker 列表。
 *
 * 与 `downloads` store 分离：这些是低频/全局数据，不参与任务级增量更新。
 */
export const useSystemStore = defineStore("system", () => {
  const globalStat = ref<GlobalStat | null>(null);
  const history = ref<HistoryTask[]>([]);
  const historyTotal = ref(0);
  const historyLoading = ref(false);
  const historyError = ref("");

  const trackers = ref<TrackerSnapshot | null>(null);
  const trackerLoading = ref(false);
  const trackerRefreshing = ref(false);
  const trackerError = ref("");

  const downloads = useDownloadsStore();
  let timer: ReturnType<typeof setInterval> | null = null;

  async function refreshGlobalStat() {
    try {
      globalStat.value = await invoke<GlobalStat>("get_global_stat");
    } catch {
      // 引擎未就绪时保留上一帧，下一轮再试。
    }
  }

  function startStatPolling() {
    if (timer !== null) return;
    void refreshGlobalStat();
    timer = setInterval(() => {
      if (!downloads.hasActive()) {
        // 无活跃任务：补一帧让速度归零，随后真正停表 —— 空闲期 IPC 彻底静默。
        void refreshGlobalStat();
        stopStatPolling();
        return;
      }
      void refreshGlobalStat();
    }, 1000);
  }

  function stopStatPolling() {
    if (timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  }

  async function loadHistory(limit = 400) {
    historyLoading.value = true;
    historyError.value = "";
    try {
      const page = await invoke<TaskHistoryPage>("get_history_tasks", {
        limit,
        offset: 0,
      });
      history.value = page.items;
      historyTotal.value = page.total;
    } catch (error) {
      historyError.value = errText(error);
    } finally {
      historyLoading.value = false;
    }
  }

  async function loadTrackers() {
    trackerLoading.value = true;
    trackerError.value = "";
    try {
      trackers.value = await invoke<TrackerSnapshot>("get_trackers");
    } catch (error) {
      trackerError.value = errText(error);
    } finally {
      trackerLoading.value = false;
    }
  }

  async function refreshTrackers() {
    trackerRefreshing.value = true;
    trackerError.value = "";
    try {
      trackers.value = await invoke<TrackerSnapshot>("refresh_trackers");
    } catch (error) {
      trackerError.value = errText(error);
    } finally {
      trackerRefreshing.value = false;
    }
  }

  // 任务变为活跃时恢复全局速度轮询。停表由 tick 自己负责（见 `startStatPolling`），
  // 这里只负责"重新开始"，避免每个 tick 都起停定时器。
  watch(
    () => downloads.list.some((task) => task.status === "active"),
    (active) => {
      if (active) startStatPolling();
    },
  );

  return {
    globalStat,
    history,
    historyTotal,
    historyLoading,
    historyError,
    trackers,
    trackerLoading,
    trackerRefreshing,
    trackerError,
    startStatPolling,
    stopStatPolling,
    refreshGlobalStat,
    loadHistory,
    loadTrackers,
    refreshTrackers,
  };
});
