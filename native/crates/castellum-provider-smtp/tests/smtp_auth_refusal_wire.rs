// Castellum - SMTP AUTH refusal wire test, scripted end to end (MS1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drives the real lettre `SmtpTransport` (via `LettreSmtpSender`) against a
// local scripted TCP server that refuses AUTH PLAIN/LOGIN with a chosen
// single-line 535 reply. Proves that Microsoft's "SMTP AUTH is switched off"
// refusal (`535 5.7.139 ... SmtpClientAuthentication is disabled`, the tracker
// 2968) reaches the caller as `ProviderError::AuthSmtpDisabled`, while an
// ordinary rejected credential - including Exchange's reuse of 5.7.139 for
// one - stays `ProviderError::Auth`. Never talks to real Microsoft.

use castellum_provider_smtp::LettreSmtpSender;
use castellum_providers::{ProviderError, SmtpConfig, SmtpSender};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// The 535 reply a plain rejected-credentials failure gets (RFC 4954).
const REPLY_BAD_CREDENTIALS: &str = "535 5.7.8 Authentication failed";

/// The exact reply a real Outlook.com account got on 2026-10-05 from both
/// smtp.office365.com and smtp-mail.outlook.com (port 587, STARTTLS).
const REPLY_SMTP_AUTH_DISABLED_MAILBOX: &str = "535 5.7.139 Authentication unsuccessful, SmtpClientAuthentication is disabled for the Mailbox. Visit https://aka.ms/smtp_auth_disabled";

/// The organisation-wide form of the same refusal.
const REPLY_SMTP_AUTH_DISABLED_TENANT: &str = "535 5.7.139 Authentication unsuccessful, SmtpClientAuthentication is disabled for the Tenant. Visit https://aka.ms/smtp_auth_disabled for more information.";

/// 5.7.139 is also what Exchange Online answers for a rejected credential;
/// that one must stay an ordinary auth failure.
const REPLY_5_7_139_BAD_CREDENTIALS: &str =
    "535 5.7.139 Authentication unsuccessful, the user credentials were incorrect.";

/// A one-connection SMTP server that answers EHLO, then refuses the first
/// AUTH command with `reply` and hangs up, as a real server does after a
/// hard authentication failure.
struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedServer {
    fn start(reply: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream, reply);
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

fn serve(stream: TcpStream, reply: &str) {
    // A broken client must fail the test, not hang the suite.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(b"220 test.example.com ESMTP ready\r\n");
    let _ = writer.flush();

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let upper = line.trim_end().to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            let _ = writer.write_all(
                b"250-test.example.com Hello\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n",
            );
        } else if upper.starts_with("AUTH ") {
            let _ = writer.write_all(format!("{reply}\r\n").as_bytes());
            let _ = writer.flush();
            return;
        } else if upper.starts_with("QUIT") {
            let _ = writer.write_all(b"221 2.0.0 Bye\r\n");
            let _ = writer.flush();
            return;
        } else {
            let _ = writer.write_all(b"250 OK\r\n");
        }
        let _ = writer.flush();
    }
}

fn cfg(port: u16) -> SmtpConfig {
    SmtpConfig {
        host: "127.0.0.1".into(),
        port,
        username: "alice@outlook.com".into(),
        password: "app-password".into(),
        use_tls: false,
        allow_invalid_certs: false,
    }
}

/// Sends one message against a server that refuses AUTH with `reply` and
/// returns the typed error.
fn send_refused_with(reply: &'static str) -> ProviderError {
    let server = ScriptedServer::start(reply);
    let mut sender = LettreSmtpSender::new(&cfg(server.port)).expect("build sender");
    sender
        .send(
            "alice@outlook.com",
            &["bob@example.com".to_string()],
            b"Subject: test\r\n\r\nhello\r\n",
        )
        .expect_err("a 535 auth failure must surface as an error, not succeed")
}

#[test]
fn a_rejected_password_surfaces_as_the_typed_auth_error() {
    let err = send_refused_with(REPLY_BAD_CREDENTIALS);
    assert!(
        matches!(err, ProviderError::Auth(_)),
        "expected ProviderError::Auth, got {err:?}"
    );
}

#[test]
fn smtp_auth_disabled_for_the_mailbox_is_its_own_error_kind() {
    let err = send_refused_with(REPLY_SMTP_AUTH_DISABLED_MAILBOX);
    assert!(
        matches!(err, ProviderError::AuthSmtpDisabled(_)),
        "expected ProviderError::AuthSmtpDisabled, got {err:?}"
    );
}

#[test]
fn smtp_auth_disabled_for_the_tenant_is_the_same_kind() {
    let err = send_refused_with(REPLY_SMTP_AUTH_DISABLED_TENANT);
    assert!(
        matches!(err, ProviderError::AuthSmtpDisabled(_)),
        "expected ProviderError::AuthSmtpDisabled, got {err:?}"
    );
}

#[test]
fn a_5_7_139_for_rejected_credentials_stays_an_ordinary_auth_failure() {
    // Only the "SmtpClientAuthentication is disabled" text is the setting.
    let err = send_refused_with(REPLY_5_7_139_BAD_CREDENTIALS);
    assert!(
        matches!(err, ProviderError::Auth(_)),
        "expected ProviderError::Auth, got {err:?}"
    );
}

#[test]
fn test_connection_reports_smtp_auth_disabled_too() {
    // The account sheet's Test Connection runs the same AUTH; it must be
    // able to explain the setting before the first send fails.
    let server = ScriptedServer::start(REPLY_SMTP_AUTH_DISABLED_MAILBOX);
    let sender = LettreSmtpSender::new(&cfg(server.port)).expect("build sender");
    let err = sender
        .test_connection()
        .expect_err("a refused AUTH must fail the preflight");
    assert!(
        matches!(err, ProviderError::AuthSmtpDisabled(_)),
        "expected ProviderError::AuthSmtpDisabled, got {err:?}"
    );
}
