// 直播录制（v1.1.3 · rtmp / rtsp / HTTP-FLV / 标准 HLS 直播 / 自定义特征）
//
// 触发链路（v1.1.1 起直播独占）：
//   1. 扩展在直播间点击悬浮窗时**不再发 /probe**，而是把真实直播流地址投给 /add，
//      由本插件经 TaskRouter 认领（`*live.bilibili.com*` / `*bilivideo.cn*` / `*.flv`）；
//   2. 用户直接粘贴 rtmp/rtsp/flv 地址时同样由本插件接管。
//      —— 这样直播 m3u8 永远不会落到普通的 HLS 点播解析链路里无限下载。
//
// v1.1.2（B站直播攻坚）：
//   - 线索里出现伪造标记 `bilibili_live_blob`（MSE）时**不做任何尝试**，直接报错，
//     避免对着 blob: 假地址空转 FFmpeg；
//   - B站直播的 `-headers` 强制带 Referer / Cookie / User-Agent 三件套，
//     防 CDN 防盗链导致的 403 / 立刻断流。
//
// v1.1.3（通用化收尾）：
//   - 默认安全项 `-y` 永远置于参数数组最前，用户配置只做追加，清空也不会失败；
//   - HTTP / HTTPS 源统一带 `-reconnect 1 -reconnect_streamed 1 -reconnect_delay_max 5`，
//     应对 301/302 跳转与短暂断流。
//
// 契约要点：
//   - 任务卡 `mode: "indeterminate"`（直播没有确定进度，前端显示脉冲条）；
//   - FFmpeg 阻塞录制，用户点击「停止录制」时宿主强杀该插件的子进程，
//     exec 返回失败后本轮 emit 会被终态保护拦下，onTaskCancel 再补一次 cancelled；
//   - 已录制内容留在输出文件里，不做删除；
//   - 同一个直播流（同主机 + 路径）只允许一条录制任务，重复添加直接拒绝。
//
// 用户配置（manifest.settings_schema → configs/<id>.json）：
//   custom_live_patterns  每行一个特征，URL / 页面地址包含即接管
//   custom_ffmpeg_args    额外 FFmpeg 参数，默认 "-c copy"（按数组传入，无注入风险）
//
// 宿主注入的上下文：
//   ctx.emit("plugin-task", {...})              → 推进虚拟任务卡
//   ctx.exec({program,args,cwd,timeoutMs})      → {ok,code,stdout,stderr,timedOut,error}
//   ctx.fs.exists(path)                         → {ok,exists}
//   ctx.settings.get(key)                       → 配置值
//   ctx.notify(title, body)                     → 桌面通知

var harness = globalThis.harness;

/** 扩展伪造的 MSE 标记：真实流被 `blob:`（MSE）隐藏，拿不到可录制地址。 */
var LIVE_BLOB_MARKER = "bilibili_live_blob";

/** 情况 B 的统一话术（B站直播被 MSE 拦截）。 */
var MSE_HIDDEN =
  "B站直播流被浏览器隐藏（MSE格式），无法获取真实地址。请使用专门的B站直播解析插件，或稍后尝试。";

/** 单个虚拟任务 -> { output, key }（供 onTaskCancel 落盘确认 / 去重）。 */
var active = {};
/** 直播流标识（host + path）-> 正在录制的任务 id。 */
var running = {};

// ---------------------------------------------------------------------------
// 认领
// ---------------------------------------------------------------------------

harness.on("onTaskDetected", function (ctx, event) {
  var url = String((event && event.url) || "").trim();
  if (!url) {
    return { claim: false };
  }
  var pageUrl = pageUrlOf(event);
  if (!isLiveStream(ctx, url, pageUrl, event)) {
    return { claim: false };
  }
  // 直播流一律独占：宁可本插件报「已在录制中」，也绝不放给普通下载链路。
  return { claim: true, title: liveTitle(url, pageUrl) };
});

