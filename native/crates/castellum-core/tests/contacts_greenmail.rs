// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end contacts aggregation against a local GreenMail server.
//
// #[ignore] by default: requires the GreenMail container from scripts/local-imap.
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test contacts_greenmail -- --ignored --nocapture
//
// The test provisions its own account (parallel-safe, no seeding needed) and
// appends its own deterministic PROJECT_THREADS block with per-run
// message-ids; every assertion is an inequality or a per-address property
// that more copies of the same traffic cannot flip.

mod common;

use castellum_core::open_profile;
use std::net::TcpStream;

const ALICE: &str = "alice@example.test";
const BOB: &str = "bob@example.test";
const CAROL: &str = "carol@example.test";

struct ThreadMsg {
    from: (&'static str, &'static str),
    cc: Vec<(&'static str, &'static str)>,
}

fn person(email: &'static str) -> (&'static str, &'static str) {
    match email {
        ALICE => ("Alice Novak", ALICE),
        BOB => ("Bob Iyer", BOB),
        CAROL => ("Carol Reyes", CAROL),
        other => ("", other),
    }
}

/// Format epoch seconds as an RFC 2822 Date header value (UTC). Civil-from-days
/// per Howard Hinnant's algorithm — avoids pulling a date crate into the test.
fn rfc2822_utc(epoch: u64) -> String {
    let days = (epoch / 86_400) as i64;
    let secs = epoch % 86_400;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][(days % 7) as usize];
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let month_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(month - 1) as usize];
    format!("{weekday}, {d:02} {month_name} {year} {h:02}:{m:02}:{s:02} +0000")
}

