export type View = "overview" | "profiles" | "nodes" | "events";

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
