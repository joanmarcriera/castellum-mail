// Castellum - Gmail wire-format regression fixtures.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// GreenMail cannot emit the responses a real Gmail server does (SPECIAL-USE
// LIST attributes, `[Gmail]` \Noselect containers, literal-encoded ENVELOPE
// fields, deeply nested BODYSTRUCTURE, unsolicited FETCH/EXPUNGE mid-response).
// These tests drive the real `ImapProvider` against a scripted TCP server that
// replays captured Gmail-shaped bytes, so provider-data crashes are reproduced
// deterministically and offline.

use castellum_provider_imap::ImapProvider;
use castellum_providers::{
    ImapConfig, MailProvider, PanicGuardedProvider, ProviderError, SessionAbandonReason,
};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// A scripted IMAP server. `responses` maps an uppercased command keyword to the
/// untagged payload sent before the tagged `OK`.
struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Script {
    greeting: String,
    /// (command keyword, untagged lines, tagged-OK trailer)
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
        if std::env::var_os("WIRE_DEBUG").is_some() {
            eprintln!("C: {trimmed}");
        }
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
                if std::env::var_os("WIRE_DEBUG").is_some() {
                    eprintln!("S: {untagged}{tag} {ok}");
                }
                let _ = writer.write_all(untagged.as_bytes());
                let _ = writer.write_all(format!("{tag} {ok}\r\n").as_bytes());
            }
            None => {
                if std::env::var_os("WIRE_DEBUG").is_some() {
                    eprintln!("S: {tag} OK done (fallback)");
                }
                let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
            }
        }
        let _ = writer.flush();
    }
}

fn gmail_config(port: u16) -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".to_string(),
        port,
        username: "pilot@gmail.com".to_string(),
        password: "app-password".to_string(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    }
}

/// Gmail's real CAPABILITY / NAMESPACE / LIST / LSUB responses.
fn gmail_prelude() -> Vec<(&'static str, String, String)> {
    let caps = "* CAPABILITY IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST \
                CHILDREN X-GM-EXT-1 UIDPLUS COMPRESS=DEFLATE ENABLE MOVE \
                CONDSTORE ESEARCH UTF8=ACCEPT LIST-EXTENDED LIST-STATUS \
                LITERAL- SPECIAL-USE APPENDLIMIT=35651584\r\n";
    let list = "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                * LIST (\\HasChildren \\Noselect) \"/\" \"[Gmail]\"\r\n\
                * LIST (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n\
                * LIST (\\Drafts \\HasNoChildren) \"/\" \"[Gmail]/Drafts\"\r\n\
                * LIST (\\HasNoChildren \\Important) \"/\" \"[Gmail]/Important\"\r\n\
                * LIST (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n\
                * LIST (\\HasNoChildren \\Junk) \"/\" \"[Gmail]/Spam\"\r\n\
                * LIST (\\Flagged \\HasNoChildren) \"/\" \"[Gmail]/Starred\"\r\n\
                * LIST (\\HasNoChildren \\Trash) \"/\" \"[Gmail]/Trash\"\r\n";
    let lsub = list.replace("* LIST", "* LSUB");
    vec![
        ("CAPABILITY", caps.to_string(), "OK Success".to_string()),
        (
            "LOGIN",
            String::new(),
            "OK pilot@gmail.com authenticated (Success)".to_string(),
        ),
        (
            "NAMESPACE",
            "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n".to_string(),
            "OK Success".to_string(),
        ),
        ("LSUB", lsub, "OK Success".to_string()),
        ("LIST", list.to_string(), "OK Success".to_string()),
    ]
}

/// Gmail's SELECT response, including the CONDSTORE HIGHESTMODSEQ line.
fn gmail_select() -> (&'static str, String, String) {
    (
        "SELECT",
        "* FLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen $NotPhishing $Phishing)\r\n\
         * OK [PERMANENTFLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen $NotPhishing $Phishing \\*)] Flags permitted.\r\n\
         * OK [UIDVALIDITY 1] UIDs valid.\r\n\
         * 12345 EXISTS\r\n\
         * 0 RECENT\r\n\
         * OK [UIDNEXT 54321] Predicted next UID.\r\n\
         * OK [HIGHESTMODSEQ 1234567]\r\n"
            .to_string(),
        "OK [READ-WRITE] INBOX selected. (Success)".to_string(),
    )
}