/**
 * 直播判定：基础特征（rtmp/rtsp/.flv / B站直播页 / B站直播 CDN）
 * + 宿主可能附带的网络线索 + 用户自定义特征。
 */
function isLiveStream(ctx, url, pageUrl, event) {
  var lower = String(url).toLowerCase();
  if (/^rtmp:\/\//.test(lower) || /^rtsp:\/\//.test(lower)) {
    return true;
  }
  if (/\.flv(?:[?#]|$)/.test(lower)) {
    return true;
  }
  if (isBilibiliLiveHost(hostOf(url)) || isBilibiliLiveHost(hostOf(pageUrl))) {
    return true;
  }
  if (isBilibiliLiveCdn(hostOf(url)) || isBilibiliLiveCdn(hostOf(pageUrl))) {
    return true;
  }
  if (hintsLookLive(event)) {
    return true;
  }
  if (matchesCustom(ctx, url) || (pageUrl && matchesCustom(ctx, pageUrl))) {
    return true;
  }
  return false;
}

/** B站直播间页面主机（live.bilibili.com）。 */
function isBilibiliLiveHost(host) {
  return /(^|\.)live\.bilibili\.com$/.test(String(host || "").toLowerCase());
}

/** B站直播 CDN（HLS 走 mcdn.bilivideo.cn，FLV 走 *.bilivideo.com）。 */
function isBilibiliLiveCdn(host) {
  var lower = String(host || "").toLowerCase();
  return /(^|\.)bilivideo\.cn$/.test(lower) || /(^|\.)bilivideo\.com$/.test(lower);
}

/**
 * 宿主如果随事件附带网络线索（`networkHints` / `packet.networkHints`），
 * 只要出现 flv 或直播 CDN 地址就按直播处理。当前宿主的认领载荷不带线索，
 * 这里保留兼容读取，扩展侧已改为直接投递真实流地址。
 */
function hintsLookLive(event) {
  var raw = (event && event.networkHints) ||
    (event && event.packet && event.packet.networkHints) ||
    [];
  if (!raw || typeof raw.length !== "number") {
    return false;
  }
  for (var i = 0; i < raw.length; i++) {
    var hint = raw[i] || {};
    var kind = String(hint.kind || "").toLowerCase();
    var hintUrl = String(hint.url || "");
    // v1.1.2：伪造标记同样算直播特征（后续由 onTaskRun 给出 MSE 结论）。
    if (hintUrl === LIVE_BLOB_MARKER) {
      return true;
    }
    if (kind === "flv") {
      return true;
    }
    if (isBilibiliLiveCdn(hostOf(hintUrl))) {
      return true;
    }
  }
  return false;
}

/** 自定义特征：每行一个，`#` 开头视为注释；命中任意一行即接管。 */
function matchesCustom(ctx, url) {
  var text = readSetting(ctx, "custom_live_patterns");
  if (!text) {
    return false;
  }
  var lower = String(url).toLowerCase();
  var lines = text.split(/\r?\n/);
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i].trim();
    if (!line || line.charAt(0) === "#") {
      continue;
    }
    if (lower.indexOf(line.toLowerCase()) >= 0) {
      return true;
    }
  }
  return false;
}

/** 标题：B站直播固定为「直播录制 - B站」，其余用主机名。 */
function liveTitle(url, pageUrl) {
  if (isBilibiliLiveHost(hostOf(pageUrl)) || isBilibiliLiveHost(hostOf(url))) {
    return "直播录制 - B站";
  }
  return "直播录制 - " + hostOf(url);
}

// ---------------------------------------------------------------------------
// 录制
// ---------------------------------------------------------------------------

