// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S34 unified (cross-account) message list against real GreenMail.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test unified_inbox_greenmail -- --ignored --nocapture

mod common;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-unified-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Two real accounts synced against GreenMail; `list_messages_by_role_json`
/// ("inbox") must return both accounts' Inbox mail, newest first — the core
/// half of the sidebar's "All Inboxes" group.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn list_messages_by_role_spans_every_synced_account() {
    let dir = temp_profile_dir("unified");
    let profile = castellum_core::open_profile(&dir).expect("open profile");

    let email1 = common::provision("unified-acct1");
    let email2 = common::provision("unified-acct2");
    profile
        .sync_imap_json(&common::sync_config_json("gm1", &email1))
        .expect("sync gm1");
    profile
        .sync_imap_json(&common::sync_config_json("gm2", &email2))
        .expect("sync gm2");

    let json = profile
        .list_messages_by_role_json("inbox", 50, 0)
        .expect("list_messages_by_role_json");
    let headers: serde_json::Value = serde_json::from_str(&json).unwrap();
    let headers = headers.as_array().expect("array");
    let account_ids: std::collections::HashSet<&str> = headers
        .iter()
        .map(|h| h["account_id"].as_str().unwrap())
        .collect();
    assert!(
        account_ids.contains("gm1") && account_ids.contains("gm2"),
        "unified inbox must include both synced accounts: {headers:?}"
    );
    assert!(
        headers
            .iter()
            .all(|h| h["mailbox_id"].as_str().unwrap().ends_with(":INBOX")),
        "every row must come from an inbox-role mailbox: {headers:?}"
    );
}
