# 第三方组件与开源协议声明（THIRD-PARTY NOTICES）

Aria2-Harness 主程序（`client/`、`engine-net/`）以 **MIT** 协议发布。安装包与插件生态中还会分发下列第三方组件，它们各自遵循不同的开源协议，**其协议条款优先于本项目自身的 MIT 条款**。

---

## 1. FFmpeg（GPL v3）

| 项 | 说明 |
| --- | --- |
| 组件 | `ffmpeg.exe` / `ffprobe.exe` 及其 MinGW 运行库（`libbz2-1.dll`、`libgcc_s_seh-1.dll`、`libiconv-2.dll`、`liblzma-5.dll`、`libstdc++-6.dll`、`libvpl-2.dll`、`libwinpthread-1.dll`、`zlib1.dll`） |
| 分发位置 | Full 安装包内置 `data\plugins\.shared\com.harness.shared-ffmpeg`；独立包 `shared-ffmpeg.zip` |
| 版本 | FFmpeg N-128119-gc220739be5（2026，MSYS2 / GCC 16.2.0 构建） |
| 构建配置 | `--enable-gpl --enable-version3`（**未启用** `--enable-nonfree`，不含任何 nonfree 组件） |
| 协议 | **GNU General Public License v3.0** |
| 协议全文 | 随包提供：`data\plugins\.shared\com.harness.shared-ffmpeg\COPYING.GPLv3` |
| 官方文本 | <https://www.gnu.org/licenses/gpl-3.0.txt> |
| 源码 | <https://ffmpeg.org/download.html>（对应版本可通过 `git checkout c220739be5` 获取） |

因为启用了 `--enable-gpl --enable-version3`，该二进制为 **GPLv3** 覆盖作品。任何再分发（含随本安装包分发）都必须：

1. 附带本协议文本（已内置 `COPYING.GPLv3`）；
2. 提供或指明对应源码的获取方式（见上表）；
3. 不得对该二进制附加额外限制。

本项目对 FFmpeg 仅做**进程调用**（`ffmpeg.exe -i <url> ...`），不链接其库、不修改其源码。

---

## 2. aria2 1.37.0（GPL v2）

| 项 | 说明 |
| --- | --- |
| 组件 | `aria2c.exe`（BT / 磁力 / 传统协议引擎） |
| 分发位置 | Full / Lite 安装包均随包 |
| 协议 | **GNU General Public License v2.0** |
| 协议全文 | 仓库内 `aria2-1.37.0/COPYING` |
| 源码 | 仓库内 `aria2-1.37.0/`（完整上游源码），上游 <https://aria2.github.io/> |

aria2 作为独立的 sidecar 进程由宿主拉起并通过 JSON-RPC 通信，本项目未修改其源代码（构建参数见 `docs/` 与仓库构建说明）。

---

## 3. WebView2 Fixed Runtime（Microsoft）

| 项 | 说明 |
| --- | --- |
| 组件 | WebView2 Fixed Version Runtime |
| 分发位置 | 仅 **Full** 安装包 |
| 协议 | Microsoft 软件许可条款（可再分发，需遵守其 EULA） |
| 说明 | Lite 版不包含该运行时，直接复用系统已安装的 WebView2 |

---

## 4. 浏览器扩展与前端依赖

| 组件 | 协议 |
| --- | --- |
| Vue 3 / Vite / Naive UI 等前端依赖 | MIT（见 `client/package-lock.json` 各包 license 字段） |
| Rust crates（tokio / hyper / rustls / rquickjs / wasmtime 等） | MIT 或 Apache-2.0（见各 crate 元数据） |
| wslay（aria2 依赖，仓库内 `aria2-1.37.0/deps/wslay`） | MIT |

---

## 5. 合规提示

- 本项目**不包含** `--enable-nonfree` 构建的 FFmpeg，也不分发 libfdk-aac 等不可再分发组件。
- 若你自行替换 `shared-ffmpeg` 中的二进制，请同步替换 `COPYING.GPLv3` 并确认新构建的许可条款与本文件不冲突。
- 本项目仅供学习与个人使用；使用者需自行遵守目标站点的服务条款与当地法律法规。

---

_最后更新：v1.0.0-rc1_
