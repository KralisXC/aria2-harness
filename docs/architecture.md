# Aria2-Harness · 核心服务架构（双进程模型）

> 长期架构文档 · 首版由增量 23（核心服务架构）与增量 24（提权自检）沉淀而成。
> 定位：描述 **UI 进程** 与 **核心服务进程（harness-service）** 的职责边界、IPC 协议、引擎接管方式，
> 以及 `falloc` 预分配为何必须由高权限服务执行。**做法在同源代码里；本文只讲"为什么这样拆"和"边界在哪"。**
>
> 相关决策：ADR-001（已被覆盖）、ADR-002、ADR-015。相关红线：ROADMAP §3.1 / §3.2 / §3.3。

---

## 1. 进程拆分结果

### 1.1 为什么必须拆：`falloc` 的特权依赖

`--file-allocation=falloc` 在 Windows 上的实现是 **`truncate()` 先拉逻辑长度 → 再 `SetFileValidData` 标记有效数据区**（`aria2-1.37.0/src/AbstractDiskWriter.cc`）。

- `truncate()` 只改元数据，**逻辑长度瞬间就满**，不占物理空间；
- `SetFileValidData()` 才是真·物理占位，它需要 **`SE_MANAGE_VOLUME_NAME`** 特权（`RequestGroup.cc` 中的 `util::gainPrivilege(SE_MANAGE_VOLUME_NAME)`）。

UI 进程永远普通权限，拿不到该特权 → `SetFileValidData` 必失败。因此把引擎的**启动与预分配**交给一个提权进程：**子进程继承父进程的特权令牌**，`falloc` 才能真正"瞬间占位"。

> 非管理员下 `falloc` **不会**自动降级成 `trunc`，而是退化为 **阻塞式填零**（aria2 原文 WARN：`File will be allocated by filling zero, which blocks whole aria2 execution.`，见 §3.2）。这是"必须提权"而非"可选提权"的根本原因。

### 1.2 形态：同一 package 的第二个 bin

按 ADR 取向「避免跨 crate 冗余配置」，服务**不建独立 crate**，而是作为同 package 的第二个 bin，最大化复用现有 `serde` / `tokio` / `windows` 依赖。

关键陷阱：package 内出现两个 bin 后，`tauri-cli` 内部执行的 `cargo run` 会因**目标二义性**失败（`could not determine which binary to run`），直接打断 `npm run tauri dev`。因此必须显式声明：

```toml
[package]
autobins = false                 # 关掉自动发现，显式声明两个 bin
default-run = "client"    # 指定默认目标，保住 cargo run / tauri dev

[[bin]]
name = "client"
path = "src/main.rs"

[[bin]]
name = "harness-service"
path = "src/bin/harness_service.rs"
```

`tokio` features 需扩充为 `time` / `sync` / `net`（Windows 命名管道）/ `io-util`（`AsyncReadExt`/`AsyncWriteExt`/`BufReader`）/ `macros` / `rt-multi-thread`。tokio 的 `default = []`，缺一不可。

### 1.3 文件清单

| 文件 | 性质 | 说明 |
| --- | --- | --- |
| `src-tauri/src/bin/harness_service.rs` | 新增 | 服务端：命名管道 accept 循环 + aria2c 拉起/监控/回收 + Job Object + `falloc` + **提权自检** |
| `src-tauri/src/service_protocol.rs` | 新增 | UI 与服务**共用**的 IPC 协议（请求/响应 + 编解码），含契约单测 |
| `src-tauri/src/service_client.rs` | 新增 | UI 侧命名管道客户端（连接重试 + 超时） |
| `src-tauri/src/lib.rs` | 修改 | 剥离 `spawn_aria2`；`Aria2State` 重写；新增 `bootstrap_service`；`RunEvent::Exit` 改发 `shutdown` |
| `src-tauri/src/process_guard.rs` | 修改（1 行） | `mod` → `pub mod`，供服务 bin 复用 Job Object 守护 |
| `src-tauri/src/Cargo.toml` | 修改 | `autobins`/`default-run`/两个 `[[bin]]` + tokio features 扩充 |
| `client/start_dev.ps1` | 新增 | 开发期一键启动：管理员起服务 → 普通权限起 UI（纯 ASCII，见 §6） |

