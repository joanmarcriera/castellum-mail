// Castellum - RFC 2047 encoded words in ENVELOPE text.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A standards-following IMAP server (GreenMail, Dovecot, Cyrus) returns the
// Subject and the address display names of an ENVELOPE exactly as they sit in
// the header: as RFC 2047 encoded words whenever they hold non-ASCII text.
// Gmail returns raw UTF-8 instead, which is why the real-Gmail pilot never
// showed `=?UTF-8?Q?Solutions_Architect_...` in the message list.
//
// These tests drive the real `ImapProvider` against a scripted TCP server and
// assert that every ENVELOPE text field comes out decoded: the subject and
// the display names of From, To, Cc and Bcc. (Sender and Reply-To are parsed
// off the wire by imap-codec but are not carried into `SessionEnvelope`, so
// there is nothing to decode for them.)

use castellum_provider_imap::ImapProvider;
use castellum_providers::{ImapConfig, MailProvider, RemoteMessageDescriptor};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};

/// One-connection scripted IMAP server: the untagged `fetch` payload answers
/// any `UID FETCH`; everything else gets a plausible minimal reply.
struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedServer {
    fn start(fetch: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream, &fetch);
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

fn serve(stream: TcpStream, fetch: &str) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(b"* OK IMAP4rev1 ready\r\n");
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

        let untagged = if command.starts_with("LOGOUT") {
            let _ = writer.write_all(b"* BYE Logging out\r\n");
            let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
            let _ = writer.flush();
            return;
        } else if command.starts_with("CAPABILITY") {
            "* CAPABILITY IMAP4rev1 UIDPLUS\r\n".to_string()
        } else if command.starts_with("SELECT") {
            "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
             * OK [UIDVALIDITY 7] UIDs valid\r\n\
             * 1 EXISTS\r\n\
             * 0 RECENT\r\n\
             * OK [UIDNEXT 2] Predicted next UID\r\n"
                .to_string()
        } else if command.starts_with("UID FETCH") {
            fetch.to_string()
        } else {
            String::new()
        };
        let _ = writer.write_all(untagged.as_bytes());
        let _ = writer.write_all(format!("{tag} OK done\r\n").as_bytes());
        let _ = writer.flush();
    }
}

/// One descriptor FETCH line whose ENVELOPE carries the given (already
/// IMAP-quoted or NIL) subject and address lists.
fn fetch_line(subject: &str, from: &str, to: &str, cc: &str, bcc: &str) -> String {
    format!(
        "* 1 FETCH (UID 1 RFC822.SIZE 900 FLAGS () \
         INTERNALDATE \"13-Sep-2026 02:00:00 +0000\" \
         ENVELOPE (\"Sat, 13 Sep 2026 02:00:00 +0000\" {subject} \
         {from} {from} {from} {to} {cc} {bcc} NIL \"<x1@example.test>\") \
         BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1))\r\n"
    )
}

fn fetch_one(fetch: String) -> RemoteMessageDescriptor {
    let server = ScriptedServer::start(fetch);
    let mut provider = ImapProvider::connect(&ImapConfig {
        host: "127.0.0.1".to_string(),
        port: server.port,
        username: "alice".to_string(),
        password: "not-a-real-password".to_string(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    })
    .expect("connect to the scripted server");
    provider.select("INBOX").expect("select INBOX");
    let mut descriptors = provider
        .fetch_new_descriptors(None)
        .expect("fetch descriptors");
    let _ = provider.logout();
    assert_eq!(descriptors.len(), 1, "one descriptor: {descriptors:?}");
    descriptors.remove(0)
}

/// The exact bytes GreenMail returns for the alice persona's recruiter mail
/// (scripts/local-imap/seed-personas.js): nodemailer Q-encodes the subject
/// because of the em dash, and the sender's display name because of `é`.
#[test]
fn encoded_word_subject_and_from_name_come_out_decoded() {
    let d = fetch_one(fetch_line(
        "\"=?UTF-8?Q?Solutions_Architect_role_=E2=80=94_Cambridge=2C_hybrid?=\"",
        "((\"=?UTF-8?Q?Jos=C3=A9_Mart=C3=ADnez?=\" NIL \"jose\" \"example.test\"))",
        "((\"Alice\" NIL \"alice\" \"example.test\"))",
        "NIL",
        "NIL",
    ));

    assert_eq!(
        d.subject.as_deref(),
        Some("Solutions Architect role — Cambridge, hybrid")
    );
    let from = d.from.expect("from");
    assert_eq!(from.name.as_deref(), Some("José Martínez"));
    assert_eq!(from.address, "jose@example.test");
}

/// To, Cc and Bcc display names take the same path as From.
#[test]
fn encoded_word_recipient_names_come_out_decoded() {
    let d = fetch_one(fetch_line(
        "\"=?ISO-8859-1?Q?Reuni=F3?= =?ISO-8859-1?Q?_de_dem=E0?=\"",
        "((\"Plain Sender\" NIL \"sender\" \"example.test\"))",
        "((\"=?iso-8859-15?B?TnVyaWEgUHVpZyAopCk=?=\" NIL \"nuria\" \"example.test\"))",
        "((\"=?windows-1252?Q?Llu=EDs_=93Lluc=94?=\" NIL \"lluis\" \"example.test\"))",
        "((\"=?utf-8*ca?Q?Mart=C3=AD?=\" NIL \"marti\" \"example.test\"))",
    ));

    assert_eq!(d.subject.as_deref(), Some("Reunió de demà"));
    assert_eq!(
        d.from.as_ref().and_then(|a| a.name.as_deref()),
        Some("Plain Sender")
    );
    assert_eq!(d.to[0].name.as_deref(), Some("Nuria Puig (€)"));
    assert_eq!(d.cc[0].name.as_deref(), Some("Lluís \u{201C}Lluc\u{201D}"));
    assert_eq!(d.bcc[0].name.as_deref(), Some("Martí"));
}

/// Gmail's shape: raw UTF-8 in a literal, no encoded word. It must stay
/// byte-for-byte what the server sent.
#[test]
fn raw_utf8_envelope_text_is_left_exactly_as_it_is() {
    let subject = "Café réunió — \"quoted\" & <angled> =? not a word";
    let d = fetch_one(fetch_line(
        &format!("{{{}}}\r\n{subject}", subject.len()),
        "((\"Núria Puig\" NIL \"nuria\" \"example.test\"))",
        "NIL",
        "NIL",
        "NIL",
    ));

    assert_eq!(d.subject.as_deref(), Some(subject));
    assert_eq!(
        d.from.as_ref().and_then(|a| a.name.as_deref()),
        Some("Núria Puig")
    );
}
