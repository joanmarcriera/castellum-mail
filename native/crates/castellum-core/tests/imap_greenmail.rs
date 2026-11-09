// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// End-to-end IMAP sync tests against a local GreenMail server.
//
// These are #[ignore] by default: they require the GreenMail container from
// scripts/local-imap (no seeding needed — each test provisions its own
// account and appends its own probes, so the suite is parallel-safe).
// Run locally with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-core --test imap_greenmail -- --ignored --nocapture
//
// CI runs these against a GreenMail service container with
// GREENMAIL_HOST=<service>.

mod common;

use castellum_core::{open_profile, sync_progress, CoreError};

/// Append a raw RFC-822 message to the given account's mailbox on GreenMail.
fn append_raw(email: &str, mailbox: &str, raw: &[u8]) {
    let mut session = common::imap_session(email);
    session.append(mailbox, &[], raw).expect("IMAP append");
    let _ = session.logout();
}

/// A tiny plain-text probe message with a unique subject/Message-ID.
/// `in_reply_to` chains probes into a thread for the projection assertions.
fn build_text_probe(to: &str, subject: &str, tag: &str, in_reply_to: Option<&str>) -> Vec<u8> {
    let reply_headers = in_reply_to
        .map(|parent| format!("In-Reply-To: {parent}\r\nReferences: {parent}\r\n"))
        .unwrap_or_default();
    format!(
        "From: probe@local.test\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <probe-{tag}@local.test>\r\n\
         {reply_headers}\
         \r\n\
         Reconciliation probe body in the plain text form.\r\n"
    )
    .into_bytes()
}

/// The IMAP UID encoded in a Castellum message id ("gm:INBOX:{uid}").
fn uid_from_message_id(message_id: &str) -> u32 {
    message_id
        .rsplit(':')
        .next()
        .and_then(|u| u.parse().ok())
        .unwrap_or_else(|| panic!("no uid in message id {message_id}"))
}

/// Set `\Seen` on a message by UID in the account's INBOX (a real remote flag
/// change).
fn mark_seen_on_server(email: &str, uid: u32) {
    let mut session = common::imap_session(email);
    session.select("INBOX").expect("select");
    session
        .uid_store(
            &uid.to_string(),
            castellum_provider_imap::session::StoreSign::Add,
            &[castellum_provider_imap::session::SessionFlag::Seen],
        )
        .expect("uid store seen");
    let _ = session.logout();
}

/// True when the INBOX message with `uid` is still \Unseen on the server.
fn server_is_unseen(email: &str, uid: u32) -> bool {
    let mut session = common::imap_session(email);
    session.select("INBOX").expect("select");
    let unseen = session
        .uid_search_all(&[
            castellum_provider_imap::session::SessionSearch::Unseen,
            castellum_provider_imap::session::SessionSearch::Uid(uid),
        ])
        .expect("uid search unseen");
    let _ = session.logout();
    unseen.contains(&uid)
}

/// Expunge a message by UID from the account's INBOX (a real remote deletion).
fn expunge_on_server(email: &str, uid: u32) {
    let mut session = common::imap_session(email);
    session.select("INBOX").expect("select");
    session
        .uid_store(
            &uid.to_string(),
            castellum_provider_imap::session::StoreSign::Add,
            &[castellum_provider_imap::session::SessionFlag::Deleted],
        )
        .expect("uid store deleted");
    session.expunge().expect("expunge");
    let _ = session.logout();
}

/// Build a minimal multipart/alternative raw email (CRLF-terminated) with the
/// given subject. The HTML part contains hostile markup so the sanitizer is
/// exercised; the text part provides a plain-text fallback.
fn build_html_probe(to: &str, subject: &str) -> Vec<u8> {
    let boundary = "castellum-probe-boundary";
    let html_body = concat!(
        "<p>SAFE</p>",
        "<script>alert(1)</script>",
        "<img src=\"https://tracker.example/p.gif\" onerror=\"x()\">",
        "<a href=\"javascript:alert(2)\">link</a>",
        "<table><tr><td>CELL</td></tr></table>"
    );
    let text_body = "SAFE text fallback.";
    let raw = format!(
        "From: probe@local.test\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <probe-{pid}@local.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\
         \r\n\
         --{boundary}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         {text_body}\r\n\
         --{boundary}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         \r\n\
         {html_body}\r\n\
         --{boundary}--\r\n",
        subject = subject,
        boundary = boundary,
        pid = std::process::id(),
        text_body = text_body,
        html_body = html_body,
    );
    raw.into_bytes()
}

