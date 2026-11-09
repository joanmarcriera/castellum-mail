// Castellum - outgoing attachment send e2e (S33), against real GreenMail.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// #[ignore] by default: requires local GreenMail (scripts/local-imap). Run:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test attachment_send_greenmail -- --ignored --nocapture
//
// Two round trips through the real stack (SMTP delivery, IMAP re-fetch, and
// our own sync + hydrate + on-demand attachment download):
//   1. compose with a regular attachment -> send -> the recipient's copy
//      carries it byte for byte;
//   2. forward a message whose HTML carries an inline image plus a PDF -> both
//      arrive, the inline one still `is_inline` with its original Content-ID.

mod common;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use castellum_core::{open_profile, Profile};
use castellum_provider_imap::session::{FetchItem, SessionSearch};

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-attach-send-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn send_config(email: &str) -> String {
    serde_json::json!({
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
    .to_string()
}

/// Sync, find `subject` in the INBOX, hydrate its body, return its message id.
fn sync_and_hydrate(profile: &Profile, config: &str, subject: &str) -> String {
    profile.sync_imap_json(config).expect("sync");
    let list: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm", "gm:INBOX", 500, 0)
            .expect("list"),
    )
    .unwrap();
    let id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject))
        .unwrap_or_else(|| panic!("'{subject}' present after sync"))["id"]
        .as_str()
        .unwrap()
        .to_string();
    profile
        .hydrate_message_body_json(config, &id)
        .expect("hydrate body");
    id
}

fn attachments_of(profile: &Profile, message_id: &str) -> Vec<serde_json::Value> {
    let detail: serde_json::Value =
        serde_json::from_str(&profile.get_message_json(message_id).unwrap()).unwrap();
    detail["attachments"].as_array().unwrap().clone()
}

/// Download attachment `ordinal` through the real IMAP path; return its bytes.
fn download_bytes(profile: &Profile, config: &str, message_id: &str, ordinal: i64) -> Vec<u8> {
    let result: serde_json::Value = serde_json::from_str(
        &profile
            .download_attachment_json(config, message_id, ordinal)
            .expect("download"),
    )
    .unwrap();
    let path = result["local_path"]
        .as_str()
        .unwrap_or_else(|| panic!("downloaded: {result}"))
        .to_string();
    std::fs::read(path).expect("read cached attachment")
}

fn save_draft(profile: &Profile, to: &str, subject: &str, body: &str) -> i64 {
    profile
        .save_draft_json(
            &serde_json::json!({
                "account_id": "gm",
                "to": [{"email": to}],
                "subject": subject,
                "body_text": body,
            })
            .to_string(),
        )
        .expect("save draft")
}

fn queue_and_send(profile: &Profile, email: &str, draft_id: i64) {
    profile.send_draft(draft_id).expect("send_draft");
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config(email)).unwrap()).unwrap();
    assert_eq!(report["sent"], 1, "one draft delivered: {report}");
    assert_eq!(report["failed"], 0);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn compose_with_attachment_delivers_and_round_trips() {
    let dir = temp_profile_dir("compose");
    let profile = open_profile(&dir).expect("open profile");
    let email = common::provision("attach-send");
    let tag = common::run_tag("attach-send");
    let config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let subject = format!("castl-attach-send-{tag}");
    let draft_id = save_draft(&profile, &email, &subject, "Message with an attachment.\n");
    // Non-UTF-8 payload: must survive SMTP + IMAP + MIME decode byte for byte.
    let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    let list_json = profile
        .add_draft_attachment_json(
            draft_id,
            &serde_json::json!({
                "filename": "probe.bin",
                "content_type": "application/octet-stream",
                "data_base64": BASE64.encode(&payload),
            })
            .to_string(),
        )
        .expect("add attachment");
    let listed: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    assert_eq!(listed[0]["size_bytes"], 4096);

    queue_and_send(&profile, &email, draft_id);

    // Raw RFC822 over IMAP: a real multipart/mixed carrying the attachment.
    let mut session = common::imap_session(&email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 1, "delivered exactly once");
    let uid = uids[0];
    let fetches = session
        .uid_fetch(&uid.to_string(), &[FetchItem::BodyPeek])
        .unwrap();
    let raw = fetches[0].body.as_deref().unwrap().to_vec();
    let _ = session.logout();
    let raw_lower = String::from_utf8_lossy(&raw).to_ascii_lowercase();
    assert!(raw_lower.contains("multipart/mixed"));
    assert!(raw_lower.contains("probe.bin"));

    // Our own parser agrees: exactly one attachment, addressed by a section.
    let parsed = castellum_mail::parse_message(&raw);
    assert_eq!(parsed.attachments.len(), 1);
    assert_eq!(parsed.attachments[0].filename.as_deref(), Some("probe.bin"));
    assert!(!parsed.attachments[0].section.is_empty());

    // And through the whole read path: sync, hydrate, download, compare bytes.
    let message_id = sync_and_hydrate(&profile, &config, &subject);
    let attachments = attachments_of(&profile, &message_id);
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], "probe.bin");
    assert_eq!(download_bytes(&profile, &config, &message_id, 0), payload);

    let _ = std::fs::remove_dir_all(&dir);
}

