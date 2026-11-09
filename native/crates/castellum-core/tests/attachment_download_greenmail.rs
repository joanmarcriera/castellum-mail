// Castellum - attachment on-demand download e2e (S32), against real GreenMail.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// #[ignore] by default: requires local GreenMail (scripts/local-imap). Run:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test attachment_download_greenmail -- --ignored --nocapture
//
// CI runs these against a GreenMail service container with
// GREENMAIL_HOST=<service>.

mod common;

use castellum_core::open_profile;

const PDF_BYTES_B64: &str = "aGVsbG8gd29ybGQ="; // "hello world"

/// multipart/mixed { text/plain, application/pdf } — the PDF is IMAP section 2.
fn build_probe_with_attachment(to: &str, subject: &str, tag: &str) -> Vec<u8> {
    format!(
        "From: probe@local.test\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <attach-probe-{tag}@local.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"B\"\r\n\
         \r\n\
         --B\r\n\
         Content-Type: text/plain; charset=\"utf-8\"\r\n\
         \r\n\
         Body text.\r\n\
         --B\r\n\
         Content-Type: application/pdf; name=\"probe.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"probe.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {PDF_BYTES_B64}\r\n\
         --B--\r\n"
    )
    .into_bytes()
}

/// multipart/mixed { multipart/alternative { text/plain, text/html }, pdf } —
/// exercises nested numbering: the alternative is section 1 (children 1.1 and
/// 1.2), so the PDF is section 2 even though it is the only attachment.
fn build_probe_with_nested_alternative(to: &str, subject: &str, tag: &str) -> Vec<u8> {
    format!(
        "From: probe@local.test\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <attach-nested-{tag}@local.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"OUT\"\r\n\
         \r\n\
         --OUT\r\n\
         Content-Type: multipart/alternative; boundary=\"ALT\"\r\n\
         \r\n\
         --ALT\r\n\
         Content-Type: text/plain; charset=\"utf-8\"\r\n\
         \r\n\
         Plain body.\r\n\
         --ALT\r\n\
         Content-Type: text/html; charset=\"utf-8\"\r\n\
         \r\n\
         <p>HTML body.</p>\r\n\
         --ALT--\r\n\
         --OUT\r\n\
         Content-Type: application/pdf; name=\"nested.pdf\"\r\n\
         Content-Disposition: attachment; filename=\"nested.pdf\"\r\n\
         Content-Transfer-Encoding: base64\r\n\
         \r\n\
         {PDF_BYTES_B64}\r\n\
         --OUT--\r\n"
    )
    .into_bytes()
}

