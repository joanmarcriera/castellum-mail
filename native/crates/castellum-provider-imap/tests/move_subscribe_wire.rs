// Castellum - move destination SUBSCRIBE wire test (#3017).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A move whose destination did not exist makes `ImapProvider::move_message`
// CREATE it. Sync only picks up unsubscribed archive mailboxes when the user
// selects them, so a CREATEd-but-unsubscribed Archive made archived mail
// vanish from the app. The move must SUBSCRIBE a mailbox it just created, and
// must not touch the subscription of one that already existed. Same scripted
// server harness as expunge_trash_wire.rs (deliberately a separate copy).

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

fn script(create_reply: &str) -> Script {
    Script {
        greeting: "* OK Test ready\r\n".to_string(),
        replies: vec![
            ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
            (
                "CAPABILITY",
                "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n".to_string(),
                "OK Success".to_string(),
            ),
            (
                "SELECT",
                "* 3 EXISTS\r\n* OK [UIDVALIDITY 42] UIDs valid.\r\n".to_string(),
                "OK [READ-WRITE] selected".to_string(),
            ),
            (
                "UID SEARCH",
                "* SEARCH 7\r\n".to_string(),
                "OK done".to_string(),
            ),
            ("CREATE", String::new(), create_reply.to_string()),
        ],
    }
}

#[test]
fn move_subscribes_a_destination_it_just_created() {
    let server = ScriptedServer::start(script("OK CREATE completed"));
    let mut provider = connect(server.port);
    provider
        .move_message("INBOX", 7, "", "Archive")
        .expect("move");
    let _ = provider.logout();
    let received = server.received();
    let create = received
        .iter()
        .position(|c| c.starts_with("CREATE"))
        .expect("CREATE sent");
    let sub = received
        .iter()
        .position(|c| c.to_ascii_uppercase().starts_with("SUBSCRIBE"))
        .expect("SUBSCRIBE sent after CREATE");
    assert!(create < sub, "{received:?}");
}

#[test]
fn move_leaves_the_subscription_of_an_existing_destination_alone() {
    let server = ScriptedServer::start(script("NO [ALREADYEXISTS] Mailbox already exists"));
    let mut provider = connect(server.port);
    provider
        .move_message("INBOX", 7, "", "Archive")
        .expect("move");
    let _ = provider.logout();
    assert!(
        !server
            .received()
            .iter()
            .any(|c| c.to_ascii_uppercase().starts_with("SUBSCRIBE")),
        "{:?}",
        server.received()
    );
}
