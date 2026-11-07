// RapidGrab extension: confirm-first browser capture.
// Click the toolbar button (or the context menu) to send the page's first
// HTTP(S) link found by the heuristics below to RapidGrab via the native host.
// The GUI pre-fills the form; nothing auto-starts.

const HOST = "com.rapidgrab";

// Firefox exposes the same APIs under `browser`; Chrome uses `chrome`.
const api = globalThis.browser || globalThis.chrome;

// Extensions treated as files (own list; IDM's registry list covers the same
// ground — archives, media, documents, installers).
const FILE_EXTENSIONS = new Set([
  "3gp", "7z", "aac", "ace", "aif", "apk", "arj", "asf", "avi", "bin", "bz2",
  "exe", "gz", "gzip", "img", "iso", "lzh", "m4a", "m4v", "mkv", "mov", "mp3",
  "mp4", "mpa", "mpeg", "mpg", "msi", "msu", "ogg", "ogv", "pdf", "ppt", "pptx",
  "qt", "ra", "rar", "rm", "rmvb", "sit", "sitx", "tar", "tif", "tiff", "wav",
  "webm", "wma", "wmv", "xls", "xlsx", "zip",
]);

const FILE_CONTENT_TYPES = new Set([
  "application/pdf",
  "application/zip",
  "application/x-rar-compressed",
  "application/x-7z-compressed",
  "application/x-tar",
  "application/gzip",
  "application/octet-stream",
  "application/vnd.ms-excel",
  "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
  "application/msword",
  "application/vnd.ms-powerpoint",
  "application/vnd.openxmlformats-officedocument.presentationml.presentation",
  "application/x-msdownload",
  "application/vnd.microsoft.portable-executable",
]);

// requestId -> { url, time } for deduping the same response.
const seenRequests = new Map();

// tabId -> last intercept capture time (ms). Media players fire a
// `video/*` response per segment; without a per-tab cooldown every
// segment would spawn a native host. One intercept per tab per 5 s.
const lastInterceptByTab = new Map();

// browser downloadId -> captured url, for cancelling after the GUI acks.
const capturedDownloads = new Map();

function headerValue(headers, name) {
  const lower = name.toLowerCase();
  for (const h of headers || []) {
    if ((h.name || "").toLowerCase() === lower) return h.value || "";
  }
  return null;
}

function parseDispositionFilename(disposition) {
  if (!disposition) return null;
  // RFC 5987: filename*=UTF-8''%E2%82%AC.txt
  const star = disposition.match(/filename\*\s*=\s*(?:UTF-8'')?([^;]+)/i);
  if (star) {
    try {
      return decodeURIComponent(star[1].trim().replace(/^"|"$/g, ""));
    } catch (_) {}
  }
  const plain = disposition.match(/filename\s*=\s*([^;]+)/i);
  if (plain) return plain[1].trim().replace(/^"|"$/g, "");
  return null;
}

function extensionOf(url, filename) {
  const name = filename || "";
  const dot = name.lastIndexOf(".");
  if (dot > 0) return name.slice(dot + 1).toLowerCase();
  try {
    const parts = new URL(url).pathname.split("/").filter(Boolean);
    const last = parts[parts.length - 1] || "";
    const d = last.lastIndexOf(".");
    if (d > 0) return last.slice(d + 1).toLowerCase();
  } catch (_) {}
  return "";
}

function isDownloadable(headers, url, resourceType) {
  const disposition = headerValue(headers, "content-disposition") || "";
  if (/attachment/i.test(disposition)) return true;
  const type = (headerValue(headers, "content-type") || "").split(";")[0].trim().toLowerCase();
  if (type.startsWith("video/") || type.startsWith("audio/")) return true;
  if (FILE_CONTENT_TYPES.has(type)) return true;
  const ext = extensionOf(url, parseDispositionFilename(disposition));
  if (ext && FILE_EXTENSIONS.has(ext)) return true;
  // Media subresource (video element streams) with a known file extension.
  if (resourceType === "media" && ext) return true;
  return false;
}

