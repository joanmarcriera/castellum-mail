// Castellum - ImapProvider-level QRESYNC negotiation + delta wire tests (S30).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Own ScriptedServer, deliberately independent of gmail_wire.rs's and
// session_wire.rs's (see session_wire.rs's own doc comment for why) — this
// file drives the real ImapProvider::connect against a local TCP listener to
// prove CONDSTORE/QRESYNC negotiation end to end, not just Session's parsing.
// GreenMail has no QRESYNC support, so this is the only coverage for it.

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider};
use std::io::{BufRead, BufReader, Write};
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
        host: "127.0.0.1".to_string(),
        port,
        username: "pilot@example.com".to_string(),
        password: "app-password".to_string(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    })
    .expect("connect")
}

const CAPS_WITH_QRESYNC: &str = "* CAPABILITY IMAP4rev1 CONDSTORE QRESYNC ENABLE UIDPLUS MOVE\r\n";
const CAPS_WITHOUT_QRESYNC: &str = "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n";

#[test]
fn connect_negotiates_qresync_when_the_server_advertises_it() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                CAPS_WITH_QRESYNC.to_string(),
                "OK Success".to_string(),
            ),
            (
                "ENABLE",
                "* ENABLED CONDSTORE QRESYNC\r\n".to_string(),
                "OK Success".to_string(),
            ),
        ],
    });
    let provider = connect(server.port);
    assert!(provider.qresync_available());
}

#[test]
fn connect_leaves_qresync_unavailable_when_the_server_lacks_it() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                CAPS_WITHOUT_QRESYNC.to_string(),
                "OK Success".to_string(),
            ),
        ],
    });
    let provider = connect(server.port);
    assert!(!provider.qresync_available());
}

#[test]
fn select_qresync_is_refused_when_the_server_never_negotiated_it() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                CAPS_WITHOUT_QRESYNC.to_string(),
                "OK Success".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let err = provider
        .select_qresync("INBOX", 1, 100)
        .expect_err("must refuse without negotiation");
    assert!(err.to_string().contains("did not negotiate"));
}

#[test]
fn select_qresync_applies_the_vanished_and_changed_delta() {
    let select_reply = "\
* 3 EXISTS\r\n\
* OK [UIDVALIDITY 5] UIDs valid\r\n\
* OK [UIDNEXT 40] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 700] Highest\r\n\
* VANISHED (EARLIER) 10,12\r\n\
* 3 FETCH (UID 30 FLAGS (\\Seen))\r\n";
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                CAPS_WITH_QRESYNC.to_string(),
                "OK Success".to_string(),
            ),
            (
                "ENABLE",
                "* ENABLED CONDSTORE QRESYNC\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                select_reply.to_string(),
                "OK [READ-WRITE] SELECT completed".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let result = provider
        .select_qresync("INBOX", 5, 690)
        .expect("select_qresync");
    assert_eq!(result.state.uid_validity, 5);
    assert_eq!(result.state.exists, 3);
    assert_eq!(result.highest_modseq, Some(700));
    assert_eq!(result.vanished.ranges(), &[10..=10, 12..=12]);
    assert_eq!(result.changed.len(), 1);
    assert_eq!(result.changed[0].uid, 30);
    assert!(result.changed[0].is_seen);
    assert_eq!(provider.last_highest_modseq(), Some(700));
}

/// The provider-level half of the bounded-VANISHED fix: a whole-UID-space
/// range crosses the `MailProvider` boundary as one range, not ~4.3 billion
/// UIDs, so neither the session nor the sync engine ever expands it.
#[test]
fn select_qresync_passes_a_whole_uid_space_vanished_range_through_as_one_range() {
    let select_reply = "\
* 1 EXISTS\r\n\
* OK [UIDVALIDITY 5] UIDs valid\r\n\
* OK [UIDNEXT 40] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 700] Highest\r\n\
* VANISHED (EARLIER) 1:4294967295\r\n";
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                CAPS_WITH_QRESYNC.to_string(),
                "OK Success".to_string(),
            ),
            (
                "ENABLE",
                "* ENABLED CONDSTORE QRESYNC\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                select_reply.to_string(),
                "OK [READ-WRITE] SELECT completed".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let result = provider
        .select_qresync("INBOX", 5, 690)
        .expect("select_qresync");
    assert_eq!(result.vanished.ranges().len(), 1);
    assert_eq!(result.vanished.ranges()[0], 1..=4294967295);
    assert!(result.vanished.contains(39));
}
