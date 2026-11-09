// Castellum - attachment partial-fetch wire test (S32).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Proves ImapProvider::fetch_attachment_part sends the right
// UID FETCH ... BODY.PEEK[<section>] command and decodes the returned
// section's bytes, using the same scripted-TCP-server pattern as
// gmail_wire.rs (deliberately a separate copy of the harness, not shared,
// matching that file's own precedent).

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider, ProviderError};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Script {
    greeting: String,
    /// (command prefix to match, untagged payload, tagged-OK trailer)
    replies: Vec<(&'static str, String, String)>,
}

impl Script {
    fn reply_for(&self, command: &str) -> Option<(String, String)> {
        self.replies
            .iter()
            .find(|(k, _, _)| command.starts_with(k))
            .map(|(_, untagged, ok)| (untagged.clone(), ok.clone()))
    }
}

impl ScriptedServer {
    fn start(script: Script) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, script);
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

fn serve(stream: TcpStream, script: Script) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(script.greeting.as_bytes());
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
        let command = parts.next().unwrap_or("").to_ascii_uppercase();

        if command.starts_with("LOGOUT") {
            let _ = writer.write_all(b"* BYE Logging out\r\n");
            let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
            let _ = writer.flush();
            return;
        }

        match script.reply_for(&command) {
            Some((untagged, ok)) => {
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

#[test]
fn fetch_attachment_part_sends_body_peek_section_and_returns_bytes() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "UID FETCH 7 BODY.PEEK[2.2]",
                "* 1 FETCH (UID 7 BODY[2.2] {5}\r\nhello)\r\n".to_string(),
                "OK UID FETCH completed".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let bytes = provider
        .fetch_attachment_part(7, "2.2")
        .expect("fetch must succeed");
    assert_eq!(bytes, b"hello");
    let _ = provider.logout();
}

#[test]
fn fetch_attachment_part_with_no_server_data_is_a_typed_error_not_a_panic() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "UID FETCH",
                String::new(),
                "OK UID FETCH completed".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let err = provider
        .fetch_attachment_part(999, "2.2")
        .expect_err("no FETCH data must be a typed error");
    assert!(matches!(err, ProviderError::Protocol(_)));
    let _ = provider.logout();
}

#[test]
fn fetch_attachment_part_rejects_malformed_section_without_panicking() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![("A1 LOGIN", String::new(), "OK LOGIN completed".to_string())],
    });
    let mut provider = connect(server.port);
    let err = provider
        .fetch_attachment_part(1, "not-a-number")
        .expect_err("malformed section must error, not panic");
    assert!(matches!(err, ProviderError::Protocol(_)));
    let _ = provider.logout();
}
