// Castellum - Ollama privacy egress guard.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// This module is THE privacy gate for Ollama: before any HTTP request goes out,
// `validate_endpoint` verifies that the resolved address is private. A single
// public IP in the resolved set (including IPv4-mapped public IPs) causes
// EndpointRefused — mixed A records are an exfiltration vector.
//
// Only `http://` endpoints are accepted in v1 (we control the server). HTTPS
// Ollama endpoints are deferred to a future slice.
//
// CGNAT (100.64/10) is refused and commented: Tailscale addresses fall in that
// range. When Tailscale support ships, an explicit opt-in flag will be added
// rather than expanding the allow-list silently.

use castellum_providers::AiError;
use std::net::IpAddr;

/// A validated Ollama endpoint. Carries the resolved IP (or the literal IP from
/// the URL) so the HTTP client can pin requests without re-resolving DNS.
#[derive(Debug, Clone)]
pub struct ValidatedEndpoint {
    /// Base URL with the host replaced by the pinned IP literal.
    /// IPv6 addresses are bracketed: `http://[::1]:11434`.
    pub pinned_url: String,
    /// The canonical IP used for pinning.
    pub ip: IpAddr,
    /// Port extracted from the URL (defaulting to 11434 for Ollama).
    pub port: u16,
}

/// Validate `base_url` and resolve its hostname to a private IP.
///
/// `resolve` is injected so tests can drive the DNS step with fake data.
/// Production callers pass `|host| std::net::ToSocketAddrs::to_socket_addrs(&(host, 0))`.
pub fn validate_endpoint(
    base_url: &str,
    resolve: &dyn Fn(&str) -> std::io::Result<Vec<IpAddr>>,
) -> Result<ValidatedEndpoint, AiError> {
    // --- scheme check -------------------------------------------------------
    // v1: http only. https Ollama is deferred (self-signed certs + pinning).
    let without_scheme = if let Some(rest) = base_url.strip_prefix("http://") {
        rest
    } else {
        return Err(AiError::EndpointRefused(format!(
            "only http:// is supported in v1 (got {:?})",
            base_url
        )));
    };

    // --- host + port extraction ---------------------------------------------
    // Handle IPv6 literals in brackets: [::1]:11434
    let (host_str, port) = parse_host_port(without_scheme).ok_or_else(|| {
        AiError::EndpointRefused(format!("cannot parse host/port from {:?}", base_url))
    })?;

    // --- resolve / validate -------------------------------------------------
    let ip: IpAddr = if let Ok(ip) = host_str.parse::<IpAddr>() {
        // IP literal — validate directly, no DNS.
        if !is_private_ip(ip) {
            return Err(AiError::EndpointRefused(format!(
                "IP {} is not a private address; Ollama must run locally",
                ip
            )));
        }
        ip
    } else {
        // Hostname — resolve, then check every returned address.
        let addrs = resolve(host_str).map_err(|e| {
            AiError::EndpointRefused(format!("failed to resolve {:?}: {}", host_str, e))
        })?;
        if addrs.is_empty() {
            return Err(AiError::EndpointRefused(format!(
                "hostname {:?} resolved to zero addresses",
                host_str
            )));
        }
        // All addresses must be private. A single public address is refused —
        // mixed A records are an exfiltration vector.
        for addr in &addrs {
            if !is_private_ip(*addr) {
                return Err(AiError::EndpointRefused(format!(
                    "hostname {:?} resolved to a public address {}; \
                     all resolved IPs must be private",
                    host_str, addr
                )));
            }
        }
        // Use the first address for pinning. They're all private so the choice
        // doesn't affect the security invariant.
        addrs[0]
    };

    // --- build pinned URL ---------------------------------------------------
    let pinned_url = match ip {
        IpAddr::V6(v6) => format!("http://[{}]:{}", v6, port),
        IpAddr::V4(v4) => format!("http://{}:{}", v4, port),
    };

    Ok(ValidatedEndpoint {
        pinned_url,
        ip,
        port,
    })
}