### 1.4 职责边界

| 职责 | UI 进程（普通权限） | 核心服务（管理员权限） |
| --- | --- | --- |
| 界面 / Tauri 命令 | ✅ | ❌ |
| 连接 aria2 WS RPC | ✅（用服务回传的密钥） | ❌ |
| 拉起 / 监控 / 销毁 `aria2c.exe` | ❌ | ✅ |
| 生成 RPC 密钥 | ❌ | ✅ |
| 端口占用探测（只报错不 kill） | ❌ | ✅ |
| `--file-allocation=falloc` | ❌ | ✅ |
| Job Object 守护 | ❌ | ✅ |
| 优雅退出 | 发 `shutdown` 指令 | 收敛引擎后退出 |

职责迁移对照（相对单体架构）：

| 原职责（单体） | 去向 | 说明 |
| --- | --- | --- |
| `spawn_aria2` / `resolve_aria2_path` / `SIDECAR_NAMES` | 删除 → 服务 | UI 不再持有引擎子进程句柄 |
| Job Object 附着 | UI 库保留 → 服务调用 | `pub mod process_guard`，两侧复用同一实现 |
| `state.child: Mutex<Option<CommandChild>>` | 删除 | `Aria2State` 不再有 `child` 字段 |
| RPC 密钥生成（UI 侧 `uuid`） | 移到服务 | 服务生成并回传，杜绝硬编码 |
| 端口占用探测（6800） | 移到服务 | 只报错不 kill，避免误杀用户手动启动的 aria2 |
| `--file-allocation=trunc` | 服务侧改为 `falloc` | 见 §3 / ADR-015 |
| `RunEvent::Exit` 里 `child.kill()` | 改为 IPC `shutdown` | 兜底语义不变，实施通道改变 |

`Aria2State` 重写后只剩三个字段，且**两个必须内部可变**（握手发生在 `setup` 之后的异步任务里）：

```rust
pub(crate) struct Aria2State {
    client: Mutex<Option<Client>>,            // tokio Mutex：异步命令里要跨 await
    secret: StdMutex<String>,                 // 服务回传的 rpc-secret
    service_error: StdMutex<Option<String>>,  // "前端零改动"前提下向前端报错的主通道
}
```

---

## 2. IPC 通信机制

### 2.1 通道与单实例闸门

- 管道名：`\\.\pipe\aria2-harness-service`，可用环境变量 `HARNESS_SERVICE_PIPE` 覆盖（红线：不写死路径）。
- 服务端用 `ServerOptions::new().first_pipe_instance(true).create(addr)`，即以 `FILE_FLAG_FIRST_PIPE_INSTANCE` 做**单实例闸门** —— 第二个服务实例会在 `create` 阶段直接失败，而非静默共存。
- 两个方向都走 `PipeMode::Byte`（默认），配合按行分帧。

### 2.2 报文协议

请求是**带内部标签的枚举**，响应是扁平结构体：

```rust
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ServiceRequest {
    StartEngine { port: u16 },   // {"cmd":"start_engine","port":6800}
    GetStatus,                   // {"cmd":"get_status"}
    Shutdown,                    // {"cmd":"shutdown"}
}

pub struct ServiceResponse {
    ok: bool,
    error: Option<String>,
    rpc_secret: Option<String>,
    engine_running: bool,
    engine_pid: Option<u32>,
}
```

**按行分帧**是本设计的关键前提：`serde_json::to_string` 不产出裸换行，因此「一条消息 == 一个物理行」，`read_line` 即可正确切分，无需长度前缀。这一点被单测 `encoded_frames_are_single_physical_lines` 与 `wire_format_is_locked` 锁死：

```
lock: {"cmd":"start_engine","port":6800}\n
```

> `wire_format_is_locked` 是一条**契约测试**：任何人给枚举改字段、改 `rename_all`、或改 `cmd` 命名，都会立刻红灯 —— 防止 UI 与服务两侧悄悄漂移。

### 2.3 时序：短连接（一次连接 = 一次请求-响应）

