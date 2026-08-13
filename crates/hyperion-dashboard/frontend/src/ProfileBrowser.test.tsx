import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { createRoot, type Root } from "react-dom/client";
import { act } from "react";
import { ProfileBrowser } from "./ProfileBrowser";
import {
  bindPreviewDocument,
  createThrottle,
  dashboardError,
  isLocalProfile,
  nodeIdFromElement,
  overlayForError,
  parsePreviewMessage,
  PREVIEW_NODE_ATTR,
  previewWsUrl,
  serializePreviewHello,
  serializePreviewInput,
  shouldRestoreScroll,
} from "./api";

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  static OPEN = 1;
  static CLOSED = 3;
  url: string;
  readyState = 0;
  sent: string[] = [];
  onopen: (() => void) | null = null;
  onclose: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
    queueMicrotask(() => {
      this.readyState = MockWebSocket.OPEN;
      this.onopen?.();
    });
  }
  send(data: string) {
    this.sent.push(data);
  }
  close() {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.();
  }
  emit(msg: unknown) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("preview protocol helpers", () => {
  it("serializes hello and input without empty fields", () => {
    expect(serializePreviewHello("tok", "prof")).toEqual({
      type: "hello",
      csrf: "tok",
      profileId: "prof",
    });
    expect(serializePreviewInput({ id: 1, generation: 4, action: "click", nodeId: 9 })).toEqual({
      type: "input",
      id: 1,
      generation: 4,
      action: "click",
      nodeId: 9,
    });
  });

  it("parses snapshot and stale errors", () => {
    const snap = parsePreviewMessage(
      JSON.stringify({ type: "snapshot", url: "https://example.test/", html: "<p/>", generation: 2 }),
    );
    expect(snap?.type).toBe("snapshot");
    const err = parsePreviewMessage(
      JSON.stringify({ type: "error", id: 3, code: "stale_node", message: "gone", refresh: true }),
    );
    expect(overlayForError(err as { type: "error"; id: number; code: string; message: string; refresh: boolean }, true)).toBe(
      "stale",
    );
  });

  it("builds a same-origin preview websocket URL", () => {
    expect(previewWsUrl("t1", { protocol: "https:", host: "op.example" })).toBe("wss://op.example/preview/live/t1");
  });

  it("walks ancestors for node ids", () => {
    const root = document.createElement("div");
    const child = document.createElement("span");
    root.setAttribute(PREVIEW_NODE_ATTR, "12");
    root.appendChild(child);
    expect(nodeIdFromElement(child)).toBe(12);
    expect(nodeIdFromElement(document.createElement("p"))).toBeNull();
  });

  it("throttles hover bursts and keeps the trailing call", () => {
    vi.useFakeTimers();
    const throttle = createThrottle(50);
    const calls: number[] = [];
    throttle.run(() => calls.push(1));
    throttle.run(() => calls.push(2));
    expect(calls).toEqual([1]);
    vi.advanceTimersByTime(50);
    expect(calls).toEqual([1, 2]);
    vi.useRealTimers();
  });

  it("forwards clicks from annotated nodes and blocks native navigation", () => {
    const sent: Array<{ action: string; nodeId?: number }> = [];
    const root = document.createElement("div");
    const link = document.createElement("a");
    link.setAttribute("href", "https://evil.example/");
    link.setAttribute(PREVIEW_NODE_ATTR, "42");
    link.textContent = "Go";
    root.appendChild(link);
    document.body.appendChild(root);
    const detach = bindPreviewDocument(document, (p) => sent.push(p));
    const ev = new MouseEvent("click", { bubbles: true, cancelable: true });
    link.dispatchEvent(ev);
    expect(ev.defaultPrevented).toBe(true);
    expect(sent).toEqual([{ action: "click", nodeId: 42 }]);
    detach();
    root.remove();
  });

  it("does not restore scroll across navigations", () => {
    expect(shouldRestoreScroll("https://a.test/", "https://a.test/")).toBe(true);
    expect(shouldRestoreScroll("https://a.test/", "https://b.test/")).toBe(false);
  });

  it("treats named dashboard errors as failures", () => {
    expect(
      dashboardError({
        code: -32602,
        message: "profile is owned by other",
        data: { code: "not_local" },
      }),
    ).toEqual({ code: "not_local", message: "profile is owned by other" });
  });

  it("disables remote-owned profiles", () => {
    expect(isLocalProfile({ local: false, owner_node: "n2" })).toBe(false);
    expect(isLocalProfile({ local: true })).toBe(true);
  });
});

