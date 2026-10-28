// M4S 音视频合并（beta12 · Step 2 / 1.0.0-rc1 · 模块三）
//
// 触发：扩展 /probe → 宿主 dispatch_holo → 本例 onHolo。
// 分流顺序（1.0.0-rc1）：
//   1. B站普通投稿页（/video/BV...）：原解析路径（playurl API → DASH 音视频 → 合并）。
//      **守护项**：B站的 networkHints 只有视频 m4s、缺音轨，走 hints 会产出无声视频。
//   2. 其它站点且扩展上报了 networkHints：按 m3u8 > mp4 > m4s 优先消费（runFromHints）。
//   3. 都没有：YouTube 明确降级提示，其它站点静默返回。
//
// 宿主注入的上下文：
//   ctx.startTask({title,url})                 → {ok,id}（onHolo 没有宿主任命的任务 id）
//   ctx.downloadDir()                          → 下载根目录（只读）
//   ctx.httpGet({url,headers,timeoutMs})        → {ok,status,body,error}
//   ctx.addTasksAndWait(tasks,opts)             → {ok,tasks:[{status,path,errorMessage}],aborted}
//   ctx.exec({program,args,cwd,timeoutMs})      → {ok,code,stdout,stderr,timedOut,error}
//   ctx.fs.{mkdir,exists,remove}                → {ok,error}
//   ctx.emit("plugin-task",{...})               → 推进虚拟任务卡
//   ctx.notify(title, body)                     → 桌面通知

var harness = globalThis.harness;

var TASK_TIMEOUT_MS = 600000;
var MERGE_TIMEOUT_MS = 600000;
/** 网络线索条数上限（与宿主 MAX_NET_HINTS 对齐，防异常包拖慢解析）。 */
var MAX_HINTS = 48;
var DEFAULT_UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) " +
  "Chrome/140.0.0.0 Safari/537.36";

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

harness.on("onHolo", function (ctx, event) {
  var packet = (event && event.packet) || {};
  var url = String(packet.url || "");
  var host = String(packet.host || "").toLowerCase();

  if (!url) {
    return;
  }

  // 1. B站投稿视频页：保持原生解析（hints 缺音轨，切换会退化）。
  if (isBilibiliVideo(host, url)) {
    runBilibili(ctx, packet);
    return;
  }

  // 2. 其它站点：优先消费扩展嗅探到的网络线索。
  var hints = normalizeHints(packet.networkHints);
  if (hints.length > 0) {
    runFromHints(ctx, packet, hints);
    return;
  }

  // 3. 无可用线索：YouTube / B站 给出明确的站点级提示；其它站点必须显式报错。
  if (/(^|\.)youtube\.com$/.test(host) || /(^|\.)youtu\.be$/.test(host)) {
    ctx.notify(
      "暂不支持 YouTube",
      "YouTube 需要签名解密与 PO Token，本插件未内置该能力；请在页面改用其他下载方式。",
    );
    return;
  }
  if (/(^|\.)bilibili\.com$/.test(host)) {
    ctx.notify("暂不支持该页面", "M4S 合并目前只支持普通投稿视频页（/video/BV...）。");
    return;
  }
  reportNoHints(ctx, packet);
});

/**
 * 通用站点无任何线索时的兜底（v1.1.1）：绝不允许静默返回。
 *
 * 在桌面端建一张「解析失败」任务卡，并把原因写进任务卡，用户能直接看到为什么没下。
 */
function reportNoHints(ctx, packet) {
  var reason = "未能在当前页面找到可下载的媒体流线索";
  var taskId = startTask(ctx, "解析失败", String(packet.url || ""));
  if (taskId) {
    emit(ctx, taskId, { phase: "error", progress: 0, detail: reason, error: reason });
  }
  ctx.notify("解析失败", reason);
}

/** B站普通投稿视频页（懒激活域名之外的精确判定）。 */
function isBilibiliVideo(host, url) {
  return /(^|\.)bilibili\.com$/.test(host) && /\/video\/BV[0-9A-Za-z]{10}/.test(String(url));
}

