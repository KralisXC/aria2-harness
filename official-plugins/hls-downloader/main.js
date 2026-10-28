// HLS Downloader（beta10 示例插件 · 自包含分发）
//
// 依赖：本插件目录下的 bin/ffmpeg.exe（由分发方随 zip 放入），宿主不负责下载 / 探测。
// 流程：master playlist → 最高带宽 variant → media playlist → 分片批量下载
//       → ffmpeg concat 合并 → 清理临时目录。
//
// 上下文约定（宿主注入）：
//   ctx.httpGet({url, headers})              → {ok, status, headers, body, error}
//   ctx.addTasksAndWait(tasks, opts)          → {ok, tasks:[{status,path,errorMessage}], aborted}
//   ctx.exec({program, args, cwd, timeoutMs}) → {ok, code, stdout, stderr, timedOut, error}
//   ctx.fs.{mkdir,writeText,exists,remove}    → {ok, error}
//   ctx.emit("plugin-task", {...})            → 虚拟任务状态（宿主转发到前端）

var harness = globalThis.harness;
var ctxGlobal = globalThis.ctx;

var SEGMENTS_PER_CALL = 100;
var TASK_TIMEOUT_MS = 600000;
var MERGE_TIMEOUT_MS = 600000;

/** 正在运行的任务：taskId -> { tempDir }（供 onTaskCancel 清理）。 */
var active = {};

// ---------------------------------------------------------------------------
// 事件路由
// ---------------------------------------------------------------------------

