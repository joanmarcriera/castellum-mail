// Castellum - imap-codec wire-shape probe (S27, PR1 of the IMAP migration spike).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Dev-dependency only: proves imap-codec/imap-types (=2.0.0-alpha.7) decode the
// EXACT bytes that panic today's imap/imap-proto stack (see
// native/crates/castellum-provider-imap/src/lib.rs's top-of-file comment and
// tests/gmail_wire.rs), before any production code changes. See
// project-docs/imap-crate-migration-2026-09-17.md §5 (PR1).
//
// This probe feeds every fixture `tests/gmail_wire.rs` uses, not a subset: the
// full CAPABILITY/LOGIN/NAMESPACE/LSUB/LIST prelude (all 9 special-use rows,
// not just the first 3), the `gmail_select()` fixture (PERMANENTFLAGS \*,
// HIGHESTMODSEQ), the NIL-delimiter LIST row from
// `a_nil_delimiter_row_borrows_the_delimiter_another_row_reported`, the
// literal-encoded descriptor FETCH, and the two crash-site regressions
// (untagged NO during SELECT, INTERNALDATE NIL). An earlier draft of this
// probe fed only 3 of the prelude's 9 LSUB/LIST rows and never fed
// `gmail_select()` or the NIL-delimiter row at all while claiming
// byte-for-byte parity with `gmail_wire.rs` — that claim was false; see the
// Deviations note at the end of this stage's plan section for how it was
// caught and fixed.
//
// This file imports nothing from castellum_provider_imap — it is a probe of
// the third-party crate alone.

use imap_codec::decode::Decoder;
use imap_codec::imap_types::response::{Code, Data, Response, Status};
use imap_codec::{GreetingCodec, ResponseCodec};

/// Decode every `Response` in `bytes` in turn, feeding the remainder back in.
/// Panics the TEST (via `expect`) on a decode `Err` — the whole point of this
/// probe is that these exact bytes must decode cleanly. Returns the responses
/// in wire order.
fn decode_all_responses(bytes: &[u8]) -> Vec<Response<'static>> {
    let mut input = bytes;
    let mut out = Vec::new();
    while !input.is_empty() {
        let (remainder, response) = ResponseCodec::default()
            .decode_static(input)
            .unwrap_or_else(|e| panic!("imap-codec failed to decode a response it must accept: {e:?}\nremaining input: {:?}", String::from_utf8_lossy(input)));
        out.push(response);
        if remainder.len() == input.len() {
            panic!("decode_static made no progress; would loop forever");
        }
        input = remainder;
    }
    out
}

/// Gmail's real CAPABILITY/LOGIN/NAMESPACE/LSUB/LIST prelude: every untagged
/// line from `tests/gmail_wire.rs`'s `gmail_prelude()` (all 9 SPECIAL-USE
/// LIST/LSUB rows — INBOX, `[Gmail]`, All Mail, Drafts, Important, Sent Mail,
/// Spam, Starred, Trash — not a subset), with synthetic `a1`..`a5` tags added
/// since this probe decodes raw bytes directly rather than going through
/// `gmail_wire.rs`'s scripted TCP server.
const GMAIL_PRELUDE: &[u8] = b"\
* CAPABILITY IMAP4rev1 UNSELECT IDLE NAMESPACE QUOTA ID XLIST \
CHILDREN X-GM-EXT-1 UIDPLUS COMPRESS=DEFLATE ENABLE MOVE \
CONDSTORE ESEARCH UTF8=ACCEPT LIST-EXTENDED LIST-STATUS \
LITERAL- SPECIAL-USE APPENDLIMIT=35651584\r\n\
a1 OK Success\r\n\
a2 OK pilot@gmail.com authenticated (Success)\r\n\
* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n\
a3 OK Success\r\n\
* LSUB (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
* LSUB (\\HasChildren \\Noselect) \"/\" \"[Gmail]\"\r\n\
* LSUB (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n\
* LSUB (\\Drafts \\HasNoChildren) \"/\" \"[Gmail]/Drafts\"\r\n\
* LSUB (\\HasNoChildren \\Important) \"/\" \"[Gmail]/Important\"\r\n\
* LSUB (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n\
* LSUB (\\HasNoChildren \\Junk) \"/\" \"[Gmail]/Spam\"\r\n\
* LSUB (\\Flagged \\HasNoChildren) \"/\" \"[Gmail]/Starred\"\r\n\
* LSUB (\\HasNoChildren \\Trash) \"/\" \"[Gmail]/Trash\"\r\n\
a4 OK Success\r\n\
* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
* LIST (\\HasChildren \\Noselect) \"/\" \"[Gmail]\"\r\n\
* LIST (\\All \\HasNoChildren) \"/\" \"[Gmail]/All Mail\"\r\n\
* LIST (\\Drafts \\HasNoChildren) \"/\" \"[Gmail]/Drafts\"\r\n\
* LIST (\\HasNoChildren \\Important) \"/\" \"[Gmail]/Important\"\r\n\
* LIST (\\HasNoChildren \\Sent) \"/\" \"[Gmail]/Sent Mail\"\r\n\
* LIST (\\HasNoChildren \\Junk) \"/\" \"[Gmail]/Spam\"\r\n\
* LIST (\\Flagged \\HasNoChildren) \"/\" \"[Gmail]/Starred\"\r\n\
* LIST (\\HasNoChildren \\Trash) \"/\" \"[Gmail]/Trash\"\r\n\
a5 OK Success\r\n";

