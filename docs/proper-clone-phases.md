# Proper IDM Clone — Phased Plan (Clean-Room)

Decisions: **Interception = A (extension-only, no driver) · Browsers = Chrome + Edge + Firefox · Modes = confirm-first + auto-start trust list**
Legal: reimplement from observed behavior only (strings, registry, manifests, runtime UI). No IDM code, assets, icons, help text, or DLL contents copied.

## Phase 0 — Foundation (1–2 days)
- Installer: ship `rapidgrab-host.exe` to `C:\Program Files\RapidGrab\`, write both host manifests + registry keys (HKCU + HKLM), Firefox manifest in profile dir; uninstaller reverses. No `target\debug` absolute paths.
- Host hardening: persistent stdio host (multi-frame loop, not one-shot), ACL'd named pipe (current-user-only SD), structured ack `{ok, error}`.
- Extension: MV3 shared manifest + Firefox variant (`allowed_extensions`, `browser.*` shim).

Gate: install/uninstall on clean machine; click capture works for Chrome/Edge/Firefox.

## Phase 1 — Download popup + interception (1–2 weeks) [core, decide A confirmed]
- MV3 `webRequest.onHeadersReceived` plus `downloads.onCreated` capture real browser downloads → sends the capture to the host → after the host acks, `chrome.downloads.pause/cancel/erase` removes the browser's duplicate, and the GUI takes over. Streaming media is deliberately excluded: a video/audio response (by content type, `media` request type, or a streaming file extension such as `.mp4`) only counts when the browser navigated to it with `Content-Disposition: attachment`; in-page playback fetches never pop, even when a CDN labels them attachment (Facebook's scontent player requires this). Captures are deduped per URL for 30 s so chunked/range requests and the header+downloads double path produce one window. Video on a page is captured only when the user clicks the inline `↓ RapidGrab` player button; MSE players expose only a `blob:` `currentSrc`, so the service worker remembers the most recent streaming response per tab (15 min) and the button resolves to that real CDN URL — except for resolver hosts (YouTube/`googlevideo`), which keep sending the page URL to yt-dlp.
- Download popup in GUI: filename, size/type (probe), Save-to + category, Start / Download Later / Cancel, "don't ask again for this site", Resume when partial exists.
- File-type rules + ExceptionServers equivalent.
- Stale signed-URL flow: tell user to re-trigger from browser.

Known limits (A): POST-auth and some streaming do not intercept.

Gate: download a ZIP/PDF/MP4 from a real site in Chrome — popup appears, browser download is cancelled, GUI completes it.

## Phase 2 — Browser coverage + content sniffing (3–5 days)
- Chrome + Edge Chromium registry manifests; Brave/Vivaldi/Opera via Chromium keys; Firefox native host + `moz-extension://` origin.
- Content-script media sniffing: `<video>`, `<a download>`, HLS playlists → inline "Download this video" affordance (IDM player-panel equivalent).
- Context menus + toolbar action state per page type.

Gate: each browser captures page + link + video with its own registration.

## Phase 3 — Protocols & network (1 week) ✅ DONE
- FTP (PASV/PORT, user/pass from popup, REST resume). ✅ PASV (EPSV→PASV fallback, no active PORT), URL/Advanced/CLI credentials, REST resume into `.rgtmp`; parallel segments deferred (needs a server-capability probe).
- Proxy: HTTP/HTTPS/SOCKS5 + PAC + per-protocol auth, wired into reqwest + GUI options. ✅ Off/Manual/System (`WinHttpGetProxyForUrl` + WPAD/PAC), no_proxy bypass, preemptive Basic auth; cross-host redirect keeps the job's first proxy choice (accepted limitation).
- Auth-retry popup on 401/407 instead of failing. ✅ GUI authentication dialog retries the job with entered credentials; FTP `530` (`Error::FtpAuth`) opens the same dialog.
- RTSP/MMS basic support. ❌ deferred to Phase 5 entirely (per decision).

Gate: FTP download with credentials; proxied HTTPS download; 401 prompts for login. ✅
`tests/proxy.rs` (proxy routing + bypass), `tests/auth.rs` (401→prompt→complete
with credentials), `tests/ftp.rs` (anonymous/credentials/wrong-password/PASV
fallback/REST resume); proxied-HTTPS and System-mode verified manually on a
real proxy (docs in README §6).

## Phase 4 — Productivity (1–2 weeks)
- Scheduler: timed start/stop, hibernate/sleep after queue, daily MB limits.
- Queue UI: Start/Stop Queue, per-item priority.
- Categories: Compressed/Documents/Music/Programs/Video auto-bucket + per-category Save-to.
- History + log persisted across sessions.

Gate: schedule an overnight queue; category auto-routing verified.

## Phase 5 — Site Grabber + media (1 week)
- Grabber crawler: site URL + rules → same-domain crawl, file-type filter → project list → batch download.
- YouTube: keep yt-dlp path but vendor pinned `yt-dlp.exe` (+ ffmpeg mux) so it works offline; or native get_video_info later.
- Virus-check hook (launch AV on completion) — low priority.

Gate: grab all MP4s from a test site into a project; YouTube download works without system yt-dlp.

## Phase 6 — Polish + release (2–3 days)
- Tray icon, balloon notifications, auto-start, CLI parity (`rapidgrab get` exists).
- Signed CRX + Firefox XPI via store channels.
- Screenshots 1920×1080 + 640×520; manual checklist (popup per browser, resume, scheduler, grabber, Bengali titles).

## Analysis note (Ghidra)
Static IDM pass used REA/Ghidra 12.1.4 (`IDMan.exe` overview `ev_17be9aca…`, string/procedure searches). Before Phase 1, targeted Ghidra pass: `IDMMsgHost.exe` stdio protocol, `Download File Info` dialog resources in `IDMan.exe` `.rsrc`, `content.js` heuristics in the Edge CRX — to mirror behavior, not copy assets.

## Estimate
Phases 0–2: shippable interception + popup clone (~2–3 weeks). Full IDM surface: ~6–8 weeks.