fn connect(port: u16) -> ImapProvider {
    ImapProvider::connect(&gmail_config(port)).expect("connect to scripted Gmail")
}

#[test]
fn gmail_mailbox_list_is_parsed_with_special_use_roles() {
    let mut replies = gmail_prelude();
    replies.push(gmail_select());
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = connect(server.port);
    let boxes = provider.list_mailboxes().expect("list mailboxes");

    let inbox = boxes.iter().find(|m| m.path == "INBOX").expect("INBOX");
    assert_eq!(inbox.role.as_deref(), Some("inbox"));
    assert!(inbox.selectable);

    let container = boxes.iter().find(|m| m.path == "[Gmail]").expect("[Gmail]");
    assert!(
        !container.selectable,
        "\\Noselect container must not be selectable"
    );

    let all = boxes
        .iter()
        .find(|m| m.path == "[Gmail]/All Mail")
        .expect("All Mail");
    assert_eq!(all.role.as_deref(), Some("archive"));
    assert_eq!(all.name, "All Mail");

    let _ = provider.logout();
}

/// A real Gmail descriptor FETCH: RFC2047-encoded subject, literal-encoded
/// fields, angle-bracketed Message-ID, and a nested multipart BODYSTRUCTURE
/// carrying the extension fields Gmail always sends (GreenMail sends none).
fn gmail_descriptor_fetch() -> (&'static str, String, String) {
    let subject_literal = "Café réunió — \"quoted\" & <angled>";
    let fetch = format!(
        "* 1 FETCH (UID 12345 RFC822.SIZE 4523 FLAGS (\\Seen) \
         INTERNALDATE \"11-Jul-2026 08:12:03 +0100\" \
         ENVELOPE (\"Fri, 11 Jul 2026 08:12:03 +0100\" {{{len}}}\r\n{subject_literal} \
         ((\"Alice Smith\" NIL \"alice\" \"example.com\")) \
         ((\"Alice Smith\" NIL \"alice\" \"example.com\")) \
         ((\"Alice Smith\" NIL \"alice\" \"example.com\")) \
         ((\"Marc\" NIL \"alice\" \"example.com\")) NIL NIL \
         \"<CA+prev123@mail.gmail.com>\" \"<CA+abc123@mail.gmail.com>\") \
         BODYSTRUCTURE ((((\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL)\
         (\"text\" \"html\" (\"charset\" \"UTF-8\") NIL NIL \"quoted-printable\" 100 3 NIL NIL NIL NIL) \
         \"alternative\" (\"boundary\" \"000000000000a1b2c3\") NIL NIL NIL)\
         (\"image\" \"png\" (\"name\" \"inline.png\") \"<ii_abc>\" NIL \"base64\" 900 NIL \
         (\"inline\" (\"filename\" \"inline.png\")) NIL NIL) \
         \"related\" (\"boundary\" \"000000000000d4e5f6\") NIL NIL NIL)\
         (\"application\" \"pdf\" (\"name\" \"invoice.pdf\") NIL NIL \"base64\" 51234 NIL \
         (\"attachment\" (\"filename\" \"invoice.pdf\")) NIL NIL) \
         \"mixed\" (\"boundary\" \"000000000000112233\") NIL NIL NIL))\r\n",
        len = subject_literal.len(),
        subject_literal = subject_literal,
    );
    ("UID FETCH", fetch, "OK Success".to_string())
}

#[test]
fn gmail_descriptor_fetch_survives_real_envelope_and_bodystructure() {
    let mut replies = gmail_prelude();
    replies.push(gmail_select());
    replies.push(gmail_descriptor_fetch());
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = connect(server.port);
    // A first sync always lists mailboxes before selecting one.
    let _ = provider.list_mailboxes().expect("list mailboxes");
    provider.select("INBOX").expect("select INBOX");
    let descriptors = provider
        .fetch_new_descriptors(None)
        .expect("fetch descriptors");

    assert_eq!(descriptors.len(), 1, "one descriptor: {descriptors:?}");
    let d = &descriptors[0];
    assert_eq!(d.uid, 12345);
    assert_eq!(d.size_bytes, Some(4523));
    assert!(d.is_seen);
    assert!(!d.is_flagged);
    assert_eq!(
        d.subject.as_deref(),
        Some("Café réunió — \"quoted\" & <angled>")
    );
    assert_eq!(
        d.rfc822_message_id.as_deref(),
        Some("CA+abc123@mail.gmail.com")
    );
    assert_eq!(d.references, vec!["CA+prev123@mail.gmail.com".to_string()]);
    assert_eq!(
        d.from.as_ref().map(|a| a.address.as_str()),
        Some("alice@example.com")
    );
    assert_eq!(
        d.has_attachments,
        Some(true),
        "the PDF part is an attachment"
    );

    let _ = provider.logout();
}

