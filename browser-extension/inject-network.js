// Aria2-Harness 全息探针 · 网络嗅探（扩展 v1.0.7 · 模块三）
//
// 在 MAIN world / document_start 注入，劫持 window.fetch 与 XMLHttpRequest：
//   - 请求发起时先按 URL 判定（.m3u8 / .m4s / .mpd / .mp4）；
//   - 响应到达后补看 Content-Type（application/vnd.apple.mpegurl、video/mp4 等，
//     覆盖「无后缀 URL + 媒体响应头」的接口）。
// 命中的地址去重后分批 postMessage 给 holo-bridge.js，最终转给后台。
//
// 红线：
//   - 只读旁路：绝不修改请求参数、绝不读取 / 消费响应体（避免破坏页面播放）；
//   - 不阻塞主线程：判定只是字符串比较，消息 500ms 合批；
//   - 单页最多记 256 条去重地址、每批最多 48 条，超限丢弃最旧。

(() => {
  "use strict";

  const NET = "harness-holo-net";
  const FLUSH_MS = 500;
  const MAX_HINTS = 48;
  const MAX_URL_CHARS = 2048;
  const SEEN_LIMIT = 256;

  /** 已上报的 URL（Set 保持插入顺序，用于 FIFO 淘汰）。 */
  const seen = new Set();
  /** 待合批上报的命中（每 500ms 清空一次）。 */
  const pending = [];
  let flushTimer = 0;

  /**
   * 判定媒体类型；命中返回 hls / m4s / dash / mp4 / flv，否则空串。
   *
   * v1.0.9 直播增强：B站 直播的真实 FLV 常常藏在**无后缀**的深层请求里
   * （`*.bilivideo.com/live-bvc/...`、`mcdn.bilivideo.cn/...`），响应头也可能
   * 被跨域屏蔽读不到，所以这里对 B站 直播 CDN 与 `live-bvc` 关键字做宽松判定，
   * **无视 Content-Type** 直接记为 flv。顺序要紧：显式后缀（m3u8/m4s/mpd）优先，
   * 避免把普通投稿页的 DASH 分片误判成直播。
   */
  function classify(url, contentType) {
    const href = String(url || "");
    const mime = String(contentType || "").toLowerCase();
    if (!href && !mime) return "";
    if (/\.m3u8(?:[?#]|$)/i.test(href) || mime.includes("mpegurl")) return "hls";
    if (/\.m4s(?:[?#]|$)/i.test(href)) return "m4s";
    if (/\.mpd(?:[?#]|$)/i.test(href) || mime.includes("dash+xml")) return "dash";
    if (/\.flv(?:[?#]|$)/i.test(href) || mime.includes("x-flv")) return "flv";
    if (/bilivideo\.(?:com|cn)/i.test(href) || /live-bvc/i.test(href)) return "flv";
    if (/\.mp4(?:[?#]|$)/i.test(href) || mime.includes("video/mp4")) return "mp4";
    return "";
  }

  function flush() {
    flushTimer = 0;
    if (pending.length === 0) return;
    const hints = pending.splice(0, pending.length);
    try {
      window.postMessage({ source: NET, type: "net-hints", hints }, "*");
    } catch {
      // 消息通道异常：丢弃本批，下一批继续。
    }
  }

  /** 去重 + 合批；错误 / 非媒体请求在这里被直接丢弃。 */
  function remember(url, contentType) {
    const kind = classify(url, contentType);
    if (!kind) return;
    const key = String(url || "").slice(0, MAX_URL_CHARS);
    if (!key || seen.has(key)) return;
    if (seen.size >= SEEN_LIMIT) {
      seen.delete(seen.values().next().value);
    }
    seen.add(key);
    if (pending.length >= MAX_HINTS) return;
    pending.push({ url: key, kind });
    if (flushTimer) return;
    flushTimer = setTimeout(flush, FLUSH_MS);
  }

  function installFetch() {
    const nativeFetch = window.fetch;
    if (typeof nativeFetch !== "function") return;
    window.fetch = function harnessFetch(input, init) {
      let url = "";
      try {
        url = typeof input === "string" ? input : input && input.url ? input.url : "";
      } catch {
        url = "";
      }
      // URL 可直接判定：先记一条，不等响应。
      if (url) remember(url, "");
      const result = nativeFetch.apply(this, arguments);
      try {
        if (result && typeof result.then === "function") {
          result.then(
            (response) => {
              try {
                const finalUrl = (response && response.url) || url;
                const contentType =
                  response && response.headers ? response.headers.get("content-type") : "";
                remember(finalUrl, contentType);
              } catch {
                // opaque 响应读不到头：URL 判定已经覆盖多数场景。
              }
            },
            () => {
              // 请求失败：不产生任何副作用。
            },
          );
        }
      } catch {
        // 页面自定义 Promise：旁路失败不影响请求本身。
      }
      return result;
    };
  }

  function installXhr() {
    const proto = XMLHttpRequest.prototype;
    const nativeOpen = proto.open;
    const nativeSend = proto.send;
    if (typeof nativeOpen !== "function" || typeof nativeSend !== "function") return;

    proto.open = function harnessOpen(method, url) {
      try {
        this.__harnessUrl = typeof url === "string" ? url : url ? String(url) : "";
      } catch {
        this.__harnessUrl = "";
      }
      return nativeOpen.apply(this, arguments);
    };

    proto.send = function harnessSend() {
      const xhr = this;
      const url = xhr.__harnessUrl || "";
      if (url) remember(url, "");
      try {
        xhr.addEventListener(
          "loadend",
          () => {
            try {
              remember(xhr.responseURL || url, xhr.getResponseHeader("content-type"));
            } catch {
              // 头不可读（跨域 / 已 abort）：忽略。
            }
          },
          { once: true },
        );
      } catch {
        // 老引擎不支持 once：URL 阶段已经记过，直接放弃响应阶段。
      }
      return nativeSend.apply(this, arguments);
    };
  }

  installFetch();
  installXhr();
})();
