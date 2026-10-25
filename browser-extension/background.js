// Aria2-Harness 浏览器接管（MV3 Service Worker）
//
// 交互：没有 Popup。点击工具栏图标 = 切换接管开关，角标就是状态灯：
//   ON  （亮蓝）接管开启且已连上桌面端
//   OFF （灰色）用户主动关闭接管
//   ERR （红色）接管开启但连不上（软件没开 / 正在启动）
//
// 连接：桌面端的 17800 可能被占用而顺延到 17801+，所以不写死端口 ——
// 每 30 秒（chrome.alarms，Service Worker 休眠也能唤醒）扫一遍 17800~17809，
// 用 `/status` 的 `status === "ok"` 做防伪校验（**不比对版本号**，避免桌面端
// 一升级扩展就集体 ERR）。
//
// 红线：**绝不因为增强功能失败而让用户下不了文件。** 任何异常都要落到
// `suggest()` 放行浏览器原生下载。

const PORT_START = 17800;
const PORT_COUNT = 10; // 17800 ~ 17809

const ALARM_NAME = "harness-heartbeat";
const ALARM_PERIOD_MINUTES = 0.5; // 30s

/**
 * 单次探测 / 调用的等待上界。
 *
 * 软件没开时，连接会被立刻拒绝（ECONNREFUSED），所以这里放大到 1.5s 不会拖慢回落；
 * 但 Service Worker 冷启动 + 首包建连可能超过 500ms，旧值会让"软件明明开着"也判成
 * 超时（全场端口 ERR 的典型假阴性）。
 */
const PROBE_TIMEOUT_MS = 1500;
const TOKEN_TIMEOUT_MS = 2000;
/** `/add` 只暂存并秒回 200，4s 足够覆盖冷启动抖动。 */
const ADD_TIMEOUT_MS = 4000;

/** 连接被拒后的快速重试：每次等 300ms、最多 3 次，每次重新扫描端口。 */
const RETRY_DELAY_MS = 300;
const MAX_RETRIES = 3;

const DEFAULT_SETTINGS = { enabled: true, hostPort: 0, token: "" };

const BADGE_ON = "#00B4D8";
const BADGE_OFF = "#888888";
const BADGE_ERR = "#FF3B30";

/** 本次 Service Worker 生命周期内的连接状态（被回收后靠 storage + 重新探测恢复）。 */
const state = {
  enabled: true,
  port: 0,
  token: "",
  connected: false,
};

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** 只接管 http(s)；本机接口自身与 blob:/data: 一律放行。 */
function isInterceptable(url) {
  if (!/^https?:\/\//i.test(url)) return false;
  return !/^http:\/\/127\.0\.0\.1:/i.test(url) && !/^http:\/\/localhost:/i.test(url);
}

/** 带超时的 JSON 请求；网络错误 / 超时返回 status=0（并带回可诊断的 error）。 */
async function request(port, path, options = {}, timeoutMs = TOKEN_TIMEOUT_MS) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`http://127.0.0.1:${port}${path}`, {
      // 显式 CORS：/auto-token 靠 Origin: chrome-extension://<id> 放行扩展。
      mode: "cors",
      // 必须 omit：禁止携带 cookie。带凭据的跨域请求会让服务端的
      // `Access-Control-Allow-Origin: *` 失效，也更容易触发 Chrome 对回环地址的拦截。
      cache: "no-store",
      ...options,
      credentials: "omit",
      signal: controller.signal,
    });
    const data = await response.json().catch(() => ({}));
    return { ok: response.ok, status: response.status, data, error: "" };
  } catch (error) {
    // 保留真实原因（net::ERR_CONNECTION_REFUSED / CORS / AbortError），
    // 否则"角标 ERR"只有现象、没有线索。
    return {
      ok: false,
      status: 0,
      data: {},
      error: String((error && error.message) || error || "unknown"),
    };
  } finally {
    clearTimeout(timer);
  }
}