#[test]
fn gmail_prelude_decodes_without_panicking() {
    let responses = decode_all_responses(GMAIL_PRELUDE);
    // 5 tagged OKs + 20 untagged data lines (1 CAPABILITY + 1 NAMESPACE +
    // 9 LSUB + 9 LIST) = 25.
    assert_eq!(
        responses.len(),
        25,
        "5 tagged OKs + 1 CAPABILITY + 1 NAMESPACE + 9 LSUB + 9 LIST"
    );

    let namespace = responses
        .iter()
        .find(|r| matches!(r, Response::Data(Data::Namespace { .. })))
        .expect("a Data::Namespace response — the NAMESPACE gap that caused the 2026-07-11 crash");
    let Response::Data(Data::Namespace { personal, .. }) = namespace else {
        unreachable!()
    };
    assert_eq!(
        personal.len(),
        1,
        "one personal namespace, Gmail's \"\" prefix"
    );

    let tagged_oks = responses
        .iter()
        .filter(|r| matches!(r, Response::Status(Status::Tagged(_))))
        .count();
    assert_eq!(
        tagged_oks, 5,
        "every tagged OK must still be found — no stranded tag"
    );

    let list_rows: Vec<_> = responses
        .iter()
        .filter(|r| matches!(r, Response::Data(Data::List { .. })))
        .collect();
    let lsub_rows: Vec<_> = responses
        .iter()
        .filter(|r| matches!(r, Response::Data(Data::Lsub { .. })))
        .collect();
    assert_eq!(
        list_rows.len(),
        9,
        "all 9 special-use LIST rows fed, not a subset: {list_rows:?}"
    );
    assert_eq!(
        lsub_rows.len(),
        9,
        "all 9 special-use LSUB rows fed, not a subset: {lsub_rows:?}"
    );

    // Every special-use attribute Gmail sends must have decoded into a typed
    // FlagNameAttribute, not been swallowed as an opaque extension token.
    let list_flags: Vec<String> = list_rows
        .iter()
        .filter_map(|r| match r {
            Response::Data(Data::List { items, .. }) => Some(items),
            _ => None,
        })
        .flatten()
        .map(|f| format!("{f:?}"))
        .collect();
    for special_use in ["Drafts", "Important", "Sent", "Junk", "Flagged", "Trash"] {
        assert!(
            list_flags.iter().any(|f| f.contains(special_use)),
            "expected a LIST row carrying \\{special_use}, decoded flags were {list_flags:?}"
        );
    }
}

