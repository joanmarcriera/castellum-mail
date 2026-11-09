// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Shared GreenMail e2e test support.
//
// Every GreenMail test provisions its OWN account so the suite is
// parallel-safe: `-Dgreenmail.setup.test.all` auto-creates an account when
// mail is delivered to an unknown address (login == password == the address,
// verified against GreenMail 2.1.9), so a bootstrap SMTP delivery is all the
// provisioning a test needs. IMAP login alone does NOT auto-create accounts.
//
// The host is overridable with GREENMAIL_HOST for CI service containers
// (default 127.0.0.1 for the local scripts/local-imap docker-compose).
// Ports are fixed: SMTP 3025, IMAP 3143, IMAPS 3993.
//
// Each Cargo integration-test binary compiles its own copy of this module and
// uses a subset of it, so dead_code is expected.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

pub const IMAP_PORT: u16 = 3143;
pub const IMAPS_PORT: u16 = 3993;
pub const SMTP_PORT: u16 = 3025;

pub fn host() -> String {
    std::env::var("GREENMAIL_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

/// Provision (idempotently) a dedicated GreenMail account for one test and
/// return its address. Password == address. Re-provisioning an existing
/// account just delivers one more bootstrap message to its INBOX, so tests
/// must assert on their own probes or on within-run invariants, never on
/// absolute pre-existing mailbox contents.
pub fn provision(test: &str) -> String {
    let email = format!("e2e-{test}@local.test");
    smtp_deliver(
        "bootstrap@local.test",
        &email,
        &format!("Subject: castellum e2e account bootstrap\r\n\r\nbootstrap for {test}\r\n"),
    );
    email
}

/// Minimal lockstep SMTP delivery (HELO/MAIL/RCPT/DATA/QUIT) used only to
/// trigger GreenMail's deliver-to-unknown-address account auto-creation.
pub fn smtp_deliver(from: &str, to: &str, headers_and_body: &str) {
    let stream = TcpStream::connect((host().as_str(), SMTP_PORT)).expect("connect GreenMail SMTP");
    let mut reader = BufReader::new(stream.try_clone().expect("clone SMTP stream"));
    let mut writer = stream;
    let mut expect = |code: &str| {
        // Multi-line replies repeat the code with '-' until the final
        // "<code> " line; read until that terminator.
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read SMTP reply");
            assert!(line.starts_with(code), "SMTP expected {code}, got: {line}");
            if line.as_bytes().get(3) == Some(&b' ') {
                break;
            }
        }
    };
    expect("220");
    writer.write_all(b"HELO castellum-e2e\r\n").unwrap();
    expect("250");
    writer
        .write_all(format!("MAIL FROM:<{from}>\r\n").as_bytes())
        .unwrap();
    expect("250");
    writer
        .write_all(format!("RCPT TO:<{to}>\r\n").as_bytes())
        .unwrap();
    expect("250");
    writer.write_all(b"DATA\r\n").unwrap();
    expect("354");
    writer
        .write_all(format!("{headers_and_body}\r\n.\r\n").as_bytes())
        .unwrap();
    expect("250");
    writer.write_all(b"QUIT\r\n").unwrap();
    // Read the closing 221 so the server never sees a half-closed connection
    // with an unread reply (some stacks RST those).
    expect("221");
}

/// Plain-IMAP session for a provisioned per-test account (password == address).
///
/// An independent verification oracle (a raw `Session`, never `ImapProvider`,
/// which is what these tests exercise).
pub fn imap_session(email: &str) -> castellum_provider_imap::session::Session<TcpStream> {
    imap_login(email, email)
}

/// As [`imap_session`] with explicit credentials.
pub fn imap_login(user: &str, pass: &str) -> castellum_provider_imap::session::Session<TcpStream> {
    let tcp =
        TcpStream::connect((host().as_str(), IMAP_PORT)).expect("tcp connect to GreenMail IMAP");
    let mut session =
        castellum_provider_imap::session::Session::connect(tcp).expect("IMAP greeting");
    session.login(user, pass).expect("IMAP login");
    session
}

/// Castellum IMAP sync config JSON for a per-test account.
pub fn sync_config_json(account_id: &str, email: &str) -> String {
    serde_json::json!({
        "account_id": account_id,
        "email": email,
        "host": host(),
        "port": IMAP_PORT,
        "username": email,
        "password": email,
        "use_tls": false,
        "allow_invalid_certs": false,
        "mailboxes": ["INBOX"]
    })
    .to_string()
}

/// A per-run unique tag: pid alone can repeat across container restarts, so
/// mix in nanoseconds.
pub fn run_tag(test: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{test}-{}-{nanos}", std::process::id())
}