/// Append one deterministic project thread; message-ids carry `tag` so reruns
/// don't collide with earlier copies or the JS seed.
fn append_thread(
    session: &mut castellum_provider_imap::session::Session<TcpStream>,
    to: &str,
    tag: &str,
    subject: &str,
    msgs: &[ThreadMsg],
) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut references: Vec<String> = Vec::new();
    for (i, m) in msgs.iter().enumerate() {
        let message_id = format!("<contacts-e2e-{tag}-{i}@example.test>");
        let cc_header = if m.cc.is_empty() {
            String::new()
        } else {
            let list =
                m.cc.iter()
                    .map(|(n, a)| format!("\"{n}\" <{a}>"))
                    .collect::<Vec<_>>()
                    .join(", ");
            format!("Cc: {list}\r\n")
        };
        let reply_headers = if i == 0 {
            String::new()
        } else {
            format!(
                "In-Reply-To: {}\r\nReferences: {}\r\n",
                references[i - 1],
                references.join(" ")
            )
        };
        // Strictly ordered recent dates (age ~minutes → decay ~1).
        let sent = now - ((msgs.len() - i) as u64) * 60;
        let date = rfc2822_utc(sent);
        let subj = if i == 0 {
            subject.to_string()
        } else {
            format!("Re: {subject}")
        };
        let raw = format!(
            "From: \"{from_name}\" <{from_addr}>\r\n\
             To: {to}\r\n\
             {cc_header}\
             Subject: {subj}\r\n\
             Date: {date}\r\n\
             Message-ID: {message_id}\r\n\
             {reply_headers}\
             MIME-Version: 1.0\r\n\
             Content-Type: text/plain; charset=utf-8\r\n\
             \r\n\
             Message {n} of {subject}.\r\n",
            from_name = m.from.0,
            from_addr = m.from.1,
            n = i + 1,
        );
        session
            .append("INBOX", &[], raw.as_bytes())
            .expect("IMAP append");
        references.push(message_id);
    }
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn contacts_aggregate_from_greenmail() {
    // Own account; unique tag per run so reruns stay idempotent.
    let self_addr = common::provision("contacts");
    let tag = common::run_tag("contacts");
    {
        let mut session = common::imap_session(&self_addr);
        // p1: alice+bob cross-cc'd on every message; carol only on the first.
        append_thread(
            &mut session,
            &self_addr,
            &format!("p1-{tag}"),
            "Project Phoenix kickoff",
            &[
                ThreadMsg {
                    from: person(ALICE),
                    cc: vec![person(BOB), person(CAROL)],
                },
                ThreadMsg {
                    from: person(BOB),
                    cc: vec![person(ALICE)],
                },
                ThreadMsg {
                    from: person(ALICE),
                    cc: vec![person(BOB)],
                },
                ThreadMsg {
                    from: person(BOB),
                    cc: vec![person(ALICE)],
                },
            ],
        );
        // p2: carol replies into the thread without alice/bob on the message →
        // exercises the thread-only (0.25×) bonus.
        append_thread(
            &mut session,
            &self_addr,
            &format!("p2-{tag}"),
            "Project Phoenix budget",
            &[
                ThreadMsg {
                    from: person(ALICE),
                    cc: vec![person(BOB)],
                },
                ThreadMsg {
                    from: person(CAROL),
                    cc: vec![],
                },
                ThreadMsg {
                    from: person(BOB),
                    cc: vec![person(ALICE)],
                },
            ],
        );
        let _ = session.logout();
    }

    // Fresh profile: sync INBOX from scratch.
    let dir = castellum_testutil::scratch_root().join(format!("castellum-contacts-e2e-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &self_addr);
    let report = profile.sync_imap_json(&config).expect("sync");
    println!("sync report: {report}");

    // Refresh the aggregate.
    let agg = profile.refresh_contacts_json().expect("refresh contacts");
    println!("aggregate report: {agg}");
    let agg: serde_json::Value = serde_json::from_str(&agg).unwrap();
    assert!(agg["contacts_upserted"].as_i64().unwrap() >= 4);
    assert!(agg["edges_rebuilt"].as_i64().unwrap() >= 3);

    // Contacts present with the expected best names; the account owner is self.
    let list = profile
        .list_contacts_json(None, true, 10_000)
        .expect("list contacts");
    let contacts: serde_json::Value = serde_json::from_str(&list).unwrap();
    let find = |email: &str| -> &serde_json::Value {
        contacts
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["email"].as_str() == Some(email))
            .unwrap_or_else(|| panic!("contact {email} not found"))
    };
    let alice = find(ALICE);
    let bob = find(BOB);
    let carol = find(CAROL);
    let me = find(&self_addr);
    assert_eq!(alice["derived_name"].as_str(), Some("Alice Novak"));
    assert_eq!(bob["derived_name"].as_str(), Some("Bob Iyer"));
    assert_eq!(carol["derived_name"].as_str(), Some("Carol Reyes"));
    assert_eq!(me["is_self"].as_bool(), Some(true));
    assert!(alice["inbound_count"].as_i64().unwrap() >= 3);

    // Edges: read them via a second handle on the profile db (the edge list
    // API stays db-level until the graph view lands in 8c).
    let db = castellum_db::Database::open(&dir.join("castellum.db")).expect("open db");
    let edges = db.list_edges(0.0, 100_000).expect("list edges");
    println!("{} edges", edges.len());
    let (alice_id, bob_id, carol_id, me_id) = (
        alice["id"].as_i64().unwrap(),
        bob["id"].as_i64().unwrap(),
        carol["id"].as_i64().unwrap(),
        me["id"].as_i64().unwrap(),
    );
    let weight = |x: i64, y: i64| -> f64 {
        let key = (x.min(y), x.max(y));
        edges
            .iter()
            .find(|e| (e.contact_a, e.contact_b) == key)
            .map(|e| e.weight)
            .unwrap_or_else(|| panic!("edge {key:?} missing"))
    };
    let ab = weight(alice_id, bob_id);
    let ac = weight(alice_id, carol_id);
    println!("weight alice-bob = {ab}, alice-carol = {ac}");
    assert!(
        ab > ac,
        "cross-cc'd alice-bob ({ab}) must outweigh alice-carol ({ac})"
    );

    // No self edges, and the account owner appears in no edge at all.
    for e in &edges {
        assert!(e.contact_a < e.contact_b, "canonical order, no self-loops");
        assert_ne!(e.contact_a, me_id, "account owner must not be edged");
        assert_ne!(e.contact_b, me_id, "account owner must not be edged");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
