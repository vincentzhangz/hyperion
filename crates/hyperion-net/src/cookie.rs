//! A small in-memory cookie jar with per-domain/path scoping (RFC 6265 subset).

use cookie::Cookie;
use std::collections::HashMap;
use url::Url;

#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    /// (domain, path, name) -> cookie value.
    cookies: HashMap<(String, String, String), StoredCookie>,
}

#[derive(Debug, Clone)]
struct StoredCookie {
    value: String,
    secure: bool,
    expires: Option<i64>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a `Set-Cookie` header value and store it, scoped to `url`.
    pub fn set(&mut self, url: &Url, set_cookie: &str) {
        let Ok(cookie) = Cookie::parse(set_cookie.to_owned()) else {
            return;
        };
        let Some(host) = url.host_str() else {
            return;
        };
        let domain = cookie
            .domain()
            .map(|d| d.trim_start_matches('.').to_ascii_lowercase())
            .unwrap_or_else(|| host.to_ascii_lowercase());
        let path = cookie
            .path()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_path(url.path()));
        let expires = cookie
            .expires()
            .and_then(|e| e.datetime())
            .map(|dt| dt.unix_timestamp());
        self.cookies.insert(
            (domain, path, cookie.name().to_owned()),
            StoredCookie {
                value: cookie.value().to_owned(),
                secure: cookie.secure().unwrap_or(false),
                expires,
            },
        );
    }

    /// Return all cookies that apply to `url`, as `(name, value)` pairs.
    pub fn get(&self, url: &Url) -> Vec<(String, String)> {
        let Some(host) = url.host_str() else {
            return Vec::new();
        };
        let host = host.to_ascii_lowercase();
        let path = url.path();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let secure = url.scheme() == "https";

        let mut out: Vec<_> = self
            .cookies
            .iter()
            .filter(|((domain, cpath, _), c)| {
                domain_matches(domain, &host)
                    && path_matches(cpath, path)
                    && (!c.secure || secure)
                    && c.expires.map(|e| e > now).unwrap_or(true)
            })
            .map(|((_, _, name), c)| (name.clone(), c.value.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// All cookies as CDP-style entries.
    pub fn all(&self) -> Vec<CookieEntry> {
        self.cookies
            .iter()
            .map(|((domain, path, name), c)| CookieEntry {
                name: name.clone(),
                value: c.value.clone(),
                domain: domain.clone(),
                path: path.clone(),
                secure: c.secure,
                expires: c.expires,
            })
            .collect()
    }

    /// Explicitly set a cookie (CDP `Network.setCookie`).
    pub fn set_explicit(
        &mut self,
        name: &str,
        value: &str,
        domain: &str,
        path: &str,
        secure: bool,
        expires: Option<i64>,
    ) {
        self.cookies.insert(
            (
                domain.trim_start_matches('.').to_ascii_lowercase(),
                if path.is_empty() {
                    "/".to_owned()
                } else {
                    path.to_owned()
                },
                name.to_owned(),
            ),
            StoredCookie {
                value: value.to_owned(),
                secure,
                expires,
            },
        );
    }

    /// Delete cookies matching the filter. Returns the number removed.
    pub fn delete(
        &mut self,
        name: Option<&str>,
        domain: Option<&str>,
        path: Option<&str>,
    ) -> usize {
        let before = self.cookies.len();
        self.cookies.retain(|(d, p, n), _| {
            if let Some(name) = name
                && n != name
            {
                return true;
            }
            if let Some(domain) = domain {
                let domain = domain.trim_start_matches('.').to_ascii_lowercase();
                if d != &domain {
                    return true;
                }
            }
            if let Some(path) = path
                && p != path
            {
                return true;
            }
            false
        });
        before - self.cookies.len()
    }

    pub fn clear(&mut self) {
        self.cookies.clear();
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

/// A cookie as exposed to CDP / callers.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CookieEntry {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub expires: Option<i64>,
}

fn default_path(path: &str) -> String {
    // RFC 6265: default-path is the directory of the request path.
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(i) => path[..i].to_owned(),
        None => "/".to_owned(),
    }
}

fn domain_matches(domain: &str, host: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_by_domain_and_path() {
        let mut jar = CookieJar::new();
        let base = Url::parse("https://example.com/account/login").unwrap();
        jar.set(&base, "sid=abc; Path=/account; Domain=example.com; Secure");

        let same = Url::parse("https://example.com/account/profile").unwrap();
        let got = jar.get(&same);
        assert_eq!(got, vec![("sid".to_owned(), "abc".to_owned())]);

        let sub = Url::parse("https://sub.example.com/account/x").unwrap();
        assert_eq!(jar.get(&sub).len(), 1);

        let other_path = Url::parse("https://example.com/other").unwrap();
        assert_eq!(jar.get(&other_path).len(), 0);

        let insecure = Url::parse("http://example.com/account/x").unwrap();
        assert_eq!(jar.get(&insecure).len(), 0, "secure cookie on http");
    }
}
