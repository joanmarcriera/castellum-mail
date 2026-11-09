// Castellum - RFC 2047 encoded ENVELOPE text, end to end through sync (X1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Offline (no GreenMail): a scripted TCP server returns the ENVELOPE bytes a
// standards-following IMAP server sends, the REAL `ImapProvider` decodes
// them, and the REAL `sync_with_provider` stores, indexes and threads the
// result. Nothing here hydrates a body, so every row is descriptor-only —
// the case is about.
//
//   1. everything derived from the subject uses the decoded text: the list
//      row, the FTS index (an accented word finds the message), and
//      threading (an encoded reply joins its plain parent, and the thread
//      row carries the decoded text);
//   2. rows an older build stored still encoded are repaired by
//      `envelope_repair`: decoded, re-indexed, re-threaded, idempotent,
//      resumable, and never touching a row with no encoded word.

use castellum_core::envelope_repair::{
    repair_encoded_envelope_text, repair_encoded_envelope_text_in_batches,
};
use castellum_core::sync::sync_with_provider;
use castellum_db::Database;
use castellum_provider_imap::ImapProvider;
use castellum_providers::{
    ImapConfig, MailProvider, MailboxState, ProviderError, RemoteAddress, RemoteMailbox,
    RemoteMessage, RemoteMessageDescriptor, UidFlags,
};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};

const ACCOUNT: &str = "acc";

fn temp_db() -> Database {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let path = castellum_testutil::scratch_root().join(format!(
        "castellum-x1-encoded-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    Database::open(&path).expect("open temp db")
}

// ---------------------------------------------------------------- wire side

/// One-connection scripted IMAP server that answers every `UID FETCH` with
/// `fetch` and everything else with a bare OK.
fn serve_once(fetch: String) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            serve(stream, &fetch);
        }
    });
    (port, handle)
}

fn serve(stream: TcpStream, fetch: &str) {
    let mut writer = stream.try_clone().expect("clone");
    let mut reader = BufReader::new(stream);
    let _ = writer.write_all(b"* OK IMAP4rev1 ready\r\n");
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut parts = line.trim_end().splitn(2, ' ');
        let tag = parts.next().unwrap_or("*").to_string();
        let command = parts.next().unwrap_or("").to_ascii_uppercase();
        let untagged = if command.starts_with("LOGOUT") {
            let _ = writer.write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes());
            return;
        } else if command.starts_with("CAPABILITY") {
            "* CAPABILITY IMAP4rev1 UIDPLUS\r\n"
        } else if command.starts_with("SELECT") {
            "* FLAGS (\\Answered \\Flagged \\Deleted \\Seen \\Draft)\r\n\
             * OK [UIDVALIDITY 7] UIDs valid\r\n\
             * 3 EXISTS\r\n\
             * 0 RECENT\r\n\
             * OK [UIDNEXT 4] Predicted next UID\r\n"
        } else if command.starts_with("UID FETCH") {
            fetch
        } else {
            ""
        };
        let _ = writer.write_all(format!("{untagged}{tag} OK done\r\n").as_bytes());
        let _ = writer.flush();
    }
}

/// One descriptor FETCH line. `subject` and `from_name` go on the wire as
/// IMAP quoted strings, exactly as given.
fn fetch_line(
    uid: u32,
    subject: &str,
    from_name: &str,
    in_reply_to: &str,
    message_id: &str,
) -> String {
    format!(
        "* {uid} FETCH (UID {uid} RFC822.SIZE 900 FLAGS () \
         INTERNALDATE \"13-Sep-2026 02:0{uid}:00 +0000\" \
         ENVELOPE (\"Sat, 13 Sep 2026 02:0{uid}:00 +0000\" \"{subject}\" \
         ((\"{from_name}\" NIL \"sender{uid}\" \"example.test\")) NIL NIL \
         ((\"=?ISO-8859-1?Q?N=FAria_Puig?=\" NIL \"nuria\" \"example.test\")) NIL NIL \
         {in_reply_to} \"<{message_id}>\") \
         BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1))\r\n"
    )
}

