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

## Phase 4 — Productivity (1–2 weeks) ✅ DONE
- Scheduler: timed start/stop, hibernate/sleep after queue, daily MB limits. ✅ Window (HH:MM start/stop, crosses midnight) auto starts/stops the queue; after-queue Sleep/Hibernate/Shut down via `rundll32`/`shutdown.exe`; daily MB cap pauses the queue at the limit and resets at midnight (`%APPDATA%\RapidGrab\usage.json`). New `crates/rg-config` crate holds the shared config, queue, history and usage files.
- Queue UI: Start/Stop Queue, per-item priority. ✅ Header toggle persists to `settings.json`; per-card priority cycles Low→Normal→High and `pump_queue()` runs High first; pending jobs persist to `queue.json` and restore as Queued on launch.
- Categories: Compressed/Documents/Music/Programs/Video auto-bucket + per-category Save-to. ✅ `Category::from_filename`/`from_content_type` in `rg-config`; routing applies in the GUI suggestions and the standalone popup (both read `settings.json`), with per-category folder overrides in the Categories settings section.
- History + log persisted across sessions. ✅ Append-only `history.jsonl` (rotated at 5000 lines) with a History page (Open/Folder/Retry/Copy URL/Clear); daily rolling log under `logs\` via `tracing-appender`.

Gate: schedule an overnight queue; category auto-routing verified. ✅ (manual: set Scheduler 22:00–06:00 + Sleep; add an MP4 and a ZIP and check `Downloads\Video` / `Downloads\Compressed`; restart the GUI — queue and history survive.)
`crates/rg-config/tests` — category mapping, schedule windows incl. midnight crossing, priority ordering, atomic config round-trip.

## Phase 5 — Site Grabber + media (1 week) ✅ DONE (tool vendoring deferred)
- Grabber crawler: site URL + rules → same-domain crawl, file-type filter → project list → batch download. ✅ New `crates/rg-grab`: BFS `scan` over `href`/`src`/`data-*` attributes plus absolute URLs in inline script JSON; relative-URL resolution, depth/page caps, include/exclude `*` patterns, `robots.txt`, four concurrent fetches with a polite delay, per-URL dedupe and `AtomicBool` cancellation. Reuses `NetworkConfig::client_for`, so proxy and credential settings apply. 14 unit tests run against a local HTTP server (relative/cross-host links, depth, page cap, filters, robots, cancel, pattern matching).
- Grabber UI: ✅ New **Grabber** page (`Page::{Downloads, History, Grabber}`) with URL, depth, max pages, same-domain/robots toggles, preset file types (Video/Audio/Images/Archives/Documents/Programs) plus custom extensions, include/exclude filters, live results with per-file checkboxes and Select all/none/Clear, and **Add selected** (enqueues through the normal path: category routing, priority, `Referer: <page>`). The form persists in `settings.json` as `GrabberSettings`.
- Helper tool discovery: ✅ `rg_resolve::find_tool` now searches `%LOCALAPPDATA%\Programs\RapidGrab\tools` → `<exe folder>\tools` → `PATH` → common install dirs, with unit tests; a manually dropped `yt-dlp.exe`/`ffmpeg.exe` is picked up first.
- YouTube: resolver path unchanged. **Deferred:** vendoring pinned `yt-dlp.exe`/`ffmpeg.exe` (no bundling per decision) and therefore the offline-YouTube part of the gate.
- Virus-check hook (launch AV on completion). **Deferred to Phase 6** per decision.

Gate: grab all MP4s from a test site and batch-download them ✅ (manual: open Grabber, scan a page with MP4 links, tick the results, Add selected → files land in `Downloads\Video`). YouTube without a system yt-dlp ⏸ deferred with vendoring.
`crates/rg-grab` — extraction, crawl traversal, depth/page caps, extension/pattern filters, robots, cancellation; `crates/rg-resolve` — bundled-tool search order.

## Phase 6 — Polish + release (2–3 days) ✅ IMPLEMENTED (manual verification/signing pending)
- Tray icon, balloon notifications, auto-start, CLI parity. ✅ GUI: Win32 tray (`crates/rg-gui/src/tray.rs`) with show/start-stop queue/open downloads/exit menu, close-to-tray (`CancelClose` + hide), tray Exit, balloon notifications on complete/failure (suppressed behavior configurable), `--minimized` start. Application settings: tray/close-to-tray/notify toggles, Start with Windows (HKCU Run) + start minimized. CLI: `rapidgrab get` now accepts optional `--output` (derived via shared naming + `--dir`/`--title`), `--referer`, `--cookie`, and `--virus-check`/`--virus-command`.
- Virus-check hook (deferred from Phase 5): `virus_check_command` with a `{file}` placeholder, Defender preset, off by default. ✅ GUI settings with Defender auto-detect/fill (`MpCmdRun.exe`, newest Platform preferred), background scan on completion and manual scan path, result stored on job (`Clean`/`Threat`/`Error` with message). CLI runs the same post-download scan when requested.
- Signed CRX + Firefox XPI via store channels. ✅ Added `tools/rapidgrab-extension/manifest.firefox.json` (gecko id/background scripts), `tools/pack-extension.ps1` (Chrome zip + optional CRX, Firefox unsigned XPI, updates.xml template), and `docs/release.md` documenting CWS/Edge/AMO, self-hosted CRX policy limits, and native-host ID alignment.
- Screenshots 1920×1080 + 640×520; manual checklist (popup per browser, resume, scheduler, grabber, Bengali titles). ✅ Added `tools/screenshots.ps1`, `docs/release-checklist.md` (includes tray/balloons/autostart/Defender/CLI cases); popup preview frames can be re-captured, GUI shots and full manual/browser/store sign-off remain to be completed on target environments.

Note: implementation is uncommitted. Some Phase 6 UI polish items (e.g. dedicated virus-status chip/action rendering on every card) may need follow-up during manual verification.

## Analysis note (Ghidra)
Static IDM pass used REA/Ghidra 12.1.4 (`IDMan.exe` overview `ev_17be9aca…`, string/procedure searches). Before Phase 1, targeted Ghidra pass: `IDMMsgHost.exe` stdio protocol, `Download File Info` dialog resources in `IDMan.exe` `.rsrc`, `content.js` heuristics in the Edge CRX — to mirror behavior, not copy assets.

## Estimate
Phases 0–2: shippable interception + popup clone (~2–3 weeks). Full IDM surface: ~6–8 weeks.
