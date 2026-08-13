import { FormEvent, useCallback, useEffect, useRef, useState } from "react";
import {
  IFRAME_SANDBOX,
  HOVER_THROTTLE_MS,
  bindPreviewDocument,
  createThrottle,
  fetchProfileDetail,
  overlayForError,
  parsePreviewMessage,
  previewWsUrl,
  profileAction,
  serializePreviewHello,
  serializePreviewInput,
  shouldRestoreScroll,
  targetAction,
  type ConnectionState,
  type OverlayKind,
  type PreviewAction,
  type PreviewError,
  type PreviewSnapshot,
  type ProfileDetail,
  type ProfileTarget,
} from "./api";

export interface ProfileBrowserProps {
  csrf: string;
  profileId: string;
  initialTargetId?: string | null;
  onBack: () => void;
}

type Pending = { action: PreviewAction; nodeId?: number; key?: string; text?: string; eventType?: string };

export function ProfileBrowser({ csrf, profileId, initialTargetId, onBack }: ProfileBrowserProps) {
  const [detail, setDetail] = useState<ProfileDetail | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(initialTargetId || null);
  const [urlDraft, setUrlDraft] = useState("");
  const [connection, setConnection] = useState<ConnectionState>("closed");
  const [overlay, setOverlay] = useState<OverlayKind>("none");
  const [snapshot, setSnapshot] = useState<PreviewSnapshot | null>(null);
  const [err, setErr] = useState("");
  const [busy, setBusy] = useState(false);
  const [srcdoc, setSrcdoc] = useState("");

  const iframeRef = useRef<HTMLIFrameElement>(null);
  const wsRef = useRef<WebSocket | null>(null);
  const genRef = useRef(0);
  const reqIdRef = useRef(1);
  const selectedRef = useRef<string | null>(selectedId);
  const snapshotUrlRef = useRef("");
  const scrollRef = useRef({ x: 0, y: 0 });
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const reconnectAttempt = useRef(0);
  const closedRef = useRef(false);
  const hoverThrottle = useRef(createThrottle(HOVER_THROTTLE_MS));

  selectedRef.current = selectedId;

  const loadDetail = useCallback(async () => {
    try {
      const next = await fetchProfileDetail(csrf, profileId);
      setDetail(next);
      setErr("");
      if (!next.local) {
        setOverlay("unavailable");
        return next;
      }
      setOverlay((o) => (o === "unavailable" ? "none" : o));
      setSelectedId((cur) => {
        if (cur && next.targets.some((t) => t.id === cur)) return cur;
        if (initialTargetId && next.targets.some((t) => t.id === initialTargetId)) return initialTargetId;
        return next.targets[0]?.id ?? null;
      });
      return next;
    } catch (e) {
      const code = (e as { code?: string }).code;
      if (code === "not_local") {
        setOverlay("unavailable");
        setDetail({
          id: profileId,
          name: "",
          kind: "",
          local: false,
          targets: [],
        });
        return null;
      }
      setErr(String((e as Error).message || e));
      return null;
    }
  }, [csrf, initialTargetId, profileId]);

  useEffect(() => {
    void loadDetail();
  }, [loadDetail]);

  const sendInput = useCallback(
    (pending: Pending) => {
      const ws = wsRef.current;
      if (!ws || ws.readyState !== WebSocket.OPEN) return;
      const id = reqIdRef.current++;
      ws.send(
        JSON.stringify(
          serializePreviewInput({
            id,
            generation: genRef.current,
            nodeId: pending.nodeId,
            action: pending.action,
            key: pending.key,
            text: pending.text,
            eventType: pending.eventType,
          }),
        ),
      );
    },
    [],
  );

  const attachPreviewListeners = useCallback(
    (doc: Document) => bindPreviewDocument(doc, sendInput, hoverThrottle.current),
    [sendInput],
  );

  useEffect(() => {
    const iframe = iframeRef.current;
    if (!iframe) return;
    let detach: (() => void) | undefined;
    const onLoad = () => {
      detach?.();
      const doc = iframe.contentDocument;
      if (!doc) return;
      if (shouldRestoreScroll(snapshotUrlRef.current, snapshot?.url || snapshotUrlRef.current)) {
        doc.documentElement.scrollLeft = scrollRef.current.x;
        doc.documentElement.scrollTop = scrollRef.current.y;
        if (doc.body) {
          doc.body.scrollLeft = scrollRef.current.x;
          doc.body.scrollTop = scrollRef.current.y;
        }
      }
      detach = attachPreviewListeners(doc);
    };
    iframe.addEventListener("load", onLoad);
    return () => {
      iframe.removeEventListener("load", onLoad);
      detach?.();
    };
  }, [attachPreviewListeners, snapshot?.url, srcdoc]);

  const canInteract = detail?.local === true;

  useEffect(() => {
    closedRef.current = false;
    hoverThrottle.current = createThrottle(HOVER_THROTTLE_MS);
    if (!selectedId || !canInteract) {
      setConnection("closed");
      return;
    }
    const connect = () => {
      if (closedRef.current || selectedRef.current !== selectedId) return;
      setConnection(reconnectAttempt.current === 0 ? "connecting" : "reconnecting");
      const ws = new WebSocket(previewWsUrl(selectedId));
      wsRef.current = ws;
      ws.onopen = () => {
        reconnectAttempt.current = 0;
        setConnection("live");
        setOverlay((o) => (o === "disconnected" ? "none" : o));
        ws.send(JSON.stringify(serializePreviewHello(csrf, profileId)));
      };
      ws.onmessage = (ev) => {
        const msg = parsePreviewMessage(String(ev.data));
        if (!msg) return;
        if (msg.type === "snapshot") {
          const snap = msg as PreviewSnapshot;
          const iframe = iframeRef.current;
          const doc = iframe?.contentDocument;
          if (doc) {
            scrollRef.current = {
              x: doc.documentElement.scrollLeft || doc.body?.scrollLeft || 0,
              y: doc.documentElement.scrollTop || doc.body?.scrollTop || 0,
            };
          }
          const restore = shouldRestoreScroll(snapshotUrlRef.current, snap.url || "");
          if (!restore) scrollRef.current = { x: 0, y: 0 };
          snapshotUrlRef.current = snap.url || "";
          genRef.current = snap.generation || 0;
          setSnapshot(snap);
          setUrlDraft(snap.url || "");
          setSrcdoc(snap.html || "");
          setOverlay((o) => (o === "stale" ? "none" : o));
        } else if (msg.type === "ack") {
          const ack = msg as { generation?: number };
          if (ack.generation) genRef.current = ack.generation;
        } else if (msg.type === "error") {
          const error = msg as PreviewError;
          const next = overlayForError(error, true);
          if (next !== "none") setOverlay(next);
          if (error.refresh) setErr(error.message || "Preview is stale");
        }
      };
      ws.onclose = () => {
        if (wsRef.current === ws) wsRef.current = null;
        if (closedRef.current || selectedRef.current !== selectedId) return;
        setConnection("reconnecting");
        setOverlay("disconnected");
        const delay = Math.min(8000, 400 * 2 ** reconnectAttempt.current);
        reconnectAttempt.current += 1;
        reconnectTimer.current = setTimeout(connect, delay);
      };
      ws.onerror = () => {
        ws.close();
      };
    };
    connect();
    return () => {
      closedRef.current = true;
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current);
      hoverThrottle.current.cancel();
      wsRef.current?.close();
      wsRef.current = null;
    };
  }, [canInteract, csrf, profileId, selectedId]);

  async function onGo(ev: FormEvent) {
    ev.preventDefault();
    const url = urlDraft.trim();
    if (!url) return;
    setBusy(true);
    setErr("");
    try {
      await profileAction(csrf, "open", profileId);
      if (selectedId) {
        await targetAction(csrf, "navigate", profileId, { id: selectedId, url });
      } else {
        const created = await targetAction(csrf, "create", profileId, { url });
        const id = String(created.id || created.targetId || "");
        if (id) setSelectedId(id);
      }
      await loadDetail();
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function onNewTab() {
    setBusy(true);
    setErr("");
    try {
      await profileAction(csrf, "open", profileId);
      const created = await targetAction(csrf, "create", profileId, { url: urlDraft.trim() || "about:blank" });
      const id = String(created.id || created.targetId || "");
      if (id) setSelectedId(id);
      await loadDetail();
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function onCloseTab(id: string) {
    setBusy(true);
    setErr("");
    try {
      await targetAction(csrf, "close", profileId, { id });
      if (selectedId === id) {
        setSelectedId(null);
        setSnapshot(null);
        setSrcdoc("");
      }
      await loadDetail();
    } catch (e) {
      setErr(String((e as Error).message || e));
    } finally {
      setBusy(false);
    }
  }

  async function onSelectTab(t: ProfileTarget) {
    setSelectedId(t.id);
    setUrlDraft(t.url);
    try {
      await targetAction(csrf, "activate", profileId, { id: t.id });
    } catch {
      /* activate is best-effort */
    }
  }

  const local = detail?.local !== false && overlay !== "unavailable";
  const targets = detail?.targets || [];
  const statusLabel =
    overlay === "unavailable"
      ? "unavailable"
      : overlay === "disconnected"
        ? "reconnecting"
        : connection;

  return (
    <section className="flex min-h-[70vh] flex-col gap-3" data-testid="profile-browser">
      <header className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex flex-wrap items-center gap-3">
          <button className="rounded-lg border border-[var(--border)] px-4" onClick={onBack}>
            Back
          </button>
          <div>
            <h2 className="m-0">{detail?.name || "Profile browser"}</h2>
            <p className="m-0 text-[var(--muted)]">
              {profileId}
              {detail?.kind ? ` · ${detail.kind}` : ""}
              {detail?.owner_node ? ` · ${detail.owner_node}` : ""}
            </p>
          </div>
        </div>
        <span
          className={`rounded-full border border-[var(--border)] px-3 py-1 text-sm ${
            statusLabel === "live" ? "text-[var(--ok)]" : "text-[var(--warn)]"
          }`}
          data-testid="preview-connection"
        >
          {statusLabel}
        </span>
      </header>

      {err ? <p className="text-[var(--err)]">{err}</p> : null}

      {!local ? (
        <p className="rounded-lg border border-[var(--border)] bg-[var(--panel)] p-4 text-[var(--muted)]">
          This profile is owned by another node. Interactive preview is unavailable here.
        </p>
      ) : (
        <>
          <p className="m-0 text-sm text-[var(--muted)]">
            Interactive DOM mirror — not pixel-accurate Chrome rendering. Page scripts stay disabled in this
            sandbox.
          </p>
          <div className="flex flex-wrap gap-2" role="tablist" aria-label="Profile targets">
            {targets.map((t) => (
              <div
                key={t.id}
                className={`flex items-center rounded-lg border ${
                  selectedId === t.id ? "border-[var(--accent)] bg-[var(--panel-2)]" : "border-[var(--border)]"
                }`}
              >
                <button
                  role="tab"
                  aria-selected={selectedId === t.id}
                  className="rounded-lg px-3"
                  onClick={() => void onSelectTab(t)}
                >
                  {t.title || t.url || t.id}
                </button>
                <button
                  aria-label={`Close tab ${t.id}`}
                  className="px-2 text-[var(--muted)]"
                  disabled={busy}
                  onClick={() => void onCloseTab(t.id)}
                >
                  ×
                </button>
              </div>
            ))}
            <button
              data-testid="preview-new-tab"
              disabled={busy}
              className="rounded-lg border border-[var(--border)] px-3"
              onClick={() => void onNewTab()}
            >
              New tab
            </button>
          </div>
          <form className="flex flex-wrap gap-2" onSubmit={onGo}>
            <input
              data-testid="preview-url"
              aria-label="Target URL"
              className="min-w-[16rem] flex-1 rounded-lg border border-[var(--border)] bg-[var(--panel-2)] px-3 py-2.5"
              value={urlDraft}
              placeholder="https://"
              onChange={(e) => setUrlDraft(e.target.value)}
            />
            <button
              data-testid="preview-go"
              disabled={busy}
              className="rounded-lg bg-[var(--accent)] px-4 font-semibold text-[#041018]"
              type="submit"
            >
              Go
            </button>
          </form>
          <div className="preview-shell relative min-h-[60vh] overflow-hidden rounded-lg border border-[var(--border)] bg-white">
            <iframe
              ref={iframeRef}
              className="preview-frame min-h-[60vh] w-full bg-white"
              title="Hostile page preview"
              sandbox={IFRAME_SANDBOX}
              srcDoc={srcdoc}
            />
            {overlay === "stale" ? (
              <div className="preview-overlay absolute inset-0 flex items-center justify-center bg-[rgba(11,15,20,0.72)] text-center">
                Snapshot is stale. Waiting for a refresh…
              </div>
            ) : null}
            {overlay === "disconnected" ? (
              <div className="preview-overlay absolute inset-0 flex items-center justify-center bg-[rgba(11,15,20,0.72)] text-center">
                Reconnecting to preview…
              </div>
            ) : null}
          </div>
        </>
      )}
    </section>
  );
}
