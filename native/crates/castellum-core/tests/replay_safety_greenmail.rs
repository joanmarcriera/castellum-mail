// Castellum - operation-replay data-safety GreenMail e2e.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Server-side proof for the 2026-10-12 replay audit findings, against a real
// (embedded) IMAP/SMTP server, checked with a raw IMAP `Session` as an
// independent oracle (never `ImapProvider`, which is what is under test):
//
//   #3370  a queued server-draft cleanup, replayed after the Drafts mailbox
//          was rebuilt (new UIDVALIDITY), must not delete the unrelated
//          message that now holds the same UID;
//   #3368  a queued Delete (move to Trash), replayed after its source
//          mailbox was rebuilt, must not move the message now at that UID;
//   #3369  an instant send that was mid-submission when the app died is
//          never submitted again by itself, only by an explicit Send Now;
//   #3367  a draft save queued under account A, replayed after the user
//          switched the draft's From to account B, never lands in A's Drafts.
//
// Each test provisions its own GreenMail account(s) (delivery auto-creates
// them; IMAP login alone does not), so the tests are parallel-safe.
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test replay_safety_greenmail -- --ignored --nocapture

mod common;

use castellum_provider_imap::session::{SessionFlag, SessionSearch};

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-replay-safety-e2e-{tag}-{}",
        std::process::id()
    ));
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

fn send_config(account_id: &str, email: &str) -> String {
    serde_json::json!({
        "account_id": account_id,
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

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("valid JSON from the core")
}

/// One tagged IMAP command on a raw socket; returns the tagged status line.
/// For RENAME, which the oracle `Session` (deliberately minimal) lacks.
fn raw_imap(email: &str, command: &str) -> String {
    use std::io::{BufRead, BufReader, Write};
    let stream = std::net::TcpStream::connect((common::host().as_str(), common::IMAP_PORT))
        .expect("connect GreenMail IMAP");
    let mut reader = BufReader::new(stream.try_clone().expect("clone IMAP stream"));
    let mut writer = stream;
    let mut run = |tag: &str, command: &str| -> String {
        writer
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .expect("write IMAP command");
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).expect("read IMAP reply");
            assert!(n > 0, "connection closed during {command}");
            if line.starts_with(tag) {
                return line;
            }
        }
    };
    // The untagged greeting is skipped by `run`, like any untagged line.
    let login = run("a1", &format!("LOGIN \"{email}\" \"{email}\""));
    assert!(login.starts_with("a1 OK"), "LOGIN: {login}");
    let status = run("a2", command);
    let _ = run("a3", "LOGOUT");
    status
}

static REBUILDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Replace `path` on the server with a brand-new, empty mailbox of the same
/// name, as a rebuild or provider migration would, and return a fresh oracle
/// session plus the new mailbox's UIDVALIDITY.
///
/// Done as RENAME-away + CREATE, not DELETE + CREATE: GreenMail's DELETE
/// notifies every session that ever selected the mailbox, fails on the ones
/// that have since disconnected (Castellum's own sync/replay connections),
/// and then drops the deleting connection with the mailbox still in place.
fn rebuild_mailbox(
    email: &str,
    path: &str,
) -> (
    castellum_provider_imap::session::Session<std::net::TcpStream>,
    u32,
) {
    let n = REBUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let retired = format!("{path}-retired-{}", common::run_tag(&n.to_string()));
    // NO (the mailbox does not exist yet) is fine: CREATE then makes it.
    let _ = raw_imap(email, &format!("RENAME \"{path}\" \"{retired}\""));
    // GreenMail's UIDVALIDITY is the mailbox's creation time in seconds, so
    // a mailbox created within the same second as its predecessor would
    // share the value and the reset would be invisible.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut session = common::imap_login(email, email);
    session.create(path).expect("create the new mailbox");
    let selected = session.select(path).expect("select the new mailbox");
    assert_eq!(selected.exists, 0, "a rebuilt mailbox starts empty");
    let uid_validity = selected.uid_validity.expect("UIDVALIDITY");
    (session, uid_validity)
}

fn raw_message(message_id: &str, subject: &str) -> Vec<u8> {
    format!(
        "From: someone@local.test\r\nTo: me@local.test\r\nMessage-ID: {message_id}\r\n\
         Subject: {subject}\r\n\r\nnot Castellum's to touch\r\n"
    )
    .into_bytes()
}

