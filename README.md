<div align="center">

# Aria2-Harness

**把下载器拆成三层，每层都用最合适的语言重写一遍。**
Rust 写 HTTP 栈、C++ 守 BT 老本行、Vue 画界面 —— 没有 Tauri，没有 Node 运行时，没有妥协。

<p>
  <img alt="platform" src="https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-0078D4?style=flat-square">
  <img alt="rust" src="https://img.shields.io/badge/Rust-2024%20edition-B7410E?style=flat-square&logo=rust">
  <img alt="vue" src="https://img.shields.io/badge/Vue-3-42B883?style=flat-square&logo=vuedotjs">
  <img alt="license" src="https://img.shields.io/badge/license-MIT%20%2B%20GPLv2%2Fv3-blue?style=flat-square">
  <img alt="size" src="https://img.shields.io/badge/Lite-14%20MB-brightgreen?style=flat-square">
  <img alt="version" src="https://img.shields.io/badge/version-1.0.0--rc1-orange?style=flat-square">
</p>

</div>

---

## 为什么又造一个 aria2 前端？

因为现存的方案都在某处将就：要么是 Electron 外壳拖着 100MB 的 Chromium，要么只读 aria2 的 RPC 做个皮，要么纯 Rust 重写却在 BT 上从头踩一遍十几年的坑。

Aria2-Harness 的选择是**分层混合** —— 每一层只做自己最擅长的事：

```text
              ┌────────────────────────────────────────────────┐
              │   浏览器 (Chrome / Edge) · MV3 扩展             │
              └───────────────────┬────────────────────────────┘
                                  │  HTTP 127.0.0.1:1780x
                                  │  /check · /probe · /add
┌─────────────────────────────────▼──────────────────────────────┐
│  UI 层      aria2-harness.exe     tao + wry + Vue 3 + Pinia    │  普通权限
│             WebView2 渲染 · 系统托盘 · 单实例互斥 · 虚拟列表     │
└─────────────────────────────────┬──────────────────────────────┘
                                  │  命名管道 IPC（JSON，双向流）
┌─────────────────────────────────▼──────────────────────────────┐
│  服务层     aria2-service.exe     Rust 2024 · LocalSystem      │  提权：falloc
│                                                                 │
│   JSON-RPC :6800 ── TaskRouter ──┬── Harness-Net (Rust)          │  HTTP/1.1 + HTTP/2
│                                  ├── aria2c      (C++)          │  BT / magnet / metalink
│                                  ├── QuickJS     (沙盒)          │  插件 · 每插件一线程
│                                  └── FFmpeg      (共享池)        │  HLS / DASH / 直播 / 转码
│                                                                 │
│   browser_api :1780x  ·  SQLite 历史  ·  Job Object 守护进程树   │
└─────────────────────────────────────────────────────────────────┘
```

对外只暴露两个面：UI 走命名管道，引擎与插件走 `:6800` JSON-RPC，浏览器走 `:1780x` HTTP。内部谁接活、按什么优先级接活，是 `TaskRouter` 自己的事。

---

## 核心特性

### 🚀 极致轻量：剥离 Tauri，Lite 版整包 14MB

Tauri 很好，但它把整个 WebView2 固定运行时（约 180MB）塞进安装包。这里直接和 `tao` / `wry` 打交道 —— 窗口自己创建，WebView 用系统的，前端产物是一坨静态文件由内置服务器托管。**Lite 安装包 14MB**，VC++ 运行库随包，双击即用，不写注册表。

### ⚡ 纯血 Rust 引擎：重写 HTTP 层，吞吐量是原版 3~4 倍

`engine-net`（代号 **Harness-Net**）不是 aria2 的翻译版，是重写版：

- `hyper` 负责 HTTP/1.1 + HTTP/2（h2 只走 TLS ALPN，不做 h2c）
- `rustls` + `rustls-platform-verifier`：信任根直接读 Windows 证书存储，企业内网 CA 不再报错
- `hickory-resolver` 替换 c-ares，DNS 解析不再阻塞事件循环
- 多段连接池 + 零拷贝写入，省掉 C++ 侧反复的 `read/write` 与锁开销

`--engine=auto`（默认）由 TaskRouter 分流：纯 HTTP(S) 直链交给 Harness-Net，`.torrent` / magnet / metalink 交给 aria2。强制回滚随时可用：`--engine=aria2` 与旧版行为逐字节一致。