harness.on("onTaskDetected", function (ctx, event) {
  var url = event && event.url ? String(event.url) : "";
  if (!url || !/\.m3u8(\?|#|$)/i.test(url)) {
    return { claim: false };
  }
  return { claim: true, title: fileName(url) };
});

harness.on("onTaskRun", function (ctx, event) {
  var task = (event && event.task) || {};
  var id = String(task.id || "");
  var url = String(task.url || "");
  var outDir = String(task.outputDir || task.dir || "");
  if (!id || !url) {
    return;
  }

  var state = { tempDir: "" };
  active[id] = state;
  var headers = headersFrom(task.options);

  try {
    emit(ctx, id, { phase: "detecting", progress: 2, detail: "解析播放列表" });

    var fetched = fetchText(ctx, url, headers);
    if (!fetched.ok) {
      throw new Error(fetched.error);
    }
    if (!/^\s*#EXTM3U/.test(fetched.body)) {
      throw new Error("不是有效的 m3u8 播放列表");
    }

    var playlistUrl = url;
    var playlistText = fetched.body;

    var variant = pickVariant(playlistText);
    if (variant) {
      var variantUrl = resolveUrl(playlistUrl, variant.url);
      emit(ctx, id, {
        phase: "detecting",
        progress: 5,
        detail: "已选择清晰度 " + (variant.label || "auto")
      });
      var media = fetchText(ctx, variantUrl, headers);
      if (!media.ok) {
        throw new Error(media.error);
      }
      playlistUrl = variantUrl;
      playlistText = media.body;
    }

    var segments = parseSegments(playlistText, playlistUrl);
    if (segments.length === 0) {
      throw new Error("播放列表中没有可下载的分片");
    }

    var output = outputPath(ctx, task);
    emit(ctx, id, {
      phase: "downloading",
      progress: 10,
      detail: "共 " + segments.length + " 个分片"
    });

    if (needsFfmpegDirect(playlistText)) {
      // AES-128 / fMP4 init / BYTERANGE：交给 ffmpeg 直连播放列表处理。
      emit(ctx, id, { phase: "merging", progress: 80, detail: "正在合并 HLS 分片..." });
      var direct = ctx.exec({
        program: "ffmpeg",
        args: ffmpegDirectArgs(playlistUrl, headers, output),
        timeoutMs: MERGE_TIMEOUT_MS
      });
      if (!direct.ok) {
        throw new Error(ffmpegError(direct));
      }
      emit(ctx, id, { phase: "merging", progress: 95, detail: "合并完成！" });
    } else {
      var tempDir = outDir + "/.harness-plugin/" + safeName(id);
      state.tempDir = tempDir;
      var made = ctx.fs.mkdir(tempDir);
      if (!made || made.ok === false) {
        throw new Error("创建临时目录失败：" + ((made && made.error) || "未知原因"));
      }
      // beta12.3：把临时目录上报给宿主，删除任务时可级联清理。
      emit(ctx, id, { tempDir: tempDir });

      var files = downloadSegments(ctx, id, segments, tempDir, headers);
      if (state.cancelled) {
        throw { cancelled: true, message: "已取消" };
      }

      var listPath = tempDir + "/list.txt";
      var listText = "";
      for (var i = 0; i < files.length; i++) {
        listText += "file '" + toFfmpegPath(files[i]) + "'\n";
      }
      var written = ctx.fs.writeText(listPath, listText);
      if (!written || written.ok === false) {
        throw new Error("写入分片清单失败：" + ((written && written.error) || "未知原因"));
      }

      emit(ctx, id, { phase: "merging", progress: 85, detail: "正在合并 HLS 分片..." });
      var merged = ctx.exec({
        program: "ffmpeg",
        args: [
          "-y",
          "-f", "concat",
          "-safe", "0",
          "-i", listPath,
          "-c", "copy",
          "-bsf:a", "aac_adtstoasc",
          output
        ],
        cwd: tempDir,
        timeoutMs: MERGE_TIMEOUT_MS
      });
      if (!merged.ok) {
        throw new Error(ffmpegError(merged));
      }
      emit(ctx, id, { phase: "merging", progress: 95, detail: "合并完成！" });
    }

    emit(ctx, id, {
      phase: "complete",
      progress: 100,
      detail: "已保存：" + fileName(output),
      outputPath: output
    });
  } catch (error) {
    if (error && error.cancelled) {
      emit(ctx, id, { phase: "cancelled", progress: 0, detail: "已取消" });
    } else {
      var message = error && error.message ? String(error.message) : String(error);
      emit(ctx, id, { phase: "error", progress: 0, detail: "下载失败", error: message });
    }
  } finally {
    cleanup(ctx, state);
    delete active[id];
  }
});

harness.on("onTaskCancel", function (ctx, event) {
  var task = (event && event.task) || {};
  var id = String(task.id || "");
  var state = active[id];
  if (state) {
    cleanup(ctx, state);
    delete active[id];
  }
  emit(ctx, id, { phase: "cancelled", progress: 0, detail: "已取消" });
});

// ---------------------------------------------------------------------------
// 分片下载与合并
// ---------------------------------------------------------------------------

function downloadSegments(ctx, id, segments, tempDir, headers) {
  var files = [];
  var done = 0;

  for (var start = 0; start < segments.length; start += SEGMENTS_PER_CALL) {
    var batch = segments.slice(start, start + SEGMENTS_PER_CALL);
    var tasks = [];
    for (var i = 0; i < batch.length; i++) {
      tasks.push({
        url: batch[i],
        out: segmentName(start + i),
        dir: tempDir,
        headers: headers,
        referer: headers.Referer || ""
      });
    }

    var result = ctx.addTasksAndWait(tasks, {
      timeoutMs: TASK_TIMEOUT_MS,
      concurrency: 8
    });
    if (!result || result.ok === false) {
      // 取消：宿主把未完成任务标成 cancelled，优先按取消处理。
      if (result && hasStatus(result, "cancelled")) {
        throw { cancelled: true, message: "已取消" };
      }
      throw new Error(failureMessage(result));
    }

    for (var j = 0; j < result.tasks.length; j++) {
      var item = result.tasks[j];
      files.push(item.path && item.path !== "" ? item.path : tempDir + "/" + segmentName(start + j));
    }
    done += batch.length;

    var progress = 10 + Math.round((done / segments.length) * 70);
    emit(ctx, id, {
      phase: "downloading",
      progress: progress,
      detail: done + "/" + segments.length + " 分片"
    });
  }

  return files;
}

function hasStatus(result, status) {
  if (!result || !result.tasks) {
    return false;
  }
  for (var i = 0; i < result.tasks.length; i++) {
    if (result.tasks[i].status === status) {
      return true;
    }
  }
  return false;
}

function failureMessage(result) {
  if (result && result.error) {
    return String(result.error);
  }
  if (result && result.tasks) {
    for (var i = 0; i < result.tasks.length; i++) {
      var task = result.tasks[i];
      if (task.status !== "complete") {
        var reason = task.errorMessage || task.errorCode || task.status;
        return "第 " + (i + 1) + " 个分片失败：" + reason;
      }
    }
  }
  return "分片下载失败";
}

function segmentName(index) {
  var text = String(index + 1);
  while (text.length < 6) {
    text = "0" + text;
  }
  return "seg_" + text + ".ts";
}

// ---------------------------------------------------------------------------
// 播放列表解析
// ---------------------------------------------------------------------------

function fetchText(ctx, url, headers) {
  var res = ctx.httpGet({ url: url, headers: headers, timeoutMs: 15000 });
  if (!res || res.ok === false) {
    return { ok: false, error: (res && res.error) || "网络请求失败" };
  }
  if (res.status && res.status >= 400) {
    return { ok: false, error: "HTTP " + res.status };
  }
  return { ok: true, body: String(res.body || ""), headers: res.headers || {} };
}

/** 从 master playlist 选带宽最高的 variant；media playlist 返回 null。 */
function pickVariant(text) {
  var lines = text.split(/\r?\n/);
  var best = null;
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i].trim();
    if (line.indexOf("#EXT-X-STREAM-INF:") !== 0) {
      continue;
    }
    var uri = "";
    for (var j = i + 1; j < lines.length; j++) {
      var next = lines[j].trim();
      if (next === "") {
        continue;
      }
      if (next.charAt(0) !== "#") {
        uri = next;
      }
      break;
    }
    if (!uri) {
      continue;
    }
    var bandwidth = parseInt(attr(line, "BANDWIDTH"), 10);
    if (isNaN(bandwidth)) {
      bandwidth = 0;
    }
    if (!best || bandwidth > best.bandwidth) {
      best = {
        url: uri,
        bandwidth: bandwidth,
        label: attr(line, "RESOLUTION") || (bandwidth ? bandwidth + "bps" : "")
      };
    }
  }
  return best;
}

