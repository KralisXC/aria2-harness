# Aria2-Harness 架构说明（C-3）

> 适用版本：v1.0.0-beta8 及以后
> 本文描述的是**当前实现**，不是历史设计稿。旧的三层 Tauri 方案、`src-tauri/` 目录、
> 单引擎直连 RPC 等内容均已被 C-3 取代。

---

## 0. 一句话架构

**三个进程、两层引擎、一个 RPC 入口。**

- L1 引擎层：HTTP(S) 走 Rust 的 **Harness-Net**，BT / 磁力 / metalink 走 C++ 的 **aria2 魔改核心**
- L2 宿主层：`tao + wry` 手搓的轻量宿主（**无 Tauri**），负责路由、进程、持久化与配置下发
- L3 表现层：Vue 3 + Naive UI + Tailwind，跑在系统 WebView2 里

```
┌──────────────────────────────────────────────────────────────────┐
│  L3  aria2-harness.exe  (普通权限)                                │
│      Vue 3 + Naive UI + Tailwind  →  wry / WebView2               │
└───────────────▲──────────────────────────────────────────────────┘
                │  命名管道（JSON，双向请求/事件）
┌───────────────┴──────────────────────────────────────────────────┐
│  L2  aria2-service.exe  (LocalSystem)                             │
│      ├── RPC Router :6800  ◄── UI / 浏览器扩展 / 命令行           │
│      ├── Process Guard     （Job Object，父死子亡）               │
│      ├── SQLite 历史       （aria2-harness.db，WAL）              │
│      └── Config 下发       （config.json → 引擎全局选项）         │
│                                                                   │
│  L1  ├── Harness-Net（同进程 Rust）────────────────────────────┐  │
│      └── aria2c.exe  :6801（C++ 子进程）──────────────────────┐│  │
└───────────────────────────────────────────────────────────────┴┴──┘
```

对外**只有一个** RPC 端口 `6800`。UI 与扩展都只认它，内部用哪个引擎由 Router 决定。

---

## 1. L1 引擎层

### 1.1 Harness-Net —— HTTP / HTTPS（Rust）

代码：`engine-net/`，与宿主同进程（`--features native-http` 编译进 `aria2-service.exe`）。

| 模块 | 职责 |
| --- | --- |
| `http/client.rs` | 建连、多段 Range 请求、连接复用 |
| `http/h2.rs` | HTTP/2（仅 TLS ALPN，不做 h2c） |
| `http/tls.rs` | `rustls` + `rustls-platform-verifier`（信任根读 Windows 证书存储） |
| `http/proxy.rs` | HTTP / HTTPS / SOCKS 代理与 CONNECT 隧道 |
| `http/redirect.rs` | 重定向策略（跨源、方法降级、循环检测） |
| `http/session.rs` | 会话级 Cookie / 认证 / 默认头 |
| `dns/resolver.rs` | `hickory-resolver`（DoH 可选），替换 c-ares |
| `dns/connect.rs` | 解析结果 → 地址族择优 → Happy Eyeballs 建连 |
| `download/scheduler.rs` | **SegmentManager**：分片调度、动态切割、抢占 |
| `download/worker.rs` | 单分片拉取、写入、校验、事件上报 |
| `progress/*` | 断点进度（`.sqlite` / `.aria2` 双格式，见 §3.2） |
| `rpc/*` | 与 aria2 对齐的 JSON-RPC 方法子集（`tellStatus` / `pause` / `remove` …） |
| `task/registry.rs` | 任务注册表、GID 生成（`fffe` + 12 hex）、事件扇出 |
| `backpressure.rs` | 写盘反压与内存水位 |

设计取向：**Rust 负责传输与调度，不碰 BT**。BT 的 DHT / PEX / 分块谈判交给 C++。

### 1.2 aria2 魔改核心 —— BT / 磁力 / metalink（C++）

代码：`aria2-1.37.0/`（GPLv2），以独立进程 `aria2c.exe` 运行，仅监听 `127.0.0.1:6801`。

在上游 1.37.0 之上的改动集中在：

- `SegmentMan.cc`：动态切割（`enable-dynamic-slicing` / `dynamic-slicing-stall` /
  `dynamic-slicing-max-preempts` / `dynamic-slicing-min-tail`），慢分片抢占后重新切分
