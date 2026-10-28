// Aria2-Harness 全息探针 · 隔离世界中继（beta12 · 模块 E）
//
// MAIN world 的 holo-main.js 拿不到 chrome.* API，本脚本负责把它的
// postMessage 请求转成 runtime 消息，再把宿主应答回传。
//
// 红线：只做中继，不碰 DOM；`/check` 的**命中**结果按 URL 缓存 30s，避免 SPA 反复询问宿主
//（未命中不缓存，用户新增自定义域名后 F5 即可命中）。

(() => {
  "use strict";

  const MAIN = "harness-holo-main";
  const NET = "harness-holo-net";
  const BRIDGE = "harness-holo-bridge";
  const CHECK_TTL_MS = 30_000;
  const CHECK_LIMIT = 24;
  const COLLECT_TIMEOUT_MS = 2500;
  const PROBE_THROTTLE_MS = 800;
  const NET_HINT_LIMIT = 48;
  const NET_URL_MAX = 2048;
  /**
   * v1.0.7 网络嗅探类型白名单（与 inject-network.js 的 classify 保持一致）。
   * v1.0.9：补 `flv` —— B站 直播真实流是 FLV，白名单缺它会把这些线索全部丢掉。
   */
  const NET_KINDS = new Set(["hls", "m4s", "dash", "mp4", "flv"]);

  /** href → { at, matched }：只缓存**命中**结果（空列表每次重问宿主，保证配置即时生效）。 */
  const checkCache = new Map();
  /** requestId → { resolve, timer }：Popup 采集请求的等待队列。 */
  const collecting = new Map();
  let checking = 0;
  let lastProbeAt = 0;

  /** 页面只能为**自己所在的站点**发起探针，防止脚本伪造其他域名。 */
  function sameHost(url) {
    try {
      return new URL(url).host === location.host;
    } catch {
      return false;
    }
  }

  function postToMain(message) {
    window.postMessage({ source: BRIDGE, ...message }, "*");
  }

  function cachedCheck(href) {
    const record = checkCache.get(href);
    if (!record) return null;
    if (Date.now() - record.at > CHECK_TTL_MS) {
      checkCache.delete(href);
      return null;
    }
    return record.matched;
  }

  function rememberCheck(href, matched) {
    const now = Date.now();
    for (const [key, record] of checkCache) {
      if (now - record.at > CHECK_TTL_MS) checkCache.delete(key);
    }
    checkCache.set(href, { at: now, matched });
    while (checkCache.size > CHECK_LIMIT) {
      checkCache.delete(checkCache.keys().next().value);
    }
  }

  function replyCheck(url, matched) {
    postToMain({ type: "check-result", url, matched });
  }

  function runCheck(url) {
    checking += 1;
    try {
      chrome.runtime.sendMessage({ type: "harness-check", url }, (response) => {
        checking -= 1;
        if (chrome.runtime.lastError || !response || !response.ok) {
          // 宿主没开 / 插件系统未就绪：回空列表，页面侧 10s 后再问一次。
          replyCheck(url, []);
          return;
        }
        const matched = Array.isArray(response.matched) ? response.matched : [];
        // beta12.4：只缓存**命中**结果。用户刚在桌面端保存自定义域名时，
        // 同一 URL 的旧"未命中"缓存会让 F5 后仍等 30s，与"保存即生效"矛盾。
        if (matched.length > 0) rememberCheck(url, matched);
        replyCheck(url, matched);
      });
    } catch {
      checking -= 1;
      replyCheck(url, []);
    }
  }

  function runProbe(data) {
    const now = Date.now();
    if (now - lastProbeAt < PROBE_THROTTLE_MS) return;
    lastProbeAt = now;

    const packet = {
      url: data.url,
      title: data.title,
      initialState: data.initialState,
      html: data.html,
      // v1.0.8：原生 <video>/<audio> 直链由 MAIN world 采集，这里原样转交后台。
      mediaHints: Array.isArray(data.mediaHints) ? data.mediaHints : [],
    };
    try {
      chrome.runtime.sendMessage({ type: "harness-probe", packet }, (response) => {
        if (chrome.runtime.lastError || !response) {
          postToMain({ type: "probe-result", ok: false, matched: [] });
          return;
        }
        postToMain({
          type: "probe-result",
          ok: Boolean(response.ok),
          matched: Array.isArray(response.matched) ? response.matched : [],
          // v1.0.9：直播直提结果（后台走 /add，不走 /probe），页面据此给出提示。
          live: Boolean(response.live),
          error: response.error ? String(response.error).slice(0, 200) : "",
        });
      });
    } catch {
      postToMain({ type: "probe-result", ok: false, matched: [] });
    }
  }

  /**
   * v1.0.7：把 MAIN world 嗅探到的媒体线索转给后台。
   *
   * 只做白名单清洗：http(s) URL、已知类型、长度上限；后台按 tab 缓存，
   * 供 `/probe` 附带（2.0 全息探针的底层数据）。
   */
  function forwardNetHints(hints) {
    if (!Array.isArray(hints) || hints.length === 0) return;
    const clean = [];
    for (const hint of hints.slice(0, NET_HINT_LIMIT)) {
      const url = String((hint && hint.url) || "");
      if (!/^https?:\/\//i.test(url) || url.length > NET_URL_MAX) continue;
      const kind = String((hint && hint.kind) || "");
      if (!NET_KINDS.has(kind)) continue;
      clean.push({ url, kind });
    }
    if (clean.length === 0) return;
    try {
      chrome.runtime.sendMessage({ type: "harness-net-hints", hints: clean }, () => {
        // 后台休眠 / 未响应时读取 lastError，避免控制台刷错。
        void chrome.runtime.lastError;
      });
    } catch {
      // 扩展上下文失效（升级 / 卸载）：静默丢弃。
    }
  }

  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    const data = event.data;
    if (!data) return;

    // 网络嗅探由 inject-network.js 在 MAIN world 发出，独立于 holo-main 的协议。
    if (data.source === NET) {
      if (data.type === "net-hints") forwardNetHints(data.hints);
      return;
    }

    if (data.source !== MAIN) return;

    if (data.type === "check") {
      const url = String(data.url || "");
      if (!sameHost(url)) return;
      const cached = cachedCheck(url);
      if (cached) {
        replyCheck(url, cached);
        return;
      }
      // 同一时刻只允许一次预检，避免 SPA 频繁变更 DOM 时放大请求。
      if (checking > 0) return;
      runCheck(url);
      return;
    }

    if (data.type === "probe") {
      if (!sameHost(String(data.url || ""))) return;
      runProbe(data);
      return;
    }

    if (data.type === "collect-result") {
      const pending = collecting.get(data.requestId);
      if (!pending) return;
      collecting.delete(data.requestId);
      clearTimeout(pending.timer);
      pending.resolve({ ok: true, data: data.data || null });
    }
  });

  // Popup 采集：Popup → bridge → holo-main；MAIN 不可用时由 Popup 走
  // chrome.scripting.executeScript 兜底。
  chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
    if (!message || message.type !== "harness-collect") return undefined;
    if (sender && sender.id && sender.id !== chrome.runtime.id) return undefined;

    const requestId = `${Date.now()}-${Math.random().toString(36).slice(2)}`;
    const reply = (value) => {
      try {
        sendResponse(value);
      } catch {
        // Popup 已关闭：通道失效，忽略。
      }
    };
    const timer = setTimeout(() => {
      collecting.delete(requestId);
      reply({ ok: false, error: "页面脚本未响应" });
    }, COLLECT_TIMEOUT_MS);
    collecting.set(requestId, { resolve: reply, timer });
    postToMain({ type: "collect", requestId });
    return true;
  });
})();
