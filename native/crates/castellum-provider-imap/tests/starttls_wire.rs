// Castellum - IMAP STARTTLS at the Session layer, scripted end to end (3375).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `Session::starttls` sends STARTTLS, accepts only a tagged OK, and refuses
// to hand the stream over when ANY byte follows that OK (a man in the middle
// can inject responses there that would otherwise be read as if they had
// arrived over TLS). NO, BAD and undecodable replies are errors, never a
// panic and never a silent fallback. The TLS handshake itself is the
// caller's (`ImapProvider`), tested in its own unit tests with a
// pass-through upgrade; a real handshake needs a port-143 server.

use castellum_provider_imap::session::{Session, SessionError};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// Greets, answers the first command with `reply` (raw bytes, one write),
/// then reports the first command and the next line it receives, if any.
fn server(reply: &'static str) -> (u16, mpsc::Receiver<(String, Option<String>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let _ = writer.write_all(b"* OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] ready\r\n");
        let mut first = String::new();
        let _ = reader.read_line(&mut first);
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
        let mut next = String::new();
        let got = match reader.read_line(&mut next) {
            Ok(n) if n > 0 => Some(next.trim_end().to_string()),
            _ => None,
        };
        let _ = tx.send((first.trim_end().to_string(), got));
    });
    (port, rx)
}

fn connect(port: u16) -> Session<TcpStream> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    Session::connect(tcp).expect("greeting")
}

#[test]
fn starttls_sends_the_command_and_returns_the_raw_stream() {
    let (port, rx) = server("C1 OK Begin TLS negotiation now\r\n");
    let mut raw = connect(port)
        .starttls()
        .expect("tagged OK hands the stream back");
    // The caller would start the TLS handshake here; prove it is the same
    // socket, positioned right after the OK.
    raw.write_all(b"HANDSHAKE\r\n").unwrap();
    let (first, next) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(first, "C1 STARTTLS");
    assert_eq!(next.as_deref(), Some("HANDSHAKE"));
}

#[test]
fn starttls_refuses_bytes_buffered_after_the_ok() {
    // CVE-2011-0411 class: a response injected after the OK, in cleartext.
    let (port, _rx) =
        server("C1 OK Begin TLS\r\n* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] injected\r\n");
    match connect(port).starttls() {
        Err(SessionError::Protocol(m)) => assert!(m.contains("injection"), "{m}"),
        Err(other) => panic!("expected Protocol, got {other:?}"),
        Ok(_) => panic!("an injected response must not be accepted"),
    }
}

#[test]
fn starttls_no_and_bad_are_errors() {
    let (port, _rx) = server("C1 NO STARTTLS is not available\r\n");
    assert!(matches!(
        connect(port).starttls(),
        Err(SessionError::No { .. })
    ));
    let (port, _rx) = server("C1 BAD Unknown command\r\n");
    assert!(matches!(
        connect(port).starttls(),
        Err(SessionError::Bad { .. })
    ));
}

#[test]
fn starttls_undecodable_reply_is_an_error_not_a_panic() {
    let (port, _rx) = server("C1 \x01\x02 garbage (((\r\n");
    assert!(connect(port).starttls().is_err());
}

#[test]
fn a_session_resumed_after_the_handshake_reads_nothing_until_asked() {
    // resume_after_starttls must not expect a second greeting.
    let (port, rx) = server("C1 OK go\r\n");
    let raw = connect(port).starttls().unwrap();
    let mut resumed = Session::resume_after_starttls(raw);
    // The scripted server is not a real IMAP server after the OK; a NOOP
    // goes out as the first command of the resumed session with tag C1.
    let _ = resumed.noop();
    let (_, next) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(next.as_deref(), Some("C1 NOOP"));
}