// ---------------------------------------------------------------------------
// 网络线索（1.0.0-rc1 · 模块三）
// ---------------------------------------------------------------------------

/** 只保留宿主归一过的可用类型（m3u8 / mp4 / m4s），并限制总条数。 */
function normalizeHints(raw) {
  if (!raw || typeof raw.length !== "number") {
    return [];
  }
  var out = [];
  for (var i = 0; i < raw.length && out.length < MAX_HINTS; i++) {
    var item = raw[i] || {};
    var url = String(item.url || "");
    var kind = String(item.kind || "").toLowerCase();
    if (!/^https?:\/\//i.test(url)) {
      continue;
    }
    if (kind !== "m3u8" && kind !== "mp4" && kind !== "m4s") {
      continue;
    }
    out.push({ url: url, kind: kind });
  }
  return out;
}

/** 类型优先级：m3u8 > mp4 > m4s。 */
function pickHint(hints) {
  var order = ["m3u8", "mp4", "m4s"];
  for (var k = 0; k < order.length; k++) {
    for (var i = 0; i < hints.length; i++) {
      if (hints[i].kind === order[k]) {
        return hints[i];
      }
    }
  }
  return null;
}

/** 全部 m4s 线索（≥2 条时前两条按 视频 / 音轨 合并）。 */
function m4sHints(hints) {
  var out = [];
  for (var i = 0; i < hints.length; i++) {
    if (hints[i].kind === "m4s") {
      out.push(hints[i]);
    }
  }
  return out;
}

/**
 * 消费网络线索：非 B站 页面由扩展嗅探到的媒体地址直接产出 MP4。
 *
 * 与 B站路径共用任务卡 / 下载 / FFmpeg / 收尾语义，只是不再请求站点 API。
 */