### 🧩 微内核插件生态：QuickJS 沙盒 + 无代码配置表单

插件 = `manifest.json` + `main.js`，跑在 **QuickJS 沙盒**里，只暴露 `harness` / `ctx` / `fetch` / `console`：

- **任务路由**：manifest 声明 `routing` 规则（通配 / 正则 / 优先级），`TaskRouter` 按 `(规则优先级, 插件优先级, id)` 升序认领，插件之间不抢活
- **插件管理中心**：列表 / 启停 / 卸载 / 热重载 / 日志 / 熔断状态一屏可见；每插件独立线程 + 有界队列 + 超时预算，崩了只熔断它自己
- **`settings_schema` 动态表单**：`string` / `text` / `textarea` / `boolean` / `number` / `select` 六种字段，前端纯 schema 驱动渲染 —— 插件作者不写一行 UI 代码就有配置面板
- **权限最小化**：`exec:ffmpeg` / `exec:local` / `notify` 默认全部拒绝，安装时逐项确认；`exec:local` 会弹红色安全警告，拒绝即零残留
- **完整性与安装**：`integrity.files` 逐文件 SHA-256 校验；`.zip` 直接拖进窗口即可安装

### 🎬 全能媒体处理：自带 FFmpeg 共享池

Full 版内置 **FFmpeg 共享池**（`ffmpeg.exe` + `ffprobe.exe` + `COPYING.GPLv3`），所有媒体插件复用同一份，不重复占空间：

- **网页媒体嗅探**：MV3 扩展劫持 `fetch` / XHR + `<video>` 扫描，页面主世界注入悬浮窗，B站 / YouTube 等站点一键接管
- **HLS / DASH 合并**：`.m3u8` 分片直拉与封装，`.m4s` 音视频双轨 `-map` 合流
- **直播录制**：HTTP-FLV / RTMP / RTSP / HLS，无限进度条 + 「停止录制」落盘保存，`-headers` 三件套透传防盗链
- **本地转码**：容器封装 / 编码转换，同样走共享池

### 🛡️ 无缝兼容与防崩溃

升级不是「重新开始」：

- `aria2.session` 原样读取，旧任务继续下；任务历史落在自己的 SQLite（WAL + `user_version` 版本化迁移，**拒绝降级**而不是静默跑错逻辑）
- **Job Object 进程树守护**：aria2c / FFmpeg 全部挂在服务进程的 Job 上，宿主崩溃时内核直接收尸，绝不留僵尸进程
- **启动全量重放配置**：`config.json` 里的 `dir` / `split` / 并发 / 限速 / 代理在启动时一次性灌给引擎，UI 显示什么，引擎就用什么
- 覆盖安装时 `data/` / `config.json` / `downloads/` 标记 `uninsneveruninstall`，Full 与 Lite 可任意互切

---

## 安装：Full 还是 Lite？

| 版本 | 体积 | 适合谁 | 内容 |
| --- | --- | --- | --- |
| **Full** | ~240 MB | 小白 / 纯净系统 / 内网离线 | 内置 **WebView2 Fixed Runtime**、4 个官方插件、FFmpeg 共享池 —— 装完开箱即用 |
| **Lite** | **14 MB** | 极客 / 已有 WebView2 的 Win10·11 | 纯净主程序，插件与共享组件按需拖入 |

> 两者可任意覆盖安装，用户数据（任务列表、`config.json`、下载目录、插件配置）不会被触碰。

### 浏览器扩展

1. 打开 `chrome://extensions`（Edge 为 `edge://extensions`），开启「开发者模式」；
2. 「加载已解压的扩展程序」→ 选择安装目录下的 `browser-extension`；
3. 工具栏图标角标显示 **ON** 即已连上桌面端（ERR = 软件未启动）。

扩展拿不到 Token 或桌面端没开时，一律**放行浏览器原生下载** —— 接管失败绝不能导致文件下不了。

### 插件：拖拽即装

Lite 版或想单独升级插件时，把 `.zip` **直接拖进客户端窗口**即可（也可以走 设置 → 插件 → 导入插件）：

