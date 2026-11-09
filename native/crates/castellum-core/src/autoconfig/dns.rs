// Castellum - DNS side of account auto-detect (MM-14, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two parts:
//   * `parse_answer`: a hand-written, bounds-checked decoder for a raw DNS
//     response (RFC 1035) that extracts SRV and MX records. It never panics
//     and never allocates more than the packet can hold: every read is
//     checked, name compression pointers are hop-limited, and anything
//     malformed yields an empty result. Fuzzed by
//     `native/fuzz/fuzz_targets/dns_answer_decode.rs`.
//   * `SystemDns`: the production `DnsTransport`. On macOS it calls the
//     system resolver (`res_query` from libresolv, which honours VPN and split
//     DNS) on a worker thread so a slow resolver is abandoned at the lookup
//     deadline. Other targets get a stub that reports "no records".
//
// Only the DOMAIN (or a `_service._tcp.<domain>` name built from it) is ever
// put on the wire; the user's address never reaches this module.

use super::{DnsError, DnsTransport, SrvRecord};
use std::time::Instant;

pub(crate) const TYPE_MX: u16 = 15;
pub(crate) const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;
/// Compression-pointer hops allowed while decoding one name. A legitimate
/// name needs at most a couple; a loop is an attack.
const MAX_POINTER_HOPS: usize = 16;
/// RFC 1035 §2.3.4: a name is at most 255 octets on the wire.
const MAX_NAME_LEN: usize = 255;
/// Hard cap on answer records inspected, so a packet stuffed with tiny
/// records cannot make the decoder do unbounded work.
const MAX_RECORDS: usize = 64;

/// What `parse_answer` extracted from a response.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ParsedAnswer {
    pub srv: Vec<SrvRecord>,
    /// `(preference, exchange)` pairs, unsorted.
    pub mx: Vec<(u16, String)>,
}

fn be16(buf: &[u8], at: usize) -> Option<u16> {
    let b = buf.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// Decode a (possibly compressed) domain name starting at `start`. Returns the
/// lower-cased dotted name (`"."` for the root) and the offset just past the
/// name's FIRST encoding (i.e. where the next field begins).
pub(crate) fn read_name(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut pos = start;
    let mut resume: Option<usize> = None;
    let mut hops = 0usize;
    loop {
        let len = *buf.get(pos)? as usize;
        match len {
            0 => {
                pos += 1;
                break;
            }
            l if l & 0xC0 == 0xC0 => {
                let lo = *buf.get(pos + 1)? as usize;
                let target = ((l & 0x3F) << 8) | lo;
                if resume.is_none() {
                    resume = Some(pos + 2);
                }
                hops += 1;
                if hops > MAX_POINTER_HOPS || target >= buf.len() {
                    return None;
                }
                pos = target;
            }
            l if l & 0xC0 != 0 => return None, // 0x40/0x80 label types are not valid here
            l => {
                let label = buf.get(pos + 1..pos + 1 + l)?;
                if !out.is_empty() {
                    out.push('.');
                }
                for &b in label {
                    // Hostnames and service labels only; anything exotic
                    // makes the whole name unusable rather than escaped.
                    if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                        return None;
                    }
                    out.push(b.to_ascii_lowercase() as char);
                }
                if out.len() > MAX_NAME_LEN {
                    return None;
                }
                pos += 1 + l;
            }
        }
    }
    if out.is_empty() {
        out.push('.');
    }
    Some((out, resume.unwrap_or(pos)))
}

/// Decode a raw DNS response. Malformed or truncated input returns whatever
/// was decoded before the fault (usually nothing) and never panics.
///
/// `expected` is the name that was queried: an answer record whose owner name
/// differs is ignored (so a CNAME'd answer, whose owner is the alias target, is
/// skipped by design) (DNS is unauthenticated, so a stray or spoofed record for
/// some other name must not be taken as the answer).
pub(crate) fn parse_answer(buf: &[u8], expected: &str) -> ParsedAnswer {
    let mut result = ParsedAnswer::default();
    let expected = expected.trim_end_matches('.').to_ascii_lowercase();
    let _ = parse_inner(buf, &expected, &mut result);
    result
}

