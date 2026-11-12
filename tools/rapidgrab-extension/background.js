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

// Request types that are page navigations. Auto-capture is allowed only for
// these — playback and previews use `xmlhttprequest`, `media`, `other`, …
const NAVIGATION_TYPES = new Set(["main_frame", "sub_frame", "object"]);

// URL -> capture time. Chunked/range requests and the header + downloads
// paths must not pop more than one window per URL.
const recentCaptures = new Map();
const CAPTURE_WINDOW_MS = 30000;

// Chrome re-fires `downloads.onCreated` for every item restored from the
// download history when the browser starts. A restored item keeps the start
// time of the original attempt, while a new capture's start time is "now", so
// anything older than this is history and must not open a RapidGrab window.
const RESTORED_DOWNLOAD_GRACE_MS = 2 * 60 * 1000;

// tabId -> [{ url, kind, type, time }] (newest first): the recent streaming
// media responses seen on that tab, classified. MSE players (Facebook, …)
// play from blob: URLs, so the player button cannot read `currentSrc`; this is
// what it resolves to instead. Facebook serves video and audio as separate
// DASH tracks — recording only the latest response used to hand the audio
// track to the downloader. Matching is newest-wins per kind; the media button
// prefers the video track. Purely recorded — it never opens a popup by itself.
const lastMediaByTab = new Map();
const MEDIA_WINDOW_MS = 5 * 60 * 1000;
const MAX_MEDIA_CANDIDATES = 8;

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

function isFacebookHostStr(host) {
  const h = (host || "").toLowerCase();
  return (
    h === "facebook.com" ||
    h.endsWith(".facebook.com") ||
    h === "fbcdn.net" ||
    h.endsWith(".fbcdn.net")
  );
}