刻意**不做长连接**：UI 侧因此**无状态、无锁、无心跳**；服务端每处理完一个连接就立刻挂出下一个监听实例，天然抗"UI 崩溃半途断开"。

服务端主循环：

```rust
loop {
    select! {
        r = server.connect() => { r?; tokio::spawn(serve_one(server, ...)); }
        _ = stop.notify.notified() => break,     // Shutdown 指令叫醒
    }
    server = ServerOptions::new().create(pipe_name())?;
}
```

`Notify::notify_one` 有 **permit 语义**（即使被叫醒时主循环尚未进入 `notified()`，唤醒也不会丢失），因此这里**不存在竞态**，无需额外的布尔标志。

UI 侧客户端：`CONNECT_ATTEMPTS = 3`、`CONNECT_BACKOFF = 60ms`、`RESPONSE_TIMEOUT = 15s`；用 `BufReader::new(client)` 同时读+写（`BufReader<R>` 的 `AsyncWrite` 实现直接透传内层，不另设写缓冲）。

### 2.4 启动握手与降级（红线：绝不崩溃、绝不死循环）

`bootstrap_service` 在 `setup` 后以异步任务运行，最多 `SERVICE_HANDSHAKE_ATTEMPTS = 5` 次、间隔 `SERVICE_HANDSHAKE_BACKOFF = 2s`（≈ 8s"等 Service 起来"窗口）：

| 结果 | 处理 |
| --- | --- |
| `Ok(resp)` 且 `resp.ok` | 存 secret、清 `service_error`、进入 `run_ws_loop` |
| `Ok(resp)` 但 `ok=false` | **立即 break**（服务可达却起不来引擎，重试无意义） |
| `Err`（连不上管道） | 记录 + 退避重试；用尽后停下 |

用尽后**不崩溃**，而是把可读原因写入 `service_error`，通过**两条既有通道**触达前端（前端零改动）：

1. `aria2://event` 的 `disconnected` 事件 → 前端徽标转"未连接"；
2. `client_or_err` 优先抛出 `service_error` → 各引擎命令原样回传 → 前端提示
   「…请以管理员身份启动核心服务 harness-service（运行 start_dev.ps1，或直接运行 harness-service.exe），然后重试。」

### 2.5 退出级联（三进程"全消失"）

```
关窗 → UI RunEvent::Exit → block_on 发 ServiceRequest::Shutdown
     → aria2 收到 shutdown，打印 "Serialized session ... successfully" 自行退出
     → [svc] 收到 shutdown 指令
     → 服务先轮询 15×100ms 等引擎自行退出（期间不持锁），超时才强杀
     → [svc] 引擎未自行退出则强制终止（pid=...）
     → [svc] harness-service 已退出
```

第二道保险是 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）：即便 UI 被任务管理器**强杀**（`RunEvent::Exit` 不执行），服务进程一旦消失，内核也会连带 kill 掉 Job 内的 `aria2c`。

---

## 3. 文件预分配（`falloc`）与判据

### 3.1 管理员组实测（提权服务）

- aria2c 由服务拉起，PID 与 `[svc] 引擎已由核心服务拉起（pid=Some(...)）` 一致；
- 命令行确认带 `--file-allocation=falloc`：

```
aria2c.exe --enable-rpc=true --rpc-listen-all=false --rpc-listen-port=6800 --rpc-secret=...
  --dir=<repo>\downloads --max-connection-per-server=16 --file-allocation=falloc
  --input-file=<repo>\downloads\aria2.session --save-session=... --save-session-interval=15 --auto-save-interval=5
```

- **预分配实测（核心证据）**：下载刚开始、`completedLength` 很小时，

```
fsutil file queryvaliddata <repo>\downloads\<test>.bin
→ Valid data length is 0x40000000 (1073741824)
```

VDL 立即等于文件总长度 1GB → `SetFileValidData` 真实生效，`falloc` 满血复活。aria2 日志中**不再出现** `Gaining privilege SeManageVolumePrivilege failed`。

> 测速口径：本地测试服务器（支持 Range、**全局合计**限速 2 MB/s）提供 1GB 全零文件，1GB 理论耗时 ≈ 512s。VDL 满占的时刻下载进度不足 1% —— 以此把"预分配瞬时完成"与"下载已经下完"彻底区分开。