function runFromHints(ctx, packet, hints) {
  var hint = pickHint(hints);
  if (!hint) {
    return;
  }
  var url = String(packet.url || "");
  var title = cleanGenericTitle(packet.title, url);
  var taskId = startTask(ctx, title, url);

  var root = String(ctx.downloadDir() || "");
  if (!root) {
    fail(ctx, taskId, "无法获取下载目录", "");
    return;
  }

  var stamp = String(Date.now());
  var tempDir = root + "/.harness-plugin-m4s-" + stamp;
  var made = ctx.fs.mkdir(tempDir);
  if (!made || made.ok === false) {
    fail(ctx, taskId, "创建临时目录失败：" + ((made && made.error) || "未知原因"), tempDir);
    return;
  }

  var headers = requestHeadersFor(packet, url);
  var referer = headers.Referer;
  var output = uniqueOutput(ctx, root, title, "");

  try {
    if (hint.kind === "mp4") {
      // 单文件直链：无需合并，直接落下载目录。
      emit(ctx, taskId, {
        phase: "downloading",
        progress: 10,
        detail: "下载视频流",
        tempDir: tempDir,
      });
      var direct = ctx.addTasksAndWait(
        [{ url: hint.url, out: fileName(output), dir: root, headers: headers, referer: referer }],
        { timeoutMs: TASK_TIMEOUT_MS },
      );
      if (!direct || direct.ok === false) {
        if (direct && hasStatus(direct, "cancelled")) {
          throw { cancelled: true, message: "已取消" };
        }
        throw new Error(failureMessage(direct));
      }
      var saved = direct.tasks && direct.tasks[0] ? direct.tasks[0] : null;
      if (!saved || saved.status !== "complete") {
        throw new Error(
          "下载未完成：" + ((saved && (saved.errorMessage || saved.status)) || "未知原因"),
        );
      }
      output = saved.path ? String(saved.path) : output;
    } else if (hint.kind === "m3u8") {
      // HLS：由 FFmpeg 拉流并封装（带页面真实请求头，防盗链）。
      emit(ctx, taskId, {
        phase: "downloading",
        progress: 10,
        detail: "正在合并 HLS 分片...",
        tempDir: tempDir,
      });
      var merged = ctx.exec({
        program: "ffmpeg",
        args: [
          "-y",
          "-headers", ffmpegHeaders(packet, url),
          "-i", hint.url,
          "-c", "copy",
          "-movflags", "+faststart",
          output,
        ],
        cwd: root,
        timeoutMs: MERGE_TIMEOUT_MS,
      });
      if (!merged || merged.ok === false) {
        throw new Error(ffmpegError(merged));
      }
      emit(ctx, taskId, { phase: "merging", progress: 95, detail: "合并完成！" });
    } else {
      // m4s：≥2 条取前两条作 视频 / 音轨；仅 1 条按单流 remux。
      var streams = m4sHints(hints).slice(0, 2);
      var specs = [];
      for (var i = 0; i < streams.length; i++) {
        specs.push({ url: streams[i].url, out: "part" + i + ".m4s" });
      }
      emit(ctx, taskId, {
        phase: "downloading",
        progress: 15,
        detail: streams.length > 1 ? "下载视频流 + 音轨" : "下载媒体流",
      });
      var paths = downloadParts(ctx, specs, tempDir, headers, referer, taskId);

      var args = ["-y", "-i", paths[0]];
      if (paths.length > 1) {
        args.push("-i", paths[1], "-map", "0:v:0", "-map", "1:a:0");
      }
      args.push("-c", "copy", "-movflags", "+faststart", output);

      emit(ctx, taskId, { phase: "merging", progress: 85, detail: "正在合并音视频..." });
      var remixed = ctx.exec({ program: "ffmpeg", args: args, cwd: tempDir, timeoutMs: MERGE_TIMEOUT_MS });
      if (!remixed || remixed.ok === false) {
        throw new Error(ffmpegError(remixed));
      }
      emit(ctx, taskId, { phase: "merging", progress: 95, detail: "合并完成！" });
    }

    emit(ctx, taskId, {
      phase: "complete",
      progress: 100,
      detail: "已保存：" + fileName(output),
      outputPath: output,
    });
    ctx.notify("视频已下载", fileName(output));
  } catch (error) {
    var message = error && error.message ? String(error.message) : String(error);
    if (error && error.cancelled) {
      emit(ctx, taskId, { phase: "cancelled", detail: "已取消" });
      ctx.fs.remove(tempDir);
      return;
    }
    fail(ctx, taskId, message, tempDir);
    return;
  }

  ctx.fs.remove(tempDir);
}

// ---------------------------------------------------------------------------
// B站主流程
// ---------------------------------------------------------------------------