/// Host from an absolute http(s) URL, lower-cased, without port/userinfo.
function hostFromUrl(url) {
  const match = /^https?:\/\/([^/?#]+)/i.exec(url || "");
  if (!match) return null;
  let authority = match[1];
  const at = authority.lastIndexOf("@");
  if (at >= 0) authority = authority.slice(at + 1);
  authority = authority.split(":")[0];
  return authority ? authority.toLowerCase() : null;
}

// Facebook's CDN encodes byte-range requests in the URL itself (bytestart /
// byteend) for DASH/fMP4 playback. Downloading such a URL returns only the
// range fragment. Strip them for the Facebook hosts that use these params.
// Done as a string filter (not URLSearchParams) so base64 values such as
// `efg=` keep every byte exactly as packed — signing relies on it.
function stripRangeParams(rawUrl) {
  try {
    const hashIndex = rawUrl.indexOf("#");
    const beforeHash = hashIndex === -1 ? rawUrl : rawUrl.slice(0, hashIndex);
    const hash = hashIndex === -1 ? "" : rawUrl.slice(hashIndex);
    const q = beforeHash.indexOf("?");
    if (q === -1) return rawUrl;
    const base = beforeHash.slice(0, q);
    const host = hostFromUrl(base);
    if (!host || !isFacebookHostStr(host)) return rawUrl;
    const query = beforeHash.slice(q + 1);
    const kept = [];
    let removed = false;
    for (const pair of query.split("&")) {
      if (!pair) continue;
      const eq = pair.indexOf("=");
      const key = (eq === -1 ? pair : pair.slice(0, eq)).trim().toLowerCase();
      if (key === "bytestart" || key === "byteend") {
        removed = true;
        continue;
      }
      kept.push(pair);
    }
    if (!removed) return rawUrl;
    const qs = kept.join("&");
    return base + (qs ? "?" + qs : "") + hash;
  } catch (_) {
    return rawUrl;
  }
}

// Audio MIME hint from a content-type header, e.g. `audio/mp4` -> "audio".
const AUDIO_URL_EXTENSIONS = new Set([
  "aac", "m4a", "mp3", "oga", "ogg", "opus", "wav", "wma",
]);

function isManifestType(type) {
  const t = (type || "").split(";")[0].trim().toLowerCase();
  return (
    t === "application/dash+xml" ||
    t === "application/vnd.apple.mpegurl" ||
    t === "application/x-mpegurl" ||
    t === "application/json" ||
    t.startsWith("text/")
  );
}

// Classify a recorded media response. Video wins over audio when the CDN
// labels its own track; the URL extension is the fallback. Manifests (DASH /
// HLS / text) are never a downloadable track and rank last. The URL extension
// is the fallback.
function mediaKind(contentType, url) {
  const type = (contentType || "").split(";")[0].trim().toLowerCase();
  if (isManifestType(type)) return "manifest";
  if (type.startsWith("video/")) return "video";
  if (type.startsWith("audio/")) return "audio";
  const ext = extensionOf(url, null);
  if (AUDIO_URL_EXTENSIONS.has(ext)) return "audio";
  if (STREAM_EXTENSIONS.has(ext)) return "video";
  return "unknown";
}

function mediaType(contentType) {
  return (contentType || "").split(";")[0].trim().toLowerCase();
}

/// One entry per rendition, not per segment. Facebook segment URLs share the
/// origin+path and differ only in `bytestart`/`byteend`; those were stripped,
/// so before path-dedupe the list could hold 8 signed copies of one video
/// track and evict the audio track entirely.
function trackKey(url) {
  try {
    const u = new URL(url);
    if (isFacebookHostStr(u.hostname)) {
      return u.origin + u.pathname;
    }
  } catch (_) {}
  return url;
}

function pruneMediaByTab(now) {
  for (const [tabId, entries] of lastMediaByTab) {
    const kept = entries
      .filter((entry) => now - entry.time <= MEDIA_WINDOW_MS)
      .slice(0, MAX_MEDIA_CANDIDATES);
    if (kept.length === 0) {
      lastMediaByTab.delete(tabId);
    } else {
      lastMediaByTab.set(tabId, kept);
    }
  }
}

function rememberMedia(tabId, url, contentType, now, dash) {
  pruneMediaByTab(now);
  const kind = mediaKind(contentType, url);
  const key = trackKey(url);
  const existing = lastMediaByTab.get(tabId);
  const kept = Array.isArray(existing)
    ? existing.filter((entry) => trackKey(entry.url) !== key && now - entry.time <= MEDIA_WINDOW_MS)
    : [];
  kept.push({ url, kind, type: mediaType(contentType), time: now, dash: !!dash });
  kept.sort((a, b) => b.time - a.time);
  lastMediaByTab.set(tabId, kept.slice(0, MAX_MEDIA_CANDIDATES));
}

/// The freshest recorded media candidates for a tab, newest first. Excludes
/// hosts the resolver handles better (YouTube).
function recentMediaFor(tabId, now) {
  pruneMediaByTab(now);
  const entries = lastMediaByTab.get(tabId);
  if (!Array.isArray(entries)) return [];
  return entries.filter((entry) => {
    if (now - entry.time > MEDIA_WINDOW_MS) return false;
    return !isResolverHost(entry.url);
  });
}

// Cheap Range probe of a stripped CDN URL to read its true Content-Type,
// total size and — when the body happens to be visible — the MP4 handler
// types enclosed in `hdlr` boxes. Signed URLs usually answer 206 with
// Content-Range; a CDN that ignores Range answers 200 and we use
// Content-Length instead. The first 16 KB are enough for the init segment
// (ftyp + moov) of every single-track DASH rendition Facebook serves.
const SNIFF_BYTES = 16384;

function readAscii(view, offset, length) {
  let out = "";
  for (let i = offset; i < offset + length && i < view.length; i++) {
    out += String.fromCharCode(view[i]);
  }
  return out;
}

// Walk the `hdlr` boxes: a DASH video track carries handler `vide`, an
// audio track `soun`, a muxed (progressive) file both. Returns "video",
// "audio", "mixed" or null when the handler was not inside the window.
function sniffHandlerKind(bytes) {
  if (!bytes || bytes.byteLength < 16) return null;
  const view = new Uint8Array(bytes);
  let video = false;
  let audio = false;
  for (let i = 0; i + 16 <= view.length; i++) {
    if (readAscii(view, i, 4) !== "hdlr") continue;
    // handler_type sits 8 bytes past the box type (version/flags + pre_defined)
    const scanEnd = Math.min(i + 20, view.length - 4);
    for (let j = i + 4; j <= scanEnd; j++) {
      const tag = readAscii(view, j, 4);
      if (tag === "vide") {
        video = true;
        j += 3;
      } else if (tag === "soun") {
        audio = true;
        j += 3;
      }
    }
  }
  if (video && audio) return "mixed";
  if (video) return "video";
  if (audio) return "audio";
  return null;
}

async function probeMedia(url) {
  try {
    const res = await fetch(url, {
      method: "GET",
      credentials: "include",
      cache: "no-store",
      headers: { Range: `bytes=0-${SNIFF_BYTES - 1}` },
    });
    const type = (res.headers.get("content-type") || "").split(";")[0].trim().toLowerCase();
    let total = null;
    const contentRange = res.headers.get("content-range");
    if (contentRange) {
      const slash = contentRange.lastIndexOf("/");
      const size = contentRange.slice(slash + 1).trim();
      if (/^\d+$/.test(size)) total = Number(size);
    } else if (res.status === 200) {
      const length = Number(res.headers.get("content-length") || 0);
      if (Number.isFinite(length) && length > 0) total = length;
    }
    const body = await res.arrayBuffer().catch(() => null);
    const sniffed = body ? sniffHandlerKind(body) : null;
    return { type, total, sniffed };
  } catch (_) {
    return null;
  }
}

/// Probe every (bounded) candidate in parallel. The probe — not the URL —
/// is the source of truth for the type/size: Facebook labels both DASH
/// tracks `video/mp4`, which is why the player-button path used to hand out
/// the audio track. The MP4 handler kind wins when the bytes are readable;
/// the MIME type and a size heuristic are the fallbacks. `total` is the full
/// size when the CDN reports a 206 Content-Range, or the full Content-Length
/// otherwise.
const MAX_PROBED_CANDIDATES = 6;
// A companion whose size exceeds this share of the video total is a second
// video rendition, not the missing audio track.
const AUDIO_MAX_SHARE_OF_VIDEO = 0.6;
const AUDIO_MIN_BYTES = 8 * 1024;

async function probeCandidates(candidates) {
  return Promise.all(
    candidates.slice(0, MAX_PROBED_CANDIDATES).map(async (entry) => {
      const probe = await probeMedia(entry.url);
      if (!probe) {
        return { ...entry, type: entry.type || undefined, total: null };
      }
      const kind =
        probe.sniffed ||
        (probe.type ? mediaKind(probe.type, entry.url) : entry.kind);
      return {
        ...entry,
        kind,
        type: probe.type || entry.type || undefined,
        total: probe.total ?? null,
      };
    }),
  );
}

function kindRank(kind) {
  if (kind === "video" || kind === "mixed") return 2;
  if (kind === "unknown") return 1;
  if (kind === "audio") return 0;
  return -1;
}

/// Pick the video track and, when the player streamed a separate DASH audio
/// rendition, its sibling. Returns { video, audio, videoOnly }.
async function selectMediaTracks(candidates) {
  const probed = await probeCandidates(candidates);
  const usable = probed.filter((entry) => entry.kind !== "manifest");
  if (usable.length === 0) {
    return { video: null, audio: null, videoOnly: false };
  }
  usable.sort(
    (a, b) =>
      kindRank(b.kind) - kindRank(a.kind) ||
      (b.total ?? -1) - (a.total ?? -1) ||
      b.time - a.time,
  );
  const video = usable[0];
  const others = usable.slice(1);

  // 1) Explicit audio classification wins; the largest such track is the
  //    best-quality companion.
  const explicit = others.filter((entry) => entry.kind === "audio");
  let audio = null;
  if (explicit.length) {
    audio = explicit.reduce((best, current) =>
      (current.total ?? 0) > (best.total ?? 0) ? current : best,
    );
  } else {
    // 2) Fallback for bodies we could not sniff: a small, distinct rendition
    //    is the audio track. Only accept it when its size is plausible
    //    against the video total.
    const videoTotal = Number.isFinite(video.total) ? video.total : null;
    const plausible = others.filter((entry) => {
      const t = entry.total;
      if (!Number.isFinite(t) || t <= 0 || t < AUDIO_MIN_BYTES) return false;
      if (videoTotal && t > AUDIO_MAX_SHARE_OF_VIDEO * videoTotal) return false;
      return true;
    });
    if (plausible.length) {
      audio = plausible.reduce((best, current) =>
        (current.total ?? 0) > (best.total ?? 0) ? current : best,
      );
    }
  }
  return { video, audio, videoOnly: !!video.dash && !audio };
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

/// The only responses allowed to open the RapidGrab window automatically:
/// the browser turned the user's navigation into a download. Everything
/// else that is a real download is caught by `downloads.onCreated`, so in-
/// page fetches (hover previews, DASH segments, thumbnails, JSON APIs)
/// stay silent instead of popping one window per request.
function isNavigation(resourceType) {
  // Firefox sometimes reports no type at all for its own downloads.
  return NAVIGATION_TYPES.has(resourceType) || resourceType === undefined || resourceType === null;
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
    // A direct video URL in a tab plays inline; only an attachment header
    // turns a tree of navigations into a download.
    return isAttachment;
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
    const captureUrl = stripRangeParams(details.url);
    const dash = captureUrl !== details.url;
    const contentType = headerValue(headers, "content-type");
    if (isStreamResponse(headers, captureUrl, details.type)) {
      rememberMedia(details.tabId, captureUrl, contentType, Date.now(), dash);
    }
    // Never auto-capture an in-page fetch. Every hover preview / DASH
    // segment / thumbnail / API call would otherwise open its own RapidGrab
    // window (each hover produces a fresh signed URL, so the per-URL
    // dedupe cannot catch it). Real downloads from here are handled by
    // `downloads.onCreated` below.
    if (!isNavigation(details.type)) return;
    if (!isDownloadable(headers, captureUrl, details.type)) return;
    const now = Date.now();
    if (wasCapturedRecently(captureUrl, now)) return;
    noteCapture(captureUrl, now);

    const disposition = headerValue(headers, "content-disposition") || "";
    const filename = parseDispositionFilename(disposition) || guessFilename(captureUrl);
    const contentRange = headerValue(headers, "content-range");
    const hadRangeParam = captureUrl !== details.url;
    const lengthHeader = headerValue(headers, "content-length");
    const contentLength =
      !contentRange && !hadRangeParam && lengthHeader && /^\d+$/.test(lengthHeader.trim())
        ? Number(lengthHeader.trim())
        : undefined;

    const tab = await api.tabs.get(details.tabId).catch(() => null);
    await sendCapture(captureUrl, tab, {
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
  const when = Date.now();

  // Only a download that just started is a capture. Chrome replays the whole
  // download history through this event on startup (complete and cancelled
  // entries included), which used to open one confirm window per entry — the
  // stack of "New download — RapidGrab" windows after every boot.
  const startedAt = Date.parse(downloadItem.startTime || "");
  if (Number.isFinite(startedAt) && when - startedAt > RESTORED_DOWNLOAD_GRACE_MS) {
    return;
  }
  if (downloadItem.state && downloadItem.state !== "in_progress") return;
  // Duplicates we cancelled ourselves must never reopen the popup.
  if (downloadItem.byExtensionId && downloadItem.byExtensionId === api.runtime.id) return;

  const url = stripRangeParams(downloadItem.finalUrl || downloadItem.url || "");
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

async function sendCapture(url, tab, info) {
  url = stripRangeParams(url);
  if (!/^https?:\/\//i.test(url)) {
    console.warn("[RapidGrab] not an http(s) URL:", url);
    return;
  }
  info = info || {};
  // Send the title raw. rg-core's clean_title owns site-suffix stripping,
  // notification counters, generic rejection, and Windows sanitizing.
  const rawTitle = info.title || (tab && tab.title) || "";
  const payload = {
    url,
    filename: info.filename || guessFilename(url) || undefined,
    referer: (tab && tab.url) || undefined,
    user_agent: navigator.userAgent,
    title: String(rawTitle).trim() || undefined,
    content_disposition_filename: info.contentDisposition
      ? parseDispositionFilename(info.contentDisposition) || undefined
      : undefined,
    content_type: info.contentType,
    content_length: info.contentLength,
    audio_url: info.audioUrl,
    audio_content_type: info.audioContentType,
    audio_content_length: info.audioContentLength,
    audio_missing: info.audioMissing === true || undefined,
    source: info.source || "click",
  };

  // Cookies for the target host (best-effort; empty on failure).
  try {
    const cookies = await api.cookies.getAll({ url });
    const cookie = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    if (cookie) payload.cookies = cookie;
  } catch (_) {}
  // A separate audio track usually lives on the same CDN host, but some
  // deliveries use a second host — carry its own cookie jar when it does.
  if (info.audioUrl && hostFromUrl(info.audioUrl) !== hostFromUrl(url)) {
    try {
      const audioCookies = await api.cookies.getAll({ url: info.audioUrl });
      const audioCookie = audioCookies.map((c) => `${c.name}=${c.value}`).join("; ");
      if (audioCookie) payload.audio_cookies = audioCookie;
    } catch (_) {}
  }
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
  if (!msg || msg.type !== "rapidgrab-media") {
    return false;
  }
  const now = Date.now();
  const tabId = sender.tab ? sender.tab.id : undefined;
  (async () => {
    // MSE players (Facebook, …) have no downloadable `currentSrc`; the
    // content script sends an empty URL and we resolve to the freshest
    // media response seen on this tab. Falls back to the page URL (the
    // resolver handles it) when nothing recent was recorded or the host
    // needs yt-dlp.
    let url = stripRangeParams(msg.url || "");
    let contentType;
    let contentLength;
    let audioUrl;
    let audioContentType;
    let audioContentLength;
    let audioMissing;
    if (!/^https?:\/\//i.test(url)) {
      const candidates = tabId !== undefined ? recentMediaFor(tabId, now) : [];
      const tracks = candidates.length ? await selectMediaTracks(candidates) : null;
      if (tracks && tracks.video) {
        // The chosen video track. Facebook DASH serves a separate audio track
        // alongside it; when the tab recorded one, hand it to the downloader
        // so the two can be merged. Otherwise the popup warns "Video only".
        url = tracks.video.url;
        if (tracks.video.type) contentType = tracks.video.type;
        if (Number.isFinite(tracks.video.total) && tracks.video.total > 0) {
          contentLength = tracks.video.total;
        }
        if (tracks.audio) {
          audioUrl = tracks.audio.url;
          if (tracks.audio.type) audioContentType = tracks.audio.type;
          if (Number.isFinite(tracks.audio.total) && tracks.audio.total > 0) {
            audioContentLength = tracks.audio.total;
          }
        } else if (tracks.videoOnly) {
          audioMissing = true;
        }
      } else {
        url = (sender.tab && sender.tab.url) || "";
      }
    }
    if (!/^https?:\/\//i.test(url)) {
      sendResponse({ ok: false });
      return;
    }
    // Keep the tab's own title as a second source; the content script's
    // click-time title (post text / watch h1 / og:title) is more precise.
    const tab = sender.tab
      ? { url: sender.tab.url, title: sender.tab.title }
      : null;
    sendCapture(url, tab, {
      title: msg.title,
      source: "media",
      contentType,
      contentLength,
      audioUrl,
      audioContentType,
      audioContentLength,
      audioMissing,
    });
    sendResponse({ ok: true });
  })().catch((e) => {
    console.warn("[RapidGrab] media message failed:", e);
    try {
      sendResponse({ ok: false });
    } catch (_) {}
  });
  return true; // asynchronous sendResponse
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

// Interception: watch navigations only. The browser's own download is
// cancelled after the GUI acks. In-page responses never pop the window —
// anything the browser downloads from one is caught by `downloads.onCreated`
// below instead of by this header watch.
if (api.webRequest && api.webRequest.onHeadersReceived) {
  api.webRequest.onHeadersReceived.addListener(
    handleIntercept,
    { urls: ["http://*/*", "https://*/*"] },
    // Without this extra info spec Chrome/Firefox omit the headers
    // entirely, so nothing can tell an audio track from a video track.
    ["responseHeaders"],
  );
}
if (api.downloads && api.downloads.onCreated) {
  api.downloads.onCreated.addListener(onDownloadCreated);
}
