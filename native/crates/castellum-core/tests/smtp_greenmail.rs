// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end compose/send test against a local GreenMail server.
//
// #[ignore] by default. The test provisions its own account (parallel-safe;
// no seeding needed). Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test smtp_greenmail -- --ignored --nocapture
//
// GreenMail (see scripts/local-imap): SMTP 3025 (plain), IMAP 3143 (plain);
// `-Dgreenmail.setup.test.all` auto-creates any recipient account on
// delivery, with login == password == email address.

mod common;

use castellum_provider_imap::session::{FetchItem, SessionSearch};
use std::net::TcpStream;

fn imap_login(user: &str, pass: &str) -> castellum_provider_imap::session::Session<TcpStream> {
    common::imap_login(user, pass)
}

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root()
        .join(format!("castellum-smtp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Full compose lifecycle over the real stack: save a draft, queue it, deliver
/// it through GreenMail's SMTP (:3025, plaintext), then prove — over IMAP —
/// that it arrived with the right subject/body/reply headers, that the Bcc
/// recipient got a copy without appearing in the headers, and that the local
/// draft/op state machine landed on sent/succeeded with an idempotent re-run.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn compose_send_delivers_and_round_trips() {
    let dir = temp_profile_dir("send");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("smtp-roundtrip");
    let tag = common::run_tag("smtp");
    let bcc_addr = format!("e2e-smtp-bcc-{tag}@local.test");
    let sync_config = common::sync_config_json("gm", &email);

    // 1. Sync once so the "gm" account exists locally (send ops FK-reference it).
    profile.sync_imap_json(&sync_config).expect("first sync");
    // This e2e sends and replays real ops, so leave evaluation mode.
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    // 2. Save a reply-shaped draft to ourselves, with a Bcc to a fresh address.
    let subject = format!("castellum-smtp-e2e-{tag}");
    let parent_id = format!("<castellum-e2e-parent-{tag}@local.test>");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Primary", "email": email}],
        "bcc": [{"email": bcc_addr}],
        "subject": subject,
        "body_text": "Hello from the Castellum compose e2e.\nSecond line.\n",
        "in_reply_to": parent_id,
        "references_header": parent_id,
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");

    // 3. Queue it: draft -> outbox + one pending `send` op.
    profile.send_draft(draft_id).expect("send_draft");
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(drafts[0]["status"], "outbox");
    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1);
    assert_eq!(ops[0]["kind"], "send");

    // 4. Deliver over real SMTP (plaintext :3025).
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
    assert_eq!(report["sent"], 1, "one draft delivered: {report}");
    assert_eq!(report["failed"], 0);

    // 5. Local state: draft is sent, the send op succeeded, and exactly one
    //    `sent_copy` op is queued for the IMAP replayer (servers don't file
    //    SMTP submissions into Sent — we APPEND our own copy).
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(drafts[0]["status"], "sent");
    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1, "sent_copy queued");
    assert_eq!(ops[0]["kind"], "sent_copy");
    let copy_payload: serde_json::Value =
        serde_json::from_str(ops[0]["payload_json"].as_str().unwrap()).unwrap();
    let sent_path = copy_payload["dest_path"].as_str().unwrap().to_string();
    let sent_msgid = copy_payload["message_id"].as_str().unwrap().to_string();
    assert!(sent_msgid.starts_with("<castellum."), "got {sent_msgid}");

    // 6. Idempotency: a second run has nothing to do (and does not double-send).
    let again: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(again["sent"], 0);
    assert_eq!(again["failed"], 0);

    // 7. Recipient's inbox via our own sync path: the message arrived.
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
        .expect("sent message present in the recipient's INBOX after sync");
    let detail: serde_json::Value = serde_json::from_str(
        &profile
            .get_message_json(arrived["id"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert!(
        detail["text_body"]
            .as_str()
            .unwrap()
            .contains("Hello from the Castellum compose e2e."),
        "body round-trips"
    );
    assert_eq!(detail["header"]["from_address"], email.as_str());
    assert_eq!(detail["header"]["from_name"], "Castellum E2E");

    // 8. Raw RFC822 over IMAP: reply headers round-trip; Bcc never appears.
    let mut session = imap_login(&email, &email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 1, "exactly one delivery to the To recipient");
    let uid = uids[0];
    let fetches = session
        .uid_fetch(&uid.to_string(), &[FetchItem::BodyPeek])
        .unwrap();
    let raw = String::from_utf8_lossy(fetches[0].body.as_deref().unwrap()).to_string();
    let _ = session.logout();

    assert!(
        raw.contains(&format!("In-Reply-To: {parent_id}")),
        "In-Reply-To round-trips, raw:\n{raw}"
    );
    assert!(raw.contains(&format!("References: {parent_id}")));
    assert!(raw.contains("Message-ID: <castellum."));
    assert!(
        !raw.contains(bcc_addr.as_str()),
        "Bcc must not leak into the delivered message"
    );

    // 9. The Bcc recipient DID get a copy (auto-created account, login ==
    //    password == address under greenmail.setup.test.all).
    let mut bcc_session = imap_login(&bcc_addr, &bcc_addr);
    bcc_session.select("INBOX").unwrap();
    let bcc_uids = bcc_session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(bcc_uids.len(), 1, "bcc copy delivered via the envelope");
    let _ = bcc_session.logout();

    // 10. Replay the queued sent_copy: the IMAP replayer APPENDs the delivered
    //     bytes to the (auto-created) Sent mailbox.
    let replay_config = serde_json::json!({
        "account_id": "gm",
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false
    })
    .to_string();
    let replay: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(replay["applied"], 1, "sent_copy applied: {replay}");
    assert_eq!(replay["failed"], 0);
    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 0, "queue drained");

    // 11. Server-side: the Sent mailbox holds exactly one copy, marked \Seen,
    //     with the same Message-ID that was delivered.
    let count_in_sent = |session: &mut castellum_provider_imap::session::Session<TcpStream>| {
        session.select(&sent_path).expect("select Sent");
        session.uid_search_by_message_id(&sent_msgid).unwrap().len()
    };
    let mut session = imap_login(&email, &email);
    assert_eq!(count_in_sent(&mut session), 1, "one copy in {sent_path}");
    let seen = session
        .uid_search_all(&[
            SessionSearch::Seen,
            SessionSearch::Header {
                field: "Message-Id",
                value: &sent_msgid,
            },
        ])
        .unwrap();
    assert_eq!(seen.len(), 1, "the Sent copy is \\Seen");

    // 12. Idempotency, both levels: a replay re-run has nothing to do, and a
    //     raw provider-level retry of the same append (a crashed-mid-op replay)
    //     is skipped by the Message-ID guard — never a duplicate.
    let again: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(again["applied"], 0);
    assert_eq!(again["failed"], 0);

    let raw_bytes = {
        session.select("INBOX").unwrap();
        let uids = session
            .uid_search_all(&[SessionSearch::Header {
                field: "Subject",
                value: &subject,
            }])
            .unwrap();
        let uid = uids[0];
        let fetches = session
            .uid_fetch(&uid.to_string(), &[FetchItem::BodyPeek])
            .unwrap();
        fetches[0].body.as_deref().unwrap().to_vec()
    };
    use castellum_providers::MailProvider;
    let mut provider =
        castellum_provider_imap::ImapProvider::connect(&castellum_providers::ImapConfig {
            host: common::host(),
            port: common::IMAP_PORT,
            username: email.clone(),
            password: email.clone(),
            security: castellum_providers::TransportSecurity::Plaintext,
            allow_invalid_certs: false,
        })
        .unwrap();
    provider
        .append(&sent_path, &sent_msgid, &raw_bytes)
        .unwrap();
    provider
        .append(&sent_path, &sent_msgid, &raw_bytes)
        .unwrap();
    let _ = provider.logout();
    assert_eq!(
        count_in_sent(&mut session),
        1,
        "retried appends must not duplicate the Sent copy"
    );
    let _ = session.logout();

    let _ = std::fs::remove_dir_all(&dir);
}

/// S25 undo-send: a draft queued with a short durable delay must NOT reach
/// GreenMail before the window ends, and must be delivered exactly once once
/// it does — proving the skip-then-deliver behaviour end to end over the real
/// SMTP stack, not just against the in-memory `MockSender` the unit tests use.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn undo_send_delay_delivers_exactly_once_after_the_window() {
    let dir = temp_profile_dir("undo-send");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("smtp-undo-send");
    let tag = common::run_tag("undo-send");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    // Mirrors `AppModel.bootstrap()`'s launch-time reconcile, which always
    // runs before anything can be composed/sent — finds nothing to hold
    // (nothing queued yet) and marks the account clean for this launch, so
    // the fresh window queued below is not itself mistaken for a crash
    // survivor by `send_pending_json`'s own crash-safety pass (S25; see
    // `castellum_core::send::send_pending_with_sender`).
    profile
        .reconcile_undo_sends_json("gm")
        .expect("launch reconcile");

    let subject = format!("castellum-undo-send-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Self", "email": email}],
        "subject": subject,
        "body_text": "Delayed by the undo-send window.\n",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");

    // Queue with a 2s undo window instead of immediate delivery.
    profile
        .send_draft_delayed(draft_id, 2)
        .expect("send_draft_delayed");
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(drafts[0]["status"], "outbox");

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

    // Still inside the window: a drain right now must not reach GreenMail.
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(report["sent"], 0, "not due yet: {report}");
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(
        drafts[0]["status"], "outbox",
        "still queued, not delivered early"
    );

    // Window ends: the next drain delivers it, exactly once.
    std::thread::sleep(std::time::Duration::from_millis(2200));
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(
        report["sent"], 1,
        "delivered once the window ended: {report}"
    );
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(drafts[0]["status"], "sent");

    // A further drain does not resubmit.
    let again: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(again["sent"], 0);

    // Confirm over IMAP: exactly one copy reached the recipient's INBOX.
    let mut session = imap_login(&email, &email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 1, "exactly one delivery");
    let _ = session.logout();

    let _ = std::fs::remove_dir_all(&dir);
}