/// Parse `host_str[:port]` from the authority component of an http URL.
/// Returns `(host, port)` where port defaults to 11434 (Ollama default).
fn parse_host_port(authority_and_path: &str) -> Option<(&str, u16)> {
    // Strip any path/query after the authority.
    let authority = authority_and_path
        .split('/')
        .next()
        .unwrap_or(authority_and_path);

    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 bracketed: [::1]:11434 or [::1]
        let close = rest.find(']')?;
        let host = &rest[..close];
        let after = &rest[close + 1..]; // ":11434" or ""
        let port = if let Some(p) = after.strip_prefix(':') {
            p.parse().ok()?
        } else {
            11434
        };
        Some((host, port))
    } else if let Some(colon) = authority.rfind(':') {
        // IPv4 with port, or hostname with port.
        let host = &authority[..colon];
        let port = authority[colon + 1..].parse().ok()?;
        Some((host, port))
    } else {
        // No port: use Ollama default.
        Some((authority, 11434))
    }
}

/// Returns `true` if `ip` is a private/loopback/link-local address that is
/// safe to reach from a local Ollama client.
///
/// Allowed ranges:
///   IPv4: 127.0.0.0/8 (loopback), 10.0.0.0/8, 172.16.0.0/12,
///         192.168.0.0/16, 169.254.0.0/16 (link-local)
///   IPv6: ::1 (loopback), fc00::/7 (ULA), fe80::/10 (link-local),
///         IPv4-mapped (::ffff:) forms of the above IPv4 ranges.
///
/// Refused: all public addresses AND CGNAT 100.64.0.0/10.
/// CGNAT note: Tailscale uses 100.64/10. When Tailscale support ships,
/// add an explicit `allow_tailscale` flag rather than widening this check.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 127.0.0.0/8
            if octets[0] == 127 {
                return true;
            }
            // 10.0.0.0/8
            if octets[0] == 10 {
                return true;
            }
            // 172.16.0.0/12
            if octets[0] == 172 && (octets[1] >= 16 && octets[1] <= 31) {
                return true;
            }
            // 192.168.0.0/16
            if octets[0] == 192 && octets[1] == 168 {
                return true;
            }
            // 169.254.0.0/16 (link-local)
            if octets[0] == 169 && octets[1] == 254 {
                return true;
            }
            // CGNAT 100.64.0.0/10: explicitly refused.
            // (Tailscale opt-in deferred to a future slice.)
            false
        }
        IpAddr::V6(v6) => {
            let segs = v6.segments();
            // ::1 loopback
            if v6.is_loopback() {
                return true;
            }
            // fc00::/7 (ULA): first two bits are 1111 110x
            if segs[0] & 0xfe00 == 0xfc00 {
                return true;
            }
            // fe80::/10 (link-local)
            if segs[0] & 0xffc0 == 0xfe80 {
                return true;
            }
            // IPv4-mapped: ::ffff:a.b.c.d
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn fake_resolve_single(ip: IpAddr) -> impl Fn(&str) -> std::io::Result<Vec<IpAddr>> {
        move |_| Ok(vec![ip])
    }

    fn fake_resolve_multi(ips: Vec<IpAddr>) -> impl Fn(&str) -> std::io::Result<Vec<IpAddr>> {
        move |_| Ok(ips.clone())
    }

    // --- is_private_ip ------------------------------------------------------

    #[test]
    fn egress_allows_loopback_rfc1918_ula_and_linklocal() {
        // IPv4 loopback
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("127.255.255.255".parse().unwrap()));
        // RFC 1918
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("10.255.255.255".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("172.31.255.255".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
        assert!(is_private_ip("192.168.255.255".parse().unwrap()));
        // IPv4 link-local
        assert!(is_private_ip("169.254.0.1".parse().unwrap()));
        // IPv6 loopback
        assert!(is_private_ip("::1".parse().unwrap()));
        // IPv6 ULA
        assert!(is_private_ip("fc00::1".parse().unwrap()));
        assert!(is_private_ip("fdff:ffff:ffff:ffff::1".parse().unwrap()));
        // IPv6 link-local
        assert!(is_private_ip("fe80::1".parse().unwrap()));
        // IPv4-mapped private
        assert!(is_private_ip("::ffff:127.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("::ffff:192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn egress_refuses_public_v4_v6_cgnat_and_v4mapped_public() {
        // Public IPv4
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
        assert!(!is_private_ip("172.15.255.255".parse().unwrap())); // just before 172.16
        assert!(!is_private_ip("172.32.0.0".parse().unwrap())); // just after 172.31
                                                                // CGNAT (Tailscale)
        assert!(!is_private_ip("100.64.0.1".parse().unwrap()));
        assert!(!is_private_ip("100.127.255.255".parse().unwrap()));
        // Public IPv6
        assert!(!is_private_ip("2001:db8::1".parse().unwrap()));
        assert!(!is_private_ip("2606:4700::1".parse().unwrap()));
        // IPv4-mapped public
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("::ffff:100.64.0.1".parse().unwrap())); // CGNAT-mapped
    }

    // --- validate_endpoint --------------------------------------------------

    #[test]
    fn egress_refuses_hostname_resolving_to_public() {
        let result = validate_endpoint(
            "http://evil.example.com:11434",
            &fake_resolve_single("8.8.8.8".parse().unwrap()),
        );
        assert!(matches!(result, Err(AiError::EndpointRefused(_))));
    }

    #[test]
    fn egress_refuses_mixed_private_public_resolution() {
        let result = validate_endpoint(
            "http://mixed.local:11434",
            &fake_resolve_multi(vec![
                "192.168.1.1".parse().unwrap(),
                "8.8.8.8".parse().unwrap(), // public — must refuse
            ]),
        );
        assert!(matches!(result, Err(AiError::EndpointRefused(_))));
    }

    #[test]
    fn egress_refuses_https_and_garbage_urls() {
        let resolve = fake_resolve_single("127.0.0.1".parse().unwrap());
        assert!(matches!(
            validate_endpoint("https://localhost:11434", &resolve),
            Err(AiError::EndpointRefused(_))
        ));
        assert!(matches!(
            validate_endpoint("ftp://localhost:11434", &resolve),
            Err(AiError::EndpointRefused(_))
        ));
        assert!(matches!(
            validate_endpoint("not-a-url", &resolve),
            Err(AiError::EndpointRefused(_))
        ));
    }

    #[test]
    fn egress_allows_loopback_url() {
        let result = validate_endpoint(
            "http://127.0.0.1:11434",
            &fake_resolve_single("127.0.0.1".parse().unwrap()),
        );
        let ep = result.expect("loopback should be allowed");
        assert_eq!(ep.port, 11434);
        assert!(ep.pinned_url.contains("127.0.0.1"));
    }

    #[test]
    fn egress_resolves_localhost_hostname_to_loopback() {
        // Simulate: localhost → [127.0.0.1]
        let result = validate_endpoint(
            "http://localhost:11434",
            &fake_resolve_single("127.0.0.1".parse().unwrap()),
        );
        let ep = result.expect("private hostname resolution allowed");
        assert_eq!(ep.ip, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn egress_handles_ipv6_loopback_literal() {
        let result = validate_endpoint(
            "http://[::1]:11434",
            &fake_resolve_single("::1".parse().unwrap()),
        );
        let ep = result.expect("IPv6 loopback allowed");
        assert_eq!(ep.ip, IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert!(ep.pinned_url.contains("[::1]"));
    }

    #[test]
    fn egress_default_port_11434() {
        // No port in URL → defaults to 11434.
        let result = validate_endpoint(
            "http://127.0.0.1",
            &fake_resolve_single("127.0.0.1".parse().unwrap()),
        );
        let ep = result.expect("should succeed");
        assert_eq!(ep.port, 11434);
    }
}
