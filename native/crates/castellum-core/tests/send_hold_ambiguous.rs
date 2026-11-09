// Castellum - "never double-send" regression tests for the outbound executor.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Two windows where an SMTP submission may already have reached the server
// but the send executor cannot prove it:
//
// (a) the connection drops after DATA, before the server's final reply, so
//     the client never learns whether the message was accepted;
// (b) the server accepts the message (250) and the local database write that
//     records the success fails.
//
// In both, retrying automatically would risk mailing every recipient a second
// copy. The op must end `held` ("may have been sent", only the user's Send Now
// or Back to Drafts moves it) or `sent`, and exactly ONE submission may reach
// the server across any number of later drains.
//
// (a) drives the real `LettreSmtpSender` against a tiny in-process SMTP
// server, so the test covers the provider's error classification as well as
// the executor's reaction to it: the fault is on the wire, not in a fake.

use castellum_core::send::send_pending_with_sender_at;
use castellum_db::Database;
use castellum_models::{Account, DraftAddress, NewDraft};
use castellum_provider_smtp::LettreSmtpSender;
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Far beyond any backoff the shared replay machinery can choose, so the
/// second drain would retry a `failed` op if one were left behind.
const A_DAY_LATER: i64 = 24 * 60 * 60;

fn temp_db_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = castellum_testutil::scratch_root().join(format!(
        "castellum-send-hold-test-{}-{}.db",
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_file(&path);
    path
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn seed_account_and_queue(db: &Database, subject: &str) -> i64 {
    db.upsert_account(&Account {
        id: "acc".into(),
        email: "alice@example.com".into(),
        display_name: Some("Alice".into()),
        provider: "imap".into(),
    })
    .unwrap();
    db.set_account_write_enabled("acc", true).unwrap();
    // The launch-time sweep has already run clean (see the corollary in
    // `send_pending_impl`); this is an ordinary instant send, not a crash
    // survivor.
    db.reconcile_undo_sends("acc").unwrap();
    let id = db
        .save_draft(&NewDraft {
            id: None,
            account_id: "acc".into(),
            to: vec![DraftAddress {
                name: None,
                email: "bob@example.com".into(),
            }],
            cc: vec![],
            bcc: vec![],
            subject: subject.into(),
            body_text: "body\n".into(),
            body_html: None,
            from_email: None,
            from_name: None,
            in_reply_to: None,
            references_header: None,
        })
        .unwrap();
    db.send_draft(id).unwrap();
    id
}

fn drain_at(db: &Database, sender: &mut dyn SmtpSender, now: i64) {
    send_pending_with_sender_at(db, "acc", "alice@example.com", None, sender, now)
        .expect("a drain never aborts on a per-op failure");
}

fn assert_held_never_requeued(db: &Database, draft_id: i64) {
    let ops: Vec<_> = db
        .pending_operations("acc")
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == "send")
        .collect();
    assert_eq!(ops.len(), 1, "exactly one send op: {ops:?}");
    let op = &ops[0];
    assert_eq!(op.status, "held", "never queued/failed/retried: {op:?}");
    let error = op.last_error.as_deref().unwrap_or_default();
    assert!(error.starts_with("may have been sent"), "{error}");
    assert!(error.contains("Check Sent before retrying"), "{error}");
    // Still in the Outbox (not silently marked sent): the user decides.
    assert_eq!(db.get_draft(draft_id).unwrap().unwrap().status, "outbox");
}

/// An SMTP server that accepts the envelope, reads the whole message after
/// DATA, then closes the connection WITHOUT the final 250/5xx reply: the
/// textbook ambiguous submission. Counts every message body it received.
fn spawn_drop_after_data_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut out) = stream else { return };
            let mut lines = BufReader::new(out.try_clone().unwrap());
            let _ = out.write_all(b"220 fake.test ESMTP\r\n");
            let mut line = String::new();
            loop {
                line.clear();
                if lines.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let verb = line.trim_end().to_ascii_uppercase();
                if verb.starts_with("EHLO") || verb.starts_with("HELO") {
                    let _ = out.write_all(b"250 fake.test\r\n");
                } else if verb.starts_with("MAIL") || verb.starts_with("RCPT") {
                    let _ = out.write_all(b"250 OK\r\n");
                } else if verb == "DATA" {
                    let _ = out.write_all(b"354 go ahead\r\n");
                    loop {
                        line.clear();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 || line == ".\r\n" {
                            break;
                        }
                    }
                    seen.fetch_add(1, Ordering::SeqCst);
                    // The message is in the server's hands; the reply never
                    // reaches the client.
                    let _ = out.shutdown(Shutdown::Both);
                    break;
                } else if verb == "QUIT" {
                    let _ = out.write_all(b"221 bye\r\n");
                    break;
                } else {
                    let _ = out.write_all(b"250 OK\r\n");
                }
            }
        }
    });
    (port, bodies)
}

