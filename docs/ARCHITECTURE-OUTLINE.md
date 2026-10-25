# 架构速览（C-3）

> 详细说明见 [architecture.md](architecture.md)。本文是索引页：谁在哪、一次下载怎么走、改代码要守住什么。

---

## 1. 分层速查

| 层 | 进程 | 技术 | 目录 |
| --- | --- | --- | --- |
| L1 引擎 | 同进程 / 子进程 | Harness-Net（Rust）、aria2 魔改（C++） | `engine-net/`、`aria2-1.37.0/` |
| L2 宿主 | `aria2-harness.exe`（普通权限）+ `aria2-service.exe`（LocalSystem） | tao、wry、SQLite、命名管道 | `client/src-host/` |
| L3 表现 | 嵌入 L2 的 WebView2 | Vue 3、Naive UI、Tailwind | `client/src/` |

---

## 2. 模块索引

### L1 · `engine-net/src/`

```
http/       client.rs  h2.rs  tls.rs  proxy.rs  redirect.rs  session.rs
dns/        resolver.rs（hickory-dns）  connect.rs（Happy Eyeballs）
download/   scheduler.rs（SegmentManager）  worker.rs
progress/   sqlite_control.rs  control.rs（.aria2）  migrate.rs  session.rs
rpc/        methods.rs  types.rs
task/       registry.rs（GID / 事件）  event.rs  cancel.rs
            backpressure.rs  error.rs  log.rs
```

### L2 · `client/src-host/src/`

```
main.rs             窗口 / WebView / IPC 注册 / 单实例
lib.rs              RPC 命令面、引导、WS 事件循环、配置下发
bin/aria2_service.rs  服务入口、引擎生命周期、build_args
router.rs           EngineMode / uri_target / GID 路由表
rpc_client.rs       自研 WS JSON-RPC 客户端（重连 + 退避）
service_client.rs / service_protocol.rs   命名管道 IPC
db.rs               任务历史（SQLite / WAL / 迁移）
paths.rs            HOME 解析（便携 / 安装 / LocalSystem）
degradation.rs      单线程降级与回滚
process_guard.rs    Job Object 守护
plugin/             QuickJS / Wasm 插件运行时与审计
web_ui.rs           内置静态服务
update_check.rs     版本清单检查
```

### L3 · `client/src/`

```
App.vue  ipc.ts  router.ts
views/   DashboardView  DownloadsView  SettingsView  PluginsView  TrackersView  NotificationsView
stores/  downloads  system  plugins  notifications  intercept  theme
components/  NodeProgressBar（多段进度）  ThreadMatrixCard  UpdateNotifier  Sidebar  TopBar
```

---

## 3. 一次 HTTP 下载的调用链

```
L3  DownloadsView  ──invoke('add_download')──►  L2 main.rs
                                                │
                                      router::uri_target(uri)
                                                │
                    ┌───────────────────────────┴──────────────────────┐
                    ▼ native                                          ▼ aria2
            engine-net::TaskRegistry                          转发 :6801 JSON-RPC
            scheduler 切分 → worker 拉取                        aria2c 原生调度
                    │                                                  │
                    └────────────► gid + 事件 ◄────────────────────────┘
                                        │
                          db.upsert_task（历史）/ Router 汇聚通知
                                        │
                            管道事件 ──► L3 进度条刷新
```

## 4. 一次 BT 下载的调用链

```
magnet / .torrent ──► uri_target = aria2 ──► aria2c :6801 ──► DHT / PEX / 分块
                                             │
                        tellStatus / onDownload* 由 Router 装饰 _engine 后下发
```

---

## 5. 三条必须记住的机制

| 机制 | 一句话 | 关键代码 |
| --- | --- | --- |
| 动态切割 | 慢分片被抢占重切，速度长期不行就自适应放大阈值直至关闭 | `download/scheduler.rs`、`SegmentMan.cc` |
| 断点接管 | 载入什么控制格式就写什么格式，`engine` 可随时互换 | `progress/mod.rs` |
| 配置全量下发 | 每次连上引擎都重放 `config.json`，不依赖「保存」动作 | `lib.rs::apply_saved_globals`、`sync_engine_config` |

---

## 6. 不可破坏的不变量

1. 一个 GID 只属于一个引擎；`gid → engine` 只在 `addUri` 成功后落表
2. `6800` 是唯一外部 RPC 入口，`6801` 只绑回环且带随机 secret
3. 引擎全局选项在进程重启后必然丢失 → 连接时必须全量重放
4. 服务（LocalSystem）与 UI 必须解析出**同一个** HOME（WTS 交互式环境块）
5. `aria2c.exe` 必须挂在 Job Object 下，UI 异常退出不得留孤儿
6. SQLite 迁移只增不改；库版本高于程序时直接报错，不静默降级
7. `engine-net` 不实现 BT；BT 相关改动只进 `aria2-1.37.0/`

---

## 7. 扩展点

| 想做的事 | 改哪里 |
| --- | --- |
| 新增 RPC 方法 | `engine-net/src/rpc/methods.rs` + `client/src-host/src/router.rs` 装饰 |
| 新增协议 | `router.rs::uri_target` + 对应 Client 实现 |
| 新增设置项 | `config.json` 读写（`lib.rs`）+ `apply_saved_globals` 重放 + `SettingsView.vue` |
| 新增进度格式 | `progress/` 下新增 Store 实现，保持「载入格式优先」 |
| 插件能力 | `client/src-host/src/plugin/hostapi.rs` + 审计白名单 |