/// #3370. The cleanup of a superseded/discarded server draft is a permanent
/// delete by bare UID. After Drafts is rebuilt on the server that UID is a
/// different message (here: a draft "written on the phone").
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn draft_cleanup_after_a_drafts_uidvalidity_reset_spares_the_message_now_at_that_uid() {
    let dir = temp_profile_dir("uv-drafts");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("replay-safety-uv-drafts");
    let tag = common::run_tag("uv-drafts");
    profile
        .sync_imap_json(&common::sync_config_json("gm", &email))
        .expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");
    let replay = replay_config("gm", &email);

    // A freshly created Drafts, so the first UID handed out is predictable
    // both now and after the reset below.
    let (mut session, v1) = rebuild_mailbox(&email, "Drafts");
    let _ = session.logout();

    // Castellum mirrors a draft to the server.
    let draft_id = profile
        .save_draft_json(
            &serde_json::json!({
                "account_id": "gm",
                "to": [],
                "subject": format!("castellum-replay-safety-{tag} mine"),
                "body_text": "my draft\n",
            })
            .to_string(),
        )
        .expect("save draft");
    let from = serde_json::json!({ "from_email": email }).to_string();
    profile
        .save_draft_to_server_json(draft_id, &from)
        .expect("queue server save");
    let saved = json(&profile.replay_operations_json(&replay).unwrap());
    assert_eq!(saved["applied"], 1, "{saved}");
    let drafts = json(&profile.list_drafts_json("gm").unwrap());
    let mine = drafts
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft_id)
        .expect("draft listed");
    let mirror_uid = mine["server_uid"].as_i64().expect("mirror uid recorded");
    assert_eq!(mine["server_uid_validity"], v1 as i64);

    // The server rebuilds Drafts (migration, reindex): new UIDVALIDITY, and
    // a draft from another client now holds the UID Castellum remembers.
    let (mut session, v2) = rebuild_mailbox(&email, "Drafts");
    assert_ne!(
        v1, v2,
        "precondition: a recreated mailbox must get a new UIDVALIDITY"
    );
    let phone_id = format!("<phone-{tag}@local.test>");
    let phone = session
        .append_get_uid(
            "Drafts",
            &[SessionFlag::Draft],
            &raw_message(&phone_id, "written on the phone"),
        )
        .expect("append phone draft")
        .expect("APPENDUID");
    assert_eq!(
        phone.uid as i64, mirror_uid,
        "precondition: the stranger must hold exactly the UID the cleanup names"
    );

    // The user discards the local draft: a cleanup for (Drafts, uid, v1).
    profile.delete_draft(draft_id).expect("discard draft");
    let report = json(&profile.replay_operations_json(&replay).unwrap());
    assert_eq!(report["failed"], 0, "an orphan is not a failure: {report}");

    session.select("Drafts").expect("re-select Drafts");
    assert_eq!(
        session.uid_search_by_message_id(&phone_id).unwrap().len(),
        1,
        "the phone's draft must survive a cleanup aimed at the old UID space"
    );
    let pending = json(&profile.pending_operations_json("gm").unwrap());
    assert_eq!(
        pending.as_array().unwrap().len(),
        0,
        "nothing left to retry: {pending}"
    );
    let _ = session.logout();
    let _ = std::fs::remove_dir_all(&dir);
}

