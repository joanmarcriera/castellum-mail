// Castellum - UIDVALIDITY-guarded write wire test.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Proves `ImapProvider`'s `*_checked` writes compare
// the mailbox's UIDVALIDITY on the SELECT the write itself needs, and send
// NOTHING addressed by UID when it is not the expected value: no STORE, no
// MOVE/COPY, no EXPUNGE. A UID names a message only inside one UIDVALIDITY,
// so after a reset the same number is some other message. Also proves the
// matching case still sends the write, and that `find_uids_by_message_id`
// reports every copy, not only the newest.
//
// Same scripted-TCP-server pattern as expunge_trash_wire.rs (deliberately a
// separate copy of the harness, not shared).

use castellum_provider_imap::ImapProvider;
use castellum_providers::{Guarded, ImapConfig, MailProvider};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Every command line the server received (tag stripped), in order.
    commands: Arc<Mutex<Vec<String>>>,
}

/// (command prefix to match, untagged payload, tagged-OK trailer)
type Replies = Vec<(&'static str, String, String)>;

impl ScriptedServer {
    fn start(replies: Replies) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let commands_clone = commands.clone();
        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, replies, commands_clone);
        });
        ScriptedServer {
            port,
            handle: Some(handle),
            commands,
        }
    }

    /// Upper-cased commands received so far.
    fn received(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.to_ascii_uppercase())
            .collect()
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(stream: TcpStream, replies: Replies, commands: Arc<Mutex<Vec<String>>>) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(b"* OK Test ready\r\n");
    let _ = writer.flush();

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let mut parts = trimmed.splitn(2, ' ');
        let tag = parts.next().unwrap_or("*").to_string();
        let rest = parts.next().unwrap_or("").to_string();
        let command = rest.to_ascii_uppercase();
        commands.lock().unwrap().push(rest);

        if command.starts_with("LOGOUT") {
            let _ = writer.write_all(b"* BYE Logging out\r\n");
            let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
            let _ = writer.flush();
            return;
        }
        match replies.iter().find(|(k, _, _)| command.starts_with(k)) {
            Some((_, untagged, ok)) => {
                let _ = writer.write_all(untagged.as_bytes());
                let _ = writer.write_all(format!("{tag} {ok}\r\n").as_bytes());
            }
            None => {
                let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
            }
        }
        let _ = writer.flush();
    }
}

fn connect(port: u16) -> ImapProvider {
    ImapProvider::connect(&ImapConfig {
        host: "127.0.0.1".into(),
        port,
        username: "user".into(),
        password: "pass".into(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: true,
    })
    .expect("connect")
}

/// A server with MOVE and UIDPLUS whose every mailbox reports `select`.
fn server(select: &str) -> ScriptedServer {
    ScriptedServer::start(vec![
        ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
        (
            "CAPABILITY",
            "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n".to_string(),
            "OK Success".to_string(),
        ),
        (
            "SELECT",
            select.to_string(),
            "OK [READ-WRITE] selected".to_string(),
        ),
        (
            "UID SEARCH",
            "* SEARCH 9 4 7\r\n".to_string(),
            "OK SEARCH completed".to_string(),
        ),
        // The Message-ID lookups confirm each SEARCH hit's real header
        // (SEARCH HEADER is a substring match): all three are exact copies.
        (
            "UID FETCH",
            [(9, 1), (4, 2), (7, 3)]
                .iter()
                .map(|(uid, seq)| {
                    format!(
                        "* {seq} FETCH (UID {uid} ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL \"<m1@x>\"))\r\n"
                    )
                })
                .collect(),
            "OK FETCH completed".to_string(),
        ),
    ])
}

/// The mailbox as it is on the server today: UIDVALIDITY 42.
const UV42: &str = "* 3 EXISTS\r\n* OK [UIDVALIDITY 42] UIDs valid.\r\n";
/// A server that reports no UIDVALIDITY at all.
const NO_UV: &str = "* 3 EXISTS\r\n";

/// Commands that act on a message by UID. None may follow a refused SELECT.
fn uid_writes(received: &[String]) -> Vec<&String> {
    received
        .iter()
        .filter(|c| {
            [
                "UID STORE",
                "UID MOVE",
                "UID COPY",
                "UID EXPUNGE",
                "EXPUNGE",
            ]
            .iter()
            .any(|w| c.starts_with(w))
        })
        .collect()
}

#[test]
fn delete_uid_checked_deletes_when_uidvalidity_matches() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let outcome = provider.delete_uid_checked("Drafts", 42, 9).expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::Applied(()));
    let received = server.received();
    assert!(
        received.iter().any(|c| c.starts_with("UID STORE 9 ")),
        "{received:?}"
    );
    assert!(
        received.iter().any(|c| c.starts_with("UID EXPUNGE 9")),
        "{received:?}"
    );
}

