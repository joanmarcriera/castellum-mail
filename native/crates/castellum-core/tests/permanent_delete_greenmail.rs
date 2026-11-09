// Castellum - permanent delete from Trash, end-to-end against GreenMail.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// S73 /. Delete on a message that is already in Trash queues a
// single-message `delete_permanent` op (UID STORE +FLAGS \Deleted, then
// UID EXPUNGE). This proves the message really disappears from the server's
// Trash, that a sibling in Trash survives, and that Delete in a non-Trash
// mailbox is refused as a permanent delete.
//
// #[ignore]d: needs the GreenMail container from scripts/local-imap. Each
// test provisions its own account, so the suite is parallel-safe.
//   cargo test -p castellum-core --test permanent_delete_greenmail -- --ignored

mod common;

use castellum_core::open_profile;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn probe(to: &str, subject: &str, tag: &str) -> Vec<u8> {
    format!(
        "From: sender@local.test\r\nTo: {to}\r\nSubject: {subject}\r\n\
         Message-ID: <{tag}@castl.test>\r\nDate: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         \r\nBody {tag}.\r\n"
    )
    .into_bytes()
}

fn find_id(list_json: &str, subject: &str) -> Option<String> {
    let list: serde_json::Value = serde_json::from_str(list_json).unwrap();
    list.as_array()?
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject))
        .and_then(|m| m["id"].as_str().map(str::to_string))
}

/// Number of messages the SERVER holds in `mailbox` (independent oracle).
fn server_count(email: &str, mailbox: &str) -> u32 {
    let mut s = common::imap_session(email);
    let n = s.select(mailbox).expect("select").exists;
    let _ = s.logout();
    n
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn permanent_delete_expunges_only_the_chosen_trash_message() {
    // A fresh account per run (name carries the unique tag): the assertions
    // below count the SERVER's Trash, which must start empty.
    let tag = common::run_tag("permdel");
    let email = common::provision(&tag);
    let subj_gone = format!("castl-permdel-gone-{tag}");
    let subj_kept = format!("castl-permdel-kept-{tag}");
    {
        let mut s = common::imap_session(&email);
        s.append(
            "INBOX",
            &[],
            &probe(&email, &subj_gone, &format!("{tag}-a")),
        )
        .expect("append a");
        s.append(
            "INBOX",
            &[],
            &probe(&email, &subj_kept, &format!("{tag}-b")),
        )
        .expect("append b");
        let _ = s.logout();
    }

    let dir = temp_profile_dir("permdel");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("first sync");
    profile.set_account_write_enabled("gm", true).unwrap();

    let inbox = profile
        .list_messages_json("gm", "gm:INBOX", 500, 0)
        .unwrap();
    let id_gone = find_id(&inbox, &subj_gone).expect("probe a synced");
    let id_kept = find_id(&inbox, &subj_kept).expect("probe b synced");

    // Delete outside Trash still just moves; a permanent delete is refused.
    assert!(
        profile.delete_message_permanently(&id_gone).is_err(),
        "permanent delete must be refused outside Trash"
    );
    profile.delete_message(&id_gone).unwrap();
    profile.delete_message(&id_kept).unwrap();
    let r: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&config).unwrap()).unwrap();
    assert_eq!(r["applied"], 2, "moves to Trash replay: {r}");
    assert_eq!(server_count(&email, "Trash"), 2, "both now in server Trash");

    // Re-sync (INBOX + Trash; the helper config filters to INBOX) so both
    // appear locally under Trash.
    let mut cfg: serde_json::Value = serde_json::from_str(&config).unwrap();
    cfg["mailboxes"] = serde_json::json!(["INBOX", "Trash"]);
    let config = cfg.to_string();
    profile.sync_imap_json(&config).expect("resync");
    let trash_id = "gm:Trash";
    let trash = profile.list_messages_json("gm", trash_id, 500, 0).unwrap();
    let trashed_gone = find_id(&trash, &subj_gone).expect("gone probe is in local Trash");
    let trashed_kept = find_id(&trash, &subj_kept).expect("kept probe is in local Trash");

    // Delete inside Trash: queued, local row stays until the server confirms.
    profile.delete_message_permanently(&trashed_gone).unwrap();
    let still = profile.list_messages_json("gm", trash_id, 500, 0).unwrap();
    assert!(
        find_id(&still, &subj_gone).is_some(),
        "row kept until replay"
    );

    let r: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&config).unwrap()).unwrap();
    assert_eq!(r["applied"], 1, "permanent delete applied: {r}");
    assert_eq!(r["failed"], 0, "{r}");

    // Server truth: exactly one message left in Trash.
    assert_eq!(server_count(&email, "Trash"), 1, "one expunged, one kept");
    // Local truth: the row is gone, the sibling is untouched.
    let after = profile.list_messages_json("gm", trash_id, 500, 0).unwrap();
    assert!(find_id(&after, &subj_gone).is_none());
    assert_eq!(find_id(&after, &subj_kept), Some(trashed_kept));
    // And a fresh sync does not resurrect it.
    profile.sync_imap_json(&config).expect("final sync");
    let fin = profile.list_messages_json("gm", trash_id, 500, 0).unwrap();
    assert!(find_id(&fin, &subj_gone).is_none(), "not resurrected");
}
