// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Integration test that exercises the C ABI exactly as a C/Swift caller would.
// This is the automated FFI smoke test (runs on the Rust CI runner, no Swift
// toolchain required).

use castellum_ffi::{
    castellum_close_profile, castellum_open_profile, castellum_profile_schema_version,
    castellum_redact_diagnostic_text, castellum_string_free, castellum_version,
};
use std::ffi::{CStr, CString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    // Unique per RUN: pids recycle and these are never deleted, so a
    // pid+counter name reopens an earlier run's directory. See the note
    // in castellum_db::ai::tests::fresh_db.
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    castellum_testutil::scratch_root().join(format!(
        "castellum-ffi-test-{}-{stamp}-{n}",
        std::process::id()
    ))
}

const OK: i32 = 0;
const INVALID_ARGUMENT: i32 = 1;
const INVALID_HANDLE: i32 = 2;

#[test]
fn version_round_trips_and_frees() {
    let ptr = castellum_version();
    assert!(!ptr.is_null());
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .expect("utf8")
        .to_owned();
    assert!(!s.is_empty());
    castellum_string_free(ptr);
    // Freeing null must be safe.
    castellum_string_free(std::ptr::null_mut());
}

#[test]
fn redact_diagnostic_text_scrubs_addresses_and_secrets_without_a_handle() {
    let input =
        CString::new("contact alice@example.test password=SENTINEL_NOT_A_REAL_SECRET").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_redact_diagnostic_text(input.as_ptr(), &mut out),
        OK
    );
    let text = take_json(out); // reused helper: takes ownership + frees
    assert!(!text.contains("alice@example.test"));
    assert!(!text.contains("SENTINEL"));

    // Null out_text is an invalid argument.
    assert_eq!(
        castellum_redact_diagnostic_text(input.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    // Null input is an invalid argument.
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_redact_diagnostic_text(std::ptr::null(), &mut out2),
        INVALID_ARGUMENT
    );
    assert!(out2.is_null());
}

