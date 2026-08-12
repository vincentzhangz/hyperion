#!/usr/bin/env node
/**
 * Hyperion stress driver: idle targets, nav/eval pressure, overload, WS churn,
 * cleanup, health, Prometheus, RSS, and client-side latency percentiles.
 */
import http from "node:http";
import fs from "node:fs";
import path from "node:path";

import { createCdpClient, withTimeout } from "./cdp-client.mjs";

const MODE = process.env.STRESS_MODE || "smoke";
const HOST = process.env.HOST || "127.0.0.1:9222";
const TOKEN = process.env.HYPERION_AUTH_TOKEN || "";
const ARTIFACT_DIR = process.env.ARTIFACT_DIR || "artifacts/stress";
const ORIGIN_HOST = process.env.STRESS_ORIGIN_HOST || "127.0.0.1";

const defaults = {
  smoke: { targets: 8, extra: 8, holdSecs: 2, navEval: 4, churn: 8 },
  soak: { targets: 2000, extra: 16, holdSecs: 300, navEval: 32, churn: 64 },
  selftest: { targets: 2, extra: 4, holdSecs: 1, navEval: 1, churn: 4 },
};
const cfg = defaults[MODE] || defaults.smoke;
const TARGETS = Number(process.env.STRESS_TARGETS || cfg.targets);
const EXTRA = Number(process.env.STRESS_EXTRA || cfg.extra);
const HOLD_SECS = Number(process.env.STRESS_HOLD_SECS || cfg.holdSecs);
const NAV_EVAL = Number(process.env.STRESS_NAV_EVAL || cfg.navEval);
const CHURN = Number(process.env.STRESS_CHURN || cfg.churn);

const FAIL_P99_MS = Number(process.env.STRESS_FAIL_P99_MS || (MODE === "soak" ? 5000 : 15000));
const FAIL_CLEANUP = Number(process.env.STRESS_FAIL_CLEANUP_REMAINING || 0);
const FAIL_RSS = Number(process.env.STRESS_FAIL_RSS_BYTES || 0);

const latencies = [];
const errors = [];
let overloaded = 0;
const openIds = [];
const { request, wsConnect, wsCommand } = createCdpClient({
  host: HOST,
  token: TOKEN,
  recordLatency: (ms) => latencies.push(ms),
});

function percentile(sorted, p) {
  if (!sorted.length) return 0;
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[idx];
}

function tryJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function startOrigin() {
  return new Promise((resolve) => {
    const server = http.createServer((_req, res) => {
      res.writeHead(200, { "content-type": "text/html" });
      res.end("<!doctype html><title>stress</title><h1>ok</h1>");
    });
    server.listen(0, "0.0.0.0", () => {
      const { port } = server.address();
      resolve({ server, url: `http://${ORIGIN_HOST}:${port}/` });
    });
  });
}

async function openTarget(url) {
  const res = await request("GET", `/json/new?url=${encodeURIComponent(url)}`);
  const msg = res.json?.message || "";
  if (typeof msg === "string" && msg.includes("Overloaded:")) {
    overloaded += 1;
    return null;
  }
  const id = res.json?.id;
  if (!id) {
    errors.push(`open failed: ${res.text.slice(0, 200)}`);
    return null;
  }
  openIds.push(id);
  return id;
}

async function closeAll() {
  const ids = [...openIds];
  openIds.length = 0;
  for (const id of ids) {
    await request("GET", `/json/close/${id}`);
  }
}

async function listPages() {
  const res = await request("GET", "/json/list");
  return Array.isArray(res.json) ? res.json : [];
}

async function scrapeMetrics() {
  const res = await request("GET", "/metrics");
  const gauges = {};
  for (const line of res.text.split("\n")) {
    if (!line || line.startsWith("#")) continue;
    const [name, value] = line.split(" ");
    if (name && value) gauges[name] = Number(value);
  }
  return { text: res.text, gauges };
}

function summarize(extra) {
  const sorted = [...latencies].sort((a, b) => a - b);
  return {
    mode: MODE,
    host: HOST,
    targetsRequested: TARGETS,
    opened: extra.opened,
    overloaded,
    errors,
    latencyMs: {
      p50: percentile(sorted, 50),
      p95: percentile(sorted, 95),
      p99: percentile(sorted, 99),
      n: sorted.length,
    },
    health: extra.health,
    prometheus: extra.prometheus,
    rssBytes: extra.rssBytes,
    remaining: extra.remaining,
    ...extra,
  };
}

