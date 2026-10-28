// Aria2-Harness 全息探针 · 页面主世界脚本（扩展 v1.0.9 · 仿 IDM 悬浮窗架构）
//
// 职责：
//   1. 页面上下文采集（__INITIAL_STATE__ / HTML 快照，严格体积上限）；
//   2. `/check` 命中站点插件后，在 document.body 下用 Shadow DOM 注入唯一悬浮按钮；
//   3. 按钮使用文档坐标绝对定位，跟随 <video> / <audio> / <object>；滚动 / 缩放用 rAF 合帧重算；
//   4. 悬停唤醒：鼠标在媒体区域或页面右侧时淡入，3s 无活动淡出。
//
// 性能红线：
//   - MutationObserver 500ms 节流，绝不逐次扫描 DOM；
//   - scroll / resize / mousemove 全部 requestAnimationFrame 合帧；
//   - 无 setInterval（less 轮询鼠标坐标）；
//   - 媒体不可见 / 坐标异常时回落 fixed 定位，保证按钮一定能被看到。

(() => {
  "use strict";

  const MAIN = "harness-holo-main";
  const BRIDGE = "harness-holo-bridge";
  const HOST_ID = "harness-holo-host";
  const MAX_HTML_BYTES = 256 * 1024;
  const MAX_STATE_BYTES = 128 * 1024;
  const MAX_TITLE_CHARS = 512;
  /** 原生媒体直链兜底（v1.0.8）：单页最多采集的条数 / 单条 URL 上限。 */
  const MAX_MEDIA_HINTS = 8;
  const MAX_MEDIA_URL = 2048;
  /**
   * v1.0.9：MSE 标记。B站 直播间走 MSE（`blob:`）播放时页面里拿不到真实 FLV 地址，
   * 这条伪造线索专门告诉插件「这是 B站 直播，但地址被浏览器隐藏了」。
   */
  const LIVE_BLOB_MARKER = "bilibili_live_blob";
  const SCAN_THROTTLE_MS = 500;
  const CHECK_RETRY_MS = 10_000;
  /** 鼠标离开媒体区域后，按钮保持可见的时长。 */
  const HOVER_IDLE_MS = 3_000;
  /** 媒体区域外扩的判定边距（px），避免贴边时闪烁。 */
  const HOVER_MARGIN = 24;
  /** 页面右侧唤醒区宽度：鼠标贴近右边缘即可唤出按钮。 */
  const RIGHT_ZONE = 160;
  /** 按钮默认放在视频外框上方 36px 处（不遮挡画面）。 */
  const OUTSIDE_OFFSET = 36;
  /** 上方没空间时贴到视频右侧的间距。 */
  const SIDE_GAP = 10;
  /** 贴右侧所需的最小可用宽度（按钮本身 + 间距 + 余量）。 */
  const SIDE_MIN_ROOM = 16;
  const STATE_KEYS = [
    "__INITIAL_STATE__",
    "__NEXT_DATA__",
    "__NUXT__",
    "__APOLLO_STATE__",
    "__PRELOADED_STATE__",
  ];

  let mediaEl = null;
  let buttonHost = null;
  let buttonEl = null;
  let matched = [];
  let checkedUrl = "";
  let lastCheckAt = 0;
  let scanTimer = 0;
  let lastScanAt = 0;
  let positionFrame = 0;
  let hoverFrame = 0;
  let hideTimer = 0;
  let probePending = false;
  /**
   * v1.0.9：点击锁。
   *
   * 悬浮窗点击后到宿主回包之间禁止再次点击；回包成功后继续锁一小段"冷却期"，
   * 防止用户狂点在同一页面建出多条（尤其是不会自然结束的直播录制）任务。
   * 页面路由变化会立刻解锁。
   */
  let isDownloading = false;
  let cooldownTimer = 0;
  /** 普通点播的冷却（够短，不碍重复下载）；直播录制冷却（一单可能长时间在跑）。 */
  const PROBE_COOLDOWN_MS = 3_000;
  const LIVE_COOLDOWN_MS = 30_000;
  let lastHref = location.href;
  const pointer = { x: 0, y: 0, ready: false };

  const encoder = new TextEncoder();

  function byteLength(text) {
    try {
      return encoder.encode(text).length;
    } catch {
      return Number.MAX_SAFE_INTEGER;
    }
  }

  /** 体积硬上限：超限返回 null（绝不截断半截 JSON / HTML）。 */
  function boundedText(text, maxBytes) {
    if (typeof text !== "string") return null;
    // 字符数已经超过上限时，UTF-8 字节数只会更大。
    if (text.length > maxBytes) return null;
    return byteLength(text) <= maxBytes ? text : null;
  }

  function readInitialState() {
    for (const key of STATE_KEYS) {
      try {
        const value = window[key];
        if (value === undefined || value === null) continue;
        const text = JSON.stringify(value);
        const bounded = boundedText(text, MAX_STATE_BYTES);
        if (bounded !== null) return bounded;
      } catch {
        // getter 抛错 / 循环引用：尝试下一个键。
      }
    }
    return null;
  }

  function collect() {
    let html = null;
    try {
      const raw = document.documentElement ? document.documentElement.outerHTML : "";
      html = boundedText(raw, MAX_HTML_BYTES);
    } catch {
      html = null;
    }
    return {
      url: location.href,
      title: String(document.title || "").slice(0, MAX_TITLE_CHARS),
      initialState: readInitialState(),
      html,
      mediaHints: collectMediaHints(),
    };
  }

  /**
   * 原生媒体直链兜底（v1.0.8）：`<video>` / `<audio>` 直接加载的流不会经过
   * fetch / XHR，`inject-network.js` 抓不到，这里补一条线索。
   *
   * v1.0.9 直播增强：
   *   - `blob:`（MSE）不再静默跳过；若页面是 live.bilibili.com，额外塞一条伪造标记
   *     `{url:"bilibili_live_blob", kind:"flv"}`，让插件明确知道「地址被 MSE 隐藏」；
   *   - B站 直播 CDN（bilivideo.com/cn、live-bvc）一律记为 flv，无视后缀。
   */
  function collectMediaHints() {
    const out = [];
    let nodes = [];
    try {
      nodes = document.querySelectorAll("video, audio");
    } catch {
      return out;
    }
    const biliLive = /(^|\.)live\.bilibili\.com$/i.test(String(location.hostname || ""));
    let sawBlob = false;
    const seen = new Set();
    for (const el of nodes) {
      let url = "";
      try {
        url = String(el.currentSrc || el.src || "");
      } catch {
        url = "";
      }
      // MSE：真实流藏在 MediaSource 背后，只留一个"用了 blob"的事实。
      if (/^blob:/i.test(url)) {
        sawBlob = true;
        continue;
      }
      if (!/^https?:\/\//i.test(url)) continue;
      if (url.length > MAX_MEDIA_URL || seen.has(url)) continue;
      seen.add(url);
      let kind = "mp4";
      if (/\.m3u8(?:[?#]|$)/i.test(url)) {
        kind = "m3u8";
      } else if (
        /\.flv(?:[?#]|$)/i.test(url) ||
        /bilivideo\.(?:com|cn)/i.test(url) ||
        /live-bvc/i.test(url)
      ) {
        kind = "flv";
      }
      out.push({ url, kind });
      if (out.length >= MAX_MEDIA_HINTS) break;
    }
    if (biliLive && sawBlob) {
      out.unshift({ url: LIVE_BLOB_MARKER, kind: "flv" });
      if (out.length > MAX_MEDIA_HINTS) out.length = MAX_MEDIA_HINTS;
    }
    return out;
  }

  // -------------------------------------------------------------------------
  // 媒体元素探测（video → audio → object）
  // -------------------------------------------------------------------------

  function visibleSize(el) {
    const rect = el.getBoundingClientRect();
    return rect && rect.width >= 240 && rect.height >= 135 ? rect : null;
  }

  /**
   * 优先返回有实际播放尺寸的 video；没有则退回第一个 video，
   * 再退 audio / object。闭包内只用一次 DOM 查询，避免每层重复扫描。
   */
  function findMedia() {
    let firstVideo = null;
    for (const video of document.querySelectorAll("video")) {
      if (!firstVideo) firstVideo = video;
      if (visibleSize(video)) return video;
    }
    if (firstVideo) return firstVideo;
    return document.querySelector("audio") || document.querySelector("object");
  }

  /** 媒体在视口内的矩形；不可见 / 尚未布局时返回 null。 */
  function mediaRect() {
    if (!mediaEl || !mediaEl.isConnected) return null;
    const rect = mediaEl.getBoundingClientRect();
    if (!rect) return null;
    if (!Number.isFinite(rect.left) || !Number.isFinite(rect.top)) return null;
    if (rect.width <= 0 || rect.height <= 0) return null;
    return rect;
  }

  // -------------------------------------------------------------------------
  // 定位：文档坐标绝对定位 + fixed 兜底
  // -------------------------------------------------------------------------

  /** 坐标不可信（元素被隐藏 / 容器高度为 0）时，强制 fixed 到视口右侧中部。 */
  function applyFallbackPosition() {
    if (!buttonHost) return;
    buttonHost.style.position = "fixed";
    buttonHost.style.left = "auto";
    buttonHost.style.right = "20px";
    buttonHost.style.top = "50%";
    buttonHost.style.transform = "translateY(-50%)";
  }

  /** 按钮实际宽度：首次定位时取实测值，未布局则按 80px 估算。 */
  function buttonWidth() {
    if (!buttonHost) return 80;
    const width = buttonHost.getBoundingClientRect().width;
    return width > 0 ? Math.ceil(width) : 80;
  }

  function positionButton() {
    if (!buttonHost) return;
    const rect = mediaRect();
    if (!rect) {
      applyFallbackPosition();
      return;
    }
    const width = buttonWidth();
    const docTop = rect.top + window.scrollY;
    const docLeft = rect.left + window.scrollX;
    // 优先不遮挡画面：上方留白 ≥36px 就放外框正上方。
    const canPlaceAbove = rect.top >= OUTSIDE_OFFSET;
    // 上方不够（全屏等）时看右侧是否有放按钮的空间。
    const canPlaceRight =
      window.innerWidth - rect.right >= width + SIDE_GAP + SIDE_MIN_ROOM;

    let buttonTop;
    let buttonLeft;
    if (canPlaceAbove) {
      buttonTop = docTop - OUTSIDE_OFFSET;
      buttonLeft = docLeft + rect.width - width;
    } else if (canPlaceRight) {
      buttonTop = docTop + 10;
      buttonLeft = docLeft + rect.width + SIDE_GAP;
    } else {
      // 上方与右侧都没有空间：退化为视频内部右上角。
      buttonTop = docTop + 10;
      buttonLeft = docLeft + rect.width - width;
    }

    buttonHost.style.position = "absolute";
    buttonHost.style.left = `${Math.round(buttonLeft)}px`;
    buttonHost.style.top = `${Math.round(buttonTop)}px`;
    buttonHost.style.right = "auto";
    buttonHost.style.transform = "none";
  }

  /** scroll / resize / DOM 变动共用：一帧内最多重算一次。 */
  function schedulePosition() {
    if (positionFrame) return;
    positionFrame = requestAnimationFrame(() => {
      positionFrame = 0;
      positionButton();
    });
  }

  // -------------------------------------------------------------------------
  // 悬停显隐：纯 opacity 过渡，mousemove 只做 rAF 合帧判定
  // -------------------------------------------------------------------------

  function clearHideTimer() {
    if (hideTimer) {
      clearTimeout(hideTimer);
      hideTimer = 0;
    }
  }

  function setVisible(visible) {
    if (!buttonHost) return;
    buttonHost.style.opacity = visible ? "1" : "0";
    buttonHost.style.pointerEvents = visible ? "auto" : "none";
  }

  function scheduleHide() {
    clearHideTimer();
    hideTimer = setTimeout(() => {
      hideTimer = 0;
      setVisible(false);
    }, HOVER_IDLE_MS);
  }

  function updateHover() {
    hoverFrame = 0;
    if (!buttonHost || !pointer.ready) return;
    const { x, y } = pointer;
    const rect = mediaRect();
    const hostRect = buttonHost.getBoundingClientRect();
    const nearMedia =
      rect !== null &&
      x >= rect.left - HOVER_MARGIN &&
      x <= rect.right + HOVER_MARGIN &&
      y >= rect.top - HOVER_MARGIN &&
      y <= rect.bottom + HOVER_MARGIN;
    const onButton =
      hostRect.width > 0 &&
      x >= hostRect.left &&
      x <= hostRect.right &&
      y >= hostRect.top &&
      y <= hostRect.bottom;
    const rightSide = x >= Math.max(0, window.innerWidth - RIGHT_ZONE);

    if (onButton) {
      // 指针已经落在按钮上：保持可见，且不启动自动隐藏。
      clearHideTimer();
      setVisible(true);
      return;
    }
    if (nearMedia || rightSide) {
      setVisible(true);
      scheduleHide();
      return;
    }
    clearHideTimer();
    setVisible(false);
  }

  function onPointerMove(event) {
    if (!buttonHost) return;
    pointer.x = event.clientX;
    pointer.y = event.clientY;
    pointer.ready = true;
    if (hoverFrame) return;
    hoverFrame = requestAnimationFrame(updateHover);
  }

  // -------------------------------------------------------------------------
  // 媒体就绪：play / loadedmetadata 立即定位，不等 500ms 扫描节流
  // -------------------------------------------------------------------------
  let mediaEventsEl = null;

  function onMediaReady() {
    if (!buttonHost) {
      // 按钮还没注入（例如刚换视频）：立刻补扫一次。
      scheduleScan();
      return;
    }
    positionButton();
    setVisible(true);
    scheduleHide();
  }

  function unbindMediaEvents() {
    if (!mediaEventsEl) return;
    mediaEventsEl.removeEventListener("loadedmetadata", onMediaReady);
    mediaEventsEl.removeEventListener("play", onMediaReady);
    mediaEventsEl = null;
  }

  function bindMediaEvents(media) {
    if (mediaEventsEl === media) return;
    unbindMediaEvents();
    mediaEventsEl = media;
    media.addEventListener("loadedmetadata", onMediaReady);
    media.addEventListener("play", onMediaReady);
  }

  // -------------------------------------------------------------------------
  // 按钮注入 / 移除
  // -------------------------------------------------------------------------

  function mountTarget() {
    return document.body || document.documentElement;
  }

  function setButtonLabel(text) {
    if (buttonEl) buttonEl.textContent = text;
  }

  function removeButton() {
    clearHideTimer();
    unbindMediaEvents();
    if (buttonHost && buttonHost.parentNode) {
      buttonHost.parentNode.removeChild(buttonHost);
    }
    buttonHost = null;
    buttonEl = null;
    mediaEl = null;
  }

  function onProbeClick() {
    // v1.0.9：isDownloading 覆盖"请求中 + 冷却期"，狂点不会重复建任务。
    if (probePending || isDownloading) return;
    // v1.0.9：画面还没出来就发探针，既抓不到流又白耗一次网络，直接劝退。
    if (!mediaLoaded()) {
      showToast("媒体流尚未加载完成，请等画面出现后再试");
      return;
    }
    probePending = true;
    isDownloading = true;
    setButtonLabel("解析中…");
    const packet = collect();
    window.postMessage(
      {
        source: MAIN,
        type: "probe",
        url: packet.url,
        title: packet.title,
        initialState: packet.initialState,
        html: packet.html,
        mediaHints: packet.mediaHints,
      },
      "*",
    );
    // 兜底复位：宿主无响应时 8s 后恢复按钮（单次定时器，不是轮询）。
    setTimeout(() => {
      if (!probePending) return;
      probePending = false;
      isDownloading = false;
      setButtonLabel("重试解析");
    }, 8000);
  }

  /**
   * v1.0.9：媒体是否已经加载出画面。
   *
   * 页面上根本没有媒体元素时返回 true（交给站点插件按 HTML 解析，不能一刀切拦掉）；
   * 有 `<video>` / `<audio>` 但还停在 `readyState 0`（HAVE_NOTHING）—— 播放器还在
   * 初始化、分片还没到手 —— 返回 false，此时发探针只会抓到空气。
   */
  function mediaLoaded() {
    const el = findMedia();
    if (!el) return true;
    if (el.tagName !== "VIDEO" && el.tagName !== "AUDIO") return true;
    if (el.readyState >= 2) return true;
    let src = "";
    try {
      src = String(el.currentSrc || el.src || "");
    } catch {
      src = "";
    }
    // 连地址都没有 = 还没开始加载；有地址但 readyState < 1 = 元数据未就绪。
    return Boolean(src) && el.readyState >= 1;
  }

  /** 进入冷却期：期间按钮不可再触发，到期自动恢复。 */
  function startCooldown(ms, label) {
    if (cooldownTimer) clearTimeout(cooldownTimer);
    isDownloading = true;
    setButtonLabel(label);
    cooldownTimer = setTimeout(() => {
      cooldownTimer = 0;
      isDownloading = false;
      setButtonLabel("解析下载");
    }, ms);
  }

  /** 立即解锁（页面路由变化 / 解析失败时用）。 */
  function releaseLock() {
    if (cooldownTimer) {
      clearTimeout(cooldownTimer);
      cooldownTimer = 0;
    }
    probePending = false;
    isDownloading = false;
    setButtonLabel("解析下载");
  }

  /** 单例注入：挂到 body（脱离播放器内部），媒体切换时先移除旧的再挂新的。 */
  function ensureButton(media) {
    // SPA 可能整体替换 body：宿主节点被摘掉后必须重建，而不是继续给幽灵节点定位。
    if (buttonHost && !buttonHost.isConnected) removeButton();
    if (buttonHost && mediaEl === media) {
      bindMediaEvents(media);
      positionButton();
      return;
    }
    removeButton();
    bindMediaEvents(media);

    const host = document.createElement("div");
    host.id = HOST_ID;
    host.style.cssText = [
      "position:absolute",
      "left:0",
      "top:0",
      "z-index:2147483647",
      "width:max-content",
      "opacity:0",
      "pointer-events:none",
      "transition:opacity .18s ease",
    ].join(";");
    // closed Shadow DOM：页面样式（B 站全局 button 重置等）无法污染按钮。
    const shadow = host.attachShadow({ mode: "closed" });
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "解析下载";
    button.style.cssText = [
      "all:initial",
      "display:inline-block",
      "padding:6px 10px",
      "border:0",
      "border-radius:6px",
      "background:linear-gradient(135deg,#00b4d8,#0077b6)",
      "color:#fff",
      "font:600 12px/1.2 system-ui,-apple-system,'Segoe UI',sans-serif",
      "cursor:pointer",
      "box-shadow:0 4px 12px rgba(0,0,0,.35)",
      "white-space:nowrap",
      "pointer-events:auto",
    ].join(";");
    button.addEventListener("click", (event) => {
      event.preventDefault();
      event.stopPropagation();
      onProbeClick();
    });
    shadow.appendChild(button);

    const target = mountTarget();
    if (!target) return;
    target.appendChild(host);

    buttonHost = host;
    buttonEl = button;
    mediaEl = media;
    positionButton();
  }

  // -------------------------------------------------------------------------
  // 极简页内 Toast（beta12.1）：取代原生 chrome.notifications / alert
  // -------------------------------------------------------------------------
  let toastHost = null;
  let toastFade = 0;
  let toastRemove = 0;

  function removeToast() {
    if (toastFade) {
      clearTimeout(toastFade);
      toastFade = 0;
    }
    if (toastRemove) {
      clearTimeout(toastRemove);
      toastRemove = 0;
    }
    if (toastHost && toastHost.parentNode) {
      toastHost.parentNode.removeChild(toastHost);
    }
    toastHost = null;
  }

  /** 页面右下角注入一条极简提示，3 秒后自动淡出。 */
  function showToast(text) {
    if (!text) return;
    removeToast();
    const host = document.createElement("div");
    host.style.cssText =
      "position:fixed;right:20px;bottom:24px;z-index:2147483647;pointer-events:none;";
    const shadow = host.attachShadow({ mode: "closed" });
    const box = document.createElement("div");
    box.textContent = text;
    box.style.cssText = [
      "all:initial",
      "display:inline-block",
      "max-width:320px",
      "padding:9px 14px",
      "border-radius:10px",
      "background:rgba(15,23,26,.92)",
      "color:#e4efed",
      "font:500 13px/1.4 system-ui,-apple-system,'Segoe UI',sans-serif",
      "box-shadow:0 8px 22px rgba(0,0,0,.35)",
      "opacity:0",
      "transform:translateY(6px)",
      "transition:opacity .2s ease,transform .2s ease",
    ].join(";");
    shadow.appendChild(box);
    document.documentElement.appendChild(host);
    toastHost = host;
    requestAnimationFrame(() => {
      box.style.opacity = "1";
      box.style.transform = "translateY(0)";
    });
    toastFade = setTimeout(() => {
      box.style.opacity = "0";
      box.style.transform = "translateY(6px)";
      toastRemove = setTimeout(removeToast, 220);
    }, 3000);
  }

  // -------------------------------------------------------------------------
  // /check 预检与扫描
  // -------------------------------------------------------------------------

  function requestCheck(href) {
    if (checkedUrl === href) return;
    const now = Date.now();
    if (now - lastCheckAt < CHECK_RETRY_MS) return;
    lastCheckAt = now;
    checkedUrl = href;
    window.postMessage({ source: MAIN, type: "check", url: href }, "*");
    // 桥接脚本可能与本脚本同一时刻注入：3s 无应答就解除标记，等下一次扫描重问。
    setTimeout(() => {
      if (checkedUrl === href && matched.length === 0) checkedUrl = "";
    }, 3000);
  }

  function runScan() {
    const href = location.href;
    if (href !== lastHref) {
      lastHref = href;
      matched = [];
      checkedUrl = "";
      removeButton();
    }
    const media = findMedia();
    if (!media) {
      removeButton();
      return;
    }
    if (matched.length === 0) {
      // 只有页面上真的存在媒体元素时才向宿主预检，普通网页零请求。
      requestCheck(href);
      return;
    }
    ensureButton(media);
  }

  function runScanNow() {
    if (scanTimer) {
      clearTimeout(scanTimer);
      scanTimer = 0;
    }
    lastScanAt = Date.now();
    runScan();
    schedulePosition();
  }

  /**
   * 500ms 节流：首个变动立即扫描，之后每 500ms 最多一次。
   *
   * 刻意不用纯防抖 —— B 站弹幕等高频 DOM 变动会不断重置计时器，纯防抖会让
   * 扫描永远等不到执行；节流既守住性能上限，也保证按钮最终能够挂载。
   */
  function scheduleScan() {
    const elapsed = Date.now() - lastScanAt;
    if (elapsed >= SCAN_THROTTLE_MS) {
      runScanNow();
      return;
    }
    if (scanTimer) return;
    scanTimer = setTimeout(runScanNow, SCAN_THROTTLE_MS - elapsed);
  }

  function onRouteChange() {
    matched = [];
    checkedUrl = "";
    // SPA 换视频必须立刻重新预检：清掉 10s 重试间隔，否则按钮会晚十几秒才出现。
    lastCheckAt = 0;
    lastHref = location.href;
    // 新页面 = 新任务，解除点击锁与冷却。
    releaseLock();
    removeButton();
    // 不排队等 500ms 节流，路由变化当场重扫一次。
    runScanNow();
  }

  // -------------------------------------------------------------------------
  // 桥接消息
  // -------------------------------------------------------------------------

  window.addEventListener("message", (event) => {
    if (event.source !== window) return;
    const data = event.data;
    if (!data || data.source !== BRIDGE) return;

    if (data.type === "collect") {
      window.postMessage(
        {
          source: MAIN,
          type: "collect-result",
          requestId: data.requestId,
          data: collect(),
        },
        "*",
      );
      return;
    }

    if (data.type === "check-result") {
      if (data.url && data.url !== location.href) return;
      matched = Array.isArray(data.matched) ? data.matched : [];
      if (matched.length === 0) {
        // 允许 10s 后重试（插件可能刚安装 / 刚启用），由节流兜底。
        checkedUrl = "";
      }
      runScan();
      return;
    }

    if (data.type === "probe-result") {
      probePending = false;
      // beta12.1：结果只用页内 Toast（3s 自动淡出），不再改按钮文案 / 弹系统通知。
      if (data.ok) {
        if (data.live) {
          // v1.0.9：直播直提 —— 后台已把真实流地址投给宿主，等用户在桌面端确认。
          showToast("已提交直播录制，请在桌面端确认");
          startCooldown(LIVE_COOLDOWN_MS, "录制中…");
          return;
        }
        showToast(matched.length > 0 ? `已唤醒 ${matched.length} 个插件` : "已提交下载任务");
        startCooldown(PROBE_COOLDOWN_MS, "解析下载");
        return;
      }
      showToast(data.error ? `解析失败：${data.error}` : "解析失败，请稍后重试");
      releaseLock();
      return;
    }
  });

  // -------------------------------------------------------------------------
  // DOM 监听：document_start 注入，body 未就绪时先盯 documentElement
  // -------------------------------------------------------------------------

  const observer = new MutationObserver(scheduleScan);

  function observeBody() {
    const target = document.body || document.documentElement;
    if (!target) return;
    observer.disconnect();
    observer.observe(target, { childList: true, subtree: true });
  }

  observeBody();
  document.addEventListener(
    "DOMContentLoaded",
    () => {
      observeBody();
      scheduleScan();
    },
    { once: true },
  );

  for (const type of ["play", "loadedmetadata", "durationchange", "emptied"]) {
    document.addEventListener(type, scheduleScan, true);
  }
  window.addEventListener("resize", schedulePosition, { passive: true });
  window.addEventListener("scroll", schedulePosition, { passive: true, capture: true });
  // 悬停唤醒：整页 mousemove，rAF 合帧后只做矩形比较。
  document.addEventListener("mousemove", onPointerMove, { passive: true, capture: true });
  document.addEventListener("fullscreenchange", scheduleScan, true);
  document.addEventListener("webkitfullscreenchange", scheduleScan, true);
  window.addEventListener("popstate", onRouteChange);

  // SPA 内部路由不会触发 popstate，轻量代理 history 以重置按钮。
  for (const method of ["pushState", "replaceState"]) {
    const original = history[method];
    if (typeof original !== "function") continue;
    history[method] = function patchedHistory(...args) {
      const result = original.apply(this, args);
      window.dispatchEvent(new Event("harness-location-change"));
      return result;
    };
  }
  window.addEventListener("harness-location-change", onRouteChange);

  // document_start 注入：body 可能还没创建，先扫一次，DOMContentLoaded 再补一次。
  scheduleScan();
})();
