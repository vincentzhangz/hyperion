# CDP support matrix

Hyperion implements a **scraping-oriented subset**. It is not Chrome and does not claim protocol completeness.

| Domain | Status | Notes |
|--------|--------|-------|
| Target / Browser | Partial | create/close/attach, browser contexts → profiles |
| Page | Partial | navigate, lifecycle, init scripts; **no** screenshot/PDF/screencast |
| Runtime | Partial | evaluate, callFunctionOn, getProperties, awaitPromise, remote objects |
| DOM | Partial | query, attributes, outerHTML, approximate box model |
| Network | Partial | cookies, UA, headers, events, bounded body cache |
| Fetch | Partial | enable/pause/continue/fulfill/fail with deadline |
| Input | Partial | key/mouse/insertText via JS dispatch; mouse events may name a preview `nodeId` (no pixel hit testing) |
| Emulation | Partial | viewport + UA |
| Log | Partial | console bridge |
| Hyperion.Profile | Yes | list/create/open/close/delete/flush |
| Accessibility / CSS / Debugger / Tracing / Profiler / Memory / Performance | **Not implemented** | methods return `-32601` |

Known non-goals: pixel rendering, full Chrome compatibility, service workers, IndexedDB, extensions, WebRTC, downloads.

The dashboard profile browser and `/preview/live/<id>` share a **DOM/HTML mirror** (annotated `outerHTML` + synthetic events). That is not a Chrome-compatible visual browser: there is no framebuffer, no CSS layout, and no coordinate hit-testing. Existing CDP clients that omit `nodeId` on `Input.dispatchMouseEvent` still click `activeElement` or `body`.

Admission errors use `-32000` with a message prefixed `Overloaded:`.