harness.on("onTaskRun", function (ctx, event) {
  var task = (event && event.task) || {};
  var id = String(task.id || "");
  var url = String(task.url || "");
  // 页面原地址：认领时的 options.referer（扩展直提时会带直播间 URL）。
  var pageUrl = String(optionsOf(task).referer || "");
  if (!id || !url) {
    return;
  }

  // 防重复：同一路直播只允许一条录制任务。
  var key = streamKey(url);
  if (running[key]) {
    var busy = "该直播已在录制中";
    emit(ctx, id, { phase: "error", progress: 0, detail: busy, error: busy });
    ctx.notify("直播录制", busy + "，请勿重复添加");
    return;
  }

  // 情况 B：扩展只递来 MSE 标记（真实地址被 blob: 隐藏）——不做任何尝试。
  if (url.indexOf(LIVE_BLOB_MARKER) >= 0) {
    emit(ctx, id, {
      phase: "error",
      progress: 0,
      mode: "indeterminate",
      detail: MSE_HIDDEN,
      error: MSE_HIDDEN,
    });
    ctx.notify("直播录制", MSE_HIDDEN);
    return;
  }

  // 页面地址（HTML）无法直接交给 FFmpeg：给出可执行的指引，而不是静默失败。
  if (isPageUrl(url)) {
    var biliPage = isBilibiliLiveHost(hostOf(url)) || isBilibiliLiveHost(hostOf(pageUrl));
    var tip = biliPage
      ? MSE_HIDDEN
      : "无法直接录制直播间页面地址，请在直播间点击页面的悬浮按钮";
    emit(ctx, id, { phase: "error", progress: 0, mode: "indeterminate", detail: tip, error: tip });
    ctx.notify("直播录制", tip);
    return;
  }

  var scheme = schemeOf(url);
  var outDir = String(task.outputDir || task.dir || "");
  var extension = extensionFor(url);
  var output = joinPath(outDir, "live-" + safeName(hostOf(url)) + "-" + Date.now() + "." + extension);
  if (fileExists(ctx, output)) {
    output = joinPath(outDir, "live-" + safeName(hostOf(url)) + "-" + Date.now() + "-" + id.slice(-6) + "." + extension);
  }

  running[key] = id;
  active[id] = { output: output, key: key };

  emit(ctx, id, {
    phase: "downloading",
    mode: "indeterminate",
    progress: 0,
    detail: "录制中 · 点击「停止录制」保存",
    outputPath: output,
  });
  ctx.notify("开始录制", hostOf(url) + " → " + fileName(output));

  // 安全的默认项**永远**排在最前：`-y` 保证输出文件已存在时也能覆盖写入。
  // 用户填写的参数只作为"追加项"拼接在后面，清空配置也不会让录制直接失败。
  var args = ["-y"];
  if (scheme === "rtsp") {
    // RTSP 默认 UDP 在公网丢包严重，强制 TCP 更稳。
    args.push("-rtsp_transport", "tcp");
  }
  if (scheme === "http" || scheme === "https") {
    // 通用直播源经常 301/302 跳转或短暂断流：让 FFmpeg 自己续上，而不是整场录制中断。
    args.push("-reconnect", "1", "-reconnect_streamed", "1", "-reconnect_delay_max", "5");
  }
  // 复用浏览器上下文（Referer / Cookie / UA / 自定义头），绕过直播 CDN 的防盗链。
  var headers = ffmpegHeaders(optionsOf(task), url);
  if (headers) {
    args.push("-headers", headers);
  }
  args.push("-i", url);
  // 用户自定义的额外参数（默认 -c copy）；按数组传入，天然防命令注入。
  args = args.concat(parseArgs(readSetting(ctx, "custom_ffmpeg_args") || "-c copy"));
  args.push("-copyts", output);

  var result;
  try {
    result = ctx.exec({
      program: "ffmpeg",
      args: args,
      cwd: outDir || undefined,
      timeoutMs: 1_800_000,
    });
  } catch (error) {
    result = { ok: false, error: String((error && error.message) || error) };
  }

  if (!result || result.ok === false) {
    // 用户取消：宿主已把任务标成 cancelled，本次 error 会被终态保护忽略。
    emit(ctx, id, {
      phase: "error",
      detail: "录制已停止",
      error: result && result.timedOut ? "达到 30 分钟录制上限" : "录制中断（可能已手动停止）",
    });
    ctx.notify("录制结束", "已保存到 " + fileName(output));
  } else {
    emit(ctx, id, {
      phase: "complete",
      progress: 100,
      detail: "已保存：" + fileName(output),
      outputPath: output,
    });
    ctx.notify("录制完成", fileName(output));
  }

  forget(id, key);
});

