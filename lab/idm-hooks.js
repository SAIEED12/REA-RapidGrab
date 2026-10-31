// Frida hooks for observing IDM's download engine behavior.
// Logs: HTTP request/response text over plain sockets and OpenSSL, socket connects,
// file create/write patterns, and process spawns.

const LOG_LIMIT = 8192;

function clean(s) {
  if (!s) return s;
  return s.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, ".");
}

function tryRead(bufPtr, len) {
  try {
    if (!bufPtr || len <= 0) return null;
    const n = Math.min(len, LOG_LIMIT);
    return clean(bufPtr.readUtf8String(n) || "");
  } catch (e) {
    try { return clean(bufPtr.readCString()); } catch (e2) { return null; }
  }
}

function interestingHttp(s) {
  return s && /(^|\n)(GET |HEAD |POST |Range:|HTTP\/1\.)/i.test(s);
}

function log(obj) {
  send(JSON.stringify(obj));
}

// ---------- sockets ----------
function hookSocketExports() {
  const ws2 = Process.findModuleByName("WS2_32.dll");
  if (!ws2) { log({ ev: "warn", msg: "WS2_32.dll not loaded yet" }); return; }

  const sendFn = ws2.findExportByName("send");
  if (sendFn) Interceptor.attach(sendFn, {
    onEnter(args) {
      const s = tryRead(args[1], args[2].toInt32());
      if (interestingHttp(s)) log({ ev: "send", len: args[2].toInt32(), data: s });
    }
  });

  const wsaSend = ws2.findExportByName("WSASend");
  if (wsaSend) Interceptor.attach(wsaSend, {
    onEnter(args) {
      try {
        const n = args[2].toInt32();
        const first = args[1];
        for (let i = 0; i < Math.min(n, 2); i++) {
          const len = first.add(i * 8).readU32();
          const buf = first.add(i * 8 + 4).readPointer();
          const s = tryRead(buf, len);
          if (interestingHttp(s)) log({ ev: "wsasend", len, data: s });
        }
      } catch (e) {}
    }
  });

  const recvFn = ws2.findExportByName("recv");
  if (recvFn) Interceptor.attach(recvFn, {
    onEnter(args) { this.buf = args[1]; },
    onLeave(ret) {
      const n = ret.toInt32();
      if (n > 0) {
        const s = tryRead(this.buf, n);
        if (interestingHttp(s)) log({ ev: "recv", len: n, data: s });
      }
    }
  });

  const wsaRecv = ws2.findExportByName("WSARecv");
  if (wsaRecv) Interceptor.attach(wsaRecv, {
    onEnter(args) {
      this.bufArr = args[1];
      this.cnt = args[2].toInt32();
      this.bytesPtr = args[3];
    },
    onLeave(ret) {
      try {
        const n = this.bytesPtr.readU32();
        if (n > 0 && this.cnt > 0) {
          const len = this.bufArr.readU32();
          const buf = this.bufArr.add(4).readPointer();
          const s = tryRead(buf, Math.min(len, n));
          if (interestingHttp(s)) log({ ev: "wsarecv", len: n, data: s });
        }
      } catch (e) {}
    }
  });

  const connectFn = ws2.findExportByName("connect");
  if (connectFn) Interceptor.attach(connectFn, {
    onEnter(args) {
      try {
        const fam = args[1].readU16();
        if (fam === 2) {
          const port = (args[1].add(2).readU8() << 8) | args[1].add(3).readU8();
          const ip = [0, 1, 2, 3].map(i => args[1].add(4 + i).readU8()).join(".");
          log({ ev: "connect", ip, port });
        }
      } catch (e) {}
    }
  });
}

// ---------- TLS ----------
function hookSsl() {
  const mod = Process.findModuleByName("libssl.dll") || Process.findModuleByName("libssl-3.dll");
  if (!mod) { log({ ev: "info", msg: "libssl not loaded" }); return; }
  const w = mod.findExportByName("SSL_write");
  const r = mod.findExportByName("SSL_read");
  if (w) Interceptor.attach(w, {
    onEnter(args) {
      const s = tryRead(args[1], args[2].toInt32());
      if (interestingHttp(s)) log({ ev: "ssl-write", len: args[2].toInt32(), data: s });
    }
  });
  if (r) Interceptor.attach(r, {
    onEnter(args) { this.buf = args[1]; },
    onLeave(ret) {
      const n = ret.toInt32();
      if (n > 0) {
        const s = tryRead(this.buf, n);
        if (interestingHttp(s)) log({ ev: "ssl-read", len: n, data: s });
      }
    }
  });
  log({ ev: "info", msg: "libssl hooked", path: mod.path });
}

// ---------- files ----------
function hookFiles() {
  const k32 = Process.findModuleByName("kernel32.dll");
  if (!k32) return;

  const createW = k32.findExportByName("CreateFileW");
  if (createW) Interceptor.attach(createW, {
    onEnter(args) {
      try {
        const p = args[0].readUtf16String();
        if (p && /\.part|\.tmp|idm|test/i.test(p)) log({ ev: "createfileW", path: p });
      } catch (e) {}
    }
  });

  const createA = k32.findExportByName("CreateFileA");
  if (createA) Interceptor.attach(createA, {
    onEnter(args) {
      try {
        const p = args[0].readCString();
        if (p && /\.part|\.tmp|idm|test/i.test(p)) log({ ev: "createfileA", path: p });
      } catch (e) {}
    }
  });

  const writeFn = k32.findExportByName("WriteFile");
  if (writeFn) {
    const counts = {};
    Interceptor.attach(writeFn, {
      onEnter(args) {
        try {
          const h = args[0].toString();
          counts[h] = (counts[h] || 0) + 1;
          const c = counts[h];
          if (c <= 3 || c % 200 === 0) {
            log({ ev: "writefile", handle: h, len: args[2].toInt32(), count: c });
          }
        } catch (e) {}
      }
    });
  }

  const cpW = k32.findExportByName("CreateProcessW");
  if (cpW) Interceptor.attach(cpW, {
    onEnter(args) {
      try {
        const cmd = args[1].readUtf16String();
        if (cmd) log({ ev: "createprocessW", cmd });
      } catch (e) {}
    }
  });
}

function main() {
  log({ ev: "script-loaded", modules: Process.enumerateModules().length });
  hookSocketExports();
  hookSsl();
  hookFiles();
  log({ ev: "hooks-installed" });
}

main();
