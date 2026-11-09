// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end identity/alias send test against a local GreenMail server (S47).
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test identity_send_greenmail -- --ignored --nocapture

mod common;

use castellum_provider_imap::session::{FetchItem, SessionSearch};
use std::net::TcpStream;

/// The verification oracle: a raw `Session`, never `ImapProvider`, so the
/// assertion is independent of the code under test. See `common::imap_login`.
fn imap_login(user: &str, pass: &str) -> castellum_provider_imap::session::Session<TcpStream> {
    common::imap_login(user, pass)
}

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-identity-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A draft with `from_email`/`from_name` set to a chosen identity (not the
/// account's own address) is delivered with THAT address as the From header
/// — proving apply_send's per-draft override (Task 4) end to end over the
/// real SMTP stack, and that the identity CRUD (create_identity_json) and
/// the send path agree on the same email string.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn draft_with_an_identity_sends_from_the_alias_not_the_account_address() {
    let dir = temp_profile_dir("identity");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("identity-send");
    let tag = common::run_tag("identity");
    let alias = format!("alias-{tag}@local.test");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    // Settings > Accounts would create this; not strictly required for the
    // send path itself (from_email/from_name are denormalized straight onto
    // the draft — migration 0026's doc comment), but proves the two agree.
    let created = profile
        .create_identity_json(
            &serde_json::json!({"account_id": "gm", "email": alias, "display_name": "Alias Person"})
                .to_string(),
        )
        .expect("create identity");
    let created: serde_json::Value = serde_json::from_str(&created).unwrap();
    assert_eq!(created["email"], alias.as_str());

    let subject = format!("castellum-identity-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "subject": subject,
        "body_text": "Sent from an alias.\n",
        "from_email": alias,
        "from_name": "Alias Person",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("send_draft");

    // The SMTP endpoint/credentials are still the ACCOUNT's own (aliases
    // send THROUGH the account's SMTP server — they don't need their own
    // credentials); only from_email/from_name in SendConfig are the
    // account's address, which apply_send's per-draft override replaces.
    let send_config = serde_json::json!({
        "account_id": "gm",
        "from_email": email,
        "from_name": "Castellum E2E",
        "smtp": {
            "host": common::host(),
            "port": common::SMTP_PORT,
            "username": email,
            "password": email,
            "use_tls": false,
            "allow_invalid_certs": false
        }
    })
    .to_string();
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(report["sent"], 1, "delivered: {report}");

    let mut session = imap_login(&email, &email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 1);
    let uid = uids[0];
    let fetches = session
        .uid_fetch(&uid.to_string(), &[FetchItem::BodyPeek])
        .unwrap();
    let raw = String::from_utf8_lossy(fetches[0].body.as_deref().unwrap()).to_string();
    let _ = session.logout();

    let from_line = raw
        .lines()
        .find(|l| l.starts_with("From:"))
        .expect("From header present");
    assert!(from_line.contains(&alias), "From header: {from_line}");
    assert!(
        !from_line.contains(&email),
        "From header must be the alias, not the account address: {from_line}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
