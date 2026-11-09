// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end format=flowed send test against a local GreenMail server
//.
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   GREENMAIL_HOST=127.0.0.1 cargo test -p castellum-core --test flowed_greenmail -- --ignored

mod common;

use castellum_provider_imap::session::{FetchItem, SessionSearch};

/// With `plain_text_flowed: true` in the send config a plain-text draft goes
/// out as `text/plain; format=flowed`; after the recipient syncs it, the raw
/// source declares flowed and the stored `text_body` is the composed
/// paragraphs again (S1's parse-time decode undoes the soft breaks).
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn flowed_send_round_trips_through_greenmail() {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-flowed-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("flowed-send");
    let tag = common::run_tag("flowed");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let para = "Aquest és un paràgraf llarg escrit d'una sola tirada que el client ha de \
                tallar amb salts suaus a setanta-sis columnes sense perdre res pel camí.";
    let body = format!("{para}\n\n> citació breu\n\nAdéu.\n");
    let subject = format!("castellum-flowed-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "subject": subject,
        "body_text": body,
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("send_draft");

    let send_config = serde_json::json!({
        "account_id": "gm",
        "from_email": email,
        "from_name": "Castellum E2E",
        "plain_text_flowed": true,
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

    // Raw source over IMAP declares flowed, and carries soft breaks.
    let mut session = common::imap_login(&email, &email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 1);
    let fetches = session
        .uid_fetch(&uids[0].to_string(), &[FetchItem::BodyPeek])
        .unwrap();
    let raw = String::from_utf8_lossy(fetches[0].body.as_deref().unwrap()).to_string();
    let _ = session.logout();
    assert!(
        raw.to_ascii_lowercase()
            .contains("content-type: text/plain; charset=utf-8; format=flowed"),
        "raw source:\n{raw}"
    );

    // Synced back, the stored body is the composed paragraphs.
    profile.sync_imap_json(&sync_config).expect("re-sync");
    let headers: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm", "gm:INBOX", 500, 0)
            .unwrap(),
    )
    .unwrap();
    let arrived = headers
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["subject"] == subject.as_str())
        .expect("sent message present after sync");
    let detail: serde_json::Value = serde_json::from_str(
        &profile
            .get_message_json(arrived["id"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let text = detail["text_body"].as_str().unwrap();
    assert_eq!(
        text.trim_end(),
        body.trim_end(),
        "stored text_body: {text:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
