// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//! Malformed and adversarial input for the castellum-mail parsers: every case
//! must finish without a panic, a hang or a stack overflow. Each case runs on
//! a 2 MiB thread stack (Rust's default for spawned threads, which is where
//! sync parses mail) so a recursion that only fits the 8 MiB main stack fails
//! here. A stack overflow aborts the whole test binary rather than failing one
//! case, which is still a failure CI reports.

use castellum_mail::{decode_transfer_encoding, extract_stored_headers, flowed, parse_message};
use std::time::{Duration, Instant};

/// Generous for a debug build; a quadratic blow-up overshoots it by far.
const BUDGET: Duration = Duration::from_secs(10);

fn on_small_stack<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let start = Instant::now();
    let out = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .expect("spawn")
        .join()
        .unwrap_or_else(|_| panic!("case {name:?} panicked"));
    assert!(
        start.elapsed() < BUDGET,
        "case {name:?} took {:?}",
        start.elapsed()
    );
    out
}

fn nested_multipart(depth: usize) -> Vec<u8> {
    let mut s = String::from("From: a@example.com\r\nSubject: deep\r\n");
    for i in 0..depth {
        s.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"b{i}\"\r\n\r\n--b{i}\r\n"
        ));
    }
    s.push_str("Content-Type: text/plain\r\n\r\nleaf\r\n");
    for i in (0..depth).rev() {
        s.push_str(&format!("--b{i}--\r\n"));
    }
    s.into_bytes()
}

