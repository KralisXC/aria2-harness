# Aria2-Harness 核心开发与架构大纲

## 1. 项目概述与技术栈

一句话：以魔改 aria2 为引擎、Rust 双进程为宿主、WebView2 为壳的 Windows 桌面下载器，便携/安装双形态、常驻服务免 UAC。

- **C++**：aria2 1.37（nghttp2 / OpenSSL / SQLite3），MinGW 静态编译为 `aria2c.exe`。
- **Rust 2024**：tao 0.37 + wry 0.57、tokio、rusqlite、rquickjs（可选 wasmtime）、windows 0.62、windows-service 0.8。
- **前端**：Vue 3 + TS + Pinia + Naive UI + Tailwind + Vite，产物 `dist/` 由宿主内置静态服务器托管。
- **打包**：Inno Setup 7；WebView2 Fixed Version 随包内置。

## 2. 核心架构图谱（L1→L3）

- **L1 引擎层**：`aria2c.exe`，无 UI、只暴露 JSON-RPC；自研 Http2 多路复用、动态切割、SQLite 分片进度、并发上限解封至 128。
- **L2 宿主层（双进程）**：
  - `aria2-harness.exe`（普通权限）：tao 窗口、WebView2、托盘、IPC 路由、内置静态服务器、扩展 HTTP API、插件系统。
  - `aria2-service.exe`（SCM/LocalSystem 或管理员）：引擎拉起/监控/回收、Job Object、命名管道服务端；同一二进制兼作 SCM 服务与独立进程。
- **L3 表现层**：Vue 3 SPA，`ipc.ts` → `dispatch(cmd,args)`，后端事件统一走 `aria2://event`。
- **外部生态**：MV3 浏览器扩展（探测 `127.0.0.1:17800–17809` + token）；QuickJS 沙箱插件（默认）、wasmtime（`--features wasm-runtime`）。

## 3. 核心机制与数据流转

- **RPC（自研，替代 aria2-ws）**：`rpc_client.rs` 直连 `ws://127.0.0.1:6800/jsonrpc`；调用超时 5s 且到期必摘除 id，连接超时 3s，重连退避 1s→30s；事件按 GID 做 500ms 节流合并，前端收到后重拉 `tellStatus`。
- **IPC**：命名管道 + 单行 JSON（`service_protocol.rs`：`start_engine` / `get_status` / `shutdown`）。管道 SDDL 放行 Everyone 并打 Medium 完整性标签（需 `SeSecurityPrivilege`），否则普通权限 UI 必报 os error 5；`first_pipe_instance` 保证单实例。
- **拉起链**：`sc start Aria2-Harness-Service` → `schtasks /run`（HIGHEST）→ `ShellExecuteW("runas")`（UAC 兜底）；服务以 SYSTEM 常驻，UI 永不提权。
- **持久化**：引擎 `data/aria2.session` + C++ `SqliteProgressInfoFile`（分片位图，`.sqlite` 后缀）；Rust `rusqlite` 存 `data/aria2-harness.db`（历史与重连对账）。`paths.rs` 统一 HOME：便携 = exe 同级，安装 = `%LOCALAPPDATA%\Aria2-Harness`。
- **WebView2 自闭环**：定位内置 `WebView2Runtime` 后设 `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER`；user data 目录显式指定（安装版 `%LOCALAPPDATA%\...\WebView2`、便携版 `data\WebView2`），规避 Program Files 不可写导致的静默闪退。

## 4. 核心硬核突破

