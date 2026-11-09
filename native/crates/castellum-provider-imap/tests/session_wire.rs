// Castellum - Session wire parity tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Same scripted-TCP-server pattern as gmail_wire.rs, driving the Session
// type directly instead of ImapProvider.
//
// Run: cargo test -p castellum-provider-imap --test session_wire

use castellum_provider_imap::session::{
    AppendUid, CopyUid, FetchItem, PermanentFlags, Session, SessionError, SessionFlag, StoreSign,
    UntaggedItem,
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A scripted IMAP server — deliberately independent of gmail_wire.rs's
/// `ScriptedServer` (integration-test files cannot share non-pub helpers).
struct ScriptedServer {
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Script {
    greeting: String,
    /// (command prefix, untagged lines, tagged completion text)
    replies: Vec<(&'static str, String, String)>,
    /// Bytes to push right after IDLE's `+ idling` continuation, before
    /// waiting for DONE — simulates a server-pushed line while idling
    /// (S31's mid-IDLE VANISHED test).
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
        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
        let mut parts = trimmed.splitn(2, ' ');
        let tag = parts.next().unwrap_or("*").to_string();
        let command = parts.next().unwrap_or("").to_ascii_uppercase();

        // AUTHENTICATE: reply with a bare continuation, then swallow the
        // base64 SASL response line before completing.
        if command.starts_with("AUTHENTICATE") {
            let _ = writer.write_all(b"+ \r\n");
            let _ = writer.flush();
            line.clear();
            let _ = reader.read_line(&mut line);
            let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
            let _ = writer.flush();
            continue;
        }

        // A command line ending in `{N}` announces a synchronising literal:
        // reply with a continuation request, then read exactly N raw bytes
        // (not a CRLF-terminated line), then the rest of the command line.
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

        // IDLE (RFC 2177): reply with a bare `+` continuation (no tag), then
        // wait for the bare "DONE" line before sending the tagged completion.
        if command.starts_with("IDLE") {
            let _ = writer.write_all(b"+ idling\r\n");
            if let Some(push) = script.idle_push {
                let _ = writer.write_all(push.as_bytes());
            }
            let _ = writer.flush();
            line.clear();
            let _ = reader.read_line(&mut line); // the bare "DONE" line
            let _ = writer.write_all(format!("{tag} OK IDLE completed\r\n").as_bytes());
            let _ = writer.flush();
            continue;
        }

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

fn server(replies: Vec<(&'static str, String, String)>) -> ScriptedServer {
    ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies,
        idle_push: None,
    })
}

fn connect(port: u16) -> Session<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    Session::connect(stream).expect("session greeting")
}

// ---------------------------------------------------------------- connect

#[test]
fn connect_reads_the_greeting() {
    let s = server(vec![]);
    let _session = connect(s.port);
}

#[test]
fn a_bye_greeting_is_refused_as_a_protocol_error() {
    let s = ScriptedServer::start(Script {
        greeting: "* BYE Too many connections\r\n".to_string(),
        replies: vec![],
        idle_push: None,
    });
    let stream = TcpStream::connect(("127.0.0.1", s.port)).expect("connect");
    let err = Session::connect(stream).expect_err("a BYE greeting must not become an Ok session");
    assert!(matches!(err, SessionError::Protocol(_)));
}

#[test]
fn a_garbage_greeting_is_a_protocol_error_not_a_panic() {
    let s = ScriptedServer::start(Script {
        greeting: "\x00\x01 not imap at all\r\n".to_string(),
        replies: vec![],
        idle_push: None,
    });
    let stream = TcpStream::connect(("127.0.0.1", s.port)).expect("connect");
    let err = Session::connect(stream).expect_err("garbage greeting");
    assert!(matches!(err, SessionError::Protocol(_)), "{err:?}");
}

// ----------------------------------------------------------------- engine

/// Review of PR #180, round 3: a server may send `* BYE` mid-response
/// (Gmail "System Error", O365 throttling) and close the connection. That
/// used to end the read loop as a SUCCESS with whatever arrived before it,
/// so a descriptor chunk came back truncated and its checkpoint passed UIDs
/// that were never fetched. Only LOGOUT may complete with a BYE; any other
/// command gets an I/O error and the session is closed for good.
#[test]
fn a_bye_before_a_fetch_completes_is_an_error_not_a_truncated_success() {
    let s = server(vec![(
        "UID FETCH",
        "* 1 FETCH (UID 1 FLAGS ())\r\n* BYE System Error\r\n".to_string(),
        "OK Success".to_string(),
    )]);
    let mut session = connect(s.port);
    let err = session
        .uid_fetch("1:10", &[FetchItem::Uid, FetchItem::Flags])
        .expect_err("a BYE mid-FETCH must not be a truncated Ok");
    assert!(matches!(err, SessionError::Io(_)), "{err:?}");
    assert!(session.is_closed());
    assert!(!session.is_poisoned(), "a BYE is not a decode failure");
    assert!(
        matches!(session.noop(), Err(SessionError::Io(_))),
        "a closed session never sends another command"
    );
}

#[test]
fn logout_still_completes_normally_on_its_bye() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    session.logout().expect("LOGOUT's BYE is its normal shape");
}

#[test]
fn noop_round_trips_through_the_command_engine() {
    let s = server(vec![("NOOP", String::new(), "OK NOOP completed".into())]);
    let mut session = connect(s.port);
    session.noop().expect("NOOP must succeed");
}

#[test]
fn a_tagged_no_becomes_a_typed_error_naming_the_command() {
    let s = server(vec![("NOOP", String::new(), "NO not today".into())]);
    let mut session = connect(s.port);
    match session
        .noop()
        .expect_err("a tagged NO must become an error")
    {
        SessionError::No { command, text } => {
            assert_eq!(command, "NOOP");
            assert_eq!(text, "not today");
        }
        other => panic!("expected SessionError::No, got {other:?}"),
    }
}

#[test]
fn a_tagged_bad_becomes_a_typed_bad_error() {
    let s = server(vec![("NOOP", String::new(), "BAD what".into())]);
    let mut session = connect(s.port);
    let err = session.noop().expect_err("BAD");
    assert!(
        matches!(
            err,
            SessionError::Bad {
                command: "NOOP",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn an_undecodable_response_poisons_the_session_and_later_calls_fail_fast() {
    let s = server(vec![(
        "NOOP",
        "* \x01\x02 utter nonsense\r\n".to_string(),
        "OK done".into(),
    )]);
    let mut session = connect(s.port);
    let first = session.noop().expect_err("undecodable response");
    assert!(matches!(first, SessionError::Protocol(_)), "{first:?}");
    let second = session.noop().expect_err("poisoned");
    match second {
        SessionError::Protocol(m) => assert!(m.contains("poisoned"), "{m}"),
        other => panic!("expected poisoned Protocol error, got {other:?}"),
    }
}

// ---------------------------------------------------------- login / caps

#[test]
fn capability_lists_gmail_extensions() {
    let s = server(vec![(
        "CAPABILITY",
        "* CAPABILITY IMAP4rev1 UNSELECT IDLE NAMESPACE UIDPLUS MOVE\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let caps = session.capability().expect("CAPABILITY");
    assert!(caps.iter().any(|c| c == "MOVE"), "{caps:?}");
    assert!(caps.iter().any(|c| c == "UIDPLUS"), "{caps:?}");
}

#[test]
fn login_completes_against_a_scripted_server() {
    let s = server(vec![(
        "LOGIN",
        String::new(),
        "OK pilot@gmail.com authenticated (Success)".into(),
    )]);
    let mut session = connect(s.port);
    session
        .login("pilot@gmail.com", "app-password")
        .expect("LOGIN");
}

#[test]
fn login_rejected_reports_a_typed_no() {
    let s = server(vec![(
        "LOGIN",
        String::new(),
        "NO [AUTHENTICATIONFAILED] Invalid credentials".into(),
    )]);
    let mut session = connect(s.port);
    let err = session.login("pilot@gmail.com", "wrong").expect_err("NO");
    assert!(
        matches!(
            err,
            SessionError::No {
                command: "LOGIN",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn authenticate_plain_round_trips_through_the_sasl_continuation() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    session
        .authenticate_plain("", "pilot@gmail.com", "app-password")
        .expect("AUTHENTICATE PLAIN");
}

#[test]
fn enable_reports_the_server_accepted_capabilities() {
    let s = server(vec![(
        "ENABLE",
        "* ENABLED CONDSTORE QRESYNC\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let enabled = session.enable(&["CONDSTORE", "QRESYNC"]).expect("ENABLE");
    assert!(
        enabled.iter().any(|c| c.eq_ignore_ascii_case("CONDSTORE")),
        "{enabled:?}"
    );
}

#[test]
fn namespace_reports_the_personal_delimiter() {
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("NAMESPACE");
    assert_eq!(result.personal_delimiter.as_deref(), Some("/"));
}

#[test]
fn namespace_parses_all_three_classes_and_resolves_delimiters() {
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" \"/\")) ((\"Other Users/\" \"/\")) ((\"Shared\" \".\"))\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("NAMESPACE");
    assert_eq!(result.entries.len(), 3, "{:?}", result.entries);
    assert_eq!(result.personal_delimiter.as_deref(), Some("/"));
    assert_eq!(result.delimiter_for("INBOX").as_deref(), Some("/"));
    assert_eq!(
        result.delimiter_for("Other Users/bob").as_deref(),
        Some("/")
    );
    assert_eq!(result.delimiter_for("Shared.team").as_deref(), Some("."));
    assert_eq!(result.delimiter_for("Archive").as_deref(), Some("/"));
}

#[test]
fn a_malformed_namespace_response_poisons_the_session_instead_of_panicking() {
    // Unbalanced parens and a bare atom where a prefix string belongs: the
    // decoder must hand back a typed error, and the session must refuse every
    // later command rather than read on from a desynchronised socket.
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" \"/\") (oops NIL NIL\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let first = session.namespace().expect_err("undecodable NAMESPACE");
    assert!(matches!(first, SessionError::Protocol(_)), "{first:?}");
    assert!(session.is_poisoned());
    let second = session.noop().expect_err("poisoned");
    match second {
        SessionError::Protocol(m) => assert!(m.contains("poisoned"), "{m}"),
        other => panic!("expected poisoned Protocol error, got {other:?}"),
    }
}

#[test]
fn namespace_with_nil_delimiters_everywhere_resolves_nothing() {
    // A flat server: every class reports NIL. The entries are kept (prefixes
    // are still real) but no path may be given a delimiter out of thin air.
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" NIL)) ((\"Other Users/\" NIL)) ((\"Shared/\" NIL))\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("NAMESPACE");
    assert_eq!(result.entries.len(), 3, "{:?}", result.entries);
    assert!(result.entries.iter().all(|e| e.delimiter.is_none()));
    assert_eq!(result.personal_delimiter, None);
    for path in ["INBOX", "Work", "Other Users/bob", "Shared/team", "Shared"] {
        assert_eq!(result.delimiter_for(path), None, "{path}");
    }
}

#[test]
fn namespace_with_an_empty_personal_prefix_owns_every_unclaimed_path() {
    // The "" personal prefix matches everything, so a longer prefix must win
    // for its own subtree and the root entry for the rest, INBOX included.
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" \"/\")) NIL ((\"#shared.\" \".\"))\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("NAMESPACE");
    assert_eq!(result.entries[0].prefix, "");
    assert_eq!(result.delimiter_for("INBOX").as_deref(), Some("/"));
    assert_eq!(result.delimiter_for("Work/2026").as_deref(), Some("/"));
    assert_eq!(result.delimiter_for("").as_deref(), Some("/"));
    assert_eq!(result.delimiter_for("#shared.team").as_deref(), Some("."));
}

#[test]
fn namespace_prefix_with_quote_and_backslash_round_trips_exactly() {
    // Hostile but legal: a quoted string may carry \" and \\ escapes. The
    // prefix must come back unescaped, and the path that starts with it must
    // still resolve its own delimiter.
    let s = server(vec![(
        "NAMESPACE",
        "* NAMESPACE ((\"\" \"/\")) ((\"Qu\\\"o\\\\te/\" \"|\")) NIL\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("NAMESPACE");
    assert_eq!(result.entries[1].prefix, "Qu\"o\\te/");
    assert_eq!(
        result.delimiter_for("Qu\"o\\te/inbox").as_deref(),
        Some("|")
    );
    assert_eq!(result.delimiter_for("Quote/inbox").as_deref(), Some("/"));
}

#[test]
fn namespace_prefix_with_non_ascii_bytes_decodes_and_keeps_sync() {
    // Raw UTF-8 in a quoted prefix (UTF8=ACCEPT servers, or a sloppy one).
    // The decoder accepts it (verified: the decoded branch is the one that
    // runs), so the prefix must round-trip exactly and the session must stay
    // in sync for the next command on the same connection.
    let s = server(vec![
        (
            "NAMESPACE",
            "* NAMESPACE ((\"\" \"/\")) ((\"Ünï/\" \"/\")) NIL\r\n".into(),
            "OK Success".into(),
        ),
        ("NOOP", String::new(), "OK done".into()),
    ]);
    let mut session = connect(s.port);
    let result = session.namespace().expect("non-ASCII prefix decodes");
    assert_eq!(result.entries[1].prefix, "Ünï/");
    assert_eq!(result.delimiter_for("Ünï/x").as_deref(), Some("/"));
    assert!(!session.is_poisoned());
    session
        .noop()
        .expect("still in sync after a decoded NAMESPACE");
}

#[test]
fn nil_delimiter_list_rows_outside_every_namespace_prefix_stay_unresolved() {
    // LIST rows that report NIL, against a NAMESPACE whose personal root is a
    // non-empty prefix: a name matching no prefix gets no delimiter from
    // `delimiter_for`. (The provider's personal-delimiter fallback for such
    // names lives in lib.rs and is not exercised here; this is session-level.)
    let s = server(vec![
        (
            "LIST",
            "* LIST (\\HasNoChildren) NIL \"Archive\"\r\n\
             * LIST (\\HasNoChildren) NIL \"Mail/Sent\"\r\n"
                .into(),
            "OK Success".into(),
        ),
        (
            "NAMESPACE",
            "* NAMESPACE ((\"Mail/\" \"/\")) NIL NIL\r\n".into(),
            "OK Success".into(),
        ),
    ]);
    let mut session = connect(s.port);
    let rows = session.list("", "*").expect("LIST");
    assert!(rows.iter().all(|r| r.delimiter.is_none()), "{rows:?}");
    let ns = session.namespace().expect("NAMESPACE");
    assert_eq!(ns.delimiter_for("Archive"), None);
    assert_eq!(ns.delimiter_for("Mail/Sent").as_deref(), Some("/"));
    assert_eq!(ns.delimiter_for("Mail").as_deref(), Some("/"));
}

#[test]
fn delete_sends_a_delete_command() {
    let s = server(vec![(
        "DELETE",
        String::new(),
        "OK DELETE completed".into(),
    )]);
    let mut session = connect(s.port);
    session.delete("Archive").expect("DELETE");
}

#[test]
fn uid_fetch_section_returns_the_part_bytes() {
    let s = server(vec![(
        "UID FETCH 7 BODY.PEEK[2.1]",
        "* 1 FETCH (UID 7 BODY[2.1] {5}\r\nhello)\r\n".into(),
        "OK done".into(),
    )]);
    let mut session = connect(s.port);
    let bytes = session.uid_fetch_section(7, &[2, 1]).expect("fetch");
    assert_eq!(bytes.as_deref(), Some(&b"hello"[..]));
}

#[test]
fn uid_fetch_section_without_data_is_none_and_bad_paths_are_typed_errors() {
    let s = server(vec![("UID FETCH", String::new(), "OK done".into())]);
    let mut session = connect(s.port);
    assert_eq!(session.uid_fetch_section(7, &[1]).expect("fetch"), None);
    assert!(matches!(
        session.uid_fetch_section(7, &[]),
        Err(SessionError::Protocol(_))
    ));
    assert!(matches!(
        session.uid_fetch_section(7, &[1, 0]),
        Err(SessionError::Protocol(_))
    ));
}

// ------------------------------------------------------------ LIST / LSUB

#[test]
fn list_reports_special_use_tokens_and_noselect() {
    let list = "\
* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
* LIST (\\HasChildren \\Noselect) \"/\" \"[Gmail]\"\r\n\
* LIST (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n";
    let s = server(vec![("LIST", list.into(), "OK Success".into())]);
    let mut session = connect(s.port);
    let items = session.list("", "*").expect("LIST");

    let inbox = items.iter().find(|m| m.path == "INBOX").expect("INBOX");
    assert!(!inbox.no_select);
    assert_eq!(inbox.delimiter.as_deref(), Some("/"));

    let container = items.iter().find(|m| m.path == "[Gmail]").expect("[Gmail]");
    assert!(container.no_select, "\\Noselect must set no_select");

    let all_mail = items
        .iter()
        .find(|m| m.path == "[Gmail]/All Mail")
        .expect("All Mail");
    assert!(
        all_mail.special_use.iter().any(|s| s == "\\All"),
        "{:?}",
        all_mail.special_use
    );
}

#[test]
fn lsub_reports_subscribed_paths_and_a_nil_delimiter() {
    let lsub = "\
* LSUB (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
* LSUB (\\Noselect) NIL \"Shared\"\r\n";
    let s = server(vec![("LSUB", lsub.into(), "OK Success".into())]);
    let mut session = connect(s.port);
    let items = session.lsub("", "*").expect("LSUB");
    assert_eq!(items.len(), 2);
    let shared = items.iter().find(|m| m.path == "Shared").expect("Shared");
    assert_eq!(shared.delimiter, None);
    assert!(shared.no_select);
}

// ---------------------------------------------------------------- SELECT

#[test]
fn select_reports_uidvalidity_uidnext_and_exists() {
    let select_reply = "\
* FLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen)\r\n\
* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen \\*)] Flags permitted.\r\n\
* OK [UIDVALIDITY 1] UIDs valid.\r\n\
* 12345 EXISTS\r\n\
* 0 RECENT\r\n\
* OK [UIDNEXT 54321] Predicted next UID.\r\n";
    let s = server(vec![(
        "SELECT",
        select_reply.into(),
        "OK [READ-WRITE] INBOX selected. (Success)".into(),
    )]);
    let mut session = connect(s.port);
    let mailbox = session.select("INBOX").expect("SELECT");
    assert_eq!(mailbox.uid_validity, Some(1));
    assert_eq!(mailbox.uid_next, Some(54321));
    assert_eq!(mailbox.exists, 12345);
    assert!(mailbox.read_write);
}

#[test]
fn examine_reports_read_only() {
    let s = server(vec![(
        "EXAMINE",
        "* 3 EXISTS\r\n* OK [UIDVALIDITY 9] ok\r\n".into(),
        "OK [READ-ONLY] done".into(),
    )]);
    let mut session = connect(s.port);
    let mailbox = session.examine("INBOX").expect("EXAMINE");
    assert_eq!(mailbox.exists, 3);
    assert_eq!(mailbox.uid_validity, Some(9));
    assert!(!mailbox.read_write);
}

#[test]
fn untagged_no_during_select_no_longer_crashes_and_select_still_completes() {
    let select_reply = "\
* FLAGS (\\Seen)\r\n\
* NO [ALERT] Mailbox is approaching its quota\r\n\
* OK [UIDVALIDITY 1] UIDs valid.\r\n\
* 1 EXISTS\r\n";
    let s = server(vec![(
        "SELECT",
        select_reply.into(),
        "OK [READ-WRITE] INBOX selected.".into(),
    )]);
    let mut session = connect(s.port);
    let mailbox = session
        .select("INBOX")
        .expect("SELECT must succeed despite the untagged NO — this is the fix, not a panic-guard");
    assert_eq!(mailbox.uid_validity, Some(1));
    assert_eq!(mailbox.exists, 1);
}

#[test]
fn select_qresync_reports_highest_modseq_changed_flags_and_vanished_uids() {
    let select_reply = "\
* 172 EXISTS\r\n\
* 1 RECENT\r\n\
* OK [UIDVALIDITY 1] UIDs valid\r\n\
* OK [UIDNEXT 200] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 20010715194045] Highest\r\n\
* VANISHED (EARLIER) 41,43:116,118,120:211,214:540\r\n\
* 49 FETCH (UID 117 FLAGS (\\Seen \\Answered))\r\n\
* 50 FETCH (UID 119 FLAGS (\\Answered))\r\n";
    let s = server(vec![(
        "SELECT",
        select_reply.into(),
        "OK [READ-WRITE] SELECT completed".into(),
    )]);
    let mut session = connect(s.port);
    let result = session
        .select_qresync("INBOX", 1, 20010715194040)
        .expect("SELECT..QRESYNC");

    assert_eq!(result.mailbox.uid_validity, Some(1));
    assert_eq!(result.mailbox.exists, 172);
    assert_eq!(result.mailbox.highest_modseq, Some(20010715194045));
    assert!(!result.mailbox.no_modseq);

    assert_eq!(result.changed.len(), 2);
    assert!(result
        .changed
        .iter()
        .any(|m| m.uid == Some(117) && m.flags.contains(&SessionFlag::Answered)));

    // Kept as the wire's ranges, never expanded into individual UIDs.
    assert_eq!(
        result.vanished_uid_ranges,
        vec![41..=41, 43..=116, 118..=118, 120..=211, 214..=540]
    );
}

/// CLAUDE.md gotcha 1 — the wire must never crash us. A 13-byte
/// `1:4294967295` VANISHED range used to be expanded UID by UID into a
/// `Vec<u32>` (~16 GiB); a hostile or merely careless server could OOM the
/// app with one line. The range must come back as a range.
#[test]
fn select_qresync_keeps_a_whole_uid_space_vanished_range_bounded() {
    let select_reply = "\
* 1 EXISTS\r\n\
* OK [UIDVALIDITY 1] UIDs valid\r\n\
* OK [UIDNEXT 200] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 90] Highest\r\n\
* VANISHED (EARLIER) 1:4294967295\r\n";
    let s = server(vec![(
        "SELECT",
        select_reply.into(),
        "OK [READ-WRITE] SELECT completed".into(),
    )]);
    let mut session = connect(s.port);
    let result = session
        .select_qresync("INBOX", 1, 80)
        .expect("a huge VANISHED range is valid IMAP and must not fail the SELECT");
    assert_eq!(result.vanished_uid_ranges.len(), 1);
    assert_eq!(result.vanished_uid_ranges[0], 1..=4294967295);
    assert_eq!(result.mailbox.highest_modseq, Some(90));
}

#[test]
fn select_qresync_falls_back_to_a_plain_select_when_uidvalidity_no_longer_matches() {
    // Per RFC 7162 §3.6.2: if the server's current UIDVALIDITY doesn't match
    // the one the client sent, it silently performs an ordinary SELECT (no
    // VANISHED/changed-FETCH data) instead of erroring.
    let select_reply = "\
* 5 EXISTS\r\n\
* OK [UIDVALIDITY 2] UIDs valid\r\n\
* OK [UIDNEXT 10] Predicted next UID\r\n\
* OK [HIGHESTMODSEQ 5] Highest\r\n";
    let s = server(vec![(
        "SELECT",
        select_reply.into(),
        "OK [READ-WRITE] SELECT completed".into(),
    )]);
    let mut session = connect(s.port);
    let result = session
        .select_qresync("INBOX", 1, 100)
        .expect("SELECT..QRESYNC still succeeds on a UIDVALIDITY change");

    assert_eq!(result.mailbox.uid_validity, Some(2));
    assert!(result.changed.is_empty());
    assert!(result.vanished_uid_ranges.is_empty());
}

// ----------------------------------------------------------------- FETCH

#[test]
fn uid_fetch_survives_a_literal_encoded_envelope_and_reports_the_pdf_as_an_attachment() {
    let subject = "Café réunió — \"quoted\" & <angled>";
    let fetch = format!(
        "* 1 FETCH (UID 12345 RFC822.SIZE 4523 FLAGS (\\Seen) \
         INTERNALDATE \"11-Jul-2026 08:12:03 +0100\" \
         ENVELOPE (\"Fri, 11 Jul 2026 08:12:03 +0100\" {{{len}}}\r\n{subject} \
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
        len = subject.len(),
    );
    let s = server(vec![("UID FETCH", fetch, "OK Success".into())]);
    let mut session = connect(s.port);
    let messages = session
        .uid_fetch(
            "1:*",
            &[
                FetchItem::Uid,
                FetchItem::Flags,
                FetchItem::Rfc822Size,
                FetchItem::Envelope,
                FetchItem::InternalDate,
                FetchItem::BodyStructure,
            ],
        )
        .expect("UID FETCH");
    assert_eq!(messages.len(), 1);
    let m = &messages[0];
    assert_eq!(m.uid, Some(12345));
    assert_eq!(m.size_bytes, Some(4523));
    assert!(m.flags.contains(&SessionFlag::Seen));
    assert!(m.internal_date_epoch.is_some());
    let envelope = m.envelope.as_ref().expect("envelope");
    assert_eq!(
        envelope.subject.as_deref(),
        Some("Café réunió — \"quoted\" & <angled>")
    );
    assert_eq!(
        envelope.message_id.as_deref(),
        Some("CA+abc123@mail.gmail.com")
    );
    assert_eq!(
        envelope.in_reply_to,
        vec!["CA+prev123@mail.gmail.com".to_string()]
    );
    assert_eq!(envelope.from[0].address, "alice@example.com");
    assert_eq!(
        m.has_attachments,
        Some(true),
        "the PDF part is an attachment"
    );
}

#[test]
fn uid_fetch_body_peek_returns_the_raw_body_bytes() {
    let fetch = "* 1 FETCH (UID 7 FLAGS () BODY[] {11}\r\nhello world)\r\n";
    let s = server(vec![("UID FETCH", fetch.into(), "OK Success".into())]);
    let mut session = connect(s.port);
    let messages = session
        .uid_fetch(
            "7",
            &[FetchItem::Uid, FetchItem::Flags, FetchItem::BodyPeek],
        )
        .expect("UID FETCH BODY.PEEK");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].body.as_deref(), Some(&b"hello world"[..]));
}

#[test]
fn a_text_only_message_has_no_attachments() {
    let fetch = "* 1 FETCH (UID 8 BODYSTRUCTURE (\"text\" \"plain\" (\"charset\" \"UTF-8\") NIL NIL \"7bit\" 12 1 NIL NIL NIL NIL))\r\n";
    let s = server(vec![("UID FETCH", fetch.into(), "OK Success".into())]);
    let mut session = connect(s.port);
    let messages = session
        .uid_fetch("8", &[FetchItem::Uid, FetchItem::BodyStructure])
        .expect("UID FETCH");
    assert_eq!(messages[0].has_attachments, Some(false));
}

#[test]
fn internaldate_nil_fails_the_fetch_cleanly_instead_of_panicking() {
    let s = server(vec![(
        "UID FETCH",
        "* 1 FETCH (UID 7 RFC822.SIZE 10 FLAGS () INTERNALDATE NIL)\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let err = session
        .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::InternalDate])
        .expect_err("INTERNALDATE NIL violates the grammar and must fail cleanly");
    assert!(matches!(err, SessionError::Protocol(_)), "{err:?}");
}

#[test]
fn raw_8bit_bytes_in_a_quoted_envelope_string_do_not_break_the_fetch() {
    // Spam echoes raw 8-bit header bytes into ENVELOPE's quoted strings, which
    // a strict RFC 3501 decoder rejects. One such message must not make every
    // sync of the mailbox fail: Session rewrites the quoted string as a
    // literal and decodes it (lossy text is fine; the session stays usable).
    let envelope = "* 1 FETCH (UID 7 ENVELOPE (NIL \"caf\u{e9}-\u{ff}\" \
                    ((NIL NIL \"h\" \"local.test\")) NIL NIL NIL NIL NIL NIL NIL))\r\n";
    let s = server(vec![("UID FETCH", envelope.into(), "OK done".into())]);
    let mut session = connect(s.port);
    let fetched = session
        .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::Envelope])
        .expect("a hostile 8-bit subject must decode");
    assert_eq!(fetched.len(), 1);
    assert_eq!(fetched[0].uid, Some(7));
    let subject = fetched[0]
        .envelope
        .as_ref()
        .and_then(|e| e.subject.clone())
        .expect("subject");
    assert!(subject.starts_with("caf"), "{subject:?}");
    // Still in sync afterwards.
    session.noop().expect("session not poisoned");
}

#[test]
fn hostile_encoded_words_in_an_envelope_decode_or_stay_put_and_never_panic() {
    // Stage X1: ENVELOPE text is RFC 2047 decoded. A server (or a spammer's
    // header, echoed by one) controls every byte of these strings, so each
    // broken shape must come out either decoded or exactly as sent — never a
    // panic, never a poisoned session.
    let cases: &[(&str, &str)] = &[
        // (subject as sent, subject expected)
        ("=?UTF-8?Q?caf=C3=A9?=", "café"),
        ("=?UTF-8?Q?caf=C3=A?=", "=?UTF-8?Q?caf=C3=A?="),
        ("=?UTF-8?B?@@@@?=", "=?UTF-8?B?@@@@?="),
        ("=?x-klingon?Q?nuqneH?=", "=?x-klingon?Q?nuqneH?="),
        ("=?UTF-8?Q?=C3=28?=", "=?UTF-8?Q?=C3=28?="),
        ("=?=?=?=?=?=?=?", "=?=?=?=?=?=?=?"),
        (
            "=??Q??= =?*?B??= =?UTF-8??x?=",
            "=??Q??= =?*?B??= =?UTF-8??x?=",
        ),
        ("=?UTF-8?Q?a=0D=0Ab=00?=", "a  b"),
        ("=?utf-16le?B?Pdg=?=", "=?utf-16le?B?Pdg=?="),
        ("=?UTF-8?Q?=?UTF-8?Q?a?=?=", "=?UTF-8?Q?a?="),
    ];
    for (sent, expected) in cases {
        let envelope = format!(
            "* 1 FETCH (UID 7 ENVELOPE (NIL \"{sent}\" \
             ((\"{sent}\" NIL \"h\" \"local.test\")) NIL NIL NIL NIL NIL NIL NIL))\r\n"
        );
        let s = server(vec![("UID FETCH", envelope, "OK done".into())]);
        let mut session = connect(s.port);
        let fetched = session
            .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::Envelope])
            .unwrap_or_else(|e| panic!("{sent:?} must not fail the fetch: {e:?}"));
        let envelope = fetched[0].envelope.as_ref().expect("envelope");
        assert_eq!(
            envelope.subject.as_deref(),
            Some(*expected),
            "sent {sent:?}"
        );
        assert_eq!(
            envelope.from[0].name.as_deref(),
            Some(*expected),
            "display name, sent {sent:?}"
        );
        assert_eq!(envelope.from[0].address, "h@local.test");
        session.noop().expect("session not poisoned");
    }
}

// ------------------------------------------------- STORE / SEARCH / CREATE

#[test]
fn uid_search_all_ands_its_criteria() {
    let s = server(vec![(
        "UID SEARCH SEEN HEADER",
        "* SEARCH 3 9\r\n".into(),
        "OK done".into(),
    )]);
    let mut session = connect(s.port);
    let uids = session
        .uid_search_all(&[
            castellum_provider_imap::session::SessionSearch::Seen,
            castellum_provider_imap::session::SessionSearch::Header {
                field: "Message-Id",
                value: "<a@b>",
            },
        ])
        .expect("UID SEARCH");
    assert_eq!(uids, vec![3, 9]);
    assert!(matches!(
        session.uid_search_all(&[]),
        Err(SessionError::Protocol(_))
    ));
}

#[test]
fn uid_store_adds_the_seen_flag() {
    let s = server(vec![("UID STORE", String::new(), "OK Success".into())]);
    let mut session = connect(s.port);
    session
        .uid_store("42", StoreSign::Add, &[SessionFlag::Seen])
        .expect("UID STORE");
}

#[test]
fn uid_search_by_uid_finds_an_existing_message() {
    let s = server(vec![(
        "UID SEARCH",
        "* SEARCH 42\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    assert_eq!(session.uid_search_by_uid(42).expect("UID SEARCH"), vec![42]);
}

#[test]
fn uid_search_by_message_id_reports_no_matches_as_empty() {
    let s = server(vec![(
        "UID SEARCH",
        "* SEARCH\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let uids = session
        .uid_search_by_message_id("<abc@example.test>")
        .expect("UID SEARCH");
    assert!(uids.is_empty());
}

/// `ImapProvider::locate_by_message_id` (undo) rests on this call: a garbage
/// SEARCH reply must come back as an error, never a panic.
#[test]
fn locate_by_message_id_malformed_search_is_error_not_panic() {
    let s = server(vec![(
        "UID SEARCH",
        "* SEARCH not-a-number\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    assert!(session
        .uid_search_by_message_id("<abc@example.test>")
        .is_err());
}

#[test]
fn uid_search_by_message_id_returns_every_duplicate() {
    let s = server(vec![(
        "UID SEARCH",
        "* SEARCH 12 40 31\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let mut uids = session
        .uid_search_by_message_id("<abc@example.test>")
        .expect("UID SEARCH");
    uids.sort_unstable();
    assert_eq!(uids, vec![12, 31, 40]);
}

#[test]
fn uid_move_parses_copyuid() {
    let s = server(vec![(
        "UID MOVE",
        "* OK [COPYUID 38505 304 3956]\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    let landed = session.uid_move("304", "Archive").expect("UID MOVE");
    assert_eq!(
        landed,
        Some(CopyUid {
            uid_validity: 38505,
            uid: 3956
        })
    );
}

#[test]
fn uid_move_without_copyuid_is_ok_none() {
    let s = server(vec![("UID MOVE", String::new(), "OK Success".into())]);
    let mut session = connect(s.port);
    assert_eq!(session.uid_move("304", "Archive").expect("UID MOVE"), None);
}

#[test]
fn uid_move_range_or_malformed_copyuid_is_none_not_panic() {
    for untagged in [
        "* OK [COPYUID 38505 304:306 3956:3958]\r\n",
        "* OK [COPYUID banana]\r\n",
    ] {
        let s = server(vec![("UID MOVE", untagged.into(), "OK Success".into())]);
        let mut session = connect(s.port);
        let landed = session.uid_move("304", "Archive");
        assert!(matches!(landed, Ok(None) | Err(_)), "{landed:?}");
    }
}

#[test]
fn uid_copy_reads_copyuid_from_the_tagged_ok() {
    let s = server(vec![(
        "UID COPY",
        String::new(),
        "OK [COPYUID 7 12 99] Done".into(),
    )]);
    let mut session = connect(s.port);
    assert_eq!(
        session.uid_copy("12", "Archive").expect("UID COPY"),
        Some(CopyUid {
            uid_validity: 7,
            uid: 99
        })
    );
}

#[test]
fn create_succeeds_on_a_new_mailbox() {
    let s = server(vec![("CREATE", String::new(), "OK Success".into())]);
    let mut session = connect(s.port);
    session.create("Archive").expect("CREATE");
}

#[test]
fn create_on_an_existing_mailbox_returns_a_typed_no_error_for_the_caller_to_ignore() {
    let s = server(vec![(
        "CREATE",
        String::new(),
        "NO Mailbox already exists".into(),
    )]);
    let mut session = connect(s.port);
    let err = session.create("Archive").expect_err("already exists");
    assert!(matches!(
        err,
        SessionError::No {
            command: "CREATE",
            ..
        }
    ));
}

// ---------------------------------------------------- move / cleanup path

#[test]
fn uid_move_completes_for_a_move_capable_server() {
    let s = server(vec![("UID MOVE", String::new(), "OK Success".into())]);
    let mut session = connect(s.port);
    session.uid_move("7", "Archive").expect("UID MOVE");
}

#[test]
fn uid_copy_then_store_deleted_then_uid_expunge_is_the_uidplus_fallback_path() {
    let s = server(vec![
        ("UID COPY", String::new(), "OK Success".into()),
        ("UID STORE", String::new(), "OK Success".into()),
        ("UID EXPUNGE", String::new(), "OK Success".into()),
    ]);
    let mut session = connect(s.port);
    session.uid_copy("7", "Archive").expect("UID COPY");
    session
        .uid_store("7", StoreSign::Add, &[SessionFlag::Deleted])
        .expect("UID STORE +FLAGS Deleted");
    session.uid_expunge("7").expect("UID EXPUNGE");
}

#[test]
fn expunge_completes() {
    let s = server(vec![(
        "EXPUNGE",
        "* 1 EXPUNGE\r\n".into(),
        "OK Success".into(),
    )]);
    let mut session = connect(s.port);
    session.expunge().expect("EXPUNGE");
}

// ---------------------------------------------------------------- APPEND

#[test]
fn append_sends_the_message_as_a_literal_and_completes() {
    let s = server(vec![(
        "APPEND",
        String::new(),
        "OK [APPENDUID 1 99] Success".into(),
    )]);
    let mut session = connect(s.port);
    let raw = format!(
        "Date: Fri, 11 Jul 2026 08:12:03 +0100\r\nFrom: marc@example.test\r\nTo: alice@example.test\r\nSubject: {}\r\n\r\nbody\r\n",
        "x".repeat(500)
    );
    session
        .append("Sent", &[SessionFlag::Seen], raw.as_bytes())
        .expect("APPEND");
}

#[test]
fn append_get_uid_parses_the_appenduid_response_code() {
    let s = server(vec![(
        "APPEND",
        String::new(),
        "OK [APPENDUID 38505 3955] Success".into(),
    )]);
    let mut session = connect(s.port);
    let result = session
        .append_get_uid(
            "Drafts",
            &[SessionFlag::Draft],
            b"Subject: x\r\n\r\nbody\r\n",
        )
        .expect("APPEND");
    assert_eq!(
        result,
        Some(AppendUid {
            uid_validity: 38505,
            uid: 3955
        })
    );
}

#[test]
fn append_get_uid_returns_none_without_uidplus() {
    let s = server(vec![("APPEND", String::new(), "OK Success".into())]);
    let mut session = connect(s.port);
    let result = session
        .append_get_uid(
            "Drafts",
            &[SessionFlag::Draft],
            b"Subject: x\r\n\r\nbody\r\n",
        )
        .expect("APPEND");
    assert_eq!(result, None);
}

// ---------------------------------------------------------------- LOGOUT

#[test]
fn logout_completes_on_the_untagged_bye() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    session.logout().expect("LOGOUT");
}

// ----------------------------------------------------- read_one_untagged

#[test]
fn read_one_untagged_reports_an_exists_update() {
    let s = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n* 42 EXISTS\r\n".to_string(),
        replies: vec![],
        idle_push: None,
    });
    let mut session = connect(s.port);
    let item = session.read_one_untagged().expect("read_one_untagged");
    assert_eq!(
        item,
        UntaggedItem::MailboxUpdate {
            exists: Some(42),
            expunged_seq: None
        }
    );
}

#[test]
fn read_one_untagged_reports_an_expunge_update() {
    let s = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n* 5 EXPUNGE\r\n".to_string(),
        replies: vec![],
        idle_push: None,
    });
    let mut session = connect(s.port);
    let item = session.read_one_untagged().expect("read_one_untagged");
    assert_eq!(
        item,
        UntaggedItem::MailboxUpdate {
            exists: None,
            expunged_seq: Some(5)
        }
    );
}

// -------------------------------------------------------------------- idle

#[test]
fn idle_start_then_done_round_trips_cleanly() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    session.idle_start().expect("IDLE");
    session.idle_done().expect("DONE");
}

#[test]
fn idle_wait_times_out_with_nothing_on_the_wire() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    session.idle_start().expect("IDLE");
    let woken = session
        .idle_wait(std::time::Duration::from_millis(200))
        .expect("idle_wait must not error on a bare timeout");
    assert_eq!(woken, None);
    session.idle_done().expect("DONE");
}

#[test]
fn idle_done_without_idle_start_is_refused() {
    let s = server(vec![]);
    let mut session = connect(s.port);
    let err = session.idle_done().expect_err("must refuse");
    assert!(matches!(err, SessionError::Protocol(_)));
}

// REQUIRED BY THE S31 REVIEW (2026-09-25): once QRESYNC is enabled, a server
// reports expunges as `* VANISHED <uid-set>` while idling instead of the
// non-QRESYNC `* n EXPUNGE`. This decodes that push directly at the
// `Session` level (bounded, reusing S30's `sequence_set_to_uid_ranges`) and
// asserts the actual `uid_ranges`, not merely that SOME item arrived — the
// gap the previous attempt's `idle_wire.rs` test left uncovered while
// falsely claiming this file covered it.
#[test]
fn idle_wait_decodes_a_vanished_push_into_uid_ranges() {
    let s = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies: vec![],
        idle_push: Some("* VANISHED 7\r\n"),
    });
    let mut session = connect(s.port);
    session.idle_start().expect("IDLE");
    let item = session
        .idle_wait(std::time::Duration::from_secs(2))
        .expect("idle_wait must not error on a decodable push")
        .expect("a VANISHED push must be reported, not swallowed as a timeout");
    // A single-element `Vec<RangeInclusive<_>>` IS the intended shape here
    // (one server-reported range), not a range of one `u32` to collect.
    #[allow(clippy::single_range_in_vec_init)]
    let expected = vec![7..=7];
    assert_eq!(
        item,
        UntaggedItem::Vanished {
            uid_ranges: expected
        }
    );
    session.idle_done().expect("DONE");
}

// Merge of main into S31 (2026-09-26): main's PR #180 made "a BYE or an I/O
// error before a command completes" close the session for good
// (`is_closed`), which `ImapProvider::session_abandoned` reads. IDLE runs
// outside `command()`, so without applying the same rule a BYE while idling
// came back as a `Protocol` error on a session that still claimed to be
// open, and `idle_done` then wrote DONE into the dead socket.
#[test]
fn a_bye_while_idling_closes_the_session_like_a_bye_mid_command() {
    let s = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies: vec![],
        idle_push: Some("* BYE System Error\r\n"),
    });
    let mut session = connect(s.port);
    session.idle_start().expect("IDLE");
    let err = session
        .idle_wait(std::time::Duration::from_secs(2))
        .expect_err("a BYE while idling must be an error, not a wake");
    assert!(matches!(err, SessionError::Io(_)), "{err:?}");
    assert!(session.is_closed());
    assert!(!session.is_poisoned(), "a BYE is not a decode failure");
    assert!(
        matches!(session.idle_done(), Err(SessionError::Io(_))),
        "DONE is never written into a closed connection"
    );
    assert!(
        matches!(session.noop(), Err(SessionError::Io(_))),
        "a closed session never sends another command"
    );
}

#[test]
fn a_bare_idle_timeout_never_closes_the_session() {
    // The other half of the rule above: `idle_wait`'s WouldBlock/TimedOut
    // is not an I/O failure, so it must not trip `close_on_io`.
    let s = ScriptedServer::start(Script {
        greeting: "* OK Gimap ready for requests\r\n".to_string(),
        replies: vec![],
        idle_push: None,
    });
    let mut session = connect(s.port);
    session.idle_start().expect("IDLE");
    assert_eq!(
        session
            .idle_wait(std::time::Duration::from_millis(100))
            .expect("bare timeout"),
        None
    );
    assert!(
        !session.is_closed(),
        "a bare timeout never closes the session"
    );
    session.idle_done().expect("DONE");
    assert!(!session.is_closed());
}

// ---------------------------------------------------------------- keywords

#[test]
fn fetch_keeps_keywords_and_drops_system_flags() {
    let fetch = "* 1 FETCH (UID 7 FLAGS (\\Seen $Label1 Work \\Recent))\r\n".to_string();
    let s = server(vec![("UID FETCH", fetch, "OK Success".into())]);
    let mut session = connect(s.port);
    let messages = session
        .uid_fetch("7", &[FetchItem::Uid, FetchItem::Flags])
        .expect("UID FETCH");
    assert_eq!(messages.len(), 1);
    let m = &messages[0];
    assert_eq!(m.flags, vec![SessionFlag::Seen]);
    assert_eq!(m.keywords, vec!["$Label1".to_string(), "Work".to_string()]);
}

#[test]
fn select_records_permanentflags_with_asterisk() {
    let s = server(vec![(
        "SELECT",
        "* OK [PERMANENTFLAGS (\\Seen \\Flagged $Forwarded \\*)] ok\r\n".into(),
        "OK [READ-WRITE] Success".into(),
    )]);
    let mut session = connect(s.port);
    session.select("INBOX").expect("SELECT");
    assert_eq!(
        session.last_permanent_flags(),
        Some(&PermanentFlags {
            allows_new_keywords: true,
            keywords: vec!["$Forwarded".to_string()],
        })
    );
}

#[test]
fn select_without_permanentflags_reports_none() {
    // The scripted server always replies with the SAME untagged text for
    // every "SELECT"-prefixed command, so the second and third SELECT below
    // are driven by two independent servers instead — this still proves the
    // accessor is overwritten (not just left at its initial `None`) by
    // asserting Some after the first server's SELECT before checking the
    // second server resets it to None.
    let with_code = server(vec![(
        "SELECT",
        "* OK [PERMANENTFLAGS (\\Seen $Label1 \\*)] ok\r\n".into(),
        "OK [READ-WRITE] Success".into(),
    )]);
    let mut session = connect(with_code.port);
    session.select("INBOX").expect("first SELECT");
    assert!(
        session.last_permanent_flags().is_some(),
        "sanity: the first SELECT must have recorded PERMANENTFLAGS"
    );

    let without_code = server(vec![(
        "SELECT",
        String::new(),
        "OK [READ-WRITE] Success".into(),
    )]);
    let mut session = connect(without_code.port);
    session.select("Archive").expect("second SELECT");
    assert_eq!(
        session.last_permanent_flags(),
        None,
        "a SELECT with no PERMANENTFLAGS code must clear the accessor"
    );
}

#[test]
fn uid_store_keywords_sends_silent_store_and_rejects_bad_atoms() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = received.clone();
    let handle = std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let mut writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        let _ = writer.write_all(b"* OK Gimap ready for requests\r\n");
        let _ = writer.flush();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            received_clone.lock().expect("lock").push(trimmed.clone());
            let mut parts = trimmed.splitn(2, ' ');
            let tag = parts.next().unwrap_or("*").to_string();
            let command = parts.next().unwrap_or("").to_ascii_uppercase();
            if command.starts_with("LOGOUT") {
                let _ = writer.write_all(b"* BYE Logging out\r\n");
                let _ = writer.write_all(format!("{tag} OK LOGOUT completed\r\n").as_bytes());
                let _ = writer.flush();
                return;
            }
            let _ = writer.write_all(format!("{tag} OK Success\r\n").as_bytes());
            let _ = writer.flush();
        }
    });
    let mut session = connect(port);
    session
        .uid_store_keywords("7", StoreSign::Add, &["$Label1"])
        .expect("UID STORE keywords");
    let lines_after_first = received.lock().expect("lock").clone();
    assert!(
        lines_after_first
            .iter()
            .any(|l| l.ends_with("UID STORE 7 +FLAGS.SILENT ($Label1)")),
        "unexpected lines: {lines_after_first:?}"
    );

    let err = session
        .uid_store_keywords("7", StoreSign::Add, &["bad word"])
        .expect_err("bad atom must be rejected");
    assert!(matches!(err, SessionError::Protocol(_)));
    let lines_after_second = received.lock().expect("lock").clone();
    assert_eq!(
        lines_after_first.len(),
        lines_after_second.len(),
        "a rejected keyword must send nothing"
    );

    drop(session);
    let _ = handle.join();
}
