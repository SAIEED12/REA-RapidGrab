// RapidGrab extension: confirm-first browser capture.
// Click the toolbar button (or the context menu) to send the page's first
// HTTP(S) link found by the heuristics below to RapidGrab via the native host.
// The GUI pre-fills the form; nothing auto-starts.

const HOST = "com.rapidgrab";

function guessFilename(url) {
  try {
    const u = new URL(url);
    const parts = u.pathname.split("/").filter(Boolean);
    const last = parts[parts.length - 1];
    if (last && last.includes(".")) return decodeURIComponent(last);
  } catch (_) {}
  return null;
}

function sendCapture(url, tab) {
  if (!/^https?:\/\//i.test(url)) {
    console.warn("[RapidGrab] not an http(s) URL:", url);
    return;
  }
  const filename = guessFilename(url);
  const referer = tab && tab.url ? tab.url : undefined;
  const user_agent = navigator.userAgent;

  // Cookies for the target host (best-effort; empty on failure).
  chrome.cookies
    .getAll({ url })
    .then((cookies) => {
      const cookie = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
      post({ url, filename, referer, cookies: cookie || undefined, user_agent });
    })
    .catch(() => post({ url, filename, referer, user_agent }));
}

function post(payload) {
  const port = chrome.runtime.connectNative(HOST);
  // Native messaging: send one length-prefixed JSON message.
  port.postMessage(payload);
  port.onDisconnect.addListener(() => {
    if (chrome.runtime.lastError) {
      console.warn("[RapidGrab] native host unavailable:", chrome.runtime.lastError.message);
    }
  });
  // Chrome closes the port after the message is sent; no response expected.
}

chrome.action.onClicked.addListener((tab) => {
  if (!tab || !tab.url) return;
  // Prefer the page URL itself; for link-heavy pages the user can use the
  // context menu on a specific link instead.
  sendCapture(tab.url, tab);
});

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "rapidgrab-page",
    title: "Download page with RapidGrab",
    contexts: ["page"],
  });
  chrome.contextMenus.create({
    id: "rapidgrab-link",
    title: "Download link with RapidGrab",
    contexts: ["link"],
  });
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  if (info.menuItemId === "rapidgrab-link" && info.linkUrl) {
    sendCapture(info.linkUrl, tab);
  } else if (info.menuItemId === "rapidgrab-page" && tab && tab.url) {
    sendCapture(tab.url, tab);
  }
});
