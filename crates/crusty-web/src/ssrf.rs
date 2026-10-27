//! Protection against server-side request forgery: which URLs and addresses a web tool may
//! reach. Every hop of a fetch goes through `check_url` and `resolve_checked`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use crusty_core::ToolError;
use crusty_core::config::WebConfig;
use url::Url;

/// Checks scheme, credentials, host and the domain lists of `web`.
pub fn check_url(url: &Url, web: &WebConfig) -> Result<(), ToolError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ToolError::Denied(format!("scheme `{}` is not allowed, use http or https", url.scheme())));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::Denied("URLs with credentials are not allowed".into()));
    }
    let host = host_name(url).ok_or_else(|| ToolError::InvalidInput("URL has no host".into()))?;
    if web.blocked_domains.iter().any(|d| domain_matches(&host, d)) {
        return Err(ToolError::Denied(format!("domain `{host}` is blocked")));
    }
    if !web.allowed_domains.is_empty() && !web.allowed_domains.iter().any(|d| domain_matches(&host, d)) {
        return Err(ToolError::Denied(format!("domain `{host}` is not in web.allowed_domains")));
    }
    Ok(())
}

/// Loopback, private, link-local, shared, multicast and reserved addresses, including IPv6
/// forms that embed an IPv4 address.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => is_forbidden_v6(v6),
    }
}

fn is_forbidden_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || o[0] == 0
        // Carrier-grade NAT, 100.64.0.0/10.
        || (o[0] == 100 && (o[1] & 0xc0) == 64)
}

fn is_forbidden_v6(ip: Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible (::a.b.c.d) forms.
    if let Some(v4) = ip.to_ipv4() {
        return is_forbidden_v4(v4);
    }
    let s = ip.segments();
    // NAT64 well-known prefix 64:ff9b::/96 embeds the IPv4 address in the last 32 bits.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0; 4] {
        return is_forbidden_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    let first = s[0];
    ip.is_loopback()
        || ip.is_unspecified()
        || (first & 0xfe00) == 0xfc00
        || (first & 0xffc0) == 0xfe80
        // Site-local fec0::/10 is deprecated but still routable on some networks.
        || (first & 0xffc0) == 0xfec0
        || (first >> 8) == 0xff
}

/// Resolves `host` and returns its addresses. With `block_private`, any forbidden address
/// makes the whole lookup denied, so a name pointing at loopback is refused.
pub async fn resolve_checked(host: &str, port: u16, block_private: bool) -> Result<Vec<SocketAddr>, ToolError> {
    let addrs: Vec<SocketAddr> =
        tokio::net::lookup_host((host, port)).await.map_err(|e| ToolError::Failed(format!("cannot resolve `{host}`: {e}")))?.collect();
    if addrs.is_empty() {
        return Err(ToolError::Failed(format!("`{host}` resolves to no address")));
    }
    if block_private && let Some(addr) = addrs.iter().find(|a| is_forbidden_ip(a.ip())) {
        return Err(ToolError::Denied(format!("`{host}` resolves to {} (private or reserved address)", addr.ip())));
    }
    Ok(addrs)
}

/// Host of `url` in lowercase, without IPv6 brackets or a trailing dot.
pub(crate) fn host_name(url: &Url) -> Option<String> {
    let host = url.host_str()?.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether `host` is `domain` or one of its subdomains. `host` must be lowercase.
pub(crate) fn domain_matches(host: &str, domain: &str) -> bool {
    let domain = domain.trim().trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn forbidden_addresses_are_refused() {
        let forbidden = [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.1.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a9fe:a9fe",
        ];
        for s in forbidden {
            assert!(is_forbidden_ip(ip(s)), "{s} should be forbidden");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        let public = [
            "8.8.8.8",
            "1.1.1.1",
            "172.15.0.1",
            "172.32.0.1",
            "192.167.1.1",
            "100.63.0.1",
            "100.128.0.1",
            "2606:4700::1",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ];
        for s in public {
            assert!(!is_forbidden_ip(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn scheme_credentials_and_domains_are_checked() {
        let web = WebConfig::default();
        let check = |u: &str, web: &WebConfig| check_url(&Url::parse(u).unwrap(), web);
        assert!(matches!(check("file:///etc/passwd", &web), Err(ToolError::Denied(_))));
        assert!(matches!(check("ftp://example.com/x", &web), Err(ToolError::Denied(_))));
        assert!(matches!(check("https://user:pass@example.com/", &web), Err(ToolError::Denied(_))));
        assert!(check("https://example.com/", &web).is_ok());

        let blocked = WebConfig { blocked_domains: vec!["evil.com".into()], ..WebConfig::default() };
        assert!(matches!(check("https://evil.com/", &blocked), Err(ToolError::Denied(_))));
        assert!(matches!(check("https://api.evil.com./", &blocked), Err(ToolError::Denied(_))));
        assert!(check("https://notevil.com/", &blocked).is_ok());

        let allowed = WebConfig { allowed_domains: vec!["docs.rs".into()], ..WebConfig::default() };
        assert!(check("https://docs.rs/x", &allowed).is_ok());
        assert!(check("https://sub.docs.rs/x", &allowed).is_ok());
        assert!(matches!(check("https://notdocs.rs/", &allowed), Err(ToolError::Denied(_))));
        assert!(matches!(check("https://example.com/", &allowed), Err(ToolError::Denied(_))));
    }

    #[tokio::test]
    async fn loopback_names_and_addresses_are_refused_when_blocking() {
        assert!(matches!(resolve_checked("127.0.0.1", 80, true).await, Err(ToolError::Denied(_))));
        assert!(matches!(resolve_checked("localhost", 80, true).await, Err(ToolError::Denied(_))));
        assert!(resolve_checked("127.0.0.1", 80, false).await.is_ok());
    }
}