describe("ProfileBrowser", () => {
  let root: Root;
  let host: HTMLDivElement;
  const originalFetch = globalThis.fetch;
  const originalWS = globalThis.WebSocket;

  beforeEach(() => {
    host = document.createElement("div");
    document.body.appendChild(host);
    root = createRoot(host);
    MockWebSocket.instances = [];
    // @ts-expect-error test double
    globalThis.WebSocket = MockWebSocket;
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  });

  afterEach(() => {
    act(() => root.unmount());
    host.remove();
    globalThis.fetch = originalFetch;
    globalThis.WebSocket = originalWS;
  });

  it("shows remote profiles as unavailable and does not open a websocket", async () => {
    globalThis.fetch = vi.fn(async () =>
      jsonResponse({ code: -32602, message: "owned elsewhere", data: { code: "not_local" } }),
    ) as typeof fetch;
    await act(async () => {
      root.render(<ProfileBrowser csrf="csrf" profileId="p-remote" onBack={() => {}} />);
    });
    await flush();
    expect(host.textContent).toMatch(/owned by another node/i);
    expect(MockWebSocket.instances).toHaveLength(0);
  });

  it("connects, hellos, and renders a sandboxed srcdoc snapshot", async () => {
    globalThis.fetch = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes("/profiles/detail")) {
        return jsonResponse({
          id: "p1",
          name: "lab",
          kind: "persistent",
          local: true,
          targets: [{ id: "t1", url: "https://example.test/", title: "Example", profileId: "p1" }],
        });
      }
      return jsonResponse({ ok: true });
    }) as typeof fetch;

    await act(async () => {
      root.render(<ProfileBrowser csrf="csrf" profileId="p1" initialTargetId="t1" onBack={() => {}} />);
    });
    await flush();
    await flush();

    const ws = MockWebSocket.instances[0];
    expect(ws).toBeTruthy();
    expect(JSON.parse(ws.sent[0])).toMatchObject({ type: "hello", csrf: "csrf", profileId: "p1" });

    await act(async () => {
      ws.emit({
        type: "snapshot",
        url: "https://example.test/",
        title: "Example",
        generation: 7,
        html: `<html><body><button id="btn" ${PREVIEW_NODE_ATTR}="42">Go</button></body></html>`,
      });
    });

    const iframe = host.querySelector("iframe.preview-frame") as HTMLIFrameElement;
    expect(iframe.getAttribute("sandbox")).toBe("allow-same-origin");
    expect(iframe.getAttribute("sandbox")).not.toContain("allow-scripts");
    expect(iframe.srcdoc).toContain("data-hyperion-node-id");
    expect(JSON.parse(ws.sent[0])).toMatchObject({ type: "hello", csrf: "csrf", profileId: "p1" });
  });

  it("shows a stale overlay when the server asks to refresh", async () => {
    globalThis.fetch = vi.fn(async () =>
      jsonResponse({
        id: "p1",
        name: "lab",
        kind: "persistent",
        local: true,
        targets: [{ id: "t1", url: "about:blank", title: "", profileId: "p1" }],
      }),
    ) as typeof fetch;
    await act(async () => {
      root.render(<ProfileBrowser csrf="csrf" profileId="p1" initialTargetId="t1" onBack={() => {}} />);
    });
    await flush();
    const ws = MockWebSocket.instances[0];
    await act(async () => {
      ws.emit({ type: "error", id: 1, code: "stale_node", message: "node is no longer valid", refresh: true });
    });
    expect(host.textContent).toMatch(/stale/i);
  });

  it("shows reconnecting after the websocket closes", async () => {
    globalThis.fetch = vi.fn(async () =>
      jsonResponse({
        id: "p1",
        name: "lab",
        kind: "persistent",
        local: true,
        targets: [{ id: "t1", url: "about:blank", title: "", profileId: "p1" }],
      }),
    ) as typeof fetch;
    await act(async () => {
      root.render(<ProfileBrowser csrf="csrf" profileId="p1" initialTargetId="t1" onBack={() => {}} />);
    });
    await flush();
    await act(async () => {
      MockWebSocket.instances[0].close();
    });
    expect(host.textContent).toMatch(/reconnecting/i);
  });
});