fn cases() -> Vec<(&'static str, Vec<u8>)> {
    let long_line = "x".repeat(2 << 20);
    vec![
        ("empty", Vec::new()),
        ("only crlf", b"\r\n".to_vec()),
        ("only lf", b"\n\n\n".to_vec()),
        ("lone cr", b"\r\r\r".to_vec()),
        ("headers no body", b"From: a@example.com\r\nSubject: x".to_vec()),
        ("body no headers", b"\r\njust a body".to_vec()),
        ("nul bytes", b"Subject: a\0b\r\n\r\n\0\0\0".to_vec()),
        ("crlf lf mix", b"Subject: mix\nFrom: a@example.com\r\n\nbody\r\nline\nline\r".to_vec()),
        ("8-bit latin1 header", b"Subject: caf\xe9 \xff\xfe\r\nFrom: J\xf6rg <j@example.com>\r\n\r\nx".to_vec()),
        ("invalid utf8 body", b"Content-Type: text/plain; charset=utf-8\r\n\r\n\xc3\x28\xa0\xa1\xf0\x28\x8c\xbc".to_vec()),
        ("over-long header line", format!("Subject: {long_line}\r\n\r\nx").into_bytes()),
        ("over-long body line", format!("Subject: s\r\n\r\n{long_line}").into_bytes()),
        ("many folds", format!("Subject: a{}\r\n\r\nx", "\r\n b".repeat(100_000)).into_bytes()),
        ("whitespace-only fold line", b"Subject: a\r\n \r\nX-Hidden: y\r\n\r\nx".to_vec()),
        ("multipart no boundary param", b"Content-Type: multipart/mixed\r\n\r\n--x\r\nhello\r\n--x--".to_vec()),
        ("multipart empty boundary", b"Content-Type: multipart/mixed; boundary=\"\"\r\n\r\n--\r\n\r\nhi\r\n----".to_vec()),
        ("boundary never appears", b"Content-Type: multipart/mixed; boundary=zz\r\n\r\nno parts here".to_vec()),
        ("missing close boundary", b"Content-Type: multipart/mixed; boundary=zz\r\n\r\n--zz\r\nContent-Type: text/plain\r\n\r\nopen".to_vec()),
        ("empty multipart", b"Content-Type: multipart/alternative; boundary=zz\r\n\r\n--zz--\r\n".to_vec()),
        ("boundary inside boundary", b"Content-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\nContent-Type: multipart/mixed; boundary=a\r\n\r\n--a\r\n\r\nx\r\n--a--\r\n--a--".to_vec()),
        ("nested 11", nested_multipart(11)),
        ("nested 65", nested_multipart(65)),
        ("nested 20000", nested_multipart(20_000)),
        ("10000 parts", {
            let mut s = String::from("Content-Type: multipart/mixed; boundary=p\r\n\r\n");
            for i in 0..10_000 {
                s.push_str(&format!("--p\r\nContent-Type: application/octet-stream; name=f{i}\r\nContent-Disposition: attachment\r\n\r\nx\r\n"));
            }
            s.push_str("--p--\r\n");
            s.into_bytes()
        }),
        ("message/rfc822 chain", {
            let mut s = String::from("Subject: leaf\r\n\r\nleaf");
            for _ in 0..50 {
                s = format!("Content-Type: message/rfc822\r\n\r\n{s}");
            }
            s.into_bytes()
        }),
        ("rfc2047 empty q", b"Subject: =?utf-8?q??=\r\n\r\n".to_vec()),
        ("rfc2047 empty b", b"Subject: =?utf-8?b??=\r\n\r\n".to_vec()),
        ("rfc2047 bad base64", b"Subject: =?utf-8?b?!!!!***?=\r\n\r\n".to_vec()),
        ("rfc2047 unknown charset", b"Subject: =?x-klingon?q?abc?=\r\n\r\n".to_vec()),
        ("rfc2047 unknown encoding", b"Subject: =?utf-8?z?abc?=\r\n\r\n".to_vec()),
        ("rfc2047 unterminated", b"Subject: =?utf-8?q?abc\r\n\r\n".to_vec()),
        ("rfc2047 truncated multibyte", b"Subject: =?utf-8?q?=E2=82?= =?utf-8?q?=AC?=\r\n\r\n".to_vec()),
        ("rfc2047 language tag", b"Subject: =?utf-8*en?q?hi?=\r\n\r\n".to_vec()),
        ("rfc2047 10000 words", format!("Subject: {}\r\n\r\n", "=?utf-8?b?w6k=?= ".repeat(10_000)).into_bytes()),
        ("rfc2047 in address", b"From: =?utf-8?q?=00=FF?= <=?utf-8?q?a?=@example.com>\r\n\r\n".to_vec()),
        ("address unterminated quote", b"From: \"Bob <bob@example.com>\r\nTo: <<<>>>, @, ,,,\r\n\r\n".to_vec()),
        ("address nested groups", b"To: g1: g2: a@b.c;; ;, x@y.z\r\n\r\n".to_vec()),
        ("address comments", b"From: ((((((((((((((((((((nested)))))))))))))))))))) a@b.c\r\n\r\n".to_vec()),
        ("address deep comments", format!("From: {}a@b.c{}\r\n\r\n", "(".repeat(50_000), ")".repeat(50_000)).into_bytes()),
        ("10000 addresses", format!("To: {}\r\n\r\n", "a@example.com, ".repeat(10_000)).into_bytes()),
        ("date garbage", b"Date: not a date at all\r\n\r\n".to_vec()),
        ("date huge year", b"Date: Mon, 1 Jan 999999999 00:00:00 +0000\r\n\r\n".to_vec()),
        ("date huge offset", b"Date: Mon, 1 Jan 2024 00:00:00 +9999\r\n\r\n".to_vec()),
        ("date out of range fields", b"Date: Mon, 99 Feb 2024 99:99:99 -9999\r\n\r\n".to_vec()),
        ("date digits overflow", format!("Date: {} Jan 2024 00:00:00 +0000\r\n\r\n", "9".repeat(500)).into_bytes()),
        ("flowed many quote markers", format!("Content-Type: text/plain; format=flowed\r\n\r\n{} x \r\n{}y\r\n", ">".repeat(100_000), ">".repeat(99_999)).into_bytes()),
        ("flowed delsp only spaces", b"Content-Type: text/plain; format=flowed; delsp=yes\r\n\r\n \r\n  \r\n   \r\n-- \r\n".to_vec()),
        ("qp stray equals", b"Content-Transfer-Encoding: quoted-printable\r\n\r\n=\r\n= \r\n=4\r\n=ZZ=".to_vec()),
        ("base64 garbage", b"Content-Transfer-Encoding: base64\r\n\r\n%%%===\r\nQQ=\r\n=".to_vec()),
        ("html only deep tags", format!("Content-Type: text/html\r\n\r\n{}", "<div>".repeat(50_000)).into_bytes()),
    ]
}