### 3.2 对照组（非管理员，无提权）

同一套代码、同一文件，只是不给服务提权：

```
fsutil file queryvaliddata ...\e2e_big.bin
→ Valid data length is 0x6400000 (104869888)     ≈ 100 MiB
```

aria2 日志给出决定性解释：

```
[WARN] Gaining privilege SeManageVolumePrivilege failed.
[WARN] --file-allocation=falloc will not work properly.
[WARN] File allocation (SetFileValidData) failed (cause: A required privilege is not held by the client.).
       File will be allocated by filling zero, which blocks whole aria2 execution.
```

### 3.3 判据：必须看 VDL，而不是文件长度

| 场景 | 逻辑长度（文件大小） | VDL | 结论 |
| --- | --- | --- | --- |
| 管理员 + `falloc` | 满 | **满（= 1073741824）** | ✅ `SetFileValidData` 生效，瞬时物理占位 |
| 非管理员 + `falloc` | 满（`truncate` 拉的） | 小（≈已写入量） | ❌ 特权缺失，退化为**阻塞式填零** |

**文件长度 ⊥ 预分配是否生效；VDL 才是唯一判据。**（`truncate()` 只改元数据，逻辑长度瞬间就满，是伪信号。）

### 3.4 旁证：会话文件记为 `falloc`

测试后 `aria2.session` 中该任务被原样持久化，其中一行即 `file-allocation=falloc` —— `falloc` 不仅是命令行参数，也已成为 aria2 对该任务的持久化属性。

---

## 4. 提权自检（增量 24）

### 4.1 动机

§3.2 证明：服务未被提权时 `falloc` 会**退化为阻塞式填零**（拖死整个引擎），且**没有任何提示** —— 用户双击 `harness-service.exe` 即会无声踩进性能悬崖。

### 4.2 策略：拒绝启动，而非降级运行

在 `harness_service.rs` 的 `main` **最前面**（拉起任何线程之前）做提权检测；未提权则打印醒目红色 WARN 并 `std::process::exit(1)`：

```
================================================================
[svc][严重] 未以管理员身份运行，拒绝启动！
[svc][严重] 大文件预分配将退化为「阻塞式填零」，拖死整个引擎。
[svc] 请关闭本窗口，右键 harness-service.exe →「以管理员身份运行」，
[svc] 或直接运行项目根目录的 start_dev.ps1。
================================================================
```

### 4.3 实现要点

- 判定依据：进程令牌的 `TokenElevation`（`OpenProcessToken` + `GetTokenInformation`）。**拿不到令牌时按最保守处理（视为未提权）。**
- **刻意不用 `#[tokio::main]`**：多线程 runtime 在 `Builder::build()` 时已启动 worker 线程，而 `#[tokio::main]` 会把 runtime 的创建放在函数体之前 —— 做不到"零线程"。故改为 `fn main()` 先自检，再手工 `Builder::new_multi_thread().enable_all().build()` + `block_on(async_main())`。
- 依赖：`windows` crate 的 `Win32_Security` feature（已启用，无需新增）。
- 退出码为 `1`，便于脚本/CI 判定失败。

### 4.4 命名管道安全：DACL + SACL 两道闸门（增量 24 实机排查）

**症状**：UI（普通权限 = Medium 完整性）连 `\\.\pipe\aria2-harness-service` 连续报
`[service] 第 X/3 次连接失败：拒绝访问。 (os error 5)`，最终降级为
「下载服务未就绪：连接核心服务失败…」。

**根因**：服务以 **High 完整性**（管理员）运行，UI 永远 **Medium**。Medium 进程想连上
High 进程创建的管道，必须**同时**通过两关：

| 闸门 | 管什么 | 默认行为 | 放行手段 |
| --- | --- | --- | --- |
| **DACL**（自主访问控制） | 能不能**打开**管道 | 只给 SYSTEM / Administrators / 创建者完全控制，Everyone 只读 → 请求 `GENERIC_READ+WRITE` 被拒 | SDDL 的 `D:` 段显式授权 |
| **SACL**（强制性完整性标签） | 能不能**写入**管道 | 标签随创建者 = **High**，Medium 的写请求属 "write-up"，被内核直接拦掉 | SDDL 的 `S:(ML;;NW;;;S-1-16-8192)` 把标签降为 Medium |

