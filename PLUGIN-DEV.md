# Aria2-Harness 插件开发白皮书（1.0.0-rc1）

面向插件作者。插件运行在宿主 QuickJS 沙箱中，只暴露 `harness` / `ctx` / `fetch` / `console`，所有能力默认拒绝、按 manifest 逐项申请。

## 一、目录结构

```text
data/plugins/
├─ <插件id>/                      # 普通插件，目录名 = manifest.id
│  ├─ manifest.json              # 必需
│  ├─ main.js                    # 必需（入口文件名约定）
│  └─ bin/                       # 可选：自带 exe（exec:local，或覆盖 ffmpeg）
├─ .shared/
│  └─ com.harness.shared-ffmpeg/ # 共享包（type=shared）
│     ├─ manifest.json
│     ├─ ffmpeg.exe              # 平铺优先
│     └─ bin/ffmpeg.exe          # 兼容旧布局，次优先
├─ configs/<插件id>.json          # 用户设置（对象：key -> value）
├─ registry.json                 # 授权账本 / 启用状态（宿主维护）
├─ .install-<uuid>/              # 安装 staging（点目录，热重载忽略）
└─ .backup-<id>-<ms>/            # 覆盖安装备份（成功后删除）
```

- 日志：审计日志 `data/logs/plugin-audit.log`（10MB 轮转为 `.1`）；`ctx.log` / `console.*` 另进入内存日志控制台（每插件 500 条，重启清空）。
- 临时目录：插件自行在下载根目录内创建 `.harness-plugin-<用途>-<时间戳>/`（如示例插件的 `.harness-plugin-m4s-<ms>`），任务结束或失败时由插件 `ctx.fs.remove` 清理；`ctx.emit("plugin-task", {id,tempDir})` 上报后，删除任务时宿主会级联清理该目录。

## 二、manifest.json

### 字段说明

| 字段 | 必填 | 说明 / 校验 |
| --- | --- | --- |
| `type` | 否 | `plugin`（默认）或 `shared`；未知值拒绝。`shared` 不要求 `entry` / `main.js` / `integrity` |
| `id` | 是 | 反向域名，3–128 字符，仅 `[a-z0-9.-]`，至少两段，每段非空且不以 `-` 开头/结尾；同时作为落盘目录名 |
| `name` | 是 | 展示名，非空 |
| `description` / `author` | 否 | 插件卡片展示 |
| `icon` | 否 | 相对插件根目录的图片路径；禁止绝对路径、`..`、盘符 |
| `version` | 是 | 语义版本字符串，非空 |
| `apiVersion` | 否 | 仅接受 `"1"` 或 `"1.1"`，默认 `"1"` |
| `entry` | 普通插件必填 | 入口脚本相对路径，禁止绝对路径 / `..` / 盘符；约定 `main.js`（安装器要求根目录存在 `main.js`） |
| `engine` | 否 | `js`（默认，QuickJS）或 `wasm`（Wasmtime 后端） |
| `priority` | 否 | 默认 100，越小越先收到同一事件 |
| `hooks` | 否 | 静态订阅的 hook 名数组；未声明不会收到对应事件 |
| `permissions` | 否 | 能力令牌数组，默认全部拒绝 |
| `settings_schema` | 否 | 设置项数组，别名 `settings`；见下 |
| `actions` | 否 | 右键动作 `[{id,label}]`，id 唯一且非空 |
| `limits` | 否 | 资源上限，见下 |
| `routing` | 否 | 任务路由 `[{pattern,priority,handler}]`；命中后调用 `handler`（默认 `onTaskDetected`） |
| `activationEvents.domains` | 否 | 懒激活域名，支持 `*.example.com` 与精确主机；空数组/缺省 = 始终激活 |
| `integrity` | 否 | `{algorithm:"sha256", files:{相对路径:十六进制哈希}}`；加载前逐文件校验 |

`limits` 默认值：`timeoutMs` 2000（QuickJS 硬上限 10s）、`taskTimeoutMs` 1800000（30 分钟）、`maxHttpBytes` 8388608（宿主硬上限 8MB）、`maxTasksPerCall` 500（宿主硬上限 500）、`memoryMb` 64、`fuelUnits` 100000（Wasm）。

