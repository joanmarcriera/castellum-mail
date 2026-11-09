// Castellum - Undo's message locator, against a scripted server.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `ImapProvider::locate_by_message_id` must return only messages whose REAL
// Message-ID header equals the target. IMAP `SEARCH HEADER` is a substring
// match, so a crafted mail whose Message-ID merely CONTAINS the target's would
// otherwise be picked up (and then moved by Undo). Same scripted-server
// pattern as attachment_fetch_wire.rs (a deliberate copy of the harness).

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

fn envelope(uid: u32, seq: u32, message_id: &str) -> String {
    format!(
        "* {seq} FETCH (UID {uid} ENVELOPE (NIL NIL NIL NIL NIL NIL NIL NIL NIL \"{message_id}\"))\r\n"
    )
}

fn locate_server(search: &str, fetched: String) -> ScriptedServer {
    ScriptedServer::start(Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "UID SEARCH",
                format!("* SEARCH {search}\r\n"),
                "OK done".to_string(),
            ),
            ("UID FETCH", fetched, "OK done".to_string()),
        ],
    })
}

#[test]
fn locate_ignores_decoys_whose_message_id_merely_contains_the_target() {
    let fetched = [
        envelope(5, 1, "<xtarget@x.test>"), // superstring of the local part
        envelope(6, 2, "<target@X.TEST>"),  // the real one (domain case differs)
        envelope(7, 3, "<target@x.test.evil>"), // superstring of the domain
        envelope(8, 4, "<target@y.test>"),  // different domain
    ]
    .concat();
    let server = locate_server("5 6 7 8", fetched);
    let mut provider = connect(server.port);
    let found = provider
        .locate_by_message_id("Archive", "<target@x.test>")
        .expect("locate");
    assert_eq!(found, vec![6], "only the exact Message-ID may be returned");
    let _ = provider.logout();
}

#[test]
fn locate_reports_every_exact_duplicate_so_the_caller_can_refuse_to_guess() {
    let fetched = [
        envelope(6, 1, "<target@x.test>"),
        envelope(9, 2, "<target@x.test>"),
        envelope(7, 3, "<decoy-target@x.test>"),
    ]
    .concat();
    let server = locate_server("6 7 9", fetched);
    let mut provider = connect(server.port);
    let found = provider
        .locate_by_message_id("Archive", "<target@x.test>")
        .expect("locate");
    assert_eq!(found, vec![6, 9]);
    let _ = provider.logout();
}

#[test]
fn locate_with_only_decoys_finds_nothing() {
    let server = locate_server("5", envelope(5, 1, "<xtarget@x.test>"));
    let mut provider = connect(server.port);
    let found = provider
        .locate_by_message_id("Archive", "<target@x.test>")
        .expect("locate");
    assert!(found.is_empty());
    let _ = provider.logout();
}

/// Review M1: the server-draft cleanup expunges every match but the newest,
/// and records `find_uid_by_message_id`'s hit as the mirror UID it later
/// expunges. Both must ignore substring decoys exactly as Undo's locator does.
#[test]
fn draft_lookups_ignore_decoys_whose_message_id_merely_contains_the_target() {
    let fetched = || {
        [
            envelope(5, 1, "<xtarget@x.test>"),
            envelope(6, 2, "<target@x.test>"),
            envelope(9, 3, "<target@x.test.evil>"),
        ]
        .concat()
    };
    let server = locate_server("5 6 9", fetched());
    let mut provider = connect(server.port);
    let all = provider
        .find_uids_by_message_id("Drafts", "<target@x.test>")
        .expect("find all");
    assert_eq!(all.uids, vec![6], "decoys must never be queued for expunge");
    let _ = provider.logout();
    drop(server);

    let server = locate_server("5 6 9", fetched());
    let mut provider = connect(server.port);
    let newest = provider
        .find_uid_by_message_id("Drafts", "<target@x.test>")
        .expect("find newest")
        .expect("one exact match");
    assert_eq!(newest.uid, 6, "UID 9 is a decoy despite being the highest");
    let _ = provider.logout();
}
