# Release and distribution (Phase 6)

This document covers packaging the RapidGrab browser extension for store channels, signing notes, and the native-host reinstall step. The extension must talk to the installed native host (`rapidgrab-host.exe`); store distribution of the extension does **not** replace the desktop install.

## Binaries and native host

- Build and install with `tools/install-native-host.ps1` (or the installer). The host/popup/GUI live under the install dir (e.g. `%LOCALAPPDATA%\Programs\RapidGrab\`).
- After any extension ID change, rerun the installer/registration so the native host manifests include the correct `allowed_origins` / `allowed_extensions`.

## Chrome and Edge (Chromium)

- Source: `tools/rapidgrab-extension/` (MV3, `background.service_worker`).
- Pack: `powershell -ExecutionPolicy Bypass -File tools\pack-extension.ps1 -Chrome [-KeyPath <pem>]`
  - Always produces `dist/extension/chrome/rapidgrab-extension.zip` for **Chrome Web Store** and **Edge Partner Center** upload.
  - Produces a signed CRX3 only when Chrome/Edge is installed and `-KeyPath` points to an existing packing key PEM. Generate a dev key once via Chrome's "Pack extension" UI or `openssl`; keep the PEM private and gitignored.
  - `updates.xml` is a template for self-hosted updates (enterprise use only; see below).
- **Store channels (recommended):** upload the zip to CWS and Edge Partner Center. The same MV3 package is accepted by both. Store distribution handles signing and updates.
- **Self-hosted CRX3:** not viable for regular users on modern Chrome/Edge. Installing a self-hosted CRX requires enterprise policy (`ExtensionInstallForcelist`, allowlists/update URL, and appropriate `ExtensionSettings`). Treat self-hosting as an enterprise/admin path, not end-user distribution.
- Extension ID continuity: the unpacked/dev ID is derived from the key/path; the published store ID may differ. Ensure the native messaging host manifest `allowed_origins` includes the final `chrome-extension://<id>/` origin(s) for each channel you ship.

## Firefox

- Source manifest: `tools/rapidgrab-extension/manifest.firefox.json` (MV3 with `background.scripts` and `browser_specific_settings.gecko`).
- Pack: `powershell -ExecutionPolicy Bypass -File tools\pack-extension.ps1 -Firefox`
  - Produces `dist/extension/firefox/rapidgrab-extension.xpi` (unsigned zip).
- **AMO (recommended):** submit the XPI/zip to addons.mozilla.org. Listed or unlisted signing via AMO is required for distribution to release Firefox (unlisted can be used for controlled/self-distribution with a signed artifact).
- **`web-ext sign`:** possible with AMO API credentials (`web-ext sign --api-key ... --api-secret ...`), producing a signed XPI. Credentials are not stored in the repo; run locally/CI with secrets.
- Gecko ID: currently `rapidgrab@rapidgrab.local` in `manifest.firefox.json`. Keep it stable across releases and ensure the native host manifest `allowed_extensions` includes it (installer `-FirefoxExtensionId`).

## Native host manifests

- Chrome/Edge registry keys point to `hosts\com.rapidgrab.json` with `allowed_origins`.
- Firefox uses `hosts\com.rapidgrab.moz.json` with `allowed_extensions` (registry/profile install per `tools/install-native-host.ps1`).
- After packaging for a store, verify the installed host manifests match the shipped extension IDs and fully restart the browser.

## Verification before publishing

- Run the manual checklist in `docs/release-checklist.md`.
- Capture screenshots per `tools/screenshots.ps1` guidance and place them in `docs/screenshots/`.
- Confirm tray/notifications/autostart/virus-check (Defender preset) and `rapidgrab get` CLI parity on a clean profile where possible.