`settings_schema` 每项为 `{key,label,type,options,default}`；`key` 唯一非空，`type` 仅 `boolean` / `string` / `text` / `textarea` / `number` / `select`（缺省 `string`）。`text` 是 `textarea` 的别名；`select` 的 `options` 必须非空；`number` 保存时只接受 JSON 数字（字符串 `"2"` 会被拒绝）。

### 可直接使用的示例

```json
{
  "id": "com.example.media-helper",
  "name": "媒体助手",
  "version": "1.0.0",
  "description": "示例插件",
  "author": "you",
  "apiVersion": "1.1",
  "entry": "main.js",
  "engine": "js",
  "priority": 100,
  "hooks": ["onHolo", "onTaskDetected", "onAction", "onAppStart"],
  "permissions": ["net:http", "downloads:read", "downloads:write", "exec:ffmpeg", "fs:write:downloads", "notify"],
  "activationEvents": { "domains": ["*.example.com"] },
  "routing": [{ "pattern": "*.m3u8", "priority": 50, "handler": "onTaskDetected" }],
  "actions": [{ "id": "remux-mp4", "label": "重新封装为 MP4" }],
  "settings_schema": [
    { "key": "enabled", "label": "启用", "type": "boolean", "default": true },
    { "key": "quality", "label": "画质", "type": "select", "options": ["1080P", "720P"], "default": "1080P" },
    { "key": "cookie", "label": "Cookie", "type": "textarea", "default": "" },
    { "key": "note", "label": "备注", "type": "text", "default": "" },
    { "key": "threads", "label": "并发数", "type": "number", "default": 2 },
    { "key": "userAgent", "label": "User-Agent", "type": "string", "default": "" }
  ],
  "limits": { "timeoutMs": 5000, "taskTimeoutMs": 600000, "maxHttpBytes": 1048576, "maxTasksPerCall": 16, "memoryMb": 64 }
}
```

通过安装通道安装时，若缺少 `integrity`，宿主会自动为脚本文件（`js` / `mjs` / `cjs` / `wat` / `wasm`）生成 SHA-256 清单并写回 manifest；手工放入 `data/plugins/` 时必须自带清单，否则加载被拒绝。手工计算：

```powershell
(Get-FileHash -Algorithm SHA256 .\main.js).Hash.ToLower()
```

写入形式（`files` 的键是相对插件根目录的路径）：

```json
"integrity": {
  "algorithm": "sha256",
  "files": { "main.js": "2fefa9c3...（64 位十六进制）" }
}
```

## 三、ctx API

JS 侧只注入 `harness` 与 `ctx`；所有结构化调用失败时返回 `{ok:false,error}`，不会抛出到宿主。

