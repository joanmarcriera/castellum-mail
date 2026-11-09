// Castellum - ImapProvider-level IMAP IDLE wire tests (S31).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Own ScriptedServer (see session_wire.rs's doc comment for why each wire
// test file keeps its own copy rather than sharing one).

use castellum_provider_imap::ImapProvider;
use castellum_providers::{IdleWake, ImapConfig, MailProvider, SessionAbandonReason};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Script {
    greeting: String,
    replies: Vec<(&'static str, String, String)>,
    /// Bytes to push right after IDLE's `+ idling` continuation, before
    /// waiting for DONE — simulates server-pushed activity while idling.
    idle_push: Option<&'static str>,
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
        if command.starts_with("IDLE") {
            let _ = writer.write_all(b"+ idling\r\n");
            if let Some(push) = script.idle_push {
                let _ = writer.write_all(push.as_bytes());
            }
            let _ = writer.flush();
            line.clear();
            let _ = reader.read_line(&mut line); // bare "DONE"
            let _ = writer.write_all(format!("{tag} OK IDLE completed\r\n").as_bytes());
            let _ = writer.flush();
            continue;
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

const CAPS_WITH_IDLE: &str = "* CAPABILITY IMAP4rev1 IDLE UIDPLUS MOVE\r\n";
const CAPS_WITHOUT_IDLE: &str = "* CAPABILITY IMAP4rev1 UIDPLUS MOVE\r\n";

fn base_replies(caps: &str) -> Vec<(&'static str, String, String)> {
    vec![
        ("LOGIN", String::new(), "OK LOGIN completed".to_string()),
        ("CAPABILITY", caps.to_string(), "OK Success".to_string()),
        (
            "SELECT",
            "* 0 EXISTS\r\n".to_string(),
            "OK [READ-WRITE] SELECT completed".to_string(),
        ),
    ]
}

#[test]
fn idle_supported_reflects_the_capability() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITH_IDLE),
        idle_push: None,
    });
    let provider = connect(server.port);
    assert!(provider.idle_supported());

    let server2 = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITHOUT_IDLE),
        idle_push: None,
    });
    let provider2 = connect(server2.port);
    assert!(!provider2.idle_supported());
}

#[test]
fn idle_wait_reports_activity_when_the_server_pushes_an_exists() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITH_IDLE),
        idle_push: Some("* 5 EXISTS\r\n"),
    });
    let mut provider = connect(server.port);
    provider.idle_start("INBOX").expect("idle_start");
    let wake = provider
        .idle_wait(Duration::from_secs(2))
        .expect("idle_wait");
    assert_eq!(wake, IdleWake::Activity);
    provider.idle_done().expect("idle_done");
}

#[test]
fn idle_wait_reports_timeout_when_nothing_arrives() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITH_IDLE),
        idle_push: None,
    });
    let mut provider = connect(server.port);
    provider.idle_start("INBOX").expect("idle_start");
    let wake = provider
        .idle_wait(Duration::from_millis(200))
        .expect("idle_wait");
    assert_eq!(wake, IdleWake::Timeout);
    provider.idle_done().expect("idle_done");
}

// REQUIRED BY THE S30 REVIEW (2026-09-25), fixed in S31 (2026-09-25 retry):
// once QRESYNC is enabled, a server reports expunges as `* VANISHED
// <uid-set>` during IDLE instead of `* n EXPUNGE`. `ImapProvider::idle_wait`
// is deliberately coarse (it can only report `IdleWake::Activity`/`Timeout`,
// per `IdleWake`'s doc comment — the precise "what changed" always comes
// from the following incremental resync, never from the raw IDLE
// notification itself), so THIS test only proves a VANISHED line is
// decoded at all here (not silently dropped as `UntaggedItem::Other`, which
// would leave the resync never woken) and reported as Activity like any
// other server push — it cannot and does not distinguish a decoded
// `Vanished` from a dropped `Other` by itself. The two facts that close
// that gap live elsewhere, for real (not just claimed):
// `Session`-level decode of the actual UID ranges is
// `session_wire::idle_wait_decodes_a_vanished_push_into_uid_ranges` (this
// crate). The full end-to-end path — a mid-IDLE VANISHED, through the real
// `run_account_idle` thread, through `record_and_run_sync`'s own QRESYNC
// delta, to the message actually being removed from the local DB — is
// `castellum_core::idle`'s
// `a_vanished_wake_mid_idle_causes_the_incremental_resync_to_remove_the_message`.
#[test]
fn idle_wait_reports_activity_when_a_vanished_arrives_mid_idle() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITH_IDLE),
        idle_push: Some("* VANISHED 7\r\n"),
    });
    let mut provider = connect(server.port);
    provider.idle_start("INBOX").expect("idle_start");
    let wake = provider
        .idle_wait(Duration::from_secs(2))
        .expect("idle_wait");
    assert_eq!(wake, IdleWake::Activity);
    provider.idle_done().expect("idle_done");
}

// Merge of main into S31, 2026-09-26: a server BYE
// while idling closes the connection, so the IDLE loop's session reads as
// abandoned with reason `Closed` — which the sync engine records as a
// "network" account failure, not "protocol". Pins that
// `ImapProvider::session_abandon_reason` covers the IDLE path this PR added
// (it runs outside `Session::command`, so it marks the session closed itself).
#[test]
fn a_bye_while_idling_abandons_the_session_as_closed() {
    let server = ScriptedServer::start(Script {
        greeting: "* OK test ready\r\n".to_string(),
        replies: base_replies(CAPS_WITH_IDLE),
        idle_push: Some("* BYE server shutting down\r\n"),
    });
    let mut provider = connect(server.port);
    provider.idle_start("INBOX").expect("idle_start");
    assert!(!provider.session_abandoned(), "idling is not abandoned");
    assert_eq!(provider.session_abandon_reason(), None);

    provider
        .idle_wait(Duration::from_secs(2))
        .expect_err("a BYE mid-IDLE is an error, not a wake");
    assert!(provider.session_abandoned());
    assert_eq!(
        provider.session_abandon_reason(),
        Some(SessionAbandonReason::Closed)
    );
}
