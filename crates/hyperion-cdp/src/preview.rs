//! Live content-preview UI: mirrors the current DOM as HTML in a client iframe.

use serde::Deserialize;
use serde_json::{Value, json};

/// Client → server preview WebSocket messages.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PreviewClientMsg {
    Hello {
        csrf: Option<String>,
        #[serde(default, rename = "profileId")]
        profile_id: Option<String>,
    },
    Input {
        id: i64,
        #[serde(default)]
        generation: u64,
        #[serde(default, rename = "nodeId")]
        node_id: Option<i32>,
        action: String,
        #[serde(default)]
        key: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default, rename = "eventType")]
        event_type: Option<String>,
    },
}

pub fn preview_ack(id: i64, generation: u64) -> Value {
    json!({ "type": "ack", "id": id, "generation": generation })
}

pub fn preview_error(id: Option<i64>, code: &str, message: &str, refresh: bool) -> Value {
    json!({
        "type": "error",
        "id": id,
        "code": code,
        "message": message,
        "refresh": refresh,
    })
}

/// Landing page that lists open targets (filled client-side from `/json/list`).
pub const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Hyperion Preview</title>
<style>
  :root { color-scheme: dark light; font-family: ui-sans-serif, system-ui, sans-serif; }
  body { margin: 2rem; max-width: 720px; }
  h1 { font-size: 1.4rem; }
  a { color: #6cb6ff; }
  li { margin: 0.4rem 0; }
  .meta { color: #888; font-size: 0.85rem; }
</style>
</head>
<body>
  <h1>Hyperion live preview</h1>
  <p class="meta">DOM/HTML content mirror (no pixel rendering). Open a target:</p>
  <ul id="targets"><li>loading…</li></ul>
  <script>
    async function refresh() {
      const list = await fetch('/json/list').then(r => r.json());
      const ul = document.getElementById('targets');
      ul.innerHTML = '';
      if (!list.length) {
        ul.innerHTML = '<li>No page targets yet. Create one via CDP <code>Target.createTarget</code> or <code>/json/new?url=…</code>.</li>';
        return;
      }
      for (const t of list) {
        const li = document.createElement('li');
        const a = document.createElement('a');
        a.href = '/preview/' + encodeURIComponent(t.id);
        a.textContent = (t.title || '(untitled)') + ' — ' + t.url;
        li.appendChild(a);
        ul.appendChild(li);
      }
    }
    refresh();
    setInterval(refresh, 2000);
  </script>
</body>
</html>"#;

/// Per-target viewer: sandboxed iframe + console/network panels + live WS.
pub fn viewer_html(target_id: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Preview {target_id}</title>
<style>
  :root {{ color-scheme: dark; font-family: ui-sans-serif, system-ui, sans-serif; }}
  html, body {{ margin: 0; height: 100%; background: #111; color: #eee; }}
  #layout {{ display: grid; grid-template-rows: auto 1fr auto; height: 100%; }}
  header {{ padding: 0.5rem 1rem; background: #1a1a1a; border-bottom: 1px solid #333;
            display: flex; gap: 1rem; align-items: center; font-size: 0.85rem; }}
  header .url {{ flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: #9cf; }}
  #frame-wrap {{ background: #fff; }}
  iframe {{ border: 0; width: 100%; height: 100%; background: #fff; }}
  #panels {{ display: grid; grid-template-columns: 1fr 1fr; height: 180px;
             border-top: 1px solid #333; background: #161616; font-size: 0.75rem; }}
  .panel {{ overflow: auto; padding: 0.4rem 0.6rem; border-right: 1px solid #333; }}
  .panel h3 {{ margin: 0 0 0.3rem; font-size: 0.7rem; text-transform: uppercase; color: #888; }}
  .log-error {{ color: #f66; }}
  .log-warn {{ color: #fc6; }}
  .net-line {{ font-family: ui-monospace, monospace; }}
  .status {{ color: #888; }}
</style>
</head>
<body>
<div id="layout">
  <header>
    <strong>Hyperion</strong>
    <span class="url" id="url">connecting…</span>
    <span class="status" id="status">●</span>
  </header>
  <div id="frame-wrap"><iframe id="frame" sandbox="allow-same-origin"></iframe></div>
  <div id="panels">
    <div class="panel"><h3>Console</h3><div id="console"></div></div>
    <div class="panel"><h3>Network</h3><div id="network"></div></div>
  </div>
</div>
<script>
  const targetId = {target_id_json};
  const frame = document.getElementById('frame');
  const consoleEl = document.getElementById('console');
  const networkEl = document.getElementById('network');
  const urlEl = document.getElementById('url');
  const statusEl = document.getElementById('status');

  function append(el, text, cls) {{
    const div = document.createElement('div');
    if (cls) div.className = cls;
    div.textContent = text;
    el.appendChild(div);
    el.scrollTop = el.scrollHeight;
  }}

  function applySnapshot(msg) {{
    urlEl.textContent = msg.url || '';
    const html = msg.html || '';
    frame.srcdoc = html;
  }}

  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const ws = new WebSocket(proto + '://' + location.host + '/preview/live/' + encodeURIComponent(targetId));
  ws.onopen = () => {{ statusEl.textContent = 'live'; statusEl.style.color = '#6f6'; }};
  ws.onclose = () => {{ statusEl.textContent = 'closed'; statusEl.style.color = '#f66'; }};
  ws.onerror = () => {{ statusEl.textContent = 'error'; statusEl.style.color = '#f66'; }};
  ws.onmessage = (ev) => {{
    let msg;
    try {{ msg = JSON.parse(ev.data); }} catch (e) {{ return; }}
    if (msg.type === 'snapshot') applySnapshot(msg);
    else if (msg.type === 'console') {{
      const cls = msg.level === 'error' ? 'log-error' : (msg.level === 'warning' ? 'log-warn' : '');
      append(consoleEl, '[' + (msg.level || 'log') + '] ' + (msg.text || ''), cls);
    }} else if (msg.type === 'network') {{
      append(networkEl, (msg.method || '') + ' ' + (msg.status || '') + ' ' + (msg.url || ''), 'net-line');
    }}
  }};
</script>
</body>
</html>"#,
        target_id = target_id,
        target_id_json = serde_json::to_string(target_id).unwrap_or_else(|_| "\"\"".to_owned()),
    )
}

/// Build a previewable HTML document with an injected `<base href>` so relative
/// URLs resolve against the page's origin.
pub fn snapshot_with_base(url: &str, outer_html: &str) -> String {
    let base = match url::Url::parse(url) {
        Ok(u) => {
            let mut b = u.clone();
            b.set_path("/");
            b.set_query(None);
            b.set_fragment(None);
            // Prefer the directory of the document URL as base.
            if let Ok(joined) = u.join("./") {
                joined.to_string()
            } else {
                b.to_string()
            }
        }
        Err(_) => url.to_owned(),
    };
    let base_tag = format!("<base href=\"{}\">", html_escape_attr(&base));
    if let Some(idx) = outer_html.to_ascii_lowercase().find("<head") {
        if let Some(gt) = outer_html[idx..].find('>') {
            let insert_at = idx + gt + 1;
            let mut out = String::with_capacity(outer_html.len() + base_tag.len());
            out.push_str(&outer_html[..insert_at]);
            out.push_str(&base_tag);
            out.push_str(&outer_html[insert_at..]);
            return out;
        }
    }
    format!("<!DOCTYPE html><html><head>{base_tag}</head><body>{outer_html}</body></html>")
}

fn html_escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            _ => out.push(c),
        }
    }
    out
}
