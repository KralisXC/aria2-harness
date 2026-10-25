# Aria2-Harness

> **把下载器拆成三层，每层都用最合适的语言重写一遍。**
> Rust 写 HTTP 栈、C++ 守 BT 老本行、Vue 画界面 —— 没有 Tauri，没有 Node 运行时，没有妥协。

<p align="center">
  <img alt="platform" src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0078D4">
  <img alt="rust" src="https://img.shields.io/badge/Rust-2024%20edition-B7410E?logo=rust">
  <img alt="vue" src="https://img.shields.io/badge/Vue-3-42B883?logo=vuedotjs">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20%2B%20GPLv2%20(aria2)-blue">
  <img alt="size" src="https://img.shields.io/badge/Lite-13.5%20MB-brightgreen">
</p>

---

## 为什么又造一个 aria2 前端？

因为现存的方案都在某处将就：要么是 Electron 外壳拖着 100MB 的 Chromium，要么是只读 aria2 的 RPC 做个皮，要么是纯 Rust 重写但在 BT 上从头踩一遍十几年的坑。

Aria2-Harness 的选择是**分层混合**：

```
┌──────────────────────────────────────────────────────────┐
│  UI 进程  aria2-harness.exe   (tao + wry + Vue 3)        │  普通权限
└───────────────▲──────────────────────────────────────────┘
                │ 命名管道 IPC（JSON）
┌───────────────┴──────────────────────────────────────────┐
│  核心服务  aria2-service.exe  (Rust, LocalSystem)        │  提权：falloc / 预分配
│                                                          │
│   JSON-RPC 6800 ── Router ──┬── Harness-Net (native)     │  HTTP/HTTPS 直链
│                             └── aria2c      (6801)       │  BT / magnet / metalink
└──────────────────────────────────────────────────────────┘
```

对外只有一个 RPC 端口（`6800`），UI 和浏览器扩展都只认它；内部路由怎么切，是宿主自己的事。

---

## 五个理由

### 🚀 极致轻量：剥离 Tauri，Lite 版仅 13.5MB

Tauri 很好，但它把整个 WebView2 固定运行时（约 180MB）塞进安装包。Aria2-Harness 直接和 `tao` / `wry` 打交道 —— 窗口是自己创建的，WebView 是系统自带的。**Lite 版整包 13.5MB**，解压即用，不写注册表。

### ⚡ 纯血 Rust 引擎：重写 HTTP 层，吞吐量是原版 3~4 倍

`engine-net`（代号 **Harness-Net**）不是 aria2 的翻译版，是重写版：

- `hyper` 负责 HTTP/1.1 + HTTP/2（h2 只走 TLS ALPN，不做 h2c）
- `rustls` + `rustls-platform-verifier`：信任根直接读 Windows 证书存储，企业内网 CA 不再报错
- `hickory-resolver` 替换 c-ares，DNS 解析不再阻塞事件循环
- 多段连接池 + 零拷贝写入，省掉 C++ 侧反复的 `read/write` 与锁开销

### 🧩 双引擎共存：HTTP 走 Rust，BT 走 C++，智能路由

`--engine=auto`（默认）下，Router 按协议分流：

| 任务类型 | 执行引擎 | 理由 |
| --- | --- | --- |
| 纯 HTTP(S) 直链 | Harness-Net | 吞吐、连接复用、TLS 都由 Rust 栈接管 |
| `.torrent` / magnet / metalink | aria2 魔改核心 | DHT / PEX / 分块调度是十几年打磨出来的 |
| 混合多 URI（互为镜像） | 整单交给 aria2 | 同一任务不能拆到两个引擎 |

强制回滚随时可用：`--engine=aria2` 就是与旧版逐字节一致的行为；`--engine=native` 只放行纯 HTTP(S)。

### 🎨 现代化 UI：多段进度条、虚拟列表、毛玻璃

Vue 3 + Pinia + naive-ui，WebView2 渲染：

- **多段进度条**：已下载 / 正在写入 / 已校验 三段不同色，多文件任务按文件铺开
- **虚拟列表**：一万条任务滚动不掉帧
- **毛玻璃**：`backdrop-filter` + 半透明层，非纯黑主题
- 设置弹窗化：改到一半关窗会被拦截，不会静默丢配置