harness.on("onTaskCancel", function (ctx, event) {
  var task = (event && event.task) || {};
  var id = String(task.id || "");
  var state = active[id];
  var output = state && state.output ? state.output : "";
  var key = state && state.key ? state.key : "";
  forget(id, key);
  // 宿主已强杀 FFmpeg；这里只确认已录制内容确实落盘，不做删除。
  var saved = output && fileExists(ctx, output);
  emit(ctx, id, {
    phase: "cancelled",
    progress: 0,
    detail: saved ? "已停止录制，文件已保存" : "已停止录制",
    outputPath: saved ? output : "",
  });
  if (saved) {
    ctx.notify("录制已停止", "已保存：" + fileName(output));
  }
});

/** 任务终结：清理运行中的去重标记。 */
function forget(id, key) {
  if (key && running[key] === id) {
    delete running[key];
  }
  delete active[id];
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/** 读取配置项，缺失 / 非字符串一律回落空串。 */
function readSetting(ctx, key) {
  try {
    var value = ctx.settings && ctx.settings.get(key);
    return value == null ? "" : String(value);
  } catch (e) {
    return "";
  }
}

/**
 * 把用户填写的 FFmpeg 参数拆成 argv（支持单 / 双引号包裹的片段）。
 * 参数始终以数组形式交给 ctx.exec，不经过 shell，天然防注入。
 */
function parseArgs(text) {
  var raw = String(text || "");
  var out = [];
  var cur = "";
  var quote = "";
  for (var i = 0; i < raw.length; i++) {
    var ch = raw.charAt(i);
    if (quote) {
      if (ch === quote) {
        quote = "";
      } else {
        cur += ch;
      }
    } else if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (/\s/.test(ch)) {
      if (cur) {
        out.push(cur);
        cur = "";
      }
    } else {
      cur += ch;
    }
  }
  if (cur) {
    out.push(cur);
  }
  return out.length ? out : ["-c", "copy"];
}

/** 认领载荷里的 `options`（浏览器上下文：referer / user-agent / header）。 */
function optionsOf(task) {
  return (task && task.options && typeof task.options === "object") ? task.options : {};
}

/** 从认领载荷里还原「页面原始 URL」：优先 options.referer，其次事件附带的 packet.url。 */
function pageUrlOf(event) {
  var options = (event && event.options) || {};
  var referer = String(options.referer || options.Referer || "");
  if (/^https?:\/\//i.test(referer)) {
    return referer;
  }
  var packet = event && event.packet;
  var packetUrl = String((packet && packet.url) || "");
  if (/^https?:\/\//i.test(packetUrl)) {
    return packetUrl;
  }
  return "";
}

/**
 * 组装 FFmpeg `-headers`（每行以 \r\n 结尾，换行已清洗，防头部注入）。
 *
 * v1.1.2：B站直播强制三件套 —— Referer（缺省 `https://live.bilibili.com/`）、
 * Cookie、User-Agent。`*.bilivideo.com` 的 FLV 常校验 Referer 与 Cookie，
 * 缺任一项都会直接 403 或几秒后断流。
 */
function ffmpegHeaders(options, streamUrl) {
  var lines = [];
  var referer = cleanHeaderValue(options.referer || options.Referer || "");
  if (!referer && isBilibiliLiveStream(streamUrl)) {
    referer = "https://live.bilibili.com/";
  }
  if (referer) {
    lines.push("Referer: " + referer);
  }
  var ua = cleanHeaderValue(options["user-agent"] || options.userAgent || "");
  if (ua) {
    lines.push("User-Agent: " + ua);
  }
  var cookie = cleanHeaderValue(options.cookie || options.Cookie || "");
  if (cookie) {
    lines.push(/^cookie:/i.test(cookie) ? cookie : "Cookie: " + cookie);
  }
  var extra = options.header;
  if (typeof extra === "string") {
    extra = [extra];
  }
  if (extra && typeof extra.length === "number") {
    for (var i = 0; i < extra.length; i++) {
      var line = cleanHeaderValue(extra[i]);
      if (!line) {
        continue;
      }
      var lower = line.toLowerCase();
      if (
        lower.indexOf("referer:") === 0 ||
        lower.indexOf("user-agent:") === 0 ||
        lower.indexOf("cookie:") === 0
      ) {
        continue;
      }
      lines.push(line);
    }
  }
  return lines.length ? lines.join("\r\n") + "\r\n" : "";
}

/** 是否为 B站直播流（直播 CDN / live-bvc 路径 / 直播间主机）。 */
function isBilibiliLiveStream(url) {
  return (
    isBilibiliLiveCdn(hostOf(url)) ||
    /live-bvc/i.test(String(url)) ||
    isBilibiliLiveHost(hostOf(url))
  );
}

function cleanHeaderValue(value) {
  return String(value == null ? "" : value).replace(/[\r\n]+/g, " ").trim();
}

/** 是否为页面地址（无媒体后缀的 http(s) URL）。 */
function isPageUrl(url) {
  var scheme = schemeOf(url);
  if (scheme !== "http" && scheme !== "https") {
    return false;
  }
  return !/\.(m3u8|flv|mp4|ts|m4s|mpd)(?:[?#]|$)/i.test(String(url));
}

/** 直播流标识：协议 + 主机 + 路径（忽略每次都会变的鉴权 query）。 */
function streamKey(url) {
  return schemeOf(url) + "://" + hostOf(url).toLowerCase() + pathOf(url).toLowerCase();
}

/** 输出容器：rtsp → mkv，HLS 直播 → ts，FLV / rtmp → flv，其它 → mkv。 */
function extensionFor(url) {
  var scheme = schemeOf(url);
  if (scheme === "rtsp") {
    return "mkv";
  }
  if (/\.m3u8(?:[?#]|$)/i.test(url)) {
    return "ts";
  }
  if (/\.flv(?:[?#]|$)/i.test(url) || scheme === "rtmp") {
    return "flv";
  }
  return "mkv";
}

function emit(ctx, taskId, payload) {
  if (!taskId) {
    return;
  }
  payload.id = taskId;
  ctx.emit("plugin-task", payload);
}

function schemeOf(url) {
  var match = /^([a-z][a-z0-9+.-]*):\/\//i.exec(String(url));
  return match ? match[1].toLowerCase() : "";
}

function hostOf(url) {
  var match = /^[a-z][a-z0-9+.-]*:\/\/([^\/?#]+)/i.exec(String(url));
  if (!match) {
    return "stream";
  }
  var host = match[1].split("@").pop();
  host = host.split(":")[0];
  return host || "stream";
}

/** URL path（不含 query / fragment）。 */
function pathOf(url) {
  var match = /^[a-z][a-z0-9+.-]*:\/\/[^\/?#]*(\/[^?#]*)?/i.exec(String(url));
  return (match && match[1]) || "/";
}

function safeName(input) {
  var text = String(input == null ? "" : input).replace(/[\x00-\x1f<>:"\/\\|?*]/g, "_");
  text = text.replace(/^[\s.]+|[\s.]+$/g, "");
  return text || "stream";
}

function joinPath(dir, name) {
  var base = String(dir || "");
  if (!base) {
    return name;
  }
  return base.replace(/[\\/]+$/, "") + "/" + name;
}

function fileName(path) {
  var text = String(path).replace(/\\/g, "/");
  var index = text.lastIndexOf("/");
  return index >= 0 ? text.substring(index + 1) : text;
}

function fileExists(ctx, path) {
  var res = ctx.fs.exists(path);
  return Boolean(res && res.ok !== false && res.exists);
}