/// S25 relaunch race regression: `AppModel.reconcileUndoSendsThenReplay` runs
/// the crash-safety hold off-actor and asynchronously, so an immediate Send
/// triggered right after relaunch can reach `sendPending` (`send_pending_json`
/// here) before that launch reconcile has actually completed. A profile
/// re-`open`ed on a directory that already has an overdue, interrupted
/// delayed send — with `send_pending_json` called DIRECTLY, no
/// `reconcile_undo_sends_json` call ever made first, simulating exactly that
/// race — must still hold it rather than deliver it to GreenMail: proves the
/// fix end to end over the real SMTP stack, not just the in-memory
/// `MockSender` the `castellum-core` unit tests use.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn undo_send_relaunch_race_holds_an_interrupted_send_with_no_explicit_reconcile_first() {
    let dir = temp_profile_dir("undo-send-race");
    let email = common::provision("smtp-undo-send-race");
    let tag = common::run_tag("undo-send-race");
    let sync_config = common::sync_config_json("gm", &email);

    let subject = format!("castellum-undo-send-race-e2e-{tag}");
    let draft_id;
    {
        // "First session": queue a short delayed send and then stop —
        // deliberately never drains and never reconciles, exactly as if the
        // app had crashed with the window still open.
        let profile = castellum_core::open_profile(&dir).expect("open profile");
        profile.sync_imap_json(&sync_config).expect("first sync");
        profile
            .set_account_write_enabled("gm", true)
            .expect("enable writes");
        let draft_json = serde_json::json!({
            "account_id": "gm",
            "to": [{"name": "Self", "email": email}],
            "subject": subject,
            "body_text": "Interrupted before relaunch.\n",
        })
        .to_string();
        draft_id = profile.save_draft_json(&draft_json).expect("save draft");
        profile
            .send_draft_delayed(draft_id, 1)
            .expect("send_draft_delayed");
    }

    // The window elapses with nothing having drained it — the same shape as
    // a crash sitting past the undo window until the app is reopened.
    std::thread::sleep(std::time::Duration::from_millis(1200));

    // "Relaunch, and a send drain runs first": a brand-new `Profile` opened
    // on the SAME directory, with `send_pending_json` called immediately —
    // no `reconcile_undo_sends_json` call at all, simulating the race where
    // an immediate Send's own drain beats the launch reconcile task to it.
    let relaunch = castellum_core::open_profile(&dir).expect("reopen profile");
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
        serde_json::from_str(&relaunch.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(
        report["sent"], 0,
        "must not auto-send an interrupted delayed send: {report}"
    );

    let ops: serde_json::Value =
        serde_json::from_str(&relaunch.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(ops.as_array().unwrap().len(), 1);
    assert_eq!(
        ops[0]["status"], "held",
        "the drain itself must hold it, with no separate reconcile call: {ops}"
    );

    let drafts: serde_json::Value =
        serde_json::from_str(&relaunch.list_drafts_json("gm").unwrap()).unwrap();
    assert_eq!(drafts[0]["status"], "outbox", "not auto-sent");
    let _ = draft_id;

    // Confirm over IMAP: nothing reached the recipient.
    let mut session = imap_login(&email, &email);
    session.select("INBOX").unwrap();
    let uids = session
        .uid_search_all(&[SessionSearch::Header {
            field: "Subject",
            value: &subject,
        }])
        .unwrap();
    assert_eq!(uids.len(), 0, "held, never delivered");
    let _ = session.logout();

    let _ = std::fs::remove_dir_all(&dir);
}

/// Send Later (HP1) over the real SMTP stack: a message scheduled a few
/// seconds ahead is NOT delivered by a drain before its time (checked over
/// IMAP as well as from the send report), is delivered by the first drain
/// after it, and is delivered exactly once even if the drain is repeated.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn send_later_delivers_once_and_only_after_its_time() {
    let dir = temp_profile_dir("send-later");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("smtp-send-later");
    let tag = common::run_tag("send-later");
    let sync_config = common::sync_config_json("gm", &email);

    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");
    // The launch-time reconcile (nothing queued yet), as in the undo-send test.
    profile
        .reconcile_undo_sends_json("gm")
        .expect("launch reconcile");

    let subject = format!("castellum-send-later-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Self", "email": email}],
        "subject": subject,
        "body_text": "Scheduled with Send Later.\n",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");

    let send_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 4;
    profile
        .schedule_send(draft_id, send_at)
        .expect("schedule_send");

    // The outbox JSON carries the scheduled flag and time.
    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(ops[0]["scheduled"], true, "{ops}");
    assert_eq!(ops[0]["not_before_epoch"], send_at, "{ops}");
    assert_eq!(ops[0]["status"], "queued");

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
    let inbox_count = |subject: &str| -> usize {
        let mut session = imap_login(&email, &email);
        session.select("INBOX").unwrap();
        let n = session
            .uid_search_all(&[SessionSearch::Header {
                field: "Subject",
                value: subject,
            }])
            .unwrap()
            .len();
        let _ = session.logout();
        n
    };

    // Before its time: nothing is sent and nothing arrives.
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(report["sent"], 0, "not due yet: {report}");
    assert_eq!(inbox_count(&subject), 0, "nothing delivered before send_at");

    // After its time: delivered once, then never again.
    std::thread::sleep(std::time::Duration::from_secs(5));
    let report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(report["sent"], 1, "delivered after send_at: {report}");
    let again: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(again["sent"], 0, "no resubmission: {again}");
    assert_eq!(inbox_count(&subject), 1, "exactly one delivery");

    let _ = std::fs::remove_dir_all(&dir);
}

