import crypto from "node:crypto";
import net from "node:net";

export async function withTimeout(promise, ms, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`${label} timed out after ${ms}ms`)),
          ms,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

export function createCdpClient({ host, token = "", recordLatency = () => {} }) {
  const authHeaders = () => (token ? { Authorization: `Bearer ${token}` } : {});

  async function request(method, urlPath, { body = "", extraHeaders = {} } = {}) {
    const started = Date.now();
    const url = new URL(urlPath, `http://${host}`);
    const res = await fetch(url, {
      method,
      headers: { ...authHeaders(), ...extraHeaders },
      body: body || undefined,
    });
    const text = await res.text();
    recordLatency(Date.now() - started);
    return { status: res.status, text, json: tryJson(text) };
  }

  function wsConnect(wsPath) {
    const { hostname, port } = splitHost(host);
    const key = crypto.randomBytes(16).toString("base64");
    return new Promise((resolve, reject) => {
      const socket = net.connect({ host: hostname, port }, () => {
        const headers = [
          `GET ${wsPath} HTTP/1.1`,
          `Host: ${host}`,
          "Upgrade: websocket",
          "Connection: Upgrade",
          `Sec-WebSocket-Key: ${key}`,
          "Sec-WebSocket-Version: 13",
        ];
        if (token) headers.push(`Authorization: Bearer ${token}`);
        socket.write(headers.join("\r\n") + "\r\n\r\n");
      });
      let buf = Buffer.alloc(0);
      const onData = (chunk) => {
        buf = Buffer.concat([buf, chunk]);
        const idx = buf.indexOf("\r\n\r\n");
        if (idx < 0) return;
        const head = buf.subarray(0, idx).toString("utf8");
        if (!head.startsWith("HTTP/1.1 101")) {
          socket.destroy();
          reject(new Error(`ws handshake failed: ${head.split("\r\n")[0]}`));
          return;
        }
        socket.off("data", onData);
        resolve({ socket, leftover: buf.subarray(idx + 4) });
      };
      socket.on("data", onData);
      socket.on("error", reject);
    });
  }

  async function wsCommand(socket, leftover, id, method, params) {
    const started = Date.now();
    socket.write(maskFrame(JSON.stringify({ id, method, params })));
    for (;;) {
      const frame = await readWsText(socket, leftover);
      leftover = frame.leftover;
      const msg = tryJson(frame.text);
      if (msg && msg.id === id) {
        recordLatency(Date.now() - started);
        return { msg, leftover };
      }
    }
  }

  return { request, wsConnect, wsCommand };
}

function splitHost(host) {
  const url = new URL(`tcp://${host}`);
  return {
    hostname: url.hostname,
    port: Number(url.port || 80),
  };
}

function tryJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function maskFrame(payload) {
  const data = Buffer.from(payload);
  const mask = crypto.randomBytes(4);
  const header = Buffer.alloc(data.length < 126 ? 2 : 4);
  header[0] = 0x81;
  if (data.length < 126) {
    header[1] = 0x80 | data.length;
  } else {
    header[1] = 0x80 | 126;
    header.writeUInt16BE(data.length, 2);
  }
  const masked = Buffer.from(data);
  for (let i = 0; i < masked.length; i++) masked[i] ^= mask[i % 4];
  return Buffer.concat([header, mask, masked]);
}

export function readWsText(socket, leftover) {
  return new Promise((resolve, reject) => {
    let buf = leftover;
    const cleanup = () => {
      socket.off("data", onData);
      socket.off("error", onError);
    };
    const onError = (error) => {
      cleanup();
      reject(error);
    };
    const onData = (chunk) => {
      buf = Buffer.concat([buf, chunk]);
      if (buf.length < 2) return;
      const len7 = buf[1] & 127;
      let offset = 2;
      let length = len7;
      if (len7 === 126) {
        if (buf.length < 4) return;
        length = buf.readUInt16BE(2);
        offset = 4;
      } else if (len7 === 127) {
        if (buf.length < 10) return;
        const wideLength = buf.readBigUInt64BE(2);
        if (wideLength > BigInt(Number.MAX_SAFE_INTEGER)) {
          cleanup();
          reject(new Error("WebSocket frame is too large"));
          return;
        }
        length = Number(wideLength);
        offset = 10;
      }
      if (buf.length < offset + length) return;
      const payload = buf.subarray(offset, offset + length).toString("utf8");
      cleanup();
      resolve({ text: payload, leftover: buf.subarray(offset + length) });
    };
    socket.on("data", onData);
    socket.on("error", onError);
    if (buf.length) onData(Buffer.alloc(0));
  });
}