- 预分配与磁盘 I/O：`falloc` 路径、长路径（`\\?\`）与稀疏文件处理
- 错误页与 RPC 扩展字段（`_engine`、`pieceProgress`、`activeWorkers` 等）
- 静态链接（`ARIA2_STATIC=yes`），单文件 13.8MB 无外部依赖

### 1.3 双引擎路由

`--engine=auto|aria2|native`（`router.rs`，默认 `auto`）：

```rust
pub fn uri_target(uri: &str) -> EngineKind {
    // http/https 且不是 .torrent/.metalink → native，其余 → aria2
}
```

| 输入 | 归属 |
| --- | --- |
| `http(s)://…` 普通直链 | Harness-Net |
| `http(s)://…/x.torrent`、`*.metalink` | aria2c（下载元数据后由 aria2 接管 BT 调度） |
| `magnet:`、`udp://`、`bt://` | aria2c |
| 同一任务多 URI 镜像 | 整单交给首个命中引擎（**不拆分**） |

**不变量**：一个 GID 只属于一个引擎。路由表 `gid → engine` 仅在 `addUri` 成功返回后写入；
`tellStatus` 等读操作先查路由表，命中即转发，未命中才做协议回落。

---

## 2. L2 宿主层

### 2.1 外壳：tao + wry（无 Tauri）

| 关注点 | 实现 |
| --- | --- |
| 窗口 | `tao 0.37`，`with_visible(false)` 创建，`PageLoadEvent::Finished` 后显示（避免白屏） |
| WebView | `wry 0.57` + `WebView2Loader.dll`（Full 版内嵌固定运行时） |
| 前端托管 | `web_ui.rs` 在 `127.0.0.1` 随机端口起内置静态服务，`dist/` 即站点根 |
| 单实例 | 互斥锁 + 二次启动唤起已有窗口 |
| 托盘 / 通知 | `tray-icon` + `tauri-winrt-notification`（只借库，不用 Tauri 运行时） |
| IPC | 命名管道（显式 DACL，仅当前用户可连），JSON 请求/响应 + 服务端事件推送 |

### 2.2 RPC Router

```
UI / 扩展 / CLI ──► :6800 Router ──┬── Harness-Net（同进程调用）
                                   └── aria2c :6801（转发 + 结果装饰）
```

- 外部只暴露 `6800`；`6801` 只绑回环，且带随机 `rpc-secret`
- 读方法按 `gid → engine` 转发；写方法按 URI 协议归类后投递
- 结果统一补齐 `_engine` 等扩展字段，前端无需感知引擎差异
- WebSocket 通知（`aria2.onDownloadStart/Complete/Error`）由 Router 汇聚后单流下发

### 2.3 进程管理

- `aria2-service.exe` 由 SCM 以 LocalSystem 运行；UI 以普通权限运行，仅持有管道客户端
- **Job Object** 守护：UI 崩溃/退出时内核自动回收 `aria2c.exe`，不留孤儿
- 服务崩溃时 UI 负责重启并重连；重连后自动对账（`tellStatus` → SQLite upsert）
- 路径解析（`paths.rs`）：便携版 = exe 同级；安装版 = `%LOCALAPPDATA%\Aria2-Harness`。
  **LocalSystem 服务必须用 WTS 交互式用户环境块取 `LOCALAPPDATA`**，否则数据会落到
  `systemprofile`，与 UI 算出两个不同的 HOME。

### 2.4 持久化

| 数据 | 落点 | 说明 |
| --- | --- | --- |
| 任务历史 | `data/aria2-harness.db` | SQLite / WAL，`user_version` 版本化迁移，拒绝降级 |
| 引擎会话 | `data/aria2.session` | aria2 原生格式，升级/重启后继续下 |
| 断点控制 | `<文件>.sqlite` 或 `<文件>.aria2` | 见 §3.2 |
| 用户设置 | `config.json` | UI 与引擎的**唯一真源** |
| 日志 | `data/logs/` | `app.log` / `service.log`，按天轮转 |

SQLite 写路径一律在 `spawn_blocking` 内执行，事件循环不碰 SQL；单连接 + 阻塞池内互斥。

---

## 3. 核心机制

### 3.1 动态切割（Rust SegmentManager）

`engine-net/src/download/scheduler.rs`：

1. 按 `split` 把剩余区间切成初始分片，交给 `worker` 并发拉取
2. 每个分片维护速度高水位；低于 `PREEMPT_SPEED_FLOOR`（100 KiB/s）且持续
   `dynamic-slicing-stall` 秒 → 判定为「慢分片」
3. 慢分片被抢占（最多 `dynamic-slicing-max-preempts` 次），尾部重新切分给空闲 worker
4. 若整体速度长期上不去，stall 阈值**自适应放大**；放大到 120s 仍未改善则关闭抢占，
   避免无限抖动
5. C++ 引擎侧使用同名选项（`SegmentMan.cc`），保证两引擎行为语义一致、设置可复用