/// #2980: Outbox Retry / Send Now on a failed send really re-sends, exactly
/// once. SMTP is refused first (a closed port: connection refused), so the
/// send fails and stays in the Outbox with nothing delivered; the same message
/// is then retried against the working GreenMail SMTP and arrives once; a
/// further Retry and a drain must not deliver it a second time.
#[test]
#[ignore]
fn outbox_retry_after_smtp_refusal_resends_exactly_once() {
    let dir = temp_profile_dir("outbox-retry");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("smtp-outbox-retry");
    let tag = common::run_tag("outbox-retry");
    profile
        .sync_imap_json(&common::sync_config_json("gm", &email))
        .expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");
    profile.reconcile_undo_sends_json("gm").expect("reconcile");

    let subject = format!("castellum-outbox-retry-e2e-{tag}");
    let draft_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"name": "Self", "email": email}],
        "subject": subject,
        "body_text": "Failed once, retried from the Outbox.\n",
    })
    .to_string();
    let draft_id = profile.save_draft_json(&draft_json).expect("save draft");
    profile.send_draft(draft_id).expect("queue send");

    let config = |port: u16| {
        serde_json::json!({
            "account_id": "gm",
            "from_email": email,
            "from_name": "Castellum E2E",
            "smtp": {
                "host": common::host(),
                "port": port,
                "username": email,
                "password": email,
                "use_tls": false,
                "allow_invalid_certs": false
            }
        })
        .to_string()
    };
    // A port nothing listens on: bind then drop to get a free, closed one.
    let closed_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let inbox_count = || -> usize {
        let mut session = imap_login(&email, &email);
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
    let report = |json: String| -> serde_json::Value { serde_json::from_str(&json).unwrap() };
    let row = || -> serde_json::Value {
        let rows: serde_json::Value =
            serde_json::from_str(&profile.outbox_sends_json("gm").unwrap()).unwrap();
        rows.as_array()
            .and_then(|r| r.iter().find(|r| r["draft_id"] == draft_id).cloned())
            .unwrap_or(serde_json::Value::Null)
    };

    // 1. SMTP refused: the send fails, nothing is delivered, row says failed.
    let r = report(
        profile
            .send_draft_now_json(&config(closed_port), draft_id)
            .unwrap(),
    );
    assert_eq!(r["failed"], 1, "refused SMTP is a failure: {r}");
    assert_eq!(r["sent"], 0, "{r}");
    assert_eq!(inbox_count(), 0, "nothing delivered while SMTP is down");
    assert_eq!(row()["status"], "failed", "{}", row());

    // 2. Retry against the working server delivers it, once.
    let r = report(
        profile
            .send_draft_now_json(&config(common::SMTP_PORT), draft_id)
            .unwrap(),
    );
    assert_eq!(r["sent"], 1, "retry re-sends: {r}");
    assert_eq!(inbox_count(), 1, "delivered after Retry");
    assert!(
        row().is_null(),
        "a sent message leaves the Outbox: {}",
        row()
    );

    // 3. A second Retry and a full drain must not deliver it again.
    let r = report(
        profile
            .send_draft_now_json(&config(common::SMTP_PORT), draft_id)
            .unwrap(),
    );
    assert_eq!(r["sent"], 0, "already sent: {r}");
    let d = report(
        profile
            .send_pending_json(&config(common::SMTP_PORT))
            .unwrap(),
    );
    assert_eq!(d["sent"], 0, "drain finds nothing: {d}");
    assert_eq!(inbox_count(), 1, "exactly one delivery");

    let _ = std::fs::remove_dir_all(&dir);
}
