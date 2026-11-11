# RapidGrab Phase 6 — manual release checklist

Run on a machine with the desktop app installed and the extension loaded for the target browser(s). Record pass/fail, browser/version, and notes.

## Core capture & downloads

- [ ] **Popup — Chrome:** click a direct download link and a media player button; popup appears with correct title/extension, no `RESOLVED ON START` chip, full-name tooltip, wrapped chips; Start and Download Later work.
- [ ] **Popup — Edge:** same as Chrome.
- [ ] **Popup — Firefox:** same as Chrome (native host registered with the Firefox extension ID).
- [ ] **Intercepted download:** a `Content-Disposition: attachment; filename="..."` download keeps the server filename (unless generic) and starts in the chosen category folder.
- [ ] **Bengali titles:** YouTube/Facebook Bengali titles render without tofu boxes in the popup and GUI (Nirmala UI fallback); grapheme truncation does not break conjuncts.
- [ ] **Collision naming:** saving to a folder where the name exists suggests `name (1).ext` (and increments).

## Queue, resume, scheduler, grabber

- [ ] **Resume after restart:** pause/quit mid-download, relaunch RapidGrab, resume completes the file (hash/size sane).
- [ ] **Scheduler:** configure a start/stop window (including a cross-midnight case if relevant), verify the queue starts/stops at the boundary and after-queue action (if enabled) fires once.
- [ ] **Grabber:** scan a test page with known MP4/media links, filter/select results, **Add selected** enqueues with correct Referer/category routing and files download.

## Phase 6 polish

- [ ] **Tray icon:** tray icon appears when enabled; right-click menu shows Show / Start|Stop Queue / Open Downloads / Exit.
- [ ] **Close-to-tray:** closing the main window hides it (does not exit) when "Minimize to tray when closed" is on; tray Show restores it; captures still open the popup while hidden.
- [ ] **Tray Exit:** Exit from tray fully quits the app.
- [ ] **Balloon — complete:** finishing a download shows a completion notification (suppressed appropriately when focused, per setting).
- [ ] **Balloon — failure:** a failed download shows a failure notification with the error.
- [ ] **Auto-start:** enable "Start RapidGrab with Windows", sign out/in (or check HKCU Run) — app starts; with "Start minimized to tray" it starts hidden to tray. Disable removes the Run entry.
- [ ] **Virus check — Defender preset:** "Use Windows Defender" fills the command and enables scanning; a completed clean file shows a clean result; (optional, controlled) EICAR test shows threat detection; a bad command shows a scan error. Feature is off by default.
- [ ] **CLI parity:** `rapidgrab get <url>` derives a name into Downloads when `--output` omitted; `--dir`, `--title`, `--referer`, `--cookie` behave as expected; `--virus-check` / `--virus-command` runs post-download scan.

## Packaging / release artifacts

- [ ] **Extension packages:** `tools\pack-extension.ps1` produces Chrome zip (+ CRX if key provided) and Firefox unsigned XPI; manifests/IDs match installed native host registration.
- [ ] **Screenshots:** 1920×1080 and 640×520 GUI shots plus popup shots (YouTube, Facebook/CDN, Bangla, Content-Disposition) saved to `docs/screenshots/`.
- [ ] **Docs:** `docs/release.md` reviewed; `docs/proper-clone-phases.md` Phase 6 marked done with notes.

## Environment

- OS/Build:
- Chrome version:
- Edge version:
- Firefox version:
- RapidGrab build/commit:
- Tester / date:
- Notes:
