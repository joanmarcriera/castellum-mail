// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S44 List-Unsubscribe against real GreenMail: a delivered
// message carrying the header is synced, hydrated, and its targets read
// back through get_message_json.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test unsubscribe_greenmail -- --ignored --nocapture

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-unsub-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn a_synced_and_hydrated_message_surfaces_its_list_unsubscribe_targets() {
    let dir = temp_profile_dir("unsub");
    let profile = open_profile(&dir).expect("open profile");

    let email = common::provision("unsub-acct");
    common::smtp_deliver(
        "sender@local.test",
        &email,
        "Subject: newsletter\r\n\
         List-Unsubscribe: <mailto:unsub@local.test?subject=unsubscribe>, <https://local.test/unsub?id=1>\r\n\
         List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
         \r\nbody\r\n",
    );
    profile
        .sync_imap_json(&common::sync_config_json("gm1", &email))
        .expect("sync gm1");

    let list: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm1", "gm1:INBOX", 50, 0)
            .expect("list"),
    )
    .unwrap();
    let messages = list.as_array().unwrap();
    let target = messages
        .iter()
        .find(|h| h["subject"] == "newsletter")
        .expect("newsletter message present");
    let message_id = target["id"].as_str().unwrap();

    // The lightweight sync path may already have full bodies in this test
    // setup (GreenMail delivery is small); assert on get_message_json either
    // way — hydrate first if needed.
    let mut detail: serde_json::Value =
        serde_json::from_str(&profile.get_message_json(message_id).expect("get detail")).unwrap();
    if !detail["header"]["has_body"].as_bool().unwrap_or(false) {
        let cfg = common::sync_config_json("gm1", &email);
        let hydrated = profile
            .hydrate_message_body_json(&cfg, message_id)
            .expect("hydrate");
        detail = serde_json::from_str(&hydrated).unwrap();
    }

    assert_eq!(
        detail["list_unsubscribe_mailto"].as_str(),
        Some("mailto:unsub@local.test?subject=unsubscribe")
    );
    assert_eq!(
        detail["list_unsubscribe_url"].as_str(),
        Some("https://local.test/unsub?id=1")
    );
    assert_eq!(detail["list_unsubscribe_one_click"].as_bool(), Some(true));
}
