//! Network egress guard — SSRF protection for outbound HTTP tools.
//!
//! Tools that fetch arbitrary URLs from the model
//! ([`crate::tools::web`]) route every request through this module
//! before the socket is opened. The guard rejects:
//!
//! - non-`http(s)` schemes (`file://`, `gopher://`, …),
//! - URLs carrying basic-auth credentials (`https://user:pass@host`),
//! - any hostname whose DNS resolution lands on a loopback, link-local,
//!   private RFC1918, carrier-grade NAT, IPv6 ULA, or the AWS/GCP/Azure
//!   metadata range (`169.254.0.0/16`).
//!
//! The blocklist mirrors `nanobot`'s
//! `nanobot/security/network.py:_BLOCKED_NETWORKS` 1:1. Coverage is
//! deliberately broader than just loopback — the prize for an attacker
//! is usually a metadata endpoint (`169.254.169.254`) or an internal
//! service on a private subnet, both of which would slip past a
//! `127.0.0.0/8`-only check.
//!
//! DNS happens once before the request and once after every redirect.
//! The TOCTOU window between resolve-and-validate and reqwest's own
//! resolution is tiny; closing it would mean shipping a custom resolver
//! into the HTTP client, which is more machinery than the threat model
//! warrants for a developer tool.

use std::net::IpAddr;

use thiserror::Error;
use tokio::task;

/// IP networks that resolve-time validation rejects.
///
/// Each entry is `(prefix_string, prefix_len_bits)`. We materialize them
/// into `(IpAddr, u8)` once at module load via [`blocked_networks`], so
/// the per-request check is a fixed-size loop with no allocation.
///
/// Source: nanobot `nanobot/security/network.py:_BLOCKED_NETWORKS`.
const BLOCKED_NETWORK_DEFS: &[(&str, u8)] = &[
    ("0.0.0.0", 8),
    ("10.0.0.0", 8),
    ("100.64.0.0", 10),  // carrier-grade NAT
    ("127.0.0.0", 8),    // loopback
    ("169.254.0.0", 16), // link-local + cloud metadata (AWS/GCP/Azure)
    ("172.16.0.0", 12),
    ("192.168.0.0", 16),
    ("::1", 128),
    ("fc00::", 7),  // IPv6 unique local
    ("fe80::", 10), // IPv6 link-local
];

/// Errors surfaced from the egress guard.
#[derive(Debug, Error)]
pub enum NetworkError {
    /// URL string did not parse.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// Scheme is not `http` or `https`.
    #[error("only http/https URLs are allowed (got `{0}`)")]
    UnsupportedScheme(String),
    /// URL contains userinfo (`https://user:pass@host`).
    #[error("URLs with embedded credentials are not allowed")]
    EmbeddedCredentials,
    /// URL has no hostname.
    #[error("URL is missing a hostname")]
    MissingHost,
    /// `getaddrinfo` failed.
    #[error("cannot resolve `{host}`: {source}")]
    DnsFailed {
        host: String,
        #[source]
        source: std::io::Error,
    },
    /// Hostname resolved to an address inside the blocklist.
    #[error("`{host}` resolves to blocked address {addr}")]
    BlockedAddress { host: String, addr: IpAddr },
}

/// Materialize [`BLOCKED_NETWORK_DEFS`] into parsed `(IpAddr, u8)` pairs.
fn blocked_networks() -> Vec<(IpAddr, u8)> {
    BLOCKED_NETWORK_DEFS
        .iter()
        .map(|(addr, bits)| {
            let ip: IpAddr = addr
                .parse()
                .expect("BLOCKED_NETWORK_DEFS entries are valid IPs");
            (ip, *bits)
        })
        .collect()
}

/// `true` when `addr` falls inside any blocked network.
fn is_blocked(addr: IpAddr) -> bool {
    blocked_networks()
        .iter()
        .any(|(net, bits)| ip_in_network(addr, *net, *bits))
}

/// Bitwise `addr ∈ net/prefix_len`.
///
/// Generic over IPv4/IPv6 to keep the call site small. Prefix lengths
/// outside `0..=128` (or `0..=32` for v4) cannot occur by construction
/// because [`BLOCKED_NETWORK_DEFS`] is internal.
fn ip_in_network(addr: IpAddr, net: IpAddr, prefix_len: u8) -> bool {
    match (addr, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => {
            if prefix_len > 32 {
                return false;
            }
            let a = u32::from_be_bytes(a.octets());
            let n = u32::from_be_bytes(n.octets());
            let mask: u32 = if prefix_len == 0 {
                0
            } else {
                u32::MAX << (32 - prefix_len)
            };
            (a & mask) == (n & mask)
        }
        (IpAddr::V6(a), IpAddr::V6(n)) => {
            if prefix_len > 128 {
                return false;
            }
            let a = u128::from_be_bytes(a.octets());
            let n = u128::from_be_bytes(n.octets());
            let mask: u128 = if prefix_len == 0 {
                0
            } else {
                u128::MAX << (128 - prefix_len)
            };
            (a & mask) == (n & mask)
        }
        _ => false,
    }
}

