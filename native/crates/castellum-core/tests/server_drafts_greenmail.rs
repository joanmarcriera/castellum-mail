// Castellum - server drafts GreenMail e2e (S36).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Full round trip against a real (embedded) IMAP server, at the Profile
// level (mirrors smtp_greenmail.rs's account-provisioning idiom - accounts
// are created on SMTP delivery, never by IMAP login alone):
//   0. evaluation mode refuses a server-draft save outright (no op is even
//      queued, so nothing could have been APPENDed) - write-safety policy.
//   1. save -> APPEND with \Draft, resolved server uid recorded locally.
//   2. edit -> a NEW copy is APPENDed; the old one is expunged (replace-on-
//      edit), proven both locally (server_uid changed) and against a raw
//      IMAP session (independent oracle).
//   3. send -> the server mirror is cleaned up (queued in the same DB
//      transaction as the `send` op, applied by the IMAP replayer
//      independently of SMTP delivery timing - see
//      castellum-db::Database::send_draft_delayed); the message is then
//      actually delivered over SMTP so the outbox doesn't leave anything
//      undelivered behind for other tests/inspection.
//   4. a second draft, saved then discarded before ever being sent -> its
//      server mirror is cleaned up too.
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test server_drafts_greenmail -- --ignored --nocapture

mod common;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-server-drafts-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn server_draft_save_replace_send_and_discard_round_trip() {
    let dir = temp_profile_dir("drafts");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("server-drafts");
    let tag = common::run_tag("drafts");
    let sync_config = common::sync_config_json("gm", &email);

    // Sync once so the "gm" account exists locally (drafts FK-reference it).
    profile.sync_imap_json(&sync_config).expect("first sync");

    let from_json = serde_json::json!({ "from_email": email }).to_string();
    let v1_subject = format!("castellum-draft-e2e-{tag} v1");
    let draft_json_v1 = serde_json::json!({
        "account_id": "gm",
        "to": [],
        "subject": v1_subject,
        "body_text": "first version\n",
    })
    .to_string();
    let draft_id = profile
        .save_draft_json(&draft_json_v1)
        .expect("save local draft");

    // 0. Write-safety: a fresh sync leaves the account in evaluation mode,
    //    which must refuse a server-draft save outright - no op is even
    //    queued, so nothing could have been APPENDed.
    assert!(
        profile
            .save_draft_to_server_json(draft_id, &from_json)
            .is_err(),
        "evaluation mode must refuse a server-draft save"
    );
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

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

