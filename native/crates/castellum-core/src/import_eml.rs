// Castellum - import a local .eml (RFC822) file into a mailbox (S49).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Reads the file itself (Swift hands over a path, not bytes — the same
// convention every other FFI boundary in this crate uses for local files),
// parses it with the REFUSING parser (castellum_mail::parse_message_checked
// — see Task 1), and queues it for IMAP APPEND via the existing
// operations/operation_blobs machinery (Database::enqueue_eml_import, Task
// 2) — the same durable path `sent_copy` already uses. Never opens a
// network connection itself; the import shows up once the queued op
// replays (replay.rs's "import_eml" arm, Task 5) and the account's next
// sync discovers the new UID.

use crate::CoreError;
use castellum_db::Database;
use castellum_mail::{parse_message_checked, ParsedMessage};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Largest `.eml` file this will import in one call — a generous bound for
/// a single RFC822 message (base64 attachments included) while refusing a
/// hostile/corrupt file before it is fully read into memory. Checked via
/// `fs::metadata` BEFORE reading, so an oversized file is never loaded.
const MAX_IMPORT_BYTES: u64 = 100 * 1024 * 1024;

static IMPORT_MSGID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportEmlResult {
    /// "queued" | "failed".
    pub status: String,
    pub error: Option<String>,
}

fn failed(reason: &str) -> ImportEmlResult {
    ImportEmlResult {
        status: "failed".into(),
        error: Some(reason.into()),
    }
}

fn to_json(result: &ImportEmlResult) -> Result<String, CoreError> {
    serde_json::to_string(result).map_err(|e| CoreError::Sync(format!("serialize: {e}")))
}

/// Read, parse and queue `file_path` for import into `mailbox_id` on
/// `account_id`. Every expected refusal (unreadable file, oversized file,
/// unparseable content, mailbox/account mismatch) is a `{"status":"failed"}`
/// result, never an `Err` — mirrors `download_attachment_json`'s convention.
pub fn import_eml_json(
    db: &Database,
    account_id: &str,
    mailbox_id: &str,
    file_path: &str,
) -> Result<String, CoreError> {
    let path = Path::new(file_path);
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return to_json(&failed(&format!("can't read {file_path}: {e}"))),
    };
    if size > MAX_IMPORT_BYTES {
        return to_json(&failed(&format!(
            "message is {size} bytes, over the {MAX_IMPORT_BYTES}-byte import limit"
        )));
    }
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return to_json(&failed(&format!("can't read {file_path}: {e}"))),
    };
    let Some(parsed) = parse_message_checked(&raw) else {
        return to_json(&failed(
            "doesn't look like a valid email message (.eml) file",
        ));
    };
    // Deviation from the plan (S49): mail-parser 0.11.8's parse() only
    // returns None for a byte slice with NO header shape at all (see
    // castellum-mail's parse_message_checked_treats_pure_garbage_as_an_
    // empty_parsed_message test) — pure garbage with no ':' or '\n' still
    // parses to Some(ParsedMessage::default()) via a synthetic
    // HeaderName::Other fallback. A hostile/corrupt file that mail-parser
    // could not extract ANY recognised field from is refused here too,
    // rather than silently importing an empty message.
    if parsed == ParsedMessage::default() {
        return to_json(&failed(
            "doesn't look like a valid email message (.eml) file",
        ));
    }

    let (message_id_header, raw) = match parsed.rfc822_message_id {
        Some(id) => (wrap_message_id(&id), raw),
        None => {
            let synth = synthesize_message_id(account_id);
            let injected = inject_message_id_header(&raw, &synth);
            (synth, injected)
        }
    };

    match db.enqueue_eml_import(account_id, mailbox_id, &message_id_header, &raw) {
        Ok(_op_id) => to_json(&ImportEmlResult {
            status: "queued".into(),
            error: None,
        }),
        Err(e) => to_json(&failed(&format!("db: {e}"))),
    }
}

/// `mail-parser` strips angle brackets from id headers (proven by
/// `castellum_mail::tests::parses_simple_text_email`). IMAP's `HEADER
/// Message-Id` search and `MailProvider::append`'s duplicate guard compare
/// against the WIRE header value, which IS bracketed — wrap once here
/// rather than at every caller.
fn wrap_message_id(id: &str) -> String {
    if id.starts_with('<') && id.ends_with('>') {
        id.to_string()
    } else {
        format!("<{id}>")
    }
}

/// A unique RFC822 Message-ID for a `.eml` file that arrived with none,
/// domained on the importing account (traceable, never a real DNS lookup).
/// Mirrors `castellum-provider-smtp::generate_message_id`'s shape — not
/// reused directly, since that function is private to its own crate — with
/// an "import" tag so diagnostics can tell the two sources apart.
fn synthesize_message_id(account_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = IMPORT_MSGID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "<castellum.import.{nanos}.{}.{seq}@{account_id}.invalid>",
        std::process::id()
    )
}