function runBilibili(ctx, packet) {
  var url = String(packet.url || "");
  var state = readState(packet);
  var bvid = pickBvid(state, url);
  if (!bvid) {
    ctx.notify("解析失败", "未能从页面中识别 BV 号，请刷新页面后重试。");
    return;
  }

  var title = cleanTitle(packet.title, bvid);
  var taskId = startTask(ctx, title, url);

  var root = String(ctx.downloadDir() || "");
  if (!root) {
    fail(ctx, taskId, "无法获取下载目录", "");
    return;
  }

  var stamp = String(Date.now());
  var tempDir = root + "/.harness-plugin-m4s-" + stamp;
  var made = ctx.fs.mkdir(tempDir);
  if (!made || made.ok === false) {
    fail(ctx, taskId, "创建临时目录失败：" + ((made && made.error) || "未知原因"), tempDir);
    return;
  }

  try {
    // beta12.3：首个事件就带上临时目录，宿主删除任务时可级联清理。
    emit(ctx, taskId, { phase: "detecting", progress: 3, detail: "解析播放地址", tempDir: tempDir });

    var cid = pickCid(state, url);
    if (!cid) {
      cid = fetchCid(ctx, bvid, packet);
    }
    if (!cid) {
      throw new Error("未能获取 cid（合集页请先刷新再重试）");
    }

    var play = fetchPlayurl(ctx, bvid, cid, packet);
    var output = outputPath(ctx, root, title, bvid);

    if (play.audioUrl) {
      emit(ctx, taskId, {
        phase: "downloading",
        progress: 15,
        detail: "下载视频流 + 音轨（" + play.qualityLabel + "）",
      });
      var spec = [
        { url: play.videoUrl, out: "video.m4s" },
        { url: play.audioUrl, out: "audio.m4s" },
      ];
      var paths = downloadParts(ctx, spec, tempDir, play.headers, play.referer, taskId);

      emit(ctx, taskId, { phase: "merging", progress: 85, detail: "正在合并音视频..." });
      var merged = ctx.exec({
        program: "ffmpeg",
        args: [
          "-y",
          "-i", paths[0],
          "-i", paths[1],
          "-map", "0:v:0",
          "-map", "1:a:0",
          "-c", "copy",
          "-movflags", "+faststart",
          output,
        ],
        cwd: tempDir,
        timeoutMs: MERGE_TIMEOUT_MS,
      });
      if (!merged || merged.ok === false) {
        throw new Error(ffmpegError(merged));
      }
      emit(ctx, taskId, { phase: "merging", progress: 95, detail: "合并完成！" });
    } else {
      emit(ctx, taskId, {
        phase: "downloading",
        progress: 15,
        detail: "下载单文件流（" + play.qualityLabel + "）",
      });
      var single = downloadParts(
        ctx,
        [{ url: play.videoUrl, out: "video.flv" }],
        tempDir,
        play.headers,
        play.referer,
        taskId,
      );

      emit(ctx, taskId, { phase: "merging", progress: 85, detail: "正在封装为 MP4..." });
      var remux = ctx.exec({
        program: "ffmpeg",
        args: ["-y", "-i", single[0], "-c", "copy", "-movflags", "+faststart", output],
        cwd: tempDir,
        timeoutMs: MERGE_TIMEOUT_MS,
      });
      if (!remux || remux.ok === false) {
        throw new Error(ffmpegError(remux));
      }
      emit(ctx, taskId, { phase: "merging", progress: 95, detail: "合并完成！" });
    }

    emit(ctx, taskId, {
      phase: "complete",
      progress: 100,
      detail: "已保存：" + fileName(output),
      outputPath: output,
    });
    ctx.notify("M4S 合并完成", fileName(output));
  } catch (error) {
    var message = error && error.message ? String(error.message) : String(error);
    // 用户取消：宿主已经把任务标成 cancelled；这里只收尾，不再重复报错。
    if (error && error.cancelled) {
      emit(ctx, taskId, { phase: "cancelled", detail: "已取消" });
      ctx.fs.remove(tempDir);
      return;
    }
    fail(ctx, taskId, message, tempDir);
    return;
  }

  // 成功路径：临时目录整体删除（含两个分片）。
  ctx.fs.remove(tempDir);
}

// ---------------------------------------------------------------------------
// playurl API
// ---------------------------------------------------------------------------