/// What a standards-following server returns for three messages: a plain
/// parent, a reply to it whose subject and sender are encoded words, and the
/// alice persona's recruiter mail (scripts/local-imap/seed-personas.js).
fn wire() -> String {
    [
        fetch_line(
            1,
            "Budget 2027",
            "Plain Sender",
            "NIL",
            "parent@example.test",
        ),
        fetch_line(
            2,
            "=?UTF-8?Q?Re:_Budget_2027_=E2=80=94_aprovaci=C3=B3?=",
            "=?UTF-8?Q?Jos=C3=A9_Mart=C3=ADnez?=",
            "\"<parent@example.test>\"",
            "reply@example.test",
        ),
        fetch_line(
            3,
            "=?UTF-8?Q?Solutions_Architect_role_=E2=80=94_Cambridge=2C_hybrid?=",
            "=?ISO-8859-15?B?TnVyaWEgUHVpZyAopCk=?=",
            "NIL",
            "recruiter@example.test",
        ),
    ]
    .concat()
}

/// The descriptors the real `ImapProvider` produces for [`wire`].
fn descriptors_from_the_real_provider() -> Vec<RemoteMessageDescriptor> {
    let (port, handle) = serve_once(wire());
    let mut provider = ImapProvider::connect(&ImapConfig {
        host: "127.0.0.1".to_string(),
        port,
        username: "alice".to_string(),
        password: "not-a-real-password".to_string(),
        security: castellum_providers::TransportSecurity::Plaintext,
        allow_invalid_certs: false,
    })
    .expect("connect to the scripted server");
    provider.select("INBOX").expect("select INBOX");
    let descriptors = provider
        .fetch_new_descriptors(None)
        .expect("fetch descriptors");
    let _ = provider.logout();
    drop(provider);
    let _ = handle.join();
    assert_eq!(descriptors.len(), 3, "{descriptors:?}");
    descriptors
}

// ---------------------------------------------------------------- sync side

/// Feeds fixed descriptors to the real sync; never serves a body, so every
/// stored row stays descriptor-only.
struct DescriptorOnly {
    descriptors: Vec<RemoteMessageDescriptor>,
}

impl MailProvider for DescriptorOnly {
    fn list_mailboxes(&mut self) -> Result<Vec<RemoteMailbox>, ProviderError> {
        Ok(vec![RemoteMailbox {
            path: "INBOX".into(),
            name: "INBOX".into(),
            role: Some("inbox".into()),
            role_source: None,
            delimiter: Some("/".into()),
            subscribed: true,
            selectable: true,
        }])
    }
    fn select(&mut self, _path: &str) -> Result<MailboxState, ProviderError> {
        Ok(MailboxState {
            uid_validity: 7,
            uid_next: self.descriptors.len() as i64 + 1,
            exists: self.descriptors.len() as i64,
        })
    }
    fn fetch_new(&mut self, _after_uid: Option<i64>) -> Result<Vec<RemoteMessage>, ProviderError> {
        Err(ProviderError::Protocol("no body fetch in this test".into()))
    }
    fn fetch_new_descriptors(
        &mut self,
        after_uid: Option<i64>,
    ) -> Result<Vec<RemoteMessageDescriptor>, ProviderError> {
        Ok(self
            .descriptors
            .iter()
            .filter(|d| after_uid.is_none_or(|uid| d.uid > uid))
            .cloned()
            .collect())
    }
    fn list_uid_flags(&mut self) -> Result<Vec<UidFlags>, ProviderError> {
        Ok(self
            .descriptors
            .iter()
            .map(|d| UidFlags {
                keywords: None,
                uid: d.uid,
                is_seen: d.is_seen,
                is_flagged: d.is_flagged,
            })
            .collect())
    }
    fn logout(&mut self) -> Result<(), ProviderError> {
        Ok(())
    }
}

