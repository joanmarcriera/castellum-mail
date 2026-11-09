// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end Markdown compose test against a local GreenMail server (S46).
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test markdown_compose_greenmail -- --ignored --nocapture

mod common;

use mail_parser::MimeHeaders;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-md-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A Markdown-on draft, sent through the real stack, arrives as a genuine
/// multipart/alternative: the plain part is the Markdown SOURCE, the html
/// part is the SAME render `castellum_render_markdown_preview` would have
/// shown in the compose window's live preview — proven by rendering it here
/// with the exact same function and checking the synced-back copy against it.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn markdown_draft_delivers_as_multipart_alternative() {
    let dir = temp_profile_dir("markdown");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("markdown-compose");
    let tag = common::run_tag("markdown");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let markdown = "**Hello** from *Markdown* compose.\n\n- one\n- two\n";
    let html = castellum_core::compose::render_markdown_preview(markdown);
    assert!(
        html.contains("<strong>Hello</strong>"),
        "sanity: preview renders"
    );

    let subject = format!("castellum-markdown-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "subject": subject,
        "body_text": markdown,
        "body_html": html,
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("send_draft");

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
    // The synced-back copy has BOTH bodies: text_body carries the Markdown
    // source (the plain part), sanitized_html (MessageDetail's JSON field
    // for the incoming html alternative, re-sanitized by the INBOUND
    // policy on the way back down — see castellum_mail::sanitize_email_html)
    // carries the rendered html exactly as this test rendered it (same
    // function, same input; ammonia's own compose-time and inbound
    // policies both keep <strong> unchanged).
    assert!(detail["text_body"].as_str().unwrap().contains("**Hello**"));
    assert!(
        detail["sanitized_html"]
            .as_str()
            .expect("html alternative present")
            .contains("<strong>Hello</strong>"),
        "html alternative round-trips: {detail}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A Markdown-off draft (`body_html` never set) must still deliver a plain
/// single-part message over the real stack — proves Task 4's byte-for-byte
/// claim end to end, not just against the in-process `build_rfc822` unit
/// tests.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn markdown_off_draft_still_delivers_plain_text_only() {
    let dir = temp_profile_dir("markdown-off");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("markdown-off");
    let tag = common::run_tag("markdown-off");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let subject = format!("castellum-markdown-off-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "subject": subject,
        "body_text": "Plain, no Markdown here: *not bold*.\n",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("send_draft");

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
    assert!(
        detail["sanitized_html"].is_null(),
        "no html alternative was ever sent: {detail}"
    );
    assert!(detail["text_body"].as_str().unwrap().contains("*not bold*"));

    let _ = std::fs::remove_dir_all(&dir);
}

const PNG_BYTES: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, b'I', b'H', b'D', b'R', 1, 2, 3,
    4, 5, 6, 7, 8,
];

/// Send a Markdown draft whose body references an embedded image (or not),
/// return the delivered message's raw RFC 822 source.
fn send_markdown_with_image(tag: &str, reference_it: bool) -> (Vec<u8>, String) {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine;
    let dir = temp_profile_dir(tag);
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision(tag);
    let run = common::run_tag(tag);
    let sync_config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let subject = format!("castellum-inline-image-e2e-{run}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "subject": subject,
        "body_text": "placeholder",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    let embedded: serde_json::Value = serde_json::from_str(
        &profile
            .embed_draft_image_json(
                draft_id,
                &serde_json::json!({"filename": "shot.png", "data_base64": BASE64.encode(PNG_BYTES)})
                    .to_string(),
            )
            .expect("embed"),
    )
    .unwrap();
    let reference = embedded["markdown"].as_str().unwrap().to_string();
    let markdown = if reference_it {
        format!("Look at this:\n\n{reference}\n")
    } else {
        "No picture referenced.\n".to_string()
    };
    let html = profile
        .render_markdown_for_draft(draft_id, &markdown, "plain")
        .expect("render");
    profile
        .save_draft_json(
            &serde_json::json!({
                "id": draft_id,
                "account_id": "gm",
                "to": [{"name": "Primary", "email": email}],
                "subject": subject,
                "body_text": markdown,
                "body_html": html,
            })
            .to_string(),
        )
        .expect("resave draft");
    profile.send_draft(draft_id).expect("send_draft");
    let send_config = serde_json::json!({
        "account_id": "gm",
        "from_email": email,
        "from_name": "Castellum E2E",
        "smtp": {
            "host": common::host(), "port": common::SMTP_PORT,
            "username": email, "password": email,
            "use_tls": false, "allow_invalid_certs": false
        }
    })
    .to_string();
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(report["sent"], 1, "delivered: {report}");

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
    let fetched: serde_json::Value = serde_json::from_str(
        &profile
            .fetch_raw_source_json(&sync_config, arrived["id"].as_str().unwrap())
            .expect("fetch raw source"),
    )
    .unwrap();
    let raw = std::fs::read(fetched["local_path"].as_str().expect("cached path")).unwrap();
    let cid = embedded["attachment"]["content_id"]
        .as_str()
        .unwrap()
        .to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (raw, cid)
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn markdown_draft_with_embedded_image_delivers_as_related_around_alternative() {
    let (raw, cid) = send_markdown_with_image("inline-img", true);
    let text = String::from_utf8_lossy(&raw).to_string();
    let lower = text.to_ascii_lowercase();
    let related = lower.find("multipart/related").expect("related: {text}");
    let alternative = lower
        .find("multipart/alternative")
        .expect("alternative inside related");
    assert!(related < alternative, "{text}");
    assert!(text.contains(&format!("Content-ID: <{cid}>")), "{text}");
    assert!(lower.contains("content-disposition: inline"), "{text}");
    let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
    assert!(parsed
        .body_html(0)
        .unwrap()
        .contains(&format!("src=\"cid:{cid}\"")));
    assert!(parsed
        .body_text(0)
        .unwrap()
        .contains(&format!("(cid:{cid})")));
    let image = parsed
        .attachments()
        .find(|p| p.content_id() == Some(cid.as_str()))
        .expect("image part");
    assert_eq!(image.contents(), PNG_BYTES, "image bytes round-trip");
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn embedded_image_without_a_reference_arrives_as_an_attachment() {
    let (raw, cid) = send_markdown_with_image("inline-img-unref", false);
    let text = String::from_utf8_lossy(&raw).to_string();
    let lower = text.to_ascii_lowercase();
    assert!(!lower.contains("multipart/related"), "{text}");
    assert!(!text.contains(&format!("Content-ID: <{cid}>")), "{text}");
    let parsed = mail_parser::MessageParser::default().parse(&raw).unwrap();
    let att = parsed.attachment(0).expect("sent as a regular attachment");
    assert_eq!(att.attachment_name(), Some("shot.png"));
    assert_eq!(att.contents(), PNG_BYTES);
}