function fetchPlayurl(ctx, bvid, cid, packet) {
  var headers = requestHeaders(packet);
  var referer = headers.Referer || "https://www.bilibili.com/";
  var api =
    "https://api.bilibili.com/x/player/playurl?bvid=" +
    encodeURIComponent(bvid) +
    "&cid=" +
    encodeURIComponent(cid) +
    "&qn=116&fnval=4048&fourk=1";

  var res = ctx.httpGet({ url: api, headers: headers, timeoutMs: 20000 });
  if (!res || res.ok === false) {
    throw new Error("播放地址接口请求失败：" + ((res && res.error) || "网络错误"));
  }
  if (res.status && res.status >= 400) {
    throw new Error("播放地址接口 HTTP " + res.status);
  }

  var payload;
  try {
    payload = JSON.parse(String(res.body || ""));
  } catch (e) {
    throw new Error("播放地址接口返回非 JSON");
  }
  if (!payload || payload.code !== 0) {
    var code = payload && payload.code !== undefined ? payload.code : "?";
    var msg = payload && payload.message ? payload.message : "未知错误";
    throw new Error("B站接口返回 " + code + "：" + msg);
  }

  var data = payload.data || {};
  var dash = data.dash;
  if (dash && dash.video && dash.video.length > 0) {
    var video = pickVideo(dash.video);
    var audio = pickAudio(dash.audio || []);
    return {
      videoUrl: streamUrl(video),
      audioUrl: audio ? streamUrl(audio) : "",
      qualityLabel: qualityName(video),
      headers: headers,
      referer: referer,
    };
  }

  var durl = data.durl || [];
  if (durl.length > 0 && durl[0].url) {
    return {
      videoUrl: String(durl[0].url),
      audioUrl: "",
      qualityLabel: "单文件",
      headers: headers,
      referer: referer,
    };
  }

  throw new Error("该视频没有可下载的播放地址（可能需要大会员或已下架）");
}

/** 分页视频的 cid 兜底：/x/player/pagelist?bvid=。 */
function fetchCid(ctx, bvid, packet) {
  var headers = requestHeaders(packet);
  var res = ctx.httpGet({
    url: "https://api.bilibili.com/x/player/pagelist?bvid=" + encodeURIComponent(bvid),
    headers: headers,
    timeoutMs: 15000,
  });
  if (!res || res.ok === false) {
    return "";
  }
  var payload;
  try {
    payload = JSON.parse(String(res.body || ""));
  } catch (e) {
    return "";
  }
  var pages = (payload && payload.data) || [];
  return pages.length > 0 && pages[0].cid ? String(pages[0].cid) : "";
}

/** 请求头：Referer / UA / Cookie（Cookie 可能为空，不发送空值）。 */
function requestHeaders(packet) {
  var headers = {
    Referer: "https://www.bilibili.com/",
    "User-Agent": String(packet.userAgent || DEFAULT_UA),
  };
  var referer = String(packet.referer || "");
  if (/^https?:\/\//i.test(referer)) {
    headers.Referer = referer;
  }
  var cookie = String(packet.cookie || "");
  if (cookie) {
    headers.Cookie = cookie;
  }
  return headers;
}

/**
 * 通用请求头：Referer 优先用页面上报值，缺失时回退到页面 origin（防盗链站点必需）。
 */
function requestHeadersFor(packet, pageUrl) {
  var headers = {
    Referer: originOf(pageUrl),
    "User-Agent": String(packet.userAgent || DEFAULT_UA),
  };
  var referer = String(packet.referer || "");
  if (/^https?:\/\//i.test(referer)) {
    headers.Referer = referer;
  }
  var cookie = String(packet.cookie || "");
  if (cookie) {
    headers.Cookie = cookie;
  }
  return headers;
}

/** FFmpeg `-headers` 参数体：`Name: Value\r\n` 逐行拼接，缺值不写。 */
function ffmpegHeaders(packet, pageUrl) {
  var headers = requestHeadersFor(packet, pageUrl);
  var text = "";
  var names = ["Referer", "User-Agent", "Cookie"];
  for (var i = 0; i < names.length; i++) {
    var value = headers[names[i]];
    if (value) {
      text += names[i] + ": " + value + "\r\n";
    }
  }
  return text;
}