/** 收集 media playlist 中的分片 URL（相对地址按播放列表地址解析）。 */
function parseSegments(text, baseUrl) {
  var lines = text.split(/\r?\n/);
  var out = [];
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i].trim();
    if (!line || line.charAt(0) === "#") {
      continue;
    }
    out.push(resolveUrl(baseUrl, line));
  }
  return out;
}

/** 这些特性无法用「先下载分片再 concat」处理，改走 ffmpeg 直连。 */
function needsFfmpegDirect(text) {
  return /#EXT-X-KEY:[^\n]*METHOD=(?!NONE)/.test(text)
    || text.indexOf("#EXT-X-MAP:") !== -1
    || text.indexOf("#EXT-X-BYTERANGE:") !== -1;
}

/** 读取 `#EXT-X-STREAM-INF:NAME="value"` 形式的属性。 */
function attr(line, name) {
  var match = new RegExp(name + '=("[^"]*"|[^,]*)').exec(line);
  if (!match) {
    return "";
  }
  return match[1].replace(/^"|"$/g, "");
}

// ---------------------------------------------------------------------------
// URL / 路径工具
// ---------------------------------------------------------------------------

/** 相对 URL 解析（http(s)、协议相对、根相对、路径相对 + `./` `../` 归一）。 */
function resolveUrl(base, relative) {
  var rel = String(relative).trim();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(rel)) {
    return rel;
  }
  var origin = "";
  var match = /^([a-z][a-z0-9+.-]*:\/\/[^\/?#]+)(.*)$/i.exec(base);
  if (match) {
    origin = match[1];
  }
  if (rel.indexOf("//") === 0) {
    var protocol = /^([a-z][a-z0-9+.-]*:)/i.exec(base);
    return (protocol ? protocol[1] : "https:") + rel;
  }
  if (rel.charAt(0) === "/") {
    return origin + rel;
  }

  var path = base;
  var cut = path.search(/[?#]/);
  if (cut !== -1) {
    path = path.substring(0, cut);
  }
  path = path.substring(0, path.lastIndexOf("/") + 1) + rel;

  var prefix = "";
  var scheme = /^([a-z][a-z0-9+.-]*:\/\/[^\/?#]+)/i.exec(path);
  if (scheme) {
    prefix = scheme[1];
    path = path.substring(prefix.length);
  }
  var parts = path.split("/");
  var stack = [];
  for (var i = 0; i < parts.length; i++) {
    var part = parts[i];
    if (part === "" || part === ".") {
      continue;
    }
    if (part === "..") {
      if (stack.length > 0) {
        stack.pop();
      }
      continue;
    }
    stack.push(part);
  }
  return prefix + "/" + stack.join("/");
}

function fileName(url) {
  var text = String(url).split("#")[0].split("?")[0];
  text = text.replace(/\/+$/, "");
  var index = text.lastIndexOf("/");
  return index >= 0 ? text.substring(index + 1) : text;
}

function baseName(url) {
  var name = fileName(url);
  var dot = name.lastIndexOf(".");
  return dot > 0 ? name.substring(0, dot) : name;
}

/** Windows 文件名安全化（非法字符、结尾点空格、保留设备名）。 */
function safeName(input) {
  var text = String(input).replace(/[\x00-\x1f<>:"\/\\|?*]/g, "_");
  text = text.replace(/^[\s.]+|[\s.]+$/g, "");
  if (text.length > 120) {
    text = text.substring(0, 120);
  }
  if (!text) {
    text = "video";
  }
  if (/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/i.test(text)) {
    text = "video_" + text;
  }
  return text;
}

/** 输出文件：下载目录/<标题>.mp4；已存在时追加时间戳，绝不静默覆盖。 */
function outputPath(ctx, task) {
  var outDir = String(task.outputDir || task.dir || "");
  var base = safeName(baseName(task.title || task.url || "video"));
  var path = outDir + "/" + base + ".mp4";
  var exists = ctx.fs.exists(path);
  if (exists && exists.ok !== false && exists.exists) {
    path = outDir + "/" + base + "-" + Date.now() + ".mp4";
  }
  return path;
}

/** ffmpeg concat 清单里的路径统一成正斜杠（Windows 下亦可识别）。 */
function toFfmpegPath(path) {
  return String(path).replace(/\\/g, "/");
}

// ---------------------------------------------------------------------------
// 直连合并（AES-128 / fMP4）
// ---------------------------------------------------------------------------

function ffmpegDirectArgs(url, headers, output) {
  var args = ["-y", "-headers", headerLines(headers), "-i", url, "-c", "copy"];
  args.push(output);
  return args;
}

/** ffmpeg `-headers` 参数：每行 `Key: Value\r\n`。 */
function headerLines(headers) {
  var names = Object.keys(headers);
  var text = "";
  for (var i = 0; i < names.length; i++) {
    text += names[i] + ": " + headers[names[i]] + "\r\n";
  }
  return text;
}

/** 从 FFmpeg 日志提取友好进度：`time=00:00:32.15` / `speed= 2.2x`。 */
function ffmpegProgress(stderr) {
  var text = String(stderr || "");
  var time = "";
  var speed = "";
  var m;
  var reTime = /time=(\d{2}:\d{2}:\d{2}\.\d{2})/g;
  var reSpeed = /speed=\s*([\d.]+)x/g;
  while ((m = reTime.exec(text))) {
    time = m[1];
  }
  while ((m = reSpeed.exec(text))) {
    speed = m[1];
  }
  if (!time && !speed) {
    return "";
  }
  var out = "已处理时长: " + (time || "未知");
  if (speed) {
    out += " (速度: " + speed + "x)";
  }
  return out;
}

function ffmpegError(result) {
  if (!result) {
    return "FFmpeg 未执行";
  }
  if (result.error) {
    return String(result.error);
  }
  var stderr = String(result.stderr || "");
  var progress = ffmpegProgress(stderr);
  var prefix = "合并失败";
  if (progress) {
    prefix += "（" + progress + "）";
  }
  if (result.timedOut) {
    return prefix + "：FFmpeg 执行超时";
  }
  var lines = stderr.split(/\r?\n/);
  for (var i = lines.length - 1; i >= 0; i--) {
    var line = lines[i].trim();
    if (line && line.indexOf("Error") >= 0) {
      if (line.length > 200) {
        line = line.substring(0, 200);
      }
      return prefix + "：" + line;
    }
  }
  return prefix + "：FFmpeg 退出码 " + (result.code === undefined ? "未知" : result.code);
}

// ---------------------------------------------------------------------------
// 任务状态 / 清理
// ---------------------------------------------------------------------------

function emit(ctx, id, patch) {
  if (!id) {
    return;
  }
  var payload = { id: id };
  var keys = Object.keys(patch);
  for (var i = 0; i < keys.length; i++) {
    payload[keys[i]] = patch[keys[i]];
  }
  ctx.emit("plugin-task", payload);
}

function cleanup(ctx, state) {
  if (state && state.tempDir) {
    ctx.fs.remove(state.tempDir);
    state.tempDir = "";
  }
}

/** 从 aria2 选项提取可复用的请求头（Referer / Cookie / User-Agent / header[]）。 */
function headersFrom(options) {
  var headers = {};
  if (!options) {
    return headers;
  }
  if (options.referer) {
    headers.Referer = String(options.referer);
  }
  var extra = options.header;
  if (extra) {
    var list = typeof extra === "string" ? [extra] : extra;
    for (var i = 0; i < list.length; i++) {
      var line = String(list[i]);
      var index = line.indexOf(":");
      if (index > 0) {
        headers[line.substring(0, index).trim()] = line.substring(index + 1).trim();
      }
    }
  }
  if (options["user-agent"]) {
    headers["User-Agent"] = String(options["user-agent"]);
  }
  return headers;
}
