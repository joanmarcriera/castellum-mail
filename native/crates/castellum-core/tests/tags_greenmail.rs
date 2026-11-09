// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S42 tags/mute against real GreenMail.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test tags_greenmail -- --ignored --nocapture

mod common;

use castellum_core::open_profile;
use castellum_provider_imap::keyword_is_permanent;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-tags-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn replay_config(account_id: &str, email: &str) -> String {
    serde_json::json!({
        "account_id": account_id,
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false
    })
    .to_string()
}

/// GreenMail 2.1.9's INBOX PERMANENTFLAGS, read directly off a raw session
/// (the same probe `ImapProvider::store_keyword` does before every STORE) —
/// decides which branch `tag_round_trips_as_keyword` below takes, and both
/// branches assert deterministically rather than accepting either outcome.
/// Verified empirically (2026-09-25): GreenMail 2.1.9's INBOX SELECT
/// advertises `PERMANENTFLAGS (... \*)`, so `allows_new_keywords` is true
/// and the round-trip test always takes the "server kept it" branch — the
/// `else` arm below is exercised only against a real mailbox that omits `\*`.
fn inbox_allows_new_keywords(email: &str) -> bool {
    let mut session = common::imap_session(email);
    session.select("INBOX").expect("SELECT INBOX");
    let allows = keyword_is_permanent(session.last_permanent_flags(), "Work");
    let _ = session.logout();
    allows
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn tag_round_trips_as_keyword() {
    let email = common::provision(&common::run_tag("tags-roundtrip"));
    let allows_new_keywords = inbox_allows_new_keywords(&email);

    let dir = temp_profile_dir("roundtrip");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("sync");

    let list: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    profile.set_account_write_enabled("gm", true).unwrap();
    let outcome: serde_json::Value =
        serde_json::from_str(&profile.set_message_tag_json(&id, "Work", true).unwrap()).unwrap();
    assert_eq!(
        outcome["synced"], true,
        "writes enabled -> queued for the server: {outcome}"
    );
    assert!(outcome["reason"].is_null());

    let report: serde_json::Value = serde_json::from_str(
        &profile
            .replay_operations_json(&replay_config("gm", &email))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["applied"], 1,
        "the keyword op clears one way or another: {report}"
    );

    // A FRESH profile syncing the same account from scratch sees only what
    // the SERVER actually has — the round-trip proof neither profile's own
    // cache can fake.
    let fresh_dir = temp_profile_dir("roundtrip-fresh");
    let fresh = open_profile(&fresh_dir).expect("open fresh profile");
    fresh.sync_imap_json(&config).expect("fresh sync");
    let fresh_list: serde_json::Value =
        serde_json::from_str(&fresh.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap()).unwrap();
    let fresh_keywords: Vec<String> = fresh_list.as_array().unwrap()[0]["keywords"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_owned())
        .collect();

    if allows_new_keywords {
        // GreenMail 2.1.9's INBOX advertises `\*`: the STORE persists.
        assert_eq!(
            fresh_keywords,
            vec!["Work".to_string()],
            "server kept the keyword: {fresh_list}"
        );
    } else {
        // GreenMail refused an unlisted keyword: demoted to local-only, so
        // nothing ever reached the server and a fresh sync sees none.
        assert!(
            fresh_keywords.is_empty(),
            "refused keyword must not reach the server: {fresh_list}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fresh_dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn evaluation_mode_tag_never_stores() {
    let email = common::provision(&common::run_tag("tags-eval"));

    let dir = temp_profile_dir("eval");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("sync");

    let list: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let id = list.as_array().unwrap()[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Writes are OFF (the default for a freshly synced account) — the tag
    // applies locally but queues nothing.
    let outcome: serde_json::Value =
        serde_json::from_str(&profile.set_message_tag_json(&id, "Work", true).unwrap()).unwrap();
    assert_eq!(outcome["synced"], false);
    assert_eq!(outcome["reason"], "evaluation_mode");

    let report: serde_json::Value = serde_json::from_str(
        &profile
            .replay_operations_json(&replay_config("gm", &email))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["applied"], 0,
        "zero queued ops in evaluation mode: {report}"
    );
    assert_eq!(
        report["blocked"], 0,
        "there was nothing to block either: {report}"
    );

    let after: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let keywords = after.as_array().unwrap()[0]["keywords"].as_array().unwrap();
    assert_eq!(
        keywords.len(),
        1,
        "the tag is still visible locally: {after}"
    );

    // A fresh profile — a stand-in for the server's own view — never saw it.
    let fresh_dir = temp_profile_dir("eval-fresh");
    let fresh = open_profile(&fresh_dir).expect("open fresh profile");
    fresh.sync_imap_json(&config).expect("fresh sync");
    let fresh_list: serde_json::Value =
        serde_json::from_str(&fresh.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap()).unwrap();
    assert!(
        fresh_list.as_array().unwrap()[0]["keywords"]
            .as_array()
            .unwrap()
            .is_empty(),
        "an evaluation-mode tag must never reach the server: {fresh_list}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&fresh_dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn mute_marks_reply_read() {
    let email = common::provision(&common::run_tag("tags-mute"));
    let tag = common::run_tag("mute");
    let subj = format!("castl-mute-{tag}");
    common::smtp_deliver(
        "probe@local.test",
        &email,
        &format!(
            "From: probe@local.test\r\nTo: {email}\r\nSubject: {subj}\r\n\
             Date: Thu, 01 Jan 2026 00:00:00 +0000\r\nMessage-ID: <mute-{tag}@local.test>\r\n\r\n\
             Mute e2e probe body.\r\n"
        ),
    );

    let dir = temp_profile_dir("mute");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("sync");

    let list: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let seeded = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subj.as_str()))
        .expect("probe present after sync");
    let id = seeded["id"].as_str().unwrap().to_owned();

    profile.set_thread_muted(&id, true).expect("mute");
    let muted: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    assert_eq!(
        muted
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap()["thread_muted"],
        true
    );

    // Deliver a reply via SMTP with In-Reply-To pointing at the seeded
    // message, then resync — the automatic mark-read-on-arrival path (S42
    // Task 6) must mark it read before any S40 rule sees it.
    common::smtp_deliver(
        "replier@local.test",
        &email,
        &format!(
            "From: replier@local.test\r\nTo: {email}\r\nSubject: Re: {subj}\r\n\
             Date: Thu, 01 Jan 2026 00:01:00 +0000\r\nMessage-ID: <mute-reply-{tag}@local.test>\r\n\
             In-Reply-To: <mute-{tag}@local.test>\r\nReferences: <mute-{tag}@local.test>\r\n\r\n\
             Reply while muted.\r\n"
        ),
    );
    profile.sync_imap_json(&config).expect("resync after reply");

    let after: serde_json::Value =
        serde_json::from_str(&profile.list_messages_json("gm", "gm:INBOX", 50, 0).unwrap())
            .unwrap();
    let reply = after
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(format!("Re: {subj}").as_str()))
        .expect("reply present after resync");
    assert_eq!(
        reply["is_read"], true,
        "a reply to a muted thread arrives already read: {reply}"
    );
    assert_eq!(
        reply["thread_muted"], true,
        "the reply carries the muted glyph too: {reply}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