/** 页面 origin（协议 + 主机 + 端口），解析失败返回空串。 */
function originOf(pageUrl) {
  var match = /^(https?:\/\/[^\/?#]+)/i.exec(String(pageUrl || ""));
  return match ? match[1] + "/" : "";
}

/** 画质：优先高 id，同 id 优先 AVC（兼容性最好）。 */
function pickVideo(list) {
  var best = null;
  var bestScore = -1;
  for (var i = 0; i < list.length; i++) {
    var item = list[i] || {};
    var id = Number(item.id) || 0;
    var codecs = String(item.codecs || "");
    var score = id * 10 + (/^avc/i.test(codecs) ? 5 : 0);
    if (score > bestScore) {
      bestScore = score;
      best = item;
    }
  }
  return best;
}

function pickAudio(list) {
  var best = null;
  var bestBandwidth = -1;
  for (var i = 0; i < list.length; i++) {
    var item = list[i] || {};
    var bandwidth = Number(item.bandwidth) || 0;
    if (bandwidth > bestBandwidth) {
      bestBandwidth = bandwidth;
      best = item;
    }
  }
  return best;
}

function streamUrl(item) {
  if (!item) {
    return "";
  }
  if (item.baseUrl) {
    return String(item.baseUrl);
  }
  if (item.base_url) {
    return String(item.base_url);
  }
  var backup = item.backupUrl || item.backup_url || [];
  return backup.length > 0 ? String(backup[0]) : "";
}

function qualityName(video) {
  var id = video && video.id !== undefined ? Number(video.id) : 0;
  if (id >= 120) return "4K";
  if (id >= 116) return "1080P60";
  if (id >= 80) return "1080P";
  if (id >= 64) return "720P";
  if (id >= 32) return "480P";
  return id ? String(id) + "P" : "默认画质";
}

// ---------------------------------------------------------------------------
// 下载
// ---------------------------------------------------------------------------

function downloadParts(ctx, specs, tempDir, headers, referer, taskId) {
  var tasks = [];
  for (var i = 0; i < specs.length; i++) {
    tasks.push({
      url: specs[i].url,
      out: specs[i].out,
      dir: tempDir,
      headers: headers,
      referer: referer,
    });
  }

  var result = ctx.addTasksAndWait(tasks, { timeoutMs: TASK_TIMEOUT_MS, concurrency: 2 });
  if (!result || result.ok === false) {
    if (result && hasStatus(result, "cancelled")) {
      throw { cancelled: true, message: "已取消" };
    }
    throw new Error(failureMessage(result));
  }

  var paths = [];
  for (var j = 0; j < specs.length; j++) {
    var item = result.tasks && result.tasks[j] ? result.tasks[j] : null;
    if (!item || item.status !== "complete") {
      var reason = item && (item.errorMessage || item.status) ? item.errorMessage || item.status : "下载未完成";
      throw new Error("分片 " + (j + 1) + " 失败：" + reason);
    }
    paths.push(item.path ? String(item.path) : tempDir + "/" + specs[j].out);
  }

  emit(ctx, taskId, { phase: "downloading", progress: 70, detail: "分片下载完成" });
  return paths;
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
        return "分片 " + (i + 1) + " 失败：" + (task.errorMessage || task.status || "未知原因");
      }
    }
  }
  return "下载失败";
}

// ---------------------------------------------------------------------------
// 页面数据
// ---------------------------------------------------------------------------

function readState(packet) {
  var text = packet.initialState;
  if (!text) {
    return null;
  }
  try {
    return JSON.parse(String(text));
  } catch (e) {
    return null;
  }
}

function pickBvid(state, url) {
  var data = state && state.videoData;
  if (data && data.bvid) {
    return String(data.bvid);
  }
  if (state && state.bvid) {
    return String(state.bvid);
  }
  var match = /(BV[0-9A-Za-z]{10})/.exec(url);
  return match ? match[1] : "";
}

function pickCid(state, url) {
  var pages = state && state.videoData && state.videoData.pages;
  var target = pageIndex(url);
  if (pages && pages.length > 0) {
    for (var i = 0; i < pages.length; i++) {
      if (Number(pages[i].page) === target && pages[i].cid) {
        return String(pages[i].cid);
      }
    }
    if (target === 1 && pages[0].cid) {
      return String(pages[0].cid);
    }
  }
  var direct = state && state.videoData && state.videoData.cid;
  if (direct) {
    return String(direct);
  }
  return "";
}

