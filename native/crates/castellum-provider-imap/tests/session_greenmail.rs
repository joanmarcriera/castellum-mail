// Castellum - Session GreenMail e2e.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drives the codec-backed Session against a real GreenMail IMAP server, the
// same way castellum-core's GreenMail tests drive ImapProvider. #[ignore]d so
// plain `cargo test` stays offline; run with:
//   docker-compose -f scripts/local-imap/docker-compose.yml up -d
//   cargo test -p castellum-provider-imap \
//       --test session_greenmail -- --ignored
// Accounts are auto-created by an SMTP delivery (login == password == address);
// each test uses its own account, so tests are parallel-safe. Uses only
// throwaway local accounts — never a real profile or Keychain namespace.

use castellum_provider_imap::session::{FetchItem, Session, SessionFlag, StoreSign};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

fn host() -> String {
    std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

/// Minimal lockstep SMTP delivery; triggers GreenMail's account auto-creation.
fn smtp_deliver(to: &str, subject: &str, message_id: &str) {
    let stream = TcpStream::connect((host().as_str(), 3025)).expect("connect GreenMail SMTP");
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut writer = stream;
    let mut expect = |code: &str| loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("read SMTP reply");
        assert!(line.starts_with(code), "SMTP expected {code}, got: {line}");
        if line.as_bytes().get(3) == Some(&b' ') {
            break;
        }
    };
    expect("220");
    for (cmd, code) in [
        ("HELO local.test\r\n".to_string(), "250"),
        ("MAIL FROM:<sender@local.test>\r\n".to_string(), "250"),
        (format!("RCPT TO:<{to}>\r\n"), "250"),
        ("DATA\r\n".to_string(), "354"),
    ] {
        writer.write_all(cmd.as_bytes()).expect("write");
        expect(code);
    }
    let body = format!(
        "From: sender@local.test\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{message_id}>\r\n\r\nhello\r\n.\r\n"
    );
    writer.write_all(body.as_bytes()).expect("write body");
    expect("250");
    writer.write_all(b"QUIT\r\n").expect("quit");
}

fn login(account: &str) -> Session<TcpStream> {
    let stream = TcpStream::connect((host().as_str(), 3143)).expect("connect GreenMail IMAP");
    let mut session = Session::connect(stream).expect("greeting");
    session.login(account, account).expect("LOGIN");
    session
}

#[test]
#[ignore = "needs GreenMail on 127.0.0.1:3143 (scripts/local-imap)"]
fn session_round_trip_against_greenmail() {
    let account = "session-e2e-roundtrip@local.test";
    smtp_deliver(account, "session e2e", "session-e2e-1@local.test");

    let mut s = login(account);
    let caps = s.capability().expect("CAPABILITY");
    assert!(!caps.is_empty());
    let boxes = s.list("", "*").expect("LIST");
    assert!(boxes.iter().any(|m| m.path == "INBOX"), "{boxes:?}");
    s.lsub("", "*").expect("LSUB");
    // GreenMail may or may not implement NAMESPACE; only require no panic.
    let _ = s.namespace();

    let inbox = s.select("INBOX").expect("SELECT");
    assert!(inbox.exists >= 1);
    assert!(inbox.uid_validity.is_some());

    let fetched = s
        .uid_fetch(
            "1:*",
            &[
                FetchItem::Uid,
                FetchItem::Flags,
                FetchItem::Rfc822Size,
                FetchItem::Envelope,
                FetchItem::InternalDate,
                FetchItem::BodyStructure,
            ],
        )
        .expect("UID FETCH descriptors");
    let msg = fetched
        .iter()
        .find(|m| m.envelope.as_ref().and_then(|e| e.subject.as_deref()) == Some("session e2e"))
        .expect("delivered message present");
    let uid = msg.uid.expect("uid");
    assert_eq!(
        msg.envelope.as_ref().and_then(|e| e.message_id.as_deref()),
        Some("session-e2e-1@local.test")
    );
    assert_eq!(msg.has_attachments, Some(false));

    let bodies = s
        .uid_fetch(&uid.to_string(), &[FetchItem::Uid, FetchItem::BodyPeek])
        .expect("UID FETCH body");
    let body = bodies[0].body.as_deref().expect("body bytes");
    assert!(String::from_utf8_lossy(body).contains("hello"));

    s.uid_store(&uid.to_string(), StoreSign::Add, &[SessionFlag::Seen])
        .expect("STORE +Seen");
    let after = s
        .uid_fetch(&uid.to_string(), &[FetchItem::Uid, FetchItem::Flags])
        .expect("FETCH flags");
    assert!(after[0].flags.contains(&SessionFlag::Seen));

    assert_eq!(s.uid_search_by_uid(uid).expect("SEARCH uid"), vec![uid]);
    assert_eq!(
        s.uid_search_by_message_id("session-e2e-1@local.test")
            .expect("SEARCH message-id"),
        vec![uid]
    );

    // CREATE (ignoring "already exists" on reruns), APPEND with a literal,
    // then find it again by Message-ID.
    let _ = s.create("SessionArchive");
    let raw = format!(
        "From: me@local.test\r\nTo: {account}\r\nSubject: appended\r\nMessage-ID: <session-e2e-appended@local.test>\r\n\r\n{}\r\n",
        "y".repeat(600)
    );
    s.append("SessionArchive", &[SessionFlag::Seen], raw.as_bytes())
        .expect("APPEND");
    s.select("SessionArchive").expect("SELECT archive");
    assert!(!s
        .uid_search_by_message_id("session-e2e-appended@local.test")
        .expect("SEARCH appended")
        .is_empty());

    // COPY + flag Deleted + EXPUNGE path within the INBOX.
    s.select("INBOX").expect("re-SELECT INBOX");
    s.uid_copy(&uid.to_string(), "SessionArchive")
        .expect("UID COPY");
    s.uid_store(&uid.to_string(), StoreSign::Add, &[SessionFlag::Deleted])
        .expect("STORE +Deleted");
    s.expunge().expect("EXPUNGE");
    assert!(s
        .uid_search_by_uid(uid)
        .expect("SEARCH after expunge")
        .is_empty());

    s.logout().expect("LOGOUT");
}