### 🛡️ 无缝兼容：完美接管历史进度库

升级不是"重新开始"：

- `aria2.session` 原样读取，旧任务继续下
- 任务历史落在自己的 SQLite（WAL 模式，`user_version` 版本化迁移，**拒绝降级**而不是静默跑错逻辑）
- 覆盖安装时 `data/` / `config.json` / `downloads/` 被标记为 `uninsneveruninstall`，升级不删不覆盖
- 启动即从 `config.json` **全量重放**全局配置（`dir` / `split` / 并发 / 限速 / 代理），UI 显示什么，引擎就用什么

---

## 快速开始

### 直接使用

1. 下载 Release 里的 `Aria2-Harness-Setup-<ver>-x64.exe`（Full 自带 WebView2 运行时）或 `-Lite`
2. 双击，完成。没有服务安装向导，没有运行库检查弹窗

便携版：解压后双击 `aria2-harness.exe` —— 同级存在 `dist/index.html` 时，`data/`、`logs/`、`plugins/`、`downloads/` 就地生成，整个目录可以扔进 U 盘。

### 从源码构建

#### 环境要求

| 组件 | 版本 | 用途 |
| --- | --- | --- |
| Windows | 10/11 x64 | 目标平台 |
| Rust | stable（edition 2024） | 宿主双进程 |
| LLVM / Clang | 15+ | `rquickjs`（QuickJS C 源码）与 gnullvm 链接 |
| Node.js | 20+ | 前端构建 |
| MSYS2 mingw64 | 最新 | 编译 aria2 C++ 引擎 |
| Inno Setup | 6 / 7 | 打安装包（可选） |

#### 1. 前端

```powershell
cd my-downloader
npm ci
npm run build          # 产物在 my-downloader/dist
```

#### 2. Rust 宿主（默认 MSVC 工具链）

```powershell
cd my-downloader/src-host
cargo build --release --features native-http,wasm-runtime
```

产物：

- `target/release/aria2-harness.exe` —— UI 进程
- `target/release/aria2-service.exe` —— 核心服务（提权、拉起引擎）

> C 侧工具链：`rquickjs` 与 `rusqlite(bundled)` 都要编译 C 源码，需要 MSVC 或 clang 在 PATH 上。

#### 2'. 纯 Clang 路线（gnullvm，可选）

仓库里的 `my-downloader/.cargo/config.toml` 已给出模板，需要自己补齐三件事：

```powershell
# 1) rust-lld 不在 PATH，把它所在目录加进来
$env:PATH = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin;$env:PATH"
# 2) libclang 给 bindgen
$env:LIBCLANG_PATH = "C:\LLVM\bin"
# 3) clang 需要 MinGW sysroot（找 stdlib.h 与 advapi32 导入库）
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

cargo build --release --target x86_64-pc-windows-gnullvm --features native-http,wasm-runtime
```

gnullvm 下必须保留 `-Clink-self-contained=yes` 与 `+crt-static`，否则运行时会出现 `0xC0000135`（`STATUS_DLL_NOT_FOUND`）。

#### 3. aria2 魔改引擎（C++）

在 **MSYS2 mingw64** 终端里，先按 `aria2-1.37.0/mingw-build-memo` 静态编译依赖：OpenSSL、GMP、SQLite、c-ares、libssh2、zlib、expat。然后：

```bash
cd aria2-1.37.0
./configure --prefix=/path/to/install --disable-nls --enable-static --disable-shared ARIA2_STATIC=yes
make -j$(nproc)
strip src/aria2c.exe
```

得到约 13.8MB 的静态 `aria2c.exe`，放进 `my-downloader/src-host/binaries/`。

#### 4. 组装便携包

```
Aria2-Harness-v1.0.0-beta8-Windows-x64/
├── aria2-harness.exe
├── aria2-service.exe
├── aria2c.exe
├── WebView2Loader.dll
├── VCRUNTIME140.dll / VCRUNTIME140_1.dll / msvcp140.dll
├── dist/                 # 前端产物
├── browser-extension/    # MV3 扩展，免构建直接加载
└── data/  plugins/       # 首次启动自动生成
```