fn sync(db: &Database, descriptors: Vec<RemoteMessageDescriptor>) {
    let mut provider = DescriptorOnly { descriptors };
    sync_with_provider(
        db,
        ACCOUNT,
        "alice@example.test",
        None,
        &mut provider,
        "example.test",
    )
    .expect("sync");
}

fn subject(db: &Database, uid: i64) -> Option<String> {
    detail(db, uid).header.subject
}

fn detail(db: &Database, uid: i64) -> castellum_models::MessageDetail {
    db.get_message_detail(&format!("{ACCOUNT}:INBOX:{uid}"))
        .expect("detail")
        .expect("message exists")
}

fn search_ids(db: &Database, query: &str) -> Vec<String> {
    let mut ids: Vec<String> = db
        .search_messages(ACCOUNT, query, 20)
        .expect("search")
        .into_iter()
        .map(|m| m.id)
        .collect();
    ids.sort();
    ids
}

/// (thread id, thread subject) of a message, from the thread projection the
/// thread list reads.
fn thread_of(db: &Database, uid: i64) -> (String, Option<String>) {
    let message_id = format!("{ACCOUNT}:INBOX:{uid}");
    for thread in db
        .list_thread_summaries(ACCOUNT, 50, 0)
        .expect("thread summaries")
    {
        let members = db.get_thread_messages(&thread.id).expect("thread messages");
        if members.iter().any(|m| m.id == message_id) {
            return (thread.id, thread.subject);
        }
    }
    panic!("{message_id} is in no thread");
}

// -------------------------------------------------------------------- tests

#[test]
fn decoded_envelope_text_reaches_the_list_the_index_and_threading() {
    let db = temp_db();
    sync(&db, descriptors_from_the_real_provider());

    // List row / detail: decoded subject and names, on descriptor-only rows.
    let reply = detail(&db, 2);
    assert!(!reply.header.has_body, "descriptor-only");
    assert_eq!(
        reply.header.subject.as_deref(),
        Some("Re: Budget 2027 — aprovació")
    );
    assert_eq!(reply.header.from_name.as_deref(), Some("José Martínez"));
    assert!(
        reply
            .addresses
            .iter()
            .any(|a| a.kind == "to" && a.name.as_deref() == Some("Núria Puig")),
        "recipient display name decoded: {:?}",
        reply.addresses
    );
    assert_eq!(
        subject(&db, 3).as_deref(),
        Some("Solutions Architect role — Cambridge, hybrid")
    );
    assert_eq!(
        detail(&db, 3).header.from_name.as_deref(),
        Some("Nuria Puig (€)")
    );

    // FTS: an accented word that only exists inside an encoded word finds
    // the descriptor-only message; so does the sender's accented name.
    assert_eq!(search_ids(&db, "aprovació"), vec!["acc:INBOX:2"]);
    assert_eq!(search_ids(&db, "Martínez"), vec!["acc:INBOX:2"]);
    assert_eq!(search_ids(&db, "Architect"), vec!["acc:INBOX:3"]);
    assert!(
        search_ids(&db, "UTF").is_empty(),
        "no encoded-word debris in the index"
    );

    // Threading: the encoded reply joins its plain parent, and the thread
    // row shows the root's subject.
    let (parent_thread, thread_subject) = thread_of(&db, 1);
    let (reply_thread, _) = thread_of(&db, 2);
    assert_eq!(parent_thread, reply_thread, "reply threads with its parent");
    assert_eq!(thread_subject.as_deref(), Some("Budget 2027"));
    assert_ne!(thread_of(&db, 3).0, parent_thread);
    assert_eq!(
        thread_of(&db, 3).1.as_deref(),
        Some("Solutions Architect role — Cambridge, hybrid")
    );
    assert_eq!(detail(&db, 2).header.thread_count, 2);
}

