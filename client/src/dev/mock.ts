/**
 * 浏览器（Vite dev，无 `window.ipc`）下的静态数据 mock。
 *
 * 目的：纯前端渲染/截图验证 UI。仅在 `import.meta.env.DEV` 且宿主 IPC 缺失时启用，
 * 生产（WebView2）路径完全不受影响。数据形状与 Rust 侧真实返回严格一致。
 */

interface MockFile {
  index: string;
  path: string;
  length: string;
  completedLength: string;
  uris?: { uri: string; status?: string }[];
}

interface MockTask {
  gid: string;
  status: string;
  totalLength: string;
  completedLength: string;
  downloadSpeed: string;
  connections: string;
  errorCode?: string;
  errorMessage?: string;
  dir: string;
  files: MockFile[];
  /** 每分片完成百分比（与 Rust `tellStatus.pieceProgress` 对齐）。 */
  pieceProgress?: number[];
  /** 活跃 worker 数（与 Rust `tellStatus.activeWorkers` 对齐）。 */
  activeWorkers?: string;
  /** 预计剩余秒数（与 Rust `tellStatus.eta` 对齐）。 */
  eta?: string;
}

interface MockSetting {
  key: string;
  label: string;
  type: string;
  options: string[];
  default: unknown;
  value: unknown;
}

interface MockPlugin {
  id: string;
  name: string;
  version: string;
  description: string;
  enabled: boolean;
  priority: number;
  hooks: string[];
  permissions: string[];
  pendingPermissions: string[];
  circuitBroken: boolean;
  settings: MockSetting[];
}

// 中性占位目录：dev mock 不依赖任何构建机的绝对路径。
const DIR = "C:\\Users\\Public\\Downloads";

function file(name: string, length: number, completed: number): MockFile[] {
  return [
    {
      index: "1",
      path: `${DIR}\\${name}`,
      length: String(length),
      completedLength: String(completed),
      uris: [{ uri: `https://example.com/${name}`, status: "used" }],
    },
  ];
}

const tasks: MockTask[] = [
  {
    gid: "a1024f9c8e1b",
    status: "active",
    totalLength: "6100000000",
    completedLength: "2480000000",
    downloadSpeed: "8388608",
    connections: "16",
    dir: DIR,
    files: file("ubuntu-24.04.2-desktop-amd64.iso", 6100000000, 2480000000),
  },
  {
    gid: "b2048c7d9f2a",
    status: "active",
    totalLength: "367001600",
    completedLength: "125829120",
    downloadSpeed: "3250880",
    connections: "8",
    dir: DIR,
    files: file("blender-4.2-windows-x64.msi", 367001600, 125829120),
  },
  {
    gid: "c3072b6e0d3b",
    status: "waiting",
    totalLength: "199229440",
    completedLength: "0",
    downloadSpeed: "0",
    connections: "0",
    dir: DIR,
    files: file("rust-1.82.0-x86_64-pc-windows-msvc.msi", 199229440, 0),
  },
  {
    gid: "d4096a5f1c4c",
    status: "paused",
    totalLength: "660602880",
    completedLength: "220200960",
    downloadSpeed: "0",
    connections: "0",
    dir: DIR,
    files: file("debian-12.7.0-amd64-netinst.iso", 660602880, 220200960),
  },
  {
    gid: "e5120a4b2d5d",
    status: "complete",
    totalLength: "8912896",
    completedLength: "8912896",
    downloadSpeed: "0",
    connections: "4",
    dir: DIR,
    files: file("aria2-1.37.0-win-64bit-build1.zip", 8912896, 8912896),
  },
  {
    gid: "f6144b3c6e6e",
    status: "error",
    totalLength: "3221225472",
    completedLength: "1181116006",
    downloadSpeed: "0",
    connections: "0",
    errorCode: "34",
    errorMessage: "磁盘剩余空间不足，已停止预分配",
    dir: DIR,
    files: file("large-model-weights.bin", 3221225472, 1181116006),
  },
];

/** 确定性分片进度：`base` 之前全满，之后按锯齿波动，便于肉眼分辨 IDM 多段条。 */
function pieces(count: number, base: number): number[] {
  return Array.from({ length: count }, (_, index) => {
    if (index < base) return 100;
    if (index === base) return 62;
    if (index % 7 === 0) return 0;
    return Math.min(96, 12 + ((index * 17) % 74));
  });
}

// 分片 / 活跃连接 / ETA 仅用于浏览器（Vite dev）渲染验证，数值固定以便截图对比。
tasks[0].pieceProgress = pieces(64, 26);
tasks[0].activeWorkers = "16";
tasks[0].eta = "428";
tasks[1].pieceProgress = pieces(32, 11);
tasks[1].activeWorkers = "8";
tasks[1].eta = "75";
tasks[2].pieceProgress = pieces(24, 0);
tasks[2].activeWorkers = "0";
tasks[3].pieceProgress = pieces(40, 13);
tasks[3].activeWorkers = "0";
tasks[4].pieceProgress = pieces(16, 16);
tasks[4].activeWorkers = "0";