- **HTTP/2 多路复用**：`Http2Multiplexer` 独占一条 TLS+ALPN(h2) 连接，cuid→stream；每流拿 `fd=-1` 占位 SocketCore + 独立 RecvBuffer，原 Command 状态机零改动；配合 `SegmentMan` 动态切割（按速度/僵死采样抢占尾段，`PREF_ENABLE_DYNAMIC_SLICING`）。
- **128 并发 + 自动降级**：引擎 `max-connection-per-server` 上限 1..128（默认 16）；`degradation.rs` 遇 403/429/连接重置按 128→64→…→1 降档重拉；errorCode 17（文件占用）独立 500ms 起退避 ≤3 次。
- **智能防盗链**：显式 Referer > 全局自定义 > `guess_referer` 内置规则；UNC/SMB 自动降为单连接。
- **静息 ~0.23% CPU**：`ControlFlow::Wait` + 30s 巡检；5 分钟无操作、失焦且无任务 → `webview.set_visible(false)` 挂起渲染；Job Object 保证引擎随宿主回收。
- **免 UAC**：SCM 首选，服务 DACL 授 `Authenticated Users` 的 `RP/LC/CC`（启动/查询）；服务被删或禁用时自动回退计划任务与 UAC。

## 5. 核心红线（其他 AI 不得违反）

- **FFI 线程**：子线程 / 异步任务不得触碰 C++ 核心对象；引擎对象只存在于服务进程事件循环线程。
- **内存边界**：严禁跨 FFI 抛异常或 panic；错误一律收敛为 `AppError` / JSON 响应。
- **状态隔离**：UI 只展示与转发；下载目录由宿主 `create_dir_all` 后显式下发，前端不得自造路径。
- **网络**：不得破坏管道单实例、RPC 端口占用探测（6800 冲突必须显式报错）、扩展 API 仅绑 `127.0.0.1`。
- **SCM**：`ServiceMain` 必须先 `SetServiceStatus(RUNNING)` 再进阻塞循环，否则 1053；靠 1063 识别「非 SCM 启动」，禁止吞掉 dispatcher 的真实错误。

## 6. 工程目录与关键文件

- **C++**：`aria2-1.37.0/src/` — `Http2*.{h,cc}`（多路复用）、`SegmentMan.cc`（动态切割）、`SqliteProgressInfoFile.cc`（进度库）、`OptionHandlerFactory.cc`（128 上限）；`http2probe.cc` 探针；`mingw-*` 构建脚本。
- **Rust**：`client/src-host/src/` — `main.rs`（窗口/事件循环/挂起）、`lib.rs`（命令面/引导/WS 循环）、`bin/aria2_service.rs`（SCM + 引擎）、`service_protocol.rs` / `service_client.rs`（IPC）、`rpc_client.rs`、`db.rs`、`paths.rs`、`degradation.rs`、`process_guard.rs`、`plugin/*`、`browser_api.rs`、`web_ui.rs`、`power.rs`、`notify.rs`、`update_check.rs`。
- **Vue**：`client/src/` — `App.vue`、`ipc.ts`、`views/`、`stores/`、`utils/taskName.ts`；产物 `dist/`。
- **扩展**：`browser-extension/{manifest.json, background.js}`。
- **打包**：`setup.iss`、`Aria2-Harness-v*-Windows-x64/`、`installer_output/`。

## 7. 未来演进路线（C-3 蓝图）

- **网络层 Rust 化（垂直切片）**：① 新建 `engine-net`（tokio + hyper/h2），先接管纯 HTTP 直链（Range/断点/重试），对外仍伪装成 aria2 JSON-RPC；② 切入 HTTP/2 多路复用与动态切割；③ HTTP 任务的控制文件（`*.aria2`）与会话（`aria2.session`）由 engine-net 读写并与 aria2c 格式兼容（BT 位图 `.sqlite` 本期不迁移，Phase 5 灰度 BT 时再议，详见 weilai.md R5 / Q5）；④ C++ 只留 BT/DHT/Metalink，按协议族灰度下线，feature flag 双栈并行。
- **macOS 移植**：tao/wry 天然跨平台；逐项替换——windows-service→launchd plist、命名管道→Unix domain socket、Job Object→setpgid/进程组、WinRT Toast→UNUserNotificationCenter、IFileOpenDialog/winreg→NSOpenPanel/NSUserDefaults；引擎需 darwin 交叉编译并切 kqueue 轮询。