fn parse_inner(buf: &[u8], expected: &str, out: &mut ParsedAnswer) -> Option<()> {
    if buf.len() < 12 {
        return None;
    }
    let qd = be16(buf, 4)? as usize;
    let an = (be16(buf, 6)? as usize).min(MAX_RECORDS);
    let mut pos = 12usize;
    for _ in 0..qd.min(MAX_RECORDS) {
        let (_, next) = read_name(buf, pos)?;
        pos = next.checked_add(4)?; // QTYPE + QCLASS
        if pos > buf.len() {
            return None;
        }
    }
    for _ in 0..an {
        let (owner, next) = read_name(buf, pos)?;
        let rtype = be16(buf, next)?;
        let class = be16(buf, next + 2)?;
        let rdlen = be16(buf, next + 8)? as usize;
        let rdata = next.checked_add(10)?;
        let end = rdata.checked_add(rdlen)?;
        if end > buf.len() {
            return None;
        }
        if class == CLASS_IN && owner == expected {
            match rtype {
                TYPE_SRV if rdlen >= 7 => {
                    let (target, after) = read_name(buf, rdata + 6)?;
                    if after > end {
                        pos = end; // the target name runs past its own RDATA: drop it
                        continue;
                    }
                    out.srv.push(SrvRecord {
                        priority: be16(buf, rdata)?,
                        weight: be16(buf, rdata + 2)?,
                        port: be16(buf, rdata + 4)?,
                        target,
                    });
                }
                TYPE_MX if rdlen >= 3 => {
                    let (host, after) = read_name(buf, rdata + 2)?;
                    if after > end {
                        pos = end;
                        continue;
                    }
                    out.mx.push((be16(buf, rdata)?, host));
                }
                _ => {}
            }
        }
        pos = end;
    }
    Some(())
}

/// The production resolver. See the module header.
pub struct SystemDns;

impl DnsTransport for SystemDns {
    fn srv(&self, name: &str, deadline: Instant) -> Result<Vec<SrvRecord>, DnsError> {
        query(name, TYPE_SRV, deadline).map(|a| a.srv)
    }

    fn mx(&self, domain: &str, deadline: Instant) -> Result<Vec<String>, DnsError> {
        let mut records = query(domain, TYPE_MX, deadline)?.mx;
        records.sort_by_key(|(pref, _)| *pref);
        Ok(records.into_iter().map(|(_, host)| host).collect())
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::*;
    use std::ffi::{c_char, c_int, CString};
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    #[link(name = "resolv")]
    extern "C" {
        // <resolv.h> `#define res_query res_9_query`.
        #[link_name = "res_9_query"]
        fn res_query(
            dname: *const c_char,
            class: c_int,
            rtype: c_int,
            answer: *mut u8,
            anslen: c_int,
        ) -> c_int;
    }
    extern "C" {
        // <netdb.h> `extern int h_errno;` (libSystem).
        #[allow(non_upper_case_globals)]
        static h_errno: c_int;
    }
    const HOST_NOT_FOUND: c_int = 1;
    const NO_DATA: c_int = 4;

    /// `res_query` keeps its result state in globals, so calls are serialised.
    /// An abandoned (timed out) call keeps the guard until the resolver's own
    /// timeout expires; meanwhile later queries fail fast as "unavailable".
    static RESOLVER: Mutex<()> = Mutex::new(());

    pub(super) fn query(
        name: &str,
        rtype: u16,
        deadline: Instant,
    ) -> Result<ParsedAnswer, DnsError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DnsError::Unavailable);
        }
        let owned_name = name.to_string();
        let Ok(cname) = CString::new(name) else {
            return Ok(ParsedAnswer::default());
        };
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("castellum-autoconfig-dns".into())
            .spawn(move || {
                // Never queue behind a wedged resolver call: if another query
                // still holds the lock, report "unavailable" and exit, so
                // abandoned threads cannot pile up.
                let _guard = match RESOLVER.try_lock() {
                    Ok(g) => g,
                    Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        let _ = tx.send(Err(DnsError::Unavailable));
                        return;
                    }
                };
                let mut buf = vec![0u8; 4096];
                // SAFETY: `cname` is NUL-terminated and outlives the call;
                // `buf` is a live allocation of exactly `anslen` bytes.
                let n = unsafe {
                    res_query(
                        cname.as_ptr(),
                        CLASS_IN as c_int,
                        rtype as c_int,
                        buf.as_mut_ptr(),
                        buf.len() as c_int,
                    )
                };
                let reply = if n > 0 {
                    buf.truncate((n as usize).min(4096));
                    Ok(parse_answer(&buf, &owned_name))
                } else {
                    // SAFETY: plain int global, read under RESOLVER's lock.
                    match unsafe { h_errno } {
                        HOST_NOT_FOUND | NO_DATA => Ok(ParsedAnswer::default()),
                        _ => Err(DnsError::Unavailable),
                    }
                };
                let _ = tx.send(reply);
            });
        if spawned.is_err() {
            return Err(DnsError::Unavailable);
        }
        match rx.recv_timeout(remaining.max(Duration::from_millis(1))) {
            Ok(reply) => reply,
            Err(_) => Err(DnsError::Unavailable),
        }
    }
}

