// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S35 "Reset account and re-sync from scratch" against real
// GreenMail.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test reset_account_greenmail -- --ignored --nocapture

mod common;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-reset-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn reset_account_refuses_with_an_unsent_outbox_draft_then_force_discards_it_and_resyncs() {
    let dir = temp_profile_dir("reset");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("reset-acct");
    profile
        .sync_imap_json(&common::sync_config_json("gm", &email))
        .expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    // Prove there is real synced mail to lose.
    let before: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    assert!(
        !before.as_array().unwrap().is_empty(),
        "GreenMail bootstrap mail must be synced first"
    );

    // Queue (but never deliver) a draft — the "unsent outbox" blocker.
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"email": email}],
        "subject": "still sending",
        "body_text": "not delivered yet\n",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("send_draft"); // status -> outbox, queues a 'send' op

    // Unforced reset refuses.
    let err = profile
        .reset_account("gm", false)
        .expect_err("must refuse with a queued send");
    assert!(
        format!("{err}").starts_with("reset blocked:"),
        "must use the stable reset-blocked prefix: {err}"
    );

    // Forced reset discards it and wipes local mail.
    profile.reset_account("gm", true).expect("forced reset");
    let drafts = profile
        .list_drafts_json("gm")
        .expect("list drafts after reset");
    assert_eq!(drafts, "[]", "the queued draft must be gone: {drafts}");
    let mailboxes = profile
        .list_mailboxes_json("gm")
        .expect("list mailboxes after reset");
    assert_eq!(
        mailboxes, "[]",
        "mailboxes must be gone until the next sync: {mailboxes}"
    );

    // A fresh sync repopulates Inbox from the live server — the account row
    // and its credentials were never touched, so the same config still works.
    profile
        .sync_imap_json(&common::sync_config_json("gm", &email))
        .expect("resync after reset");
    let after: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    assert!(
        !after.as_array().unwrap().is_empty(),
        "resync must repopulate Inbox from the server"
    );
}