/** 94 天历史（确定性生成，避免截图每次都不一样）。 */
const history: Array<Record<string, unknown>> = [];
{
  const now = Math.floor(Date.now() / 1000);
  const names = [
    "kernel-build.tar.gz",
    "fonts-noto.zip",
    "dataset-sample.parquet",
    "node-v22.11.0-win-x64.zip",
    "vscode-setup.exe",
    "docker-desktop.iso",
    "ffmpeg-full.zip",
    "llama-3-8b.gguf",
  ];
  for (let i = 0; i < 64; i += 1) {
    const daysAgo = (i * 13) % 93;
    const ts = now - daysAgo * 86400 - (i % 24) * 3600;
    const size = (i % 7 + 1) * 1024 * 1024 * 96;
    history.push({
      gid: `hist${String(i).padStart(4, "0")}`,
      url: `https://example.com/${names[i % names.length]}`,
      dir: DIR,
      status: i % 17 === 0 ? "error" : "complete",
      completedLength: i % 17 === 0 ? Math.floor(size / 3) : size,
      totalLength: size,
      downloadSpeed: 0,
      errorCode: i % 17 === 0 ? 34 : null,
      errorMessage: i % 17 === 0 ? "磁盘剩余空间不足" : null,
      createdAt: ts - 3600,
      updatedAt: ts,
      finishedAt: ts,
    });
  }
}

const plugins: MockPlugin[] = [
  {
    id: "auto-sort",
    name: "自动分类",
    version: "1.4.0",
    description: "下载完成后按扩展名把文件移动到 视频 / 音乐 / 文档 等子目录。",
    enabled: true,
    priority: 10,
    hooks: ["onDownloadComplete"],
    permissions: ["downloads:read", "fs:write:downloads"],
    pendingPermissions: [],
    circuitBroken: false,
    settings: [
      {
        key: "enableNotify",
        label: "完成时发送通知",
        type: "boolean",
        options: [],
        default: true,
        value: true,
      },
      {
        key: "destFolder",
        label: "目标文件夹",
        type: "string",
        options: [],
        default: "downloads",
        value: "downloads",
      },
      {
        key: "sortMode",
        label: "分类模式",
        type: "select",
        options: ["后缀", "日期", "来源"],
        default: "后缀",
        value: "后缀",
      },
    ],
  },
  {
    id: "link-rewriter",
    name: "链接改写",
    version: "0.9.2",
    description: "在任务入队前把镜像站链接重写为直连地址，缩短首字节等待。",
    enabled: true,
    priority: 5,
    hooks: ["onBeforeAddDownload"],
    permissions: ["downloads:intercept"],
    pendingPermissions: [],
    circuitBroken: false,
    settings: [],
  },
  {
    id: "desktop-notifier",
    name: "桌面通知",
    version: "2.0.1",
    description: "任务完成或失败时弹出系统通知，可自定义提示音。",
    enabled: false,
    priority: 20,
    hooks: ["onDownloadComplete", "onDownloadError"],
    permissions: ["downloads:read"],
    pendingPermissions: ["notify"],
    circuitBroken: false,
    settings: [],
  },
];

const trackers = [
  "udp://tracker.opentrackr.org:1337/announce",
  "udp://open.tracker.cl:1337/announce",
  "udp://9.rarbg.com:2810/announce",
  "udp://tracker.openbittorrent.com:6969/announce",
  "udp://exodus.desync.com:6969/announce",
  "https://tracker.tamersunion.org:443/announce",
  "udp://tracker.torrent.eu.org:451/announce",
  "udp://open.stealth.si:80/announce",
];

function findTask(gid: string): MockTask | undefined {
  return tasks.find((task) => task.gid === gid);
}

function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