async function handleIntercept(details) {
  try {
    if (details.tabId < 0 || details.tabId === api.tabs.TAB_ID_NONE) return;
    if (!/^https?:\/\//i.test(details.url)) return;
    if (details.statusCode && details.statusCode >= 400) return;
    const now = Date.now();
    for (const [k, v] of seenRequests) {
      if (now - v.time > 30000) seenRequests.delete(k);
    }
    if (seenRequests.has(details.requestId)) return;
    const headers = details.responseHeaders || [];
    if (!isDownloadable(headers, details.url, details.type)) return;
    // Attachment downloads always pass; everything else (media segments,
    // file content-types) is throttled to one per tab per 5 seconds.
    const disposition = headerValue(headers, "content-disposition") || "";
    const isAttachment = /attachment/i.test(disposition);
    if (!isAttachment) {
      const last = lastInterceptByTab.get(details.tabId) || 0;
      if (now - last < 5000) return;
      lastInterceptByTab.set(details.tabId, now);
    }
    seenRequests.set(details.requestId, { url: details.url, time: now });

    const filename = parseDispositionFilename(disposition) || guessFilename(details.url);
    const contentType = headerValue(headers, "content-type");
    const lengthHeader = headerValue(headers, "content-length");
    const contentLength =
      lengthHeader && /^\d+$/.test(lengthHeader.trim())
        ? Number(lengthHeader.trim())
        : undefined;

    const tab = await api.tabs.get(details.tabId).catch(() => null);
    await sendCapture(details.url, tab, {
      filename,
      contentDisposition: disposition,
      contentType: contentType || undefined,
      contentLength,
      source: "intercept",
    });
  } catch (e) {
    console.warn("[RapidGrab] intercept failed:", e);
  }
}

function onDownloadCreated(downloadItem) {
  const url = downloadItem.finalUrl || downloadItem.url;
  const when = Date.now();
  for (const [k, v] of capturedDownloads) {
    if (when - v.time > 60000) capturedDownloads.delete(k);
  }
  capturedDownloads.set(downloadItem.id, { url, time: when });
}

function cancelBrowserDownload(downloadId) {
  if (!api.downloads) return;
  api.downloads.pause(downloadId).catch(() => {});
  // Give the pause a tick so the cancel wins (Chrome quirk).
  setTimeout(() => {
    api.downloads.cancel(downloadId).catch(() => {});
  }, 50);
}

function guessFilename(url) {
  try {
    const u = new URL(url);
    const parts = u.pathname.split("/").filter(Boolean);
    const last = parts[parts.length - 1];
    if (last && last.includes(".")) return decodeURIComponent(last);
  } catch (_) {}
  return null;
}

/// True for YouTube watch/share pages where the URL itself carries no
/// filename — the video filename can only come from the tab title.
function isYouTubePage(url) {
  try {
    const host = new URL(url).hostname.toLowerCase();
    return (
      host === "youtube.com" ||
      host.endsWith(".youtube.com") ||
      host === "youtu.be"
    );
  } catch (_) {
    return false;
  }
}

/// Turn a YouTube tab title ("My Video - YouTube") into a safe
/// "My Video.mp4" filename. Returns null when there is no usable title.
function filenameFromTitle(title) {
  if (!title) return null;
  let name = String(title)
    .replace(/\s*[-–—|]\s*YouTube(?:\s+Music)?\s*$/i, "")
    .replace(/[<>:"/\\|?*\x00-\x1f]/g, "")
    .replace(/\s+/g, " ")
    .replace(/[. ]+$/, "")
    .trim()
    .slice(0, 120);
  if (!name) return null;
  if (!/\.(mp4|webm|mkv|mov|avi|mp3|m4a|wav)$/i.test(name)) {
    name += ".mp4";
  }
  return name;
}

async function sendCapture(url, tab, info) {
  if (!/^https?:\/\//i.test(url)) {
    console.warn("[RapidGrab] not an http(s) URL:", url);
    return;
  }
  info = info || {};
  let filename = info.filename || guessFilename(url);
  // YouTube watch/share pages carry no filename in the URL — name the
  // capture from the tab title instead of falling back to download.bin.
  if (!filename && isYouTubePage(url)) {
    filename = filenameFromTitle(tab && tab.title);
  }
  const payload = {
    url,
    filename: filename || undefined,
    referer: (tab && tab.url) || undefined,
    user_agent: navigator.userAgent,
    content_disposition_filename: info.contentDisposition
      ? parseDispositionFilename(info.contentDisposition) || undefined
      : undefined,
    content_type: info.contentType,
    content_length: info.contentLength,
    source: info.source || "click",
  };

  // Cookies for the target host (best-effort; empty on failure).
  try {
    const cookies = await api.cookies.getAll({ url });
    const cookie = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    if (cookie) payload.cookies = cookie;
  } catch (_) {}
  post(payload);
}

function post(payload) {
  const port = api.runtime.connectNative(HOST);
  // Native messaging: send one length-prefixed JSON message.
  port.postMessage(payload);
  port.onMessage.addListener((msg) => {
    // Host ack: cancel the browser's own duplicate download only when the
    // GUI actually received the capture.
    if (msg && msg.ok) {
      for (const [id, entry] of capturedDownloads) {
        if (entry.url === payload.url) {
          cancelBrowserDownload(id);
          capturedDownloads.delete(id);
          break;
        }
      }
    }
  });
  port.onDisconnect.addListener(() => {
    if (api.runtime.lastError) {
      console.warn("[RapidGrab] native host unavailable:", api.runtime.lastError.message);
    }
  });
  // Chrome closes the port after the message is sent; no response expected
  // beyond the ack above.
}

api.action.onClicked.addListener((tab) => {
  if (!tab || !tab.url) return;
  // Prefer the page URL itself; for link-heavy pages the user can use the
  // context menu on a specific link instead.
  sendCapture(tab.url, tab);
});

api.runtime.onInstalled.addListener(() => {
  api.contextMenus.create({
    id: "rapidgrab-page",
    title: "Download page with RapidGrab",
    contexts: ["page"],
  });
  api.contextMenus.create({
    id: "rapidgrab-link",
    title: "Download link with RapidGrab",
    contexts: ["link"],
  });
  api.contextMenus.create({
    id: "rapidgrab-video",
    title: "Download video with RapidGrab",
    contexts: ["video"],
  });
});

// Media sniffing: the content script reports the media URL it found.
api.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  if (msg && msg.type === "rapidgrab-media") {
    // Defense in depth: blob:/data: URLs are MSE playback URLs and can
    // never be downloaded — substitute the page URL (the resolver handles
    // it) instead of letting sendCapture drop the capture as non-http(s).
    let url = msg.url || "";
    if (!/^https?:\/\//i.test(url)) {
      url = (sender.tab && sender.tab.url) || "";
    }
    if (/^https?:\/\//i.test(url)) {
      const tab = sender.tab
        ? { url: sender.tab.url, title: msg.title }
        : null;
      sendCapture(url, tab, {
        filename: msg.title ? filenameFromTitle(msg.title) : null,
        source: "media",
      });
      sendResponse({ ok: true });
      return true;
    }
  }
  return false;
});

// Toolbar button enabled state: only on http(s) pages.
function refreshActionState(tabId) {
  api.tabs
    .query({ active: true, lastFocusedWindow: true })
    .then((tabs) => {
      const tab = tabs && tabs[0];
      if (!tab || typeof api.action.setEnabled !== "function") return;
      const onHttp = tab.url && /^https?:\/\//i.test(tab.url);
      api.action.setEnabled(tab.id, !!onHttp).catch(() => {});
    })
    .catch(() => {});
}
if (api.tabs && api.tabs.onActivated) api.tabs.onActivated.addListener(refreshActionState);
if (api.tabs && api.tabs.onUpdated) {
  api.tabs.onUpdated.addListener((id, info, tab) => {
    if (info.status === "complete") refreshActionState(id);
  });
}

api.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === "rapidgrab-link" && info.linkUrl) {
    sendCapture(info.linkUrl, tab, { source: "link" });
  } else if (info.menuItemId === "rapidgrab-video" && info.srcUrl) {
    sendCapture(info.srcUrl, tab, { source: "media" });
  } else if (info.menuItemId === "rapidgrab-page" && tab && tab.url) {
    sendCapture(tab.url, tab, { source: "page" });
  }
});

// Interception (IDM-style): watch responses and capture downloadable files
// automatically. The browser's own download is cancelled after the GUI acks.
if (api.webRequest && api.webRequest.onHeadersReceived) {
  api.webRequest.onHeadersReceived.addListener(handleIntercept, {
    urls: ["http://*/*", "https://*/*"],
  });
}
if (api.downloads && api.downloads.onCreated) {
  api.downloads.onCreated.addListener(onDownloadCreated);
}