/// #3368. Delete is a move by (path, uid) queued for a later replay. After
/// the source mailbox is rebuilt that pair names someone else's message.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn delete_after_a_uidvalidity_reset_never_moves_the_message_now_at_that_uid() {
    let dir = temp_profile_dir("uv-move");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("replay-safety-uv-move");
    let tag = common::run_tag("uv-move");
    let folder = format!("Rebuilt-{tag}");

    let mut session = common::imap_login(&email, &email);
    session.create(&folder).expect("create folder");
    let v1 = session
        .select(&folder)
        .expect("select folder")
        .uid_validity
        .expect("UIDVALIDITY");
    let mine_id = format!("<mine-{tag}@local.test>");
    session
        .append(&folder, &[], &raw_message(&mine_id, "the one I delete"))
        .expect("append mine");
    // Sync runs on its own connection; the oracle reconnects afterwards.
    let _ = session.logout();

    let sync_config = serde_json::json!({
        "account_id": "gm",
        "email": email,
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false,
        "mailboxes": ["INBOX", folder]
    })
    .to_string();
    profile.sync_imap_json(&sync_config).expect("sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");
    let listed = json(
        &profile
            .list_messages_json("gm", &format!("gm:{folder}"), 10, 0)
            .unwrap(),
    );
    let message_id = listed[0]["id"]
        .as_str()
        .expect("synced message")
        .to_string();

    // The user presses Delete. Replay is scheduled, not immediate.
    profile.delete_message(&message_id).expect("queue delete");

    // Meanwhile the server rebuilds the folder; a different message now
    // holds the UID the queued move names.
    let (mut session, v2) = rebuild_mailbox(&email, &folder);
    assert_ne!(
        v1, v2,
        "precondition: a recreated mailbox must get a new UIDVALIDITY"
    );
    let stranger_id = format!("<stranger-{tag}@local.test>");
    session
        .append(&folder, &[], &raw_message(&stranger_id, "someone else's"))
        .expect("append stranger");
    session.select("INBOX").expect("leave folder");

    let report = json(
        &profile
            .replay_operations_json(&replay_config("gm", &email))
            .unwrap(),
    );
    assert_eq!(report["applied"], 0, "{report}");
    assert_eq!(report["failed"], 1, "{report}");

    session.select(&folder).expect("re-select folder");
    assert_eq!(
        session
            .uid_search_by_message_id(&stranger_id)
            .unwrap()
            .len(),
        1,
        "the stranger's message must still be where it was"
    );
    if session.select("Trash").is_ok() {
        assert!(
            session
                .uid_search_by_message_id(&stranger_id)
                .unwrap()
                .is_empty(),
            "and must not have been moved to Trash"
        );
    }

    // Refused for good, with a reason the operations list can show.
    let pending = json(&profile.pending_operations_json("gm").unwrap());
    let op = &pending[0];
    assert_eq!(op["kind"], "move");
    assert_eq!(op["status"], "failed_permanent", "{op}");
    assert!(
        op["last_error"]
            .as_str()
            .unwrap()
            .contains("was rebuilt on the server"),
        "{op}"
    );
    let _ = session.delete(&folder);
    let _ = session.logout();
    let _ = std::fs::remove_dir_all(&dir);
}

