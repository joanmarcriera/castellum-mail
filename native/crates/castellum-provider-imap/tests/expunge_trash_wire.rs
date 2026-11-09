// Castellum - Empty Trash snapshot-expunge wire test.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Proves `ImapProvider::expunge_mailbox` sends exactly the snapshotted UID
// set to UID STORE, then picks UID EXPUNGE when the server advertises
// UIDPLUS and falls back to plain EXPUNGE otherwise — the write-safety fix
// for the empty-trash data-loss gap found by the 1.0 readiness review (see
// `native/crates/castellum-core/src/replay.rs`'s `empty_trash` match arm and
// `DECISIONS.md`). Same scripted-TCP-server pattern as attachment_fetch_wire.rs
// (deliberately a separate copy of the harness, not shared).

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Every command line the server received (tag stripped), in order —
    /// lets a test assert the EXACT wire commands sent, not just "something
    /// succeeded".
    commands: Arc<Mutex<Vec<String>>>,
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
        let commands = Arc::new(Mutex::new(Vec::new()));
        let commands_clone = commands.clone();
        let handle = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            serve(stream, script, commands_clone);
        });
        ScriptedServer {
            port,
            handle: Some(handle),
            commands,
        }
    }

    fn received(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

impl Drop for ScriptedServer {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn serve(stream: TcpStream, script: Script, commands: Arc<Mutex<Vec<String>>>) {
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
        let rest = parts.next().unwrap_or("").to_string();
        let command = rest.to_ascii_uppercase();
        commands.lock().unwrap().push(rest);

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

/// A minimal SELECT response reporting the given UIDVALIDITY — unused by
/// `expunge_mailbox` itself (the UIDVALIDITY compare lives one layer up, in
/// `castellum_core::replay`), but every real server sends one, so the
/// fixture stays realistic.
fn select_reply() -> String {
    "* 3 EXISTS\r\n* OK [UIDVALIDITY 42] UIDs valid.\r\n".to_string()
}

#[test]
fn expunge_mailbox_with_uidplus_stores_then_uid_expunges_exactly_the_snapshot() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 UIDPLUS\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                select_reply(),
                "OK [READ-WRITE] Trash selected".to_string(),
            ),
            (
                "UID STORE",
                String::new(),
                "OK UID STORE completed".to_string(),
            ),
            (
                "UID EXPUNGE",
                String::new(),
                "OK UID EXPUNGE completed".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    provider
        .expunge_mailbox("Trash", "3,5:7,10")
        .expect("expunge must succeed");
    let _ = provider.logout();

    let received = server.received();
    let store = received
        .iter()
        .find(|c| c.to_ascii_uppercase().starts_with("UID STORE"))
        .expect("a UID STORE was sent");
    assert!(store.contains("3,5:7,10"), "STORE set: {store}");
    assert!(
        store.to_ascii_uppercase().contains("DELETED"),
        "STORE flag: {store}"
    );

    let expunge = received
        .iter()
        .find(|c| c.to_ascii_uppercase().starts_with("UID EXPUNGE"))
        .expect("UIDPLUS must use UID EXPUNGE, not plain EXPUNGE");
    assert!(expunge.contains("3,5:7,10"), "EXPUNGE set: {expunge}");
    assert!(
        !received.iter().any(|c| c.eq_ignore_ascii_case("EXPUNGE")),
        "must never also send a bare EXPUNGE when UIDPLUS is available: {received:?}"
    );
}

/// PR #165 review point 3: a plain `EXPUNGE` fallback removes every
/// `\Deleted` message in the mailbox, not just the snapshot — an
/// unacceptable residual for a destructive op. Without UIDPLUS,
/// `expunge_mailbox` must refuse outright (matching `move_message`'s own
/// MOVE/UIDPLUS refusal a few lines up in lib.rs) and must send NEITHER a
/// STORE NOR any form of EXPUNGE — nothing may be flagged `\Deleted` on a
/// server we already know we can't safely finish the job on.
#[test]
fn expunge_mailbox_without_uidplus_refuses_and_sends_no_store_or_expunge() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1\r\n".to_string(), // no UIDPLUS
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                select_reply(),
                "OK [READ-WRITE] Trash selected".to_string(),
            ),
        ],
    });
    let mut provider = connect(server.port);
    let err = provider
        .expunge_mailbox("Trash", "9")
        .expect_err("must refuse without UIDPLUS, never fall back to a bare EXPUNGE");
    assert!(
        matches!(err, castellum_providers::ProviderError::Unsupported(_)),
        "must be classified Unsupported so replay treats it as non-retryable: {err:?}"
    );
    let _ = provider.logout();

    let received = server.received();
    assert!(
        !received
            .iter()
            .any(|c| c.to_ascii_uppercase().starts_with("UID STORE")),
        "must never flag anything \\Deleted when the refusal happens first: {received:?}"
    );
    assert!(
        !received.iter().any(|c| {
            let up = c.to_ascii_uppercase();
            up.eq_ignore_ascii_case("EXPUNGE") || up.starts_with("UID EXPUNGE")
        }),
        "must send no EXPUNGE of any form without UIDPLUS: {received:?}"
    );
}