#[cfg(target_os = "macos")]
use sys::query;

/// Non-macOS targets (CI Linux builds) have no system-resolver binding: the
/// DNS sources report "no records" and the HTTP sources still work.
#[cfg(not(target_os = "macos"))]
fn query(_name: &str, _rtype: u16, _deadline: Instant) -> Result<ParsedAnswer, DnsError> {
    Ok(ParsedAnswer::default())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Encode `name` as uncompressed labels.
    pub(crate) fn enc_name(name: &str) -> Vec<u8> {
        let mut v = Vec::new();
        if name != "." {
            for label in name.split('.') {
                v.push(label.len() as u8);
                v.extend_from_slice(label.as_bytes());
            }
        }
        v.push(0);
        v
    }

    /// Build a response with one question and the given `(type, rdata)` answers
    /// whose owner name is a compression pointer to the question name.
    pub(crate) fn packet(qname: &str, answers: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut p = vec![
            0x12,
            0x34,
            0x81,
            0x80,
            0,
            1,
            0,
            answers.len() as u8,
            0,
            0,
            0,
            0,
        ];
        p.extend(enc_name(qname));
        p.extend([0, 0, 0, 1]);
        for (t, rdata) in answers {
            p.extend([0xC0, 12]);
            p.extend(t.to_be_bytes());
            p.extend([0, 1, 0, 0, 0, 60]);
            p.extend((rdata.len() as u16).to_be_bytes());
            p.extend(rdata);
        }
        p
    }

    pub(crate) fn srv_rdata(prio: u16, weight: u16, port: u16, target: &str) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend(prio.to_be_bytes());
        r.extend(weight.to_be_bytes());
        r.extend(port.to_be_bytes());
        r.extend(enc_name(target));
        r
    }

    #[test]
    fn parses_srv_and_mx_records() {
        let mut mx = 10u16.to_be_bytes().to_vec();
        mx.extend(enc_name("Mail.Example.COM"));
        let p = packet(
            "_imaps._tcp.example.com",
            &[
                (TYPE_SRV, srv_rdata(1, 5, 993, "imap.example.com")),
                (TYPE_MX, mx),
            ],
        );
        let a = parse_answer(&p, "_imaps._tcp.example.com");
        assert_eq!(
            a.srv,
            vec![SrvRecord {
                priority: 1,
                weight: 5,
                port: 993,
                target: "imap.example.com".into()
            }]
        );
        assert_eq!(a.mx, vec![(10, "mail.example.com".to_string())]);
    }

    #[test]
    fn srv_target_may_use_a_compression_pointer() {
        // Target = pointer to the question name at offset 12.
        let mut rdata = srv_rdata(0, 0, 993, "x");
        rdata.truncate(6);
        rdata.extend([0xC0, 12]);
        let p = packet("_imaps._tcp.example.com", &[(TYPE_SRV, rdata)]);
        assert_eq!(
            parse_answer(&p, "_imaps._tcp.example.com").srv[0].target,
            "_imaps._tcp.example.com"
        );
    }

    #[test]
    fn root_target_decodes_as_dot() {
        let p = packet(
            "_imaps._tcp.example.com",
            &[(TYPE_SRV, srv_rdata(0, 0, 0, "."))],
        );
        assert_eq!(
            parse_answer(&p, "_imaps._tcp.example.com").srv[0].target,
            "."
        );
    }

    #[test]
    fn truncated_answers_never_panic_at_any_cut_point() {
        let p = packet(
            "_imaps._tcp.example.com",
            &[(TYPE_SRV, srv_rdata(1, 5, 993, "imap.example.com"))],
        );
        for cut in 0..p.len() {
            let _ = parse_answer(&p[..cut], "_imaps._tcp.example.com"); // must not panic
        }
        assert!(parse_answer(&p[..p.len() - 3], "_imaps._tcp.example.com")
            .srv
            .is_empty());
    }

    #[test]
    fn compression_loop_is_rejected() {
        // Question name is a pointer to itself.
        let mut p = vec![0, 1, 0x81, 0x80, 0, 1, 0, 0, 0, 0, 0, 0];
        p.extend([0xC0, 12]);
        p.extend([0, 0, 0, 1]);
        assert_eq!(parse_answer(&p, "x.test"), ParsedAnswer::default());
        assert!(read_name(&[0xC0, 0], 0).is_none());
    }

    #[test]
    fn zero_length_and_garbage_input_is_empty() {
        assert_eq!(parse_answer(&[], "x.test"), ParsedAnswer::default());
        assert_eq!(parse_answer(&[0u8; 12], "x.test"), ParsedAnswer::default());
        assert_eq!(
            parse_answer(&[0xFF; 600], "x.test"),
            ParsedAnswer::default()
        );
        // Zero-length name where a name is expected is the root, not a panic.
        assert_eq!(read_name(&[0], 0), Some((".".to_string(), 1)));
    }

    #[test]
    fn rdlength_past_the_buffer_is_rejected() {
        let mut p = packet(
            "_imaps._tcp.example.com",
            &[(TYPE_SRV, srv_rdata(1, 5, 993, "imap.example.com"))],
        );
        let n = p.len();
        // Inflate RDLENGTH (2 bytes ending 6+target bytes before the end).
        let rd_off = n - (6 + enc_name("imap.example.com").len()) - 2;
        p[rd_off] = 0xFF;
        assert!(parse_answer(&p, "_imaps._tcp.example.com").srv.is_empty());
    }

    #[test]
    fn records_whose_owner_name_differs_from_the_query_are_ignored() {
        let p = packet(
            "_imaps._tcp.example.com",
            &[(TYPE_SRV, srv_rdata(1, 5, 993, "imap.example.com"))],
        );
        // Same packet, but we asked about another name: nothing is accepted.
        assert!(parse_answer(&p, "_imaps._tcp.other.test").srv.is_empty());
        // Case and a trailing dot on the expected name do not matter.
        assert_eq!(parse_answer(&p, "_IMAPS._tcp.Example.com.").srv.len(), 1);
    }

    #[test]
    fn labels_with_odd_bytes_are_rejected() {
        assert!(read_name(&[3, b'a', b' ', b'b', 0], 0).is_none());
        assert!(read_name(&[0x40, 0], 0).is_none());
    }
}