/**
 * 探测单个端口：**宽松校验**，只认 `res.ok && data.status === "ok"`。
 *
 * 刻意不比对版本号 —— 桌面端升级后 `/status` 的 version 随时会变，扩展如果
 * 强依赖某个字面量，升级当天就会集体变成 ERR。防伪只需要"它确实是我们
 * 的 API"，这一点 `status: ok` 已经足够。
 */
async function probePort(port) {
  const result = await request(port, "/status", {}, PROBE_TIMEOUT_MS);
  return {
    port,
    ok: Boolean(result.ok && result.data && result.data.status === "ok"),
    error: result.error || "",
  };
}

/**
 * 扫描 17800~17809，返回首个通过校验的端口（0 = 没找到）。
 *
 * 用 `Promise.allSettled`：任何一个端口的 fetch 超时 / 抛错都不能让整次扫描
 * 失败（`Promise.all` 会因为一个 rejected 直接短路）。并行探测同时保证
 * 软件没开时也能在 ~1.5s 内全部落空并回退。
 */
async function discoverPort() {
  const ports = Array.from({ length: PORT_COUNT }, (_, index) => PORT_START + index);
  const settled = await Promise.allSettled(ports.map((port) => probePort(port)));
  const results = settled
    .filter((item) => item.status === "fulfilled")
    .map((item) => item.value);
  const found = results.find((item) => item.ok)?.port || 0;
  if (!found) {
    // 全端口落空：把首个真实原因打进控制台。没有这行，ERR 角标永远只有现象。
    const reason = results.find((item) => item.error)?.error || "全部端口连接被拒";
    console.warn(`[Harness] 17800~17809 探测失败：${reason}`);
  }
  if (found && found !== state.port) {
    state.port = found;
    try {
      await chrome.storage.local.set({ hostPort: found });
    } catch {
      // 存储不可用不影响本次会话（内存里已经有一份）。
    }
  }
  return found;
}

/** 向该端口重新领取 Token（宿主重启会轮换，旧值必然 403）。 */
async function refreshToken(port) {
  const result = await request(port, "/auto-token");
  const token = result.ok ? String(result.data.token || "").trim() : "";
  if (!token) return "";
  state.token = token;
  try {
    await chrome.storage.local.set({ token });
  } catch {
    // 同上：内存可用即可。
  }
  return token;
}

/** 记住一条已验证可用的连接，并刷新角标。 */
async function setConnected(port, token) {
  state.port = port;
  state.token = token;
  state.connected = true;
  try {
    await chrome.storage.local.set({ hostPort: port, token });
  } catch {
    // 存储失败只是下次多扫一遍端口。
  }
  await updateBadge();
  return { port, token };
}

/** 连接不可用：清掉内存态（保留 storage，下次可先验证旧端口）。 */
async function markDisconnected() {
  state.port = 0;
  state.token = "";
  state.connected = false;
  await updateBadge();
}

/** 取有效连接：内存 → storage（先校验端口）→ 全量扫描 + 新 Token。 */
async function resolveConnection() {
  if (state.port && state.token && (await probePort(state.port)).ok) {
    state.connected = true;
    await updateBadge();
    return { port: state.port, token: state.token };
  }

  let stored = DEFAULT_SETTINGS;
  try {
    stored = await chrome.storage.local.get(DEFAULT_SETTINGS);
  } catch {
    // 读取失败：直接走扫描。
  }

  if (stored.hostPort && (await probePort(stored.hostPort)).ok) {
    const token = String(stored.token || "").trim() || (await refreshToken(stored.hostPort));
    if (token) return setConnected(stored.hostPort, token);
  }

  const port = await discoverPort();
  if (!port) {
    await markDisconnected();
    return null;
  }
  const token = await refreshToken(port);
  if (!token) {
    await markDisconnected();
    return null;
  }
  return setConnected(port, token);
}

