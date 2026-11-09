// Castellum - RFC 8058 one-click List-Unsubscribe POST (S44).
//
// This is the codebase's only outbound network call to an arbitrary
// (non-account, non-Ollama) internet host, named entirely by the mail
// itself. It fires ONLY on an explicit, already-confirmed user action (the
// Swift confirmation sheet has already shown the exact destination) — never
// from parsing or display. It is a direct, synchronous call, not queued
// through the operations replay/retry machinery send/move use (see the
// plan's Assumptions §7): a failure is an ordinary error the confirmation
// sheet can show, and the user can simply try again.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

use crate::CoreError;
use castellum_db::Database;
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::DefaultConnector;

/// RFC 8058 §3.1: the POST body is literally this token.
const ONE_CLICK_BODY: &str = "List-Unsubscribe=One-Click";
/// A short, fixed timeout so a slow/hanging third-party host can never wedge
/// the UI thread waiting on this call (it always runs off the main actor via
/// the FFI's usual Task.detached pattern, but a bound still matters).
const TIMEOUT_SECONDS: u64 = 10;

fn db_err<E: std::fmt::Display>(e: E) -> CoreError {
    CoreError::Sync(format!("db: {e}"))
}

#[derive(Serialize)]
struct UnsubscribeResult {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    http_status: Option<u16>,
}

/// Perform the RFC 8058 one-click POST for `message_id`'s stored
/// `list_unsubscribe_url`. Returns `{"status":"sent","http_status":n}` on
/// any 2xx response, `{"status":"unavailable"}` if the message has no
/// one-click target at all (nothing to do — not an error), or a `CoreError`
/// (mapped to `CastellumStatus::Network`) on any transport/non-2xx failure.
pub fn unsubscribe_one_click_json(db: &Database, message_id: &str) -> Result<String, CoreError> {
    let detail = db.get_message_detail(message_id).map_err(db_err)?;
    let Some(detail) = detail else {
        return Err(CoreError::Sync(format!("unknown message id: {message_id}")));
    };
    // castellum-mail only sets one-click alongside an https URL, but the two
    // columns are stored independently (`set_unsubscribe_targets` accepts
    // `one_click` with no URL), so a row lacking the URL is "unavailable",
    // never a panic on data read back from the profile.
    let url = match detail.list_unsubscribe_url {
        Some(url) if detail.list_unsubscribe_one_click => url,
        _ => {
            return serde_json::to_string(&UnsubscribeResult {
                status: "unavailable",
                http_status: None,
            })
            .map_err(|e| CoreError::Sync(format!("serialize unsubscribe result: {e}")));
        }
    };
    let status = post_one_click(&url)
        .map_err(|e| CoreError::Sync(format!("unsubscribe POST failed: {e}")))?;
    serde_json::to_string(&UnsubscribeResult {
        status: "sent",
        http_status: Some(status),
    })
    .map_err(|e| CoreError::Sync(format!("serialize unsubscribe result: {e}")))
}

/// The actual blocking POST, isolated so it is directly unit-testable
/// against a local mock server without a Database at all. Never panics on a
/// transport failure or a non-2xx response — both become `Err`.
fn post_one_click(url: &str) -> Result<u16, String> {
    post_one_click_with(url, true, Box::new(is_public_target))
}

/// True when connecting to `addr` is acceptable for a mail-supplied URL: a
/// globally routable address (IPv6: inside `2000::/3` only), never loopback / RFC1918 / link-local / CGNAT /
/// unique-local / multicast / unspecified, including IPv4 embedded in an
/// IPv6 address (v4-mapped, NAT64 `64:ff9b::/96`, 6to4 `2002::/16`).
pub(crate) fn is_public_target(addr: &SocketAddr) -> bool {
    is_public_ip(addr.ip())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_unspecified()
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
                || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15
                || o[0] >= 240) // 240.0.0.0/4 reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let s = v6.segments();
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                // NAT64: the low 32 bits are the IPv4 destination.
                let v4 =
                    Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
                return is_public_ip(IpAddr::V4(v4));
            }
            if s[0] == 0x2002 {
                // 6to4: bits 16..48 are the IPv4 address.
                let v4 =
                    Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8);
                return is_public_ip(IpAddr::V4(v4));
            }
            if s[..6] == [0, 0, 0, 0, 0, 0] && (s[6] != 0 || s[7] > 1) {
                // Deprecated IPv4-compatible `::a.b.c.d` (`::/96`): some stacks
                // still route it to the embedded IPv4 address.
                let v4 =
                    Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
                return is_public_ip(IpAddr::V4(v4));
            }
            // Allowlist, not denylist (#3472, #3152): only global unicast
            // `2000::/3` is routable on the public Internet. Every other
            // block is reserved, special-purpose or unallocated, so a list of
            // known-bad prefixes can never be complete. The exclusions below
            // then carve the non-public ranges out of 2000::/3 itself (the
            // rest are kept as documentation; they are already outside it).
            if (s[0] & 0xe000) != 0x2000 {
                return false;
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] == 0x2001 && s[1] == 0) // 2001::/32 Teredo (tunnels to an embedded IPv4)
                || (s[0] == 0x2001 && s[1] == 0x0db8) // 2001:db8::/32 documentation
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1) // 64:ff9b:1::/48 local-use NAT64
                || (s[0] & 0xffc0) == 0xfec0 // fec0::/10 deprecated site-local
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (s[0] & 0xffc0) == 0xfe80) // fe80::/10 link-local
        }
    }
}