// MARK: - panic-guarded provider: a crashing parser must not abort the sync

/// A server whose SELECT reply carries an untagged non-OK line. Real servers do
/// this (capacity or quota warnings); `imap` 2.4.1 `parse_mailbox` (parse.rs:210)
/// hits `unreachable!()` on it.
fn select_with_untagged_no() -> (&'static str, String, String) {
    (
        "SELECT",
        "* FLAGS (\\Seen)\r\n\
         * NO [ALERT] Mailbox is approaching its quota\r\n\
         * OK [UIDVALIDITY 1] UIDs valid.\r\n\
         * 1 EXISTS\r\n"
            .to_string(),
        "OK [READ-WRITE] INBOX selected.".to_string(),
    )
}

/// `INTERNALDATE NIL` — `imap-proto` 0.10.2 `msg_att_internal_date`
/// (rfc3501.rs:379) unwraps the `None` and panics inside the parser.
fn fetch_with_nil_internal_date() -> (&'static str, String, String) {
    (
        "UID FETCH",
        "* 1 FETCH (UID 7 RFC822.SIZE 10 FLAGS () INTERNALDATE NIL)\r\n".to_string(),
        "OK Success".to_string(),
    )
}

#[test]
fn a_select_reply_with_an_untagged_no_is_decoded_not_a_crash() {
    // Under imap-proto 0.10.2 this reply hit `unreachable!()` in
    // `parse_mailbox`; the guard turned that into an error. imap-codec decodes
    // an untagged `NO [ALERT]` as an ordinary status line, so SELECT succeeds
    // and the guard never has to trip.
    let mut replies = gmail_prelude();
    replies.push(select_with_untagged_no());
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = PanicGuardedProvider::new(connect(server.port));
    let state = provider
        .select("INBOX")
        .expect("an untagged NO [ALERT] must not break SELECT");
    assert_eq!(state.uid_validity, 1);
    assert_eq!(state.exists, 1);
    assert!(!provider.is_abandoned());
}

#[test]
fn an_undecodable_fetch_reply_is_a_typed_error_and_poisons_the_session() {
    // `INTERNALDATE NIL` panicked inside imap-proto's parser. imap-codec
    // returns a typed decode error instead — no unwind, so the panic guard is
    // not tripped — and Session poisons itself so the desynchronised socket
    // is never reused.
    let mut replies = gmail_prelude();
    replies.push(gmail_select());
    replies.push(fetch_with_nil_internal_date());
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = PanicGuardedProvider::new(connect(server.port));
    let _ = provider.list_mailboxes().expect("list mailboxes");
    provider.select("INBOX").expect("select INBOX");

    let err = provider
        .fetch_new_descriptors(None)
        .expect_err("a NIL INTERNALDATE must surface as an error");
    let ProviderError::Protocol(msg) = err else {
        panic!("expected a Protocol error")
    };
    assert!(msg.contains("could not be decoded"), "{msg}");
    assert!(!provider.is_abandoned(), "a typed error is not a panic");
    // Poisoned: the next call fails fast instead of touching the socket.
    assert!(provider.list_uid_flags().is_err());
    //: nothing tried to replace the poisoned session (this
    // path is not descriptor isolation), so it must still read as
    // "protocol" — the only cause that does.
    assert_eq!(
        provider.session_abandon_reason(),
        Some(SessionAbandonReason::Poisoned)
    );
}

// MARK: - a malformed BODYSTRUCTURE degrades one message, not the batch