/// Gmail's real SELECT response — `tests/gmail_wire.rs`'s `gmail_select()`
/// fixture, including the CONDSTORE `HIGHESTMODSEQ` line and the
/// `PERMANENTFLAGS (... \*)` line (the `\*` "any keyword" wildcard). Neither
/// was fed by the earlier draft of this probe.
const GMAIL_SELECT: &[u8] = b"\
* FLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen $NotPhishing $Phishing)\r\n\
* OK [PERMANENTFLAGS (\\Answered \\Flagged \\Draft \\Deleted \\Seen $NotPhishing $Phishing \\*)] Flags permitted.\r\n\
* OK [UIDVALIDITY 1] UIDs valid.\r\n\
* 12345 EXISTS\r\n\
* 0 RECENT\r\n\
* OK [UIDNEXT 54321] Predicted next UID.\r\n\
* OK [HIGHESTMODSEQ 1234567]\r\n\
a6 OK [READ-WRITE] INBOX selected. (Success)\r\n";

#[test]
fn gmail_select_decodes_without_panicking() {
    let responses = decode_all_responses(GMAIL_SELECT);
    assert_eq!(
        responses.len(),
        8,
        "7 untagged status/data lines + 1 tagged OK"
    );

    assert!(
        responses
            .iter()
            .any(|r| matches!(r, Response::Data(Data::Exists(12345)))),
        "the 12345 EXISTS count must decode: {responses:?}"
    );
    assert!(
        responses
            .iter()
            .any(|r| matches!(r, Response::Data(Data::Recent(0)))),
        "the 0 RECENT count must decode: {responses:?}"
    );
    assert!(
        responses.iter().any(|r| matches!(
            r,
            Response::Status(Status::Untagged(body)) if matches!(body.code, Some(Code::PermanentFlags(_)))
        )),
        "PERMANENTFLAGS (... \\*) must decode as a typed Code::PermanentFlags: {responses:?}"
    );
    assert!(
        responses.iter().any(|r| matches!(
            r,
            Response::Status(Status::Untagged(body)) if matches!(body.code, Some(Code::HighestModSeq(_)))
        )),
        "HIGHESTMODSEQ must decode as a typed Code::HighestModSeq: {responses:?}"
    );
}

/// The NIL-delimiter LIST fixture from `gmail_wire.rs`'s
/// `a_nil_delimiter_row_borrows_the_delimiter_another_row_reported`: a server
/// is allowed to report NIL for a mailbox's hierarchy delimiter, which
/// `list_mailboxes()` must fall back to another row's real delimiter for.
/// This probe only proves the wire bytes decode without panicking — the
/// fallback behaviour itself is `ImapProvider`'s, exercised by the existing
/// GreenMail/scripted-server test, not by this dev-dependency-only probe.
const GMAIL_LIST_NIL_DELIMITER: &[u8] = b"\
* LIST (\\HasNoChildren) NIL \"INBOX\"\r\n\
* LIST (\\HasChildren) \"/\" \"Work\"\r\n\
* LIST (\\HasNoChildren) \"/\" \"Work/Invoices\"\r\n\
a7 OK Success\r\n";

#[test]
fn list_response_with_nil_delimiter_decodes_without_panicking() {
    let responses = decode_all_responses(GMAIL_LIST_NIL_DELIMITER);
    assert_eq!(responses.len(), 4, "3 LIST rows + 1 tagged OK");

    let Response::Data(Data::List { delimiter, .. }) = &responses[0] else {
        panic!(
            "expected the first row to decode as Data::List, got {:?}",
            responses[0]
        );
    };
    assert!(
        delimiter.is_none(),
        "a NIL delimiter must decode as None, not a fabricated separator: {delimiter:?}"
    );

    let Response::Data(Data::List { delimiter, .. }) = &responses[1] else {
        panic!(
            "expected the second row to decode as Data::List, got {:?}",
            responses[1]
        );
    };
    assert_eq!(
        delimiter.as_ref().map(|d| d.inner()),
        Some('/'),
        "the sibling row's real delimiter must still decode: {delimiter:?}"
    );
}

