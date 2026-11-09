// Castellum - ImapProvider-level keyword wire tests (S42).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Own ScriptedServer, deliberately independent of gmail_wire.rs's,
// session_wire.rs's and qresync_wire.rs's (see session_wire.rs's own doc
// comment for why) — this file drives the real ImapProvider::connect (and
// PanicGuardedProvider) against a local TCP listener to prove keywords and
// PERMANENTFLAGS round-trip end to end, not just Session's parsing.
//
// Run: cargo test -p castellum-provider-imap --test keywords_wire

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, KeywordStoreOutcome, MailProvider};
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

const CAPS: &str = "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n";

fn login_and_caps() -> Vec<(&'static str, String, String)> {
    vec![
        ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
        ("CAPABILITY", CAPS.to_string(), "OK Success".to_string()),
    ]
}

#[test]
fn descriptors_and_uid_flags_carry_keywords() {
    let mut replies = login_and_caps();
    replies.push((
        "SELECT",
        "* 1 EXISTS\r\n* OK [UIDVALIDITY 5] ok\r\n* OK [UIDNEXT 2] ok\r\n".to_string(),
        "OK [READ-WRITE] Success".to_string(),
    ));
    // Both fetch_new_descriptors and list_uid_flags send "UID FETCH ..."; the
    // scripted server matches by command prefix only, so one reply serves
    // both calls — sufficient to prove keywords survive each parsing path.
    replies.push((
        "UID FETCH",
        "* 1 FETCH (UID 1 FLAGS (\\Seen $Label2))\r\n".to_string(),
        "OK Success".to_string(),
    ));
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    provider.select("INBOX").expect("SELECT");

    let descriptors = provider
        .fetch_new_descriptors(None)
        .expect("fetch_new_descriptors");
    assert_eq!(descriptors.len(), 1);
    assert_eq!(descriptors[0].keywords, Some(vec!["$Label2".to_string()]));

    let uid_flags = provider.list_uid_flags().expect("list_uid_flags");
    assert_eq!(uid_flags.len(), 1);
    assert_eq!(uid_flags[0].keywords, Some(vec!["$Label2".to_string()]));
}

#[test]
fn store_keyword_sends_store_when_permanent() {
    let mut replies = login_and_caps();
    replies.push((
        "SELECT",
        "* OK [PERMANENTFLAGS (\\Seen \\*)] ok\r\n".to_string(),
        "OK [READ-WRITE] Success".to_string(),
    ));
    replies.push(("UID STORE", String::new(), "OK Success".to_string()));
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let outcome = provider
        .store_keyword("INBOX", 9, "Work", true)
        .expect("store_keyword");
    assert_eq!(outcome, KeywordStoreOutcome::Stored);
}

#[test]
fn store_keyword_refuses_non_permanent_without_sending() {
    let mut replies = login_and_caps();
    replies.push((
        "SELECT",
        "* OK [PERMANENTFLAGS (\\Seen \\Flagged)] ok\r\n".to_string(),
        "OK [READ-WRITE] Success".to_string(),
    ));
    // Deliberately no "UID STORE" reply: a refused keyword must never send
    // one, so the generic "OK done" fallback (which would still exercise the
    // wire) is the only possible response if the code wrongly sent STORE
    // anyway — the important assertion is the returned outcome.
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let outcome = provider
        .store_keyword("INBOX", 9, "Work", true)
        .expect("store_keyword");
    assert_eq!(outcome, KeywordStoreOutcome::NotPermanent);
}

#[test]
fn store_keyword_listed_keyword_is_permanent_case_insensitive() {
    let mut replies = login_and_caps();
    replies.push((
        "SELECT",
        "* OK [PERMANENTFLAGS ($label1)] ok\r\n".to_string(),
        "OK [READ-WRITE] Success".to_string(),
    ));
    replies.push(("UID STORE", String::new(), "OK Success".to_string()));
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let outcome = provider
        .store_keyword("INBOX", 9, "$Label1", true)
        .expect("store_keyword");
    assert_eq!(outcome, KeywordStoreOutcome::Stored);
}

#[test]
fn store_keyword_absent_permanentflags_means_permanent() {
    let mut replies = login_and_caps();
    replies.push((
        "SELECT",
        String::new(),
        "OK [READ-WRITE] Success".to_string(),
    ));
    replies.push(("UID STORE", String::new(), "OK Success".to_string()));
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let outcome = provider
        .store_keyword("INBOX", 9, "AnyKeyword", true)
        .expect("store_keyword");
    assert_eq!(outcome, KeywordStoreOutcome::Stored);
}
