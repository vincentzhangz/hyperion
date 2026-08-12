#!/usr/bin/env node
/**
 * Create isolated ephemeral profiles and issue one scrape from every profile
 * concurrently. Hyperion's browser task is serialized, so reported end-to-end
 * latency intentionally includes command queueing.
 */
import fs from "node:fs";
import path from "node:path";

import { createCdpClient, withTimeout } from "./cdp-client.mjs";

const HOST = process.env.HOST || "127.0.0.1:9222";
const TOKEN = process.env.HYPERION_AUTH_TOKEN || "";
const PROFILE_COUNT = readPositiveInt("PROFILE_COUNT", 20);
const SCRAPE_URL = process.env.SCRAPE_URL || "https://example.com";
const EXPECT_TITLE = process.env.SCRAPE_EXPECT_TITLE ?? "Example Domain";
const EXPECT_H1 = process.env.SCRAPE_EXPECT_H1 ?? "Example Domain";
const COMMAND_TIMEOUT_MS = readPositiveInt("PROFILE_COMMAND_TIMEOUT_MS", 120_000);
const FAIL_P99_MS = readNonNegativeInt("PROFILE_FAIL_P99_MS", 120_000);
const FAIL_RSS_BYTES = readNonNegativeInt("PROFILE_FAIL_RSS_BYTES", 0);
const MAX_FAILURES = readNonNegativeInt("PROFILE_MAX_FAILURES", 0);
const ARTIFACT_DIR = process.env.ARTIFACT_DIR || "artifacts/stress-profiles";

const transportLatencies = [];
const errors = [];
const profiles = [];
const client = createCdpClient({
  host: HOST,
  token: TOKEN,
  recordLatency: (ms) => transportLatencies.push(ms),
});
let browser = null;
let baselineTargetIds = [];
let baselineContextIds = [];
let healthStatus = 0;
let metricsBefore = {};
let metricsAfter = {};
let results = [];
let fatalError = "";
let cleanup = {
  remainingTargetIds: [],
  remainingContextIds: [],
  errors: [],
};

try {
  const health = await client.request("GET", "/health/live");
  healthStatus = health.status;
  if (health.status !== 200) {
    throw new Error(`/health/live returned ${health.status}`);
  }
  metricsBefore = await readMetrics();
  baselineTargetIds = await listTargetIds();

  browser = await connectState("/devtools/browser/browser");
  baselineContextIds = await getBrowserContextIds(browser);

  for (let index = 0; index < PROFILE_COUNT; index++) {
    const profile = {
      index,
      contextId: "",
      targetId: "",
      connection: null,
    };
    profiles.push(profile);
    try {
      const context = await command(browser, "Target.createBrowserContext", {});
      profile.contextId = requiredString(context.browserContextId, "browserContextId");
      const target = await command(browser, "Target.createTarget", {
        url: "about:blank",
        browserContextId: profile.contextId,
      });
      profile.targetId = requiredString(target.targetId, "targetId");
      profile.connection = await connectState(`/devtools/page/${profile.targetId}`);
    } catch (error) {
      const message = errorMessage(error);
      errors.push(`profile ${index} setup: ${message}`);
    }
  }

  const readyProfiles = profiles.filter(
    (profile) => profile.contextId && profile.targetId && profile.connection,
  );
  let releaseStart;
  const startGate = new Promise((resolve) => {
    releaseStart = resolve;
  });
  const tasks = readyProfiles.map((profile) => scrapeProfile(profile, startGate));
  releaseStart();
  const settled = await Promise.allSettled(tasks);
  results = settled.map((outcome, index) => {
    if (outcome.status === "fulfilled") return outcome.value;
    const profile = readyProfiles[index];
    return {
      index: profile.index,
      contextId: profile.contextId,
      targetId: profile.targetId,
      ok: false,
      latencyMs: 0,
      error: errorMessage(outcome.reason),
    };
  });

  for (const profile of profiles) {
    if (results.some((result) => result.index === profile.index)) continue;
    results.push({
      index: profile.index,
      contextId: profile.contextId,
      targetId: profile.targetId,
      ok: false,
      latencyMs: 0,
      error: "profile setup failed",
    });
  }
  results.sort((a, b) => a.index - b.index);
} catch (error) {
  fatalError = errorMessage(error);
  errors.push(fatalError);
} finally {
  for (const profile of profiles) {
    profile.connection?.socket.end();
  }
  for (const profile of profiles) {
    if (!profile.targetId) continue;
    try {
      await client.request("GET", `/json/close/${encodeURIComponent(profile.targetId)}`);
    } catch (error) {
      cleanup.errors.push(`close target ${profile.targetId}: ${errorMessage(error)}`);
    }
  }
  if (browser) {
    for (const profile of profiles) {
      if (!profile.contextId) continue;
      try {
        await command(browser, "Target.disposeBrowserContext", {
          browserContextId: profile.contextId,
        });
      } catch (error) {
        cleanup.errors.push(`dispose context ${profile.contextId}: ${errorMessage(error)}`);
      }
    }
    try {
      const currentContextIds = await getBrowserContextIds(browser);
      cleanup.remainingContextIds = createdOnly(currentContextIds, baselineContextIds);
    } catch (error) {
      cleanup.errors.push(`list browser contexts: ${errorMessage(error)}`);
    }
    browser.socket.end();
  }
  try {
    const currentTargetIds = await listTargetIds();
    cleanup.remainingTargetIds = createdOnly(currentTargetIds, baselineTargetIds);
  } catch (error) {
    cleanup.errors.push(`list targets: ${errorMessage(error)}`);
  }
  try {
    metricsAfter = await readMetrics();
  } catch (error) {
    cleanup.errors.push(`read final metrics: ${errorMessage(error)}`);
  }
}