/** 从 storage 同步最新开关（storage 是唯一真相，点击图标 / 手动清除都能生效）。 */
async function readEnabled() {
  try {
    const stored = await chrome.storage.local.get({ enabled: true });
    state.enabled = stored.enabled !== false;
  } catch {
    // 读不到时维持当前值，默认开启。
  }
  return state.enabled;
}

/** 角标 = 状态灯：ON / OFF / ERR。 */
async function updateBadge() {
  await readEnabled();
  let text = "OFF";
  let color = BADGE_OFF;
  if (state.enabled) {
    if (state.connected && state.port && state.token) {
      text = "ON";
      color = BADGE_ON;
    } else {
      text = "ERR";
      color = BADGE_ERR;
    }
  }
  try {
    await chrome.action.setBadgeText({ text });
    await chrome.action.setBadgeBackgroundColor({ color });
    await chrome.action.setTitle({
      title: state.enabled
        ? `Aria2-Harness 接管已开启（${text === "ON" ? "已连接" : "未连接"}）· 点击关闭`
        : "Aria2-Harness 接管已关闭 · 点击开启",
    });
  } catch {
    // action API 在极少数受限环境不可用，不影响下载主链路。
  }
}

/** 心跳：静默探测 + 刷新角标（不弹通知、不打扰用户）。 */
async function heartbeat() {
  console.log("[Harness] Heartbeat running...");
  if (!(await readEnabled())) {
    await markDisconnected();
    return;
  }
  const connection = await resolveConnection();
  if (!connection) {
    console.warn("[Harness] 未发现桌面端（已扫描 17800~17809），角标保持 ERR");
  }
}

/** 点击图标：开关取反 → 系统通知 → 立刻刷新角标。 */
async function toggleIntercept() {
  const stored = await chrome.storage.local.get({ enabled: true }).catch(() => ({ enabled: true }));
  const next = stored.enabled === false; // false → true，其余 → false
  state.enabled = next;
  try {
    await chrome.storage.local.set({ enabled: next });
  } catch {
    // 存储失败时本次仍按 next 生效（角标会显示），下次点击再试。
  }

  try {
    await chrome.notifications.create({
      type: "basic",
      iconUrl: "icon128.png",
      title: "Aria2-Harness",
      message: next
        ? "接管已开启：浏览器下载将交给本机确认"
        : "接管已关闭：浏览器将使用原生下载",
    });
  } catch {
    // 通知权限被策略禁用时静默：角标本身已经能表达状态。
  }

  await updateBadge();
  if (next) {
    // 立刻补一次连接探测，让角标尽快从 ERR 变 ON。
    void heartbeat();
  }
}

/** 拉取 Cookie（无权限 / 特殊协议时返回空串，不影响下载）。 */
async function collectCookies(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    return cookies.map((cookie) => `${cookie.name}=${cookie.value}`).join("; ");
  } catch {
    return "";
  }
}

/** 当前标签页地址作为 Referer；取不到则退回下载项自带的 referrer。 */
async function currentPageUrl(item) {
  try {
    const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
    if (tab && tab.url && /^https?:\/\//i.test(tab.url)) return tab.url;
  } catch {
    // 无痕 / 应用窗口取不到标签页：忽略。
  }
  return item.referrer && /^https?:\/\//i.test(item.referrer) ? item.referrer : "";
}

/** 投递一次；返回 "ok" / "auth"(403) / "server"(其它状态) / "network"(连不上)。 */
async function postAdd(port, token, payload) {
  const result = await request(
    port,
    "/add",
    {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        "X-Harness-Token": token,
      },
      body: JSON.stringify(payload),
    },
    ADD_TIMEOUT_MS,
  );
  if (result.status === 200) return "ok";
  if (result.status === 403) return "auth";
  if (result.status === 0) return "network";
  return "server";
}