    // 1. Save: queue + replay APPENDs v1 with \Draft.
    let queued: serde_json::Value = serde_json::from_str(
        &profile
            .save_draft_to_server_json(draft_id, &from_json)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued["queued"], true);
    let report: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(report["applied"], 1, "server_draft_save applied: {report}");
    assert_eq!(report["failed"], 0);

    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    let d1 = drafts
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft_id)
        .expect("draft present");
    assert!(
        d1["server_uid"].is_i64(),
        "v1 must have a resolved server uid: {d1}"
    );
    let v1_uid = d1["server_uid"].as_i64().unwrap();
    let v1_message_id = d1["server_message_id"].as_str().unwrap().to_string();

    // 2. Edit: replace-on-edit. New content -> new server_draft_save;
    //    record_server_draft_saved queues the old copy's cleanup once the
    //    new one is safely on the server.
    // v2 gets a real recipient (itself) - it's the copy that goes on to be
    // actually sent below, and SMTP delivery needs at least one RCPT TO.
    let draft_json_v2 = serde_json::json!({
        "id": draft_id,
        "account_id": "gm",
        "to": [{"email": email}],
        "subject": format!("castellum-draft-e2e-{tag} v2"),
        "body_text": "second version\n",
    })
    .to_string();
    profile
        .save_draft_json(&draft_json_v2)
        .expect("save edited draft");
    let queued2: serde_json::Value = serde_json::from_str(
        &profile
            .save_draft_to_server_json(draft_id, &from_json)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued2["queued"], true);
    // First pass applies the new server_draft_save (and queues the v1 cleanup).
    let r2: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r2["applied"], 1, "v2 server_draft_save applied: {r2}");
    // Second pass applies the just-queued cleanup for v1.
    let r3: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r3["applied"], 1, "v1 cleanup applied: {r3}");

    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    let d2 = drafts
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft_id)
        .expect("draft present");
    let v2_uid = d2["server_uid"].as_i64().unwrap();
    let v2_message_id = d2["server_message_id"].as_str().unwrap().to_string();
    assert_ne!(v1_uid, v2_uid, "the edit must land at a NEW uid");

    // Independent oracle: exactly the v2 copy is live in Drafts; v1 is gone.
    let mut session = common::imap_login(&email, &email);
    session.select("Drafts").expect("select Drafts");
    assert_eq!(
        session
            .uid_search_by_message_id(&v2_message_id)
            .unwrap()
            .len(),
        1,
        "v2 live in Drafts"
    );
    assert_eq!(
        session
            .uid_search_by_message_id(&v1_message_id)
            .unwrap()
            .len(),
        0,
        "v1 expunged from Drafts"
    );

    // 3. Send: send_draft_delayed(0) queues the mirror's cleanup in the SAME
    //    DB transaction as the `send` op, so the IMAP replayer can apply it
    //    right away, independently of when SMTP delivery actually happens.
    profile.send_draft(draft_id).expect("send_draft");
    let r4: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r4["applied"], 1, "v2 cleanup on send applied: {r4}");
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    let d3 = drafts
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft_id)
        .expect("draft present (now outbox)");
    assert!(
        d3["server_mailbox_path"].is_null(),
        "mirror cleared locally on send: {d3}"
    );
    assert_eq!(
        session
            .uid_search_by_message_id(&v2_message_id)
            .unwrap()
            .len(),
        0,
        "v2 expunged from Drafts on send"
    );

    // Actually deliver it, so the outbox drains cleanly (unchanged message
    // counts elsewhere: nothing is left half-sent by this test).
    let send_config = serde_json::json!({
        "account_id": "gm",
        "from_email": email,
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
    let send_report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(
        send_report["sent"], 1,
        "the queued send delivers: {send_report}"
    );
    assert_eq!(send_report["failed"], 0);
    // Drain the sent_copy op the send just queued (APPEND to Sent), so it
    // doesn't get swept up into step 4's own applied-count assertions below.
    let r_sent_copy: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(
        r_sent_copy["applied"], 1,
        "sent_copy applied: {r_sent_copy}"
    );

    // 4. Discard: a second draft, saved to the server then deleted before
    //    ever being sent - proves cleanup-on-discard too.
    let draft3_json = serde_json::json!({
        "account_id": "gm",
        "to": [],
        "subject": format!("castellum-draft-e2e-{tag} discard"),
        "body_text": "throwaway\n",
    })
    .to_string();
    let draft3_id = profile
        .save_draft_json(&draft3_json)
        .expect("save 2nd draft");
    profile
        .save_draft_to_server_json(draft3_id, &from_json)
        .expect("queue 2nd draft's server save");
    let r5: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(
        r5["applied"], 1,
        "2nd draft's server_draft_save applied: {r5}"
    );
    let drafts: serde_json::Value =
        serde_json::from_str(&profile.list_drafts_json("gm").unwrap()).unwrap();
    let d4 = drafts
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == draft3_id)
        .expect("2nd draft present");
    let v3_message_id = d4["server_message_id"].as_str().unwrap().to_string();
    assert_eq!(
        session
            .uid_search_by_message_id(&v3_message_id)
            .unwrap()
            .len(),
        1,
        "throwaway draft live in Drafts before discard"
    );

    profile.delete_draft(draft3_id).expect("discard 2nd draft");
    let r6: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r6["applied"], 1, "discard cleanup applied: {r6}");
    assert_eq!(
        session
            .uid_search_by_message_id(&v3_message_id)
            .unwrap()
            .len(),
        0,
        "discarded copy expunged from Drafts"
    );

    // 5. The Drafts mailbox ends empty: nothing from this test's own traffic
    //    is left behind in it.
    assert_eq!(
        session
            .uid_search_by_message_id(&v1_message_id)
            .unwrap()
            .len(),
        0
    );
    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(
        ops.as_array().unwrap().len(),
        0,
        "operation queue drained: {ops}"
    );

    let _ = session.logout();
}