const successful = results.filter((result) => result.ok);
const failedProfiles = results.filter((result) => !result.ok);
const profileLatencies = successful.map((result) => result.latencyMs).sort((a, b) => a - b);
const sortedTransport = [...transportLatencies].sort((a, b) => a - b);
const summary = {
  mode: "profiles",
  host: HOST,
  scrapeUrl: SCRAPE_URL,
  profileCountRequested: PROFILE_COUNT,
  profileCountPrepared: profiles.filter((profile) => profile.connection).length,
  succeeded: successful.length,
  failureCount: failedProfiles.length,
  clientConcurrency: PROFILE_COUNT,
  serverExecution: "serialized browser task; latency includes queueing",
  health: healthStatus,
  profileLatencyMs: latencySummary(profileLatencies),
  transportLatencyMs: latencySummary(sortedTransport),
  rssBytesBefore: metricsBefore.hyperion_rss_bytes || 0,
  rssBytesAfter: metricsAfter.hyperion_rss_bytes || 0,
  metricsBefore,
  metricsAfter,
  cleanup,
  fatalError,
  errors,
  results,
};

const failureReasons = [];
if (failedProfiles.length > MAX_FAILURES) {
  failureReasons.push(`${failedProfiles.length} profile failures exceeded ${MAX_FAILURES}`);
}
if (summary.profileLatencyMs.p99 > FAIL_P99_MS) {
  failureReasons.push(
    `profile p99 ${summary.profileLatencyMs.p99}ms exceeded ${FAIL_P99_MS}ms`,
  );
}
if (FAIL_RSS_BYTES > 0 && summary.rssBytesAfter > FAIL_RSS_BYTES) {
  failureReasons.push(`rss ${summary.rssBytesAfter} exceeded ${FAIL_RSS_BYTES}`);
}
if (cleanup.remainingTargetIds.length) {
  failureReasons.push(`leaked targets: ${cleanup.remainingTargetIds.join(",")}`);
}
if (cleanup.remainingContextIds.length) {
  failureReasons.push(`leaked contexts: ${cleanup.remainingContextIds.join(",")}`);
}
if (cleanup.errors.length) {
  failureReasons.push(`${cleanup.errors.length} cleanup errors`);
}
if (fatalError) failureReasons.push(`fatal: ${fatalError}`);

summary.failed = failureReasons.length > 0;
summary.failureReasons = failureReasons;
writeArtifacts(summary);
if (summary.failed) process.exitCode = 1;

async function scrapeProfile(profile, startGate) {
  await startGate;
  const started = Date.now();
  const navigation = await command(profile.connection, "Page.navigate", {
    url: SCRAPE_URL,
  });
  if (navigation.errorText) {
    throw new Error(`navigation failed: ${navigation.errorText}`);
  }
  const evaluation = await command(profile.connection, "Runtime.evaluate", {
    expression:
      "({title: document.title, h1: document.querySelector('h1')?.textContent?.trim() || '', bodyLength: document.body?.textContent?.length || 0, url: location.href})",
    returnByValue: true,
  });
  const value = evaluation.result?.value;
  if (!value || typeof value !== "object") {
    throw new Error("Runtime.evaluate did not return scrape data");
  }
  if (EXPECT_TITLE && value.title !== EXPECT_TITLE) {
    throw new Error(`expected title ${JSON.stringify(EXPECT_TITLE)}, got ${JSON.stringify(value.title)}`);
  }
  if (EXPECT_H1 && value.h1 !== EXPECT_H1) {
    throw new Error(`expected h1 ${JSON.stringify(EXPECT_H1)}, got ${JSON.stringify(value.h1)}`);
  }
  if (!Number.isFinite(value.bodyLength) || value.bodyLength <= 0) {
    throw new Error(`expected non-empty body, got length ${value.bodyLength}`);
  }
  return {
    index: profile.index,
    contextId: profile.contextId,
    targetId: profile.targetId,
    ok: true,
    latencyMs: Date.now() - started,
    title: value.title,
    h1: value.h1,
    bodyLength: value.bodyLength,
    url: value.url,
  };
}