/// What a build before X1 stored for [`wire`]: the same descriptors with the
/// text still encoded.
fn descriptors_as_an_old_build_stored_them() -> Vec<RemoteMessageDescriptor> {
    let address = |name: &str, local: &str| RemoteAddress {
        name: Some(name.to_string()),
        address: format!("{local}@example.test"),
    };
    let descriptor =
        |uid: i64, subject: &str, from: &str, refs: &[&str], id: &str| RemoteMessageDescriptor {
            uid,
            size_bytes: Some(900),
            has_attachments: Some(false),
            is_seen: false,
            is_flagged: false,
            rfc822_message_id: Some(id.to_string()),
            subject: Some(subject.to_string()),
            from: Some(address(from, &format!("sender{uid}"))),
            to: vec![address("=?ISO-8859-1?Q?N=FAria_Puig?=", "nuria")],
            cc: vec![],
            bcc: vec![],
            sent_at: Some(1_789_000_000 + uid),
            references: refs.iter().map(|r| r.to_string()).collect(),
            keywords: None,
        };
    vec![
        descriptor(
            1,
            "=?UTF-8?Q?Pressupost_2027_=E2=80=94_revisi=C3=B3?=",
            "Plain Sender",
            &[],
            "parent@example.test",
        ),
        descriptor(
            2,
            "=?UTF-8?Q?Re:_Pressupost_2027_=E2=80=94_aprovaci=C3=B3?=",
            "=?UTF-8?Q?Jos=C3=A9_Mart=C3=ADnez?=",
            &["parent@example.test"],
            "reply@example.test",
        ),
        // Raw UTF-8 that merely looks a bit like an encoded word, and a
        // word nothing can decode: neither may be touched.
        descriptor(
            3,
            "Café =? no és una paraula codificada",
            "Núria",
            &[],
            "raw@example.test",
        ),
        descriptor(
            4,
            "=?x-klingon?Q?nuqneH?=",
            "Plain Sender",
            &[],
            "klingon@example.test",
        ),
    ]
}

#[test]
fn stored_encoded_rows_are_repaired_once_and_the_second_run_is_a_no_op() {
    let db = temp_db();
    sync(&db, descriptors_as_an_old_build_stored_them());

    // The bug, as stored: encoded text in the row, nothing to find.
    assert_eq!(
        subject(&db, 2).as_deref(),
        Some("=?UTF-8?Q?Re:_Pressupost_2027_=E2=80=94_aprovaci=C3=B3?=")
    );
    assert!(search_ids(&db, "aprovació").is_empty());
    assert_eq!(
        search_ids(&db, "UTF"),
        vec!["acc:INBOX:1", "acc:INBOX:2"],
        "the index holds the encoded-word debris instead"
    );
    assert_eq!(
        thread_of(&db, 1).1.as_deref(),
        Some("=?UTF-8?Q?Pressupost_2027_=E2=80=94_revisi=C3=B3?=")
    );
    let untouched_before = (detail(&db, 3), detail(&db, 4));

    let first = repair_encoded_envelope_text(&db).expect("repair");
    assert!(!first.already_done);
    assert_eq!(first.scanned, 4);
    // Messages 1 and 2 by subject; 3 and 4 only through their encoded
    // recipient name.
    assert_eq!(first.messages_repaired, 4);
    assert_eq!(first.subjects_repaired, 2);
    // One sender name (messages.from_name + its message_addresses row) and
    // four recipient rows.
    assert_eq!(first.names_repaired, 6);

    // Decoded rows.
    assert_eq!(
        subject(&db, 1).as_deref(),
        Some("Pressupost 2027 — revisió")
    );
    let reply = detail(&db, 2);
    assert_eq!(
        reply.header.subject.as_deref(),
        Some("Re: Pressupost 2027 — aprovació")
    );
    assert_eq!(reply.header.from_name.as_deref(), Some("José Martínez"));
    let names: Vec<_> = reply
        .addresses
        .iter()
        .map(|a| (a.kind.as_str(), a.name.as_deref()))
        .collect();
    assert_eq!(
        names,
        vec![("from", Some("José Martínez")), ("to", Some("Núria Puig"))]
    );

    // Working search, on subject and on sender.
    assert_eq!(search_ids(&db, "aprovació"), vec!["acc:INBOX:2"]);
    assert_eq!(
        search_ids(&db, "Pressupost"),
        vec!["acc:INBOX:1", "acc:INBOX:2"]
    );
    assert_eq!(search_ids(&db, "Martínez"), vec!["acc:INBOX:2"]);
    assert!(
        search_ids(&db, "UTF").is_empty(),
        "the encoded-word debris is gone from the index"
    );

    // Re-threaded: same grouping, and the thread row has the decoded text.
    assert_eq!(thread_of(&db, 1).0, thread_of(&db, 2).0);
    assert_eq!(
        thread_of(&db, 1).1.as_deref(),
        Some("Pressupost 2027 — revisió")
    );

    // Text with no decodable encoded word is exactly what it was.
    assert_eq!(subject(&db, 3), untouched_before.0.header.subject);
    assert_eq!(
        detail(&db, 3).header.from_name,
        untouched_before.0.header.from_name
    );
    assert_eq!(subject(&db, 4).as_deref(), Some("=?x-klingon?Q?nuqneH?="));
    assert_eq!(subject(&db, 4), untouched_before.1.header.subject);

    // Second run: nothing read, nothing written, same rows.
    let snapshot: Vec<_> = (1..=4).map(|uid| detail(&db, uid)).collect();
    let second = repair_encoded_envelope_text(&db).expect("second repair");
    assert!(second.already_done);
    assert_eq!(
        (
            second.batches,
            second.scanned,
            second.messages_repaired,
            second.subjects_repaired,
            second.names_repaired
        ),
        (0, 0, 0, 0, 0)
    );
    for (uid, before) in (1..=4).zip(&snapshot) {
        let after = detail(&db, uid);
        assert_eq!(after.header.subject, before.header.subject);
        assert_eq!(after.header.from_name, before.header.from_name);
    }
    assert_eq!(search_ids(&db, "aprovació"), vec!["acc:INBOX:2"]);
}