/// S36 review regression: the compose UI's 2s autosave debounce queues a
/// `server_draft_save` op and only THEN schedules replay, so a user who
/// presses Send (or discards) in that window can have a save sitting in the
/// queue when the draft is no longer `draft` status. Before this fix, replay
/// APPENDed it anyway and `record_server_draft_saved` silently dropped the
/// cleanup, leaving an orphaned copy on the server forever. Proves both the
/// send and the discard case leave the Drafts mailbox's EXISTS count
/// unchanged and the op queue drained.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn queued_save_superseded_by_send_or_discard_before_replay_never_appends() {
    let dir = temp_profile_dir("drafts-race");
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision("server-drafts-race");
    let tag = common::run_tag("drafts-race");
    let sync_config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&sync_config).expect("first sync");
    profile
        .set_account_write_enabled("gm", true)
        .expect("enable writes");

    let from_json = serde_json::json!({ "from_email": email }).to_string();
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

    let mut session = common::imap_login(&email, &email);
    // Drafts doesn't exist on a fresh account until the first successful
    // APPEND auto-creates it (see `ImapProvider::append_with_flags_get_uid`)
    // — this test's whole point is that no APPEND ever happens, so create it
    // up front rather than relying on one.
    let _ = session.create("Drafts");
    let baseline = session.select("Drafts").expect("select Drafts").exists;

    // --- Case A: save queued, then SEND before replay ever runs. ---
    let draft_a_json = serde_json::json!({
        "account_id": "gm",
        "to": [{"email": email}],
        "subject": format!("castellum-draft-e2e-{tag} race-send"),
        "body_text": "queued then sent\n",
    })
    .to_string();
    let draft_a = profile
        .save_draft_json(&draft_a_json)
        .expect("save draft A");
    let queued_a: serde_json::Value = serde_json::from_str(
        &profile
            .save_draft_to_server_json(draft_a, &from_json)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued_a["queued"], true);
    // Send fires while the server_draft_save op is STILL queued, unreplayed.
    profile.send_draft(draft_a).expect("send draft A");
    let ra: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(
        ra["applied"], 1,
        "the superseded save resolves as one applied no-op: {ra}"
    );
    assert_eq!(ra["failed"], 0);
    let after_send = session.select("Drafts").expect("re-select Drafts").exists;
    assert_eq!(
        after_send, baseline,
        "a save superseded by send must never APPEND to Drafts"
    );
    // Deliver it so nothing is left half-sent for other tests/inspection.
    let send_config = serde_json::json!({
        "account_id": "gm",
        "from_email": email,
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
    let send_report: serde_json::Value =
        serde_json::from_str(&profile.send_pending_json(&send_config).unwrap()).unwrap();
    assert_eq!(send_report["sent"], 1, "draft A delivers: {send_report}");
    assert_eq!(send_report["failed"], 0);
    // Drain the sent_copy op the send just queued (APPEND to Sent).
    let r_sent_copy: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(
        r_sent_copy["applied"], 1,
        "sent_copy applied: {r_sent_copy}"
    );

    // --- Case B: save queued, then DISCARD before replay ever runs. ---
    let draft_b_json = serde_json::json!({
        "account_id": "gm",
        "to": [],
        "subject": format!("castellum-draft-e2e-{tag} race-discard"),
        "body_text": "queued then discarded\n",
    })
    .to_string();
    let draft_b = profile
        .save_draft_json(&draft_b_json)
        .expect("save draft B");
    let queued_b: serde_json::Value = serde_json::from_str(
        &profile
            .save_draft_to_server_json(draft_b, &from_json)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(queued_b["queued"], true);
    // Discard fires while the server_draft_save op is STILL queued.
    profile.delete_draft(draft_b).expect("discard draft B");
    let rb: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(
        rb["applied"], 1,
        "the discarded draft's superseded save resolves as one applied no-op: {rb}"
    );
    assert_eq!(rb["failed"], 0);
    let after_discard = session.select("Drafts").expect("re-select Drafts").exists;
    assert_eq!(
        after_discard, baseline,
        "a save superseded by discard must never APPEND to Drafts"
    );

    let ops: serde_json::Value =
        serde_json::from_str(&profile.pending_operations_json("gm").unwrap()).unwrap();
    assert_eq!(
        ops.as_array().unwrap().len(),
        0,
        "operation queue drained: {ops}"
    );

    let _ = session.logout();
}