async function connectState(wsPath) {
  const connected = await withTimeout(
    client.wsConnect(wsPath),
    COMMAND_TIMEOUT_MS,
    `connect ${wsPath}`,
  );
  return {
    socket: connected.socket,
    leftover: connected.leftover,
    nextId: 1,
  };
}

async function command(connection, method, params) {
  const id = connection.nextId++;
  const response = await withTimeout(
    client.wsCommand(connection.socket, connection.leftover, id, method, params),
    COMMAND_TIMEOUT_MS,
    method,
  );
  connection.leftover = response.leftover;
  if (response.msg.error) {
    throw new Error(`${method}: ${response.msg.error.message || "CDP error"}`);
  }
  return response.msg.result || {};
}

async function getBrowserContextIds(connection) {
  const result = await command(connection, "Target.getBrowserContexts", {});
  return Array.isArray(result.browserContextIds) ? result.browserContextIds : [];
}

async function listTargetIds() {
  const response = await client.request("GET", "/json/list");
  if (response.status !== 200 || !Array.isArray(response.json)) {
    throw new Error(`/json/list returned ${response.status}`);
  }
  return response.json.map((target) => target.id).filter(Boolean);
}

async function readMetrics() {
  const response = await client.request("GET", "/metrics");
  if (response.status !== 200) {
    throw new Error(`/metrics returned ${response.status}`);
  }
  const gauges = {};
  for (const line of response.text.split("\n")) {
    if (!line || line.startsWith("#")) continue;
    const [name, value] = line.split(" ");
    if (name && value) gauges[name] = Number(value);
  }
  return gauges;
}

function createdOnly(current, baseline) {
  const baselineSet = new Set(baseline);
  return current.filter((id) => !baselineSet.has(id));
}

function latencySummary(sorted) {
  return {
    p50: percentile(sorted, 50),
    p95: percentile(sorted, 95),
    p99: percentile(sorted, 99),
    n: sorted.length,
  };
}

function percentile(sorted, p) {
  if (!sorted.length) return 0;
  const index = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[index];
}

function requiredString(value, name) {
  if (typeof value !== "string" || !value) throw new Error(`missing ${name}`);
  return value;
}

function readPositiveInt(name, fallback) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value <= 0) {
    console.error(`${name} must be a positive integer`);
    process.exit(1);
  }
  return value;
}

function readNonNegativeInt(name, fallback) {
  const value = Number(process.env[name] ?? fallback);
  if (!Number.isInteger(value) || value < 0) {
    console.error(`${name} must be a non-negative integer`);
    process.exit(1);
  }
  return value;
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}

function writeArtifacts(summaryValue) {
  fs.mkdirSync(ARTIFACT_DIR, { recursive: true });
  fs.writeFileSync(
    path.join(ARTIFACT_DIR, "summary.json"),
    `${JSON.stringify(summaryValue, null, 2)}\n`,
  );
  const lines = [
    "Hyperion multi-profile scrape stress",
    `profiles requested/prepared/succeeded/failed = ${summaryValue.profileCountRequested}/${summaryValue.profileCountPrepared}/${summaryValue.succeeded}/${summaryValue.failureCount}`,
    `profile latency p50/p95/p99 = ${summaryValue.profileLatencyMs.p50}/${summaryValue.profileLatencyMs.p95}/${summaryValue.profileLatencyMs.p99} ms`,
    `rss before/after = ${summaryValue.rssBytesBefore}/${summaryValue.rssBytesAfter} bytes`,
    `cleanup targets/contexts/errors = ${summaryValue.cleanup.remainingTargetIds.length}/${summaryValue.cleanup.remainingContextIds.length}/${summaryValue.cleanup.errors.length}`,
    summaryValue.failed
      ? `result=FAIL ${summaryValue.failureReasons.join("; ")}`
      : "result=PASS",
  ];
  fs.writeFileSync(path.join(ARTIFACT_DIR, "summary.txt"), `${lines.join("\n")}\n`);
  console.log(lines.join("\n"));
}
