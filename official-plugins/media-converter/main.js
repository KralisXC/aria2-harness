// 本地媒体转换（beta12 · Step 2）
//
// 触发：已完成任务的右键菜单 → manifest.actions → 宿主 run_plugin_task("onAction")。
// 事件：{ hook:"onAction", action:"to-mp4" | "extract-audio", task:{gid,name,path,dir} }
//
// 原则：只做**无损封装**（-c copy），绝不重新编码；输出重名时追加时间戳，永不覆盖。
//
// 宿主注入的上下文：
//   ctx.startTask({title,url})                 → {ok,id}（建立可见任务卡）
//   ctx.exec({program,args,timeoutMs})          → {ok,code,stdout,stderr,timedOut,error}
//   ctx.fs.exists(path)                         → {ok,exists}
//   ctx.notify(title, body)                     → 桌面通知

var harness = globalThis.harness;

var CONVERT_TIMEOUT_MS = 1800000;

harness.on("onAction", function (ctx, event) {
  var action = String((event && event.action) || "").trim();
  var task = (event && event.task) || {};
  var input = String(task.path || "").trim();

  if (!input) {
    ctx.notify("无法处理", "任务没有本地文件路径（可能尚未完成）。");
    return;
  }

  if (action === "to-mp4") {
    convert(ctx, task, input, "mp4");
    return;
  }
  if (action === "extract-audio") {
    convert(ctx, task, input, "m4a");
    return;
  }
  ctx.log("warn", "未知动作：" + action);
});

/**
 * @param {"mp4"|"m4a"} kind 目标容器：mp4 = 视频封装，m4a = 仅音轨
 */
function convert(ctx, task, input, kind) {
  var isMp4 = /\.mp4$/i.test(input);
  if (kind === "mp4" && isMp4) {
    ctx.notify("无需转换", "源文件已经是 MP4：" + fileName(input));
    return;
  }

  var dir = dirOf(input);
  var base = safeName(baseName(input));
  var output = joinPath(dir, base + "." + kind);
  if (fileExists(ctx, output)) {
    output = joinPath(dir, base + "-" + Date.now() + "." + kind);
  }

  var label = kind === "mp4" ? "极速转 MP4" : "提取音轨";
  var taskId = "";
  var created = ctx.startTask({ title: label + " · " + base, url: input });
  if (created && created.ok !== false && created.id) {
    taskId = String(created.id);
  }

  emit(ctx, taskId, { phase: "downloading", progress: 5, detail: label + "：封装中" });

  var args =
    kind === "mp4"
      ? ["-y", "-i", input, "-c", "copy", "-movflags", "+faststart", output]
      : ["-y", "-i", input, "-vn", "-c:a", "copy", output];

  var result = ctx.exec({
    program: "ffmpeg",
    args: args,
    cwd: dir || undefined,
    timeoutMs: CONVERT_TIMEOUT_MS,
  });

  if (!result || result.ok === false) {
    var message = describeFailure(result);
    emit(ctx, taskId, {
      phase: "error",
      progress: 0,
      detail: label + "失败",
      error: message,
    });
    ctx.notify(label + "失败", message);
    return;
  }

  emit(ctx, taskId, { phase: "merging", progress: 95, detail: "合并完成！" });
  emit(ctx, taskId, {
    phase: "complete",
    progress: 100,
    detail: "已保存：" + fileName(output),
    outputPath: output,
  });
  ctx.notify(label + "完成", fileName(output));
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

function emit(ctx, taskId, payload) {
  if (!taskId) {
    return;
  }
  payload.id = taskId;
  ctx.emit("plugin-task", payload);
}

function dirOf(path) {
  var text = String(path).replace(/\\/g, "/");
  var index = text.lastIndexOf("/");
  return index > 0 ? text.substring(0, index) : "";
}

function baseName(path) {
  var name = fileName(path);
  var dot = name.lastIndexOf(".");
  return dot > 0 ? name.substring(0, dot) : name;
}

function fileName(path) {
  var text = String(path).replace(/\\/g, "/");
  var index = text.lastIndexOf("/");
  return index >= 0 ? text.substring(index + 1) : text;
}

function joinPath(dir, name) {
  var base = String(dir || "");
  if (!base) {
    return name;
  }
  return base.replace(/[\\/]+$/, "") + "/" + name;
}

function safeName(input) {
  var text = String(input == null ? "" : input).replace(/[\x00-\x1f<>:"\/\\|?*]/g, "_");
  text = text.replace(/^[\s.]+|[\s.]+$/g, "");
  if (text.length > 100) {
    text = text.substring(0, 100);
  }
  return text || "output";
}

function fileExists(ctx, path) {
  var res = ctx.fs.exists(path);
  return Boolean(res && res.ok !== false && res.exists);
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

function describeFailure(result) {
  if (!result) {
    return "FFmpeg 未执行";
  }
  if (result.error) {
    return String(result.error);
  }
  var stderr = String(result.stderr || "");
  var progress = ffmpegProgress(stderr);
  var prefix = "转换失败";
  if (progress) {
    prefix += "（" + progress + "）";
  }
  if (stderr.indexOf("路径越权") >= 0) {
    return "路径越权：该文件不在下载目录内，请把任务下载到默认下载目录后重试。";
  }
  if (stderr.indexOf("Could not find tag for codec") >= 0 ||
      stderr.indexOf("not currently supported in container") >= 0) {
    return "容器不兼容：该文件的编码无法无损封装，请改用转码方式。";
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