/// multipart/mixed { multipart/related { text/html, image/png (cid) }, pdf }.
fn build_message_with_inline_image(to: &str, subject: &str, tag: &str) -> Vec<u8> {
    let png = BASE64.encode(b"\x89PNG-inline-image-bytes");
    let pdf = BASE64.encode(b"pdf attachment bytes");
    format!(
        "From: probe@local.test\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <attach-fwd-{tag}@local.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\
         \r\n\
         --OUT\r\n\
         Content-Type: multipart/related; boundary=\"REL\"\r\n\
         \r\n\
         --REL\r\n\
         Content-Type: text/html; charset=\"utf-8\"\r\n\
         \r\n\
         <p>Logo: <img src=\"cid:logo123\"></p>\r\n\
         --REL\r\n\
         Content-Type: image/png; name=\"logo.png\"\r\n\
         Content-Disposition: inline; filename=\"logo.png\"\r\n\
         Content-ID: <logo123>\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {png}\r\n\
         --REL--\r\n\
         --OUT\r\n\
         Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {pdf}\r\n\
         --OUT--\r\n"
    )
    .into_bytes()
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn forwarding_carries_inline_images_and_attachments() {
    let dir = temp_profile_dir("forward");
    let profile = open_profile(&dir).expect("open profile");
    let email = common::provision("attach-forward");
    let tag = common::run_tag("attach-fwd");
    let config = common::sync_config_json("gm", &email);

    let original_subject = format!("castl-fwd-orig-{tag}");
    {
        let mut session = common::imap_session(&email);
        session
            .append(
                "INBOX",
                &[],
                &build_message_with_inline_image(&email, &original_subject, &tag),
            )
            .expect("IMAP append");
        let _ = session.logout();
    }
    profile.sync_imap_json(&config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");
    let original_id = sync_and_hydrate(&profile, &config, &original_subject);
    let source = attachments_of(&profile, &original_id);
    assert_eq!(source.len(), 2, "inline image + pdf: {source:?}");

    // Forward: a fresh draft, attachments copied in over real IMAP.
    let fwd_subject = format!("Fwd: {original_subject}");
    let draft_id = save_draft(&profile, &email, &fwd_subject, "See attached.\n");
    let copied: serde_json::Value = serde_json::from_str(
        &profile
            .forward_attachments_json(&config, &original_id, draft_id)
            .expect("forward attachments"),
    )
    .unwrap();
    assert_eq!(copied.as_array().unwrap().len(), 2, "{copied}");
    let inline = copied
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["filename"] == "logo.png")
        .expect("inline image copied");
    assert_eq!(inline["is_inline"], true);
    assert_eq!(inline["content_id"], "logo123");

    queue_and_send(&profile, &email, draft_id);

    // The forwarded message's own structure, read back through our parser.
    let fwd_id = sync_and_hydrate(&profile, &config, &fwd_subject);
    let arrived = attachments_of(&profile, &fwd_id);
    assert_eq!(arrived.len(), 2, "{arrived:?}");
    let logo = arrived
        .iter()
        .find(|a| a["filename"] == "logo.png")
        .expect("forwarded inline image");
    assert_eq!(logo["is_inline"], true, "still inline: {logo}");
    assert_eq!(logo["content_id"], "logo123", "Content-ID intact: {logo}");
    let pdf = arrived
        .iter()
        .find(|a| a["filename"] == "doc.pdf")
        .expect("forwarded pdf");
    assert_eq!(pdf["is_inline"], false);

    // Bytes survive the whole loop: original -> draft blob -> SMTP -> IMAP.
    let logo_ordinal = logo["ordinal"].as_i64().unwrap();
    let pdf_ordinal = pdf["ordinal"].as_i64().unwrap();
    assert_eq!(
        download_bytes(&profile, &config, &fwd_id, logo_ordinal),
        b"\x89PNG-inline-image-bytes"
    );
    assert_eq!(
        download_bytes(&profile, &config, &fwd_id, pdf_ordinal),
        b"pdf attachment bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
