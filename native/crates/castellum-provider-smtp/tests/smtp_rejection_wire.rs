// Castellum - SMTP permanent-rejection wire test, scripted end to end (G8).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drives the real lettre `SmtpTransport` (via `LettreSmtpSender`) against a
// local scripted server that accepts the login and then answers RCPT TO or
// the end of DATA with a chosen reply. A permanent 5xx refusal of the
// message must surface as `ProviderError::Rejected` (no automatic retry,
//), while a transient 4xx is a retryable `Deferred` error: the
// server answered, so it must not read as "couldn't reach the server".

use castellum_provider_smtp::LettreSmtpSender;
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Where the server refuses: at the recipient, or after the message body.
#[derive(Clone, Copy)]
enum Stage {
    Rcpt,
    Data,
}

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedServer {
    fn start(stage: Stage, reply: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream, stage, reply);
            }
        });
        ScriptedServer {
            port,
            handle: Some(handle),
        }
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(stream: TcpStream, stage: Stage, reply: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let say = |w: &mut TcpStream, s: &str| {
        let _ = w.write_all(s.as_bytes());
        let _ = w.flush();
    };
    say(&mut writer, "220 test.example.com ESMTP ready\r\n");
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let upper = line.trim_end().to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            say(
                &mut writer,
                "250-test.example.com Hello\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
            );
        } else if upper.starts_with("AUTH ") {
            say(&mut writer, "235 2.7.0 Authentication successful\r\n");
        } else if upper.starts_with("RCPT TO") {
            match stage {
                Stage::Rcpt => say(&mut writer, &format!("{reply}\r\n")),
                Stage::Data => say(&mut writer, "250 2.1.5 OK\r\n"),
            }
        } else if upper.starts_with("DATA") {
            say(&mut writer, "354 End data with <CR><LF>.<CR><LF>\r\n");
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line.trim_end_matches(['\r', '\n']) == "." {
                    break;
                }
            }
            say(&mut writer, &format!("{reply}\r\n"));
        } else if upper.starts_with("QUIT") {
            say(&mut writer, "221 2.0.0 Bye\r\n");
            return;
        } else {
            say(&mut writer, "250 OK\r\n");
        }
    }
}

fn send_against(stage: Stage, reply: &'static str) -> ProviderError {
    let server = ScriptedServer::start(stage, reply);
    let cfg = SmtpConfig {
        host: "127.0.0.1".into(),
        port: server.port,
        username: "alice@example.com".into(),
        password: "app-password".into(),
        use_tls: false,
        allow_invalid_certs: false,
    };
    let mut sender = LettreSmtpSender::new(&cfg).expect("build sender");
    sender
        .send(
            "alice@example.com",
            &["nobody@example.com".to_string()],
            b"Subject: test\r\n\r\nhello\r\n",
        )
        .expect_err("a refused message must surface as an error")
}

#[test]
fn an_unknown_recipient_is_a_permanent_rejection() {
    let err = send_against(
        Stage::Rcpt,
        "550 5.1.1 <nobody@example.com>: Recipient address rejected: User unknown",
    );
    assert!(matches!(err, ProviderError::Rejected(_)), "got {err:?}");
}

#[test]
fn a_policy_refusal_after_the_body_is_a_permanent_rejection() {
    let err = send_against(Stage::Data, "554 5.7.1 Message rejected as spam");
    assert!(matches!(err, ProviderError::Rejected(_)), "got {err:?}");
}

#[test]
fn a_transient_4xx_is_deferred_not_a_network_failure() {
    let err = send_against(Stage::Rcpt, "451 4.7.1 Greylisted, try again later");
    assert!(matches!(err, ProviderError::Deferred(_)), "got {err:?}");
}

#[test]
fn a_transient_4xx_after_the_body_is_deferred() {
    let err = send_against(Stage::Data, "452 4.3.1 Insufficient system storage");
    assert!(matches!(err, ProviderError::Deferred(_)), "got {err:?}");
}

#[test]
fn a_5xx_that_mentions_auth_stays_an_auth_failure() {
    let err = send_against(
        Stage::Rcpt,
        "550 5.7.1 Client does not have permission: authentication required",
    );
    assert!(matches!(err, ProviderError::Auth(_)), "got {err:?}");
}
