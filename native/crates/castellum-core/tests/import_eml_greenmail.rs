// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - .eml import e2e (S49): local file -> queued operation ->
// real IMAP APPEND -> visible on next sync. Against real GreenMail.
// #[ignore] by default. Run:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test import_eml_greenmail -- --ignored --nocapture

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-import-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn write_eml(dir: &std::path::Path, name: &str, content: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().into_owned()
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn imported_eml_appends_to_the_server_and_appears_on_the_next_sync() {
    let email = common::provision("import-eml");
    let tag = common::run_tag("import");
    let subject = format!("castl-import-{tag}");

    let dir = temp_profile_dir("import");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    // Establish the account/mailbox rows + a sync cursor (import targets an
    // existing mailbox_id the same way the app always has one loaded).
    profile.sync_imap_json(&config).expect("initial sync");
    // New accounts start in evaluation mode (fail-closed) — this e2e
    // exercises the real APPEND, so opt in explicitly, same as every other
    // replay-exercising GreenMail test (imap_greenmail.rs).
    profile.set_account_write_enabled("gm", true).unwrap();

    let eml_content = format!(
        "From: someone@local.test\r\nTo: {email}\r\nSubject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\r\nImported body.\r\n"
    );
    let file_path = write_eml(dir.as_path(), "probe.eml", &eml_content);

    let queued: serde_json::Value = serde_json::from_str(
        &profile
            .import_eml_json("gm", "gm:INBOX", &file_path)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued["status"], "queued", "queue result: {queued}");

    // Replay: real APPEND against GreenMail.
    let report: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&config).unwrap()).unwrap();
    assert_eq!(report["applied"], 1, "report: {report}");
    assert_eq!(report["failed"], 0, "report: {report}");

    // Re-sync: the appended copy is now a real message with the subject.
    profile.sync_imap_json(&config).expect("resync");
    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 500, 0)
        .unwrap();
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|m| m["subject"].as_str() == Some(subject.as_str())),
        "imported message visible after the next sync"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn importing_an_eml_with_no_message_id_still_appends_and_syncs_back() {
    let email = common::provision("import-eml-nomsgid");
    let tag = common::run_tag("importnomsgid");
    let subject = format!("castl-import-nomsgid-{tag}");

    let dir = temp_profile_dir("import-nomsgid");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("initial sync");
    profile.set_account_write_enabled("gm", true).unwrap();

    // Deliberately no Message-ID header — exercises the synthesize+inject path.
    let eml_content = format!(
        "From: someone@local.test\r\nTo: {email}\r\nSubject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\r\nNo id here.\r\n"
    );
    let file_path = write_eml(dir.as_path(), "nomsgid.eml", &eml_content);

    let queued: serde_json::Value = serde_json::from_str(
        &profile
            .import_eml_json("gm", "gm:INBOX", &file_path)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued["status"], "queued");

    let report: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&config).unwrap()).unwrap();
    assert_eq!(report["applied"], 1, "report: {report}");

    profile.sync_imap_json(&config).expect("resync");
    let list: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm", "gm:INBOX", 500, 0)
            .unwrap(),
    )
    .unwrap();
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["subject"].as_str() == Some(subject.as_str())));

    let _ = std::fs::remove_dir_all(&dir);
}
