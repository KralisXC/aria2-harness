// 极简 IPC 兼容层：对接 tao + wry（WebView2）外壳，替代原 @tauri-apps/api。
//
// 目标：既有代码里的 `invoke(cmd, args)` / `listen(event, handler)` 用法完全不变，
// 只把 import 来源从 @tauri-apps/api 换成此处。
//
// 协议（与 src-host/src/main.rs 约定一致）：
//   - 前端 -> 宿主：window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
//   - 宿主 -> 前端（evaluate_script 调全局函数）：
//       window.__ipcResolve(id, result)
//         —— result 为 {"__err": {code, message}} 时表示调用失败
//         （Rust 侧 `AppError` 手写 Serialize，只吐这两个字段）
//       window.__ipcEmit(event, payload) —— 派发宿主事件

/** 事件载体（对齐旧 Tauri `Event` 的最小面：仅依赖 `payload`）。 */
export interface IpcEvent<T> {
  event: string;
  id: number;
  payload: T;
}

/** 取消订阅函数。 */
export type UnlistenFn = () => void;

/**
 * 宿主返回的结构化错误（对应 Rust 侧 `AppError` 的 `{ code, message }`）。
 *
 * 继承 `Error` 是为了让既有 `${error}` 插值仍能打出可读文案（而不是 `[object Object]`）；
 * 真正的价值在 `code` —— 前端可据此分流提示（断网重试 / 让用户启动核心服务 / 插件解析失败…）。
 */
export class IpcError extends Error {
  /** 稳定的机器可读错误码，如 `ARIA2_DISCONNECTED` / `SERVICE_UNAVAILABLE` / `PLUGIN_LOAD`。 */
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "IpcError";
    this.code = code;
  }
}

/** 把 `catch` 到的任意异常渲染成一行可读文案（所有显示点统一走这里）。 */
export function errText(error: unknown): string {
  if (error instanceof IpcError) return error.message;
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return String(error);
}

type Handler = (ev: IpcEvent<unknown>) => void;

/** 事件名 -> 回调集合。 */
const listeners = new Map<string, Set<Handler>>();

/** 请求 id -> 待决 Promise（等待宿主回传结果）。 */
const pending = new Map<number, { resolve: (value: unknown) => void; reject: (reason: unknown) => void }>();

/** 请求 id 自增游标。 */
let nextId = 1;

/** wry 注入的 IPC 通道与宿主回传用的全局函数。 */
interface IpcWindow extends Window {
  ipc?: { postMessage: (message: string) => void };
  __ipcResolve?: (id: number, result: unknown) => void;
  __ipcEmit?: (event: string, payload: unknown) => void;
}

const w = window as IpcWindow;

// 宿主经 evaluate_script 调用：结算一次进行中的 invoke 调用。
w.__ipcResolve = (id, result) => {
  const entry = pending.get(id);
  if (!entry) return; // 超时/重复回传，忽略
  pending.delete(id);
  // 约定：宿主把调用失败编码为 { "__err": { code, message } }。
  // 兼容老格式（{ "__err": "字符串" }）：直接以原值 reject。
  if (result !== null && typeof result === "object" && "__err" in result) {
    const raw = (result as { __err: unknown }).__err;
    if (raw !== null && typeof raw === "object" && "code" in raw && "message" in raw) {
      const { code, message } = raw as { code: string; message: string };
      entry.reject(new IpcError(code, message));
    } else {
      entry.reject(raw);
    }
  } else {
    entry.resolve(result);
  }
};

// 宿主经 evaluate_script 调用：把事件分发给所有订阅者。
w.__ipcEmit = (event, payload) => {
  const set = listeners.get(event);
  if (!set) return;
  for (const handler of set) {
    try {
      handler({ event, id: 0, payload });
    } catch (error) {
      console.error(`[ipc] 事件处理失败：${event}`, error);
    }
  }
};

/** 调用后端命令（命令名 / 参数名与旧 Tauri 命令保持一致）。 */
export function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  const channel = w.ipc;
  if (!channel) {
    // 纯浏览器（Vite dev）场景：用静态数据渲染 UI。动态 import 保证生产包不引入 mock。
    if (import.meta.env.DEV) {
      return import("./dev/mock").then((mod) => mod.mockInvoke<T>(cmd, args));
    }
    return Promise.reject(new Error("IPC 通道不可用：window.ipc 未就绪"));
  }
  const id = nextId++;
  return new Promise<T>((resolve, reject) => {
    pending.set(id, { resolve: resolve as (value: unknown) => void, reject });
    try {
      channel.postMessage(JSON.stringify({ id, cmd, args: args ?? {} }));
    } catch (error) {
      pending.delete(id);
      reject(error);
    }
  });
}

/** 订阅宿主事件；返回取消订阅函数（同步返回，用 Promise 包装以对齐旧 API）。 */
export function listen<T>(event: string, handler: (ev: IpcEvent<T>) => void): Promise<UnlistenFn> {
  let set = listeners.get(event);
  if (!set) {
    set = new Set();
    listeners.set(event, set);
  }
  const wrapped = handler as Handler;
  set.add(wrapped);
  return Promise.resolve(() => {
    const current = listeners.get(event);
    if (!current) return;
    current.delete(wrapped);
    if (current.size === 0) listeners.delete(event);
  });
}