#[test]
fn delete_uid_checked_sends_nothing_when_uidvalidity_changed() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    // The cleanup was queued when Drafts was UID space 41.
    let outcome = provider.delete_uid_checked("Drafts", 41, 9).expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::UidValidityChanged { current: 42 });
    let received = server.received();
    assert!(
        uid_writes(&received).is_empty(),
        "uid 9 is a different message now: {received:?}"
    );
}

#[test]
fn a_server_that_reports_no_uidvalidity_is_refused() {
    let server = server(NO_UV);
    let mut provider = connect(server.port);
    let outcome = provider.delete_uid_checked("Drafts", 42, 9).expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::UidValidityChanged { current: -1 });
    assert!(uid_writes(&server.received()).is_empty());
}

#[test]
fn an_unknown_expected_uidvalidity_never_matches() {
    // 0 is the DB's "no sync cursor yet" sentinel: it must not match a
    // server that (wrongly) reports 0 either, nor anything else.
    for expected in [0, -1] {
        let server = server(UV42);
        let mut provider = connect(server.port);
        let outcome = provider
            .delete_uid_checked("Drafts", expected, 9)
            .expect("ok");
        let _ = provider.logout();
        assert_eq!(outcome, Guarded::UidValidityChanged { current: 42 });
        assert!(uid_writes(&server.received()).is_empty());
    }
}

#[test]
fn move_message_checked_moves_when_uidvalidity_matches() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let outcome = provider
        .move_message_checked("INBOX", 42, 7, "<7@x>", "Trash")
        .expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::Applied(()));
    let received = server.received();
    assert!(
        received.iter().any(|c| c.starts_with("UID MOVE 7 ")),
        "{received:?}"
    );
}

#[test]
fn move_message_checked_sends_nothing_when_uidvalidity_changed() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let outcome = provider
        .move_message_checked("INBOX", 5, 7, "<7@x>", "Trash")
        .expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::UidValidityChanged { current: 42 });
    let received = server.received();
    assert!(uid_writes(&received).is_empty(), "{received:?}");
    assert!(
        !received.iter().any(|c| c.starts_with("UID SEARCH")),
        "refused on the SELECT itself, before even looking for the UID: {received:?}"
    );
}

#[test]
fn flag_read_and_keyword_stores_send_nothing_when_uidvalidity_changed() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let changed = Guarded::UidValidityChanged { current: 42 };
    assert_eq!(
        provider.store_seen_checked("INBOX", 5, 7, true).unwrap(),
        changed
    );
    assert_eq!(
        provider.store_flagged_checked("INBOX", 5, 7, true).unwrap(),
        changed
    );
    assert_eq!(
        provider
            .store_keyword_checked("INBOX", 5, 7, "Work", true)
            .unwrap(),
        Guarded::UidValidityChanged { current: 42 }
    );
    let _ = provider.logout();

    let received = server.received();
    assert!(uid_writes(&received).is_empty(), "{received:?}");
}

#[test]
fn flag_and_read_stores_are_sent_when_uidvalidity_matches() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    assert_eq!(
        provider.store_seen_checked("INBOX", 42, 7, true).unwrap(),
        Guarded::Applied(())
    );
    assert_eq!(
        provider
            .store_flagged_checked("INBOX", 42, 8, false)
            .unwrap(),
        Guarded::Applied(())
    );
    let _ = provider.logout();

    let received = server.received();
    let stores: Vec<_> = received
        .iter()
        .filter(|c| c.starts_with("UID STORE"))
        .collect();
    assert_eq!(stores.len(), 2, "{received:?}");
    assert!(stores[0].starts_with("UID STORE 7 +FLAGS") && stores[0].contains("SEEN"));
    assert!(stores[1].starts_with("UID STORE 8 -FLAGS") && stores[1].contains("FLAGGED"));
    // One SELECT per write: the compare rides on it, no extra round trip.
    assert_eq!(
        received.iter().filter(|c| c.starts_with("SELECT")).count(),
        2,
        "{received:?}"
    );
}