| `ctx` API | 宿主调用 | 返回结构 | 需要权限 |
| --- | --- | --- | --- |
| `log(level,msg)` / `log(msg)` | `log` | 无（进入日志控制台） | 无 |
| `notify(title,body)` | `notify` | 无 | `notify` |
| `emit(name,payload)` | `emit` | 无；`name="plugin-task"` 时推进虚拟任务卡 | 无 |
| `settings.get(key)` | `settings.get` | 值；未声明/未设置为 `undefined` | 无 |
| `settings.all()` | `settings.all` | `{key:value}` | 无 |
| `getTask(gid)` | `getTask` | aria2 任务对象；失败为 `null` | `downloads:read` |
| `getTasks()` | `getTasks` | 活动任务数组；失败为 `[]` | `downloads:read` |
| `dispatchTask(intent,payload)` | `dispatchTask` | `{ok:true,intent}` / `{ok:false,error}` | 无 |
| `resolveUrl(url)` | `resolveUrl` | `{ok:true}` / `{ok:false,error}` | `downloads:intercept` |
| `fs.mkdir(path)` | `fs.mkdir` | `{ok:true}` / `{ok:false,error}` | `fs:write:downloads` |
| `fs.move(src,dest)` | `fs.move` | `{ok:true}` / `{ok:false,error}` | `fs:write:downloads` |
| `fs.writeText(path,content)` | `fs.writeText` | `{ok:true}` / `{ok:false,error}`；内容上限 8MB | `fs:write:downloads` |
| `fs.readText(path)` | `fs.readText` | `{ok:true,content,truncated}` | `fs:write:downloads` |
| `fs.exists(path)` | `fs.exists` | `{ok:true,exists}` | `fs:write:downloads` |
| `fs.remove(path)` | `fs.remove` | `{ok:true}` / `{ok:false,error}` | `fs:write:downloads` |
| `httpGet({url,headers,timeoutMs,responseType})` | `http.get` | `{ok:true,status,headers,body,bytes}`；`responseType:"base64"` 时 body 为 base64 | `net:http` |
| `fetch(url,init)` | `http.get` | `Promise<{ok,status,headers,body}>`，失败 reject | `net:http` |
| `addTasksAndWait(tasks,opts)` | `tasks.addAndWait` | `{ok,tasks:[{index,gid,status,errorCode,errorMessage,path}],aborted}` | `downloads:write` |
| `exec(opts)` | `exec.run` | `{ok,code,stdout,stderr,timedOut,truncated,elapsedMs,finalized,cleaned}` | `exec:ffmpeg` / `exec:local` |
| `startTask({title,url})` | `task.create` | `{ok:true,id}` | 无 |
| `downloadDir()` | `paths.downloadDir` | 下载根目录字符串；无可用目录时为空串 | 无 |

补充：

- `tasks` 每项 `{url,out,dir?,headers?,referer?}`；`out` 不得含目录分隔符或 `..`，`dir` 必须在下载根目录内。`opts` 为 `{timeoutMs?,concurrency?}`（并发 1–16，默认 8）。`status` 取值 `pending` / `active` / `complete` / `error` / `timeout` / `cancelled`。
- `exec` 的 `opts` 为 `{program,args[],cwd?,timeoutMs?,tmpPath?,outputPath?}`；`args` 以数组传递，绝不经过 shell。`tmpPath` 成功时改名为 `outputPath`（目标已存在则报错，绝不覆盖），失败/超时自动清理 `.tmp`。
- `httpGet`：仅 `http/https`，拒绝回环/私网地址（SSRF），连接+读取默认 10s、上限 15s、重定向最多 5 跳；`headers` 为对象。
- `harness.on(name, fn)` 注册 hook；`onTaskDetected` 的返回值会回传宿主（`{claim:true,title}` 表示接管，3 秒内必须返回）。

### 最小 main.js 骨架

```js
var harness = globalThis.harness;

harness.on("onAppStart", function (ctx, event) {
  ctx.log("info", "插件已启动");                 // event = { hook: "onAppStart" }
});

harness.on("onHolo", function (ctx, event) {
  var packet = event.packet;                     // {url,host,title,headers,cookie,userAgent,referer,initialState,html,networkHints}
  if (!packet || !packet.url) return;
  var hints = packet.networkHints || [];         // [{url,kind}]
  ctx.log("info", "探针命中 " + packet.host + "，线索 " + hints.length + " 条");
});

harness.on("onTaskDetected", function (ctx, event) {
  // 仅在 manifest.routing 命中时调用；event = {hook,url,options}
  if (!/\.m3u8(\?|$)/i.test(event.url)) return { claim: false };
  return { claim: true, title: "HLS 下载" };     // 宿主随后投递 onTaskRun
});

harness.on("onAction", function (ctx, event) {
  // 仅在 manifest.actions 声明且用户点击右键菜单时调用；event.task = {gid,name,path,dir}
  if (event.action !== "remux-mp4") return;
  var input = String((event.task && event.task.path) || "");
  if (!input) return;
  var r = ctx.exec({ program: "ffmpeg", args: ["-y", "-i", input, "-c", "copy", input + ".remux.mp4"], timeoutMs: 600000 });
  ctx.notify("重新封装完成", r && r.ok ? "已生成新文件" : "失败：" + (r && r.error));
});
```