/// Resolve `host` (literal IP or DNS name) and reject if any returned
/// address is in the blocklist.
///
/// Used both before the initial request (after parsing the user-supplied
/// URL) and after every redirect.
///
/// # Errors
///
/// - [`NetworkError::DnsFailed`] when `getaddrinfo` errors. We fail
///   closed: a hostname we cannot resolve is also a hostname we cannot
///   prove is safe.
/// - [`NetworkError::BlockedAddress`] when *any* resolved address falls
///   inside the blocklist. A multi-record A response with one private
///   IP is enough to reject the whole hostname.
pub async fn validate_resolved_host(host: &str) -> Result<(), NetworkError> {
    // `Url::host_str` keeps IPv6 brackets (`[::1]`); strip them before
    // attempting `IpAddr::parse`.
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        if is_blocked(ip) {
            return Err(NetworkError::BlockedAddress {
                host: host.to_string(),
                addr: ip,
            });
        }
        return Ok(());
    }

    let host_owned = host.to_string();
    // `(host, 0)` asks getaddrinfo for any port — we only care about the
    // address. spawn_blocking keeps the sync resolver off the runtime.
    let addrs = task::spawn_blocking({
        let h = host_owned.clone();
        move || {
            std::net::ToSocketAddrs::to_socket_addrs(&(h.as_str(), 0u16))
                .map(Iterator::collect::<Vec<_>>)
        }
    })
    .await
    .map_err(|join_err| NetworkError::DnsFailed {
        host: host_owned.clone(),
        source: std::io::Error::other(join_err.to_string()),
    })?
    .map_err(|source| NetworkError::DnsFailed {
        host: host_owned.clone(),
        source,
    })?;

    for sock_addr in addrs {
        let ip = sock_addr.ip();
        if is_blocked(ip) {
            return Err(NetworkError::BlockedAddress {
                host: host_owned,
                addr: ip,
            });
        }
    }
    Ok(())
}

/// Pre-flight URL validation: scheme, credentials, hostname presence,
/// and resolved-IP blocklist.
///
/// Call this once on the user-supplied URL before opening the socket.
///
/// # Errors
///
/// See [`NetworkError`]. Any failure here means the URL must not be
/// fetched.
pub async fn validate_url_target(url: &reqwest::Url) -> Result<(), NetworkError> {
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(NetworkError::UnsupportedScheme(other.to_string())),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(NetworkError::EmbeddedCredentials);
    }
    let host = url.host_str().ok_or(NetworkError::MissingHost)?;
    validate_resolved_host(host).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn loopback_v4_is_blocked() {
        assert!(is_blocked("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn loopback_v6_is_blocked() {
        assert!(is_blocked("::1".parse().unwrap()));
    }

    #[test]
    fn metadata_address_is_blocked() {
        assert!(is_blocked("169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn private_rfc1918_is_blocked() {
        assert!(is_blocked("10.1.2.3".parse().unwrap()));
        assert!(is_blocked("192.168.1.1".parse().unwrap()));
        assert!(is_blocked("172.20.0.1".parse().unwrap()));
    }

    #[test]
    fn ipv6_link_local_is_blocked() {
        assert!(is_blocked("fe80::1".parse().unwrap()));
    }

    #[test]
    fn public_ipv4_is_allowed() {
        assert!(!is_blocked(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn public_ipv6_is_allowed() {
        // 2001:4860:4860::8888 — Google public DNS over v6.
        assert!(!is_blocked("2001:4860:4860::8888".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_file_scheme() {
        let url = reqwest::Url::parse("file:///etc/passwd").unwrap();
        let err = validate_url_target(&url).await.unwrap_err();
        assert!(matches!(err, NetworkError::UnsupportedScheme(_)));
    }

    #[tokio::test]
    async fn rejects_embedded_credentials() {
        let url = reqwest::Url::parse("https://user:pass@example.com/").unwrap();
        let err = validate_url_target(&url).await.unwrap_err();
        assert!(matches!(err, NetworkError::EmbeddedCredentials));
    }

    #[tokio::test]
    async fn rejects_literal_loopback() {
        let url = reqwest::Url::parse("http://127.0.0.1:6379/").unwrap();
        let err = validate_url_target(&url).await.unwrap_err();
        assert!(matches!(err, NetworkError::BlockedAddress { .. }));
    }

    #[tokio::test]
    async fn rejects_literal_metadata_endpoint() {
        let url = reqwest::Url::parse("http://169.254.169.254/latest/meta-data").unwrap();
        let err = validate_url_target(&url).await.unwrap_err();
        assert!(matches!(err, NetworkError::BlockedAddress { .. }));
    }

    #[tokio::test]
    async fn rejects_literal_ipv6_loopback() {
        let url = reqwest::Url::parse("http://[::1]/").unwrap();
        let err = validate_url_target(&url).await.unwrap_err();
        assert!(matches!(err, NetworkError::BlockedAddress { .. }));
    }
}