/// Gmail's real descriptor FETCH: RFC2047-hostile UTF-8 subject sent as an
/// IMAP literal, angle-bracketed Message-ID, nested multipart BODYSTRUCTURE —
/// the same fixture bytes as `tests/gmail_wire.rs`'s `gmail_descriptor_fetch()`
/// (that fixture omits the trailing tagged line, since its scripted server
/// appends the client's own tag; this probe appends a synthetic `a6 OK` to
/// decode a complete response on its own).
#[test]
fn gmail_descriptor_fetch_with_a_literal_subject_decodes_without_panicking() {
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
         \"mixed\" (\"boundary\" \"000000000000112233\") NIL NIL NIL))\r\n\
         a6 OK Success\r\n",
        len = subject.len(),
    );

    let responses = decode_all_responses(fetch.as_bytes());
    assert_eq!(responses.len(), 2, "one FETCH data response, one tagged OK");
    assert!(
        matches!(&responses[0], Response::Data(Data::Fetch { .. })),
        "expected a Data::Fetch, got {:?}",
        responses[0]
    );
}

// MARK: - crash-site regressions: these MUST return `Err` or a typed status,
// never panic.

/// `imap` 2.4.1's `parse_mailbox` hits `unreachable!()` on this exact shape
/// (an untagged `* NO` during SELECT — real servers send quota warnings this
/// way). `imap-codec` must return a typed `Err`, not panic, not succeed with
/// garbage — a `Response::Status(Status::Untagged(..))` line with `kind: No`
/// parses as an ordinary status the caller can act on; it is `EXISTS` that
/// then has nowhere sane to attach, so the test only asserts non-panic plus a
/// correctly recognised NO status line.
#[test]
fn untagged_no_during_select_does_not_panic() {
    let bytes = b"\
* FLAGS (\\Seen)\r\n\
* NO [ALERT] Mailbox is approaching its quota\r\n\
* OK [UIDVALIDITY 1] UIDs valid.\r\n\
* 1 EXISTS\r\n\
a7 OK [READ-WRITE] INBOX selected.\r\n";

    let responses = decode_all_responses(bytes);
    let has_untagged_no = responses.iter().any(|r| {
        matches!(
            r,
            Response::Status(Status::Untagged(body)) if body.kind == imap_codec::imap_types::response::StatusKind::No
        )
    });
    assert!(
        has_untagged_no,
        "the untagged NO must decode as an ordinary status line: {responses:?}"
    );
}

/// `imap-proto` 0.10.2's `msg_att_internal_date` unwraps a `None` and panics
/// on `INTERNALDATE NIL`. RFC 3501's grammar never allows NIL here (INTERNALDATE
/// is a mandatory quoted date-time), so a server sending it is protocol-hostile;
/// imap-codec's nom parser must reject the line with a typed `Err`, never panic.
#[test]
fn internaldate_nil_is_rejected_not_panicked_on() {
    let bytes = b"* 1 FETCH (UID 7 RFC822.SIZE 10 FLAGS () INTERNALDATE NIL)\r\n";
    let result = ResponseCodec::default().decode_static(bytes);
    assert!(
        result.is_err(),
        "INTERNALDATE NIL violates the grammar and must be a decode Err, not a fabricated Ok: {result:?}"
    );
}

#[test]
fn synthetic_namespace_response_decodes_into_typed_data() {
    let bytes = b"* NAMESPACE ((\"\" \"/\")) NIL NIL\r\n";
    let (remainder, response) = ResponseCodec::default()
        .decode_static(bytes)
        .expect("a well-formed NAMESPACE response must decode");
    assert!(remainder.is_empty());
    let Response::Data(Data::Namespace {
        personal,
        other,
        shared,
    }) = response
    else {
        panic!("expected Data::Namespace, got {response:?}");
    };
    assert_eq!(personal.len(), 1);
    assert!(other.is_empty());
    assert!(shared.is_empty());
}

#[test]
fn gmail_greeting_decodes() {
    let (remainder, greeting) = GreetingCodec::default()
        .decode_static(b"* OK Gimap ready for requests\r\n")
        .expect("Gmail's real greeting must decode");
    assert!(remainder.is_empty());
    assert_eq!(
        greeting.kind,
        imap_codec::imap_types::response::GreetingKind::Ok
    );
}