### 3.2 双引擎无缝接管断点续传

`engine-net/src/progress/`：

- 新任务默认写 `<文件>.sqlite`（`--sqlite-progress=true`）
- 打开目标时按 **「已有格式优先」**：有 `.sqlite` 用 SQLite，有 `.aria2` 用 legacy，
  都没有才按开关决定
- **写入沿用载入格式** —— 同一个文件不会因为切换引擎而多出一份控制文件
- `migrate.rs` 提供 legacy → SQLite 的单向升级（原文件保留，失败可回滚）

于是同一个任务可以在 `--engine=aria2` 与 `--engine=native` 之间来回切换，
进度、已校验分片、断点续传全部接得上。

### 3.3 配置全量下发

aria2 的 `changeGlobalOption` 不跨进程保留：引擎重启后一切回到命令行参数的默认值。
C-3 的处理是**每次连接都全量重放**，而不是「只在用户点保存时下发一次」：

1. 服务启动：`build_args()` 先读 `config.json`，把 `--dir` / `--split` /
   `--max-connection-per-server` 直接写进命令行
2. 引擎 WebSocket 连上：`apply_saved_globals()` **无条件**下发
   `dir`、`split`、`max-connection-per-server`，再按需下发代理、UA、Referer、
   自定义头、上下行限速、动态切割参数
3. UI 连上宿主：`bootstrap()` 首先调用 `sync_engine_config` RPC 触发同一套全量重放

结果：**界面上显示什么，引擎就用什么**。不存在「UI 看着对、文件却下到别处」的中间态。

---

## 4. L3 表现层

- Vue 3.5 + TypeScript + Pinia，路由为哈希模式，构建工具 Vite
- Naive UI 提供表单/弹窗/下拉，Tailwind 4 负责布局与主题原子类
- 关键交互：**多段进度条**（`NodeProgressBar.vue`，按分片着色、超 48 段自动合并）、
  **虚拟列表**（`n-virtual-list`，万级任务不掉帧）、毛玻璃与暗色主题
- IPC 全部收敛在 `src/ipc.ts`；开发期 `src/dev/mock.ts` 顶替宿主，浏览器可直接调试 UI

---

## 5. 数据目录

```
<HOME>/
├── config.json
├── data/
│   ├── aria2-harness.db      # 任务历史
│   ├── aria2.session         # 引擎会话
│   └── logs/
├── plugins/                  # Wasm / QuickJS 插件
└── downloads/
```

| 形态 | 判定 | HOME |
| --- | --- | --- |
| 便携版 | exe 同级存在 `dist/index.html` | exe 所在目录 |
| 安装版（不可写目录） | 如 Program Files | `%LOCALAPPDATA%\Aria2-Harness` |
| 安装版（可写目录） | 其他 | 安装目录 |

覆盖安装时 `data/`、`plugins/`、`downloads/`、`config.json` 均受保护
（安装脚本 `uninsneveruninstall`，`[InstallDelete]` 只清程序资源）。

---

## 6. 构建与验证

```powershell
# 前端
cd client && npm ci && npm run build

# 宿主（含双引擎）
cd client/src-host
cargo build --release --features native-http,wasm-runtime

# C++ 引擎（MSYS2 mingw64，见 aria2-1.37.0/mingw-build-memo）
cd aria2-1.37.0 && ./configure --disable-nls --enable-static --disable-shared ARIA2_STATIC=yes && make -j
```

验证基线：

| 测试 | 位置 | 断言 |
| --- | --- | --- |
| A/B 一致性 | `engine-net/tests/ab_consistency.rs` | 同一 URL 两引擎落盘 SHA-256 相同 |
| 断点接管 | `engine-net/tests/resume.rs` | 中断后换引擎继续，进度不回退 |
| 动态切割 | `engine-net/tests/phase4_slicing.rs` | 慢分片被抢占、总量守恒 |
| RPC 子集 | `engine-net/tests/rpc_subset.rs` | 与 aria2 语义对齐 |
| 故障注入 / 长稳 | `phase5_faults.rs` / `phase5_soak.rs` | 断连、超时、限速下不丢分片 |

---

## 7. 红线

1. **一个 GID 只属于一个引擎**，路由表只在 `addUri` 成功后写
2. **配置不允许「部分生效」**：连接即全量重放，不依赖用户点保存
3. **切换引擎不改控制文件格式**：载入什么格式就写什么格式
4. **UI 与服务的 HOME 必须一致**：LocalSystem 下走 WTS 交互式用户环境块
5. **引擎必被回收**：任何异常路径都要经过 Job Object / 显式 kill