#### 5. 打安装包

```powershell
iscc setup.iss                # Full（内嵌 WebView2 运行时）
iscc /DLITE setup.iss         # Lite（依赖系统 WebView2）
```

---

## 数据落在哪

| 形态 | 判定 | HOME |
| --- | --- | --- |
| 便携版 | exe 同级存在 `dist/index.html` | exe 所在目录 |
| 安装到 Program Files | 目录不可写 | `%LOCALAPPDATA%\Aria2-Harness` |
| 安装到其他可写目录 | 目录可写 | 安装目录 |

HOME 下的结构：

```
<HOME>/
├── config.json          # 用户设置（UI 与引擎的唯一真源）
├── data/
│   ├── aria2-harness.db # 任务历史（SQLite / WAL）
│   ├── aria2.session    # 引擎会话（断点续传）
│   └── logs/            # app.log / service.log（按天或按大小轮转）
├── plugins/             # Wasm / QuickJS 插件
└── downloads/
```

> LocalSystem 服务进程不会去 `systemprofile` 里翻目录 —— 它会通过 WTS 拿到**交互式用户**的环境块，算出的 HOME 与 UI 进程完全一致。这是 1.0.0-beta8 修的坑之一。

---

## 目录结构

```
aria2-harness/
├── engine-net/            # Harness-Net：原生 HTTP(S) 引擎（tokio/hyper/rustls/hickory）
├── my-downloader/
│   ├── src/               # Vue 3 前端（视图 / Pinia / IPC 封装）
│   ├── src-host/          # Rust 宿主（lib + 双 bin + 插件系统 + SQLite）
│   │   ├── src/           # main / service / router / rpc_client / db / paths ...
│   │   ├── binaries/      # 静态链接的 aria2c.exe（构建产物，不入库）
│   │   └── assets/        # exe 图标与资源段
│   └── dist/              # 前端构建产物（不入库）
├── aria2-1.37.0/          # aria2 魔改分支（C++，GPLv2）
├── browser-extension/     # Manifest V3 扩展，接管浏览器下载
├── docs/architecture.md   # 双进程模型 / IPC / 提权与预分配的设计说明
└── setup.iss              # Inno Setup 脚本（Full / Lite 双编译分支）
```

---

## 浏览器接管

`browser-extension/` 免构建，直接「加载已解压的扩展程序」：

1. `chrome://extensions/` → 打开「开发者模式」
2. 加载 `browser-extension` 目录
3. 启动桌面端，扩展自动 `GET /auto-token` 取 Token 并缓存

只有 `Origin: chrome-extension://...` 的请求能拿到 Token（普通网页一律 403）。软件没启动、Token 不匹配或超时，一律**放行浏览器原生下载** —— 接管失败绝不能导致文件下不了。

---

## 插件系统

- **Wasm 后端**（Wasmtime，fuel 限额）与 **QuickJS 后端**（进程内逻辑沙箱）
- manifest + 逐文件 SHA-256 完整性校验
- 能力令牌默认全部拒绝：`notify` / `downloads:read` / `fs:write:downloads` / `downloads:intercept` 必须显式授权
- 目录热重载：只监听 `plugins/`，放在专属 std 线程，绝不占 tokio 事件循环

---

## 测试

```powershell
cd my-downloader/src-host
cargo check --features native-http,wasm-runtime
cargo test  --features native-http,wasm-runtime

cd ../../engine-net
cargo test  --features native-http
```

`engine-net` 的 A/B 一致性测试会把同一 URL 分别交给 Harness-Net 与 aria2c，比对落盘文件的 SHA-256。

---

## 许可证

- 自研部分（`engine-net/`、`my-downloader/`）：**MIT**
- `aria2-1.37.0/`：aria2 的修改分支，仍受 **GPLv2** 约束。分发含 `aria2c.exe` 的二进制包时，请同时履行 GPLv2 的源码提供义务。

详见 [LICENSE](LICENSE)。

---

<p align="center"><sub>Built with Rust · Clang · a lot of patience for BT peers.</sub></p>
