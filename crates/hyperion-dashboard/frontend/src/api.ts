export type View = "overview" | "profiles" | "nodes" | "events" | "browser";

export const PREVIEW_NODE_ATTR = "data-hyperion-node-id";
export const IFRAME_SANDBOX = "allow-same-origin";
export const HOVER_THROTTLE_MS = 50;

export type PreviewAction = "hover" | "mousemove" | "focus" | "click" | "text" | "key" | "scroll";

export type ConnectionState = "connecting" | "live" | "reconnecting" | "closed";

export type OverlayKind = "none" | "stale" | "disconnected" | "unavailable";

export interface ProfileRow {
  id?: unknown;
  name?: unknown;
  kind?: unknown;
  owner_node?: unknown;
  local?: unknown;
}

export interface ProfileTarget {
  id: string;
  url: string;
  title: string;
  profileId: string;
}

export interface ProfileDetail {
  id: string;
  name: string;
  kind: string;
  local: boolean;
  owner_node?: string;
  targets: ProfileTarget[];
}

export interface PreviewSnapshot {
  type: "snapshot";
  url: string;
  title?: string;
  html: string;
  generation: number;
  profileId?: string;
}

export interface PreviewAck {
  type: "ack";
  id: number;
  generation: number;
}

export interface PreviewError {
  type: "error";
  id: number | null;
  code: string;
  message: string;
  refresh: boolean;
}

export type PreviewServerMsg = PreviewSnapshot | PreviewAck | PreviewError | { type: string };

export interface PreviewInput {
  type: "input";
  id: number;
  generation: number;
  nodeId?: number;
  action: PreviewAction;
  key?: string;
  text?: string;
  eventType?: string;
}

export interface PreviewHello {
  type: "hello";
  csrf?: string;
  profileId?: string;
}

export interface DashboardError {
  code: string;
  message: string;
}

export async function api<T>(path: string, csrf: string, init: RequestInit = {}): Promise<T> {
  const headers = new Headers(init.headers);
  if (csrf) headers.set("x-csrf-token", csrf);
  const res = await fetch(path, { credentials: "same-origin", ...init, headers });
  if (res.status === 401) {
    const err = new Error("unauthorized");
    (err as Error & { status: number }).status = 401;
    throw err;
  }
  if (!res.ok) throw new Error(await res.text());
  const ct = res.headers.get("content-type") || "";
  return (ct.includes("json") ? res.json() : res.text()) as Promise<T>;
}

export function formatMetric(value: unknown, digits = 1): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "number" && !Number.isInteger(value)) return value.toFixed(digits);
  return String(value);
}

export function profileId(p: { id?: unknown }): string {
  if (typeof p.id === "string") return p.id;
  if (p.id && typeof p.id === "object" && "0" in (p.id as object)) {
    return String((p.id as { 0: string })[0]);
  }
  return "";
}

export function isLocalProfile(p: ProfileRow): boolean {
  if (typeof p.local === "boolean") return p.local;
  return p.owner_node == null || p.owner_node === "";
}

export function dashboardError(value: unknown): DashboardError | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (typeof v.code !== "number" || typeof v.message !== "string") return null;
  const data = v.data;
  const named =
    data && typeof data === "object" && "code" in data ? String((data as { code: unknown }).code) : "";
  return { code: named || "server", message: v.message };
}

export async function apiJson<T>(path: string, csrf: string, init: RequestInit = {}): Promise<T> {
  const data = await api<unknown>(path, csrf, init);
  const err = dashboardError(data);
  if (err) {
    const e = new Error(err.message) as Error & { code: string };
    e.code = err.code;
    throw e;
  }
  return data as T;
}

export function previewWsUrl(targetId: string, loc: Pick<Location, "protocol" | "host"> = location): string {
  const proto = loc.protocol === "https:" ? "wss" : "ws";
  return `${proto}://${loc.host}/preview/live/${encodeURIComponent(targetId)}`;
}

export function serializePreviewHello(csrf: string, profileIdValue: string): PreviewHello {
  const msg: PreviewHello = { type: "hello" };
  if (csrf) msg.csrf = csrf;
  if (profileIdValue) msg.profileId = profileIdValue;
  return msg;
}

export function serializePreviewInput(input: Omit<PreviewInput, "type">): PreviewInput {
  const msg: PreviewInput = {
    type: "input",
    id: input.id,
    generation: input.generation,
    action: input.action,
  };
  if (input.nodeId != null) msg.nodeId = input.nodeId;
  if (input.key != null && input.key !== "") msg.key = input.key;
  if (input.text != null && input.text !== "") msg.text = input.text;
  if (input.eventType != null && input.eventType !== "") msg.eventType = input.eventType;
  return msg;
}

export function parsePreviewMessage(raw: string): PreviewServerMsg | null {
  try {
    const msg = JSON.parse(raw) as PreviewServerMsg;
    if (!msg || typeof msg !== "object" || typeof msg.type !== "string") return null;
    return msg;
  } catch {
    return null;
  }
}

export function overlayForError(err: PreviewError, local: boolean): OverlayKind {
  if (!local) return "unavailable";
  if (err.refresh || err.code === "stale_node" || err.code === "stale_document") return "stale";
  if (err.code === "target_not_found" || err.code === "not_local") return "unavailable";
  return "none";
}

