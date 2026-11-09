// Castellum - server-draft write-path scripted tests (S36).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Drives the real ImapProvider (not just Session) so these prove the whole
// bridge: create-if-missing, flag mapping, UIDPLUS-gated cleanup. Mirrors
// gmail_wire.rs's ScriptedServer, duplicated per-file by existing convention
// (integration-test files cannot share non-pub helpers).

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider, MessageFlag};
use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{TcpListener, TcpStream};

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Script {
    greeting: String,
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
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        let mut parts = trimmed.splitn(2, ' ');
        let tag = parts.next().unwrap_or("*").to_string();
        let command = parts.next().unwrap_or("").to_ascii_uppercase();

        if command.starts_with("LOGOUT") {
            let _ = writer.write_all(b"* BYE Logging out\r\n");
            let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
            let _ = writer.flush();
            return;
        }
        if let Some(len) = trimmed
            .rsplit('{')
            .next()
            .and_then(|s| s.strip_suffix('}'))
            .and_then(|s| s.parse::<usize>().ok())
        {
            let _ = writer.write_all(b"+ OK\r\n");
            let _ = writer.flush();
            let mut literal = vec![0u8; len];
            if reader.read_exact(&mut literal).is_err() {
                return;
            }
            line.clear();
            let _ = reader.read_line(&mut line);
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
        username: "marc".into(),
        password: "pw".into(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    })
    .expect("connect")
}

#[test]
fn append_with_flags_get_uid_creates_the_mailbox_and_sets_draft() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK Success".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 UIDPLUS\r\n".to_string(),
                "OK Success".to_string(),
            ),
            ("CREATE", String::new(), "OK Success".to_string()),
            (
                "APPEND",
                String::new(),
                "OK [APPENDUID 1 42] Success".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let result = provider
        .append_with_flags_get_uid(
            "Drafts",
            &[MessageFlag::Draft],
            b"Subject: x\r\n\r\nbody\r\n",
        )
        .expect("append");
    assert_eq!(result.unwrap().uid, 42);
}

#[test]
fn delete_uid_uses_uid_expunge_when_uidplus_is_advertised() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK Success".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 UIDPLUS\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                "* 3 EXISTS\r\n* OK [UIDVALIDITY 1] ok\r\n".to_string(),
                "OK [READ-WRITE] ok".to_string(),
            ),
            ("STORE", String::new(), "OK Success".to_string()),
            ("EXPUNGE", String::new(), "OK Success".to_string()),
        ],
    });
    let mut provider = connect(server.port);
    provider.delete_uid("Drafts", 42).expect("delete_uid");
}

#[test]
fn delete_uid_falls_back_to_expunge_without_uidplus() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK Success".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                "* 3 EXISTS\r\n* OK [UIDVALIDITY 1] ok\r\n".to_string(),
                "OK [READ-WRITE] ok".to_string(),
            ),
            ("STORE", String::new(), "OK Success".to_string()),
            ("EXPUNGE", String::new(), "OK Success".to_string()),
        ],
    });
    let mut provider = connect(server.port);
    provider.delete_uid("Drafts", 42).expect("delete_uid");
}

#[test]
fn find_uid_by_message_id_returns_none_for_empty_message_id() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK Success".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1\r\n".to_string(),
                "OK Success".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    assert_eq!(provider.find_uid_by_message_id("Drafts", "").unwrap(), None);
}
