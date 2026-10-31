// End-to-end verification of rapidgrab against the local Range server.
// Replays the same scenarios used to observe IDM, and checks byte-exact output.
import { spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const EXE = path.resolve("target/release/rapidgrab.exe");
const BASE = "http://127.0.0.1:8765/file";
const OUT = path.resolve("lab/verify-out");
fs.mkdirSync(OUT, { recursive: true });

function byteAt(i) {
  let x = (i * 2654435761) >>> 0;
  x ^= x >>> 13;
  return ((x * 2246822519) >>> 0) & 0xff;
}

function expectedHash(size) {
  const h = crypto.createHash("sha256");
  const CH = 1 << 20;
  const buf = Buffer.allocUnsafe(CH);
  for (let pos = 0; pos < size; ) {
    const n = Math.min(CH, size - pos);
    for (let k = 0; k < n; k++) buf[k] = byteAt(pos + k);
    h.update(buf.subarray(0, n));
    pos += n;
  }
  return h.digest("hex");
}

function fileHash(p) {
  return crypto.createHash("sha256").update(fs.readFileSync(p)).digest("hex");
}

function logOffset() {
  const log = path.resolve("lab/server.log");
  return fs.existsSync(log) ? fs.statSync(log).size : 0;
}

function logRecordsSince(offset) {
  const log = path.resolve("lab/server.log");
  if (!fs.existsSync(log)) return [];
  const text = fs.readFileSync(log).subarray(offset).toString();
  return text
    .split("\n")
    .filter(Boolean)
    .map((line) => JSON.parse(line));
}

function run(args, killAfterMs = 0) {
  return new Promise((resolve) => {
    const started = Date.now();
    const child = spawn(EXE, args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "", stderr = "";
    child.stdout.on("data", (d) => (stdout += d));
    child.stderr.on("data", (d) => (stderr += d));
    let timer = null;
    if (killAfterMs > 0) {
      timer = setTimeout(() => child.kill(), killAfterMs);
    }
    child.on("close", (code) => {
      if (timer) clearTimeout(timer);
      resolve({ code, stdout, stderr, elapsed: Date.now() - started });
    });
  });
}

const results = [];
function check(name, ok, detail) {
  results.push({ name, ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? "  — " + detail : ""}`);
}

async function scenario(name, { size, query = "", args = [], killAfterMs = 0, parity }) {
  const logStart = logOffset();
  const out = path.join(OUT, `${name}.bin`);
  for (const p of [out, out + ".rgparts"]) {
    fs.rmSync(p, { recursive: true, force: true });
  }
  const url = `${BASE}?size=${size}${query ? "&" + query : ""}`;
  let r = await run(["get", url, "-o", out, ...args], killAfterMs);
  if (killAfterMs > 0) {
    // interrupted run: resume with the same command
    const resumed = await run(["get", url, "-o", out, ...args]);
    r = { ...resumed, stderr: r.stderr + "\n---resume---\n" + resumed.stderr };
  }
  const exists = fs.existsSync(out);
  const ok = exists && fileHash(out) === expectedHash(size) && r.code === 0;
  check(name, ok, exists ? `${(fs.statSync(out).size / 1048576).toFixed(1)} MB, ${(r.elapsed / 1000).toFixed(1)}s` : "missing output");
  if (parity) {
    const records = logRecordsSince(logStart);
    const parityResult = parity(records);
    check(`${name}-protocol`, parityResult.ok, parityResult.detail);
  }
  return r;
}

// server reachable?
try {
  const res = await fetch(`${BASE}?size=1024`);
  if (!res.ok) throw new Error(String(res.status));
} catch (e) {
  console.error(`test server not reachable at ${BASE}: ${e}`);
  process.exit(2);
}

await scenario("normal-8mb", {
  size: 8 * 1024 * 1024,
  args: ["-n", "8"],
  parity(records) {
    const requests = records.filter((r) => r.event === "request");
    const gets = requests.filter((r) => r.method === "GET");
    const dataGets = gets.filter((r) => r.range !== "bytes=0-0");
    const validHeaders = dataGets.every(
      (r) =>
        r.ua &&
        r.accept === "*/*" &&
        r.acceptEncoding === "identity" &&
        r.referer === "http://127.0.0.1:8765/",
    );
    const ranged = dataGets.filter((r) => r.range);
    const peak = Math.max(0, ...requests.map((r) => r.active || 0));
    const ok = dataGets.length > 1 && ranged.length === dataGets.length && validHeaders && peak <= 8;
    return {
      ok,
      detail: ok
        ? `${dataGets.length} ranged GETs, headers and concurrency <= 8`
        : `unexpected request protocol (${dataGets.length} data GETs, peak ${peak})`,
    };
  },
});
await scenario("throttled-24mb", { size: 24 * 1024 * 1024, query: "rate=512000", args: ["-n", "8", "--limit", "2097152"] });
await scenario("drop-retry-32mb", {
  size: 32 * 1024 * 1024,
  query: "rate=512000&drop=2000000",
  args: ["-n", "8"],
  parity(records) {
    const ranges = records
      .filter((r) => r.event === "request" && r.method === "GET" && r.range)
      .map((r) => r.range);
    const forcedDrops = records.filter((r) => r.event === "forced-drop").length;
    const ok = ranges.length > 8 && forcedDrops > 0;
    return {
      ok,
      detail: ok
        ? `${ranges.length} ranged GETs after ${forcedDrops} forced drops`
        : `expected reconnects, saw ${ranges.length} ranged GETs and ${forcedDrops} forced drops`,
    };
  },
});
await scenario("no-range-8mb", {
  size: 8 * 1024 * 1024,
  query: "norange=1",
  args: ["-n", "8"],
  parity(records) {
    const gets = records.filter((r) => r.event === "request" && r.method === "GET");
    const dataGets = gets.filter((r) => r.range !== "bytes=0-0");
    const ok = dataGets.length === 1 && !dataGets[0].range;
    return {
      ok,
      detail: ok ? "single non-ranged GET fallback" : `expected one non-ranged GET, saw ${dataGets.length}`,
    };
  },
});
await scenario("resume-96mb", { size: 96 * 1024 * 1024, query: "rate=512000", args: ["-n", "8", "--limit", "4194304"], killAfterMs: 3000 });

// --- SHA-256 verification (deliberate improvement: IDM has no content check) ---
{
  const size = 4 * 1024 * 1024;
  const good = expectedHash(size);
  const url = `${BASE}?size=${size}`;

  const out = path.join(OUT, "sha256-match-4mb.bin");
  for (const p of [out, out + ".rgparts"]) fs.rmSync(p, { recursive: true, force: true });
  const r = await run(["get", url, "-o", out, "-n", "4", "--sha256", good]);
  const ok = r.code === 0 && fs.existsSync(out) && fileHash(out) === good;
  check("sha256-match-4mb", ok, r.code === 0 ? "digest verified" : "exit " + r.code);

  const bad = path.join(OUT, "sha256-mismatch-4mb.bin");
  for (const p of [bad, bad + ".rgparts"]) fs.rmSync(p, { recursive: true, force: true });
  const wrong = "0".repeat(64);
  const r2 = await run(["get", url, "-o", bad, "-n", "4", "--sha256", wrong]);
  const rejected = r2.code !== 0 && /sha256 mismatch/i.test(r2.stderr);
  check("sha256-mismatch-4mb", rejected, rejected ? "rejected as expected" : "unexpectedly accepted");
}

const failed = results.filter((r) => !r.ok);
console.log(`\n${results.length - failed.length}/${results.length} scenarios passed`);
process.exit(failed.length ? 1 : 0);