#[test]
fn a_connection_dropped_after_data_is_held_and_submitted_exactly_once() {
    let (port, bodies) = spawn_drop_after_data_server();
    let mut sender = LettreSmtpSender::new(&SmtpConfig {
        host: "127.0.0.1".into(),
        port,
        username: String::new(),
        password: String::new(),
        use_tls: false,
        allow_invalid_certs: false,
    })
    .unwrap();

    let db = Database::open(&temp_db_path()).unwrap();
    let draft_id = seed_account_and_queue(&db, "Dropped after DATA");
    let now = now_epoch();

    drain_at(&db, &mut sender, now);
    assert_eq!(bodies.load(Ordering::SeqCst), 1, "first drain submits once");
    assert_held_never_requeued(&db, draft_id);

    // Next launch's drain, long after any backoff: nothing is resubmitted.
    drain_at(&db, &mut sender, now + A_DAY_LATER);
    assert_eq!(
        bodies.load(Ordering::SeqCst),
        1,
        "a second drain must not submit the ambiguous message again"
    );
    assert_held_never_requeued(&db, draft_id);
}

/// Accepts every submission (the server said 250) and counts them.
#[derive(Default)]
struct AcceptingSender {
    submissions: usize,
}

impl SmtpSender for AcceptingSender {
    fn send(&mut self, _from: &str, _rcpts: &[String], _raw: &[u8]) -> Result<(), ProviderError> {
        self.submissions += 1;
        Ok(())
    }
}

#[test]
fn a_failed_db_write_after_smtp_success_is_never_resubmitted() {
    let path = temp_db_path();
    let db = Database::open(&path).unwrap();
    let draft_id = seed_account_and_queue(&db, "Accepted, then the disk failed");

    // A second connection to the same file makes the one write that records
    // the delivery fail, as a full disk or an I/O error would.
    let side = rusqlite::Connection::open(&path).unwrap();
    side.execute_batch(
        "CREATE TRIGGER inject_sent_write_failure
         BEFORE UPDATE OF status ON drafts
         WHEN NEW.status = 'sent'
         BEGIN SELECT RAISE(ABORT, 'injected: disk I/O error'); END;",
    )
    .unwrap();

    let mut sender = AcceptingSender::default();
    let now = now_epoch();
    drain_at(&db, &mut sender, now);
    assert_eq!(sender.submissions, 1);

    // The disk recovers; the next drain must still not resubmit.
    side.execute_batch("DROP TRIGGER inject_sent_write_failure;")
        .unwrap();
    drain_at(&db, &mut sender, now + A_DAY_LATER);
    assert_eq!(
        sender.submissions, 1,
        "the server already accepted it: a second drain must not resubmit"
    );

    let ops: Vec<_> = db
        .pending_operations("acc")
        .unwrap()
        .into_iter()
        .filter(|op| op.kind == "send")
        .collect();
    // Held for the user (pending), or recorded as delivered (no longer
    // pending and the draft is `sent`) — never queued, failed or retried.
    match ops.as_slice() {
        [] => assert_eq!(db.get_draft(draft_id).unwrap().unwrap().status, "sent"),
        [_] => assert_held_never_requeued(&db, draft_id),
        more => panic!("one send op at most: {more:?}"),
    }
}
