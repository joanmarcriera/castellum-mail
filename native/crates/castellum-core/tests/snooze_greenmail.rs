// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S43 snooze against real GreenMail: a synced message is
// snoozed, hidden from the inbox list, and reappears once due.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test snooze_greenmail -- --ignored --nocapture

mod common;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-snooze-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn a_snoozed_message_is_hidden_then_reappears_once_due() {
    let dir = temp_profile_dir("snooze");
    let profile = castellum_core::open_profile(&dir).expect("open profile");

    let email = common::provision("snooze-acct");
    profile
        .sync_imap_json(&common::sync_config_json("gm1", &email))
        .expect("sync gm1");

    let before: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm1", "gm1:INBOX", 50, 0)
            .expect("list before"),
    )
    .unwrap();
    let target_id = before.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Snooze far in the future — must vanish from the mailbox list.
    profile
        .snooze_message(&target_id, 9_999_999_999)
        .expect("snooze");
    let hidden: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm1", "gm1:INBOX", 50, 0)
            .expect("list hidden"),
    )
    .unwrap();
    assert!(
        hidden
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["id"] != target_id),
        "snoozed message must not appear in the mailbox list"
    );

    // Due-check with a `now` in the past: nothing releases yet.
    let none_due: serde_json::Value = serde_json::from_str(
        &profile
            .unsnooze_due_json(0)
            .expect("due check, nothing due"),
    )
    .unwrap();
    assert!(none_due.as_array().unwrap().is_empty());

    // Due-check with a `now` past the snooze time: it releases and reappears.
    let due: serde_json::Value = serde_json::from_str(
        &profile
            .unsnooze_due_json(10_000_000_000)
            .expect("due check, due"),
    )
    .unwrap();
    let due_ids: Vec<&str> = due
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["message_id"].as_str().unwrap())
        .collect();
    assert_eq!(due_ids, vec![target_id.as_str()]);

    let after: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm1", "gm1:INBOX", 50, 0)
            .expect("list after"),
    )
    .unwrap();
    assert!(
        after
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["id"] == target_id),
        "message must be back in the mailbox list once due"
    );
}