最终使用的 SDDL（`harness_service.rs` 的 `PIPE_SDDL`）：

```
D:(A;;GRGW;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;S-1-16-8192)
```

- `WD` = Everyone（比 `AU` 更宽，排除"用户不在 Authenticated Users 组"这类边角情况）；
- `S-1-16-8192` = 中等完整性（等价于 `ME`，用完整 SID 写法便于与日志比对）。

**三个必须记住的硬事实**：

1. **两道闸门缺一不可**。只加 DACL 不够 —— 高完整性进程创建的命名管道默认标签也是 High，
   仅放行 DACL 仍会因 write-up 被 MIC 拦掉，报的依旧是 `os error 5`。
2. **`SeSecurityPrivilege` 必须显式启用，且必须在最前面**。它在管理员令牌里**默认禁用**；
   未启用时，`ConvertStringSecurityDescriptorToSecurityDescriptorW`（解析含 SACL 的 SDDL）与
   `CreateNamedPipe`（写入 SACL）**都会静默丢弃 SACL** —— 调用不报错、管道照建，但标签根本
   没生效。因此本项目把它当**致命前置条件**：`enable_security_privilege()` 失败即 `exit(1)`，
   绝不带病运行。
   - 坑中坑：`AdjustTokenPrivileges` 即使**没真正授予**特权也会返回成功，必须再查
     `GetLastError() == ERROR_NOT_ALL_ASSIGNED(1300)` 才能确认。
3. **必须有回读证据**。`GetSecurityInfo(SE_KERNEL_OBJECT, DACL|LABEL)` +
   `ConvertSecurityDescriptorToStringSecurityDescriptorW` 打出管道上**实际生效**的 SDDL；
   若 `S:` 段缺失，再用 `SetSecurityInfo(..., LABEL_SECURITY_INFORMATION, ..., sacl)` 兜底单设
   标签并二次回读。本项目把这三步固化在首个管道实例上（`diagnose_and_repair_pipe_label`），
   一次运行即可自证。

**实机结果**（管理员服务 + 普通权限 UI）：

```
[svc] SeSecurityPrivilege 已启用（AdjustTokenPrivileges 后 GetLastError=0）
[svc][诊断] 目标管道 SDDL：D:(A;;GRGW;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;S-1-16-8192)
[svc][诊断] 建管道后实际安全描述符：D:(A;;GRGW;;;WD)(A;;GA;;;SY)(A;;GA;;;BA)S:AI(ML;;NW;;;ME)
[svc][诊断] 完整性标签已在管道上生效，无需兜底。
```

注意：系统把 `WD` 规范化为 `AI`、把 `S-1-16-8192` 规范化为 `ME` ——
**这是正常行为，不是错误**。`os error 5` 彻底消失，UI 成功连接并拉起引擎，
且**未走兜底路径**（说明 `SECURITY_ATTRIBUTES` 一次就写进去了）。

### 4.5 残留高权限进程锁死管道（真凶）

**现象**：上面所有修复都做完、编译也通过，`os error 5` 依旧存在；并且在普通权限 shell 里
`Stop-Process -Name harness-service` **报"权限不足"**。

**根因**：上一轮以管理员启动的 `harness-service.exe` **没被杀掉**，仍占着单实例管道名。于是：

1. 新服务因 `FILE_FLAG_FIRST_PIPE_INSTANCE` 创建失败 → 直接 `exit(1)`；
2. UI 连上的仍是**旧的、带病配置的**那个实例 → `os error 5` 照旧。

**为什么杀不掉**：残留进程是 **High 完整性**，而 `start_dev.ps1` 是从**普通权限** shell 起
`Stop-Process` —— MIC 直接拒绝跨完整性级别的终止操作。旧脚本那行"清理残留"实际上是
`-ErrorAction SilentlyContinue` 下的**静默失败**。

**修法**（已落到 `start_dev.ps1` 的 `Clear-StaleService`）：

