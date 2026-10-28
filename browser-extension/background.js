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
/** `/probe` 会带最多 256KB HTML + 128KB 状态：留足本机传输时间。 */
const HOLO_TIMEOUT_MS = 6000;

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
        ? `Aria2-Harness 接管已开启（${text === "ON" ? "已连接" : "未连接"}）· 点击打开面板`
        : "Aria2-Harness 接管已关闭 · 点击打开面板",
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

/** 点击图标：开关取反 → 立刻刷新角标（不再弹系统通知，状态由角标 / popup 表达）。 */
async function toggleIntercept() {
  const stored = await chrome.storage.local.get({ enabled: true }).catch(() => ({ enabled: true }));
  const next = stored.enabled === false; // false → true，其余 → false
  state.enabled = next;
  try {
    await chrome.storage.local.set({ enabled: next });
  } catch {
    // 存储失败时本次仍按 next 生效（角标会显示），下次点击再试。
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

// ---------------------------------------------------------------------------
// 全息探针（beta12 · 模块 E）
//
// 触发只有两条路：Popup「解析当前页面」（场景 A）与页面悬浮按钮（场景 B）。
// 后台只负责两件事：
//   1. `/check?url=` 域名预检（不改变激活状态），页面据此决定是否注入按钮；
//   2. `/probe` 提交全息包：把主框架导航的完整请求头（含 Cookie）与页面上下文
//      一起送去宿主，命中站点插件的才被唤醒。
//
// 红线：/check 只做域名匹配，绝不触发插件执行；任何失败都只是"不那么智能"，
// 不影响下载主链路。
// ---------------------------------------------------------------------------
/** 主框架请求头缓存：每个 tab 只留最新一条，120s 过期，最多 32 个 tab。 */
const NAV_TTL_MS = 120_000;
const NAV_LIMIT = 32;
const NAV_MAX_HEADERS = 64;
const NAV_MAX_BYTES = 16 * 1024;
const NAV_MAX_VALUE_BYTES = 8 * 1024;
/** tabId → { url, headers: [{name,value}], at }。 */
const navHeaders = new Map();

// ---------------------------------------------------------------------------
// v1.0.7 网络嗅探线索（inject-network.js → bridge → 这里）
//
// 页面在 MAIN world 劫持 fetch / XHR，把 m3u8 / m4s / mpd / mp4 的 URL 报上来。
// 后台只按 tab 缓存（120s TTL、48 条上限），`/probe` 时作为 `networkHints`
// 附带 —— 2.0「全息探针」的底层数据，当前宿主未消费时会被安全忽略。
// ---------------------------------------------------------------------------
const NET_HINT_TTL_MS = 120_000;
const NET_HINT_LIMIT = 48;
const NET_HINT_MAX_URL = 2048;
/** tabId → { at, hints: [{url, kind}] }。 */
const netHintsByTab = new Map();

function rememberNetHints(tabId, hints) {
  if (!Number.isInteger(tabId) || tabId < 0) return;
  if (!Array.isArray(hints) || hints.length === 0) return;
  const now = Date.now();
  for (const [id, record] of netHintsByTab) {
    if (now - record.at > NET_HINT_TTL_MS) netHintsByTab.delete(id);
  }
  const previous = netHintsByTab.get(tabId);
  const kept = previous && now - previous.at <= NET_HINT_TTL_MS ? previous.hints : [];
  const merged = [];
  const seen = new Set();
  for (const hint of [...kept, ...hints]) {
    const url = String((hint && hint.url) || "").slice(0, NET_HINT_MAX_URL);
    const kind = String((hint && hint.kind) || "");
    // v1.0.9：`bilibili_live_blob` 是页面伪造的「MSE 标记」，不是真 URL，
    // 必须放行；其余线索仍只接受 http(s)。
    const isMarker = url === LIVE_BLOB_MARKER;
    if ((!isMarker && !/^https?:\/\//i.test(url)) || !kind || seen.has(url)) continue;
    seen.add(url);
    merged.push({ url, kind });
  }
  if (merged.length === 0) return;
  netHintsByTab.set(tabId, { at: now, hints: merged.slice(-NET_HINT_LIMIT) });
  while (netHintsByTab.size > NAV_LIMIT) {
    netHintsByTab.delete(netHintsByTab.keys().next().value);
  }
}

/** 取该 tab 的有效线索（过期即清），供 `/probe` 附带。 */
function netHintsFor(tabId) {
  if (!Number.isInteger(tabId) || tabId < 0) return [];
  const record = netHintsByTab.get(tabId);
  if (!record) return [];
  if (Date.now() - record.at > NET_HINT_TTL_MS) {
    netHintsByTab.delete(tabId);
    return [];
  }
  return record.hints;
}

// ---------------------------------------------------------------------------
// v1.0.9 直播直提：直播间**不再**走 /probe
//
// 直播 m3u8 是无限播放列表，交给普通点播解析链路会一直下载、永远无法完成。
// 这里在后台识别直播页（页面主机 / 直播 CDN / .flv 线索），把**真实直播流地址**
// 直接投给 `/add`，由 live-recorder 认领成可停止的录制任务。
// ---------------------------------------------------------------------------
/** 明确的直播站点（页面主机）。 */
const LIVE_PAGE_HOSTS = [
  /(^|\.)live\.bilibili\.com$/i,
  /(^|\.)live\.douyin\.com$/i,
  /(^|\.)live\.twitch\.tv$/i,
  /(^|\.)www\.huya\.com$/i,
  /(^|\.)www\.douyu\.com$/i,
  /(^|\.)live\.yy\.com$/i,
];

/** v1.0.9：页面伪造的 MSE 标记（真实流被 `blob:` 隐藏时由 holo-main.js 塞入）。 */
const LIVE_BLOB_MARKER = "bilibili_live_blob";

/**
 * 单条线索是否带直播特征。
 *
 * 注意：`*.bilivideo.com` 同时也承载普通投稿的 DASH 分片（`.m4s`），
 * 所以不能「见到 bilivideo 就当直播」——必须再叠一条直播特征：
 * `.flv` / `live-bvc` / MSE 标记 / 直播 HLS CDN（mcdn）。
 */
function isLiveHint(hint) {
  const kind = String((hint && hint.kind) || "").toLowerCase();
  const url = String((hint && hint.url) || "");
  if (url === LIVE_BLOB_MARKER) return true;
  if (kind !== "flv" && kind !== "hls") return false;
  if (/\.flv(?:[?#]|$)/i.test(url)) return true;
  if (/live-bvc/i.test(url)) return true;
  return /(^|\.)mcdn\.bilivideo\.cn$/i.test(hostOf(url));
}

/** 直播地址优先级的取值顺序：flv（低延迟）> m3u8 > mp4；MSE 标记不是真地址，跳过。 */
function pickLiveStream(hints) {
  for (const kind of ["flv", "m3u8", "mp4"]) {
    const hit = hints.find(
      (hint) =>
        String((hint && hint.kind) || "").toLowerCase() === kind &&
        /^https?:\/\//i.test(String((hint && hint.url) || "")),
    );
    if (hit) return String(hit.url);
  }
  return "";
}

/** 页面本身或线索里出现直播特征即视为直播页。 */
function isLiveRequest(url, hints) {
  const host = hostOf(url);
  if (LIVE_PAGE_HOSTS.some((pattern) => pattern.test(host))) return true;
  return hints.some(isLiveHint);
}

/**
 * 直播直提：把真实流地址与浏览器上下文交给 `/add`（只暂存，由用户在桌面端确认）。
 * 返回结构与 `reportHolo` 对齐，另带 `live` 标记供页面提示。
 */
async function submitLiveRequest(url, hints, tabId) {
  const stream = pickLiveStream(hints);
  if (!stream) {
    // 情况 B：B站 直播被 MSE（blob:）接管，真实地址拿不到 —— 不要瞎试，
    // 直接把结论回给页面（比让某个插件去拉无限 m3u8 更诚实）。
    const biliLive =
      /(^|\.)live\.bilibili\.com$/i.test(hostOf(url)) ||
      hints.some((hint) => String((hint && hint.url) || "") === LIVE_BLOB_MARKER);
    return {
      ok: false,
      live: true,
      matched: [],
      host: hostOf(url),
      error: biliLive
        ? "B站直播流被浏览器隐藏（MSE格式），无法获取真实地址。请使用专门的B站直播解析插件，或稍后尝试。"
        : "未抓到直播流地址，请等直播开始播放后重试",
    };
  }
  const record = navRecord(tabId, url);
  let cookie = record ? headerValue(record.headers, "cookie") : "";
  if (!cookie) cookie = await collectCookies(url);
  const userAgent = (record && headerValue(record.headers, "user-agent")) || navigator.userAgent;
  const payload = {
    url: stream,
    referer: url,
    cookie,
    user_agent: userAgent,
    headers: record ? record.headers.map((header) => `${header.name}: ${header.value}`) : [],
  };
  const result = await callWithAuth("POST", "/add", JSON.stringify(payload), ADD_TIMEOUT_MS, 1);
  if (!result.ok) {
    return { ok: false, live: true, matched: [], host: hostOf(url), error: describeFailure(result) };
  }
  return { ok: true, live: true, matched: [], host: hostOf(url), stream };
}

/** 只去掉逐跳头；**保留 Cookie**（探针的目的就是还原真实请求上下文）。 */
function pickNavHeaders(requestHeaders) {
  const dropped = new Set([
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
  ]);
  const seen = new Set();
  const picked = [];
  let total = 0;
  for (const header of requestHeaders || []) {
    const name = String(header.name || "").toLowerCase();
    if (!name || dropped.has(name) || name.startsWith(":")) continue;
    if (seen.has(name)) continue;
    const value = String(header.value || "");
    if (!value || value.length > NAV_MAX_VALUE_BYTES) continue;
    const cost = name.length + value.length;
    if (picked.length >= NAV_MAX_HEADERS || total + cost > NAV_MAX_BYTES) break;
    total += cost;
    seen.add(name);
    picked.push({ name, value });
  }
  return picked;
}

function rememberNav(details) {
  if (details.type !== "main_frame") return;
  if (!Number.isInteger(details.tabId) || details.tabId < 0) return;
  if (!isInterceptable(details.url)) return;
  const now = Date.now();
  // v1.0.7：主框架导航 = 新页面，旧页面的媒体线索立即作废。
  netHintsByTab.delete(details.tabId);
  for (const [id, record] of navHeaders) {
    if (now - record.at > NAV_TTL_MS) navHeaders.delete(id);
  }
  navHeaders.set(details.tabId, {
    url: details.url,
    headers: pickNavHeaders(details.requestHeaders),
    at: now,
  });
  while (navHeaders.size > NAV_LIMIT) {
    navHeaders.delete(navHeaders.keys().next().value);
  }
}

function headerValue(headers, name) {
  if (!Array.isArray(headers)) return "";
  const found = headers.find((header) => header.name === name);
  return found ? found.value : "";
}

/** 找回与探针页面同主机的导航头；SPA 换路由时仍可复用。 */
function navRecord(tabId, url) {
  if (!Number.isInteger(tabId) || tabId < 0) return null;
  const record = navHeaders.get(tabId);
  if (!record) return null;
  if (Date.now() - record.at > NAV_TTL_MS) {
    navHeaders.delete(tabId);
    return null;
  }
  try {
    return new URL(record.url).host === new URL(url).host ? record : null;
  } catch {
    return null;
  }
}

function hostOf(url) {
  try {
    return new URL(url).host;
  } catch {
    return "";
  }
}

function describeFailure(result) {
  if (!result) return "未知错误";
  if (result.status === 0) return `连不上桌面端（${result.error || "网络错误"}）`;
  if (result.status === 503) return "宿主插件系统尚未就绪，请稍后重试";
  const message = result.data && result.data.message ? String(result.data.message) : "";
  return message || `宿主拒绝请求（HTTP ${result.status}）`;
}

/**
 * 带 Token 的本地调用：403 时刷新 Token 重试一次；`retries` 控制连不上时
 * 重新扫描端口的次数（Popup 手动触发可以补一次，页面预检不重试）。
 */
async function callWithAuth(method, path, body, timeoutMs, retries = 0) {
  let last = null;
  for (let attempt = 0; attempt <= retries; attempt += 1) {
    if (attempt > 0) {
      await sleep(RETRY_DELAY_MS);
      state.port = 0;
      state.token = "";
      state.connected = false;
    }

    const connection = await resolveConnection();
    if (!connection) {
      last = { ok: false, status: 0, data: {}, error: "未发现桌面端（已扫描 17800~17809）" };
      continue;
    }

    const headers = { "X-Harness-Token": connection.token };
    if (body) headers["Content-Type"] = "application/json";
    const options = { method, headers };
    if (body) options.body = body;

    let result = await request(connection.port, path, options, timeoutMs);
    if (result.status === 403) {
      const token = await refreshToken(connection.port);
      if (token) {
        result = await request(
          connection.port,
          path,
          { ...options, headers: { ...headers, "X-Harness-Token": token } },
          timeoutMs,
        );
      }
    }

    last = result;
    if (result.status !== 0) return result;
  }
  return last || { ok: false, status: 0, data: {}, error: "unknown" };
}

/** `GET /check?url=`：只回答"这个站点有没有站点插件"，不唤醒任何插件。 */
async function checkHolo(url) {
  if (!/^https?:\/\//i.test(url)) {
    return { ok: false, matched: [], error: "仅支持 http/https URL" };
  }
  const result = await callWithAuth(
    "GET",
    `/check?url=${encodeURIComponent(url)}`,
    null,
    HOLO_TIMEOUT_MS,
    0,
  );
  if (!result.ok) {
    return { ok: false, matched: [], host: hostOf(url), error: describeFailure(result) };
  }
  const data = result.data || {};
  return {
    ok: true,
    host: String(data.host || hostOf(url)),
    matched: Array.isArray(data.matched) ? data.matched : [],
  };
}

/** `POST /probe`：组装全息包并提交宿主。 */
async function reportHolo(packet, tabId) {
  const url = String((packet && packet.url) || "").trim();
  if (!/^https?:\/\//i.test(url)) {
    return { ok: false, matched: [], error: "仅支持 http/https 页面" };
  }

  const nav = navRecord(tabId, url);
  let cookie = nav ? headerValue(nav.headers, "cookie") : "";
  if (!cookie) cookie = await collectCookies(url);
  const userAgent = (nav && headerValue(nav.headers, "user-agent")) || navigator.userAgent;
  const referer = (nav && headerValue(nav.headers, "referer")) || url;

  // v1.0.8：先并入页面原生 <video>/<audio> 直链（同样走缓存与清洗），再取全量线索。
  if (packet && Array.isArray(packet.mediaHints)) rememberNetHints(tabId, packet.mediaHints);
  const hints = netHintsFor(tabId);
  // v1.0.9：直播页改为直提真实流地址，绝不进入 /probe（否则直播 m3u8 会被
  // 普通点播解析链路当成无限播放列表，任务永远无法完成）。
  if (isLiveRequest(url, hints)) {
    return submitLiveRequest(url, hints, tabId);
  }

  const payload = {
    url,
    title: String((packet && packet.title) || "").slice(0, 512),
    headers: nav ? nav.headers.map((header) => `${header.name}: ${header.value}`) : [],
    cookie,
    userAgent,
    referer,
    source: "browser-extension",
  };
  if (packet && typeof packet.initialState === "string") payload.initialState = packet.initialState;
  if (packet && typeof packet.html === "string") payload.html = packet.html;
  if (hints.length > 0) payload.networkHints = hints;

  const result = await callWithAuth(
    "POST",
    "/probe",
    JSON.stringify(payload),
    HOLO_TIMEOUT_MS,
    1,
  );
  if (!result.ok) {
    return { ok: false, matched: [], host: hostOf(url), error: describeFailure(result) };
  }
  const data = result.data || {};
  return {
    ok: true,
    host: String(data.host || hostOf(url)),
    matched: Array.isArray(data.matched) ? data.matched : [],
  };
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

// ---------------------------------------------------------------------------
// 网络嗅探（beta8.3）：缓存「像下载」请求的完整 Request Headers。
// 蓝奏云 / 百度网盘等动态 JS 链接直接用 aria2 拉会 403，必须带上浏览器上下文
// （Cookie / User-Agent / Referer）。这里只做旁路记录，绝不阻塞任何请求。
// ---------------------------------------------------------------------------
const SNIFF_LIMIT = 64;
const SNIFF_TTL_MS = 60_000;
/** URL → { headers: [{name, value}], at }。Map 保持插入顺序，超限淘汰最旧。 */
const sniffed = new Map();
/** requestId → url，只用于请求结束时清理。 */
const sniffedRequestIds = new Map();

const DOWNLOAD_EXTENSIONS = [
  ".zip", ".rar", ".7z", ".exe", ".msi", ".msix", ".appx", ".cab",
  ".tar", ".gz", ".tgz", ".bz2", ".xz", ".iso", ".dmg", ".pkg",
  ".apk", ".ipa", ".deb", ".rpm", ".bin", ".pdf", ".xpi", ".crx",
];

/**
 * 动态下载接口常用「无后缀 + 查询参数」形式：`/api/get?code=abc`、
 * `/file?id=123`、`/download.php?f=x`。键命中小写白名单且值非空即视为下载。
 */
const DOWNLOAD_QUERY_KEYS = [
  "download", "dl", "file", "filename", "attach", "attachment", "code",
];

/** 明显下载特征：路径后缀命中，或查询参数键命中（大小写不敏感）。 */
function isDownloadLike(url) {
  let parsed;
  try {
    parsed = new URL(url);
  } catch {
    return false;
  }
  const lower = parsed.pathname.toLowerCase();
  if (DOWNLOAD_EXTENSIONS.some((ext) => lower.endsWith(ext))) return true;
  for (const [key, value] of parsed.searchParams) {
    if (!value.trim()) continue;
    if (DOWNLOAD_QUERY_KEYS.includes(key.toLowerCase())) return true;
  }
  return false;
}

/** 只保留浏览器真正发出去的普通头（去掉 hop-by-hop 与 Cookie，Cookie 单独投递）。 */
function pickHeaders(requestHeaders) {
  const dropped = new Set([
    "host", "content-length", "connection", "keep-alive", "proxy-connection",
    "transfer-encoding", "upgrade", "te", "trailer", "cookie",
  ]);
  const seen = new Set();
  const picked = [];
  for (const header of requestHeaders || []) {
    const name = String(header.name || "").toLowerCase();
    if (!name || dropped.has(name) || name.startsWith(":")) continue;
    if (seen.has(name)) continue;
    seen.add(name);
    picked.push({ name, value: String(header.value || "") });
  }
  return picked;
}

function trimSniffed() {
  const now = Date.now();
  for (const [url, record] of sniffed) {
    if (now - record.at > SNIFF_TTL_MS) sniffed.delete(url);
  }
  while (sniffed.size > SNIFF_LIMIT) {
    sniffed.delete(sniffed.keys().next().value);
  }
}

/** 按 URL 精确命中；未命中时退化到「去掉 query 的路径前缀」匹配。 */
function resolveSniffed(url) {
  if (!url) return null;
  const exact = sniffed.get(url);
  if (exact) return exact;
  const bare = url.split("#")[0].split("?")[0];
  for (const [key, record] of sniffed) {
    if (key.split("#")[0].split("?")[0] === bare) return record;
  }
  return null;
}

/** 从嗅探到的头里取指定名字（大小写不敏感）。 */
function sniffedHeader(record, name) {
  if (!record) return "";
  const lower = name.toLowerCase();
  const found = record.headers.find((header) => header.name === lower);
  return found ? found.value : "";
}

chrome.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    // 全息探针：主框架导航的完整请求头（含 Cookie）按 tab 缓存，供 /probe 还原上下文。
    rememberNav(details);
    if (details.type !== "main_frame" && details.type !== "object") return;
    if (!isInterceptable(details.url) || !isDownloadLike(details.url)) return;
    trimSniffed();
    sniffed.set(details.url, {
      headers: pickHeaders(details.requestHeaders),
      at: Date.now(),
    });
    sniffedRequestIds.set(details.requestId, details.url);
  },
  { urls: ["<all_urls>"] },
  ["requestHeaders", "extraHeaders"],
);

function forgetSniffedRequest(requestId) {
  const url = sniffedRequestIds.get(requestId);
  if (!url) return;
  sniffedRequestIds.delete(requestId);
  // 下载可能在 onCompleted 之后才被用户确认；保留 URL 记录直到 TTL 到期，
  // 只在「请求失败」时立刻丢弃。
}

chrome.webRequest.onCompleted.addListener((details) => {
  forgetSniffedRequest(details.requestId);
}, { urls: ["<all_urls>"] });

chrome.webRequest.onErrorOccurred.addListener((details) => {
  const url = sniffedRequestIds.get(details.requestId);
  sniffedRequestIds.delete(details.requestId);
  if (url) sniffed.delete(url);
}, { urls: ["<all_urls>"] });

/**
 * 组装 `/add` 负载：优先复用嗅探到的浏览器上下文，取不到再退回 Cookie API /
 * 当前标签页 / `navigator.userAgent`。
 */
async function buildPayload(item) {
  const record = resolveSniffed(item.finalUrl || item.url || "");
  const sniffedCookie = sniffedHeader(record, "cookie");
  const [pageCookie, pageReferer] = await Promise.all([
    sniffedCookie ? Promise.resolve(sniffedCookie) : collectCookies(item.url),
    currentPageUrl(item),
  ]);
  const referer = sniffedHeader(record, "referer") || pageReferer;
  const userAgent = sniffedHeader(record, "user-agent") || navigator.userAgent;

  const payload = { url: item.url, user_agent: userAgent };
  if (referer) payload.referer = referer;
  if (pageCookie) payload.cookie = pageCookie;
  if (record && record.headers.length > 0) {
    // 完整浏览器上下文：宿主的 /add 会再清洗一次，前端确认时合并进 aria2 header。
    payload.headers = record.headers.map((header) => `${header.name}: ${header.value}`);
  }
  return payload;
}

/** 拦截时强制现测一次：绝不相信 30 秒心跳留下的旧状态（可能刚好错过软件启动）。 */
async function refreshConnection() {
  const found = await discoverPort();
  if (!found) {
    await markDisconnected();
    return;
  }
  const token = await refreshToken(found);
  if (token) await setConnected(found, token);
  else await markDisconnected();
}

/**
 * 处理单个下载。`suggest` 必须且只能调用一次：
 * 接管成功 → 取消浏览器下载；其余所有分支 → 保留原名放行。
 */
async function intercept(item, suggest) {
  // 同步登记：即便后续接管失败，兜底路径也不会对同一 downloadId 再投一次。
  markHandled(item.id);
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

  await refreshConnection();

  const accepted = await postWithRetry(await buildPayload(item));
  if (accepted) {
    try {
      await chrome.downloads.cancel(item.id);
    } catch {
      // 下载可能已经结束：取消失败不影响已暂存的任务。
    }
  }
  finish();
}

// ---------------------------------------------------------------------------
// 兜底路径（beta10）：`onDeterminingFilename` 之外，再用 `downloads.onCreated`
// 兜住主路径没接到的下载（部分渠道 / 脚本发起的下载不会走文件名判定）。
// ---------------------------------------------------------------------------
const HANDLED_TTL_MS = 60_000;
/** downloadId → 时间戳：主路径已接管的不再重复投递。 */
const handledDownloadIds = new Map();
const URL_DEDUPE_MS = 5_000;
/** url → 时间戳：两条路径几乎同时命中同一地址时只投一次。 */
const recentUrls = new Map();

function markHandled(id) {
  if (!Number.isInteger(id) || id < 0) return;
  const now = Date.now();
  for (const [key, at] of handledDownloadIds) {
    if (now - at > HANDLED_TTL_MS) handledDownloadIds.delete(key);
  }
  handledDownloadIds.set(id, now);
}

function isHandled(id) {
  const at = handledDownloadIds.get(id);
  if (!at) return false;
  if (Date.now() - at > HANDLED_TTL_MS) {
    handledDownloadIds.delete(id);
    return false;
  }
  return true;
}

/** 同 URL 短时去重：返回 false 表示刚刚投递过，本次跳过。 */
function claimUrl(url) {
  const now = Date.now();
  for (const [key, at] of recentUrls) {
    if (now - at > URL_DEDUPE_MS) recentUrls.delete(key);
  }
  const last = recentUrls.get(url);
  if (last && now - last < URL_DEDUPE_MS) return false;
  recentUrls.set(url, now);
  return true;
}

/** 给主路径留出的接管窗口：它在派发时会同步写 handledDownloadIds。 */
const FALLBACK_GRACE_MS = 1000;

async function interceptFallback(item) {
  const url = item.finalUrl || item.url || "";
  if (!isInterceptable(url)) return;

  const settings = await chrome.storage.local.get({ enabled: true }).catch(() => ({ enabled: true }));
  if (settings.enabled === false) return;

  await sleep(FALLBACK_GRACE_MS);
  if (isHandled(item.id)) return; // 主路径已接管
  if (claimUrl(url)) {
    markHandled(item.id);
    const accepted = await postWithRetry(await buildPayload(item));
    if (!accepted) return;
    try {
      await chrome.downloads.cancel(item.id);
    } catch {
      // 下载可能已经结束：取消失败不影响已暂存的任务。
    }
    try {
      // 清掉浏览器里可能残留的 0 字节记录；delete 权限缺失或已删除时忽略。
      await chrome.downloads.erase({ id: item.id });
    } catch {
      // erase 不可用时 cancel 已足够。
    }
  }
}

// MV3：事件监听必须在顶层同步注册，否则 Service Worker 休眠后收不到事件。
chrome.downloads.onCreated.addListener((item) => {
  if (!item || !Number.isInteger(item.id)) return;
  const url = item.finalUrl || item.url || "";
  if (!/^https?:/i.test(url)) return; // blob:/data: 无法交给宿主
  if (handledDownloadIds.has(item.id)) return;
  void interceptFallback(item).catch(() => {
    // 兜底路径失败一律放行浏览器原生下载。
  });
});

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

/** Popup 状态：开关 + 连接状态（未开启时不扫描端口，避免无谓探测）。 */
async function statusReply() {
  const enabled = await readEnabled();
  if (!enabled) {
    return { ok: true, enabled: false, connected: false, port: 0 };
  }
  const connection = await resolveConnection();
  return {
    ok: true,
    enabled: true,
    connected: Boolean(connection),
    port: connection ? connection.port : 0,
  };
}

/**
 * Popup / content script 的统一消息入口。
 *
 * - `harness-status`：Popup 打开时拉状态；
 * - `harness-toggle`：Popup 内切换接管（工具栏图标已改为打开面板）；
 * - `harness-check`：悬浮按钮的域名预检，只读、不唤醒插件；
 * - `harness-probe`：全息包提交（Popup 或悬浮按钮触发）。
 */
chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || typeof message.type !== "string") return undefined;
  // 只接受本扩展内部的调用方（扩展页面 / 自己的 content script）。
  if (sender && sender.id && sender.id !== chrome.runtime.id) return undefined;

  if (message.type === "harness-status") {
    void statusReply()
      .then(sendResponse)
      .catch(() => sendResponse({ ok: false, enabled: true, connected: false, port: 0 }));
    return true;
  }

  if (message.type === "harness-toggle") {
    void toggleIntercept()
      .then(async () => {
        const enabled = await readEnabled();
        sendResponse({ ok: true, enabled, connected: state.connected, port: state.port });
      })
      .catch(() => sendResponse({ ok: false, enabled: state.enabled }));
    return true;
  }

  if (message.type === "harness-check") {
    void checkHolo(String(message.url || ""))
      .then(sendResponse)
      .catch((error) => sendResponse({ ok: false, matched: [], error: String(error) }));
    return true;
  }

  if (message.type === "harness-net-hints") {
    // v1.0.7：页面侧网络嗅探线索只做旁路缓存，永不阻塞 / 永不回包数据。
    const fromTab = sender && sender.tab && Number.isInteger(sender.tab.id) ? sender.tab.id : -1;
    rememberNetHints(fromTab, message.hints);
    sendResponse({ ok: true });
    return undefined;
  }

  if (message.type === "harness-probe") {
    const fromTab = sender && sender.tab && Number.isInteger(sender.tab.id) ? sender.tab.id : -1;
    const tabId = Number.isInteger(message.tabId) ? message.tabId : fromTab;
    void reportHolo(message.packet, tabId)
      .then(sendResponse)
      .catch((error) => sendResponse({ ok: false, matched: [], error: String(error) }));
    return true;
  }

  return undefined;
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
