// Castellum - header-store cost probe (MM-1c / #2710 slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Measures what the any-header store (migration 0057) costs on a seeded
// profile, and how a Header smart-mailbox condition performs on it.
//
// Usage:
//   cargo run --release -p castellum-core --example seed_profile -- <seed-dir> --messages 100000
//   cargo run --release -p castellum-core --example header_storage_probe -- <seed-dir>
//
// The seed profile is COPIED to a scratch dir first and never modified. For
// every message the probe builds a deterministic, realistic synthetic header
// block (a seeded mix of personal mail, newsletters and service
// notifications, with the header NAMES and value LENGTHS of real mail, never
// real content), runs it through the REAL extractor (`extract_stored_headers`,
// denylist + caps) and stores it with `replace_message_headers`. That is the
// worst case: 100% coverage. Prints bytes/message (file-size delta after a
// TRUNCATE checkpoint), rows/message, and ms + query plan for contains /
// present / absent conditions. Counts and timings only, no message content.
//
// Decision gate (spec Decision for Marc 1): if bytes/message exceeds 2,500 the
// denylist default must switch to the allowlist fallback.

#[path = "perf_common/mod.rs"]
mod perf_common;

use castellum_db::Database;
use castellum_mail::extract_stored_headers;
use castellum_models::{
    BoolOp, FilterCondition, FilterField, FilterGroup, FilterOperator, FilterValue,
    HeaderCaptureSource, SmartMailboxScope,
};
use perf_common::Rng;
use std::path::Path;
use std::time::Instant;

fn rand_token(rng: &mut Rng, len: u64) -> String {
    const A: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..len)
        .map(|_| A[rng.below(A.len() as u64) as usize] as char)
        .collect()
}

/// A synthetic header block with real-world header names and value lengths.
fn synth_headers(rng: &mut Rng, i: u64) -> Vec<u8> {
    let mut h = String::new();
    let mut add = |name: &str, value: String| {
        h.push_str(&format!("{name}: {value}\r\n"));
    };
    // Routing noise every message has (all denylisted, must cost nothing).
    for hop in 0..(3 + rng.below(6)) {
        add("Received", format!("from mx{hop}.example.net by mail.example.org with ESMTPS id {}; Mon, 1 Jan 2026 00:00:00 +0000", rand_token(rng, 24)));
    }
    add(
        "DKIM-Signature",
        format!(
            "v=1; a=rsa-sha256; d=example.net; s=sel; b={}",
            rand_token(rng, 340)
        ),
    );
    add(
        "Authentication-Results",
        "mx.example.org; spf=pass; dkim=pass; dmarc=pass".into(),
    );
    add(
        "Message-ID",
        format!("<{}@example.net>", rand_token(rng, 28)),
    );
    add("MIME-Version", "1.0".into());
    add(
        "Content-Type",
        "multipart/alternative; boundary=\"b1\"".into(),
    );
    add("From", "Sender <s@example.net>".into());
    add("Subject", "hello".into());
    add(
        "Return-Path",
        format!("<bounce+{}@mg.example.net>", rand_token(rng, 30)),
    );
    add("Delivered-To", "me@example.org".into());
    let kind = rng.below(100);
    if kind < 35 {
        // Personal mail.
        if rng.below(2) == 0 {
            add(
                "X-Mailer",
                [
                    "Apple Mail (2.3826.200.121)",
                    "Microsoft Outlook 16.0",
                    "Thunderbird 128.0",
                ][rng.below(3) as usize]
                    .into(),
            );
        }
        add("Thread-Topic", rand_token(rng, 30));
        add("Thread-Index", rand_token(rng, 44));
        add("Accept-Language", "en-GB, en-US".into());
        add("Content-Language", "en-GB".into());
        add("X-MS-Has-Attach", "".into());
        add("Importance", "normal".into());
        if rng.below(3) == 0 {
            add(
                "Reply-To",
                format!("Someone <{}@example.com>", rand_token(rng, 10)),
            );
        }
        add(
            "X-Spam-Status",
            format!("No, score=-{}.{} required=5.0", rng.below(3), rng.below(10)),
        );
        add(
            "X-Spam-Score",
            format!("-{}.{}", rng.below(3), rng.below(10)),
        );
    } else if kind < 80 {
        // Newsletters / marketing.
        add(
            "List-Id",
            format!("<{}.lists.example.net>", rand_token(rng, 12)),
        );
        add(
            "List-Unsubscribe",
            format!(
                "<https://example.net/u/{}?c={}>, <mailto:unsub+{}@example.net>",
                rand_token(rng, 32),
                rand_token(rng, 16),
                rand_token(rng, 20)
            ),
        );
        add("List-Unsubscribe-Post", "List-Unsubscribe=One-Click".into());
        add("Precedence", "bulk".into());
        add(
            "Feedback-ID",
            format!(
                "{}:{}:campaign:esp",
                rand_token(rng, 10),
                rand_token(rng, 8)
            ),
        );
        add(
            "X-Mailer",
            ["Mailchimp Mailer", "SendGrid", "Amazon SES", "Braze"][rng.below(4) as usize].into(),
        );
        add("X-Campaign", rand_token(rng, 24));
        add("X-Mailgun-Sid", format!("WyI{}Ii", rand_token(rng, 60)));
        add(
            "Reply-To",
            format!("support@{}.example.net", rand_token(rng, 8)),
        );
        add(
            "X-Report-Abuse",
            format!(
                "Please report abuse here: https://example.net/abuse?id={}",
                rand_token(rng, 30)
            ),
        );
        add("X-Entity-ID", rand_token(rng, 22));
        add("X-CSA-Complaints", "whitelist-complaints@eco.de".into());
        add("List-Help", "<https://example.net/help>".into());
        add("X-Auto-Response-Suppress", "OOF, AutoReply".into());
    } else {
        // Service notifications (GitHub-shaped).
        add(
            "List-ID",
            format!(
                "repo/{} <repo.{}.github.com>",
                rand_token(rng, 8),
                rand_token(rng, 8)
            ),
        );
        add("List-Archive", format!("https://github.com/o/r/pull/{i}"));
        add("List-Post", "<mailto:reply+x@reply.github.com>".into());
        add("List-Unsubscribe", format!("<mailto:unsub+{}@reply.github.com>, <https://github.com/notifications/unsubscribe-auth/{}>", rand_token(rng, 40), rand_token(rng, 60)));
        add("X-GitHub-Sender", rand_token(rng, 9));
        add("X-GitHub-Recipient", rand_token(rng, 9));
        add(
            "X-GitHub-Reason",
            ["review_requested", "mention", "subscribed", "author"][rng.below(4) as usize].into(),
        );
        add("X-GitHub-Recipient-Address", "me@example.org".into());
        add("Auto-Submitted", "auto-generated".into());
        add("Precedence", "list".into());
        add(
            "Feedback-ID",
            format!("{}:{}:GitHub", rand_token(rng, 10), rand_token(rng, 10)),
        );
        add(
            "Reply-To",
            format!("Repo <reply+{}@reply.github.com>", rand_token(rng, 40)),
        );
        add("X-Auto-Response-Suppress", "All".into());
    }
    h.push_str("\r\n");
    h.into_bytes()
}

