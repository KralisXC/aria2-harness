// Aria2-Harness Popup（beta12 · 模块 E 场景 A）
//
// 保底路径：用户点击「解析当前页面」→ 采集全息包 → 后台 POST /probe。
// 采集优先走 content script（holo-bridge → holo-main）；页面在扩展安装前就已
// 打开、或 MAIN 脚本异常时，用 chrome.scripting.executeScript 注入采集器兜底。

(() => {
  "use strict";

  const COLLECT_TIMEOUT_MS = 4000;
  const PROBE_TIMEOUT_MS = 15000;
  const STATUS_TIMEOUT_MS = 5000;

  const els = {
    state: document.getElementById("state"),
    toggle: document.getElementById("toggle"),
    parse: document.getElementById("parse"),
    result: document.getElementById("result"),
  };

  let tabId = -1;
  let busy = false;

  function setPill(text, tone) {
    els.state.textContent = text;
    els.state.className = tone ? `pill ${tone}` : "pill";
  }

  function setResult(text, tone) {
    els.result.textContent = text;
    els.result.className = tone ? `result ${tone}` : "result";
  }

  /** 带超时的 runtime 消息：后台卡死 / Service Worker 未唤醒时不让 Popup 悬住。 */
  function runtimeMessage(message, timeoutMs) {
    return new Promise((resolve) => {
      let settled = false;
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        resolve({ ok: false, error: "宿主响应超时" });
      }, timeoutMs);
      try {
        chrome.runtime.sendMessage(message, (response) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (chrome.runtime.lastError) {
            resolve({ ok: false, error: chrome.runtime.lastError.message || "扩展通信失败" });
            return;
          }
          resolve(response || { ok: false, error: "宿主无响应" });
        });
      } catch (error) {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve({ ok: false, error: String((error && error.message) || error) });
      }
    });
  }

  /** 向 content script 发消息；无接收方（页面未注入）时返回 null。 */
  function tabMessage(targetTabId, message, timeoutMs) {
    return new Promise((resolve) => {
      let settled = false;
      const timer = setTimeout(() => {
        if (settled) return;
        settled = true;
        resolve(null);
      }, timeoutMs);
      try {
        chrome.tabs.sendMessage(targetTabId, message, { frameId: 0 }, (response) => {
          if (settled) return;
          settled = true;
          clearTimeout(timer);
          if (chrome.runtime.lastError) {
            resolve(null);
            return;
          }
          resolve(response || null);
        });
      } catch {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        resolve(null);
      }
    });
  }

  async function activeTab() {
    try {
      const tabs = await chrome.tabs.query({ active: true, currentWindow: true });
      return tabs && tabs[0] ? tabs[0] : null;
    } catch {
      return null;
    }
  }

  async function collectPacket(targetTabId) {
    const relayed = await tabMessage(targetTabId, { type: "harness-collect" }, COLLECT_TIMEOUT_MS);
    if (relayed && relayed.ok && relayed.data && relayed.data.url) return relayed.data;
    return injectCollector(targetTabId);
  }

  async function injectCollector(targetTabId) {
    try {
      const results = await chrome.scripting.executeScript({
        target: { tabId: targetTabId },
        world: "MAIN",
        func: collectPage,
      });
      const result = Array.isArray(results) && results[0] ? results[0].result : null;
      return result && result.url ? result : null;
    } catch {
      return null;
    }
  }

  function applyStatus(status) {
    const ok = Boolean(status && status.ok);
    const enabled = ok ? status.enabled !== false : true;
    const connected = Boolean(ok && status.connected);
    const port = ok && Number.isInteger(status.port) ? status.port : 0;

    els.toggle.classList.toggle("on", enabled);
    els.toggle.setAttribute("aria-checked", String(enabled));

    if (!enabled) {
      setPill("已关闭", "off");
      return;
    }
    setPill(connected && port ? `已连接 ${port}` : connected ? "已连接" : "未连接", connected ? "on" : "err");
  }

  async function refreshStatus() {
    const status = await runtimeMessage({ type: "harness-status" }, STATUS_TIMEOUT_MS);
    applyStatus(status);
    return status;
  }

  function renderProbe(response) {
    if (!response || !response.ok) {
      const reason = (response && response.error) || "宿主无响应";
      setResult(`解析失败：${reason}`, "err");
      return;
    }
    const host = response.host || "当前页面";
    const matched = Array.isArray(response.matched) ? response.matched : [];
    if (matched.length === 0) {
      setResult(`已解析 ${host}：未命中站点插件（全息包已送达宿主）。`, "warn");
      return;
    }
    setResult(`已解析 ${host}：命中 ${matched.length} 个插件 — ${matched.join("、")}`, "ok");
  }

  async function parseCurrentPage() {
    if (busy) return;
    busy = true;
    els.parse.disabled = true;
    els.parse.textContent = "解析中…";

    try {
      const tab = await activeTab();
      if (!tab || !Number.isInteger(tab.id)) {
        setResult("找不到当前标签页，请刷新页面后重试。", "err");
        return;
      }
      if (!/^https?:\/\//i.test(tab.url || "")) {
        setResult("当前页面不支持解析（仅支持 http/https 网页）。", "err");
        return;
      }
      tabId = tab.id;

      setResult("正在采集页面上下文…");
      const packet = await collectPacket(tab.id);
      if (!packet) {
        setResult("采集失败：页面脚本未就绪，请刷新页面后重试。", "err");
        return;
      }

      setResult("正在提交宿主解析…");
      const response = await runtimeMessage(
        { type: "harness-probe", tabId: tab.id, packet },
        PROBE_TIMEOUT_MS,
      );
      renderProbe(response);
    } finally {
      busy = false;
      els.parse.disabled = false;
      els.parse.textContent = "解析当前页面";
    }
  }

  els.toggle.addEventListener("click", async () => {
    if (els.toggle.disabled) return;
    els.toggle.disabled = true;
    const response = await runtimeMessage({ type: "harness-toggle" }, STATUS_TIMEOUT_MS);
    els.toggle.disabled = false;
    if (!response || !response.ok) {
      setResult("开关切换失败：扩展后台无响应。", "err");
      return;
    }
    applyStatus({
      ok: true,
      enabled: response.enabled,
      connected: response.connected,
      port: response.port,
    });
    // 刚开启时后台还在探测端口：再拉一次状态，让角标尽快变成"已连接"。
    if (response.enabled) void refreshStatus();
  });

  els.parse.addEventListener("click", () => {
    void parseCurrentPage();
  });

  async function init() {
    const tab = await activeTab();
    tabId = tab && Number.isInteger(tab.id) ? tab.id : -1;
    if (!tab || !/^https?:\/\//i.test(tab.url || "")) {
      setResult("当前页面不支持解析（仅支持 http/https 网页）。", "warn");
    }
    void refreshStatus();
  }

  void init();

  /**
   * 兜底采集器：由 chrome.scripting.executeScript 注入 MAIN world 执行。
   * 必须是**自包含**函数（序列化后执行，不能引用外部变量）。
   */
  function collectPage() {
    const MAX_HTML_BYTES = 256 * 1024;
    const MAX_STATE_BYTES = 128 * 1024;
    const MAX_TITLE_CHARS = 512;
    const STATE_KEYS = [
      "__INITIAL_STATE__",
      "__NEXT_DATA__",
      "__NUXT__",
      "__APOLLO_STATE__",
      "__PRELOADED_STATE__",
    ];

    const encoder = new TextEncoder();
    const boundedText = (text, maxBytes) => {
      if (typeof text !== "string" || text.length > maxBytes) return null;
      try {
        return encoder.encode(text).length <= maxBytes ? text : null;
      } catch {
        return null;
      }
    };

    let initialState = null;
    for (const key of STATE_KEYS) {
      try {
        const value = window[key];
        if (value === undefined || value === null) continue;
        const text = boundedText(JSON.stringify(value), MAX_STATE_BYTES);
        if (text !== null) {
          initialState = text;
          break;
        }
      } catch {
        // getter 抛错 / 循环引用：尝试下一个键。
      }
    }

    let html = null;
    try {
      html = boundedText(
        document.documentElement ? document.documentElement.outerHTML : "",
        MAX_HTML_BYTES,
      );
    } catch {
      html = null;
    }

    return {
      url: location.href,
      title: String(document.title || "").slice(0, MAX_TITLE_CHARS),
      initialState,
      html,
    };
  }
})();