export function nodeIdFromElement(el: Element | null): number | null {
  let cur: Element | null = el;
  while (cur) {
    const raw = cur.getAttribute(PREVIEW_NODE_ATTR);
    if (raw) {
      const n = Number(raw);
      if (Number.isInteger(n) && n > 0) return n;
    }
    cur = cur.parentElement;
  }
  return null;
}

export function bindPreviewDocument(
  doc: Document,
  send: (pending: {
    action: PreviewAction;
    nodeId?: number;
    key?: string;
    text?: string;
    eventType?: string;
  }) => void,
  throttle = createThrottle(HOVER_THROTTLE_MS),
): () => void {
  const onClick = (ev: Event) => {
    ev.preventDefault();
    ev.stopPropagation();
    const nodeId = nodeIdFromElement(ev.target as Element | null);
    if (nodeId == null) return;
    send({ action: "click", nodeId });
  };
  const onSubmit = (ev: Event) => {
    ev.preventDefault();
    ev.stopPropagation();
  };
  const onFocus = (ev: Event) => {
    const nodeId = nodeIdFromElement(ev.target as Element | null);
    if (nodeId == null) return;
    send({ action: "focus", nodeId });
  };
  const onMove = (ev: Event) => {
    const nodeId = nodeIdFromElement(ev.target as Element | null);
    if (nodeId == null) return;
    throttle.run(() => send({ action: "hover", nodeId }));
  };
  const onKey = (ev: Event) => {
    const kev = ev as KeyboardEvent;
    kev.preventDefault();
    const nodeId = nodeIdFromElement(kev.target as Element | null) ?? undefined;
    const eventType = kev.type === "keyup" ? "keyup" : "keydown";
    send({ action: "key", nodeId, key: kev.key, eventType });
    if (kev.type === "keydown" && kev.key.length === 1 && !kev.ctrlKey && !kev.metaKey && !kev.altKey) {
      send({ action: "text", nodeId, text: kev.key });
    }
  };
  const onScroll = (ev: Event) => {
    const nodeId = nodeIdFromElement(ev.target as Element | null);
    if (nodeId == null) return;
    send({ action: "scroll", nodeId });
  };
  doc.addEventListener("click", onClick, true);
  doc.addEventListener("auxclick", onClick, true);
  doc.addEventListener("submit", onSubmit, true);
  doc.addEventListener("focusin", onFocus, true);
  doc.addEventListener("mouseover", onMove, true);
  doc.addEventListener("mousemove", onMove, true);
  doc.addEventListener("keydown", onKey, true);
  doc.addEventListener("keyup", onKey, true);
  doc.addEventListener("scroll", onScroll, true);
  return () => {
    throttle.cancel();
    doc.removeEventListener("click", onClick, true);
    doc.removeEventListener("auxclick", onClick, true);
    doc.removeEventListener("submit", onSubmit, true);
    doc.removeEventListener("focusin", onFocus, true);
    doc.removeEventListener("mouseover", onMove, true);
    doc.removeEventListener("mousemove", onMove, true);
    doc.removeEventListener("keydown", onKey, true);
    doc.removeEventListener("keyup", onKey, true);
    doc.removeEventListener("scroll", onScroll, true);
  };
}

export function createThrottle(ms: number, now: () => number = () => Date.now()): {
  run: (fn: () => void) => void;
  cancel: () => void;
} {
  let last = 0;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let pending: (() => void) | null = null;
  const flush = () => {
    timer = null;
    last = now();
    const fn = pending;
    pending = null;
    if (fn) fn();
  };
  return {
    run(fn: () => void) {
      const remaining = ms - (now() - last);
      if (remaining <= 0) {
        if (timer != null) {
          clearTimeout(timer);
          timer = null;
        }
        pending = null;
        last = now();
        fn();
        return;
      }
      pending = fn;
      if (timer == null) timer = setTimeout(flush, remaining);
    },
    cancel() {
      if (timer != null) clearTimeout(timer);
      timer = null;
      pending = null;
    },
  };
}

export function shouldRestoreScroll(prevUrl: string, nextUrl: string): boolean {
  return prevUrl !== "" && prevUrl === nextUrl;
}

export async function fetchProfileDetail(csrf: string, id: string): Promise<ProfileDetail> {
  return apiJson<ProfileDetail>(`/api/v1/dashboard/profiles/detail?id=${encodeURIComponent(id)}`, csrf);
}

export async function profileAction(csrf: string, action: string, id?: string, name?: string): Promise<unknown> {
  const params = new URLSearchParams({ action });
  if (id) params.set("id", id);
  if (name) params.set("name", name);
  return apiJson(`/api/v1/dashboard/profiles?${params}`, csrf, { method: "POST" });
}

export async function targetAction(
  csrf: string,
  action: "create" | "navigate" | "close" | "activate",
  profileIdValue: string,
  opts: { id?: string; url?: string } = {},
): Promise<Record<string, unknown>> {
  const params = new URLSearchParams({ action, profileId: profileIdValue });
  if (opts.id) params.set("id", opts.id);
  if (opts.url) params.set("url", opts.url);
  return apiJson(`/api/v1/dashboard/targets?${params}`, csrf, { method: "POST" });
}
