// Castellum - which SMTP send failures leave the submission's outcome unknown.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `LettreSmtpSender::send` reports `ProviderError::MaybeDelivered` only when
// the server may already hold the message (the send executor then holds the op
// instead of retrying it), and keeps the ordinary retryable/permanent kinds
// when nothing can have been accepted. Every case runs against a real local
// socket, which also pins the lettre `Display` prefix the classifier reads.

use castellum_provider_smtp::LettreSmtpSender;
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::time::Duration;

/// What the fake server does once the client has sent DATA.
#[derive(Clone, Copy)]
enum AfterData {
    /// Read the whole message, then close without a final reply.
    CloseAfterMessage,
    /// Read the whole message, then refuse it with a transient 451 reply.
    Reply451,
    /// Stop reading and close with unread bytes queued: the kernel resets
    /// the connection while the client is still writing the message.
    ResetMidMessage,
}

fn sender_for(port: u16) -> LettreSmtpSender {
    LettreSmtpSender::new(&SmtpConfig {
        host: "127.0.0.1".into(),
        port,
        username: String::new(),
        password: String::new(),
        use_tls: false,
        allow_invalid_certs: false,
    })
    .unwrap()
}

fn serve_one(listener: TcpListener, after_data: AfterData) {
    std::thread::spawn(move || {
        let Ok((mut out, _)) = listener.accept() else {
            return;
        };
        let mut lines = BufReader::new(out.try_clone().unwrap());
        let _ = out.write_all(b"220 fake.test ESMTP\r\n");
        let mut line = String::new();
        loop {
            line.clear();
            if lines.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let verb = line.trim_end().to_ascii_uppercase();
            if verb == "DATA" {
                let _ = out.write_all(b"354 go ahead\r\n");
                break;
            } else if verb == "QUIT" {
                let _ = out.write_all(b"221 bye\r\n");
                return;
            }
            let _ = out.write_all(b"250 OK\r\n");
        }
        match after_data {
            AfterData::ResetMidMessage => {
                let mut first = [0u8; 1024];
                let _ = lines.read(&mut first);
                std::thread::sleep(Duration::from_millis(100));
                drop(lines);
                drop(out); // unread bytes queued: RST, not FIN
            }
            AfterData::CloseAfterMessage | AfterData::Reply451 => {
                loop {
                    line.clear();
                    if lines.read_line(&mut line).unwrap_or(0) == 0 || line == ".\r\n" {
                        break;
                    }
                }
                if let AfterData::Reply451 = after_data {
                    let _ = out.write_all(b"451 4.3.0 try again later\r\n");
                    // Answer the client's QUIT, if any, then hang up.
                    line.clear();
                    let _ = lines.read_line(&mut line);
                    let _ = out.write_all(b"221 bye\r\n");
                }
                let _ = out.shutdown(Shutdown::Both);
            }
        }
    });
}

fn send_against(after_data: AfterData, raw: &[u8]) -> ProviderError {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    serve_one(listener, after_data);
    sender_for(port)
        .send("alice@example.com", &["bob@example.com".to_string()], raw)
        .expect_err("the fake server never accepts")
}

const SMALL: &[u8] = b"Subject: hi\r\n\r\nbody\r\n";

#[test]
fn a_close_after_data_is_maybe_delivered() {
    let err = send_against(AfterData::CloseAfterMessage, SMALL);
    assert!(
        matches!(&err, ProviderError::MaybeDelivered(m) if m.starts_with("response error")),
        "{err:?}"
    );
}

#[test]
fn a_reset_while_writing_the_message_is_maybe_delivered() {
    // Large enough that the client is still writing when the reset lands.
    let mut raw = b"Subject: big\r\n\r\n".to_vec();
    while raw.len() < 16 * 1024 * 1024 {
        raw.extend_from_slice(b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abc\r\n");
    }
    let err = send_against(AfterData::ResetMidMessage, &raw);
    assert!(
        matches!(&err, ProviderError::MaybeDelivered(m) if m.starts_with("network error")),
        "{err:?}"
    );
}

#[test]
fn a_transient_reply_after_data_is_retryable_not_held() {
    // The server answered: the message was NOT accepted, so a retry is safe.
    // A 4xx is `Deferred` (retried on the backoff), not `MaybeDelivered`.
    let err = send_against(AfterData::Reply451, SMALL);
    assert!(matches!(err, ProviderError::Deferred(_)), "{err:?}");
}

#[test]
fn a_refused_connection_is_retryable_not_held() {
    // Nothing listening: nothing can have been submitted.
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
    let err = sender_for(port)
        .send("alice@example.com", &["bob@example.com".to_string()], SMALL)
        .expect_err("nothing is listening");
    assert!(matches!(err, ProviderError::Network(_)), "{err:?}");
}