// --- bulk UID-set writes ( follow-up to #3368) ------------------
//
// Empty Trash / permanent delete (`expunge_mailbox_checked`) and Mark All as
// Read (`mark_seen_uids_checked`) used to be checked by the caller on one
// SELECT and executed under a second, unchecked one.

#[test]
fn expunge_mailbox_checked_expunges_the_snapshot_when_uidvalidity_matches() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let outcome = provider
        .expunge_mailbox_checked("Trash", 42, "3,5:7")
        .expect("ok");
    let _ = provider.logout();

    assert_eq!(outcome, Guarded::Applied(()));
    let received = server.received();
    assert!(
        received
            .iter()
            .any(|c| c.starts_with("UID STORE 3,5:7 +FLAGS") && c.contains("DELETED")),
        "{received:?}"
    );
    assert!(
        received.iter().any(|c| c.starts_with("UID EXPUNGE 3,5:7")),
        "{received:?}"
    );
    // The compare rides on the write's own SELECT: exactly one, so there is
    // no second, unchecked SELECT for a rebuilt mailbox to slip through.
    assert_eq!(
        received.iter().filter(|c| c.starts_with("SELECT")).count(),
        1,
        "{received:?}"
    );
}

#[test]
fn expunge_mailbox_checked_sends_nothing_when_uidvalidity_is_not_the_snapshot() {
    // (select reply, expected, reported current)
    for (select, expected, current) in [
        (UV42, 41, 42),  // Trash was rebuilt since the snapshot
        (NO_UV, 42, -1), // the server reports no UIDVALIDITY at all
        (UV42, 0, 42),   // no snapshot value was ever recorded
        (UV42, -1, 42),
    ] {
        let server = server(select);
        let mut provider = connect(server.port);
        let outcome = provider
            .expunge_mailbox_checked("Trash", expected, "3,5:7")
            .expect("ok");
        let _ = provider.logout();

        assert_eq!(outcome, Guarded::UidValidityChanged { current });
        let received = server.received();
        assert!(
            uid_writes(&received).is_empty(),
            "expected {expected}: UIDs 3,5:7 are other messages now: {received:?}"
        );
    }
}

#[test]
fn mark_seen_uids_checked_stores_only_when_uidvalidity_matches() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    assert_eq!(
        provider
            .mark_seen_uids_checked("INBOX", 42, "3,5:7")
            .unwrap(),
        Guarded::Applied(())
    );
    let _ = provider.logout();
    let received = server.received();
    assert!(
        received
            .iter()
            .any(|c| c.starts_with("UID STORE 3,5:7 +FLAGS") && c.contains("SEEN")),
        "{received:?}"
    );
    assert_eq!(
        received.iter().filter(|c| c.starts_with("SELECT")).count(),
        1,
        "{received:?}"
    );

    for (select, expected, current) in [(UV42, 41, 42), (NO_UV, 42, -1), (UV42, 0, 42)] {
        let server = self::server(select);
        let mut provider = connect(server.port);
        assert_eq!(
            provider
                .mark_seen_uids_checked("INBOX", expected, "3,5:7")
                .unwrap(),
            Guarded::UidValidityChanged { current }
        );
        let _ = provider.logout();
        let received = server.received();
        assert!(uid_writes(&received).is_empty(), "{received:?}");
    }
}

#[test]
fn find_uids_by_message_id_reports_every_copy_in_ascending_order() {
    let server = server(UV42);
    let mut provider = connect(server.port);
    let matches = provider
        .find_uids_by_message_id("Drafts", "<m1@x>")
        .expect("ok");
    let _ = provider.logout();

    assert_eq!(matches.uid_validity, 42);
    assert_eq!(matches.uids, vec![4, 7, 9]);
    // The single-result lookup still reports only the newest.
    let server = self::server(UV42);
    let mut provider = connect(server.port);
    let newest = provider
        .find_uid_by_message_id("Drafts", "<m1@x>")
        .expect("ok")
        .expect("found");
    let _ = provider.logout();
    assert_eq!(newest.uid, 9);
}
