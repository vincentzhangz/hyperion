import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import http from "node:http";
import test from "node:test";

import { createCdpClient, readWsText, withTimeout } from "./cdp-client.mjs";

test("HTTP requests include bearer auth and record latency", async () => {
  let authorization = "";
  const server = http.createServer((req, res) => {
    authorization = req.headers.authorization || "";
    res.writeHead(200, { "content-type": "application/json" });
    res.end('{"ok":true}');
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));

  try {
    const port = server.address().port;
    const latencies = [];
    const client = createCdpClient({
      host: `127.0.0.1:${port}`,
      token: "test-token",
      recordLatency: (ms) => latencies.push(ms),
    });
    const response = await client.request("GET", "/status");

    assert.equal(response.status, 200);
    assert.deepEqual(response.json, { ok: true });
    assert.equal(authorization, "Bearer test-token");
    assert.equal(latencies.length, 1);
    assert.ok(latencies[0] >= 0);
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});

test("withTimeout rejects stalled work with its label", async () => {
  await assert.rejects(
    withTimeout(new Promise(() => {}), 5, "CDP command"),
    /CDP command timed out after 5ms/,
  );
});

test("withTimeout clears its timer after work resolves", () => {
  const moduleUrl = new URL("./cdp-client.mjs", import.meta.url).href;
  const result = spawnSync(
    process.execPath,
    [
      "--input-type=module",
      "--eval",
      `import { withTimeout } from ${JSON.stringify(moduleUrl)}; await withTimeout(Promise.resolve(), 1000, "quick");`,
    ],
    { timeout: 200 },
  );

  assert.equal(result.status, 0, result.stderr?.toString());
});

test("WebSocket reads remove temporary listeners after resolving", async () => {
  const socket = new EventEmitter();
  const frame = Buffer.from([0x81, 0x01, 0x61]);

  for (let index = 0; index < 12; index++) {
    const result = await readWsText(socket, frame);
    assert.equal(result.text, "a");
  }

  assert.equal(socket.listenerCount("data"), 0);
  assert.equal(socket.listenerCount("error"), 0);
});