fn append_raw(email: &str, mailbox: &str, raw: &[u8]) {
    let mut session = common::imap_session(email);
    session.append(mailbox, &[], raw).expect("IMAP append");
    let _ = session.logout();
}

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-attach-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Sync, find the probe by subject, hydrate its body, and return its message id.
fn sync_and_hydrate(profile: &castellum_core::Profile, config: &str, subject: &str) -> String {
    profile.sync_imap_json(config).expect("sync");
    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 500, 0)
        .expect("list");
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    let message_id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject))
        .unwrap_or_else(|| panic!("probe '{subject}' found after sync"))["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Hydrate the body so message_attachments (and its section addressing) is
    // populated — mirrors how the reading pane triggers hydration on
    // selection (AppModel.showDetail).
    profile
        .hydrate_message_body_json(config, &message_id)
        .expect("hydrate body");
    message_id
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn download_attachment_round_trips_and_then_hits_the_cache() {
    let email = common::provision("attach-download");
    let tag = common::run_tag("attach");
    let subject = format!("castl-attach-{tag}");
    append_raw(
        &email,
        "INBOX",
        &build_probe_with_attachment(&email, &subject, &tag),
    );

    let dir = temp_profile_dir("download");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    let message_id = sync_and_hydrate(&profile, &config, &subject);

    let detail_json = profile.get_message_json(&message_id).expect("detail");
    let detail: serde_json::Value = serde_json::from_str(&detail_json).unwrap();
    let attachments = detail["attachments"].as_array().expect("attachments array");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], "probe.pdf");
    assert_eq!(
        attachments[0]["section"], "2",
        "PDF is the 2nd top-level part"
    );

    // Not cached yet.
    let status_before = profile
        .attachment_cache_status_json(&message_id, 0)
        .expect("status");
    let status_before: serde_json::Value = serde_json::from_str(&status_before).unwrap();
    assert_eq!(status_before["status"], "not_cached");

    // Download: real network round trip against GreenMail.
    let download_json = profile
        .download_attachment_json(&config, &message_id, 0)
        .expect("download");
    let download: serde_json::Value = serde_json::from_str(&download_json).unwrap();
    assert_eq!(
        download["status"], "downloaded",
        "download result: {download}"
    );
    let local_path = download["local_path"].as_str().expect("local_path present");
    let bytes = std::fs::read(local_path).expect("cached file readable");
    assert_eq!(bytes, b"hello world", "transfer-decoded attachment bytes");

    // A second status check (no network) reports cached at the same path.
    let status_after = profile
        .attachment_cache_status_json(&message_id, 0)
        .expect("status after download");
    let status_after: serde_json::Value = serde_json::from_str(&status_after).unwrap();
    assert_eq!(status_after["status"], "cached");
    assert_eq!(status_after["local_path"].as_str(), Some(local_path));

    // A second download is served from the cache (status "cached", not
    // "downloaded") without needing to reach the server again.
    let again_json = profile
        .download_attachment_json(&config, &message_id, 0)
        .expect("second download");
    let again: serde_json::Value = serde_json::from_str(&again_json).unwrap();
    assert_eq!(again["status"], "cached");
    assert_eq!(again["local_path"].as_str(), Some(local_path));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn nested_multipart_section_numbering_matches_the_real_server() {
    let email = common::provision("attach-nested");
    let tag = common::run_tag("nested");
    let subject = format!("castl-attach-nested-{tag}");
    append_raw(
        &email,
        "INBOX",
        &build_probe_with_nested_alternative(&email, &subject, &tag),
    );

    let dir = temp_profile_dir("nested");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    let message_id = sync_and_hydrate(&profile, &config, &subject);

    let detail: serde_json::Value =
        serde_json::from_str(&profile.get_message_json(&message_id).unwrap()).unwrap();
    let attachments = detail["attachments"].as_array().expect("attachments array");
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["section"], "2");

    // The computed path must address the right bytes on a real server.
    let download: serde_json::Value = serde_json::from_str(
        &profile
            .download_attachment_json(&config, &message_id, 0)
            .expect("download"),
    )
    .unwrap();
    assert_eq!(
        download["status"], "downloaded",
        "download result: {download}"
    );
    let bytes = std::fs::read(download["local_path"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, b"hello world");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn download_for_a_message_from_another_account_is_refused_without_a_fetch() {
    let email = common::provision("attach-wrong-acct");
    let tag = common::run_tag("wrongacct");
    let subject = format!("castl-attach-wrongacct-{tag}");
    append_raw(
        &email,
        "INBOX",
        &build_probe_with_attachment(&email, &subject, &tag),
    );

    let dir = temp_profile_dir("wrongacct");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    let message_id = sync_and_hydrate(&profile, &config, &subject);

    // Same server credentials, but a config claiming a different account id.
    let other = common::sync_config_json("someone-else", &email);
    let result: serde_json::Value = serde_json::from_str(
        &profile
            .download_attachment_json(&other, &message_id, 0)
            .expect("download returns a typed failure, not an error"),
    )
    .unwrap();
    assert_eq!(result["status"], "failed");
    assert!(result["error"]
        .as_str()
        .unwrap()
        .contains("does not belong"));

    let _ = std::fs::remove_dir_all(&dir);
}
