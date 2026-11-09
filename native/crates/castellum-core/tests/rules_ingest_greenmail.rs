// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - S40 ingest hook: a rule fires after a real sync pass against
// GreenMail. This is the one S40 test that needs the local IMAP stack —
// "run at ingest" only exists inside `sync_with_provider` itself, which no
// throwaway-SQLite unit test can exercise (mirrors the plan's own reasoning
// for why every other S40 test stays GreenMail-free). A dedicated file
// under `tests/`, not inline in `sync.rs`, per the existing `*_greenmail.rs`
// convention every other GreenMail-gated test in this crate already follows
// (a deviation from the plan's literal File Structure list, which named
// only `sync.rs` for Task 7 — recorded in the plan's Deviations log).
//
// #[ignore] by default. Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test rules_ingest_greenmail -- --ignored --nocapture

mod common;

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-rules-ingest-e2e-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A rule matching Subject contains "invoice", trigger on_sync, action
/// MarkRead. First sync just ingests the bootstrap message (no match,
/// nothing queued). A second sync — after an invoice-subject message is
/// delivered — must enqueue exactly one `mark_read` operation for it,
/// proving the rule fired automatically inside the real sync pass, not just
/// against a hand-seeded fixture.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn on_sync_rule_fires_after_a_real_sync_pass_and_enqueues_mark_read() {
    let tag = common::run_tag("rules-ingest");
    let dir = temp_profile_dir(&tag);
    let profile = castellum_core::open_profile(&dir).expect("open profile");

    let email = common::provision(&tag);
    let account_id = "gm-s40";

    // First sync: creates the local `accounts` row (saved_filters.account_id
    // FKs to it) and ingests only the bootstrap message — no rule exists
    // yet, so nothing can fire.
    profile
        .sync_imap_json(&common::sync_config_json(account_id, &email))
        .expect("first sync (bootstrap)");
    profile
        .set_account_write_enabled(account_id, true)
        .expect("enable writes so mark_read can enqueue");

    let group = serde_json::json!({
        "op": "and",
        "conditions": [
            {"field": "subject", "operator": "contains", "value": "invoice"}
        ],
        "groups": []
    });
    let rule_req = serde_json::json!({
        "account_id": account_id,
        "name": "Invoices",
        "kind": "rule",
        "group": group,
        "trigger": "on_sync",
        "actions": ["mark_read"]
    });
    profile
        .upsert_filter_json(&rule_req.to_string())
        .expect("create on_sync rule");

    // Deliver a genuinely new message whose subject matches the rule.
    common::smtp_deliver(
        "sender@local.test",
        &email,
        "Subject: your invoice is ready\r\n\r\ninvoice body\r\n",
    );

    // Second sync: the new message lands, and the ingest hook at the end of
    // sync_with_provider must run the on_sync rule against it.
    profile
        .sync_imap_json(&common::sync_config_json(account_id, &email))
        .expect("second sync (invoice message)");

    let ops_json = profile
        .pending_operations_json(account_id)
        .expect("pending_operations_json");
    let ops: serde_json::Value = serde_json::from_str(&ops_json).unwrap();
    let ops = ops.as_array().expect("array");
    let mark_read_ops: Vec<&serde_json::Value> = ops
        .iter()
        .filter(|o| o["kind"].as_str() == Some("mark_read"))
        .collect();
    assert_eq!(
        mark_read_ops.len(),
        1,
        "exactly one mark_read op expected after the rule fires at ingest: {ops:?}"
    );
    assert!(
        mark_read_ops[0]["payload_json"]
            .as_str()
            .unwrap()
            .contains("INBOX"),
        "the queued op must reference the newly-synced INBOX message: {:?}",
        mark_read_ops[0]
    );
}

/// MM-3 review, BLOCKER 1: a Trash rule must not re-trash mail the user
/// restored. The rule trashes an invoice; replay moves it to Trash on the
/// server; then the user restores it to the Inbox from another device (a raw
/// UID MOVE here). The next sync sees it land in INBOX as a brand-new row —
/// but it is the message our own queued move took away, so the arrival pass
/// must leave it alone. A genuinely new invoice arriving in the same pass is
/// still trashed, proving the rule itself still runs.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) on 3025/3143"]
fn a_trash_rule_never_re_trashes_mail_restored_to_the_inbox() {
    let tag = common::run_tag("rules-restore");
    let dir = temp_profile_dir(&tag);
    let profile = castellum_core::open_profile(&dir).expect("open profile");
    let email = common::provision(&tag);
    let account_id = "gm-mm3-restore";
    let sync = || {
        profile
            .sync_imap_json(&common::sync_config_json(account_id, &email))
            .expect("sync")
    };
    let pending_moves = || -> Vec<serde_json::Value> {
        let ops: serde_json::Value =
            serde_json::from_str(&profile.pending_operations_json(account_id).unwrap()).unwrap();
        ops.as_array()
            .unwrap()
            .iter()
            .filter(|o| o["kind"].as_str() == Some("move"))
            .cloned()
            .collect()
    };

    sync(); // first import: the bootstrap message only
    profile
        .set_account_write_enabled(account_id, true)
        .expect("enable writes");
    let rule_req = serde_json::json!({
        "account_id": account_id,
        "name": "Trash invoices",
        "kind": "rule",
        "group": {"op": "and", "conditions": [
            {"field": "subject", "operator": "contains", "value": "invoice"}], "groups": []},
        "trigger": "on_sync",
        "actions": ["trash"]
    });
    profile
        .upsert_filter_json(&rule_req.to_string())
        .expect("create rule");

    let first_id = format!("<first-{tag}@local.test>");
    common::smtp_deliver(
        "billing@local.test",
        &email,
        &format!("Subject: first invoice\r\nMessage-ID: {first_id}\r\n\r\nbody\r\n"),
    );
    sync();
    assert_eq!(
        pending_moves().len(),
        1,
        "the rule trashed the first invoice"
    );

    let replay_config = serde_json::json!({
        "account_id": account_id,
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false
    })
    .to_string();
    let r: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r["applied"], 1, "the move reached the server: {r}");
    assert!(pending_moves().is_empty());

    // The user restores it from another device.
    let mut imap = common::imap_session(&email);
    imap.select("Trash").expect("select Trash");
    let uids = imap
        .uid_search_by_message_id(&first_id)
        .expect("search Trash");
    assert_eq!(uids.len(), 1, "the trashed invoice is in Trash: {uids:?}");
    imap.uid_move(&uids[0].to_string(), "INBOX")
        .expect("restore to INBOX");
    drop(imap);

    // Plus one genuinely new invoice in the same pass.
    common::smtp_deliver(
        "billing@local.test",
        &email,
        &format!(
            "Subject: second invoice\r\nMessage-ID: <second-{tag}@local.test>\r\n\r\nbody\r\n"
        ),
    );
    sync();

    let moves = pending_moves();
    assert_eq!(
        moves.len(),
        1,
        "only the NEW invoice is trashed, never the restored one: {moves:?}"
    );
    let payload = moves[0]["payload_json"].as_str().unwrap();
    assert!(payload.contains("second-"), "{payload}");
    assert!(!payload.contains(&first_id), "{payload}");
    let _ = std::fs::remove_dir_all(&dir);
}