fn temp_profile_dir(tag: &str) -> std::path::PathBuf {
    let dir = castellum_testutil::scratch_root().join(format!(
        "castellum-greenmail-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_populates_db_from_greenmail() {
    // Own account + own probes: 2 threaded + 1 standalone, so the read, FTS,
    // and thread assertions are self-contained.
    let email = common::provision("imap-sync");
    let tag = common::run_tag("sync");
    let root_msgid = format!("<probe-{tag}-root@local.test>");
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(
            &email,
            &format!("castl-sync-root-{tag}"),
            &format!("{tag}-root"),
            None,
        ),
    );
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(
            &email,
            &format!("Re: castl-sync-root-{tag}"),
            &format!("{tag}-reply"),
            Some(&root_msgid),
        ),
    );
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(
            &email,
            &format!("castl-sync-solo-{tag}"),
            &format!("{tag}-solo"),
            None,
        ),
    );

    let dir = temp_profile_dir("sync");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);

    let report = profile
        .sync_imap_json(&config)
        .expect("sync should succeed");
    println!("sync report: {report}");

    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    let inbox = v["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in report");
    let total = inbox["total"].as_i64().unwrap();
    assert!(
        total >= 3,
        "expected at least the three appended probes, got {total}"
    );

    // A second sync must be idempotent (no growth, fewer/zero fetched). This
    // account is private to the test, so the total cannot move under us.
    let report2 = profile.sync_imap_json(&config).expect("second sync");
    let v2: serde_json::Value = serde_json::from_str(&report2).unwrap();
    let total2 = v2["mailboxes"][0]["total"].as_i64().unwrap();
    assert_eq!(total, total2, "idempotent: total unchanged on re-sync");

    // Read APIs over real synced data. The list is paginated (limit 500), so
    // the page holds min(total, 500).
    let limit: i64 = 500;
    let list = profile
        .list_messages_json("gm", "gm:INBOX", limit, 0)
        .expect("list messages");
    let listed: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(listed.as_array().unwrap().len() as i64, total.min(limit));

    // FTS over real synced data: the probe bodies contain "the".
    let hits = profile
        .search_messages_json("gm", "the", 500)
        .expect("search");
    let hit_arr: serde_json::Value = serde_json::from_str(&hits).unwrap();
    assert!(
        !hit_arr.as_array().unwrap().is_empty(),
        "FTS finds the probe bodies"
    );

    // Message detail for the first listed message.
    let first_id = listed[0]["id"].as_str().unwrap();
    let detail = profile.get_message_json(first_id).expect("detail");
    assert!(detail.contains("\"header\""), "detail has a header");

    // Thread projection over real synced data (root + reply thread together).
    let threads = profile
        .list_threads_json("gm", 500, 0)
        .expect("list threads");
    let tarr: serde_json::Value = serde_json::from_str(&threads).unwrap();
    let n_threads = tarr.as_array().unwrap().len();
    assert!(n_threads > 0, "threads projected");
    assert!(n_threads as i64 <= total, "threads never exceed messages");
    assert!(
        tarr.as_array()
            .unwrap()
            .iter()
            .any(|t| t["message_count"].as_i64().unwrap_or(0) >= 2),
        "the root+reply probes form one multi-message thread"
    );
    let tid = tarr[0]["id"].as_str().unwrap();
    let tmsgs = profile.get_thread_json(tid).expect("thread messages");
    assert!(tmsgs.starts_with('['), "thread messages is a JSON array");

    // Redacted support bundle over a really synced profile: account-level
    // success recorded by sync_imap_json, the CAPABILITY chain captured end to
    // end, per-mailbox success + counts present — and no identifiers leaked.
    let bundle_json = profile.support_bundle_json().expect("support bundle");
    let bundle: serde_json::Value = serde_json::from_str(&bundle_json).unwrap();
    assert_eq!(bundle["bundle_schema"], 1);
    assert_eq!(bundle["integrity"]["ok"], true);
    let acct = &bundle["accounts"][0];
    assert_eq!(acct["label"], "account-1");
    assert!(
        acct["last_success_at"].is_string(),
        "account success must be recorded after a real sync: {acct}"
    );
    assert_eq!(acct["consecutive_failures"], 0);
    let caps: Vec<&str> = acct["capabilities"]
        .as_array()
        .expect("capabilities array")
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
    assert!(
        caps.contains(&"IMAP4rev1"),
        "CAPABILITY chain must capture the server atoms, got {caps:?}"
    );
    let inbox_diag = acct["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["label"] == "inbox")
        .expect("inbox mailbox in bundle");
    assert!(inbox_diag["last_success_at"].is_string());
    assert!(inbox_diag["total_messages"].as_i64().unwrap() >= 3);
    // The real account email and mailbox paths must never appear.
    assert!(
        !bundle_json.contains(&email) && !bundle_json.contains("INBOX"),
        "bundle leaked identifiers: {bundle_json}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

///: a first sync's progress snapshot lands with real message
/// counts against a known (IMAP EXISTS) total, and is left marked `done`
/// once the sync call itself returns — the exact resting state a UI poller
/// would see if its last tick raced the FFI call's own return.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_progress_reports_message_counts_against_a_known_total() {
    // A dedicated account_id, NOT the "gm" literal other tests in this file
    // share: `sync_progress` is a process-global registry keyed by
    // account_id (see `castellum_core::sync_progress`), so a shared literal
    // would let parallel tests clobber each other's snapshots.
    let tag = common::run_tag("progress");
    let email = common::provision(&format!("imap-progress-{tag}"));
    let account_id = format!("gm-progress-{tag}");
    append_probes_bulk(&email, "INBOX", &tag, 40);

    assert!(
        sync_progress::snapshot(&account_id).is_none(),
        "no snapshot before any sync has run for this account id"
    );

    let dir = temp_profile_dir("progress");
    let profile = open_profile(&dir).expect("open profile");
    let config = serde_json::json!({
        "account_id": account_id,
        "email": email,
        "host": common::host(),
        "port": common::IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false,
        "mailboxes": ["INBOX"]
    })
    .to_string();

    let report = profile
        .sync_imap_json(&config)
        .expect("sync should succeed");
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    let inbox = v["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in report");
    let total = inbox["total"].as_i64().unwrap();
    assert!(
        total >= 40,
        "expected at least the 40 bulk probes, got {total}"
    );

    let snapshot = sync_progress::snapshot(&account_id)
        .expect("a snapshot must exist once a sync has run for this account");
    assert!(
        snapshot.done,
        "the pass must be marked finished once sync_imap_json returns: {snapshot:?}"
    );
    assert_eq!(
        snapshot.mailboxes_completed, snapshot.mailboxes_total,
        "every discovered mailbox (fetched, skipped, or filtered out) is accounted for: {snapshot:?}"
    );
    assert!(snapshot.mailboxes_total >= 1);
    assert!(
        snapshot.aggregate_fetched >= total,
        "aggregate_fetched must cover at least the mailbox's reported total on a first sync: {snapshot:?}"
    );
    assert!(
        snapshot.aggregate_total >= total,
        "aggregate_total (from IMAP EXISTS at SELECT) must reflect the mailbox's message count: {snapshot:?}"
    );
    assert!(
        snapshot.current_mailbox.is_none(),
        "no mailbox left in flight once the pass is done: {snapshot:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_reconciles_remote_flag_and_expunge() {
    // Own account; two unique probes: one we'll mark \Seen server-side, one
    // we'll expunge.
    let email = common::provision("imap-recon");
    let tag = common::run_tag("recon");
    let subj_seen = format!("castl-recon-seen-{tag}");
    let subj_gone = format!("castl-recon-gone-{tag}");
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(&email, &subj_seen, &format!("{tag}-a"), None),
    );
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(&email, &subj_gone, &format!("{tag}-b"), None),
    );

    let dir = temp_profile_dir("recon");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);

    // First sync: both probes land, both unread.
    profile.sync_imap_json(&config).expect("first sync");
    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 2000, 0)
        .expect("list");
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    let find_id = |subject: &str| -> String {
        list.as_array()
            .unwrap()
            .iter()
            .find(|m| m["subject"].as_str() == Some(subject))
            .unwrap_or_else(|| panic!("probe '{subject}' not found after first sync"))
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string()
    };
    let id_seen = find_id(&subj_seen);
    let id_gone = find_id(&subj_gone);
    assert!(
        !profile
            .get_message_json(&id_seen)
            .unwrap()
            .contains("\"is_read\":true"),
        "probe starts unread"
    );

    // Mutate the server: mark one \Seen, expunge the other.
    mark_seen_on_server(&email, uid_from_message_id(&id_seen));
    expunge_on_server(&email, uid_from_message_id(&id_gone));

    // Second sync reconciles: no new bodies fetched for these, yet local converges.
    let report2 = profile.sync_imap_json(&config).expect("second sync");
    let v2: serde_json::Value = serde_json::from_str(&report2).unwrap();
    let inbox = v2["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in report");
    println!("reconcile report: {inbox}");
    assert!(
        inbox["flags_reconciled"].as_i64().unwrap() >= 1,
        "the \\Seen change must be reconciled"
    );
    assert!(
        inbox["vanished"].as_i64().unwrap() >= 1,
        "the expunged message must be purged"
    );

    // The seen probe is now read locally; the expunged probe is gone.
    let detail_seen = profile.get_message_json(&id_seen).expect("detail seen");
    assert!(
        detail_seen.contains("\"is_read\":true"),
        "remote \\Seen reconciled into local read state: {detail_seen}"
    );
    let detail_gone = profile
        .get_message_json(&id_gone)
        .expect("detail gone call");
    assert!(
        detail_gone == "null" || detail_gone.is_empty(),
        "expunged message must be absent locally, got: {detail_gone}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn evaluation_mode_blocks_replay_against_real_imap() {
    // Defence-in-depth over REAL IMAP: a STORE \Seen queued while writable is
    // blocked once the account is in evaluation mode — the server flag is
    // provably untouched — and applies only after writes are re-enabled.
    let email = common::provision("imap-eval");
    let tag = common::run_tag("eval");
    let subj = format!("castl-eval-{tag}");
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(&email, &subj, &format!("{tag}-a"), None),
    );

    let dir = temp_profile_dir("eval");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile.sync_imap_json(&config).expect("sync");

    let list: serde_json::Value = serde_json::from_str(
        &profile
            .list_messages_json("gm", "gm:INBOX", 2000, 0)
            .unwrap(),
    )
    .unwrap();
    let id = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subj.as_str()))
        .expect("probe present after sync")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let uid = uid_from_message_id(&id);

    // Queue a mark_read (STORE \Seen) op WHILE writable, then enter eval mode.
    profile.set_account_write_enabled("gm", true).unwrap();
    profile.mark_read(&id, true).unwrap();
    profile.set_account_write_enabled("gm", false).unwrap();

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

    // Replay in evaluation mode: the STORE is BLOCKED; the server is untouched.
    let r: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r["applied"], 0, "nothing applied in evaluation mode: {r}");
    assert_eq!(r["blocked"], 1, "the queued STORE is blocked: {r}");
    assert!(
        server_is_unseen(&email, uid),
        "the message must remain \\Unseen on the server in evaluation mode"
    );

    // Enable writes and replay: now the STORE reaches the server.
    profile.set_account_write_enabled("gm", true).unwrap();
    let r2: serde_json::Value =
        serde_json::from_str(&profile.replay_operations_json(&replay_config).unwrap()).unwrap();
    assert_eq!(r2["applied"], 1, "writable replay applies the STORE: {r2}");
    assert!(
        !server_is_unseen(&email, uid),
        "the server now shows \\Seen after writes are enabled"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail IMAPS (3993, self-signed cert)"]
fn tls_validation_rejects_self_signed_by_default() {
    let dir = temp_profile_dir("tls");
    let profile = open_profile(&dir).expect("open profile");

    // use_tls=true + allow_invalid_certs=false against GreenMail's self-signed
    // cert MUST fail — proving TLS validation is enforced by default. The
    // handshake fails before authentication, so no account is needed.
    let config = serde_json::json!({
        "account_id": "gm-tls",
        "email": "test@local.test",
        "host": common::host(),
        "port": common::IMAPS_PORT,
        "username": "test@local.test",
        "password": "secret",
        "use_tls": true,
        "allow_invalid_certs": false,
        "mailboxes": ["INBOX"]
    })
    .to_string();

    match profile.sync_imap_json(&config) {
        Err(CoreError::Sync(msg)) => {
            println!("correctly rejected self-signed TLS: {msg}");
        }
        Err(other) => panic!("expected Sync/TLS error, got {other:?}"),
        Ok(_) => panic!("SECURITY: self-signed cert was accepted with validation on"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_stores_sanitized_html_and_flags_remote_content() {
    // Own account; append a self-contained hostile multipart/alternative probe.
    let email = common::provision("imap-html");
    let subject = format!("castl-html-probe-{}", common::run_tag("html"));
    let raw = build_html_probe(&email, &subject);
    append_raw(&email, "INBOX", &raw);

    // Fresh profile so the sync fetches from scratch and the probe is included.
    let dir = temp_profile_dir("htmlprobe");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);

    let report = profile
        .sync_imap_json(&config)
        .expect("sync should succeed");
    println!("sync report: {report}");

    // Locate the probe by its unique subject in the full INBOX listing.
    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 1000, 0)
        .expect("list messages");
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    let probe_header = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject.as_str()))
        .unwrap_or_else(|| panic!("probe '{subject}' not found in INBOX after sync"));
    let probe_id = probe_header["id"].as_str().unwrap();
    println!("probe message id: {probe_id}");

    // Fetch full detail and check sanitization.
    let detail_json = profile.get_message_json(probe_id).expect("get detail");
    let detail: serde_json::Value = serde_json::from_str(&detail_json).unwrap();

    let html = detail["sanitized_html"]
        .as_str()
        .expect("sanitized_html must be stored");
    println!("sanitized html: {html}");

    assert!(
        !html.contains("<script"),
        "script tag must be stripped: {html}"
    );
    assert!(
        !html.to_lowercase().contains("onerror"),
        "onerror handler must be stripped: {html}"
    );
    assert!(
        !html.contains("javascript:"),
        "javascript: URI must be stripped: {html}"
    );
    assert!(
        html.contains("SAFE"),
        "safe content must be preserved: {html}"
    );
    assert!(
        html.contains("<table"),
        "table structure must be preserved: {html}"
    );
    assert!(
        html.contains("CELL"),
        "table cell content must be preserved: {html}"
    );

    assert!(
        detail["has_remote_content"].as_bool().unwrap_or(false),
        "remote tracker img must set has_remote_content"
    );
    assert!(
        detail["text_body"].is_string(),
        "text/plain fallback must be stored"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn build_forged_auth_probe(to: &str, subject: &str) -> Vec<u8> {
    format!(
        "From: attacker@evil.example\r\n\
         To: {to}\r\n\
         Subject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\
         Message-ID: <forged-{pid}@evil.example>\r\n\
         Authentication-Results: {claimed_host}; spf=pass smtp.mailfrom=attacker@evil.example; dkim=pass header.d=evil.example; dmarc=pass\r\n\
         \r\n\
         This message forges its own Authentication-Results header.\r\n",
        to = to,
        subject = subject,
        pid = std::process::id(),
        // The account's real IMAP host per common::sync_config_json/common's
        // GreenMail fixture is 127.0.0.1 (or $GREENMAIL_HOST) -- an attacker
        // cannot know that in advance in a real attack, but even naming it
        // here (worst case for the defence) must not produce a pass, because
        // GreenMail itself never adds its own Authentication-Results header,
        // so this stays the ONLY (and therefore structurally "outermost")
        // one in the message -- exactly the case Task 1's
        // `forged_authserv_id_never_yields_a_pass` proves in isolation. This
        // test proves the same property end-to-end through a real IMAP fetch
        // and the DB round-trip.
        claimed_host = "evil.example",
    )
    .into_bytes()
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_never_shows_a_false_authenticated_badge_for_a_forged_header() {
    let email = common::provision("imap-forged-auth");
    let subject = format!("castl-forged-auth-{}", common::run_tag("forged"));
    let raw = build_forged_auth_probe(&email, &subject);
    append_raw(&email, "INBOX", &raw);

    let dir = temp_profile_dir("forgedauth");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);
    profile
        .sync_imap_json(&config)
        .expect("sync should succeed");

    let list_json = profile
        .list_messages_json("gm", "gm:INBOX", 1000, 0)
        .expect("list messages");
    let list: serde_json::Value = serde_json::from_str(&list_json).unwrap();
    let probe_header = list
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["subject"].as_str() == Some(subject.as_str()))
        .unwrap_or_else(|| panic!("probe '{subject}' not found in INBOX after sync"));
    let probe_id = probe_header["id"].as_str().unwrap();

    let detail_json = profile.get_message_json(probe_id).expect("get detail");
    let detail: serde_json::Value = serde_json::from_str(&detail_json).unwrap();
    let trust = &detail["trust"];
    assert_eq!(
        trust["spf"], "none",
        "forged header must never read as pass: {trust}"
    );
    assert_eq!(trust["dkim"], "none");
    assert_eq!(trust["dmarc"], "none");
    assert_eq!(
        trust["authserv_id"].as_str(),
        Some("evil.example"),
        "the authserv-id is still surfaced for the details popover, just never trusted"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Append `count` unique text probes in ONE IMAP session (fast bulk seed).
fn append_probes_bulk(email: &str, mailbox: &str, tag: &str, count: usize) {
    let mut session = common::imap_session(email);
    for i in 0..count {
        let raw = build_text_probe(
            email,
            &format!("castl-bulk-{tag}-{i}"),
            &format!("{tag}-{i}"),
            None,
        );
        session.append(mailbox, &[], &raw).expect("IMAP append");
    }
    let _ = session.logout();
}

/// Reopen/convergence fixture: a cleanly closed and reopened profile resumes
/// from its durable cursor (no refetch), and the durable diagnostics +
/// captured capability atoms survive the reopen. NOTE: this is clean-drop
/// persistence coverage — the profile is dropped and reopened INSIDE one
/// test process. It does not simulate a killed process or crash recovery;
/// SQLite journal recovery after an unclean exit is not exercised here.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_converges_and_diagnostics_survive_profile_reopen() {
    let email = common::provision("imap-restart");
    let tag = common::run_tag("restart");
    append_probes_bulk(&email, "INBOX", &tag, 3);

    let dir = temp_profile_dir("restart");
    let config = common::sync_config_json("gm", &email);

    // First lifetime: initial sync fetches the probes.
    let total = {
        let profile = open_profile(&dir).expect("open profile");
        let report = profile.sync_imap_json(&config).expect("first sync");
        let v: serde_json::Value = serde_json::from_str(&report).unwrap();
        let inbox = v["mailboxes"]
            .as_array()
            .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
            .expect("INBOX in report");
        assert!(inbox["fetched"].as_i64().unwrap() >= 3);
        inbox["total"].as_i64().unwrap()
        // profile dropped here = clean close; only the on-disk state carries over
    };

    // Second lifetime: the reopened profile must resume, not refetch.
    let profile = open_profile(&dir).expect("reopen profile");
    let report = profile.sync_imap_json(&config).expect("post-reopen sync");
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    let inbox = v["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in report");
    assert_eq!(
        inbox["fetched"].as_i64().unwrap() + inbox["descriptors_fetched"].as_i64().unwrap(),
        0,
        "a reopened profile must resume from the durable cursor, not refetch: {inbox}"
    );
    assert_eq!(inbox["total"].as_i64().unwrap(), total, "state converged");

    // Durable diagnostics + capability capture survive the reopen.
    let bundle_json = profile.support_bundle_json().expect("bundle after reopen");
    let bundle: serde_json::Value = serde_json::from_str(&bundle_json).unwrap();
    let acct = &bundle["accounts"][0];
    assert!(acct["last_success_at"].is_string());
    assert_eq!(acct["consecutive_failures"], 0);
    assert!(
        acct["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "IMAP4rev1"),
        "capability atoms must survive the reopen: {acct}"
    );
    let inbox_diag = acct["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["label"] == "inbox")
        .expect("inbox in bundle");
    assert!(inbox_diag["last_success_at"].is_string());
    assert_eq!(inbox_diag["total_messages"].as_i64().unwrap(), total);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Restart/convergence fixture for the staged-body pipeline: a backlog left
/// by the bounded in-sync hydration batch survives a profile reopen and
/// drains exactly once — the durable missing-body queue IS the checkpoint.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn hydration_backlog_resumes_after_profile_reopen() {
    // 30 probes (+ the provisioning bootstrap message) > the 25-per-mailbox
    // in-sync hydration batch, so the first sync must leave a backlog of
    // exactly (descriptors − 25) descriptor-only rows.
    // Unique per-run account: this test asserts EXACT counts, so it cannot
    // share the stable per-test-name address across container lifetimes.
    let tag = common::run_tag("hyd");
    let email = common::provision(&format!("imap-hydrate-{tag}"));
    append_probes_bulk(&email, "INBOX", &tag, 30);

    let dir = temp_profile_dir("hydrate-restart");
    let config = common::sync_config_json("gm", &email);

    let expected_backlog = {
        let profile = open_profile(&dir).expect("open profile");
        let report = profile.sync_imap_json(&config).expect("sync");
        let v: serde_json::Value = serde_json::from_str(&report).unwrap();
        let inbox = v["mailboxes"]
            .as_array()
            .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
            .expect("INBOX in report");
        let descriptors = inbox["descriptors_fetched"].as_i64().unwrap();
        assert!(descriptors >= 30, "all probes imported: {inbox}");
        assert_eq!(
            inbox["bodies_fetched"].as_i64().unwrap(),
            25,
            "in-sync hydration is bounded per mailbox: {inbox}"
        );
        descriptors - 25
        // profile dropped = clean close with the backlog on disk
    };

    let profile = open_profile(&dir).expect("reopen profile");
    let pass = profile
        .hydrate_pending_bodies_json(&config)
        .expect("hydration pass after reopen");
    let v: serde_json::Value = serde_json::from_str(&pass).unwrap();
    assert_eq!(
        v["hydrated"].as_i64().unwrap(),
        expected_backlog,
        "the reopened profile must drain exactly the surviving backlog: {v}"
    );
    assert_eq!(v["remaining"].as_i64().unwrap(), 0);
    assert!(v["bytes_fetched"].as_i64().unwrap() > 0);

    // A second pass finds an empty queue and does no work (and would fail
    // loudly rather than refetch if the checkpoint were not durable).
    let pass2 = profile
        .hydrate_pending_bodies_json(&config)
        .expect("second pass");
    let v2: serde_json::Value = serde_json::from_str(&pass2).unwrap();
    assert_eq!(v2["requested"].as_i64().unwrap(), 0);
    assert_eq!(v2["hydrated"].as_i64().unwrap(), 0);
    assert_eq!(v2["remaining"].as_i64().unwrap(), 0);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Build one deliberately malformed RFC822 blob per hostile shape. These are
/// appended RAW so GreenMail stores them byte-exact — the point is that the
/// SYNC pipeline (descriptor fetch, MIME parse, FTS, attachment descriptors)
/// must degrade gracefully, never crash, and never loop refetching.
fn build_malformed_probes(to: &str, tag: &str) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        // NOTE deliberately absent: a multipart whose body contains NO part
        // for the declared boundary. GreenMail 2.1.9 DROPS THE CONNECTION
        // (socket EOF, verified with a raw python fetch) when asked for that
        // message's BODYSTRUCTURE, so this provider variant cannot be
        // emulated with GreenMail — zero-part multipart resilience stays
        // covered by the deterministic core MIME tests only.
        ("raw-8bit-headers", {
            let mut raw = format!(
                "From: hostile@local.test\r\nTo: {to}\r\n\
                 Subject: castl-mime-{tag}-raw-"
            )
            .into_bytes();
            // Raw unencoded UTF-8 (and one bare 0xFF) straight in the
            // Subject header — real-world hostile mail does this.
            raw.extend_from_slice("Ã¼mlaut-".as_bytes());
            raw.push(0xFF);
            raw.extend_from_slice(
                b"\r\nMIME-Version: 1.0\r\n\r\nbody with the raw-header probe\r\n",
            );
            raw
        }),
        (
            "invalid-date-bare-msgid",
            format!(
                "From: hostile@local.test\r\nTo: {to}\r\n\
                 Subject: castl-mime-{tag}-bad-date\r\n\
                 Date: not a date at all\r\n\
                 Message-ID: bare-id-without-brackets\r\n\r\n\
                 body with the bad-date probe\r\n"
            )
            .into_bytes(),
        ),
        (
            "truncated-base64-attachment",
            format!(
                "From: hostile@local.test\r\nTo: {to}\r\n\
                 Subject: castl-mime-{tag}-bad-b64\r\n\
                 MIME-Version: 1.0\r\n\
                 Content-Type: multipart/mixed; boundary=\"bb\"\r\n\r\n\
                 --bb\r\n\
                 Content-Type: text/plain\r\n\r\n\
                 text part before the attachment\r\n\
                 --bb\r\n\
                 Content-Type: application/pdf; name=\"report.pdf\"\r\n\
                 Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
                 Content-Transfer-Encoding: base64\r\n\r\n\
                 !!!!not-base64-at-all%%%%\r\n\
                 --bb\r\n\
                 (a trailing part with NO closing boundary"
            )
            .into_bytes(),
        ),
    ]
}

#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn sync_survives_malformed_mime_and_stays_convergent() {
    // Unique per-run account: a failed earlier run must not leave hostile
    // rows behind that poison this run's counts (same pattern as the
    // count-exact hydration fixture).
    let tag = common::run_tag("mime");
    let email = common::provision(&format!("imap-malformed-mime-{tag}"));

    for (_, raw) in build_malformed_probes(&email, &tag) {
        append_raw(&email, "INBOX", &raw);
    }
    // One healthy control message.
    append_raw(
        &email,
        "INBOX",
        &build_text_probe(&email, &format!("castl-mime-{tag}-control"), &tag, None),
    );

    let dir = temp_profile_dir("malformed-mime");
    let profile = open_profile(&dir).expect("open profile");
    let config = common::sync_config_json("gm", &email);

    // First sync must succeed outright — hostile MIME is a data problem,
    // never a sync failure.
    let report = profile.sync_imap_json(&config).expect("sync succeeds");
    let v: serde_json::Value = serde_json::from_str(&report).unwrap();
    let inbox = v["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in report");
    let total = inbox["total"].as_i64().unwrap();
    assert!(
        total >= 5,
        "bootstrap + 3 hostile + 1 control expected, got {total}"
    );

    // Every stored row must have a readable header projection and a detail
    // that decodes — malformed input degrades, it never poisons the reads.
    let list = profile
        .list_messages_json("gm", "gm:INBOX", 500, 0)
        .expect("list");
    let listed: serde_json::Value = serde_json::from_str(&list).unwrap();
    let rows = listed.as_array().unwrap();
    assert_eq!(rows.len() as i64, total.min(500));
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let detail = profile.get_message_json(id).expect("detail readable");
        assert!(detail.contains("\"header\""), "detail decodes for {id}");
    }

    // The hostile subjects made it into rows (prefix survives even where the
    // 8-bit tail was sanitised or replaced).
    let subjects: Vec<String> = rows
        .iter()
        .filter_map(|r| r["subject"].as_str().map(|s| s.to_string()))
        .collect();
    for marker in ["-raw-", "bad-date", "bad-b64"] {
        assert!(
            subjects.iter().any(|s| s.contains(marker)),
            "hostile message {marker} ingested as a row: {subjects:?}"
        );
    }

    // The control body is searchable — hostile siblings didn't break FTS.
    let hits = profile
        .search_messages_json("gm", &tag, 500)
        .expect("search");
    let hit_arr: serde_json::Value = serde_json::from_str(&hits).unwrap();
    assert!(
        !hit_arr.as_array().unwrap().is_empty(),
        "control probe remains searchable"
    );

    // Convergence: a second sync fetches nothing new and the total holds —
    // malformed bodies must not loop through the missing-body queue.
    let report2 = profile.sync_imap_json(&config).expect("second sync");
    let v2: serde_json::Value = serde_json::from_str(&report2).unwrap();
    let inbox2 = v2["mailboxes"]
        .as_array()
        .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
        .expect("INBOX in second report");
    assert_eq!(inbox2["total"].as_i64().unwrap(), total, "no duplicates");
    assert_eq!(
        inbox2["bodies_fetched"].as_i64().unwrap_or(0),
        0,
        "no hostile body is refetched on a convergent second sync: {v2}"
    );

    // A bounded hydration pass finds nothing left to do (durable markers
    // exist even for the empty/attachment-only hostile bodies).
    let hydrate = profile
        .hydrate_pending_bodies_json(&config)
        .expect("hydration pass");
    let hv: serde_json::Value = serde_json::from_str(&hydrate).unwrap();
    assert_eq!(
        hv["hydrated"].as_i64().unwrap_or(0),
        0,
        "no missing-body loop for malformed messages: {hv}"
    );
}

/// S62 initial-history window against a real server: `UID SEARCH SINCE`
/// must parse and bound the first import. GreenMail stamps APPENDed mail
/// with the current INTERNALDATE, so a far-future window imports nothing
/// and a far-past one imports everything — no clock arithmetic needed.
#[test]
#[ignore = "requires local GreenMail (scripts/local-imap) running"]
fn initial_history_window_bounds_the_first_import() {
    let tag = common::run_tag("window");
    let email = common::provision(&format!("imap-window-{tag}"));
    append_probes_bulk(&email, "INBOX", &tag, 5);

    let config = |account_id: &str, since: &str| {
        serde_json::json!({
            "account_id": account_id,
            "email": email,
            "host": common::host(),
            "port": common::IMAP_PORT,
            "username": email,
            "password": email,
            "use_tls": false,
            "mailboxes": ["INBOX"],
            "initial_sync_since": since
        })
        .to_string()
    };
    let inbox_counts = |report: &str| -> (i64, i64) {
        let v: serde_json::Value = serde_json::from_str(report).unwrap();
        let inbox = v["mailboxes"]
            .as_array()
            .and_then(|m| m.iter().find(|mb| mb["path"] == "INBOX"))
            .expect("INBOX in report")
            .clone();
        assert!(inbox["error"].is_null(), "window search failed: {inbox}");
        (
            inbox["fetched"].as_i64().unwrap(),
            inbox["total"].as_i64().unwrap(),
        )
    };

    let dir = temp_profile_dir(&format!("window-future-{tag}"));
    let profile = open_profile(&dir).expect("open profile");
    let report = profile
        .sync_imap_json(&config("gm-window-future", "2099-01-01"))
        .expect("sync with a future window");
    assert_eq!(inbox_counts(&report), (0, 0), "nothing is that recent");
    let _ = std::fs::remove_dir_all(&dir);

    let dir = temp_profile_dir(&format!("window-past-{tag}"));
    let profile = open_profile(&dir).expect("open profile");
    let report = profile
        .sync_imap_json(&config("gm-window-past", "2000-01-01"))
        .expect("sync with a past window");
    let (fetched, total) = inbox_counts(&report);
    assert!(
        fetched >= 5 && total >= 5,
        "everything is newer than 2000: {report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