#[test]
fn an_interrupted_repair_resumes_where_it_stopped() {
    let db = temp_db();
    sync(&db, descriptors_as_an_old_build_stored_them());
    let decode = castellum_provider_imap::encoded_words::decode_encoded_words_if_any;

    // One message per batch, and "quit" after the first batch.
    let first = db
        .repair_encoded_envelope_text_batch(1, &decode)
        .expect("batch 1");
    assert_eq!((first.scanned, first.done), (1, false));
    assert_eq!(first.affected_accounts, vec![ACCOUNT.to_string()]);
    assert!(!db.envelope_text_repair_done().expect("done flag"));
    assert_eq!(
        subject(&db, 1).as_deref(),
        Some("Pressupost 2027 — revisió")
    );
    assert_eq!(
        subject(&db, 2).as_deref(),
        Some("=?UTF-8?Q?Re:_Pressupost_2027_=E2=80=94_aprovaci=C3=B3?="),
        "not reached yet"
    );

    // Next launch: resumes after message 1 (which is not scanned again) and
    // runs to the end.
    let resumed = repair_encoded_envelope_text_in_batches(&db, 1).expect("resume");
    assert!(!resumed.already_done);
    assert_eq!(resumed.scanned, 3, "message 1 is behind the cursor");
    assert_eq!(resumed.subjects_repaired, 1);
    assert_eq!(
        subject(&db, 2).as_deref(),
        Some("Re: Pressupost 2027 — aprovació")
    );
    assert!(db.envelope_text_repair_done().expect("done flag"));
    assert_eq!(search_ids(&db, "aprovació"), vec!["acc:INBOX:2"]);
    assert_eq!(
        thread_of(&db, 2).1.as_deref(),
        Some("Pressupost 2027 — revisió")
    );
    assert!(
        repair_encoded_envelope_text(&db)
            .expect("third")
            .already_done
    );
}