fn group(name: &str, op: FilterOperator, value: FilterValue) -> FilterGroup {
    FilterGroup {
        op: BoolOp::And,
        conditions: vec![FilterCondition {
            field: FilterField::Header(name.into()),
            operator: op,
            value,
        }],
        groups: vec![],
    }
}

fn main() {
    let seed_dir = std::env::args()
        .nth(1)
        .expect("usage: header_storage_probe <seed-dir>");
    let work = std::env::temp_dir().join(format!("castellum-header-probe-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::copy(
        Path::new(&seed_dir).join("castellum.db"),
        work.join("castellum.db"),
    )
    .expect("copy seed db");
    let db_path = work.join("castellum.db");
    let size = || std::fs::metadata(&db_path).map_or(0, |m| m.len());
    let checkpoint = || {
        let c = rusqlite::Connection::open(&db_path).unwrap();
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    };
    let db = Database::open(&db_path).expect("open copy");
    checkpoint();
    let before = size();

    let raw_conn = rusqlite::Connection::open(&db_path).unwrap();
    let ids: Vec<String> = {
        let conn = &raw_conn;
        let mut st = conn.prepare("SELECT id FROM messages ORDER BY id").unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    let mut rng = Rng::new(2710);
    let t = Instant::now();
    let mut rows = 0u64;
    for (i, id) in ids.iter().enumerate() {
        let raw = synth_headers(&mut rng, i as u64);
        let (hs, truncated) = extract_stored_headers(&raw);
        rows += hs.len() as u64;
        db.replace_message_headers(id, &hs, HeaderCaptureSource::Body, truncated)
            .unwrap();
    }
    let capture_s = t.elapsed().as_secs_f64();
    checkpoint();
    let after = size();
    let n = ids.len() as f64;
    let value_bytes: i64 = raw_conn
        .query_row(
            "SELECT COALESCE(SUM(LENGTH(value)),0) FROM message_headers",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!("messages: {}", ids.len());
    println!("header rows: {rows} ({:.1}/message)", rows as f64 / n);
    println!(
        "value bytes: {value_bytes} ({:.0}/message)",
        value_bytes as f64 / n
    );
    println!("db before: {before} B, after: {after} B");
    println!("STORAGE bytes/message: {:.0}", (after - before) as f64 / n);
    println!(
        "capture wall time: {capture_s:.1}s ({:.0} us/message incl. parse)",
        capture_s * 1e6 / n
    );

    let scope = SmartMailboxScope::default();
    for (label, g) in [
        (
            "contains List-Id",
            group(
                "List-Id",
                FilterOperator::Contains,
                FilterValue::Text("lists.example".into()),
            ),
        ),
        (
            "starts_with X-GitHub-Reason",
            group(
                "X-GitHub-Reason",
                FilterOperator::StartsWith,
                FilterValue::Text("review".into()),
            ),
        ),
        (
            "present X-Mailer",
            group("X-Mailer", FilterOperator::Is, FilterValue::Bool(true)),
        ),
        (
            "absent List-Id",
            group("List-Id", FilterOperator::Is, FilterValue::Bool(false)),
        ),
        (
            "unknown name",
            group(
                "X-Nope",
                FilterOperator::Contains,
                FilterValue::Text("a".into()),
            ),
        ),
    ] {
        let mut ms = Vec::new();
        let mut hits = 0;
        for _ in 0..5 {
            let t = Instant::now();
            hits = db.count_group_matches(&g, &scope, 1_000_000).unwrap().0;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
        }
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "{label}: {hits} hits, p50 {:.1} ms (max {:.1})",
            ms[2], ms[4]
        );
    }
    let _ = std::fs::remove_dir_all(&work);
}