1. 先普通权限试杀；
2. 仍在 → `Start-Process taskkill.exe -Verb RunAs -Wait`，**提请 UAC 后再杀**；
3. 仍在 → **拒绝继续启动**并打印管理员手动清理指令（避免又一轮"改了没用"的假象）。

**教训**：双进程架构下，"进程是否真的死了"必须**显式复查**（`Get-Process` 再看一遍），
不能只发一条 `Stop-Process` 就假定成功 —— 跨完整性级别的失败是**静默**的。

---

## 5. 边界红线（本项目对服务的硬约束）

1. 服务**不得**直接打包为 Windows Service（不碰 `sc.exe`）；本阶段只是常驻 Console 程序，便于终端看日志。
2. `harness-service.exe` / `client.exe` 路径**不得硬编码**；用相对路径或环境变量（`HARNESS_ARIA2_PATH` / `HARNESS_SERVICE_PIPE`）配置。
3. 引擎子进程生命周期由服务统一管理；退出时必须主动 kill，避免残留进程锁住目标文件。
4. 启动前必须探测 RPC 端口（6800）；被占用只报错、**不自动 kill**，避免误杀用户手动启动的 aria2。
5. **不得修改**底层 C++ 源码；行为调整只通过 RPC 配置与启动参数。
6. 前端 `invoke` 接口**参数签名**不得因服务拆分而变（前端零改动是硬指标）。

---

## 6. 踩坑速查（长期沉淀）

| # | 坑 | 结论 |
| --- | --- | --- |
| 1 | 双 bin → `cargo run` 目标二义性打断 `tauri dev` | 必须 `autobins = false` + `default-run` + 显式 `[[bin]]` |
| 2 | pattern guard 里调用 `&mut self` 方法 → `E0596` | 需 `&mut` 的判定不要放进 pattern guard，写进 arm 体 |
| 3 | 降级报错被拼两遍 | 指引只在 `bootstrap_service` 拼一次；客户端只回原始错误 |
| 4 | PowerShell 5.1 按 ANSI 解码 BOM-less `.ps1` → 中文引号被吃 | 脚本全文纯 ASCII；必要时另存为 UTF-8 with BOM |
| 5 | Windows `SO_REUSEADDR` 允许静默抢占式重复绑定 | 测试服务器显式 `allow_reuse_address = False` |
| 6 | 后台终端被后续命令连带杀掉 | 用 `Start-Process` 起独立进程 + 重定向日志 |
| 7 | 文件长度瞬间满 = 误判"预分配成功" | **VDL 才是判据**（§3.3） |
| 8 | 高完整性服务建的管道，Medium 的 UI 连不上（`os error 5`） | **DACL 与 SACL 两道闸门缺一不可**；只放 DACL 不够，标签默认 = High（§4.4） |
| 9 | 加了 SACL 却毫无变化、也不报错 | `SeSecurityPrivilege` 默认禁用；未启用时解析 SDDL 与建管道都会**静默剥离 SACL** —— 必须先启用，并用 `GetSecurityInfo` 回读自证（§4.4） |
| 10 | 修复已生效但症状照旧 | **残留的高完整性旧进程占着单实例管道**；普通权限 shell 的 `Stop-Process` 静默失败 —— 必须提权杀 + 复查（§4.5） |

---

## 附 · 验证命令

```powershell
cd <repo>\client\src-tauri
cargo check --all-targets      # 0 error / 0 warning
cargo test --lib               # 协议单测等
cargo build --bins             # client.exe + harness-service.exe

# 提权自检（普通权限应打印红色横幅并以退出码 1 结束）
.\target\debug\harness-service.exe ; $LASTEXITCODE

# 开发期启动（管理员服务 + 普通权限 UI）
cd <repo>\client
powershell -ExecutionPolicy Bypass -File .\start_dev.ps1

# 残留高权限服务排查（必须用「管理员身份运行」的 PowerShell，否则会被 MIC 拒绝）
Get-Process harness-service -ErrorAction SilentlyContinue | Select-Object Id, Path
taskkill /F /IM harness-service.exe
# 管道名应已消失（无输出 = 干净）
Get-ChildItem \\.\pipe\ | Where-Object Name -like '*aria2*'
```