/// #3369. The SMTP server accepted the message, then the app died before it
/// could record that. On relaunch the send must NOT go out a second time.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn instant_send_in_flight_at_a_crash_reaches_the_recipient_at_most_once() {
    let dir = temp_profile_dir("double-send");
    let email = common::provision("replay-safety-double-send");
    let tag = common::run_tag("double-send");
    let subject = format!("castellum-replay-safety-{tag} once");
    let send = send_config("gm", &email);

    let draft_id;
    {
        // "First session": the user presses Send (no undo window).
        let profile = castellum_core::open_profile(&dir).expect("open profile");
        profile
            .sync_imap_json(&common::sync_config_json("gm", &email))
            .expect("first sync");
        profile
            .set_account_write_enabled("gm", true)
            .expect("enable writes");
        profile.reconcile_undo_sends_json("gm").expect("reconcile");
        draft_id = profile
            .save_draft_json(
                &serde_json::json!({
                    "account_id": "gm",
                    "to": [{"name": "Self", "email": email}],
                    "subject": subject,
                    "body_text": "Sent exactly once.\n",
                })
                .to_string(),
            )
            .expect("save draft");
        profile.send_draft(draft_id).expect("send_draft");
    }
    {
        // The drain claimed the op (`running`), the server accepted DATA,
        // and the process died before the status write. Reproduced as its
        // two halves: the claim, and the delivery the dead process made.
        let db = castellum_db::Database::open(&dir.join("castellum.db")).expect("open db");
        let op = db.pending_operations("gm").expect("pending").remove(0);
        assert_eq!(op.kind, "send");
        assert!(db.begin_operation_attempt(op.id).expect("claim"));
        common::smtp_deliver(
            &email,
            &email,
            &format!("Subject: {subject}\r\n\r\nSent exactly once.\r\n"),
        );
    }

    let count_delivered = || {
        let mut session = common::imap_login(&email, &email);
        session.select("INBOX").unwrap();
        let n = session
            .uid_search_all(&[SessionSearch::Header {
                field: "Subject",
                value: &subject,
            }])
            .unwrap()
            .len();
        let _ = session.logout();
        n
    };
    assert_eq!(count_delivered(), 1, "the interrupted attempt's copy");

    // "Relaunch": drains run, as they do at launch and on every trigger.
    let relaunch = castellum_core::open_profile(&dir).expect("reopen profile");
    for _ in 0..3 {
        let report = json(&relaunch.send_pending_json(&send).unwrap());
        assert_eq!(report["sent"], 0, "{report}");
    }
    assert_eq!(
        count_delivered(),
        1,
        "a send whose outcome is unknown must not be submitted again"
    );
    let ops = json(&relaunch.pending_operations_json("gm").unwrap());
    assert_eq!(ops[0]["status"], "held", "{ops}");
    assert!(
        ops[0]["last_error"]
            .as_str()
            .unwrap()
            .starts_with("may have been sent"),
        "{ops}"
    );
    let drafts = json(&relaunch.list_drafts_json("gm").unwrap());
    assert_eq!(
        drafts[0]["status"], "outbox",
        "kept for the user's decision"
    );

    // The user's explicit Send Now is the only thing that submits it.
    let released = json(&relaunch.send_now_json(draft_id).unwrap());
    assert_eq!(released["released"], true, "{released}");
    let report = json(&relaunch.send_pending_json(&send).unwrap());
    assert_eq!(report["sent"], 1, "{report}");
    assert_eq!(
        count_delivered(),
        2,
        "sent again only because the user said so"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #3367. A draft save queued under account A is still waiting (offline)
/// when the user switches the draft's From to account B.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn draft_save_queued_before_a_from_switch_never_lands_in_the_old_accounts_drafts() {
    let dir = temp_profile_dir("from-switch");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email_a = common::provision("replay-safety-from-a");
    let email_b = common::provision("replay-safety-from-b");
    let tag = common::run_tag("from-switch");
    for (account, email) in [("gma", &email_a), ("gmb", &email_b)] {
        profile
            .sync_imap_json(&common::sync_config_json(account, email))
            .expect("first sync");
        profile
            .set_account_write_enabled(account, true)
            .expect("enable writes");
    }
    let mut session_a = common::imap_login(&email_a, &email_a);
    let _ = session_a.create("Drafts");
    let baseline_a = session_a.select("Drafts").expect("select A Drafts").exists;

    let subject = format!("castellum-replay-safety-{tag} from-switch");
    let draft_id = profile
        .save_draft_json(
            &serde_json::json!({
                "account_id": "gma",
                "to": [],
                "subject": subject,
                "body_text": "typed under account A\n",
            })
            .to_string(),
        )
        .expect("save draft under A");
    let queued = json(
        &profile
            .save_draft_to_server_json(
                draft_id,
                &serde_json::json!({ "from_email": email_a }).to_string(),
            )
            .unwrap(),
    );
    assert_eq!(queued["queued"], true);

    // From is switched to B before A's replay ever ran.
    profile
        .save_draft_json(
            &serde_json::json!({
                "id": draft_id,
                "account_id": "gmb",
                "to": [],
                "subject": subject,
                "body_text": "typed under account A, now B's\n",
            })
            .to_string(),
        )
        .expect("switch From to B");

    let report = json(
        &profile
            .replay_operations_json(&replay_config("gma", &email_a))
            .unwrap(),
    );
    assert_eq!(report["failed"], 0, "{report}");
    assert_eq!(
        session_a.select("Drafts").expect("re-select").exists,
        baseline_a,
        "the stale save must not be appended to account A's Drafts"
    );

    let drafts_b = json(&profile.list_drafts_json("gmb").unwrap());
    let moved = drafts_b
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft_id)
        .expect("the draft belongs to B now");
    assert!(
        moved["server_uid"].is_null() && moved["server_mailbox_path"].is_null(),
        "B's draft must not carry a UID from A's Drafts: {moved}"
    );
    for account in ["gma", "gmb"] {
        let pending = json(&profile.pending_operations_json(account).unwrap());
        assert_eq!(pending.as_array().unwrap().len(), 0, "{account}: {pending}");
    }
    let _ = session_a.logout();
    let _ = std::fs::remove_dir_all(&dir);
}