/** 从 `?p=2` 读取分 P 序号，缺省 1。 */
function pageIndex(url) {
  var match = /[?&]p=(\d+)/.exec(String(url));
  var value = match ? parseInt(match[1], 10) : 1;
  return value > 0 ? value : 1;
}

// ---------------------------------------------------------------------------
// 输出 / 收尾
// ---------------------------------------------------------------------------

function outputPath(ctx, root, title, bvid) {
  return uniqueOutput(ctx, root, title, bvid);
}

/** 落盘路径：`<下载目录>/<标题>[ [标记]].mp4`，重名自动追加时间戳（绝不覆盖）。 */
function uniqueOutput(ctx, root, title, marker) {
  var base = safeName(title);
  var name = marker ? base + " [" + marker + "]" : base;
  var path = root + "/" + name + ".mp4";
  if (fileExists(ctx, path)) {
    path = root + "/" + name + "-" + Date.now() + ".mp4";
  }
  return path;
}

function fileExists(ctx, path) {
  var res = ctx.fs.exists(path);
  return Boolean(res && res.ok !== false && res.exists);
}

function safeName(input) {
  var text = String(input == null ? "" : input).replace(/[\x00-\x1f<>:"\/\\|?*]/g, "_");
  text = text.replace(/^[\s.]+|[\s.]+$/g, "");
  if (text.length > 100) {
    text = text.substring(0, 100);
  }
  if (!text) {
    text = "bilibili-video";
  }
  if (/^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/i.test(text)) {
    text = "video_" + text;
  }
  return text;
}

function cleanTitle(raw, bvid) {
  var text = String(raw || "").replace(/\s*[-_]\s*哔哩哔哩.*$/i, "").trim();
  return text || "B站视频 " + bvid;
}

/** 非 B站 页面标题：去空白，缺失时用主机名兜底。 */
function cleanGenericTitle(raw, pageUrl) {
  var text = String(raw || "").trim();
  if (!text) {
    var match = /^https?:\/\/([^\/?#]+)/i.exec(String(pageUrl || ""));
    text = match ? match[1] : "video";
  }
  return text.length > 120 ? text.substring(0, 120) : text;
}

/**
 * 创建虚拟任务卡（`onHolo` 没有宿主任命的任务 id）。
 *
 * beta12.1：创建可能因宿主版本过旧 / 沙箱异常而失败，必须兜住 ——
 * 否则异常冒泡出 hook，插件线程被吞掉后 UI 就再也点不动了。
 */
function startTask(ctx, title, url) {
  try {
    if (typeof ctx.startTask !== "function") {
      ctx.notify("插件能力缺失", "宿主未提供 ctx.startTask，无法创建任务卡片，请升级宿主。");
      return "";
    }
    var created = ctx.startTask({ title: title, url: url });
    return created && created.ok !== false && created.id ? String(created.id) : "";
  } catch (error) {
    ctx.notify(
      "任务创建失败",
      "无法创建任务卡片：" + (error && error.message ? error.message : error),
    );
    return "";
  }
}

function fileName(path) {
  var text = String(path).replace(/\\/g, "/");
  var index = text.lastIndexOf("/");
  return index >= 0 ? text.substring(index + 1) : text;
}

function emit(ctx, taskId, payload) {
  if (!taskId) {
    return;
  }
  payload.id = taskId;
  ctx.emit("plugin-task", payload);
}

function fail(ctx, taskId, message, tempDir) {
  emit(ctx, taskId, { phase: "error", progress: 0, detail: "处理失败", error: message });
  ctx.notify("M4S 合并失败", message);
  if (tempDir) {
    ctx.fs.remove(tempDir);
  }
}

/**
 * 从 FFmpeg 日志（stderr）里提取进度：`time=00:00:32.15` / `speed= 2.2x`。
 *
 * 只返回友好的中文片段，绝不把原始日志行透传给前端。
 */
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