/// The BODYSTRUCTURE Gmail sent for one message in the 2026-09-22 real-Gmail
/// pilot (addresses, subject and Message-IDs redacted; grammar, sizes and
/// nesting preserved). A multipart/report whose message/rfc822 part embeds
/// `("ALTERNATIVE" ("BOUNDARY" ...) NIL NIL)` — a multipart with ZERO
/// sub-parts, which RFC 3501's `body-type-mpart` (`1*body SP subtype`)
/// forbids. imap-codec rejected the whole FETCH line; imap-proto tolerated it.
const GMAIL_ZERO_PART_MULTIPART_BODYSTRUCTURE: &str = "BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 227 5 NIL NIL NIL)(\"MESSAGE\" \"DELIVERY-STATUS\" NIL NIL NIL \"7BIT\" 346 NIL NIL NIL)(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 2458 (\"Mon, 01 Jan 2026 00:00:00 +0000\" \"Test subject\" ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"recipient\" \"example.com\")) ((NIL NIL \"cc1\" \"example.com\")(NIL NIL \"cc2\" \"example.com\")) NIL \"<msgid1@example.com>\" \"<msgid2@example.com>\") (\"ALTERNATIVE\" (\"BOUNDARY\" \"boundary-marker\") NIL NIL) 50 NIL NIL NIL) \"REPORT\" (\"BOUNDARY\" \"outer-boundary-marker\" \"REPORT-TYPE\" \"delivery-status\") NIL NIL)";

/// A descriptor FETCH batch: a well-formed message with a PDF attachment, the
/// pilot's malformed message, and a well-formed plain-text message.
fn fetch_batch_with_malformed_bodystructure() -> (&'static str, String, String) {
    let envelope = |subject: &str, id: &str| {
        format!(
            "ENVELOPE (\"Mon, 01 Jan 2026 00:00:00 +0000\" \"{subject}\" \
             ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) \
             ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"recipient\" \"example.com\")) \
             NIL NIL NIL \"<{id}@example.com>\")"
        )
    };
    let fetch = format!(
        "* 75426 FETCH (UID 76872 FLAGS (\\Seen) RFC822.SIZE 60000 \
         INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" {before} \
         BODYSTRUCTURE ((\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL)\
         (\"application\" \"pdf\" (\"name\" \"invoice.pdf\") NIL NIL \"base64\" 51234 NIL \
         (\"attachment\" (\"filename\" \"invoice.pdf\")) NIL NIL) \
         \"mixed\" (\"boundary\" \"000000000000112233\") NIL NIL NIL))\r\n\
         * 75427 FETCH (UID 76873 FLAGS () RFC822.SIZE 3500 \
         INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" {bounce} \
         {bad})\r\n\
         * 75428 FETCH (UID 76874 FLAGS (\\Flagged) RFC822.SIZE 900 \
         INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" {after} \
         BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL))\r\n",
        before = envelope("Invoice", "before"),
        bounce = envelope("Undelivered Mail Returned to Sender", "bounce"),
        after = envelope("Plain note", "after"),
        bad = GMAIL_ZERO_PART_MULTIPART_BODYSTRUCTURE,
    );
    ("UID FETCH", fetch, "OK Success".to_string())
}

#[test]
fn a_malformed_bodystructure_degrades_only_its_own_message() {
    let mut replies = gmail_prelude();
    replies.push(gmail_select());
    replies.push(fetch_batch_with_malformed_bodystructure());
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = PanicGuardedProvider::new(connect(server.port));
    let _ = provider.list_mailboxes().expect("list mailboxes");
    provider.select("INBOX").expect("select INBOX");
    let descriptors = provider
        .fetch_new_descriptors(None)
        .expect("one malformed BODYSTRUCTURE must not fail the batch");

    let uids: Vec<i64> = descriptors.iter().map(|d| d.uid).collect();
    assert_eq!(uids, vec![76872, 76873, 76874], "{descriptors:?}");

    let before = &descriptors[0];
    assert_eq!(before.has_attachments, Some(true), "PDF neighbour decodes");
    assert!(before.is_seen);

    let bad = &descriptors[1];
    assert_eq!(bad.has_attachments, None, "unknown, not guessed");
    // Everything outside the dropped BODYSTRUCTURE still arrives.
    assert_eq!(bad.size_bytes, Some(3500));
    assert_eq!(
        bad.subject.as_deref(),
        Some("Undelivered Mail Returned to Sender")
    );
    assert_eq!(bad.rfc822_message_id.as_deref(), Some("bounce@example.com"));
    assert!(bad.sent_at.is_some());

    let after = &descriptors[2];
    assert_eq!(
        after.has_attachments,
        Some(false),
        "plain neighbour decodes"
    );
    assert!(after.is_flagged);

    // Not poisoned, and still in sync: the next command on the same session
    // (whose scripted reply is the same batch) decodes cleanly.
    assert!(!provider.is_abandoned());
    let flags = provider
        .list_uid_flags()
        .expect("session must stay usable after the degraded message");
    assert_eq!(flags.len(), 3);
    let _ = provider.logout();
}

#[test]
fn a_malformed_bodystructure_next_to_another_bad_field_still_poisons() {
    // Dropping the BODYSTRUCTURE is only a repair candidate: when the line is
    // broken elsewhere too (here `INTERNALDATE NIL`), the re-decode fails and
    // the session poisons exactly as for any other undecodable response.
    let mut replies = gmail_prelude();
    replies.push(gmail_select());
    replies.push((
        "UID FETCH",
        format!(
            "* 1 FETCH (UID 7 FLAGS () INTERNALDATE NIL {})\r\n",
            GMAIL_ZERO_PART_MULTIPART_BODYSTRUCTURE
        ),
        "OK Success".to_string(),
    ));
    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });

    let mut provider = PanicGuardedProvider::new(connect(server.port));
    let _ = provider.list_mailboxes().expect("list mailboxes");
    provider.select("INBOX").expect("select INBOX");
    let err = provider
        .fetch_new_descriptors(None)
        .expect_err("a second fault on the line must still surface");
    let ProviderError::Protocol(msg) = err else {
        panic!("expected a Protocol error")
    };
    assert!(msg.contains("could not be decoded"), "{msg}");
    assert!(!provider.is_abandoned());
    assert!(provider.list_uid_flags().is_err(), "session is poisoned");
}