#[test]
fn parse_message_survives_adversarial_input() {
    for (name, raw) in cases() {
        on_small_stack(name, move || {
            let parsed = parse_message(&raw);
            // The snippet is bounded whatever the body.
            assert!(parsed.snippet.as_deref().map_or(0, |s| s.chars().count()) <= 201);
        });
    }
}

#[test]
fn extract_stored_headers_survives_adversarial_input() {
    for (name, raw) in cases() {
        on_small_stack(name, move || {
            let _ = extract_stored_headers(&raw);
        });
    }
}

#[test]
fn deep_nesting_keeps_the_leaf_body_and_never_numbers_past_the_cap() {
    let parsed = on_small_stack("nested 20000", || parse_message(&nested_multipart(20_000)));
    assert_eq!(parsed.text_body.as_deref(), Some("leaf"));
    // Shallow nesting is still numbered as IMAP would: the leaf of 11 nested
    // single-child multiparts is section 1.1.1.1.1.1.1.1.1.1.1.
    let raw = {
        let mut s = String::from_utf8(nested_multipart(11)).unwrap();
        s = s.replace(
            "Content-Type: text/plain\r\n\r\nleaf",
            "Content-Type: application/pdf; name=a.pdf\r\nContent-Disposition: attachment\r\n\r\nleaf",
        );
        s.into_bytes()
    };
    let parsed = parse_message(&raw);
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].section, ["1"; 11].join("."));
}

#[test]
fn flowed_decode_is_total_over_odd_line_shapes() {
    let deep = format!("{} a \n{} b", ">".repeat(50_000), ">".repeat(50_000));
    let table: &[(&str, bool, &str)] = &[
        ("", false, ""),
        (" ", false, ""),
        (" ", true, ""),
        ("\r", false, ""),
        ("\r\n\r\n", false, "\n\n"),
        ("a \r\nb", false, "a b"),
        ("a \nb", true, "ab"),
        // A flowed line followed by the empty last line keeps its space.
        ("a \r\n", false, "a "),
        ("> a \n>> b", false, "> a\n>> b"),
        (">\u{e9} \n>\u{e9}", false, "> \u{e9} \u{e9}"),
        ("-- \nsig", false, "-- \nsig"),
        ("\u{1F600} \n\u{1F600}", true, "\u{1F600}\u{1F600}"),
    ];
    for &(input, delsp, want) in table {
        assert_eq!(
            flowed::decode(input, delsp),
            want,
            "input {input:?} delsp={delsp}"
        );
    }
    let out = on_small_stack("deep quotes", move || flowed::decode(&deep, false));
    assert!(out.ends_with(" a b"));
}

#[test]
fn quoted_printable_and_base64_decoders_are_total() {
    let qp = Some("quoted-printable");
    type Case<'a> = (&'a [u8], Option<&'a str>, Option<&'a [u8]>);
    let table: &[Case] = &[
        (b"", qp, Some(b"")),
        (b"=", qp, Some(b"=")),
        (b"=\r", qp, Some(b"=\r")),
        (b"=\r\n", qp, Some(b"")),
        (b"= \t\n", qp, Some(b"")),
        (b"=4", qp, Some(b"=4")),
        (b"=4G", qp, Some(b"=4G")),
        (b"=41=", qp, Some(b"A=")),
        (b"a  \r\nb", qp, Some(b"a\r\nb")),
        (b"a=20\r\n", qp, Some(b"a \r\n")),
        (b"\xff\xfe=FF", qp, Some(b"\xff\xfe\xff")),
        (b"", Some("base64"), Some(b"")),
        (b"QUJD", Some("base64"), Some(b"ABC")),
        (b"QUJD", Some(" BASE64 "), Some(b"ABC")),
        (b"QUJD", Some("x-unknown"), Some(b"QUJD")),
        (b"QUJD", None, Some(b"QUJD")),
    ];
    for &(raw, enc, want) in table {
        assert_eq!(
            decode_transfer_encoding(raw, enc).as_deref(),
            want,
            "raw {raw:?} encoding {enc:?}"
        );
    }
    // Malformed base64 must not panic; refusing (None) or lenient bytes are both fine.
    for raw in [
        &b"%%%"[..],
        b"=",
        b"Q",
        b"QQ=",
        b"QUJD====",
        b"\xff\xff\xff\xff",
    ] {
        let _ = decode_transfer_encoding(raw, Some("base64"));
    }
}