| 拖入这个 | 得到什么 |
| --- | --- |
| `web-media-extractor.zip` | 网页视频提取：悬浮窗接管、DASH 音视频合流、直链下载 |
| `live-recorder.zip` | 直播录制：HTTP-FLV / RTMP / RTSP / HLS，停止即保存 |
| `hls-downloader.zip` | HLS 分片下载与合并 |
| `media-converter.zip` | 本地转码 / 封装 |
| `shared-ffmpeg.zip` | **共享组件**：FFmpeg + FFprobe 二进制池，上面三个媒体插件都靠它 |

注意事项：

- zip 为**平铺结构**（`manifest.json` 与 `main.js` 在根层），安装后落到 `data/plugins/<id>/`；
- 升级请**先卸载旧版再导入**，避免两个同名 Hook 同时响应同一个任务；
- `shared-ffmpeg` 安装一次即可，路径 `data/plugins/.shared/com.harness.shared-ffmpeg/`；
- 插件声明 `exec:local` 时安装会弹安全确认，取消则安装目录零残留。

---

## 从源码构建

### 环境要求

| 组件 | 版本 | 用途 |
| --- | --- | --- |
| Windows | 10/11 x64 | 目标平台 |
| Rust | stable（edition 2024） | 宿主双进程 + Harness-Net |
| LLVM / Clang | 15+ | `rquickjs`（QuickJS C 源码）、`rusqlite` 与 gnullvm 链接 |
| Node.js | 20+ | 前端构建 |
| MSYS2 mingw64 | 最新 | 编译 aria2 C++ 引擎与 FFmpeg |
| Inno Setup | 6 / 7 | 打安装包（可选） |

### 1. 前端

```powershell
cd client
npm ci
npm run build          # 产物在 client/dist
```

### 2. Rust 宿主（默认 MSVC 工具链）

```powershell
cd client/src-host
cargo build --release --features native-http,wasm-runtime
```

产物：`target/release/aria2-harness.exe`（UI 进程）、`target/release/aria2-service.exe`（核心服务）。

### 3. Rust 原生引擎

```powershell
cd engine-net
cargo build --release
```

### 4. 纯 Clang 路线（gnullvm，可选）

仓库里的 `client/.cargo/config.toml` 已给出模板，需要自己补齐三件事：

```powershell
# 1) rust-lld 不在 PATH，把它所在目录加进来
$env:PATH = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin;$env:PATH"
# 2) libclang 给 bindgen
$env:LIBCLANG_PATH = "C:\LLVM\bin"
# 3) clang 需要 MinGW sysroot（找 stdlib.h 与 advapi32 导入库）
$env:PATH = "C:\msys64\mingw64\bin;$env:PATH"

cargo build --release --target x86_64-pc-windows-gnullvm --features native-http,wasm-runtime
```

gnullvm 下必须保留 `-Clink-self-contained=yes` 与 `+crt-static`，否则运行时会报 `0xC0000135`（`STATUS_DLL_NOT_FOUND`）。

### 5. aria2 魔改引擎（C++）

在 **MSYS2 mingw64** 终端里，先按 `aria2-1.37.0/mingw-build-memo` 静态编译依赖：OpenSSL、GMP、SQLite、c-ares、libssh2、zlib、expat。然后：

```bash
cd aria2-1.37.0
./configure --prefix=/path/to/install --disable-nls --enable-static --disable-shared ARIA2_STATIC=yes
make -j$(nproc)
strip src/aria2c.exe
```

得到约 13.8MB 的静态 `aria2c.exe`，放进 `client/src-host/binaries/`。

### 6. 组装便携包与安装包

```text
Aria2-Harness-v1.0.0-rc1-Windows-x64/
├── aria2-harness.exe / aria2-service.exe
├── aria2c.exe
├── WebView2Loader.dll + VC++ 运行库
├── dist/                 # 前端产物
├── browser-extension/    # MV3 扩展，免构建直接加载
└── data/plugins/         # 官方插件 + .shared/FFmpeg 池（Full）
```

```powershell
ISCC.exe setup.iss           # Full（内嵌 WebView2 运行时 + 插件 + FFmpeg 池）
ISCC.exe /DLITE setup.iss    # Lite（依赖系统 WebView2）
```

---

## 数据落在哪

| 形态 | 判定 | HOME |
| --- | --- | --- |
| 便携版 | exe 同级存在 `dist/index.html` | exe 所在目录 |
| 安装到 Program Files | 目录不可写 | `%LOCALAPPDATA%\Aria2-Harness` |
| 安装到其他可写目录 | 目录可写 | 安装目录 |

HOME 下的结构：