#[test]
fn namespace_is_really_sent_and_supplies_the_delimiter_when_every_row_is_nil() {
    // Every LIST/LSUB row reports NIL, so the only source of "/" is the
    // NAMESPACE reply — proving `namespace_delimiter()` issues a real
    // NAMESPACE (the command whose absence of a parser crashed the 2026-07-11
    // pilot) and that it now round-trips cleanly.
    let mut replies = gmail_prelude();
    let list = "* LIST (\\HasNoChildren) NIL \"INBOX\"\r\n\
                * LIST (\\HasNoChildren) NIL \"Work\"\r\n";
    replies.retain(|(k, _, _)| *k != "LIST" && *k != "LSUB");
    replies.push((
        "LSUB",
        list.replace("* LIST", "* LSUB"),
        "OK Success".into(),
    ));
    replies.push(("LIST", list.to_string(), "OK Success".into()));

    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let boxes = provider.list_mailboxes().expect("list mailboxes");
    assert!(
        boxes.iter().all(|m| m.delimiter.as_deref() == Some("/")),
        "NAMESPACE's personal delimiter fills every NIL row: {boxes:?}"
    );
    // The session is still in sync after NAMESPACE (the old stack desynced here).
    let _ = provider.logout();
}

#[test]
fn a_nil_delimiter_row_borrows_the_delimiter_another_row_reported() {
    // RFC 3501 lets a server report NIL for a mailbox's hierarchy delimiter.
    // Such a row must still get the namespace's real delimiter, which is what
    // replaced the (never-working) NAMESPACE probe.
    let mut replies = gmail_prelude();
    let list = "* LIST (\\HasNoChildren) NIL \"INBOX\"\r\n\
                * LIST (\\HasChildren) \"/\" \"Work\"\r\n\
                * LIST (\\HasNoChildren) \"/\" \"Work/Invoices\"\r\n";
    replies.retain(|(k, _, _)| *k != "LIST" && *k != "LSUB");
    replies.push((
        "LSUB",
        list.replace("* LIST", "* LSUB"),
        "OK Success".into(),
    ));
    replies.push(("LIST", list.to_string(), "OK Success".into()));

    let server = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
    });
    let mut provider = connect(server.port);
    let boxes = provider.list_mailboxes().expect("list mailboxes");

    let inbox = boxes.iter().find(|m| m.path == "INBOX").expect("INBOX");
    assert_eq!(
        inbox.delimiter.as_deref(),
        Some("/"),
        "a NIL-delimiter row borrows the delimiter another row reported"
    );
    let nested = boxes
        .iter()
        .find(|m| m.path == "Work/Invoices")
        .expect("nested");
    assert_eq!(nested.name, "Invoices", "leaf name uses that delimiter");

    let _ = provider.logout();
}
