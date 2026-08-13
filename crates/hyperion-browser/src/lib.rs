//! hyperion-browser: browser orchestration.
//!
//! Coordinates networking ([`hyperion_net`]), HTML parsing ([`hyperion_dom`])
//! and the V8 runtime ([`hyperion_js`]). Runs on a Glommio executor thread;
//! JavaScript executes on the per-`Page` worker thread inside [`Page`].

pub mod domjs;
pub mod page;

pub use domjs::{DomBridgeState, NodeIdMap, PageEnv};
pub use page::{InteractionOutcome, Page};

use hyperion_net::client::{HttpClient, HttpClientOptions};
use hyperion_net::error::Result;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// A shared browsing context: owns the cookie jar / HTTP client and the
/// engine configuration. Pages are created from a context via [`navigate`].
///
/// [`navigate`]: BrowserContext::navigate
pub struct BrowserContext {
    client: HttpClient,
    /// Scripts injected before every document's inline scripts.
    init_scripts: std::cell::RefCell<Vec<(String, String)>>,
    next_script_id: std::cell::Cell<u64>,
    viewport: std::cell::RefCell<(f64, f64, f64)>,
    user_agent_override: std::cell::RefCell<Option<String>>,
    pub profile_id: RefCell<String>,
    local_storage: Rc<RefCell<HashMap<String, HashMap<String, String>>>>,
}

impl BrowserContext {
    pub fn new(options: HttpClientOptions) -> Self {
        Self {
            client: HttpClient::new(options),
            init_scripts: std::cell::RefCell::new(Vec::new()),
            next_script_id: std::cell::Cell::new(1),
            viewport: std::cell::RefCell::new((980.0, 1024.0, 1.0)),
            user_agent_override: std::cell::RefCell::new(None),
            profile_id: RefCell::new("default".into()),
            local_storage: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn local_storage(&self) -> Rc<RefCell<HashMap<String, HashMap<String, String>>>> {
        self.local_storage.clone()
    }

    pub fn set_local_storage(&self, store: HashMap<String, HashMap<String, String>>) {
        *self.local_storage.borrow_mut() = store;
    }

    /// The underlying HTTP client (cookies and options live here).
    pub fn client(&self) -> &HttpClient {
        &self.client
    }

    pub fn add_init_script(&self, source: &str) -> String {
        let id = format!("script-{}", self.next_script_id.get());
        self.next_script_id.set(self.next_script_id.get() + 1);
        self.init_scripts
            .borrow_mut()
            .push((id.clone(), source.to_owned()));
        id
    }

    pub fn remove_init_script(&self, id: &str) {
        self.init_scripts.borrow_mut().retain(|(sid, _)| sid != id);
    }

    pub fn set_viewport(&self, width: f64, height: f64, dpr: f64) {
        *self.viewport.borrow_mut() = (width, height, dpr);
    }

    pub fn viewport(&self) -> (f64, f64, f64) {
        *self.viewport.borrow()
    }

    pub fn set_user_agent_override(&self, ua: Option<String>) {
        *self.user_agent_override.borrow_mut() = ua;
    }

    pub fn user_agent_override(&self) -> Option<String> {
        self.user_agent_override.borrow().clone()
    }

    /// Fetch `url`, parse the HTML into a DOM, and run its inline scripts.
    ///
    /// Must be awaited from a Glommio executor thread (the transport drives
    /// glommio sockets).
    pub async fn navigate(&self, url: &str) -> Result<Page> {
        let resp = self.client.fetch(url).await?;
        let html = String::from_utf8_lossy(&resp.body).into_owned();
        let dom = hyperion_dom::parse_html(&html);

        let mut page = Page::new(url.to_owned(), resp.status.as_u16(), dom);
        page.attach_local_storage(self.local_storage.clone());
        let (w, h, dpr) = self.viewport();
        page.set_viewport(w, h, dpr);
        if let Some(ua) = self.user_agent_override() {
            page.set_user_agent(&ua);
        }
        let scripts: Vec<String> = self
            .init_scripts
            .borrow()
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        page.set_init_scripts(scripts);
        page.run_inline_scripts();
        Ok(page)
    }
}
