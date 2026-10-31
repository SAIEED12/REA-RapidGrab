# RapidGrab — an IDM-style download engine (clean-room)

`rapidgrab` is a from-scratch Rust implementation of the download-engine behaviour
observed in **Internet Download Manager 6.41 build 20** on this machine. No IDM
code is used or copied; the implementation is written against observed protocol
behaviour and its own design choices.

## 1. What was inspected

Target: `C:\Program Files (x86)\Internet Download Manager\IDMan.exe`
(PE32 x86, 6,011,144 bytes, SHA-256 `2fdfa3de…49a6`).

Static facts (REA PE import/export evidence):

* Raw `WS2_32` sockets (23 functions imported **by ordinal**) plus delay-loaded
  `WININET` URL helpers — the engine implements HTTP itself.
* Ships OpenSSL (`libssl.dll` / `libcrypto.dll`) for TLS.
* Classic Win32 UI, tray icon, shell integration, clipboard monitor, pipe-based
  helper processes (`CreateProcess*` + `PeekNamedPipe`).
* Per-download state lives in `%APPDATA%\IDM\DwnlData\<user>\<host>_<port>\`,
  queue state in `%APPDATA%\IDM\Scheduler\q_*.dt`.

A deeper static pass (Ghidra 12.1.4 via REA, 18,791 procedures) decompiled the
split/takeover path, the resume rebuild path, the HTTP header dispatcher and the
settings accessors. The findings are folded into section 2; the three that
changed the clone are the 46,721-byte split threshold, the registry-backed
segment map, and the total absence of content checksums.

Runtime facts were collected with Frida hooks on `ws2_32!send/recv`,
`libssl!SSL_write/read`, `CreateFileW`, `WriteFile` and `CreateProcessW`,
driving downloads from a local Range-capable test server
(`lab/range-server.mjs`). All test payloads were generated deterministically and
every completed download was verified byte-for-byte.

## 2. Observed engine behaviour

| # | Behaviour | Evidence |
|---|-----------|----------|
| 1 | First request is a plain `GET` (200 + `Content-Length`), used to learn size/range support | server log run 1, id 1; Frida `send` payload |
| 2 | Header set: browser-like `User-Agent`, `Accept: */*`, `Accept-Encoding: identity`, `Referer: <origin>/` | Frida `send` capture |
| 3 | Up to **8** parallel connections (registry DWORD `MaxConnectionsNumber`); each new worker splits the current tail **at its midpoint** | log: `C2:25295786-50331647`, `C3:37813716-…`; `FUN_004faff0` |
| 4 | When a connection finishes, it takes **half of the largest remaining segment** ("connection takeover"), but only once ≥ **46,721** bytes (`0xb681`) remain | log: `Adding record … startPos 12864371`; `FUN_0055a820` |
| 5 | Each segment is written to its **own part file** (`127_0_0_1`, `127_0_0_1{11..18}`), merged only at the end | DwnlData listing + Frida `createFileW` |
| 6 | Segment map persisted as a 16-byte registry record per chunk (`{id, startPos:int64, nextID}`), rebuilt on restart ("Rebuilding old downloaded file parts…"); `Scheduler\s_*.dt` is a raw memory dump, not the authoritative store | `RegSetValueExA` in `FUN_0055a820`; `FUN_0055c280`; `log_*.log` |
| 7 | Resume after restart requires the download to be started again; it then continues from the exact per-segment offsets | run 3 (killed at ~5.8 MB, resumed at ~27 MB, final SHA-256 match) |
| 8 | Resume does **not** send `If-Range`/`If-Match`; the strings exist only as header *filters* (matched, then dropped), so the remote file is assumed unchanged | server log `ifRange: null`; `FUN_005d2ab0` |
| 9 | On connection reset: immediate reconnect (≈10 ms), no backoff, exact-offset continuation | drop test, 2 MB drops |
| 10 | Server without Range support: falls back to the full-body response | `norange=1` test |
| 11 | Aggregate speed is the sum of per-connection throughput; a global limiter is configurable | 8 × 512 KB/s ≈ 4.1 MB/s observed |
| 12 | **No content checksum anywhere** (no CRC32/MD5/SHA over the assembled file); integrity is structural only — a broken part chain aborts with "the build sequence of downloaded parts got broken" | full string search + merge path decompilation |

## 3. Clone design

```
crates/rg-core   engine: probe, scheduler, segment store, limiter, merge
crates/rg-cli    rapidgrab: get / status
```

* **Probe** — `HEAD`, falling back to `GET Range: bytes=0-0`; records size,
  `Accept-Ranges`, `ETag`, `Last-Modified`, final URL after redirects.
* **Scheduler** — starts with one segment `[0, size-1]`; each worker that finds
  no unclaimed work splits the segment with the most remaining bytes, handing its
  tail to the new worker. A worker notices its segment end being reduced and
  stops exactly at the new boundary — the same takeover behaviour as IDM.
  Splitting only happens while at least `MIN_SPLIT_BYTES` (46,721 = `0xb681`)
  remain, matching IDM's threshold; below that the remaining tail stays with its
  current worker.
* **Segment store** — one `part_XXXX` file per segment plus
  `manifest.json` (URL, size, ETag, last-modified, segment table). Written to
  `<output>.rgparts/`, atomically replaced every 500 ms and on every structural
  change. *Deliberate improvement over IDM:* a portable JSON file instead of
  registry values plus a raw `.dt` memory dump.
* **Resume** — manifest is reloaded when URL and size match; part files are
  truncated to the recorded positions. *Deliberate improvement over IDM:* the
  engine sends `If-Range: <etag>` and refuses to resume when `ETag` or
  `Last-Modified` changed (`Error::RemoteChanged`).
* **Retry** — per segment, exponential backoff 100 ms → 5 s, capped attempts
  per connection, always continuing from the last persisted offset. (IDM
  retries instantly and can hammer a failing host; backoff is a safety choice.)
* **Limiter** — global token budget shared by all workers.
* **Merge** — part files are concatenated in start order, size-checked, fsynced,
  then the parts directory is removed. The final file only appears on success.
* **Verification** — optional `--sha256 <hex>` streams the finished file through
  SHA-256 and fails the run with `sha256 mismatch for …` if the digest differs.
  The assembled file is kept so the mismatch can be inspected. *Deliberate
  improvement over IDM:* IDM never validates content at all, so a server that
  silently returns wrong bytes produces a "successful" download.
* **Fallback** — no range support or unknown length → single streaming download
  to `<output>.rgtmp`, then atomic rename.

## 4. Usage

```powershell
cargo build --release
target\release\rapidgrab.exe get https://example.com/big.iso -o D:\Downloads\big.iso -n 8
target\release\rapidgrab.exe get https://example.com/big.iso -o D:\Downloads\big.iso --limit 2097152   # 2 MB/s
target\release\rapidgrab.exe get <same url> -o <same path>      # resumes from parts
target\release\rapidgrab.exe get <url> -o <path> --restart      # discard parts
target\release\rapidgrab.exe get <url> -o <path> --sha256 <hex> # verify content
target\release\rapidgrab.exe status D:\Downloads\big.iso        # segment map
```

## 5. Test harness

`lab/range-server.mjs` serves deterministic pseudo-random bytes with:

* `?size=<bytes>` — file size (default 10 MiB)
* `?rate=<bytes/sec>` — per-connection throttle
* `?drop=<bytes>` — reset each connection after N body bytes
* `?norange=1` — ignore `Range` (simulate a non-resumable server)

It logs every request (Range, UA, timings, concurrency) as JSON lines to
`lab/server.log`, so engine behaviour can be compared directly with the IDM
runs captured here (`lab/log_run*.log`, `lab/server_run2.log`).

## 6. Verification

`lab/verify.mjs` replays the same scenarios against `lab/range-server.mjs` and
compares every output byte-for-byte with the deterministic source data:

| Scenario | Result |
|---|---|
| 8 MiB, 8 connections | PASS — 0.4 s, byte-exact; 13 ranged GETs with browser-like headers, peak concurrency ≤ 8 |
| 24 MiB, server 512 KB/s/conn, client cap 2 MB/s | PASS — 12.2 s ≈ 1.98 MB/s |
| 32 MiB with a 2 MB reset per connection | PASS — 9.7 s, 27 ranged GETs after 32 forced drops |
| 8 MiB, server ignores `Range` | PASS — single non-ranged GET fallback |
| 96 MiB, killed after 3 s then resumed | PASS — 24.0 s, byte-exact |
| 4 MiB with the correct `--sha256` | PASS — digest verified |
| 4 MiB with a wrong `--sha256` | PASS — rejected (`sha256 mismatch`), non-zero exit |

That is 10/10 checks (7 downloads plus 3 protocol assertions) on a fresh
release build.

The harness also checks protocol parity from the server log: browser-like
headers on ranged requests, no more than the requested parallel connections,
forced-drop reconnects, and a single non-ranged fallback when a server
advertises ranges but ignores them.

Effect of the 46,721-byte split threshold: a 600 KiB download now reaches
**8 segments** (the previous 256 KiB guard stopped at 4), so medium files
saturate all connections the way IDM does.

`cargo test` covers the split policy (midpoint, current-position, IDM's 46,721
threshold, minimum-chunk floor), segment accounting and Referer derivation
(7 tests).

## 7. Roadmap

1. Engine parity tests against the recorded IDM traces (split policy, takeover,
   reconnect, resume).
2. Queue persistence + scheduler (a proper JSON/SQLite store — IDM keeps its
   queue in `Scheduler\q_*.dt` and its per-download segment map in the registry,
   neither of which is a portable format).
3. Browser integration via WebExtension + native messaging (modern, documented
   APIs — no browser hooks needed).
4. Media grabber (HLS/DASH manifests) and UI.