/** 与 Rust IPC 命令名一一对应；未知命令直接 reject，便于发现遗漏。 */
export async function mockInvoke<T>(
  cmd: string,
  args?: Record<string, unknown>,
): Promise<T> {
  switch (cmd) {
    case "get_engine_status":
      return true as T;
    case "get_tasks":
      return clone(tasks) as T;
    case "get_active_tasks":
      return clone(tasks.filter((t) => t.status === "active")) as T;
    case "get_task": {
      const gid = String(args?.gid ?? "");
      return clone(findTask(gid) ?? tasks[0]) as T;
    }
    case "add_download": {
      const urls = Array.isArray(args?.urls)
        ? (args.urls as unknown[]).map((item) => String(item ?? "")).filter(Boolean)
        : [];
      const url = urls[0] ?? "";
      const gid = Math.random().toString(16).slice(2, 14);
      const name = url.split("/").pop() || "download.bin";
      tasks.unshift({
        gid,
        status: "active",
        totalLength: "104857600",
        completedLength: "0",
        downloadSpeed: "0",
        connections: "1",
        dir: DIR,
        files: file(name, 104857600, 0),
      });
      return gid as T;
    }
    case "add_torrent": {
      const name = String(args?.name ?? "download.torrent");
      const gid = Math.random().toString(16).slice(2, 14);
      tasks.unshift({
        gid,
        status: "waiting",
        totalLength: "0",
        completedLength: "0",
        downloadSpeed: "0",
        connections: "0",
        dir: DIR,
        files: file(name.replace(/\.torrent$/i, ""), 0, 0),
      });
      return gid as T;
    }
    case "pause_download": {
      const task = findTask(String(args?.gid ?? ""));
      if (task) task.status = "paused";
      return null as T;
    }
    case "resume_download": {
      const task = findTask(String(args?.gid ?? ""));
      if (task) task.status = "active";
      return null as T;
    }
    case "remove_download":
    case "force_remove": {
      const index = tasks.findIndex((task) => task.gid === String(args?.gid ?? ""));
      if (index >= 0) tasks.splice(index, 1);
      return null as T;
    }
    case "list_plugins":
      return clone(plugins) as T;
    case "toggle_plugin": {
      const plugin = plugins.find((p) => p.id === String(args?.id ?? ""));
      if (plugin) plugin.enabled = Boolean(args?.enabled);
      return null as T;
    }
    case "grant_plugin_permissions": {
      const plugin = plugins.find((p) => p.id === String(args?.id ?? ""));
      if (plugin) {
        plugin.permissions.push(...plugin.pendingPermissions);
        plugin.pendingPermissions = [];
        plugin.enabled = true;
      }
      return null as T;
    }
    case "update_plugin_settings": {
      const plugin = plugins.find((p) => p.id === String(args?.id ?? ""));
      const values = (args?.values ?? {}) as Record<string, unknown>;
      if (plugin) {
        for (const item of plugin.settings) {
          if (Object.prototype.hasOwnProperty.call(values, item.key)) {
            item.value = values[item.key];
          }
        }
      }
      return null as T;
    }
    case "get_metrics":
      return {
        rpc_calls: 14820,
        rpc_errors: 3,
        plugin_deny: 1,
        plugin_dropped: 0,
        intercept_timeout: 0,
      } as T;
    case "get_global_stat":
      return {
        downloadSpeed: "11634688",
        uploadSpeed: "245760",
        numActive: "2",
        numWaiting: "1",
        numStopped: "64",
      } as T;
    case "get_trackers":
    case "refresh_trackers":
      return {
        items: clone(trackers),
        count: trackers.length,
        lastUpdate: Math.floor(Date.now() / 1000) - 3600,
        source: "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt",
        injecting: true,
      } as T;
    case "get_history_tasks":
      return { items: clone(history), total: history.length } as T;
    case "clear_history":
      return { deleted: 0 } as T;
    case "change_global_option":
    case "sync_engine_config":
    case "reveal_in_dir":
    case "set_autostart":
    case "set_close_behavior":
      return null as T;
    case "get_autostart":
      return false as T;
    case "get_api_info":
      return {
        port: 17800,
        url: "http://127.0.0.1:17800",
        token: "6f1c2d9a-8b47-4e51-9c3d-2a7f0e5b6d18",
      } as T;
    case "pick_directory":
      return DIR as T;
    case "get_host_config":
      // download_dir 必须有值：设置弹窗打开后「保存」会校验目录非空。
      return {
        download_dir: DIR,
        resume_last: true,
        close_behavior: "tray",
        enable_auto_referer: true,
        enable_http1_compat: false,
      } as T;
    case "set_host_config":
      return null as T;
    case "get_global_option":
      return {
        dir: DIR,
        split: "8",
        "max-concurrent-downloads": "8",
        "max-connection-per-server": "8",
        "max-overall-download-limit": "0",
        "max-download-limit": "0",
        "all-proxy": "",
        "user-agent":
          "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36",
        referer: "",
        header: "",
        "dynamic-slicing-stall": "12",
        "dynamic-slicing-max-preempts": "2",
      } as T;
    // 窗口焦点唤醒：dev 下没有真实链路，直接回报已连接。
    case "reconnect_engine":
      return true as T;
    // 代理连通性测试：固定 24ms，便于验证成功提示样式。
    case "test_proxy":
      return 24 as T;
    case "get_pending_intercept":
      return null as T;
    case "cancel_intercept":
    case "remove_task_and_files":
    case "open_external":
    // 更新确认：dev 下没有真实进程可退出，直接回报成功（宿主侧会先暂停再退出）。
    case "quit_for_update":
      return null as T;
    case "get_app_paths":
      return {
        home: "C:\\Users\\Public\\aria2-harness",
        data: "C:\\Users\\Public\\aria2-harness\\data",
        plugins: "C:\\Users\\Public\\aria2-harness\\plugins",
        downloads: DIR,
      } as T;
    case "get_browser_extension_info":
      return {
        path: "C:\\Users\\Public\\aria2-harness\\browser-extension",
        exists: true,
      } as T;
    default:
      throw new Error(`mock 未实现的命令：${cmd}`);
  }
}