/// A resolver that keeps only addresses `allow` accepts. ureq calls the
/// resolver for the first hop AND every redirect hop, and connects to exactly
/// the addresses returned, so this validates each hop with no gap between
/// check and connect (no DNS-rebinding window). No allowed address = error.
pub(crate) struct GuardedResolver {
    pub(crate) inner: DefaultResolver,
    pub(crate) allow: Box<dyn Fn(&SocketAddr) -> bool + Send + Sync>,
}

impl std::fmt::Debug for GuardedResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GuardedResolver")
    }
}

impl Resolver for GuardedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let all = self.inner.resolve(uri, config, timeout)?;
        let mut kept = self.inner.empty();
        for a in all.iter().filter(|a| (self.allow)(a)) {
            kept.push(*a);
        }
        if kept.is_empty() {
            // Same variant for "not found" and "blocked": nothing to probe.
            return Err(ureq::Error::HostNotFound);
        }
        Ok(kept)
    }
}

/// `https_only` is always true in production: the stored target is https://
/// by construction (castellum-mail only accepts `https://` List-Unsubscribe
/// URIs), and with `https_only` a redirect to `http://` (downgrade) fails.
/// `allow` filters resolved addresses (production: `is_public_target`, so the
/// first hop and every redirect hop, up to 5, must be publicly routable —
/// SSRF guard, #3152). Tests pass `https_only = false` and a permissive or
/// port-scoped `allow` to talk to plain-HTTP loopback mocks.
fn post_one_click_with(
    url: &str,
    https_only: bool,
    allow: Box<dyn Fn(&SocketAddr) -> bool + Send + Sync>,
) -> Result<u16, String> {
    // ureq 2 followed up to 5 redirects by default; keep that (ureq 3's
    // default is 10). 4xx/5xx remain `Err` (`http_status_as_error` default).
    // `proxy(None)`: ureq 3 would otherwise honour ALL_PROXY/HTTPS_PROXY from
    // the environment (ureq 2 did not); a proxy would also bypass the guard.
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(TIMEOUT_SECONDS)))
        .max_redirects(5)
        .https_only(https_only)
        .proxy(None)
        .build();
    let agent = ureq::Agent::with_parts(
        config,
        DefaultConnector::new(),
        GuardedResolver {
            inner: DefaultResolver::default(),
            allow,
        },
    );
    let response = agent
        .post(url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(ONE_CLICK_BODY)
        .map_err(|e| e.to_string())?;
    Ok(response.status().as_u16())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    /// A one-shot HTTP server: accepts exactly one connection, records the
    /// request, replies with `status_line`, then stops. Returns (addr, a
    /// join handle yielding the recorded request text).
    fn one_shot_server(status_line: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            // Drain the WHOLE request — headers, then the body its
            // Content-Length declares — before answering. A single read() can
            // return just the headers; replying and dropping the socket at
            // that point closes it while the client is still writing its
            // body, and the client then fails reading the response with a
            // transport error. That is what made this test flaky, not the
            // test runner: it is a defect in this helper.
            let mut request: Vec<u8> = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                request.extend_from_slice(&buf[..n]);
                let Some(head_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue; // headers not complete yet
                };
                let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
                let declared: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + declared {
                    break;
                }
            }
            let _ = stream.write_all(status_line.as_bytes());
            let _ = stream.flush();
            String::from_utf8_lossy(&request).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    fn allow_all() -> Box<dyn Fn(&SocketAddr) -> bool + Send + Sync> {
        Box::new(|_| true)
    }

    #[test]
    fn one_click_flag_without_a_stored_url_is_unavailable_not_a_panic() {
        use crate::ai::run::tests::{fresh_db, seed_message};
        let db = fresh_db();
        seed_message(&db, "acc1:INBOX:1");
        db.set_unsubscribe_targets("acc1:INBOX:1", None, None, true)
            .expect("store targets");
        let json = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            unsubscribe_one_click_json(&db, "acc1:INBOX:1")
        }))
        .expect("must not panic")
        .expect("must not error");
        assert_eq!(json, r#"{"status":"unavailable"}"#);
    }

    #[test]
    fn non_public_addresses_are_rejected_and_public_ones_allowed() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "::127.0.0.1",
            "::10.0.0.1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "2001:db8::1",
            "64:ff9b:1::7f00:1",
            "fec0::1",
        ] {
            assert!(!is_public_ip(bad.parse().unwrap()), "{bad} must be blocked");
        }
        for good in [
            "93.184.216.34",
            "8.8.8.8",
            "2606:4700:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                is_public_ip(good.parse().unwrap()),
                "{good} must be allowed"
            );
        }
    }

    /// #3472 follow-up to #3152: IPv6 is an allowlist. Only global unicast
    /// (`2000::/3`) is routable on the public Internet; every other block is
    /// reserved, special-purpose or not yet allocated, and a denylist of the
    /// known-bad ones let all the others through.
    #[test]
    fn ipv6_outside_global_unicast_is_rejected() {
        for bad in [
            "100::1",       // 100::/64 discard-only
            "200::1",       // 0200::/7 deprecated NSAP mapping
            "400::1",       // unallocated
            "1000::1",      // unallocated
            "4000::1",      // unallocated, just above 2000::/3
            "5f00::1",      // SRv6 SIDs
            "8000::1",      // unallocated
            "e000::1",      // unallocated
            "fb00::1",      // unallocated, below unique-local
            "0:0:0:1::1",   // ::/8 reserved, not an IPv4-compatible form
            "64:ff9b:2::1", // not a NAT64 prefix
        ] {
            assert!(!is_public_ip(bad.parse().unwrap()), "{bad} must be blocked");
        }
        for good in [
            "2606:4700:4700::1111",
            "2a00:1450:4009:81f::200e",
            "3fff:ffff::1",     // top of 2000::/3
            "64:ff9b::808:808", // NAT64 to a public IPv4 keeps the IPv4 verdict
            "2002:808:808::",   // 6to4 to a public IPv4
        ] {
            assert!(
                is_public_ip(good.parse().unwrap()),
                "{good} must be allowed"
            );
        }
    }

    #[test]
    fn production_guard_refuses_a_loopback_first_hop_and_connects_nothing() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let result = post_one_click_with(
            &format!("http://127.0.0.1:{port}"),
            false,
            Box::new(is_public_target),
        );
        assert!(result.is_err(), "{result:?}");
        assert!(listener.accept().is_err(), "no connection must be made");
    }

    #[test]
    fn a_redirect_into_a_blocked_address_is_not_followed() {
        // Hop 1 is allowed (its port); it redirects to hop 2 (another
        // loopback port) which the guard rejects: hop 2 sees no connection.
        let target = TcpListener::bind("127.0.0.1:0").expect("bind");
        let target_port = target.local_addr().unwrap().port();
        target.set_nonblocking(true).unwrap();
        let status: &'static str = Box::leak(
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/x\r\nContent-Length: 0\r\n\r\n"
            )
            .into_boxed_str(),
        );
        let (url, handle) = one_shot_server(status);
        let first_port: u16 = url.rsplit(':').next().unwrap().parse().unwrap();
        let result = post_one_click_with(&url, false, Box::new(move |a| a.port() == first_port));
        let _ = handle.join();
        let err = format!("{result:?}");
        assert!(result.is_err(), "{err}");
        // The guard's own refusal, not a ureq "redirect failed" artefact.
        assert!(err.contains("host not found"), "{err}");
        assert!(
            target.accept().is_err(),
            "redirect target must not be contacted"
        );
    }

    #[test]
    fn posts_the_rfc_8058_body_and_content_type() {
        let (url, handle) = one_shot_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let result = post_one_click_with(&url, false, allow_all());
        assert!(result.is_ok(), "{result:?}");
        let request = handle.join().unwrap();
        assert!(request.starts_with("POST "), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{request}"
        );
        assert!(request.contains("List-Unsubscribe=One-Click"), "{request}");
    }

    #[test]
    fn a_non_2xx_response_is_reported_as_an_error_not_a_panic() {
        let (url, _handle) =
            one_shot_server("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");
        let result = post_one_click_with(&url, false, allow_all());
        assert!(result.is_err());
    }

    #[test]
    fn an_unreachable_host_is_reported_as_an_error_not_a_panic() {
        // Port 1 is reserved/unroutable — connection refused, fast.
        let result = post_one_click_with("http://127.0.0.1:1", false, allow_all());
        assert!(result.is_err());
    }

    #[test]
    fn production_mode_refuses_plain_http_and_sends_nothing() {
        // https_only(true) is what makes an https->http redirect downgrade
        // fail; here it is observable as a refused http:// target with no
        // connection ever reaching the mock.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let result = post_one_click(&format!("http://127.0.0.1:{port}"));
        assert!(result.is_err(), "{result:?}");
        assert!(listener.accept().is_err(), "no connection must be made");
    }

    #[test]
    fn env_proxy_is_ignored_and_request_goes_direct() {
        let (url, handle) = one_shot_server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let vars = [
            "ALL_PROXY",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "all_proxy",
            "http_proxy",
            "https_proxy",
        ];
        for v in vars {
            std::env::set_var(v, "http://127.0.0.1:1");
        }
        let result = post_one_click_with(&url, false, allow_all());
        for v in vars {
            std::env::remove_var(v);
        }
        assert!(result.is_ok(), "{result:?}");
        assert!(handle.join().unwrap().starts_with("POST "));
    }
}
