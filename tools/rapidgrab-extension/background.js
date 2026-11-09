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

// Streaming extensions: video/audio payloads. A response with one of these
// (by URL or content type) is only a download when the browser navigated to
// it with `Content-Disposition: attachment`; in-page playback fetches never
// count, whatever headers the CDN sends. Facebook's scontent CDN, for
// example, serves `video/mp4` (and sometimes attachment headers) to the
// player via XHR.
const STREAM_EXTENSIONS = new Set([
  "3gp", "aac", "aif", "asf", "avi", "flv", "m4a", "m4v", "mkv", "mov", "mp3",
  "mp4", "mpa", "mpeg", "mpg", "ogg", "ogv", "qt", "ra", "rm", "rmvb", "wav",
  "webm", "wma", "wmv",
]);

// Request types that are page navigations. Playback uses
// `xmlhttprequest`, `media`, `other`, … and can be ignored for streams.
const NAVIGATION_TYPES = new Set(["main_frame", "sub_frame", "object"]);

// URL -> capture time. Chunked/range requests and the header + downloads
// paths must not pop more than one window per URL.
const recentCaptures = new Map();
const CAPTURE_WINDOW_MS = 30000;

// tabId -> { url, time }: the most recent streaming media response seen on
// that tab. MSE players (Facebook, …) play from blob: URLs, so the player
// button cannot read `currentSrc`; this is what it resolves to instead.
// Purely recorded — it never opens a popup by itself.
const lastMediaByTab = new Map();
const MEDIA_WINDOW_MS = 15 * 60 * 1000;

// Hosts whose media must go through the yt-dlp resolver (signed URLs with
// throttling tokens); the button keeps sending the page URL for these.
const RESOLVER_HOSTS = ["googlevideo.com", "youtube.com", "youtu.be"];

// URLs the GUI acknowledged, so a late browser download for them can be
// cancelled even when it appears after the ack.
const ackedUrls = new Map();

// browser downloadId -> captured url, for cancelling after the GUI acks.
const capturedDownloads = new Map();

function pruneMap(map, now, maxAge) {
  for (const [key, value] of map) {
    const time = typeof value === "number" ? value : value.time;
    if (now - time > maxAge) map.delete(key);
  }
}

function wasCapturedRecently(url, now) {
  pruneMap(recentCaptures, now, CAPTURE_WINDOW_MS);
  return recentCaptures.has(url);
}

function noteCapture(url, now) {
  recentCaptures.set(url, now);
}

function isResolverHost(url) {
  try {
    const host = new URL(url).hostname.toLowerCase();
    return RESOLVER_HOSTS.some((known) => host === known || host.endsWith("." + known));
  } catch (_) {
    return false;
  }
}

/// Streams are recorded for the player button, not for automatic capture.
function isStreamResponse(headers, url, resourceType) {
  const type = (headerValue(headers, "content-type") || "").split(";")[0].trim().toLowerCase();
  const ext = extensionOf(url, null);
  return (
    type.startsWith("video/") ||
    type.startsWith("audio/") ||
    resourceType === "media" ||
    STREAM_EXTENSIONS.has(ext)
  );
}

function rememberMedia(tabId, url, now) {
  pruneMap(lastMediaByTab, now, MEDIA_WINDOW_MS);
  lastMediaByTab.set(tabId, { url, time: now });
}

/// The freshest recorded media URL for a tab, unless it belongs to a host
/// the resolver handles better (YouTube).
function recentMediaFor(tabId, now) {
  pruneMap(lastMediaByTab, now, MEDIA_WINDOW_MS);
  const entry = lastMediaByTab.get(tabId);
  if (!entry || now - entry.time > MEDIA_WINDOW_MS) return null;
  if (isResolverHost(entry.url)) return null;
  return entry.url;
}

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
  const isAttachment = /attachment/i.test(disposition);
  const type = (headerValue(headers, "content-type") || "").split(";")[0].trim().toLowerCase();
  const ext = extensionOf(url, parseDispositionFilename(disposition));

  const isStream =
    type.startsWith("video/") ||
    type.startsWith("audio/") ||
    resourceType === "media" ||
    STREAM_EXTENSIONS.has(ext);
  if (isStream) {
    // Only a page navigation that the browser turned into a download
    // (attachment) may pop. Playback fetches never do, even when the CDN
    // labels them attachment or their URL ends in .mp4.
    return isAttachment && NAVIGATION_TYPES.has(resourceType);
  }

  if (isAttachment) return true;
  if (FILE_CONTENT_TYPES.has(type)) return true;
  return !!(ext && FILE_EXTENSIONS.has(ext));
}

