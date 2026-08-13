import { FormEvent, useCallback, useEffect, useState } from "react";
import { api, formatMetric, isLocalProfile, profileId, type View } from "./api";
import { ProfileBrowser } from "./ProfileBrowser";

type Metrics = Record<string, number>;

export function App() {
  const [view, setView] = useState<View>("overview");
  const [token, setToken] = useState("");
  const [csrf, setCsrf] = useState("");
  const [needLogin, setNeedLogin] = useState(true);
  const [data, setData] = useState<Record<string, unknown> | null>(null);
  const [q, setQ] = useState("");
  const [err, setErr] = useState("");
  const [stale, setStale] = useState(false);
  const [browserProfileId, setBrowserProfileId] = useState<string | null>(null);
  const [browserTargetId, setBrowserTargetId] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    if (view === "browser") return;
    try {
      let path = "/api/v1/dashboard/overview";
      if (view === "profiles") {
        path =
          "/api/v1/dashboard/profiles?q=" +
          encodeURIComponent(q) +
          "&offset=0&limit=100";
      }
      if (view === "nodes") path = "/api/v1/dashboard/nodes";
      if (view === "events") path = "/api/v1/dashboard/events";
      const next = await api<Record<string, unknown>>(path, csrf);
      setData(next);
      setStale(!!next.stale);
      setErr("");
      setNeedLogin(false);
    } catch (e) {
      const status = (e as { status?: number }).status;
      if (status === 401) {
        setNeedLogin(true);
        return;
      }
      setErr(String((e as Error).message || e));
    }
  }, [csrf, q, view]);

  useEffect(() => {
    api("/api/v1/dashboard/overview", "")
      .then((d) => {
        setNeedLogin(false);
        setData(d as Record<string, unknown>);
      })
      .catch(() => setNeedLogin(true));
  }, []);

  useEffect(() => {
    if (!needLogin) void load();
  }, [view, needLogin, load]);

  useEffect(() => {
    const onVis = () => {
      if (!document.hidden && !needLogin) void load();
    };
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, [load, needLogin]);

  async function login(ev: FormEvent) {
    ev.preventDefault();
    setErr("");
    try {
      const res = await fetch("/api/v1/dashboard/login", {
        method: "POST",
        headers: {
          "content-type": "application/json",
          authorization: "Bearer " + token,
        },
        body: JSON.stringify({ token }),
        credentials: "same-origin",
      });
      if (!res.ok) throw new Error("login failed");
      const j = (await res.json()) as { csrf: string };
      setCsrf(j.csrf);
      setNeedLogin(false);
      await load();
    } catch (e) {
      setErr(String((e as Error).message || e));
    }
  }

  async function profileAction(action: string, id?: string, name?: string) {
    setBusy(true);
    setErr("");
    try {
      const params = new URLSearchParams({ action });
      if (id) params.set("id", id);
      if (name) params.set("name", name);
      await api(`/api/v1/dashboard/profiles?${params}`, csrf, { method: "POST" });
      await load();
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  if (needLogin) {
    return (
      <form className="mx-auto mt-[12vh] max-w-sm rounded-xl border border-[var(--border)] bg-[var(--panel)] p-6" onSubmit={login}>
        <h1 className="mt-0 text-xl font-semibold">Sign in</h1>
        <p className="text-[var(--muted)]">Exchange the operator bearer token for a session cookie.</p>
        <label className="mt-4 block">
          <span className="sr-only">Bearer token</span>
          <input
            type="password"
            autoComplete="current-password"
            aria-label="Bearer token"
            className="mt-2 w-full rounded-lg border border-[var(--border)] bg-[var(--panel-2)] px-3 py-2.5"
            onChange={(e) => setToken(e.target.value)}
          />
        </label>
        <button type="submit" className="mt-4 min-h-11 rounded-lg bg-[var(--accent)] px-4 font-semibold text-[#041018]">
          Continue
        </button>
        {err ? <p className="text-[var(--err)]">{err}</p> : null}
      </form>
    );
  }

  const metrics = ((data?.metrics as Metrics) || {}) as Metrics;
  const pages = (data?.pages as Array<Record<string, string>>) || [];
  const profiles = Array.isArray(data?.profiles)
    ? (data?.profiles as Array<Record<string, unknown>>)
    : [];
  const workers = (data?.workers as Array<Record<string, unknown>>) || [];
  const catalogNodes = (data?.nodes as Array<string>) || [];
  const events = (data?.events as Array<Record<string, string>>) || [];

  const nav = (id: View, label: string) => (
    <a
      href="#"
      aria-current={view === id ? "page" : undefined}
      className={`block rounded-lg px-3 py-2.5 no-underline ${view === id ? "bg-[var(--panel-2)]" : "text-[var(--text)]"}`}
      onClick={(e) => {
        e.preventDefault();
        setView(id);
      }}
    >
      {label}
    </a>
  );

  return (
    <div className="grid min-h-screen grid-cols-1 md:grid-cols-[240px_1fr]">
      <nav className="flex gap-2 overflow-auto border-b border-[var(--border)] bg-[var(--panel)] p-4 md:flex-col md:border-b-0 md:border-r" aria-label="Primary">
        <h1 className="m-0 mb-4 text-base tracking-wide">Hyperion</h1>
        {nav("overview", "Overview")}
        {nav("profiles", "Profiles")}
        {nav("nodes", "Nodes / workers")}
        {nav("events", "Events")}
      </nav>
      <main className="overflow-auto p-6">
        {err ? <p className="text-[var(--err)]">{err}</p> : null}
        {stale ? (
          <p className="text-[var(--warn)]">Partial/stale cluster data — some nodes unreachable.</p>
        ) : null}

        {view === "browser" && browserProfileId ? (
          <ProfileBrowser
            csrf={csrf}
            profileId={browserProfileId}
            initialTargetId={browserTargetId}
            onBack={() => {
              setView("profiles");
              setBrowserTargetId(null);
            }}
          />
        ) : null}

        {view === "overview" && (
          <>
            <header className="mb-4 flex items-center justify-between gap-3">
              <h2 className="m-0">Cluster overview</h2>
              <button className="rounded-lg border border-[var(--border)] bg-transparent px-4" onClick={() => void load()}>
                Refresh
              </button>
            </header>
            <div className="grid grid-cols-[repeat(auto-fill,minmax(180px,1fr))] gap-3">
              <Card k="Open targets" v={metrics.targets_open} />
              <Card k="Active" v={metrics.targets_active} cls="text-[var(--ok)]" />
              <Card k="Idle" v={metrics.targets_idle} />
              <Card k="Profiles" v={metrics.profiles_loaded} />
              <Card k="Sessions" v={metrics.sessions} />
              <Card k="Queue" v={metrics.queue_depth} cls={metrics.queue_depth > 0 ? "text-[var(--warn)]" : ""} />
              <Card k="Rejected" v={metrics.admission_rejected} cls={metrics.admission_rejected ? "text-[var(--err)]" : ""} />
              <Card k="Nav p-avg ms" v={formatMetric(metrics.nav_latency_ms_avg)} />
              <Card k="Fetch waits" v={metrics.fetch_pauses} />
              <Card k="Fetch timeouts" v={metrics.fetch_timeouts} cls={metrics.fetch_timeouts ? "text-[var(--err)]" : ""} />
              <Card k="HTTP reuse" v={metrics.http_pool_reuse} />
              <Card k="Shutdown" v={metrics.shutdown} cls={metrics.shutdown ? "text-[var(--warn)]" : "text-[var(--ok)]"} />
            </div>
            <h3 className="mt-8">Pages</h3>
            {pages.length === 0 ? (
              <p className="text-[var(--muted)]">No open pages.</p>
            ) : (
              <table className="w-full border-collapse">
                <thead>
                  <tr>
                    {["Id", "Title", "URL", "Preview"].map((h) => (
                      <th key={h} className="border-b border-[var(--border)] px-2 py-2.5 text-left text-xs uppercase text-[var(--muted)]">
                        {h}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {pages.map((p) => (
                    <tr key={p.id}>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{p.id}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{p.title}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{p.url}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">
                        <button
                          className="rounded-lg bg-[var(--accent)] px-3 font-semibold text-[#041018]"
                          disabled={!p.profileId}
                          onClick={() => {
                            if (!p.profileId) return;
                            setBrowserProfileId(p.profileId);
                            setBrowserTargetId(p.id);
                            setView("browser");
                          }}
                        >
                          Open
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}

        {view === "profiles" && (
          <>
            <header className="mb-4 flex flex-wrap items-center justify-between gap-3">
              <h2 className="m-0">Profiles</h2>
              <div className="flex flex-wrap items-center gap-2">
                <input
                  aria-label="Search profiles"
                  placeholder="Search"
                  value={q}
                  className="rounded-lg border border-[var(--border)] bg-[var(--panel-2)] px-3 py-2.5"
                  onChange={(e) => setQ(e.target.value)}
                />
                <button className="rounded-lg bg-[var(--accent)] px-4 font-semibold text-[#041018]" onClick={() => void load()}>
                  Search
                </button>
                <button
                  disabled={busy}
                  className="rounded-lg border border-[var(--border)] px-4"
                  onClick={() => void profileAction("create", undefined, q || "profile")}
                >
                  Create
                </button>
              </div>
            </header>
            {profiles.length === 0 ? (
              <p className="text-[var(--muted)]">No profiles.</p>
            ) : (
              <table className="w-full border-collapse">
                <thead>
                  <tr>
                    {["Name", "Id", "Kind", "Owner", "Actions"].map((h) => (
                      <th key={h} className="border-b border-[var(--border)] px-2 py-2.5 text-left text-xs uppercase text-[var(--muted)]">
                        {h}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {profiles.map((p) => {
                    const id = profileId(p);
                    const local = isLocalProfile(p);
                    return (
                      <tr key={id}>
                        <td className="border-b border-[var(--border)] px-2 py-2.5">{String(p.name || "")}</td>
                        <td className="border-b border-[var(--border)] px-2 py-2.5">{id}</td>
                        <td className="border-b border-[var(--border)] px-2 py-2.5">{String(p.kind || "")}</td>
                        <td className="border-b border-[var(--border)] px-2 py-2.5">{String(p.owner_node || "—")}</td>
                        <td className="border-b border-[var(--border)] px-2 py-2.5">
                          <div className="flex flex-wrap gap-2">
                            <button
                              data-testid="open-browser"
                              className="rounded-lg bg-[var(--accent)] px-3 font-semibold text-[#041018] disabled:opacity-40"
                              disabled={!local}
                              title={local ? "Open interactive DOM mirror" : "Remote-owned profile"}
                              onClick={() => {
                                setBrowserProfileId(id);
                                setBrowserTargetId(null);
                                setView("browser");
                              }}
                            >
                              Open browser
                            </button>
                            <button className="rounded-lg border border-[var(--border)] px-3" onClick={() => void profileAction("flush", id)}>
                              Flush
                            </button>
                            <button
                              className="rounded-lg bg-[var(--err)] px-3 font-semibold text-[#1a0506]"
                              onClick={() => {
                                if (window.confirm(`Delete profile ${id}?`)) void profileAction("delete", id);
                              }}
                            >
                              Delete
                            </button>
                          </div>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
          </>
        )}

        {view === "nodes" && (
          <>
            <h2>Nodes / workers</h2>
            {stale ? (
              <p className="text-[var(--warn)]">Catalog node list is partial or unavailable.</p>
            ) : null}
            {catalogNodes.length > 0 ? (
              <p className="text-[var(--muted)]">Catalog nodes: {catalogNodes.join(", ")}</p>
            ) : (
              <p className="text-[var(--muted)]">No catalog heartbeats yet. Local worker list follows.</p>
            )}
            {workers.length === 0 ? (
              <p className="text-[var(--muted)]">Single-node process. Worker list is local.</p>
            ) : (
              <table className="w-full border-collapse">
                <thead>
                  <tr>
                    {["Worker", "Bind", "Targets", "Queue"].map((h) => (
                      <th key={h} className="border-b border-[var(--border)] px-2 py-2.5 text-left text-xs uppercase text-[var(--muted)]">
                        {h}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {workers.map((w) => (
                    <tr key={String(w.id)}>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{String(w.id)}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{String(w.bind || "")}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{String(w.targets ?? "")}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{String(w.queue ?? "")}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}

        {view === "events" && (
          <>
            <h2>Events</h2>
            {events.length === 0 ? (
              <p className="text-[var(--muted)]">No operational events.</p>
            ) : (
              <table className="w-full border-collapse">
                <thead>
                  <tr>
                    {["Time", "Level", "Message", "Correlation"].map((h) => (
                      <th key={h} className="border-b border-[var(--border)] px-2 py-2.5 text-left text-xs uppercase text-[var(--muted)]">
                        {h}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {events.map((e) => (
                    <tr key={`${e.ts_ms}-${e.correlation_id}`}>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{e.ts_ms}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{e.level}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{e.message}</td>
                      <td className="border-b border-[var(--border)] px-2 py-2.5">{e.correlation_id}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </>
        )}
      </main>
    </div>
  );
}

function Card({ k, v, cls }: { k: string; v: unknown; cls?: string }) {
  return (
    <div className="rounded-[10px] border border-[var(--border)] bg-[var(--panel)] p-3.5">
      <div className="text-xs uppercase tracking-wider text-[var(--muted)]">{k}</div>
      <div className={`mt-1 text-[22px] tabular-nums ${cls || ""}`}>{formatMetric(v)}</div>
    </div>
  );
}