/**
 * 投递 + 防呆重试。
 *
 * - 连不上（TypeError / 端口还没绑上）：等 300ms，重新扫描端口后再试，最多 3 次。
 * - 403：立刻刷新 Token 重试一次。
 * - 其它 4xx/5xx：重试无意义，直接放行原生下载。
 */
async function postWithRetry(payload) {
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt += 1) {
    if (attempt > 0) {
      await sleep(RETRY_DELAY_MS);
      // 每次重试前强制重新发现：桌面端可能刚在别的端口启动。
      state.port = 0;
      state.token = "";
      state.connected = false;
    }

    const connection = await resolveConnection();
    if (!connection) continue;

    const outcome = await postAdd(connection.port, connection.token, payload);
    if (outcome === "ok") return true;
    if (outcome === "auth") {
      const token = await refreshToken(connection.port);
      if (token && (await postAdd(connection.port, token, payload)) === "ok") return true;
      return false;
    }
    if (outcome === "server") return false;
    // network：进入下一轮重试。
  }
  return false;
}

/**
 * 处理单个下载。`suggest` 必须且只能调用一次：
 * 接管成功 → 取消浏览器下载；其余所有分支 → 保留原名放行。
 */
async function intercept(item, suggest) {
  let settled = false;
  const finish = () => {
    if (settled) return;
    settled = true;
    suggest({ filename: item.filename });
  };

  const settings = await chrome.storage.local.get({ enabled: true }).catch(() => ({ enabled: true }));
  if (settings.enabled === false) {
    finish();
    return;
  }

  // 拦截时强制现测一次：绝不相信 30 秒心跳留下的旧状态（可能刚好错过软件启动）。
  const found = await discoverPort();
  if (found) {
    const token = await refreshToken(found);
    if (token) await setConnected(found, token);
    else await markDisconnected();
  } else {
    await markDisconnected();
  }

  // Cookie、Referer 并行取，尽量缩短拦截耗时。
  const [cookie, referer] = await Promise.all([
    collectCookies(item.url),
    currentPageUrl(item),
  ]);

  const payload = { url: item.url, user_agent: navigator.userAgent };
  if (referer) payload.referer = referer;
  if (cookie) payload.cookie = cookie;

  const accepted = await postWithRetry(payload);
  if (accepted) {
    try {
      await chrome.downloads.cancel(item.id);
    } catch {
      // 下载可能已经结束：取消失败不影响已暂存的任务。
    }
  }
  finish();
}

/** 确保心跳闹钟存在（SW 每次唤醒都校验一次，幂等）。 */
async function ensureAlarm() {
  try {
    const existing = await chrome.alarms.get(ALARM_NAME);
    if (!existing) {
      await chrome.alarms.create(ALARM_NAME, { periodInMinutes: ALARM_PERIOD_MINUTES });
    }
  } catch {
    // alarms 不可用时退化为仅"下载时探测"，功能仍可用。
  }
}

async function bootstrap() {
  await readEnabled();
  await ensureAlarm();
  await updateBadge();
  if (state.enabled) void heartbeat();
}

chrome.action.onClicked.addListener(() => {
  void toggleIntercept();
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name === ALARM_NAME) void heartbeat();
});

chrome.runtime.onInstalled.addListener(() => {
  void bootstrap();
});
chrome.runtime.onStartup.addListener(() => {
  void bootstrap();
});

// SW 每次被唤醒（含跨天休眠后）都重新对齐状态与角标。
void bootstrap();

chrome.downloads.onDeterminingFilename.addListener((item, suggest) => {
  const url = item.finalUrl || item.url || "";
  if (!isInterceptable(url)) {
    suggest({ filename: item.filename });
    return false;
  }

  intercept(item, suggest).catch(() => {
    // intercept 内部已兜底；这里再兜一层，绝不让异常把下载挂死。
    try {
      suggest({ filename: item.filename });
    } catch {
      // 已经 suggest 过 —— 正常情况，忽略。
    }
  });
  // 异步决定文件名：告诉 Chrome 稍后调用 suggest。
  return true;
});
