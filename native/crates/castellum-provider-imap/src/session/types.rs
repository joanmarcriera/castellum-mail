// Castellum - public DTOs of the codec-backed IMAP session.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Everything here is owned (`'static`): no `imap_types` lifetime leaks past the
// module boundary, so callers never juggle `'a`.

/// Errors from a `Session` call. Never raised from a caught panic — every
/// variant here is an ordinary `Result::Err` from a typed decode failure or a
/// tagged NO/BAD completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The transport (TCP/TLS) failed.
    Io(String),
    /// A response (or the greeting) could not be decoded, or the session was
    /// already poisoned by an earlier such failure.
    Protocol(String),
    /// A tagged `NO` completion for the command that was sent.
    No { command: &'static str, text: String },
    /// A tagged `BAD` completion for the command that was sent.
    Bad { command: &'static str, text: String },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(m) => write!(f, "imap session io error: {m}"),
            SessionError::Protocol(m) => write!(f, "imap session protocol error: {m}"),
            SessionError::No { command, text } => write!(f, "{command}: NO {text}"),
            SessionError::Bad { command, text } => write!(f, "{command}: BAD {text}"),
        }
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxListItem {
    pub path: String,
    pub delimiter: Option<String>,
    /// Raw SPECIAL-USE / name attribute tokens as the server sent them,
    /// canonical spelling (e.g. `"\\Trash"`, `"\\Noselect"`). Role resolution
    /// is the caller's job.
    pub special_use: Vec<String>,
    pub no_select: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedMailbox {
    /// `None` when the server didn't report UIDVALIDITY (treat as unknown —
    /// forces a full refetch, same policy as `ImapProvider::select` today).
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub exists: u32,
    pub read_write: bool,
    /// The server's `[HIGHESTMODSEQ n]` response code, once CONDSTORE/QRESYNC
    /// has been `ENABLE`d — RFC 7162 §3.1.4 requires every successful
    /// SELECT/EXAMINE to report either this or `no_modseq`.
    pub highest_modseq: Option<u64>,
    /// The server's `[NOMODSEQ]` response code: it does not persist
    /// mod-sequences for THIS mailbox, so incremental sync can never be used
    /// here even though the connection negotiated CONDSTORE/QRESYNC overall.
    pub no_modseq: bool,
}

/// RFC 9051 §7.1: the flags a client may change permanently, from the last
/// SELECT/EXAMINE's `[PERMANENTFLAGS ...]` response code. `Session::select`
/// callers with no such code get `None` from `last_permanent_flags` (the RFC
/// default is "everything is permanent"); this type only exists once a
/// server actually sent the code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PermanentFlags {
    /// The server sent `\*`: any new keyword may be created and will stick.
    pub allows_new_keywords: bool,
    /// The specific keywords (server spelling) the code listed as
    /// permanent, when `\*` was not also present.
    pub keywords: Vec<String>,
}

/// `Session::select_qresync`'s result: the SELECT outcome plus every
/// VANISHED/changed-FETCH item the server piggy-backed on the same command
/// (RFC 7162 §3.6.1).
#[derive(Debug, Clone, PartialEq)]
pub struct QresyncSelectResult {
    pub mailbox: SelectedMailbox,
    pub changed: Vec<FetchedMessage>,
    /// Every `VANISHED` item's UID set as the inclusive ranges the server
    /// sent, in wire order (not merged or expanded — see
    /// `sequence_set_to_uid_ranges` for why expanding is never safe).
    pub vanished_uid_ranges: Vec<std::ops::RangeInclusive<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionFlag {
    Answered,
    Deleted,
    Draft,
    Flagged,
    Seen,
}

/// RFC 4315 UIDPLUS APPENDUID response code, when the server reports one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendUid {
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreSign {
    Add,
    Remove,
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchItem {
    Uid,
    Flags,
    Rfc822Size,
    Envelope,
    InternalDate,
    BodyStructure,
    BodyPeek,
}

/// The destination of a single message moved or copied, from the server's
/// `[COPYUID uidvalidity src dst]` response code (RFC 4315 UIDPLUS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyUid {
    pub uid_validity: u32,
    pub uid: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchedMessage {
    pub uid: Option<u32>,
    pub flags: Vec<SessionFlag>,
    /// IMAP keywords (`Flag::Keyword`) the FETCH's FLAGS item carried, server
    /// spelling, `\Recent` and system flags excluded. Empty when the FETCH
    /// did not request FLAGS or the server reported none.
    pub keywords: Vec<String>,
    pub size_bytes: Option<u32>,
    pub envelope: Option<SessionEnvelope>,
    pub internal_date_epoch: Option<i64>,
    pub has_attachments: Option<bool>,
    /// Present only when `FetchItem::BodyPeek` was requested and the server
    /// returned a non-NIL body.
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEnvelope {
    pub subject: Option<String>,
    pub from: Vec<SessionAddress>,
    pub to: Vec<SessionAddress>,
    pub cc: Vec<SessionAddress>,
    pub bcc: Vec<SessionAddress>,
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    /// The ENVELOPE `date` (the message's `Date:` header) as a Unix
    /// timestamp, parsed with the same RFC 5322 parser body ingest uses so a
    /// list row's `sent_at` has ONE clock before and after hydration (#3040).
    /// `None` when absent or unparseable.
    pub date_epoch: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAddress {
    pub name: Option<String>,
    pub address: String,
}

/// One `UID SEARCH` criterion for `Session::uid_search_all`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSearch<'a> {
    Uid(u32),
    Seen,
    Unseen,
    Header { field: &'a str, value: &'a str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceResult {
    pub personal_delimiter: Option<String>,
    /// Every namespace the server reported (personal, other users, shared),
    /// in response order. RFC 2342 allows a different delimiter in each.
    pub entries: Vec<NamespaceEntry>,
}

/// Which RFC 2342 namespace class an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamespaceKind {
    Personal,
    Other,
    Shared,
}

/// One `(prefix delimiter)` pair from a NAMESPACE response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceEntry {
    pub kind: NamespaceKind,
    /// Mailbox-name prefix ("" for the personal root, e.g. "Other Users/").
    pub prefix: String,
    /// `None` = NIL (flat namespace).
    pub delimiter: Option<String>,
}

impl NamespaceResult {
    /// Delimiter for `path`: the entry with the longest matching prefix that
    /// reports a delimiter. A path equal to a prefix minus its trailing
    /// delimiter ("Shared" for "Shared/") matches that entry too. `INBOX`
    /// (case-insensitive) always belongs to the personal namespace.
    pub fn delimiter_for(&self, path: &str) -> Option<String> {
        let is_inbox = path.eq_ignore_ascii_case("INBOX");
        self.entries
            .iter()
            .filter(|e| e.delimiter.is_some())
            .filter(|e| {
                if is_inbox {
                    return e.kind == NamespaceKind::Personal;
                }
                let d = e.delimiter.as_deref().unwrap_or("");
                path.starts_with(&e.prefix)
                    || (!d.is_empty() && e.prefix.strip_suffix(d) == Some(path))
            })
            .max_by_key(|e| if is_inbox { 0 } else { e.prefix.len() })
            .and_then(|e| e.delimiter.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntaggedItem {
    MailboxUpdate {
        exists: Option<u32>,
        expunged_seq: Option<u32>,
    },
    /// `* VANISHED [(EARLIER)] <uid-set>` received while idling — only
    /// possible once QRESYNC is enabled (S30). Same bounded-range shape as
    /// `QresyncSelectResult::vanished_uid_ranges`: never expanded UID by UID
    /// (see `sequence_set_to_uid_ranges`'s doc comment for why).
    Vanished {
        uid_ranges: Vec<std::ops::RangeInclusive<u32>>,
    },
    Other,
}

#[cfg(test)]
mod namespace_tests {
    use super::*;

    fn entry(kind: NamespaceKind, prefix: &str, d: Option<&str>) -> NamespaceEntry {
        NamespaceEntry {
            kind,
            prefix: prefix.into(),
            delimiter: d.map(String::from),
        }
    }

    fn ns() -> NamespaceResult {
        NamespaceResult {
            personal_delimiter: Some("/".into()),
            entries: vec![
                entry(NamespaceKind::Personal, "", Some("/")),
                entry(NamespaceKind::Other, "Other Users/", Some("#")),
                entry(NamespaceKind::Shared, "Shared.", Some(".")),
            ],
        }
    }

    #[test]
    fn other_users_and_shared_prefixes_use_their_own_delimiter() {
        let n = ns();
        assert_eq!(
            n.delimiter_for("Other Users/bob/Inbox").as_deref(),
            Some("#")
        );
        assert_eq!(n.delimiter_for("Shared.team").as_deref(), Some("."));
        assert_eq!(n.delimiter_for("Archive/2024").as_deref(), Some("/"));
    }

    #[test]
    fn prefix_boundary_shared_vs_shared_slash() {
        let n = ns();
        // The bare namespace root matches "Shared." minus its delimiter...
        assert_eq!(n.delimiter_for("Shared").as_deref(), Some("."));
        // ...but a sibling that merely starts with the same letters does not.
        assert_eq!(n.delimiter_for("Shared2").as_deref(), Some("/"));
        assert_eq!(n.delimiter_for("Sharedfolder/x").as_deref(), Some("/"));
    }

    #[test]
    fn inbox_always_belongs_to_the_personal_namespace() {
        let n = ns();
        assert_eq!(n.delimiter_for("INBOX").as_deref(), Some("/"));
        assert_eq!(n.delimiter_for("inbox").as_deref(), Some("/"));
        // Even if a shared prefix is empty-ish/overlapping, INBOX stays personal.
        let n = NamespaceResult {
            personal_delimiter: Some("/".into()),
            entries: vec![
                entry(NamespaceKind::Shared, "", Some(".")),
                entry(NamespaceKind::Personal, "", Some("/")),
            ],
        };
        assert_eq!(n.delimiter_for("INBOX").as_deref(), Some("/"));
    }

    #[test]
    fn nil_delimiter_entries_are_skipped() {
        let n = NamespaceResult {
            personal_delimiter: None,
            entries: vec![entry(NamespaceKind::Personal, "", None)],
        };
        assert_eq!(n.delimiter_for("INBOX"), None);
    }
}
