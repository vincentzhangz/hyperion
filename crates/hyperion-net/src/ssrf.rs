//! Outbound URL policy: block loopback, private, link-local, multicast, and
//! cloud metadata destinations unless explicitly allowed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};

use url::Url;

use crate::error::{Error, Result};

/// Cloud metadata and other well-known SSRF targets.
const BLOCKED_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata.google.com",
    "kubernetes.default.svc",
];

pub fn assert_url_allowed(url: &str, allow_private: bool) -> Result<Url> {
    let parsed = Url::parse(url)?;
    assert_parsed_allowed(&parsed, allow_private)?;
    Ok(parsed)
}

pub fn assert_parsed_allowed(parsed: &Url, allow_private: bool) -> Result<()> {
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(Error::Scheme(scheme.to_owned()));
    }
    let host = parsed.host_str().ok_or(Error::NoHost)?;
    let host_l = host.to_ascii_lowercase();
    if BLOCKED_HOSTS.iter().any(|h| host_l == *h) {
        return Err(Error::Blocked(format!("blocked host {host}")));
    }
    if allow_private {
        return Ok(());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(Error::Blocked(format!("blocked address {ip}")));
        }
        return Ok(());
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| Error::Blocked(format!("dns failed for {host}: {e}")))?;
    for addr in addrs {
        if is_blocked_ip(addr.ip()) {
            return Err(Error::Blocked(format!(
                "resolved to blocked address {}",
                addr.ip()
            )));
        }
    }
    Ok(())
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_blocked_v4(v),
        IpAddr::V6(v) => is_blocked_v6(v),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.octets()[0] == 169 && ip.octets()[1] == 254
        || ip.octets() == [169, 254, 169, 254]
        || ip.octets()[0] == 0
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_multicast()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.to_ipv4_mapped().map(is_blocked_v4).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_and_metadata() {
        assert!(assert_url_allowed("http://127.0.0.1/", false).is_err());
        assert!(assert_url_allowed("http://localhost/", false).is_err());
        assert!(assert_url_allowed("http://169.254.169.254/", false).is_err());
        assert!(assert_url_allowed("http://10.0.0.1/", false).is_err());
        assert!(assert_url_allowed("http://metadata.google.internal/", false).is_err());
    }

    #[test]
    fn allows_when_override() {
        assert!(assert_url_allowed("http://127.0.0.1/", true).is_ok());
    }

    #[test]
    fn allows_public_literal() {
        assert!(assert_url_allowed("https://93.184.216.34/", false).is_ok());
    }
}