function fail(summary, reason) {
  summary.failed = true;
  summary.reason = reason;
  writeArtifacts(summary);
  console.error(reason);
  process.exit(1);
}

function writeArtifacts(summary) {
  fs.mkdirSync(ARTIFACT_DIR, { recursive: true });
  fs.writeFileSync(path.join(ARTIFACT_DIR, "summary.json"), JSON.stringify(summary, null, 2));
  const lines = [
    `Hyperion stress ${MODE}`,
    `opened=${summary.opened} overloaded=${overloaded} remaining=${summary.remaining}`,
    `latency p50/p95/p99 = ${summary.latencyMs.p50}/${summary.latencyMs.p95}/${summary.latencyMs.p99} ms (n=${summary.latencyMs.n})`,
    `rss_bytes=${summary.rssBytes} health=${summary.health}`,
    summary.reason ? `result=FAIL ${summary.reason}` : "result=PASS",
  ];
  fs.writeFileSync(path.join(ARTIFACT_DIR, "summary.txt"), lines.join("\n") + "\n");
  console.log(lines.join("\n"));
}

const origin = await startOrigin();
try {
  const health = await request("GET", "/health/live");
  if (health.status !== 200) {
    fail(summarize({ opened: 0, remaining: -1, health: health.status, prometheus: {}, rssBytes: 0 }), "health/live failed");
  }

  let opened = 0;
  for (let i = 0; i < TARGETS; i++) {
    const id = await openTarget("about:blank");
    if (id) opened += 1;
  }

  for (let i = 0; i < EXTRA; i++) {
    await openTarget("about:blank");
  }
  if (MODE === "selftest" && overloaded < 1) {
    fail(
      summarize({ opened, remaining: -1, health: 200, prometheus: {}, rssBytes: 0 }),
      "selftest expected Overloaded: when opening past the configured target limit",
    );
  }

  const navCount = Math.min(NAV_EVAL, openIds.length);
  for (let i = 0; i < navCount; i++) {
    const id = openIds[i];
    try {
      const { socket, leftover: left0 } = await withTimeout(
        wsConnect(`/devtools/page/${id}`),
        10000,
        "ws connect",
      );
      let leftover = left0;
      ({ leftover } = await withTimeout(
        wsCommand(socket, leftover, 1, "Page.navigate", { url: origin.url }),
        15000,
        "navigate",
      ));
      ({ leftover } = await withTimeout(
        wsCommand(socket, leftover, 2, "Runtime.evaluate", {
          expression: "document.title",
          returnByValue: true,
        }),
        10000,
        "evaluate",
      ));
      socket.end();
    } catch (e) {
      errors.push(`nav/eval ${id}: ${e.message}`);
    }
  }

  for (let i = 0; i < CHURN; i++) {
    const id = openIds[i % Math.max(openIds.length, 1)];
    if (!id) break;
    try {
      const { socket } = await wsConnect(`/devtools/page/${id}`);
      socket.end();
    } catch (e) {
      errors.push(`churn: ${e.message}`);
    }
  }

  if (HOLD_SECS > 0) await new Promise((r) => setTimeout(r, HOLD_SECS * 1000));

  const metrics = await scrapeMetrics();
  const rssBytes = metrics.gauges.hyperion_rss_bytes || 0;
  await closeAll();
  const remaining = (await listPages()).length;
  const summary = summarize({
    opened,
    remaining,
    health: health.status,
    prometheus: metrics.gauges,
    rssBytes,
    metricsText: metrics.text,
  });

  const sorted = [...latencies].sort((a, b) => a - b);
  if (percentile(sorted, 99) > FAIL_P99_MS) {
    fail(summary, `p99 ${percentile(sorted, 99)}ms exceeded ${FAIL_P99_MS}ms`);
  }
  if (remaining > FAIL_CLEANUP) {
    fail(summary, `cleanup left ${remaining} targets (threshold ${FAIL_CLEANUP})`);
  }
  if (FAIL_RSS > 0 && rssBytes > FAIL_RSS) {
    fail(summary, `rss ${rssBytes} exceeded ${FAIL_RSS}`);
  }
  writeArtifacts(summary);
} finally {
  origin.server.close();
}