async function handleIntercept(details) {
  try {
    if (details.tabId < 0 || details.tabId === api.tabs.TAB_ID_NONE) return;
    if (!/^https?:\/\//i.test(details.url)) return;
    if (details.statusCode && details.statusCode >= 400) return;
    const headers = details.responseHeaders || [];
    // Record the media URL for the player button before deciding whether it
    // pops; playback is never a capture, but it is the URL the button needs
    // on MSE players (Facebook) where `currentSrc` is a blob:.
    if (isStreamResponse(headers, details.url, details.type)) {
      rememberMedia(details.tabId, details.url, Date.now());
    }
    if (!isDownloadable(headers, details.url, details.type)) return;
    const now = Date.now();
    if (wasCapturedRecently(details.url, now)) return;
    noteCapture(details.url, now);

    const disposition = headerValue(headers, "content-disposition") || "";
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

/// The authoritative "the browser is downloading a file" signal: playback
/// never creates a browser download, so this is what replaces guesswork over
/// response headers. The header path above usually fires first (and gets the
/// richer file name); this catches flows the headers miss.
function onDownloadCreated(downloadItem) {
  const url = downloadItem.finalUrl || downloadItem.url || "";
  const when = Date.now();
  pruneMap(capturedDownloads, when, 60000);
  pruneMap(ackedUrls, when, 60000);

  if (!/^https?:\/\//i.test(url)) return;
  capturedDownloads.set(downloadItem.id, { url, time: when });

  if (ackedUrls.has(url)) {
    // The GUI already took this URL over; this browser download is the
    // duplicate that must be removed.
    cancelBrowserDownload(downloadItem.id);
    return;
  }
  if (wasCapturedRecently(url, when)) return;

  noteCapture(url, when);
  let filename = downloadItem.filename
    ? String(downloadItem.filename).split(/[\\/]/).pop()
    : guessFilename(url);
  // Chrome reports interrupted downloads with a `.crdownload` suffix.
  if (filename) filename = filename.replace(/\.crdownload$/i, "");
  sendCapture(url, null, {
    filename: filename || undefined,
    contentType: downloadItem.mime || undefined,
    source: "intercept",
  }).catch(() => {});
}

function cancelBrowserDownload(downloadId) {
  if (!api.downloads) return;
  api.downloads.pause(downloadId).catch(() => {});
  // Give the pause a tick so the cancel wins (Chrome quirk).
  setTimeout(() => {
    api.downloads.cancel(downloadId).catch(() => {});
    // Drop the cancelled entry (and its partial file) from browser history.
    setTimeout(() => {
      if (typeof api.downloads.erase === "function") {
        api.downloads.erase({ id: downloadId }).catch(() => {});
      }
    }, 100);
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

/// Generic titles that name nothing (site shells, feeds). A player title
/// worth using as a filename must say more than this.
const GENERIC_TITLES = new Set([
  "facebook", "watch", "home", "feed", "youtube", "video", "videos", "reels",
]);

/// True when a title is specific enough to be a useful file name.
function isMeaningfulTitle(title) {
  if (!title) return false;
  const name = String(title).trim().toLowerCase();
  if (!name || GENERIC_TITLES.has(name)) return false;
  // The site shell's own document title ("Facebook" alone, favicon suffixes
  // aside) is not a video title.
  if (/^(facebook|youtube|watch)(?:\s*[-–—|].*)?$/i.test(name)) return false;
  return true;
}

/// Turn a YouTube tab title ("My Video - YouTube") into a safe
/// "My Video.mp4" filename. Returns null when there is no usable title.
function filenameFromTitle(title) {
  if (!title || !isMeaningfulTitle(title)) return null;
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
      const now = Date.now();
      ackedUrls.set(payload.url, now);
      pruneMap(ackedUrls, now, 60000);
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
    const now = Date.now();
    const tabId = sender.tab ? sender.tab.id : undefined;
    // MSE players (Facebook, …) have no downloadable `currentSrc`; the
    // content script sends an empty URL and we use the most recent media
    // response seen on this tab. Falls back to the page URL (the resolver
    // handles it) when nothing recent was recorded or the host needs yt-dlp.
    let url = msg.url || "";
    let sniffed = false;
    if (!/^https?:\/\//i.test(url)) {
      const recent = tabId !== undefined ? recentMediaFor(tabId, now) : null;
      if (recent) {
        url = recent;
        sniffed = true;
      } else {
        url = (sender.tab && sender.tab.url) || "";
      }
    }
    if (/^https?:\/\//i.test(url)) {
      const tab = sender.tab
        ? { url: sender.tab.url, title: msg.title }
        : null;
      // Prefer a human title (post caption, player label) for both paths.
      // A sniffed CDN URL only borrows it when it is specific; generic site
      // titles ("Facebook", "Watch") fall back to the CDN basename.
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