#[test]
fn open_then_close_round_trips() {
    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();

    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    assert_ne!(handle, 0);
    assert!(dir.exists());

    assert_eq!(castellum_close_profile(handle), OK);
    // Closing the same handle again is now invalid.
    assert_eq!(castellum_close_profile(handle), INVALID_HANDLE);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn schema_version_reported_for_open_profile() {
    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let mut version: i64 = -1;
    assert_eq!(castellum_profile_schema_version(handle, &mut version), OK);
    assert!(
        version >= 1,
        "schema migrated to at least v1, got {version}"
    );

    // Unknown handle and null out-param are rejected.
    let mut v2: i64 = -1;
    assert_eq!(
        castellum_profile_schema_version(999_999, &mut v2),
        INVALID_HANDLE
    );
    assert_eq!(
        castellum_profile_schema_version(handle, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn null_path_is_invalid_argument() {
    let mut handle: u64 = 0;
    assert_eq!(
        castellum_open_profile(std::ptr::null(), &mut handle),
        INVALID_ARGUMENT
    );
    assert_eq!(handle, 0);
}

#[test]
fn null_out_handle_is_invalid_argument() {
    let c_path = CString::new(unique_tmp().to_str().unwrap()).unwrap();
    assert_eq!(
        castellum_open_profile(c_path.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
}

#[test]
fn unknown_and_zero_handles_are_invalid() {
    assert_eq!(castellum_close_profile(0), INVALID_HANDLE);
    assert_eq!(castellum_close_profile(9_999_999), INVALID_HANDLE);
}

#[test]
fn draft_lifecycle_over_the_c_abi() {
    use castellum_ffi::{
        castellum_delete_draft, castellum_list_drafts, castellum_save_draft, castellum_send_draft,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // save/list/delete need no account row; send_draft's op insert FK-references
    // accounts (created by sync, which has no offline path here), so the happy
    // send path is covered by core tests + the GreenMail e2e. Over the ABI we
    // exercise the draft lifecycle and clean failures.
    let draft = CString::new(
        r#"{"account_id":"acc","to":[{"name":"Bob","email":"bob@example.com"}],
            "subject":"ABI draft","body_text":"hi"}"#,
    )
    .unwrap();
    let mut draft_id: i64 = 0;
    assert_eq!(
        castellum_save_draft(handle, draft.as_ptr(), &mut draft_id),
        OK
    );
    assert!(draft_id > 0);

    let acc = CString::new("acc").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_drafts(handle, acc.as_ptr(), &mut out), OK);
    let listed = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_owned();
    castellum_string_free(out);
    assert!(listed.contains("\"subject\":\"ABI draft\""));
    assert!(listed.contains("\"status\":\"draft\""));

    // send_draft on a missing draft is a clean IO-status failure, not a crash.
    assert_ne!(castellum_send_draft(handle, 999_999), OK);

    assert_eq!(castellum_delete_draft(handle, draft_id), OK);
    // Deleting again: clean failure.
    assert_ne!(castellum_delete_draft(handle, draft_id), OK);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_draft_to_server_queues_when_still_a_draft_and_noops_after_send() {
    use castellum_ffi::{
        castellum_save_draft, castellum_save_draft_to_server, castellum_send_draft,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // queue_server_draft_save needs a write-enabled account row (mailboxes/
    // operations both reference it); seeded directly through castellum-db at
    // the same profile db path, same pattern as this file's other tests that
    // need an account with no ABI call to create one.
    {
        use castellum_db::Database;
        use castellum_models::Account;
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "a@x.com".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
    }

    let draft = CString::new(
        r#"{"account_id":"acc","to":[{"email":"b@x.com"}],"subject":"Hi","body_text":"hello"}"#,
    )
    .unwrap();
    let mut draft_id: i64 = 0;
    assert_eq!(
        castellum_save_draft(handle, draft.as_ptr(), &mut draft_id),
        OK
    );

    let from_json = CString::new(r#"{"from_email":"a@x.com"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    let status = castellum_save_draft_to_server(handle, draft_id, from_json.as_ptr(), &mut out);
    assert_eq!(status, OK);
    let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_string();
    castellum_string_free(out);
    assert!(json.contains("\"queued\":true"), "got: {json}");

    // Once sent, a debounced save that lands late must be a silent no-op.
    assert_eq!(castellum_send_draft(handle, draft_id), OK);
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    let status2 = castellum_save_draft_to_server(handle, draft_id, from_json.as_ptr(), &mut out2);
    assert_eq!(status2, OK);
    let json2 = unsafe { CStr::from_ptr(out2) }
        .to_str()
        .unwrap()
        .to_string();
    castellum_string_free(out2);
    assert!(json2.contains("\"queued\":false"), "got: {json2}");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn operation_retry_and_cancel_over_the_c_abi() {
    use castellum_ffi::{castellum_cancel_operation, castellum_retry_operation};

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Unknown op ids are clean errors, not crashes (creating a real op needs
    // an account row from sync; the status transitions themselves are pinned
    // by the castellum-db tests).
    assert_ne!(castellum_retry_operation(handle, 999_999), OK);
    assert_ne!(castellum_cancel_operation(handle, 999_999), OK);
    // Invalid handles are invalid arguments.
    assert_ne!(castellum_retry_operation(0, 1), OK);
    assert_ne!(castellum_cancel_operation(0, 1), OK);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn send_pending_rejects_bad_config_json() {
    use castellum_ffi::castellum_send_pending;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let bad = CString::new("{not json").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(castellum_send_pending(handle, bad.as_ptr(), &mut out), OK);
    assert!(out.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// OB1: the Outbox calls over the C ABI. No network: the account is switched
/// to read-only before Send Now, which must refuse without connecting.
#[test]
fn outbox_sends_send_now_and_requeue_over_the_c_abi() {
    use castellum_ffi::{
        castellum_outbox_sends, castellum_requeue_failed_sends, castellum_save_draft,
        castellum_send_draft, castellum_send_draft_now,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    let db = {
        use castellum_db::Database;
        use castellum_models::Account;
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc".into(),
            email: "a@x.com".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.set_account_write_enabled("acc", true).unwrap();
        db
    };
    let draft = CString::new(
        r#"{"account_id":"acc","to":[{"email":"b@x.com"}],"subject":"Queued","body_text":"x"}"#,
    )
    .unwrap();
    let mut draft_id: i64 = 0;
    assert_eq!(
        castellum_save_draft(handle, draft.as_ptr(), &mut draft_id),
        OK
    );
    assert_eq!(castellum_send_draft(handle, draft_id), OK);

    let acc = CString::new("acc").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_outbox_sends(handle, acc.as_ptr(), &mut out), OK);
    let listed = take_json(out);
    assert!(listed.contains("\"subject\":\"Queued\""), "got: {listed}");
    assert!(listed.contains("\"status\":\"queued\""), "got: {listed}");
    assert!(
        listed.contains("\"recipients\":[\"b@x.com\"]"),
        "got: {listed}"
    );

    db.set_account_write_enabled("acc", false).unwrap();
    let cfg = CString::new(
        r#"{"account_id":"acc","from_email":"a@x.com","smtp":{"host":"127.0.0.1","port":1,
            "username":"a","password":"p","use_tls":false}}"#,
    )
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_send_draft_now(handle, cfg.as_ptr(), draft_id, &mut out),
        OK
    );
    let report = take_json(out);
    assert!(report.contains("\"blocked\":1"), "got: {report}");
    assert!(report.contains("\"sent\":0"), "got: {report}");

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_requeue_failed_sends(handle, acc.as_ptr(), &mut out),
        OK
    );
    assert_eq!(take_json(out), r#"{"requeued":0}"#);

    // Null arguments are refused, not crashes.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_outbox_sends(handle, std::ptr::null(), &mut out),
        OK
    );
    assert_ne!(
        castellum_send_draft_now(handle, std::ptr::null(), draft_id, &mut out),
        OK
    );

    drop(db);
    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- contacts (slice 8a) -----------------------------------------------------

/// Take an out-JSON C string, free it, and return the owned Rust String.
fn take_json(ptr: *mut std::ffi::c_char) -> String {
    assert!(!ptr.is_null());
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .expect("utf8")
        .to_owned();
    castellum_string_free(ptr);
    s
}

#[test]
fn contacts_refresh_and_list_json() {
    use castellum_ffi::{castellum_list_contacts, castellum_refresh_contacts};

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Refresh on an empty profile: OK with a zeroed report.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_refresh_contacts(handle, &mut out), OK);
    let report = take_json(out);
    assert!(report.contains("\"contacts_upserted\":0"), "{report}");
    assert!(report.contains("\"edges_rebuilt\":0"), "{report}");

    // Null query is valid ("no filter"); result is an empty JSON array.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_contacts(handle, std::ptr::null(), 1, 100, &mut out),
        OK
    );
    assert_eq!(take_json(out), "[]");

    // With a query string.
    let q = CString::new("alice").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_contacts(handle, q.as_ptr(), 0, 100, &mut out),
        OK
    );
    assert_eq!(take_json(out), "[]");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn contact_create_update_get_roundtrip() {
    use castellum_ffi::{
        castellum_create_contact, castellum_get_contact, castellum_update_contact,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let create = CString::new(r#"{"email":"Ada@X.Test","name":"Ada"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_create_contact(handle, create.as_ptr(), &mut out),
        OK
    );
    let created = take_json(out);
    assert!(created.contains("\"email\":\"ada@x.test\""), "{created}");
    assert!(created.contains("\"is_user_created\":true"), "{created}");
    let id = created
        .split("\"id\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<i64>().ok())
        .expect("id in created json");

    let update = CString::new(format!(
        r#"{{"id":{id},"display_name":"Ada L","notes":"met her","is_hidden":false}}"#
    ))
    .unwrap();
    assert_eq!(castellum_update_contact(handle, update.as_ptr()), OK);

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_get_contact(handle, id, &mut out), OK);
    let got = take_json(out);
    assert!(got.contains("\"display_name\":\"Ada L\""), "{got}");
    assert!(got.contains("\"notes\":\"met her\""), "{got}");

    // Missing contact: JSON null, still OK.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_get_contact(handle, 999_999, &mut out), OK);
    assert_eq!(take_json(out), "null");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn contacts_invalid_args_and_handles() {
    use castellum_ffi::{
        castellum_create_contact, castellum_get_contact, castellum_list_contacts,
        castellum_refresh_contacts, castellum_update_contact,
    };

    // Handle 0 / unknown handle are rejected on every entry point.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_refresh_contacts(0, &mut out), INVALID_HANDLE);
    assert_eq!(
        castellum_list_contacts(0, std::ptr::null(), 0, 10, &mut out),
        INVALID_HANDLE
    );
    assert_eq!(
        castellum_get_contact(999_999_999, 1, &mut out),
        INVALID_HANDLE
    );
    assert!(out.is_null());

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Null out_json rejected.
    assert_eq!(
        castellum_refresh_contacts(handle, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    // Null json payloads rejected.
    assert_eq!(
        castellum_update_contact(handle, std::ptr::null()),
        INVALID_ARGUMENT
    );
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_create_contact(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    // Malformed json payloads: clean failure, not a crash.
    let bad = CString::new("{not json").unwrap();
    assert_ne!(castellum_update_contact(handle, bad.as_ptr()), OK);
    assert_ne!(castellum_create_contact(handle, bad.as_ptr(), &mut out), OK);
    assert!(out.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- AI job queue (slice 7b) -------------------------------------------------

#[test]
fn ai_enqueue_poll_cancel_roundtrip() {
    use castellum_ffi::{
        castellum_cancel_ai_job, castellum_enqueue_ai_job, castellum_get_ai_output,
        castellum_poll_ai_job,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Enqueue for a non-existent message: clean rejection (no crash, no UB).
    let req =
        CString::new(r#"{"feature":"summarize_message","message_id":"no-such-message"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(castellum_enqueue_ai_job(handle, req.as_ptr(), &mut out), OK);
    assert!(out.is_null(), "out must remain null on failure");

    // poll_ai_job for an unknown job returns JSON null (not an error).
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_poll_ai_job(handle, 9_999, 0, &mut out), OK);
    assert_eq!(take_json(out), "null");

    // cancel for an unknown job: either OK or IO error is acceptable — no panic.
    let rc = castellum_cancel_ai_job(handle, 9_999);
    let _ = rc;

    // get_ai_output for missing key returns JSON null.
    let scope = CString::new("message").unwrap();
    let owner = CString::new("no-such-id").unwrap();
    let feat = CString::new("summarize_message").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_get_ai_output(
            handle,
            scope.as_ptr(),
            owner.as_ptr(),
            feat.as_ptr(),
            &mut out
        ),
        OK
    );
    assert_eq!(take_json(out), "null");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ai_invalid_args_and_handles() {
    use castellum_ffi::{
        castellum_ai_list_models, castellum_cancel_ai_job, castellum_enqueue_ai_job,
        castellum_get_ai_output, castellum_poll_ai_job, castellum_run_ai_jobs,
    };

    // All functions reject handle 0 / unknown handles.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_enqueue_ai_job(0, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_run_ai_jobs(0, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(castellum_poll_ai_job(0, 1, 0, &mut out), INVALID_HANDLE);
    assert_eq!(castellum_cancel_ai_job(0, 1), INVALID_HANDLE);
    assert_eq!(
        castellum_ai_list_models(0, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_get_ai_output(
            0,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            &mut out
        ),
        INVALID_ARGUMENT
    );

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Null out_json pointer rejected.
    let cfg = CString::new(r#"{"endpoint":"http://127.0.0.1:11434","model":"x"}"#).unwrap();
    assert_eq!(
        castellum_enqueue_ai_job(handle, cfg.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_poll_ai_job(handle, 1, 0, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_ai_list_models(handle, cfg.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_get_ai_output(
            handle,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            &mut out
        ),
        INVALID_ARGUMENT
    );

    // Null request_json / config_json rejected.
    assert_eq!(
        castellum_enqueue_ai_job(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_run_ai_jobs(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_ai_list_models(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert!(out.is_null());

    // Malformed JSON: clean non-OK status, no crash.
    let bad = CString::new("{not json").unwrap();
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_enqueue_ai_job(handle, bad.as_ptr(), &mut out2),
        OK
    );
    assert!(out2.is_null());
    let mut out3: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(castellum_run_ai_jobs(handle, bad.as_ptr(), &mut out3), OK);
    assert!(out3.is_null());
    // ai_list_models with malformed JSON: clean failure, no network involved.
    let mut out4: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_ai_list_models(handle, bad.as_ptr(), &mut out4),
        OK
    );
    assert!(out4.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ai_get_output_null_for_missing_key() {
    use castellum_ffi::castellum_get_ai_output;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let scope = CString::new("message").unwrap();
    let owner = CString::new("msg:no-such").unwrap();
    let feat = CString::new("summarize_message").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_get_ai_output(
            handle,
            scope.as_ptr(),
            owner.as_ptr(),
            feat.as_ptr(),
            &mut out
        ),
        OK
    );
    assert_eq!(take_json(out), "null");

    // Unknown handle rejected.
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_get_ai_output(
            999_999,
            scope.as_ptr(),
            owner.as_ptr(),
            feat.as_ptr(),
            &mut out2
        ),
        INVALID_HANDLE
    );
    assert!(out2.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// "Find similar" (S53-UI), the "message already has a stored embedding"
/// path — no live Ollama backend needed (the backend-required ad-hoc-embed
/// path is covered by castellum_core::ai's Rust-level MockBackend tests
/// instead; this crate has no FFI test harness stub for AiBackend). Seeds
/// two messages directly through castellum-db at the SAME
/// `<profile>/castellum.db` path `castellum_open_profile` creates (the C ABI
/// itself has no "insert a message" call — real data arrives via IMAP
/// sync), each already embedded, then calls the FFI function and confirms
/// the anchor is excluded and the closer match sorts first.
#[test]
fn find_similar_messages_over_the_c_abi_with_a_stored_anchor_embedding() {
    use castellum_ffi::castellum_find_similar_messages;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    {
        use castellum_db::Database;
        use castellum_models::{Account, Mailbox, NewMessage};
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "alice@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        let seed = |uid: i64, subject: &str| NewMessage {
            id: format!("acc1:INBOX:{uid}"),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(uid),
            rfc822_message_id: Some(format!("<{uid}@test>")),
            subject: Some(subject.into()),
            from_name: Some("Bob".into()),
            from_address: Some("bob@example.test".into()),
            sent_at: Some(1_700_000_000 + uid),
            snippet: Some("hi".into()),
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("hello".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        };
        db.upsert_message(&seed(1, "anchor")).unwrap();
        db.upsert_message(&seed(2, "close match")).unwrap();
        let embed_le = |v: &[f32]| -> Vec<u8> { v.iter().flat_map(|f| f.to_le_bytes()).collect() };
        db.upsert_message_embedding("acc1:INBOX:1", "m", 3, &embed_le(&[1.0, 0.0, 0.0]))
            .unwrap();
        db.upsert_message_embedding("acc1:INBOX:2", "m", 3, &embed_le(&[0.9, 0.1, 0.0]))
            .unwrap();
    }

    let cfg =
        CString::new(r#"{"endpoint":"http://127.0.0.1:11434","model":"x","embedding_model":"m"}"#)
            .unwrap();
    let msg_id = CString::new("acc1:INBOX:1").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_find_similar_messages(
            handle,
            cfg.as_ptr(),
            msg_id.as_ptr(),
            std::ptr::null(), // account_id: every account
            50,
            &mut out
        ),
        OK
    );
    let json = take_json(out);
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let results = value["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "the anchor never appears in its own results"
    );
    assert_eq!(results[0]["header"]["id"], "acc1:INBOX:2");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn find_similar_messages_invalid_args_and_handles() {
    use castellum_ffi::castellum_find_similar_messages;

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_find_similar_messages(
            0,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            50,
            &mut out
        ),
        INVALID_ARGUMENT
    );

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let cfg = CString::new(r#"{"endpoint":"http://127.0.0.1:11434","model":"x"}"#).unwrap();
    let msg_id = CString::new("no-such-message").unwrap();
    // Null out_json pointer rejected.
    assert_eq!(
        castellum_find_similar_messages(
            handle,
            cfg.as_ptr(),
            msg_id.as_ptr(),
            std::ptr::null(),
            50,
            std::ptr::null_mut()
        ),
        INVALID_ARGUMENT
    );
    // Unknown handle rejected.
    assert_eq!(
        castellum_find_similar_messages(
            999_999,
            cfg.as_ptr(),
            msg_id.as_ptr(),
            std::ptr::null(),
            50,
            &mut out
        ),
        INVALID_HANDLE
    );
    assert!(out.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn enrichment_preview_and_save_shapes_over_the_c_abi() {
    // Live FFI round-trip for the enrichment dispatch. This is the contract
    // test that guards the Swift decoders:
    // - save:false rows are EXACTLY {field, value, provenance}  (PreviewEnrichmentField)
    // - save:true  rows carry {id, source, field, value, provenance, fetched_at}
    //   (EnrichmentField)
    use castellum_ffi::{castellum_create_contact, castellum_enrichment};

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Create a contact to enrich.
    let create = CString::new(r#"{"email":"ffi-enrich@x.test"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_create_contact(handle, create.as_ptr(), &mut out),
        OK
    );
    let created = take_json(out);
    let contact_id = created
        .split("\"id\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse::<i64>().ok())
        .expect("id in created json");

    // ── Preview (save:false): exactly field/value/provenance per row ────────
    let preview_op = CString::new(format!(
        r#"{{"op":"enrich","contact_id":{contact_id},"source_id":"manual","input":"Eve Adams at Initech\nBio line","save":false}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_enrichment(handle, preview_op.as_ptr(), &mut out),
        OK
    );
    let preview_json = take_json(out);
    let preview: serde_json::Value = serde_json::from_str(&preview_json).unwrap();
    let rows = preview.as_array().expect("preview is a JSON array");
    assert!(!rows.is_empty(), "{preview_json}");
    for row in rows {
        let obj = row.as_object().expect("row is an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["field", "provenance", "value"],
            "preview row shape drifted — Swift PreviewEnrichmentField will break: {row}"
        );
        assert!(row["provenance"].is_string(), "{row}");
    }

    // ── Save (save:true): rows now carry DB identity fields ─────────────────
    let save_op = CString::new(format!(
        r#"{{"op":"enrich","contact_id":{contact_id},"source_id":"manual","input":"Eve Adams at Initech\nBio line","save":true}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_enrichment(handle, save_op.as_ptr(), &mut out), OK);
    let saved_json = take_json(out);
    let saved: serde_json::Value = serde_json::from_str(&saved_json).unwrap();
    let rows = saved.as_array().expect("saved is a JSON array");
    assert!(!rows.is_empty(), "{saved_json}");
    for row in rows {
        let obj = row.as_object().expect("row is an object");
        for key in ["id", "source", "field", "value", "fetched_at"] {
            assert!(obj.contains_key(key), "saved row missing {key}: {row}");
        }
        assert!(row["id"].as_i64().unwrap() > 0, "real DB id: {row}");
    }

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// People network graph (slice 8c)
// ---------------------------------------------------------------------------

#[test]
fn ffi_graph_shape() {
    use castellum_ffi::castellum_contact_graph;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Empty profile still returns the full envelope: {"nodes":[],"edges":[]}.
    let pid = CString::new("no-such-project").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_contact_graph(handle, pid.as_ptr(), 100, &mut out),
        OK
    );
    let v: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let obj = v.as_object().expect("graph is a JSON object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["edges", "nodes"],
        "graph envelope drifted — Swift ContactGraph will break: {v}"
    );
    assert!(v["nodes"].is_array(), "{v}");
    assert!(v["edges"].is_array(), "{v}");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ffi_graph_null_project() {
    use castellum_ffi::castellum_contact_graph;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // NULL project_id means "global graph" and must be accepted.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_contact_graph(handle, std::ptr::null(), 100, &mut out),
        OK
    );
    let v: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(v["nodes"].as_array().unwrap().len(), 0, "{v}");
    assert_eq!(v["edges"].as_array().unwrap().len(), 0, "{v}");

    // Out-of-range max_nodes is clamped in core, not an error.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_contact_graph(handle, std::ptr::null(), 0, &mut out),
        OK
    );
    castellum_string_free(out);
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_contact_graph(handle, std::ptr::null(), 1_000_000, &mut out),
        OK
    );
    castellum_string_free(out);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ffi_graph_invalid_handle() {
    use castellum_ffi::castellum_contact_graph;

    // Unknown / zero handles are rejected without touching out_json.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_contact_graph(0, std::ptr::null(), 100, &mut out),
        INVALID_HANDLE
    );
    assert!(out.is_null());
    assert_eq!(
        castellum_contact_graph(9_999_999, std::ptr::null(), 100, &mut out),
        INVALID_HANDLE
    );
    assert!(out.is_null());

    // Null out_json is an invalid argument even with a bogus handle.
    assert_eq!(
        castellum_contact_graph(9_999_999, std::ptr::null(), 100, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
}

#[test]
fn hydrate_pending_bodies_empty_queue_returns_zero_report_without_network() {
    use castellum_ffi::castellum_hydrate_pending_bodies;

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let path = CString::new(dir.join("profile.db").to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(path.as_ptr(), &mut handle), OK);

    // An unroutable host proves the empty-queue path never opens a connection:
    // the call would otherwise fail (or hang) instead of returning a report.
    let config = CString::new(
        r#"{"account_id":"acc","email":"t@local.test","host":"192.0.2.1","port":1,
            "username":"u","password":"p","use_tls":false}"#,
    )
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_hydrate_pending_bodies(handle, config.as_ptr(), &mut out),
        OK
    );
    let json = take_json(out);
    assert!(json.contains("\"account_id\":\"acc\""), "{json}");
    assert!(json.contains("\"remaining\":0"), "{json}");
    assert!(json.contains("\"hydrated\":0"), "{json}");

    // Null config / null out_json are invalid arguments.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_hydrate_pending_bodies(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert!(out.is_null());
    assert_eq!(
        castellum_hydrate_pending_bodies(handle, config.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn support_bundle_and_integrity_check_return_valid_redacted_json() {
    use castellum_ffi::{castellum_integrity_check, castellum_support_bundle};

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let path = CString::new(dir.join("profile.db").to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(path.as_ptr(), &mut handle), OK);

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_support_bundle(handle, &mut out), OK);
    let json = take_json(out);
    let v: serde_json::Value = serde_json::from_str(&json).expect("bundle is valid JSON");
    assert_eq!(v["bundle_schema"], 1);
    assert!(v["schema_version"].as_i64().unwrap() >= 16);
    assert!(v["core_version"].is_string());
    assert_eq!(v["integrity"]["ok"], true);
    assert!(v["accounts"].as_array().unwrap().is_empty());

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_integrity_check(handle, &mut out), OK);
    let json = take_json(out);
    let v: serde_json::Value = serde_json::from_str(&json).expect("integrity is valid JSON");
    assert_eq!(v["ok"], true);
    assert_eq!(v["issue_count"], 0);

    // Null out_json is an invalid argument; a bogus handle is invalid.
    assert_eq!(
        castellum_support_bundle(handle, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_support_bundle(9_999_999, &mut out),
        INVALID_HANDLE
    );
    assert!(out.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn local_diagnostics_returns_valid_json_over_the_c_abi() {
    use castellum_ffi::castellum_local_diagnostics;

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let path = CString::new(dir.join("profile.db").to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(path.as_ptr(), &mut handle), OK);

    // Empty profile: accounts array is present but empty.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_local_diagnostics(handle, &mut out), OK);
    let json = take_json(out);
    let v: serde_json::Value =
        serde_json::from_str(&json).expect("local diagnostics is valid JSON");
    assert!(v["accounts"].as_array().unwrap().is_empty());

    // Null out_json is an invalid argument.
    assert_eq!(
        castellum_local_diagnostics(handle, std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    // Bogus handle is an invalid handle.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_local_diagnostics(9_999_999, &mut out),
        INVALID_HANDLE
    );
    assert!(out.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn account_write_policy_over_the_c_abi() {
    use castellum_ffi::{castellum_account_write_enabled, castellum_set_account_write_enabled};

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Fail-closed: an unknown account reads 0 (evaluation/read-only). (Real
    // accounts are created by sync; the full toggle round-trip is covered by
    // the db-layer test and the GreenMail e2e.)
    let acc = CString::new("acc").unwrap();
    let mut enabled: i32 = 42;
    assert_eq!(
        castellum_account_write_enabled(handle, acc.as_ptr(), &mut enabled),
        OK
    );
    assert_eq!(enabled, 0, "unknown account is read-only");

    // Null out-pointer / bad handle are clean argument/handle errors.
    assert_eq!(
        castellum_account_write_enabled(handle, acc.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    let mut sink: i32 = 0;
    assert_eq!(
        castellum_account_write_enabled(9_999_999, acc.as_ptr(), &mut sink),
        INVALID_HANDLE
    );

    // Enabling writes on a nonexistent account is a clean failure, not a crash.
    assert_ne!(
        castellum_set_account_write_enabled(handle, acc.as_ptr(), 1),
        OK
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn link_model_round_trips_over_the_abi() {
    use castellum_ffi::{
        castellum_create_contact, castellum_links_for, castellum_set_link_note, castellum_unlink,
        castellum_upsert_link, castellum_upsert_project,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Endpoint existence is enforced on write, so link real rows: a project
    // and a hand-created contact.
    let proj_req = CString::new(r#"{"name":"P","status":"active"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_project(handle, proj_req.as_ptr(), &mut out),
        OK
    );
    let project: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let pid = project["id"].as_str().unwrap().to_string();

    let contact_req = CString::new(r#"{"email":"c@x.test"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_create_contact(handle, contact_req.as_ptr(), &mut out),
        OK
    );
    let contact: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let cid = contact["id"].to_string();

    // Manual upsert returns the stored record.
    let req = CString::new(format!(
        r#"{{"src_kind":"project","src_id":"{pid}",
            "dst_kind":"contact","dst_id":"{cid}","relation":"involves"}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_upsert_link(handle, req.as_ptr(), &mut out), OK);
    let v: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(v["origin"], "manual");

    // Note set + read back through links_for.
    let note_req = CString::new(format!(
        r#"{{"src_kind":"project","src_id":"{pid}",
            "dst_kind":"contact","dst_id":"{cid}",
            "relation":"involves","note":"evidence check"}}"#
    ))
    .unwrap();
    assert_eq!(castellum_set_link_note(handle, note_req.as_ptr()), OK);
    let kind = CString::new("project").unwrap();
    let id = CString::new(pid.clone()).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_links_for(handle, kind.as_ptr(), id.as_ptr(), &mut out),
        OK
    );
    let arr: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["note"], "evidence check");

    // Unlink is idempotent; unknown kinds are invalid input errors.
    assert_eq!(castellum_unlink(handle, req.as_ptr()), OK);
    assert_eq!(castellum_unlink(handle, req.as_ptr()), OK);
    let bad = CString::new(
        r#"{"src_kind":"folder","src_id":"x",
            "dst_kind":"message","dst_id":"m","relation":"includes"}"#,
    )
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(castellum_upsert_link(handle, bad.as_ptr(), &mut out), OK);
    assert!(out.is_null());

    // Null request json is an invalid argument.
    assert_eq!(
        castellum_upsert_link(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn work_items_round_trip_over_the_abi() {
    use castellum_ffi::{
        castellum_delete_work_item, castellum_list_work_items, castellum_upsert_project,
        castellum_upsert_work_item,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // A work item needs a real project (FK).
    let proj_req = CString::new(r#"{"name":"P","status":"active"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_project(handle, proj_req.as_ptr(), &mut out),
        OK
    );
    let project: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let pid = project["id"].as_str().unwrap();

    let req = CString::new(format!(
        r#"{{"project_id":"{pid}","title":"Chase reply","state":"waiting"}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_work_item(handle, req.as_ptr(), &mut out),
        OK
    );
    let item: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(item["state"], "waiting");
    let wid = item["id"].as_str().unwrap();

    let c_pid = CString::new(pid).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_work_items(handle, c_pid.as_ptr(), &mut out),
        OK
    );
    let listed: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Invalid state is an error; the item list is unchanged.
    let bad = CString::new(format!(
        r#"{{"project_id":"{pid}","title":"T","state":"doing"}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_upsert_work_item(handle, bad.as_ptr(), &mut out),
        OK
    );
    assert!(out.is_null());

    let c_wid = CString::new(wid).unwrap();
    assert_eq!(castellum_delete_work_item(handle, c_wid.as_ptr()), OK);
    assert_ne!(castellum_delete_work_item(handle, c_wid.as_ptr()), OK);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn project_timeline_round_trips_over_the_abi() {
    use castellum_ffi::{
        castellum_add_timeline_entry, castellum_list_timeline, castellum_record_decision,
        castellum_upsert_project, castellum_upsert_work_item,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let call = |f: unsafe extern "C" fn(
        u64,
        *const std::ffi::c_char,
        *mut *mut std::ffi::c_char,
    ) -> i32,
                req: &str|
     -> (i32, Option<serde_json::Value>) {
        let c = CString::new(req).unwrap();
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = unsafe { f(handle, c.as_ptr(), &mut out) };
        if rc == OK {
            (rc, Some(serde_json::from_str(&take_json(out)).unwrap()))
        } else {
            assert!(out.is_null());
            (rc, None)
        }
    };

    let (rc, project) = call(
        castellum_upsert_project,
        r#"{"name":"P","status":"active"}"#,
    );
    assert_eq!(rc, OK);
    let pid = project.unwrap()["id"].as_str().unwrap().to_string();

    let (rc, e) = call(
        castellum_add_timeline_entry,
        &format!(r#"{{"project_id":"{pid}","kind":"activity","summary":"Kickoff"}}"#),
    );
    assert_eq!(rc, OK);
    assert_eq!(e.unwrap()["kind"], "activity");
    let (rc, d) = call(
        castellum_record_decision,
        &format!(r#"{{"project_id":"{pid}","summary":"Ship in Q4"}}"#),
    );
    assert_eq!(rc, OK);
    assert_eq!(d.unwrap()["kind"], "decision");

    // A work-item write appends through the hook.
    let (rc, _) = call(
        castellum_upsert_work_item,
        &format!(r#"{{"project_id":"{pid}","title":"Chase","state":"waiting"}}"#),
    );
    assert_eq!(rc, OK);

    let (rc, all) = call(
        castellum_list_timeline,
        &format!(r#"{{"project_id":"{pid}"}}"#),
    );
    assert_eq!(rc, OK);
    let all = all.unwrap();
    assert_eq!(all.as_array().unwrap().len(), 3);
    let (_, page) = call(
        castellum_list_timeline,
        &format!(r#"{{"project_id":"{pid}","kinds":["decision"],"limit":5,"offset":0}}"#),
    );
    assert_eq!(page.unwrap().as_array().unwrap().len(), 1);

    // Bad input is rejected without writing; null request is invalid.
    let (rc, _) = call(
        castellum_add_timeline_entry,
        &format!(r#"{{"project_id":"{pid}","kind":"work_item_change","summary":"x"}}"#),
    );
    assert_ne!(rc, OK);
    let (rc, _) = call(castellum_list_timeline, "not json");
    assert_ne!(rc, OK);
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_timeline(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    let (_, after) = call(
        castellum_list_timeline,
        &format!(r#"{{"project_id":"{pid}"}}"#),
    );
    assert_eq!(after.unwrap().as_array().unwrap().len(), 3);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn work_item_dates_and_filters_over_the_abi() {
    use castellum_ffi::{
        castellum_list_work_items_filtered, castellum_upsert_work_item, castellum_work_items_for,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let call = |f: unsafe fn(u64, *const std::ffi::c_char, *mut *mut std::ffi::c_char) -> i32,
                json: &str|
     -> Option<serde_json::Value> {
        let c = CString::new(json).unwrap();
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = unsafe { f(handle, c.as_ptr(), &mut out) };
        if rc == OK {
            Some(serde_json::from_str(&take_json(out)).unwrap())
        } else {
            assert!(out.is_null(), "no output on failure");
            None
        }
    };
    unsafe fn upsert(h: u64, j: *const std::ffi::c_char, o: *mut *mut std::ffi::c_char) -> i32 {
        castellum_upsert_work_item(h, j, o)
    }
    unsafe fn filtered(h: u64, j: *const std::ffi::c_char, o: *mut *mut std::ffi::c_char) -> i32 {
        castellum_list_work_items_filtered(h, j, o)
    }
    unsafe fn items_for(h: u64, j: *const std::ffi::c_char, o: *mut *mut std::ffi::c_char) -> i32 {
        castellum_work_items_for(h, j, o)
    }

    // New shape: due date, no project (unassigned). Old shape: neither.
    let dated = call(
        upsert,
        r#"{"title":"Dated","state":"waiting","due_at":100}"#,
    )
    .expect("upsert");
    assert_eq!(dated["due_at"], 100);
    assert!(dated["project_id"].is_null());
    assert!(dated["waiting_since"].as_i64().unwrap() > 0);
    call(upsert, r#"{"title":"Undated"}"#).expect("upsert");

    let all = call(filtered, "{}").expect("filtered");
    let titles: Vec<_> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, vec!["Dated", "Undated"], "due first, undated last");
    let overdue = call(filtered, r#"{"overdue":true,"unassigned":true}"#).unwrap();
    assert_eq!(overdue.as_array().unwrap().len(), 1);

    // Unknown filter keys and wrong lookup kinds are errors.
    assert!(call(filtered, r#"{"bogus":1}"#).is_none());
    assert!(call(items_for, r#"{"kind":"project","id":"x"}"#).is_none());
    let none = call(items_for, r#"{"kind":"thread","id":"no-such-thread"}"#).unwrap();
    assert!(none.as_array().unwrap().is_empty());

    // Null pointers are invalid arguments, not crashes.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_list_work_items_filtered(handle, std::ptr::null(), &mut out),
        OK
    );
    assert_ne!(
        castellum_work_items_for(handle, std::ptr::null(), &mut out),
        OK
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn suggestions_list_accept_dismiss_count_over_the_abi() {
    use castellum_ffi::{
        castellum_accept_suggestion, castellum_count_suggestions, castellum_dismiss_suggestion,
        castellum_list_suggestions,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // Producers are core-internal; seed two proposals straight into the
    // profile's own database file (the ABI has no "propose" call by design).
    {
        let db = castellum_db::Database::open(&dir.join("castellum.db")).unwrap();
        for (title, key) in [("Keep", "k1"), ("Drop", "k2")] {
            let payload = format!(r#"{{"title":"{title}"}}"#);
            db.propose_suggestion_at(
                &castellum_db::NewSuggestion {
                    kind: "work_item",
                    payload_json: &payload,
                    thread_id: Some("t1"),
                    origin: "heuristic",
                    dedupe_key: key,
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        }
    }

    let json_call = |f: &dyn Fn(*mut *mut std::ffi::c_char) -> i32| -> Option<serde_json::Value> {
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = f(&mut out);
        if rc == OK {
            Some(serde_json::from_str(&take_json(out)).unwrap())
        } else {
            assert!(out.is_null(), "no output on failure");
            None
        }
    };
    let list = |filter: &str| {
        let c = CString::new(filter).unwrap();
        json_call(&|o| castellum_list_suggestions(handle, c.as_ptr(), o))
    };
    let count = || json_call(&|o| castellum_count_suggestions(handle, o)).unwrap();

    assert_eq!(count()["proposed"], 2);
    let all = list(r#"{"status":"proposed","thread_id":"t1"}"#).unwrap();
    let rows = all.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // A NULL filter lists everything; an unknown key is an error.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_suggestions(handle, std::ptr::null(), &mut out),
        OK
    );
    let _ = take_json(out);
    assert!(list(r#"{"bogus":1}"#).is_none());

    let keep = rows
        .iter()
        .find(|r| r["payload"]["title"] == "Keep")
        .unwrap();
    let drop = rows
        .iter()
        .find(|r| r["payload"]["title"] == "Drop")
        .unwrap();
    let keep_id = CString::new(keep["id"].as_str().unwrap()).unwrap();
    let drop_id = CString::new(drop["id"].as_str().unwrap()).unwrap();

    let accepted =
        json_call(&|o| castellum_accept_suggestion(handle, keep_id.as_ptr(), o)).unwrap();
    assert_eq!(accepted["suggestion"]["status"], "accepted");
    assert_eq!(accepted["work_item"]["title"], "Keep");
    let dismissed =
        json_call(&|o| castellum_dismiss_suggestion(handle, drop_id.as_ptr(), o)).unwrap();
    assert_eq!(dismissed["status"], "dismissed");
    assert_eq!(count()["proposed"], 0);
    // Dismissed cannot be accepted; accepted cannot be dismissed.
    assert!(json_call(&|o| castellum_accept_suggestion(handle, drop_id.as_ptr(), o)).is_none());
    assert!(json_call(&|o| castellum_dismiss_suggestion(handle, keep_id.as_ptr(), o)).is_none());
    // Null ids are invalid arguments.
    assert_ne!(
        castellum_accept_suggestion(handle, std::ptr::null(), &mut out),
        OK
    );
    assert_ne!(
        castellum_dismiss_suggestion(handle, std::ptr::null(), &mut out),
        OK
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn saved_views_and_unassigned_mail_over_the_abi() {
    use castellum_ffi::{
        castellum_create_saved_view, castellum_delete_saved_view, castellum_list_saved_views,
        castellum_list_unassigned_mail, castellum_reorder_saved_views, castellum_saved_view_counts,
        castellum_saved_view_items, castellum_update_saved_view,
    };
    type Out = *mut *mut std::ffi::c_char;

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let call = |f: &dyn Fn(u64, *const std::ffi::c_char, Out) -> i32,
                json: &str|
     -> Option<serde_json::Value> {
        let c = CString::new(json).unwrap();
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = f(handle, c.as_ptr(), &mut out);
        if rc == OK {
            Some(serde_json::from_str(&take_json(out)).unwrap())
        } else {
            assert!(out.is_null(), "no output on failure");
            None
        }
    };

    // Built-ins are seeded on first use.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_saved_views(handle, 0, &mut out), OK);
    let views: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(views.as_array().unwrap().len(), 4);
    assert_eq!(views[0]["name"], "Today");

    // Create validates the filter; a bad one stores nothing.
    let create = |h, j, o| castellum_create_saved_view(h, j, o);
    assert!(call(&create, r#"{"name":"Bad","filter":{"nope":true}}"#).is_none());
    let v = call(
        &create,
        r#"{"name":"Chasers","filter":{"waiting_on_others":true}}"#,
    )
    .expect("create");
    let id = v["id"].as_str().unwrap().to_string();

    let update = |h, j, o| castellum_update_saved_view(h, j, o);
    let renamed = call(&update, &format!(r#"{{"id":"{id}","name":"Chase"}}"#)).unwrap();
    assert_eq!(renamed["name"], "Chase");
    assert!(call(&update, r#"{"id":"builtin:today","name":"Now"}"#).is_none());

    let reorder = |h, j, o| castellum_reorder_saved_views(h, j, o);
    let order = call(&reorder, &format!(r#"["{id}"]"#)).unwrap();
    assert_eq!(order[0]["id"], id.as_str());

    // One call, all visible views; the new view has no items yet.
    assert_eq!(castellum_saved_view_counts(handle, &mut out), OK);
    let counts: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(counts.as_array().unwrap().len(), 5);
    assert_eq!(counts[0]["count"], 0);

    let items = |h, j, o| castellum_saved_view_items(h, j, o);
    assert!(call(&items, &id).unwrap().as_array().unwrap().is_empty());
    assert!(call(&items, "no-such-view").is_none());

    // Unassigned mail: NULL request = defaults, junk = error.
    assert_eq!(
        castellum_list_unassigned_mail(handle, std::ptr::null(), &mut out),
        OK
    );
    let page: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(
        (page["total"].as_i64(), page["limit"].as_i64()),
        (Some(0), Some(50))
    );
    let unassigned = |h, j, o| castellum_list_unassigned_mail(h, j, o);
    assert!(call(&unassigned, r#"{"bogus":1}"#).is_none());

    // Built-ins cannot be deleted; custom ones can. Null pointers are errors.
    let c_id = CString::new(id.clone()).unwrap();
    let c_builtin = CString::new("builtin:today").unwrap();
    assert_ne!(castellum_delete_saved_view(handle, c_builtin.as_ptr()), OK);
    assert_eq!(castellum_delete_saved_view(handle, c_id.as_ptr()), OK);
    assert_ne!(castellum_delete_saved_view(handle, std::ptr::null()), OK);
    assert_ne!(
        castellum_create_saved_view(handle, std::ptr::null(), &mut out),
        OK
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attachment_cache_status_reports_not_cached_for_unknown_message() {
    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let message_id = CString::new("acc:does-not-exist").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    let rc =
        castellum_ffi::castellum_attachment_cache_status(handle, message_id.as_ptr(), 0, &mut out);
    assert_eq!(rc, OK);
    let json = unsafe { CStr::from_ptr(out) }.to_str().unwrap().to_owned();
    assert!(json.contains("not_cached"));
    castellum_string_free(out);

    castellum_close_profile(handle);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seeds account/mailbox/message rows directly into the profile's SQLite
/// file (there is no FFI entry point to create a message from scratch
/// outside a real IMAP sync) — opens its own throwaway `castellum_db::
/// Database` handle on the SAME `<dir>/castellum.db` file the FFI-opened
/// profile at `dir` already created (via `castellum_open_profile`, called
/// first by every caller below), seeds, then drops it before the FFI calls
/// resume, so the two connections never overlap.
fn seed_via_db(dir: &std::path::Path, write_enabled: bool) {
    use castellum_db::Database;
    use castellum_models::{Account, Address, Mailbox, NewMessage};

    let db = Database::open(&dir.join("castellum.db")).unwrap();
    db.upsert_account(&Account {
        id: "a1".into(),
        email: "me@x.test".into(),
        display_name: None,
        provider: "imap".into(),
    })
    .unwrap();
    db.upsert_mailbox(&Mailbox {
        id: "a1:INBOX".into(),
        account_id: "a1".into(),
        path: "INBOX".into(),
        name: "INBOX".into(),
        role: Some("inbox".into()),
        delimiter: Some("/".into()),
        subscribed: true,
        selectable: true,
        sync_by_default: true,
    })
    .unwrap();
    if write_enabled {
        db.set_account_write_enabled("a1", true).unwrap();
    }
    db.upsert_message(&NewMessage {
        id: "a1:INBOX:1".into(),
        account_id: "a1".into(),
        mailbox_id: "a1:INBOX".into(),
        provider_uid: Some(1),
        rfc822_message_id: Some("<1@x.test>".into()),
        subject: Some("your invoice is ready".into()),
        from_name: None,
        from_address: Some("billing@x.test".into()),
        sent_at: Some(1_800_000_000),
        snippet: None,
        size_bytes: None,
        has_attachments: false,
        attachments: None,
        is_read: false,
        is_starred: false,
        addresses: vec![Address {
            kind: "from".into(),
            name: None,
            address: "billing@x.test".into(),
        }],
        text_body: None,
        html_body: None,
        sanitized_html: None,
        has_remote_content: false,
        references: vec![],
    })
    .unwrap();
    drop(db);
}

const INVOICE_GROUP_JSON: &str = r#"{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]}"#;

#[test]
fn smart_mailbox_kind_filter_round_trips_over_the_abi() {
    use castellum_ffi::{
        castellum_delete_filter, castellum_get_filter, castellum_list_filters,
        castellum_upsert_filter,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, false);

    let req = CString::new(format!(
        r#"{{"account_id":"a1","name":"Invoices","kind":"smart_mailbox","group":{INVOICE_GROUP_JSON}}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_upsert_filter(handle, req.as_ptr(), &mut out), OK);
    let created: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(created["kind"], "smart_mailbox");
    let id = created["id"].as_str().unwrap().to_string();
    let c_id = CString::new(id.clone()).unwrap();

    // List — scoped to the account.
    let c_acc = CString::new("a1").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_filters(handle, c_acc.as_ptr(), &mut out), OK);
    let listed = take_json(out);
    assert!(listed.contains(&id));

    // Get.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_get_filter(handle, c_id.as_ptr(), &mut out), OK);
    assert!(take_json(out).contains(&id));

    // Delete, then delete again — idempotent-delete-is-an-error.
    assert_eq!(castellum_delete_filter(handle, c_id.as_ptr()), OK);
    assert_ne!(castellum_delete_filter(handle, c_id.as_ptr()), OK);

    // Get after delete -> JSON null, never an error.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_get_filter(handle, c_id.as_ptr(), &mut out), OK);
    assert_eq!(take_json(out), "null");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MM-1: a smart mailbox holding a nested all/any/none group
/// plus scope, through the C ABI: create, list (group survives the JSON),
/// read its messages and counts, edit it, and refuse an empty group.
#[test]
fn group_smart_mailbox_round_trips_over_the_abi() {
    use castellum_ffi::{
        castellum_delete_smart_mailbox, castellum_list_smart_mailboxes,
        castellum_messages_for_smart_mailbox, castellum_upsert_smart_mailbox,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, false); // one unread message: "your invoice is ready"

    // all( unread, none( subject contains "invoice" ) ) -> matches nothing
    let none_json = r#"{"op":"and","conditions":[{"field":"is_read","operator":"is","value":false}],"groups":[{"op":"none","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]}]}"#;
    let req = CString::new(format!(
        r#"{{"name":"Unread no invoices","group":{none_json},"scope":{{"account_ids":["a1"],"mailbox_ids":[]}}}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_smart_mailbox(handle, req.as_ptr(), &mut out),
        OK
    );
    let created: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["query"], "");
    assert_eq!(created["group"]["groups"][0]["op"], "none");
    assert_eq!(created["scope"]["account_ids"][0], "a1");
    assert_eq!(created["cached_total"], 0);

    // Edit to a plain any-group that DOES match the invoice message.
    let any_json = r#"{"op":"or","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]}"#;
    let edit = CString::new(format!(
        r#"{{"id":"{id}","name":"Invoices","group":{any_json}}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_smart_mailbox(handle, edit.as_ptr(), &mut out),
        OK
    );
    let edited: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(edited["cached_total"], 1);
    assert_eq!(edited["cached_unread"], 1);

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_smart_mailboxes(handle, &mut out), OK);
    let listed: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(listed[0]["group"]["op"], "or");

    let c_id = CString::new(id.clone()).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_messages_for_smart_mailbox(handle, c_id.as_ptr(), 50, &mut out),
        OK
    );
    let msgs = take_json(out);
    assert!(msgs.contains("your invoice is ready"), "{msgs}");

    // An empty group is refused, and leaves the stored mailbox untouched.
    let empty = CString::new(format!(
        r#"{{"id":"{id}","name":"Invoices","group":{{"op":"and","conditions":[],"groups":[]}}}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_upsert_smart_mailbox(handle, empty.as_ptr(), &mut out),
        OK
    );

    assert_eq!(castellum_delete_smart_mailbox(handle, c_id.as_ptr()), OK);
    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// HP-2: an Inbox split over the C ABI — a group smart mailbox flagged
/// `inbox_split`, its tab counts (then Other, last) and each tab's list.
#[test]
fn inbox_split_counts_and_lists_round_trip_over_the_abi() {
    use castellum_ffi::{
        castellum_inbox_split_counts, castellum_inbox_split_messages,
        castellum_upsert_smart_mailbox,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, false); // one unread Inbox message: "your invoice is ready"

    let group = r#"{"op":"and","conditions":[{"field":"subject","operator":"contains","value":"invoice"}],"groups":[]}"#;
    let req = CString::new(format!(
        r#"{{"name":"Invoices","group":{group},"scope":{{"inbox_split":true}}}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_smart_mailbox(handle, req.as_ptr(), &mut out),
        OK
    );
    let created: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["scope"]["inbox_split"], true);

    let counts_req = CString::new(r#"{"mailbox_ids":["a1:INBOX"]}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_inbox_split_counts(handle, counts_req.as_ptr(), &mut out),
        OK
    );
    let counts: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(counts[0]["id"], id.as_str());
    assert_eq!(
        (counts[0]["total"].as_i64(), counts[0]["unread"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(counts[1]["id"], "other");
    assert_eq!(counts[1]["total"], 0);

    let list_req = CString::new(format!(r#"{{"split_id":"{id}"}}"#)).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_inbox_split_messages(handle, list_req.as_ptr(), &mut out),
        OK
    );
    assert!(take_json(out).contains("your invoice is ready"));

    let gone = CString::new(r#"{"split_id":"gone"}"#).unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_inbox_split_messages(handle, gone.as_ptr(), &mut out),
        OK
    );
    assert_eq!(take_json(out), "null");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn body_hydration_progress_round_trips_over_the_abi() {
    use castellum_ffi::castellum_body_hydration_progress;

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, false); // one descriptor-only Inbox message: not hydrated

    let acct = CString::new("a1").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_body_hydration_progress(handle, acct.as_ptr(), &mut out),
        OK
    );
    let rows: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["mailbox_id"], "a1:INBOX");
    assert_eq!(rows[0]["total"], 1);
    assert_eq!(rows[0]["hydrated"], 0);

    // Unknown account: empty array, not an error. Null id: invalid argument.
    let nobody = CString::new("nobody").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_body_hydration_progress(handle, nobody.as_ptr(), &mut out),
        OK
    );
    assert_eq!(take_json(out), "[]");
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_ne!(
        castellum_body_hydration_progress(handle, std::ptr::null(), &mut out),
        OK
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn body_hydration_scope_round_trips_over_the_abi() {
    use castellum_ffi::castellum_body_hydration_scope;

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, false); // one descriptor-only Inbox message

    let scope_q = |acct: Option<&str>, mb: Option<&str>, q: Option<&str>| -> serde_json::Value {
        let a = acct.map(|s| CString::new(s).unwrap());
        let m = mb.map(|s| CString::new(s).unwrap());
        let qq = q.map(|s| CString::new(s).unwrap());
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        assert_eq!(
            castellum_body_hydration_scope(
                handle,
                a.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                m.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                qq.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                &mut out
            ),
            OK
        );
        serde_json::from_str(&take_json(out)).unwrap()
    };
    let scope = |acct: Option<&str>, mb: Option<&str>| scope_q(acct, mb, None);
    for v in [
        scope(None, None),
        scope(Some("a1"), None),
        scope(Some("a1"), Some("a1:INBOX")),
    ] {
        assert_eq!(
            (v["total"].as_i64(), v["missing"].as_i64()),
            (Some(1), Some(1))
        );
    }
    // The searched query's mailbox filters narrow the count: the only message
    // is in the inbox, so `-in:inbox` excludes it and `in:inbox` keeps it.
    let excl = scope_q(Some("a1"), None, Some("invoice -in:inbox"));
    assert_eq!(
        (excl["total"].as_i64(), excl["missing"].as_i64()),
        (Some(0), Some(0))
    );
    let incl = scope_q(None, None, Some("invoice in:inbox"));
    assert_eq!(
        (incl["total"].as_i64(), incl["missing"].as_i64()),
        (Some(1), Some(1))
    );

    // A non-NULL pointer that is not UTF-8 is refused, not treated as "all".
    let bad: [u8; 2] = [0xff, 0];
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    for (a, m, q) in [
        (bad.as_ptr(), std::ptr::null(), std::ptr::null()),
        (std::ptr::null(), bad.as_ptr(), std::ptr::null()),
        (std::ptr::null(), std::ptr::null(), bad.as_ptr()),
    ] {
        let rc = castellum_body_hydration_scope(
            handle,
            a as *const std::ffi::c_char,
            m as *const std::ffi::c_char,
            q as *const std::ffi::c_char,
            &mut out,
        );
        assert_ne!(rc, OK);
        assert!(out.is_null());
    }

    let none = scope(Some("a1"), Some("a1:nope"));
    assert_eq!(
        (none["total"].as_i64(), none["missing"].as_i64()),
        (Some(0), Some(0))
    );

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rule_kind_filter_round_trips_and_dry_run_matches_preview_and_run_now_batches() {
    use castellum_ffi::{
        castellum_evaluate_filter, castellum_preview_rule, castellum_preview_rule_conditions,
        castellum_run_rule_now, castellum_run_rule_now_batch, castellum_upsert_filter,
    };

    let dir = unique_tmp();
    std::fs::create_dir_all(&dir).unwrap();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);
    seed_via_db(&dir, true);

    let req = CString::new(format!(
        r#"{{"account_id":"a1","name":"Invoice rule","kind":"rule","group":{INVOICE_GROUP_JSON},"trigger":"on_sync","actions":["mark_read"]}}"#
    ))
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_upsert_filter(handle, req.as_ptr(), &mut out), OK);
    let created: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let c_id = CString::new(id.clone()).unwrap();
    let c_acc = CString::new("a1").unwrap();
    let c_group = CString::new(INVOICE_GROUP_JSON).unwrap();

    // Dry-run/preview: evaluate_filter's ad-hoc match count must equal
    // preview_rule's matched_count exactly — same underlying evaluate_filter
    // call (memo's own stated invariant, Task 6's round-trip requirement).
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_evaluate_filter(handle, c_group.as_ptr(), c_acc.as_ptr(), 50, 0, &mut out),
        OK
    );
    let matched: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    let dry_run_count = matched.as_array().unwrap().len();
    assert_eq!(dry_run_count, 1);

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_preview_rule(handle, c_id.as_ptr(), c_acc.as_ptr(), 50, &mut out),
        OK
    );
    let preview: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(
        preview["matches"].as_array().unwrap().len(),
        dry_run_count,
        "preview_rule's matched_count must equal evaluate_filter's dry-run count"
    );
    assert_eq!(preview["actions"], serde_json::json!(["mark_read"]));

    assert_eq!(preview["total"], 1, "exact candidate count rides along");

    // The editor's dry run of unsaved conditions: same candidates, same total.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_preview_rule_conditions(handle, c_group.as_ptr(), c_acc.as_ptr(), 50, &mut out),
        OK
    );
    let conditions: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(conditions["total"], 1);
    assert_eq!(conditions["matches"].as_array().unwrap().len(), 1);

    // castellum_run_rule_now_batch: one bounded batch from the start; the
    // single candidate fits, so there is no next batch.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_run_rule_now_batch(
            handle,
            c_id.as_ptr(),
            c_acc.as_ptr(),
            std::ptr::null(),
            200,
            &mut out
        ),
        OK
    );
    let batch: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(batch["filter_id"], id);
    assert_eq!(batch["matched_count"], 1);
    assert_eq!(batch["actions_enqueued"], 1);
    assert!(batch.get("next_after").is_none(), "{batch}");

    // castellum_run_rule_now: the "Run now" path on the same rule by id —
    // its own report's matched_count/actions_enqueued. The batch above
    // already marked the match read, so it still matches but its mark_read
    // is already in place and queues nothing (FP20).
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_run_rule_now(handle, c_id.as_ptr(), c_acc.as_ptr(), &mut out),
        OK
    );
    let report: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(report["matched_count"], 1);
    assert_eq!(report["actions_enqueued"], 0);

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- last-sync KPI snapshot -----------

#[test]
fn last_sync_kpi_is_null_before_any_sync_then_reports_a_written_snapshot() {
    use castellum_ffi::castellum_last_sync_kpi;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    // No sync has run yet: the FFI call succeeds and reports JSON null, not
    // an error status -- "nothing recorded yet" is a normal, expected state
    // for a freshly opened profile.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_last_sync_kpi(handle, &mut out), OK);
    assert_eq!(take_json(out), "null");

    // The C ABI has no "run a mock sync" entry point (a real sync needs a
    // live IMAP provider) -- so, exactly like the account/message seeding
    // above for S40/S53-UI, simulate "a sync just completed" by writing the
    // KPI snapshot straight through the core crate at the same profile path
    // `castellum_open_profile` created, then read it back over the ABI.
    let mut timings = castellum_core::sync_timing::SyncTimings::default();
    timings
        .samples
        .insert("db.upsert_descriptor", vec![120, 130, 140]);
    castellum_core::sync_kpi::write_last_sync_kpi(
        &dir,
        "acc-1",
        3,
        std::time::Duration::from_millis(42),
        &timings,
    )
    .expect("write snapshot directly at the profile path");

    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_last_sync_kpi(handle, &mut out2), OK);
    let snapshot: serde_json::Value = serde_json::from_str(&take_json(out2)).unwrap();
    assert_eq!(snapshot["available"], true);
    assert_eq!(snapshot["account_id"], "acc-1");
    assert_eq!(snapshot["message_count"], 3);
    assert_eq!(snapshot["phases"]["db.upsert_descriptor"]["count"], 3);

    // Unknown/invalid handle: a clean status, not a crash.
    let mut out3: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_last_sync_kpi(999_999, &mut out3), INVALID_HANDLE);
    assert!(out3.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// S42: tags round-trip over the C ABI, applied in evaluation mode (new
/// accounts default to read-only), and mute is visible on the message
/// header. Same seeding pattern as
/// `find_similar_messages_over_the_c_abi_with_a_stored_anchor_embedding`.
#[test]
fn tags_and_mute_over_the_c_abi() {
    use castellum_ffi::{
        castellum_list_messages, castellum_list_tags, castellum_messages_for_tag,
        castellum_set_message_tag, castellum_set_thread_muted, castellum_tag_counts,
        castellum_upsert_tag,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let msg_id = "acc1:INBOX:1";
    {
        use castellum_db::Database;
        use castellum_models::{Account, Mailbox, NewMessage};
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "alice@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&NewMessage {
            id: msg_id.into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@test>".into()),
            subject: Some("hello".into()),
            from_name: Some("Bob".into()),
            from_address: Some("bob@example.test".into()),
            sent_at: Some(1_700_000_000),
            snippet: Some("hi".into()),
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("hello".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
        // Mute is looked up by joining the `threads` row `message_headers`
        // carries — it only exists after a thread rebuild (see
        // `Database::rebuild_threads`; `repo.rs`'s
        // `header_thread_muted_reflects_the_thread` test does the same).
        db.rebuild_threads("acc1").unwrap();
    }

    // Create a tag definition and confirm it round-trips through the list.
    let tag_json = CString::new(
        r#"{"keyword":"","keyword_key":"","display_name":"Work","colour":"blue","sort_order":0}"#,
    )
    .unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_tag(handle, tag_json.as_ptr(), &mut out),
        OK
    );
    let created: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(created["keyword"], "Work");

    let mut list_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_tags(handle, &mut list_out), OK);
    let tags: serde_json::Value = serde_json::from_str(&take_json(list_out)).unwrap();
    assert_eq!(tags.as_array().unwrap().len(), 1);

    // Apply it to the seeded message. New accounts default to evaluation
    // (read-only) mode, so this stays local-only.
    let msg_id_c = CString::new(msg_id).unwrap();
    let keyword_c = CString::new("Work").unwrap();
    let mut apply_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_set_message_tag(
            handle,
            msg_id_c.as_ptr(),
            keyword_c.as_ptr(),
            1,
            &mut apply_out
        ),
        OK
    );
    let outcome: serde_json::Value = serde_json::from_str(&take_json(apply_out)).unwrap();
    assert_eq!(outcome["synced"], false);
    assert_eq!(outcome["reason"], "evaluation_mode");

    // The keyword shows up on the message header.
    let acct_c = CString::new("acc1").unwrap();
    let mailbox_c = CString::new("acc1:INBOX").unwrap();
    let mut list_msgs_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages(
            handle,
            acct_c.as_ptr(),
            mailbox_c.as_ptr(),
            50,
            0,
            &mut list_msgs_out
        ),
        OK
    );
    let headers: serde_json::Value = serde_json::from_str(&take_json(list_msgs_out)).unwrap();
    let headers = headers.as_array().unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0]["keywords"], serde_json::json!(["Work"]));
    assert_eq!(headers[0]["thread_muted"], false);

    // Mute and see it reflected on the header.
    assert_eq!(castellum_set_thread_muted(handle, msg_id_c.as_ptr(), 1), OK);
    let mut list_msgs_out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages(
            handle,
            acct_c.as_ptr(),
            mailbox_c.as_ptr(),
            50,
            0,
            &mut list_msgs_out2
        ),
        OK
    );
    let headers2: serde_json::Value = serde_json::from_str(&take_json(list_msgs_out2)).unwrap();
    assert_eq!(headers2.as_array().unwrap()[0]["thread_muted"], true);

    // MM-8: counts (with the new local_only field) and the tag's own list.
    let mut counts_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_tag_counts(handle, &mut counts_out), OK);
    let counts: serde_json::Value = serde_json::from_str(&take_json(counts_out)).unwrap();
    assert_eq!(counts[0]["keyword_key"], "work");
    assert_eq!(counts[0]["total"], 1);
    assert_eq!(counts[0]["local_only"], 1);
    let mut tag_msgs_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_messages_for_tag(
            handle,
            keyword_c.as_ptr(),
            50,
            0,
            std::ptr::null(),
            &mut tag_msgs_out
        ),
        OK
    );
    let tagged: serde_json::Value = serde_json::from_str(&take_json(tag_msgs_out)).unwrap();
    assert_eq!(tagged.as_array().unwrap().len(), 1);
    assert_eq!(tagged[0]["keywords"], serde_json::json!(["Work"]));
    let unknown_c = CString::new("nosuchtag").unwrap();
    let mut none_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_messages_for_tag(
            handle,
            unknown_c.as_ptr(),
            50,
            0,
            std::ptr::null(),
            &mut none_out
        ),
        OK
    );
    assert_eq!(take_json(none_out), "[]");

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tags_and_mute_invalid_args_and_handles() {
    use castellum_ffi::{
        castellum_delete_tag, castellum_list_keywords_in_use, castellum_list_tags,
        castellum_messages_for_tag, castellum_set_message_tag, castellum_set_thread_muted,
        castellum_tag_counts, castellum_upsert_tag,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_upsert_tag(handle, std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_delete_tag(handle, std::ptr::null()),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_set_message_tag(handle, std::ptr::null(), std::ptr::null(), 1, &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_set_thread_muted(handle, std::ptr::null(), 1),
        INVALID_ARGUMENT
    );

    // Unknown/invalid handle: a clean status on the read calls, not a crash.
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_tags(999_999, &mut out2), INVALID_HANDLE);
    assert!(out2.is_null());
    assert_eq!(
        castellum_list_keywords_in_use(999_999, &mut out2),
        INVALID_HANDLE
    );
    assert!(out2.is_null());
    assert_eq!(castellum_tag_counts(999_999, &mut out2), INVALID_HANDLE);
    assert!(out2.is_null());
    assert_eq!(
        castellum_messages_for_tag(handle, std::ptr::null(), 10, 0, std::ptr::null(), &mut out2),
        INVALID_ARGUMENT
    );
    let kw = CString::new("Work").unwrap();
    assert_eq!(
        castellum_messages_for_tag(999_999, kw.as_ptr(), 10, 0, std::ptr::null(), &mut out2),
        INVALID_HANDLE
    );
    assert!(out2.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Keyset message-list paging over the C ABI (S64): a null cursor gets the
/// first page with a `next`, feeding that `next` back gets the second page,
/// and malformed `cursor_json` is a clean `InvalidArgument`, never a panic.
#[test]
fn list_messages_page_over_the_c_abi() {
    use castellum_ffi::castellum_list_messages_page;

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    {
        use castellum_db::Database;
        use castellum_models::{Account, Mailbox, NewMessage};
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "alice@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        for uid in 1..=5i64 {
            db.upsert_message(&NewMessage {
                id: format!("acc1:INBOX:{uid}"),
                account_id: "acc1".into(),
                mailbox_id: "acc1:INBOX".into(),
                provider_uid: Some(uid),
                rfc822_message_id: Some(format!("<{uid}@test>")),
                subject: Some(format!("msg {uid}")),
                from_name: None,
                from_address: Some("bob@example.test".into()),
                sent_at: Some(1_700_000_000 + uid),
                snippet: None,
                size_bytes: None,
                has_attachments: false,
                attachments: None,
                is_read: false,
                is_starred: false,
                addresses: vec![],
                text_body: None,
                sanitized_html: None,
                html_body: None,
                has_remote_content: false,
                references: vec![],
            })
            .unwrap();
        }
    }

    let account_id = CString::new("acc1").unwrap();
    let mailbox_id = CString::new("acc1:INBOX").unwrap();

    // First page, no cursor.
    let mut out1: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages_page(
            handle,
            account_id.as_ptr(),
            mailbox_id.as_ptr(),
            std::ptr::null(),
            3,
            &mut out1,
        ),
        OK
    );
    let page1: serde_json::Value = serde_json::from_str(&take_json(out1)).unwrap();
    assert_eq!(page1["rows"].as_array().unwrap().len(), 3);
    assert!(!page1["next"].is_null());

    // Feed `next` back for the second page.
    let cursor_json = serde_json::to_string(&page1["next"]).unwrap();
    let c_cursor = CString::new(cursor_json).unwrap();
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages_page(
            handle,
            account_id.as_ptr(),
            mailbox_id.as_ptr(),
            c_cursor.as_ptr(),
            3,
            &mut out2,
        ),
        OK
    );
    let page2: serde_json::Value = serde_json::from_str(&take_json(out2)).unwrap();
    assert_eq!(page2["rows"].as_array().unwrap().len(), 2);
    assert!(
        page2["next"].is_null(),
        "only 5 messages total: page 2 is the end"
    );

    let ids1: Vec<&str> = page1["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    let ids2: Vec<&str> = page2["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    let all: std::collections::HashSet<&str> = ids1.iter().chain(ids2.iter()).copied().collect();
    assert_eq!(
        all.len(),
        5,
        "pages concatenate to every message, no duplicates"
    );

    // Malformed cursor_json: a clean InvalidArgument, not a panic.
    let bad_cursor = CString::new("{not json").unwrap();
    let mut out3: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages_page(
            handle,
            account_id.as_ptr(),
            mailbox_id.as_ptr(),
            bad_cursor.as_ptr(),
            3,
            &mut out3,
        ),
        INVALID_ARGUMENT
    );
    assert!(out3.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// VIP senders and the notification outbox over the C ABI (S59): set a VIP,
/// list it, see `from_vip` on the message header, drain an empty outbox,
/// count VIP unread, and reject null pointers / bad JSON cleanly.
#[test]
fn vips_and_notification_outbox_over_the_c_abi() {
    use castellum_ffi::{
        castellum_list_messages, castellum_list_vips, castellum_set_vip,
        castellum_take_pending_notifications, castellum_vip_unread_count,
    };

    let dir = unique_tmp();
    let c_path = CString::new(dir.to_str().unwrap()).unwrap();
    let mut handle: u64 = 0;
    assert_eq!(castellum_open_profile(c_path.as_ptr(), &mut handle), OK);

    {
        use castellum_db::Database;
        use castellum_models::{Account, Mailbox, NewMessage};
        let db = Database::open(&dir.join("castellum.db")).expect("open same profile db");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "alice@example.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        db.upsert_message(&NewMessage {
            id: "acc1:INBOX:1".into(),
            account_id: "acc1".into(),
            mailbox_id: "acc1:INBOX".into(),
            provider_uid: Some(1),
            rfc822_message_id: Some("<1@test>".into()),
            subject: Some("hello".into()),
            from_name: Some("Bob".into()),
            from_address: Some("Bob@Example.test".into()),
            sent_at: Some(1_700_000_000),
            snippet: Some("hi".into()),
            size_bytes: None,
            has_attachments: false,
            attachments: None,
            is_read: false,
            is_starred: false,
            addresses: vec![],
            text_body: Some("hello".into()),
            sanitized_html: None,
            html_body: None,
            has_remote_content: false,
            references: vec![],
        })
        .unwrap();
    }

    // Set a VIP (display name NULL is allowed) and list it.
    let addr = CString::new(" bob@example.test ").unwrap();
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_set_vip(handle, addr.as_ptr(), std::ptr::null(), 1, &mut out),
        OK
    );
    let changed: serde_json::Value = serde_json::from_str(&take_json(out)).unwrap();
    assert_eq!(changed["changed"], true);
    let mut list_out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_vips(handle, &mut list_out), OK);
    let vips: serde_json::Value = serde_json::from_str(&take_json(list_out)).unwrap();
    assert_eq!(vips[0]["address"], "bob@example.test");

    // The header carries from_vip.
    let acct_c = CString::new("acc1").unwrap();
    let mailbox_c = CString::new("acc1:INBOX").unwrap();
    let mut msgs: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_list_messages(
            handle,
            acct_c.as_ptr(),
            mailbox_c.as_ptr(),
            50,
            0,
            &mut msgs
        ),
        OK
    );
    let headers: serde_json::Value = serde_json::from_str(&take_json(msgs)).unwrap();
    assert_eq!(headers[0]["from_vip"], true);

    // Empty outbox drains to [].
    let mut taken: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_take_pending_notifications(handle, 50, &mut taken),
        OK
    );
    let notices: serde_json::Value = serde_json::from_str(&take_json(taken)).unwrap();
    assert_eq!(notices, serde_json::json!([]));

    // VIP unread count: all accounts (NULL), a matching filter, a non-matching one.
    let mut n: i64 = -1;
    assert_eq!(
        castellum_vip_unread_count(handle, std::ptr::null(), &mut n),
        OK
    );
    assert_eq!(n, 1);
    let one = CString::new(r#"["acc1"]"#).unwrap();
    assert_eq!(castellum_vip_unread_count(handle, one.as_ptr(), &mut n), OK);
    assert_eq!(n, 1);
    let other = CString::new(r#"["nope"]"#).unwrap();
    assert_eq!(
        castellum_vip_unread_count(handle, other.as_ptr(), &mut n),
        OK
    );
    assert_eq!(n, 0);
    let bad = CString::new("not json").unwrap();
    assert_eq!(
        castellum_vip_unread_count(handle, bad.as_ptr(), &mut n),
        INVALID_ARGUMENT
    );

    // Null pointers and bad addresses.
    assert_eq!(
        castellum_set_vip(handle, std::ptr::null(), std::ptr::null(), 1, &mut out),
        INVALID_ARGUMENT
    );
    assert_eq!(
        castellum_vip_unread_count(handle, std::ptr::null(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
    let no_at = CString::new("nobody").unwrap();
    assert_ne!(
        castellum_set_vip(handle, no_at.as_ptr(), std::ptr::null(), 1, &mut out),
        OK
    );
    let mut out2: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(castellum_list_vips(999_999, &mut out2), INVALID_HANDLE);
    assert!(out2.is_null());

    assert_eq!(castellum_close_profile(handle), OK);
    let _ = std::fs::remove_dir_all(&dir);
}

/// MM-14: `castellum_autoconfig_lookup` is handle-less, takes only a domain,
/// and reports "nothing found" inside the JSON. No network is touched: the
/// valid request disables every source, so the core performs no I/O at all.
#[test]
fn autoconfig_lookup_ffi_contract() {
    use castellum_ffi::castellum_autoconfig_lookup;
    let call = |req: &str| -> (i32, Option<String>) {
        let c = CString::new(req).unwrap();
        let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
        let rc = castellum_autoconfig_lookup(c.as_ptr(), &mut out);
        (
            rc,
            if out.is_null() {
                None
            } else {
                Some(take_json(out))
            },
        )
    };

    // Valid request, every source disabled -> OK + not_found, nothing attempted.
    let (rc, json) = call(r#"{"domain":"example.org","sources":[]}"#);
    assert_eq!(rc, OK);
    let v: serde_json::Value = serde_json::from_str(&json.unwrap()).unwrap();
    assert_eq!(v["status"], "not_found");
    assert_eq!(v["tried"].as_array().unwrap().len(), 0);

    // Malformed requests are a non-OK status with no output; a local part (as
    // an address, or as an extra field) is refused, never accepted or echoed.
    for bad in [
        "not json",
        "{}",
        r#"{"domain":"me@example.org"}"#,
        r#"{"domain":"example.org","email":"me@example.org","sources":[]}"#,
        r#"{"domain":"example.org","sources":["nope"]}"#,
        r#"{"domain":""}"#,
    ] {
        let (rc, json) = call(bad);
        assert_ne!(rc, OK, "{bad}");
        assert!(json.is_none(), "{bad}");
    }

    // Null pointers are an InvalidArgument, not a crash.
    let mut out: *mut std::ffi::c_char = std::ptr::null_mut();
    assert_eq!(
        castellum_autoconfig_lookup(std::ptr::null(), &mut out),
        INVALID_ARGUMENT
    );
    let c = CString::new(r#"{"domain":"example.org","sources":[]}"#).unwrap();
    assert_eq!(
        castellum_autoconfig_lookup(c.as_ptr(), std::ptr::null_mut()),
        INVALID_ARGUMENT
    );
}