其他可用 hook：`onTaskRun`（event = `{hook,task:{id,title,url,dir,outputDir,options}}`，预算取 `limits.taskTimeoutMs`）、`onTaskCancel`（event.task = `{id,title,url}`）、`onBeforeAddDownload`、`onDownloadStart` / `onDownloadPause` / `onDownloadStop` / `onDownloadComplete` / `onDownloadError` / `onBtComplete`（event = `{hook,gid}`）、`onShutdown`。

## 四、权限模型

全部能力默认拒绝：`manifest.permissions` 未声明即拒绝，拒绝会写审计日志（`plugin-audit.log` 中 `时间 - DENY - 插件ID - 操作`）并计入 `plugin_deny` 指标。

| 权限 | 语义 | 拒绝时 |
| --- | --- | --- |
| `net:http` | `ctx.httpGet` / `ctx.fetch` 出站 HTTP | `{ok:false,error:"权限不足"}` + 审计 |
| `downloads:write` | `ctx.addTasksAndWait` 批量派发并等待下载 | `{ok:false,error:"权限不足"}` + 审计 |
| `downloads:intercept` | `ctx.resolveUrl` 改写即将添加的任务 URL | `{ok:false,error:"权限不足"}` + 审计 |
| `exec:ffmpeg` | `ffmpeg` / `ffprobe`，解析顺序：插件 `bin/` → `.shared/<共享id>/`（平铺优先，`bin/` 兼容）→ 系统 `PATH` | 拒绝 + 审计 |
| `exec:local` | 执行插件自带的其它程序，双重闸门：manifest 声明 + 用户已授权 | 拒绝 + 审计 |
| `fs:write:downloads` | 全部 `ctx.fs.*`，路径白名单 = 下载根目录 + 已登记的额外任务目录 | 拒绝 + 审计 |
| `notify` | `ctx.notify` 桌面通知 | 静默丢弃 + 审计 |

另：`getTask` / `getTasks` 需要 `downloads:read`（同样默认拒绝）。`log` / `emit` / `settings` / `startTask` / `downloadDir` / `dispatchTask` 不需要权限。

### `exec:local` 重点

1. manifest 必须声明 `exec:local`，且授权快照中必须已包含该项（新增权限会默认禁用插件，待用户确认后重新启用）。
2. 安装时宿主在移动任何目录之前检测 `permissions` 中的 `exec:local`；未确认则返回错误码 `PLUGIN_CONSENT_REQUIRED`，前端弹出红色安全警告「此插件包含外部可执行文件，是否信任并安装？」，拒绝即中止且零残留，同意后带 `allowExecLocal: true` 重试一次。
3. 程序只能来自插件自身目录：`<plugin>/bin/<exe>` 或 `<plugin>/<exe>`；绝不查 `.shared/`、绝不查 `PATH`。
4. 程序名不得包含 `/`、`\`、`:` 或 `..`；命中后还会做真实路径复核，防软链接逃逸。
5. `cmd` / `powershell` / `pwsh` / `bash` / `sh` 一律拒绝，即使已授权 `exec:local`（参数数组隔离的硬约束）。
6. 所有拒绝统一走审计出口：写 `DENY` 行、插件日志错误条目、`plugin_deny` 计数，拒绝可追溯。

`exec` 的公共安全边界：默认 `cwd` 为插件根目录，显式 `cwd` 与绝对路径参数只允许插件根目录或下载根目录内；环境变量清空（仅保留 `SystemRoot`）；Windows 不弹控制台窗口；超时杀整棵进程树；stdout/stderr 各截断 1MB；stderr 逐行推送 `plugin://exec` 事件。

## 五、开发与安装流程

本地开发：

