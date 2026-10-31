// Controlled Range-capable HTTP server for observing download-manager behavior.
// Logs every request as JSON lines to lab/server.log and tracks concurrency.
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

const LOG = path.join(process.cwd(), "lab", "server.log");
const PORT = Number(process.env.PORT || 8765);

// Deterministic pseudo-content so any byte range can be verified independently.
function byteAt(i) {
  // xorshift-ish deterministic byte
  let x = (i * 2654435761) >>> 0;
  x ^= x >>> 13;
  return (x * 2246822519) >>> 0 & 0xff;
}

function writeLog(obj) {
  fs.appendFileSync(LOG, JSON.stringify({ t: Date.now(), ...obj }) + "\n");
}

let active = 0;
let peak = 0;
let reqSeq = 0;

const server = http.createServer((req, res) => {
  const id = ++reqSeq;
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const size = Number(url.searchParams.get("size") || 10485760);
  const rate = Number(url.searchParams.get("rate") || 0); // bytes/sec throttle per connection
  const dropAfter = Number(url.searchParams.get("drop") || 0); // drop connection after N body bytes
  const delayMs = Number(url.searchParams.get("delay") || 0); // delay before headers

  active++;
  peak = Math.max(peak, active);
  const started = Date.now();

  const record = {
    id,
    method: req.method,
    path: url.pathname,
    query: Object.fromEntries(url.searchParams),
    range: req.headers["range"] || null,
    ua: req.headers["user-agent"] || null,
    referer: req.headers["referer"] || null,
    accept: req.headers["accept"] || null,
    acceptEncoding: req.headers["accept-encoding"] || null,
    ifRange: req.headers["if-range"] || null,
    active: active,
    peak,
  };
  writeLog({ event: "request", ...record });

  const respond = () => {
    const etag = `"file-${size}"`;
    const lastMod = new Date(1700000000000).toUTCString();
    let start = 0;
    let end = size - 1;
    let status = 200;
    const rangeHeader = url.searchParams.get("norange") === "1" ? null : req.headers["range"];
    if (rangeHeader) {
      const m = /^bytes=(\d*)-(\d*)$/.exec(rangeHeader.trim());
      if (m) {
        if (m[1] !== "") start = Number(m[1]);
        if (m[2] !== "") end = Math.min(Number(m[2]), size - 1);
        if (m[1] === "" && m[2] !== "") { // suffix range
          start = Math.max(0, size - Number(m[2]));
          end = size - 1;
        }
        if (start > end || start >= size) {
          res.writeHead(416, { "Content-Range": `bytes */${size}` });
          res.end();
          writeLog({ event: "response-416", id });
          return;
        }
        status = 206;
      }
    }
    const length = end - start + 1;
    const headers = {
      "Content-Type": "application/octet-stream",
      "Content-Length": String(length),
      "Accept-Ranges": "bytes",
      "ETag": etag,
      "Last-Modified": lastMod,
      "Connection": "keep-alive",
    };
    if (status === 206) headers["Content-Range"] = `bytes ${start}-${end}/${size}`;

    if (req.method === "HEAD") {
      res.writeHead(status, headers);
      res.end();
      writeLog({ event: "response-head", id, status, start, end, size });
      active--;
      return;
    }

    res.writeHead(status, headers);
    writeLog({ event: "response-start", id, status, start, end, size });

    let pos = start;
    let sent = 0;
    let done = false;

    const finish = (why) => {
      if (done) return;
      done = true;
      active--;
      writeLog({
        event: why,
        id,
        sent,
        elapsedMs: Date.now() - started,
        active,
        status,
        range: req.headers["range"] || null,
      });
    };

    res.on("close", () => finish("client-closed"));
    res.on("finish", () => finish("response-finished"));

    const chunkSize = 64 * 1024;
    const pump = () => {
      if (done) return;
      if (dropAfter > 0 && sent >= dropAfter) {
        writeLog({ event: "forced-drop", id, sent });
        res.destroy(); // simulate reset
        finish("forced-drop");
        return;
      }
      let want = Math.min(chunkSize, end - pos + 1);
      if (dropAfter > 0) want = Math.min(want, dropAfter - sent);
      if (want <= 0) { res.end(); return; }
      const buf = Buffer.allocUnsafe(want);
      for (let k = 0; k < want; k++) buf[k] = byteAt(pos + k);
      pos += want;
      sent += want;
      const ok = res.write(buf);
      if (ok) {
        if (rate > 0) setTimeout(pump, (want / rate) * 1000);
        else setImmediate(pump);
      } else {
        res.once("drain", () => {
          if (rate > 0) setTimeout(pump, (want / rate) * 1000);
          else setImmediate(pump);
        });
      }
    };

    if (delayMs > 0) setTimeout(pump, delayMs);
    else pump();
  };

  if (delayMs > 0) setTimeout(respond, delayMs);
  else respond();
});

server.on("clientError", (err, socket) => {
  writeLog({ event: "client-error", message: String(err && err.message) });
  try { socket.destroy(); } catch {}
});

server.listen(PORT, "127.0.0.1", () => {
  writeLog({ event: "server-start", port: PORT, pid: process.pid });
  console.log(`range-server listening on http://127.0.0.1:${PORT}`);
});