/// Prepend a `Message-ID:` header to `raw` so the appended copy carries the
/// same id used for the operation's idempotency check. RFC 5322 does not
/// require header order, so prepending is always well-formed; the injected
/// line matches the file's own line-ending convention (CRLF if the file
/// already has one anywhere, else bare LF) rather than rewriting the body.
fn inject_message_id_header(raw: &[u8], message_id: &str) -> Vec<u8> {
    let eol: &[u8] = if raw.windows(2).any(|w| w == b"\r\n") {
        b"\r\n"
    } else {
        b"\n"
    };
    let mut out = Vec::with_capacity(raw.len() + message_id.len() + 16);
    out.extend_from_slice(b"Message-ID: ");
    out.extend_from_slice(message_id.as_bytes());
    out.extend_from_slice(eol);
    out.extend_from_slice(raw);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use castellum_models::{Account, Mailbox};

    fn temp_db_and_dir(tag: &str) -> (Database, tempfile::TempDir) {
        let dir = tempfile::Builder::new()
            .prefix(&format!("castellum-import-test-{tag}-"))
            .tempdir()
            .expect("tempdir");
        let db = Database::open(&dir.path().join("castellum.db")).expect("open db");
        db.upsert_account(&Account {
            id: "acc1".into(),
            email: "test@local.test".into(),
            display_name: None,
            provider: "imap".into(),
        })
        .unwrap();
        db.upsert_mailbox(&Mailbox {
            id: "acc1:INBOX".into(),
            account_id: "acc1".into(),
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
            sync_by_default: true,
        })
        .unwrap();
        (db, dir)
    }

    fn write_eml(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn imports_a_well_formed_eml_and_wraps_its_existing_message_id() {
        let (db, dir) = temp_db_and_dir("ok");
        let file = write_eml(
            &dir,
            "a.eml",
            b"From: a@x\r\nTo: b@x\r\nSubject: Hi\r\nMessage-ID: existing@x\r\n\r\nbody\r\n",
        );
        let json = import_eml_json(&db, "acc1", "acc1:INBOX", &file).unwrap();
        let result: ImportEmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "queued", "error: {:?}", result.error);

        let ops = db.pending_operations("acc1").unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "import_eml");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(
            payload["message_id"], "<existing@x>",
            "wrapped in angle brackets"
        );
        assert_eq!(payload["dest_path"], "INBOX");

        let blob = db.operation_blob(ops[0].id).unwrap().unwrap();
        assert!(
            String::from_utf8_lossy(&blob).contains("Message-ID: existing@x"),
            "original header untouched when one already exists"
        );
    }

    #[test]
    fn imports_an_eml_with_no_message_id_and_injects_a_synthesized_one() {
        let (db, dir) = temp_db_and_dir("nomsgid");
        let file = write_eml(&dir, "b.eml", b"From: a@x\r\nSubject: Hi\r\n\r\nbody\r\n");
        let json = import_eml_json(&db, "acc1", "acc1:INBOX", &file).unwrap();
        let result: ImportEmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "queued");

        let ops = db.pending_operations("acc1").unwrap();
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        let msgid = payload["message_id"].as_str().unwrap().to_string();
        assert!(msgid.starts_with("<castellum.import."), "got {msgid}");

        let blob = db.operation_blob(ops[0].id).unwrap().unwrap();
        let text = String::from_utf8_lossy(&blob);
        assert!(
            text.starts_with(&format!("Message-ID: {msgid}\r\n")),
            "synthesized header injected at the top, got: {text}"
        );
        assert!(text.contains("From: a@x"), "original headers preserved");
    }

    #[test]
    fn refuses_a_file_that_does_not_look_like_a_message() {
        let (db, dir) = temp_db_and_dir("garbage");
        let file = write_eml(&dir, "c.eml", &[0xFF_u8; 64]);
        let json = import_eml_json(&db, "acc1", "acc1:INBOX", &file).unwrap();
        let result: ImportEmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("valid email"));
        assert!(db.pending_operations("acc1").unwrap().is_empty());
    }

    #[test]
    fn refuses_a_missing_file_without_panicking() {
        let (db, dir) = temp_db_and_dir("missing");
        let json = import_eml_json(
            &db,
            "acc1",
            "acc1:INBOX",
            &dir.path().join("nope.eml").to_string_lossy(),
        )
        .unwrap();
        let result: ImportEmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "failed");
    }

    #[test]
    fn refuses_a_mailbox_that_does_not_belong_to_the_account() {
        let (db, dir) = temp_db_and_dir("wrongmbox");
        let file = write_eml(&dir, "d.eml", b"From: a@x\r\nSubject: Hi\r\n\r\nbody\r\n");
        let json = import_eml_json(&db, "acc1", "acc2:INBOX", &file).unwrap();
        let result: ImportEmlResult = serde_json::from_str(&json).unwrap();
        assert_eq!(result.status, "failed");
        assert!(result.error.unwrap().contains("db:"));
    }

    #[test]
    fn wrap_message_id_does_not_double_wrap() {
        assert_eq!(wrap_message_id("bare@x"), "<bare@x>");
        assert_eq!(wrap_message_id("<already@x>"), "<already@x>");
    }

    #[test]
    fn inject_message_id_header_matches_crlf_when_the_file_uses_it() {
        let out = inject_message_id_header(b"From: a@x\r\n\r\nbody", "<id@x>");
        assert_eq!(&out[..], b"Message-ID: <id@x>\r\nFrom: a@x\r\n\r\nbody");
    }

    #[test]
    fn inject_message_id_header_falls_back_to_lf() {
        let out = inject_message_id_header(b"From: a@x\n\nbody", "<id@x>");
        assert_eq!(&out[..], b"Message-ID: <id@x>\nFrom: a@x\n\nbody");
    }
}