1. 将 `.zip` 或插件文件夹拖入插件管理页。zip 解压会拒绝绝对路径、盘符、`..`、符号链接；文件夹写入同样拒绝绝对路径、盘符、`..`。两者都限制单文件 64MB、总量 256MB、条目 4000。普通插件根目录必须同时有 `manifest.json` 与 `main.js`；包内每个含 `manifest.json` 的子目录都会作为独立插件安装，单个失败不影响其余。
2. 宿主先放入 staging，再移入 `data/plugins/<id>` 并在 QuickJS 中预加载；失败会删除新目录并恢复被替换的旧版本。
3. 热重载：宿主递归监听 `data/plugins/`，静默 300ms 后重载变更目录；`configs/`、点目录、`registry.json`、`*.tmp` 不触发。重载失败保留旧版本、不自动重试，并推送 `plugin://reloaded`（`status: "failed"`）。
4. 插件详情页按 `settings_schema` 自动渲染表单（boolean 开关、select 下拉、textarea/text 多行、number 数字、string 单行）。读取顺序为 schema 默认值 ← `data/plugins/configs/<id>.json`；只认 schema 中声明的 key。保存时校验类型与 select 候选，以 `tmp + rename` 原子写入配置文件，并立即刷新运行时快照（无需重载）。
5. 日志控制台实时显示 `ctx.log` / `console.log/info/warn/error/debug`（级别归一为 debug/info/warn/error）；安全审计单独写入 `data/logs/plugin-audit.log`。

发布：

- zip 平铺打包：`manifest.json` 与 `main.js` 位于 zip 根目录，不要带外层文件夹；`bin/` 等资源也在根目录下按相对路径放置。虽然宿主能自动下钻唯一的包装目录，但发布包应保持平铺。
- PowerShell 示例：`Compress-Archive -Path manifest.json,main.js,bin -DestinationPath com.example.media-helper.zip`
- 提供 `integrity`（推荐）。未提供时安装器会自动为脚本文件生成；`bin/` 等大文件不参与哈希。

## 六、networkHints（1.0.0-rc1）

扩展在用户主动触发解析时，把 MAIN world 中劫持 fetch / XHR 嗅探到的媒体线索随 `/probe` 上报；宿主清洗后放入 `event.packet.networkHints`。插件在 `onHolo` 中读取。

- 结构：`[{ url, kind }]`。
- `kind` 取值：`m3u8` / `mpd` / `m4s` / `mp4` / `flv` / `ts`；扩展原始上报的 `hls` 归一为 `m3u8`，`dash` 归一为 `mpd`。
- 上限：最多 48 条；单条 URL 最长 2048 字节；仅接受 `http://` / `https://`；按 URL 去重；非法条目与未知类型静默丢弃，不影响探针本身。
- 未上报或格式错误时为空数组，老扩展零感知。
- 注意：某些站点（如 B 站）的线索可能只有视频流、缺音轨，直接消费会得到无声视频；示例插件因此对 B 站保留原生解析，只对其它站点按 `m3u8 > mp4 > m4s` 优先级消费。

消费线索的最小骨架（简化自 `data/plugins/m4s-merger/main.js`）：

```js
var hints = (event.packet.networkHints || []).filter(function (h) {
  return /^https?:\/\//i.test(h.url) &&
    (h.kind === "m3u8" || h.kind === "mp4" || h.kind === "m4s");
});
if (hints.length > 0) {
  var hint = hints.filter(function (h) { return h.kind === "m3u8"; })[0] ||
    hints.filter(function (h) { return h.kind === "mp4"; })[0] ||
    hints.filter(function (h) { return h.kind === "m4s"; })[0];
  var root = ctx.downloadDir();
  var tempDir = root + "/.harness-plugin-m4s-" + Date.now();
  ctx.fs.mkdir(tempDir);
  if (hint.kind === "m3u8") {
    // exec:ffmpeg 走全局共享池 .shared/com.harness.shared-ffmpeg/ffmpeg.exe
    ctx.exec({ program: "ffmpeg", args: ["-y", "-i", hint.url, "-c", "copy", root + "/out.mp4"], cwd: root });
  }
  ctx.fs.remove(tempDir);
}
```

完整示例：`data/plugins/m4s-merger/manifest.json` 与 `main.js`（`onHolo` 分流、networkHints 消费、`ctx.startTask` 建卡、`ctx.addTasksAndWait` 分片、`ctx.exec` 调 ffmpeg 合并、`ctx.emit("plugin-task")` 推进进度）。