```text
<HOME>/
├── config.json          # 用户设置（UI 与引擎的唯一真源）
├── data/
│   ├── aria2-harness.db # 任务历史（SQLite / WAL）
│   ├── aria2.session    # 引擎会话（断点续传）
│   └── logs/            # app.log / service.log（按天或按大小轮转）
├── plugins/             # QuickJS 插件 + .shared/ 共享组件
└── downloads/
```

> LocalSystem 服务进程不会去 `systemprofile` 里翻目录 —— 它会通过 **WTS** 拿到**交互式用户**的环境块，算出的 HOME 与 UI 进程完全一致。这是 beta8 修的坑之一，rc1 继续沿用。

---

## 踩坑记录（写给下一个改它的人）

| 现象 | 真因 | 处理 |
| --- | --- | --- |
| `0xC0000135` 启动即退 | gnullvm 目标丢了自包含链接参数 | 保留 `-Clink-self-contained=yes` + `+crt-static` |
| 直播任务无限点播、永不完结 | 直播间 `.m3u8` 被当成普通点播，插件永不 `complete` | 扩展在直播页改投 `/add` 真实流地址，`live-recorder` 以规则优先级 5 独占认领 |
| B站直播「未抓到地址」 | 播放走 MSE，真实地址藏在 `blob:` 后 | 扩展塞 `bilibili_live_blob` 标记，插件直接给出可执行提示，**绝不空转 FFmpeg** |
| 直播录制 3 分钟断流 | 直播源 301/302 跳转与瞬时丢包 | 参数恒带 `-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5` |
| 插件超时卡住整个宿主 | `onHolo` 只有 5s 预算 | 长任务走 `claim → onTaskRun`（30 分钟预算），Hook 只做轻量判定 |
| 插件安装后目录残留 | 安全确认被拒绝时未清理 staging | 落盘前先做 `exec:local` 确认，拒绝即整目录删除 |

---

## 测试

```powershell
cd client/src-host
cargo check --features native-http,wasm-runtime
cargo test  --features native-http,wasm-runtime

cd ../../engine-net
cargo test  --features native-http
```

`engine-net` 的 A/B 一致性测试会把同一 URL 分别交给 Harness-Net 与 aria2c，比对落盘文件的 SHA-256。

---

## 目录结构

```text
aria2-harness/
├── engine-net/            # Harness-Net：原生 HTTP(S) 引擎（tokio/hyper/rustls/hickory）
├── client/
│   ├── src/               # Vue 3 前端（视图 / Pinia / IPC 封装）
│   ├── src-host/          # Rust 宿主（lib + 双 bin + 插件系统 + SQLite）
│   │   ├── src/           # main / service / router / rpc_client / plugin / paths ...
│   │   ├── binaries/      # 静态链接的 aria2c.exe（构建产物，不入库）
│   │   └── assets/        # exe 图标与资源段（WebView2 运行时不入库）
│   └── dist/              # 前端构建产物（不入库）
├── aria2-1.37.0/          # aria2 魔改分支（C++，GPLv2）
├── browser-extension/     # Manifest V3 扩展，接管浏览器下载
├── official-plugins/      # 4 个官方插件源码（manifest.json + main.js）
├── docs/architecture.md   # 双进程模型 / IPC / 提权与预分配的设计说明
├── PLUGIN-DEV.md          # 插件开发白皮书
├── THIRD-PARTY-NOTICES.md # 第三方组件协议声明
└── setup.iss              # Inno Setup 脚本（Full / Lite 双编译分支）
```

---

## 许可证

- 自研部分（`engine-net/`、`client/`）：**MIT**
- `aria2-1.37.0/`：aria2 的修改分支，仍受 **GPLv2** 约束；分发含 `aria2c.exe` 的二进制包时请履行 GPLv2 的源码提供义务
- **FFmpeg**（`ffmpeg.exe` / `ffprobe.exe`）：`--enable-gpl --enable-version3` 构建，受 **GPLv3** 约束，协议全文随包（`COPYING.GPLv3`）
- WebView2 Fixed Runtime（仅 Full）：Microsoft EULA

完整清单、版本、构建配置与源码获取方式见 [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)。

> 本项目仅供学习与个人使用。请遵守目标站点的服务条款与当地法律法规，不要用于任何侵权用途。

---

<div align="center">
<sub>Built with Rust · Clang · QuickJS · a lot of patience for BT peers.</sub>
</div>
