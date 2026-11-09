// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - raw RFC822 source on-demand fetch e2e (S49), against real
// GreenMail. #[ignore] by default: requires local GreenMail
// (scripts/local-imap). Run:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test raw_source_greenmail -- --ignored --nocapture

mod common;

use castellum_core::open_profile;

fn append_raw(email: &str, mailbox: &str, raw: &[u8]) {
    let mut session = common::imap_session(email);
    // No flags: keeps the old `imap`-crate oracle's behaviour (append with an
    // empty flag list), so the appended message stays \Unseen.
    session.append(mailbox, &[], raw).expect("IMAP append");
    let _ = session.logout();
}

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-rawsource-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn fetch_raw_source_round_trips_and_then_hits_the_cache() {
    let email = common::provision("rawsource-fetch");
    let tag = common::run_tag("rawsource");
    let subject = format!("castl-rawsource-{tag}");
    let raw = format!(
        "From: probe@local.test\r\nTo: {email}\r\nSubject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\nMessage-ID: <rawsource-{tag}@local.test>\r\n\
         \r\nBody text.\r\n"
    )
    .into_bytes();
    append_raw(&email, "INBOX", &raw);

    let dir = temp_profile_dir("fetch");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("sync");

    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 500, 0)
        .expect("list");
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    let message_id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject.as_str()))
        .unwrap_or_else(|| panic!("probe '{subject}' found after sync"))["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Not cached yet.
    let before: serde_json::Value =
        serde_json::from_str(&profile.raw_source_cache_status_json(&message_id).unwrap()).unwrap();
    assert_eq!(before["status"], "not_cached");

    // Fetch: real network round trip against GreenMail.
    let fetched: serde_json::Value =
        serde_json::from_str(&profile.fetch_raw_source_json(&config, &message_id).unwrap())
            .unwrap();
    assert_eq!(fetched["status"], "downloaded", "fetch result: {fetched}");
    let path = fetched["local_path"].as_str().expect("local_path present");
    let bytes = std::fs::read(path).expect("cached file readable");
    assert!(
        String::from_utf8_lossy(&bytes).contains(&subject),
        "cached file is the real RFC822 source"
    );
    assert!(
        String::from_utf8_lossy(&bytes).starts_with("From:"),
        "raw source starts with headers, not just the body"
    );

    // Second fetch is served from the cache without another round trip.
    let again: serde_json::Value =
        serde_json::from_str(&profile.fetch_raw_source_json(&config, &message_id).unwrap())
            .unwrap();
    assert_eq!(again["status"], "cached");
    assert_eq!(again["local_path"].as_str(), Some(path));

    let _ = std::fs::remove_dir_all(&dir);
}
