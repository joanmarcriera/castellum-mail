// Castellum - codec-backed IMAP session.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Built on imap-codec/imap-types (pinned =2.0.0-alpha.7) instead of the
// imap/imap-proto crates ImapProvider uses today. Sans-I/O parser/encoder: we
// keep the existing blocking TcpStream/native-tls socket code and only replace
// command-building/response-parsing. See the "Session API" section of
// project-docs/plans/2026-09-18-imap-codec-migration.md for the full contract
// and project-docs/imap-crate-migration-2026-09-17.md for why this crate pair
// was chosen (§4).
//
// Usage: `Session::connect(stream)` (any `Read + Write`), then `login`, then
// the per-command methods. One command is in flight at a time (no
// pipelining), so the first tagged status seen is that command's completion.
//
// NEVER let a decode error here turn into a panic — that is this module's
// entire reason to exist (see the top-of-file comment in ../lib.rs). A decode
// failure poisons the session: later calls fail fast with
// `SessionError::Protocol` instead of touching a possibly desynchronised
// byte stream.

mod types;
pub use types::*;

use std::io::{Read, Write};
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::RangeInclusive;

use crate::encoded_words::decode_encoded_words;
use crate::ReadTimeout;

use base64::engine::general_purpose::STANDARD as base64_engine;
use base64::Engine;
use chrono::NaiveDate;
use imap_codec::decode::{Decoder, GreetingDecodeError, ResponseDecodeError};
use imap_codec::encode::{Encoder, Fragment};
use imap_codec::imap_types::auth::AuthMechanism;
use imap_codec::imap_types::body::{BasicFields, Body, BodyStructure, Disposition, SpecificFields};
use imap_codec::imap_types::command::{Command, CommandBody, SelectParameter};
use imap_codec::imap_types::core::{AString, Atom, IString, LiteralMode, NString, Text, Vec1};
use imap_codec::imap_types::envelope::{Address, Envelope};
use imap_codec::imap_types::extensions::enable::CapabilityEnable;
use imap_codec::imap_types::fetch::{
    MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName, Part, Section,
};
use imap_codec::imap_types::flag::{
    Flag, FlagFetch, FlagNameAttribute, FlagPerm, StoreResponse, StoreType,
};
use imap_codec::imap_types::mailbox::Mailbox;
use imap_codec::imap_types::response::{
    Code, Data, Greeting, GreetingKind, Response, Status, StatusKind,
};
use imap_codec::imap_types::search::SearchKey;
use imap_codec::imap_types::sequence::{Sequence, SequenceSet};
use imap_codec::imap_types::IntoStatic;
use imap_codec::{CommandCodec, GreetingCodec, ResponseCodec};

/// A connected IMAP session speaking the wire protocol through imap-codec/
/// imap-types. See the "Session API" section of
/// project-docs/plans/2026-09-18-imap-codec-migration.md for the full contract.
pub struct Session<S: Read + Write> {
    stream: S,
    read_buf: Vec<u8>,
    next_tag: u64,
    poisoned: bool,
    /// True between a successful `idle_start` and its paired `idle_done`.
    /// No other command may be sent while this is set.
    idling: bool,
    /// The connection is gone: an I/O error mid-command, or a `* BYE`
    /// before a non-LOGOUT command completed (the server hangs up after it).
    /// Like `poisoned`, permanent — every later command fails fast.
    closed: bool,
    /// The last SELECT/EXAMINE/`select_qresync`'s `[PERMANENTFLAGS ...]`
    /// code, or `None` when that command carried no such code. A
    /// side-channel accessor like `last_highest_modseq` (S30) rather than a
    /// `SelectedMailbox` field, so `SelectedMailbox` stays `Copy`.
    last_permanent_flags: Option<PermanentFlags>,
    /// Socket read timeout in force outside IDLE (restored by `idle_wait`).
    default_read_timeout: Option<std::time::Duration>,
}

// Manual Debug: never print `read_buf` (may hold in-flight server text) and
// never print credentials (they are not stored past login).
impl<S: Read + Write> std::fmt::Debug for Session<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("stream", &"<imap stream>")
            .field("poisoned", &self.poisoned)
            .field("closed", &self.closed)
            .finish()
    }
}

/// Everything a completed command reported: untagged `Data`, any `Code`s
/// carried by untagged status lines (e.g. `[UIDVALIDITY 1]`), and the `Code`
/// on the tagged completion itself (e.g. `[READ-WRITE]`).
struct CommandOutcome {
    data: Vec<Data<'static>>,
    untagged_codes: Vec<Code<'static>>,
    tagged_code: Option<Code<'static>>,
}

/// The single-message COPYUID of a COPY/MOVE completion, if any. A range or a
/// multi-message answer is `None`: the caller moved one UID, so anything else
/// is not trustworthy enough to address a message by later.
fn copy_uid_of(outcome: &CommandOutcome) -> Option<CopyUid> {
    outcome
        .untagged_codes
        .iter()
        .chain(outcome.tagged_code.iter())
        .find_map(|code| match code {
            Code::CopyUid {
                uid_validity,
                destination,
                ..
            } => match destination.0.as_ref() {
                [imap_codec::imap_types::extensions::uidplus::UidElement::Single(uid)] => {
                    Some(CopyUid {
                        uid_validity: uid_validity.get(),
                        uid: uid.get(),
                    })
                }
                _ => None,
            },
            _ => None,
        })
}

fn text_to_string(text: &Text<'_>) -> String {
    text.inner().to_string()
}

/// Shared by `select_or_examine` and `select_qresync`: scan a completed
/// SELECT/EXAMINE's untagged AND tagged codes for UIDVALIDITY/UIDNEXT/
/// HIGHESTMODSEQ/NOMODSEQ (RFC 7162 §3.1.4 allows a server to report
/// HIGHESTMODSEQ on either), plus EXISTS and the READ-WRITE/READ-ONLY code.
fn selected_mailbox_from_outcome(outcome: &CommandOutcome) -> SelectedMailbox {
    let exists = outcome
        .data
        .iter()
        .find_map(|d| match d {
            Data::Exists(n) => Some(*n),
            _ => None,
        })
        .unwrap_or(0);

    let mut uid_validity = None;
    let mut uid_next = None;
    let mut highest_modseq = None;
    let mut no_modseq = false;
    let codes = outcome
        .untagged_codes
        .iter()
        .chain(outcome.tagged_code.iter());
    for code in codes {
        match code {
            Code::UidValidity(n) => uid_validity = Some(n.get()),
            Code::UidNext(n) => uid_next = Some(n.get()),
            Code::HighestModSeq(n) => highest_modseq = Some(n.get()),
            Code::NoModSeq => no_modseq = true,
            _ => {}
        }
    }
    let read_write = !matches!(outcome.tagged_code, Some(Code::ReadOnly));

    SelectedMailbox {
        uid_validity,
        uid_next,
        exists,
        read_write,
        highest_modseq,
        no_modseq,
    }
}

/// RFC 9051 §7.1: PERMANENTFLAGS lists the flags the client may change
/// permanently; `\*` means new keywords may be created. Returns `None` when
/// the server sent no PERMANENTFLAGS code for this SELECT/EXAMINE.
fn permanent_flags_from_outcome(outcome: &CommandOutcome) -> Option<PermanentFlags> {
    let codes = outcome
        .untagged_codes
        .iter()
        .chain(outcome.tagged_code.iter());
    codes.into_iter().find_map(|code| match code {
        Code::PermanentFlags(perms) => {
            let mut pf = PermanentFlags::default();
            for p in perms {
                match p {
                    FlagPerm::Asterisk => pf.allows_new_keywords = true,
                    FlagPerm::Flag(Flag::Keyword(atom)) => {
                        pf.keywords.push(atom.as_ref().to_string())
                    }
                    FlagPerm::Flag(_) => {}
                }
            }
            Some(pf)
        }
        _ => None,
    })
}

/// A `VANISHED` response's UID set as the inclusive ranges it was sent as,
/// in wire order. Deliberately NOT expanded UID by UID: a 13-byte
/// `1:4294967295` would otherwise allocate ~16 GiB (CLAUDE.md gotcha 1 — a
/// hostile server must never be able to crash us), so the result is O(wire
/// size) whatever the ranges span. A reversed range (`5:3`) comes back
/// low-to-high; `*`, which RFC 7162 never sends in VANISHED, maps to
/// `u32::MAX` rather than failing.
fn sequence_set_to_uid_ranges(set: &SequenceSet) -> impl Iterator<Item = RangeInclusive<u32>> + '_ {
    set.0.as_ref().iter().map(|sequence| {
        let (a, b) = match sequence {
            Sequence::Single(v) => (*v, *v),
            Sequence::Range(a, b) => (*a, *b),
        };
        let (a, b) = (a.to_non_zero_u32().get(), b.to_non_zero_u32().get());
        a.min(b)..=a.max(b)
    })
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

fn io_err(e: std::io::Error) -> SessionError {
    match e.kind() {
        // A socket read/write timeout surfaces as WouldBlock (Unix) or
        // TimedOut (Windows); say so instead of a raw "os error 35".
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
            SessionError::Io("timed out waiting for the server".into())
        }
        _ => SessionError::Io(e.to_string()),
    }
}

impl<S: Read + Write> Session<S> {
    /// Read the server greeting and construct a `Session`. TLS/plaintext socket
    /// setup remains the caller's job (mirrors `ImapProvider::connect` today).
    pub fn connect(stream: S) -> Result<Self, SessionError> {
        let mut session = Session {
            stream,
            read_buf: Vec::new(),
            next_tag: 0,
            poisoned: false,
            idling: false,
            closed: false,
            last_permanent_flags: None,
            default_read_timeout: None,
        };
        let greeting = session.decode_greeting()?;
        if matches!(greeting.kind, GreetingKind::Bye) {
            return Err(SessionError::Protocol(
                "server sent a BYE greeting and refused the connection".into(),
            ));
        }
        Ok(session)
    }

    /// Build a session on a stream whose greeting was already read: the TLS
    /// stream after `starttls`. Reads nothing; nothing from before the
    /// handshake carries over (RFC 3501 §6.2.1: capabilities must be asked
    /// again, which the caller's post-login CAPABILITY does).
    pub fn resume_after_starttls(stream: S) -> Self {
        Session {
            stream,
            read_buf: Vec::new(),
            next_tag: 0,
            poisoned: false,
            idling: false,
            closed: false,
            last_permanent_flags: None,
            default_read_timeout: None,
        }
    }

    /// STARTTLS: ask the server to start TLS
    /// and hand the raw stream back for the handshake. Consumes the session,
    /// so no state learned in cleartext survives. A NO/BAD answer is an
    /// error (the caller must not fall back to cleartext), and so is ANY
    /// byte buffered after the tagged OK: a man in the middle can inject
    /// responses there that would otherwise be read as if they had arrived
    /// over TLS (the CVE-2011-0411 class of bug).
    ///
    /// Sent as a literal line rather than through imap-codec's `CommandBody`,
    /// which only has STARTTLS behind a feature this crate does not enable;
    /// the reply still goes through the same typed decoder, so an
    /// undecodable answer is a `SessionError`, never a panic.
    pub fn starttls(mut self) -> Result<S, SessionError> {
        self.poison_check()?;
        let tag = self.fresh_tag();
        self.stream
            .write_all(format!("{tag} STARTTLS\r\n").as_bytes())
            .map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;
        self.read_until_tagged("STARTTLS")?;
        if !self.read_buf.is_empty() {
            return Err(SessionError::Protocol(
                "STARTTLS: the server sent more data after its OK, before the TLS \
                 handshake; refusing to continue (possible response injection)"
                    .into(),
            ));
        }
        Ok(self.stream)
    }

    /// The read timeout the socket carries outside IDLE; `idle_wait` restores
    /// it (never `None`) after its own short deadline.
    pub fn set_default_read_timeout(&mut self, d: Option<std::time::Duration>) {
        self.default_read_timeout = d;
    }

    /// True once an undecodable response has poisoned this session: every
    /// later command fails fast, and the only way forward is a new
    /// connection (CLAUDE.md gotcha 1). Lets a caller tell a decode failure
    /// (poisons) from a local `SessionError::Protocol` such as an invalid
    /// command argument (does not).
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// True once the connection is gone (see the `closed` field): a caller
    /// must reconnect, exactly as for `is_poisoned`, but it was NOT a decode
    /// failure — descriptor isolation must not treat it as one.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn poison_check(&self) -> Result<(), SessionError> {
        if self.closed {
            return Err(SessionError::Io(
                "imap connection closed by an earlier I/O error or server BYE; reconnect required"
                    .into(),
            ));
        }
        if self.poisoned {
            return Err(SessionError::Protocol(
                "imap session poisoned by an earlier undecodable response; reconnect required"
                    .into(),
            ));
        }
        Ok(())
    }

    fn poison(&mut self, reason: impl Into<String>) -> SessionError {
        self.poisoned = true;
        SessionError::Protocol(reason.into())
    }

    fn fresh_tag(&mut self) -> String {
        self.next_tag += 1;
        format!("C{}", self.next_tag)
    }

    /// Read at least one more chunk of bytes from the socket into `read_buf`.
    fn read_more(&mut self) -> Result<(), SessionError> {
        let mut chunk = [0u8; 4096];
        let n = self.stream.read(&mut chunk).map_err(io_err)?;
        if n == 0 {
            return Err(SessionError::Io(
                "connection closed while waiting for more data".into(),
            ));
        }
        self.read_buf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn decode_greeting(&mut self) -> Result<Greeting<'static>, SessionError> {
        loop {
            match GreetingCodec::default().decode_static(&self.read_buf) {
                Ok((remainder, greeting)) => {
                    let consumed = self.read_buf.len() - remainder.len();
                    self.read_buf.drain(..consumed);
                    return Ok(greeting);
                }
                Err(GreetingDecodeError::Incomplete) => self.read_more()?,
                Err(GreetingDecodeError::Failed) => {
                    return Err(self.poison("server sent a greeting that could not be decoded"));
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // The command/response engine
    // ------------------------------------------------------------------

    /// Send `body` under a fresh tag, drive literal fragments to completion,
    /// then read responses until the tagged completion arrives. A tagged
    /// NO/BAD becomes `SessionError::No`/`Bad` naming `command_name`. A
    /// `Status::Bye` before the tagged completion ends LOGOUT as a success
    /// (its normal shape); for any other command it is an unsolicited
    /// shutdown — the server hangs up with the response unfinished — so it is
    /// a `SessionError::Io` and the session is closed (review of PR #180,
    /// round 3: returning the partial data as a success made a descriptor
    /// chunk look complete and its checkpoint pass UIDs never fetched).
    fn command(
        &mut self,
        command_name: &'static str,
        body: CommandBody<'_>,
    ) -> Result<CommandOutcome, SessionError> {
        self.poison_check()?;
        let tag = self.fresh_tag();
        let command = Command::new(tag.as_str(), body).map_err(|e| {
            SessionError::Protocol(format!("invalid {command_name} command: {e:?}"))
        })?;
        let result = self
            .send_fragments(command_name, &command)
            .and_then(|()| self.read_until_tagged(command_name));
        // An I/O error part-way through a command leaves the stream in an
        // unknown state (half a response read, or the peer gone): never
        // send on it again.
        if matches!(result, Err(SessionError::Io(_))) {
            self.closed = true;
        }
        result
    }

    fn send_fragments(
        &mut self,
        command_name: &'static str,
        command: &Command<'_>,
    ) -> Result<(), SessionError> {
        for fragment in CommandCodec::default().encode(command) {
            match fragment {
                Fragment::Line { data } => self.stream.write_all(&data).map_err(io_err)?,
                Fragment::Literal { data, mode } => {
                    if matches!(mode, LiteralMode::Sync) {
                        self.await_continuation(command_name)?;
                    }
                    self.stream.write_all(&data).map_err(io_err)?;
                }
            }
        }
        self.stream.flush().map_err(io_err)
    }

    /// Read responses until the tagged completion for the command currently in
    /// flight arrives (see the "never pipelines" note at the top of the file).
    fn read_until_tagged(
        &mut self,
        command_name: &'static str,
    ) -> Result<CommandOutcome, SessionError> {
        let mut collected = Vec::new();
        let mut untagged_codes = Vec::new();
        loop {
            match self.decode_response(command_name)? {
                Response::Data(data) => collected.push(data),
                Response::CommandContinuationRequest(_) => {
                    return Err(self.poison(format!(
                        "{command_name}: unexpected continuation request outside a literal"
                    )));
                }
                Response::Status(Status::Untagged(status_body)) => {
                    if let Some(code) = status_body.code {
                        untagged_codes.push(code);
                    }
                }
                Response::Status(Status::Bye(_)) if command_name == "LOGOUT" => {
                    return Ok(CommandOutcome {
                        data: collected,
                        untagged_codes,
                        tagged_code: None,
                    });
                }
                Response::Status(Status::Bye(_)) => {
                    return Err(SessionError::Io(format!(
                        "{command_name}: server sent BYE and closed the connection before \
                         completing the command"
                    )));
                }
                Response::Status(Status::Tagged(tagged)) => {
                    return match tagged.body.kind {
                        StatusKind::Ok => Ok(CommandOutcome {
                            data: collected,
                            untagged_codes,
                            tagged_code: tagged.body.code,
                        }),
                        StatusKind::No => Err(SessionError::No {
                            command: command_name,
                            text: text_to_string(&tagged.body.text),
                        }),
                        StatusKind::Bad => Err(SessionError::Bad {
                            command: command_name,
                            text: text_to_string(&tagged.body.text),
                        }),
                    };
                }
            }
        }
    }

    fn decode_response(
        &mut self,
        command_name: &'static str,
    ) -> Result<Response<'static>, SessionError> {
        loop {
            if let Some(response) = self.try_decode_response(command_name)? {
                return Ok(response);
            }
            self.read_more()?;
        }
    }

    /// Try to decode one response already buffered in `read_buf`, WITHOUT
    /// reading the socket. `Ok(None)` means "need more bytes" — the blocking
    /// `decode_response` above reads more and retries; `idle_wait` (added in
    /// S31, below) instead does a bounded, timed read between retries so the
    /// IDLE loop can periodically check its shutdown/nudge channel.
    ///
    /// A strict decode failure gets at most ONE attempt of each repair per
    /// response, applied in this order (so they can stack on one message):
    ///
    /// 1. `repair_quoted_8bit`: real servers echo raw 8-bit header bytes
    ///    inside quoted ENVELOPE strings (RFC 3501 forbids it; spam does it
    ///    constantly). Rewrites those strings as literals — lossless.
    /// 2. `repair_drop_bodystructure`: real servers (Gmail, 2026-09-22
    ///    pilot) send BODYSTRUCTUREs that violate the grammar, e.g. a
    ///    multipart with zero parts inside an embedded message/rfc822.
    ///    Removes the BODYSTRUCTURE/BODY item from that one FETCH line, so
    ///    the message still arrives with its UID/flags/envelope and
    ///    `has_attachments: None` — lossy, hence second.
    /// 3. `repair_vanished_seq_prefix`: pinned `imap-codec` 2.0.0-alpha.7
    ///    has its own grammar bug (fixed upstream in alpha.9, confirmed by
    ///    diffing `message_data` between the two): RFC 7162's `VANISHED`
    ///    line carries NO sequence number (`"* VANISHED (EARLIER) ..."`),
    ///    but alpha.7's parser nests the `VANISHED` alternative inside the
    ///    same `nz-number SP (...)` prefix it uses for EXPUNGE/FETCH, so a
    ///    real, spec-compliant `VANISHED` line never decodes. Inserts a
    ///    synthetic `"1 "` after `"* "` — alpha.7's parser consumes and
    ///    discards that captured `seq` for the `Vanished` arm, so the
    ///    resulting `Data::Vanished { earlier, known_uids }` is byte-for-
    ///    byte identical to what alpha.9 would have produced directly.
    ///    S30 deviation — see DECISIONS.md. This applies equally to a
    ///    VANISHED that arrives mid-IDLE (S31), which is why this repair
    ///    loop is shared rather than only reachable from `decode_response`.
    ///
    /// All three only rewrite bytes inside the first, fully-framed response
    /// and the codec must then accept the result; anything else still
    /// poisons.
    fn try_decode_response(
        &mut self,
        command_name: &'static str,
    ) -> Result<Option<Response<'static>>, SessionError> {
        let mut repaired_8bit = false;
        let mut dropped_bodystructure = false;
        let mut repaired_vanished = false;
        loop {
            match ResponseCodec::default().decode_static(&self.read_buf) {
                Ok((remainder, response)) => {
                    let consumed = self.read_buf.len() - remainder.len();
                    self.read_buf.drain(..consumed);
                    return Ok(Some(response));
                }
                Err(ResponseDecodeError::Incomplete)
                | Err(ResponseDecodeError::LiteralFound { .. }) => return Ok(None),
                Err(ResponseDecodeError::Failed) => {
                    if !repaired_8bit {
                        match repair_quoted_8bit(&self.read_buf) {
                            Repair::Rewritten(fixed) => {
                                self.read_buf = fixed;
                                repaired_8bit = true;
                                continue;
                            }
                            Repair::NeedMore => return Ok(None),
                            Repair::Nothing => {}
                        }
                    }
                    if !dropped_bodystructure {
                        match repair_drop_bodystructure(&self.read_buf) {
                            Repair::Rewritten(fixed) => {
                                self.read_buf = fixed;
                                dropped_bodystructure = true;
                                continue;
                            }
                            Repair::NeedMore => return Ok(None),
                            Repair::Nothing => {}
                        }
                    }
                    if !repaired_vanished {
                        match repair_vanished_seq_prefix(&self.read_buf) {
                            Repair::Rewritten(fixed) => {
                                self.read_buf = fixed;
                                repaired_vanished = true;
                                continue;
                            }
                            Repair::NeedMore => return Ok(None),
                            Repair::Nothing => {}
                        }
                    }
                    return Err(self.poison(format!(
                        "{command_name}: server sent a response that could not be decoded"
                    )));
                }
            }
        }
    }

    /// Wait for the server's `+ ...` continuation request before sending a
    /// sync literal's bytes (RFC 3501 §7.5).
    fn await_continuation(&mut self, command_name: &'static str) -> Result<(), SessionError> {
        loop {
            if let Some(pos) = find_crlf(&self.read_buf) {
                let line: Vec<u8> = self.read_buf[..pos].to_vec();
                self.read_buf.drain(..pos + 2);
                if line.first() == Some(&b'+') {
                    return Ok(());
                }
                return Err(self.poison(format!(
                    "{command_name}: expected a '+' continuation request, got {:?}",
                    String::from_utf8_lossy(&line)
                )));
            }
            self.read_more()?;
        }
    }

    // ------------------------------------------------------------------
    // Any-state commands
    // ------------------------------------------------------------------

    pub fn noop(&mut self) -> Result<(), SessionError> {
        self.command("NOOP", CommandBody::Noop).map(|_| ())
    }

    pub fn login(&mut self, username: &str, password: &str) -> Result<(), SessionError> {
        let body = CommandBody::login(username, password)
            .map_err(|e| SessionError::Protocol(format!("invalid LOGIN credentials: {e:?}")))?;
        self.command("LOGIN", body).map(|_| ())
    }

    /// SASL PLAIN (RFC 4616) via AUTHENTICATE — a single round trip, no
    /// initial-response optimisation. `authcid`/`password` are never logged or
    /// stored. Extension point: XOAUTH2 adds a sibling next to this.
    pub fn authenticate_plain(
        &mut self,
        authzid: &str,
        authcid: &str,
        password: &str,
    ) -> Result<(), SessionError> {
        self.poison_check()?;
        let tag = self.fresh_tag();
        let command = Command::new(
            tag.as_str(),
            CommandBody::authenticate(AuthMechanism::Plain),
        )
        .map_err(|e| SessionError::Protocol(format!("invalid AUTHENTICATE command: {e:?}")))?;
        self.send_fragments("AUTHENTICATE PLAIN", &command)?;
        self.await_continuation("AUTHENTICATE PLAIN")?;

        let sasl_plain = format!("{authzid}\0{authcid}\0{password}");
        let response = base64_engine.encode(sasl_plain.as_bytes());
        self.stream.write_all(response.as_bytes()).map_err(io_err)?;
        self.stream.write_all(b"\r\n").map_err(io_err)?;
        self.stream.flush().map_err(io_err)?;

        self.read_until_tagged("AUTHENTICATE PLAIN").map(|_| ())
    }

    /// The server's CAPABILITY tokens, canonical spelling. Callers that persist
    /// these must still run them through the `sanitize_capability_tokens`
    /// allowlist — this returns raw (if canonically spelled) server text.
    pub fn capability(&mut self) -> Result<Vec<String>, SessionError> {
        let outcome = self.command("CAPABILITY", CommandBody::Capability)?;
        let mut out = Vec::new();
        for d in outcome.data {
            if let Data::Capability(caps) = d {
                out.extend(caps.as_ref().iter().map(|c| c.to_string()));
            }
        }
        Ok(out)
    }

    /// RFC 5161 ENABLE. Extension point for the QRESYNC+IDLE plan.
    pub fn enable(&mut self, capabilities: &[&str]) -> Result<Vec<String>, SessionError> {
        let mut wire_caps = Vec::new();
        for c in capabilities {
            let atom = Atom::try_from(*c).map_err(|e| {
                SessionError::Protocol(format!("invalid ENABLE capability {c:?}: {e:?}"))
            })?;
            wire_caps.push(CapabilityEnable::from(atom));
        }
        let capabilities = Vec1::try_from(wire_caps).map_err(|_| {
            SessionError::Protocol("ENABLE requires at least one capability".into())
        })?;
        let outcome = self.command("ENABLE", CommandBody::Enable { capabilities })?;
        let mut out = Vec::new();
        for d in outcome.data {
            if let Data::Enabled { capabilities } = d {
                out.extend(capabilities.iter().map(|c| c.to_string()));
            }
        }
        Ok(out)
    }

    pub fn logout(&mut self) -> Result<(), SessionError> {
        self.command("LOGOUT", CommandBody::Logout).map(|_| ())
    }

    // ------------------------------------------------------------------
    // Authenticated-state commands
    // ------------------------------------------------------------------

    pub fn list(
        &mut self,
        reference: &str,
        wildcard: &str,
    ) -> Result<Vec<MailboxListItem>, SessionError> {
        self.list_or_lsub("LIST", reference, wildcard, false)
    }

    pub fn lsub(
        &mut self,
        reference: &str,
        wildcard: &str,
    ) -> Result<Vec<MailboxListItem>, SessionError> {
        self.list_or_lsub("LSUB", reference, wildcard, true)
    }

    fn list_or_lsub(
        &mut self,
        command_name: &'static str,
        reference: &str,
        wildcard: &str,
        is_lsub: bool,
    ) -> Result<Vec<MailboxListItem>, SessionError> {
        let body = if is_lsub {
            CommandBody::lsub(reference, wildcard)
        } else {
            CommandBody::list(reference, wildcard)
        }
        .map_err(|e| SessionError::Protocol(format!("invalid {command_name} arguments: {e:?}")))?;

        let outcome = self.command(command_name, body)?;
        let mut out = Vec::new();
        for d in outcome.data {
            match d {
                Data::List {
                    items,
                    delimiter,
                    mailbox,
                } if !is_lsub => out.push(list_item_from_parts(
                    &items,
                    delimiter.map(|d| d.inner()),
                    &mailbox,
                )),
                Data::Lsub {
                    items,
                    delimiter,
                    mailbox,
                } if is_lsub => out.push(list_item_from_parts(
                    &items,
                    delimiter.map(|d| d.inner()),
                    &mailbox,
                )),
                _ => {}
            }
        }
        Ok(out)
    }

    pub fn create(&mut self, mailbox: &str) -> Result<(), SessionError> {
        let body = CommandBody::create(mailbox).map_err(|e| {
            SessionError::Protocol(format!("invalid mailbox name {mailbox:?}: {e:?}"))
        })?;
        self.command("CREATE", body).map(|_| ())
    }

    /// SUBSCRIBE to a mailbox, so LSUB (and the first-sync default set)
    /// includes a mailbox this client created.
    pub fn subscribe(&mut self, mailbox: &str) -> Result<(), SessionError> {
        let body = CommandBody::subscribe(mailbox).map_err(|e| {
            SessionError::Protocol(format!("invalid mailbox name {mailbox:?}: {e:?}"))
        })?;
        self.command("SUBSCRIBE", body).map(|_| ())
    }

    /// DELETE a mailbox. No `ImapProvider` call site uses this — it exists
    /// only for castellum-core's GreenMail test setup, which resets a mailbox
    /// between runs.
    pub fn delete(&mut self, mailbox: &str) -> Result<(), SessionError> {
        let body = CommandBody::delete(mailbox).map_err(|e| {
            SessionError::Protocol(format!("invalid mailbox name {mailbox:?}: {e:?}"))
        })?;
        self.command("DELETE", body).map(|_| ())
    }

    pub fn select(&mut self, mailbox: &str) -> Result<SelectedMailbox, SessionError> {
        self.select_or_examine("SELECT", mailbox, false)
    }

    pub fn examine(&mut self, mailbox: &str) -> Result<SelectedMailbox, SessionError> {
        self.select_or_examine("EXAMINE", mailbox, true)
    }

    fn select_or_examine(
        &mut self,
        command_name: &'static str,
        mailbox: &str,
        examine: bool,
    ) -> Result<SelectedMailbox, SessionError> {
        let body = if examine {
            CommandBody::examine(mailbox)
        } else {
            CommandBody::select(mailbox)
        }
        .map_err(|e| {
            SessionError::Protocol(format!("invalid mailbox name for {command_name}: {e:?}"))
        })?;

        let outcome = self.command(command_name, body)?;
        self.last_permanent_flags = permanent_flags_from_outcome(&outcome);
        Ok(selected_mailbox_from_outcome(&outcome))
    }

    /// The last SELECT/EXAMINE/`select_qresync`'s `[PERMANENTFLAGS ...]`
    /// code, or `None` when that command reported none (RFC 9051 §7.1: the
    /// client should then assume every flag can be changed permanently).
    pub fn last_permanent_flags(&self) -> Option<&PermanentFlags> {
        self.last_permanent_flags.as_ref()
    }

    /// SELECT with RFC 7162 QRESYNC parameters, resuming from
    /// `uid_validity`/`highest_modseq`. Sends no `known-uids`/`seq-match-data`
    /// (see the S30 plan's Assumption 2) — the server still returns the full
    /// VANISHED/changed-FETCH delta, just without the client-side bandwidth
    /// optimisation that parameter enables.
    pub fn select_qresync(
        &mut self,
        mailbox: &str,
        uid_validity: u32,
        highest_modseq: u64,
    ) -> Result<QresyncSelectResult, SessionError> {
        let uid_validity = NonZeroU32::new(uid_validity).ok_or_else(|| {
            SessionError::Protocol("select_qresync: uid_validity must be nonzero".into())
        })?;
        let mod_sequence_value = NonZeroU64::new(highest_modseq).ok_or_else(|| {
            SessionError::Protocol("select_qresync: highest_modseq must be nonzero".into())
        })?;

        // Reuse CommandBody::select's own mailbox-name conversion (the same
        // TryInto path select_or_examine relies on) rather than re-deriving
        // it, then rebuild the variant with QRESYNC parameters attached.
        let CommandBody::Select {
            mailbox: mailbox_value,
            ..
        } = CommandBody::select(mailbox).map_err(|e| {
            SessionError::Protocol(format!("invalid mailbox name for SELECT..QRESYNC: {e:?}"))
        })?
        else {
            unreachable!("CommandBody::select always returns CommandBody::Select");
        };
        let body = CommandBody::Select {
            mailbox: mailbox_value,
            parameters: vec![SelectParameter::QResync {
                uid_validity,
                mod_sequence_value,
                known_uids: None,
                seq_match_data: None,
            }],
        };

        let outcome = self.command("SELECT", body)?;
        self.last_permanent_flags = permanent_flags_from_outcome(&outcome);
        let mailbox = selected_mailbox_from_outcome(&outcome);

        let mut changed = Vec::new();
        let mut vanished_uid_ranges = Vec::new();
        for d in &outcome.data {
            match d {
                Data::Fetch { items, .. } => {
                    changed.push(fetched_message_from_items(items.as_ref()))
                }
                Data::Vanished { known_uids, .. } => {
                    vanished_uid_ranges.extend(sequence_set_to_uid_ranges(known_uids));
                }
                _ => {}
            }
        }

        Ok(QresyncSelectResult {
            mailbox,
            changed,
            vanished_uid_ranges,
        })
    }

    /// RFC 2342. A server without NAMESPACE answers with a tagged BAD, which
    /// surfaces as `SessionError::Bad`; callers treating it as optional should
    /// map any error to `None`.
    pub fn namespace(&mut self) -> Result<NamespaceResult, SessionError> {
        let outcome = self.command("NAMESPACE", CommandBody::Namespace)?;
        let mut entries = Vec::new();
        for d in outcome.data {
            if let Data::Namespace {
                personal,
                other,
                shared,
            } = d
            {
                entries.clear();
                for (kind, group) in [
                    (NamespaceKind::Personal, &personal),
                    (NamespaceKind::Other, &other),
                    (NamespaceKind::Shared, &shared),
                ] {
                    for ns in group.iter() {
                        entries.push(NamespaceEntry {
                            kind,
                            prefix: String::from_utf8_lossy(ns.prefix.as_ref()).into_owned(),
                            delimiter: ns.delimiter.map(|c| c.inner().to_string()),
                        });
                    }
                }
            }
        }
        let personal_delimiter = entries
            .iter()
            .find(|e| e.kind == NamespaceKind::Personal)
            .and_then(|e| e.delimiter.clone());
        Ok(NamespaceResult {
            personal_delimiter,
            entries,
        })
    }

    // ------------------------------------------------------------------
    // Selected-state commands
    // ------------------------------------------------------------------

    pub fn uid_fetch(
        &mut self,
        set: &str,
        items: &[FetchItem],
    ) -> Result<Vec<FetchedMessage>, SessionError> {
        let item_names: Vec<MessageDataItemName> =
            items.iter().copied().map(fetch_item_to_name).collect();
        let body = CommandBody::fetch(
            set,
            MacroOrMessageDataItemNames::MessageDataItemNames(item_names),
            true,
        )
        .map_err(|e| SessionError::Protocol(format!("invalid UID FETCH {set:?}: {e:?}")))?;
        let outcome = self.command("UID FETCH", body)?;
        let mut out = Vec::new();
        for d in outcome.data {
            if let Data::Fetch { items, .. } = d {
                out.push(fetched_message_from_items(items.as_ref()));
            }
        }
        Ok(out)
    }

    /// `UID FETCH <uid> (BODY.PEEK[<section>])` for a numeric MIME part path
    /// (`[2, 1]` = section `2.1`). Returns the part's bytes, or `None` when the
    /// server sent no matching data. An empty path or a zero component is a
    /// typed `Protocol` error — never a panic.
    pub fn uid_fetch_section(
        &mut self,
        uid: u32,
        section: &[u32],
    ) -> Result<Option<Vec<u8>>, SessionError> {
        let part = section_part(section)?;
        let body = CommandBody::fetch(
            uid.to_string().as_str(),
            MacroOrMessageDataItemNames::MessageDataItemNames(vec![MessageDataItemName::BodyExt {
                section: Some(Section::Part(part.clone())),
                partial: None,
                peek: true,
            }]),
            true,
        )
        .map_err(|e| SessionError::Protocol(format!("invalid UID FETCH {uid}: {e:?}")))?;
        let outcome = self.command("UID FETCH", body)?;
        for d in outcome.data {
            if let Data::Fetch { items, .. } = d {
                for item in items.as_ref() {
                    if let MessageDataItem::BodyExt {
                        section: Some(Section::Part(p)),
                        data,
                        ..
                    } = item
                    {
                        if *p == part {
                            return Ok(nstring_bytes(data));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    /// `.SILENT` — callers only ever want the write to happen, never the FETCH
    /// echo STORE would otherwise send back.
    pub fn uid_store(
        &mut self,
        set: &str,
        sign: StoreSign,
        flags: &[SessionFlag],
    ) -> Result<(), SessionError> {
        let kind = match sign {
            StoreSign::Add => StoreType::Add,
            StoreSign::Remove => StoreType::Remove,
            StoreSign::Replace => StoreType::Replace,
        };
        let wire_flags: Vec<Flag> = flags.iter().copied().map(session_flag_to_wire).collect();
        let body = CommandBody::store(set, kind, StoreResponse::Silent, wire_flags, true)
            .map_err(|e| SessionError::Protocol(format!("invalid UID STORE {set:?}: {e:?}")))?;
        self.command("UID STORE", body).map(|_| ())
    }

    /// `UID STORE <set> ±FLAGS.SILENT (<keywords>)`. Each keyword is
    /// validated as an IMAP atom by `Flag::try_from` before anything is
    /// written, so an invalid keyword is a `Protocol` error and never
    /// reaches the wire (CLAUDE.md gotcha 1: never send what imap-codec
    /// cannot encode).
    pub fn uid_store_keywords(
        &mut self,
        set: &str,
        sign: StoreSign,
        keywords: &[&str],
    ) -> Result<(), SessionError> {
        let mut wire: Vec<Flag<'static>> = Vec::with_capacity(keywords.len());
        for k in keywords {
            match Flag::try_from(*k) {
                Ok(Flag::Keyword(atom)) => wire.push(Flag::Keyword(atom.into_static())),
                _ => return Err(SessionError::Protocol(format!("not a keyword atom: {k:?}"))),
            }
        }
        let kind = match sign {
            StoreSign::Add => StoreType::Add,
            StoreSign::Remove => StoreType::Remove,
            StoreSign::Replace => StoreType::Replace,
        };
        let body = CommandBody::store(set, kind, StoreResponse::Silent, wire, true)
            .map_err(|e| SessionError::Protocol(format!("invalid UID STORE {set:?}: {e:?}")))?;
        self.command("UID STORE", body).map(|_| ())
    }

    pub fn uid_search_by_uid(&mut self, uid: u32) -> Result<Vec<u32>, SessionError> {
        let seq = SequenceSet::try_from(uid.to_string().as_str())
            .map_err(|e| SessionError::Protocol(format!("invalid UID {uid}: {e:?}")))?;
        self.uid_search(SearchKey::Uid(seq))
    }

    /// `UID SEARCH UID <low>:<high>` — the UIDs the server currently holds in
    /// that span (, Opus review: a sparse mailbox, e.g. a
    /// heavily-expunged Spam folder with 300 messages spread across a
    /// UIDNEXT in the millions, must chunk over the messages that actually
    /// exist, not blindly walk the whole UID number space in
    /// `DESCRIPTOR_CHUNK_SIZE` windows most of which would come back empty).
    ///
    /// An explicit upper bound, never `low:*`: RFC 3501 §6.4.8 makes `n:*`
    /// always include the mailbox's last UID even when it is below `n`
    /// (review of PR #180, nit 4). Returned as the server sent it: the
    /// caller must still filter to the span, because a server can be wrong
    /// AND because imap-codec's default-on `quirk_always_normalize_sequence_
    /// sets` re-encodes a range ending at `u32::MAX - 1` or `u32::MAX` as
    /// `low:*`, which brings the quirk back for those two bounds.
    pub fn uid_search_uid_range(&mut self, low: u32, high: u32) -> Result<Vec<u32>, SessionError> {
        let seq = SequenceSet::try_from(format!("{low}:{high}").as_str()).map_err(|e| {
            SessionError::Protocol(format!("invalid UID range {low}:{high}: {e:?}"))
        })?;
        self.uid_search(SearchKey::Uid(seq))
    }

    pub fn uid_search_by_message_id(&mut self, message_id: &str) -> Result<Vec<u32>, SessionError> {
        let field = AString::try_from("Message-Id")
            .map_err(|e| SessionError::Protocol(format!("{e:?}")))?;
        let value = AString::try_from(message_id).map_err(|e| {
            SessionError::Protocol(format!("invalid Message-Id {message_id:?}: {e:?}"))
        })?;
        self.uid_search(SearchKey::Header(field, value))
    }

    /// `UID SEARCH SINCE <date>`. imap-types wraps chrono's date in its own
    /// `NaiveDate` that rejects years outside 0..=9999 (unencodable in IMAP),
    /// so the conversion is fallible and surfaces as a protocol error.
    pub fn uid_search_since(&mut self, date: NaiveDate) -> Result<Vec<u32>, SessionError> {
        let date = imap_codec::imap_types::datetime::NaiveDate::try_from(date)
            .map_err(|e| SessionError::Protocol(format!("invalid SINCE date {date}: {e:?}")))?;
        self.uid_search(SearchKey::Since(date))
    }

    /// `UID SEARCH` with every criterion AND-ed (IMAP's implicit top-level
    /// AND). Exists for verification oracles (castellum-core's GreenMail
    /// tests) that need Subject / SEEN / UNSEEN searches; `ImapProvider`
    /// itself only uses the two typed shapes above.
    pub fn uid_search_all(
        &mut self,
        criteria: &[SessionSearch<'_>],
    ) -> Result<Vec<u32>, SessionError> {
        let mut keys = Vec::with_capacity(criteria.len());
        for c in criteria {
            keys.push(match c {
                SessionSearch::Uid(uid) => {
                    let seq = SequenceSet::try_from(uid.to_string().as_str())
                        .map_err(|e| SessionError::Protocol(format!("invalid UID {uid}: {e:?}")))?;
                    SearchKey::Uid(seq)
                }
                SessionSearch::Seen => SearchKey::Seen,
                SessionSearch::Unseen => SearchKey::Unseen,
                SessionSearch::Header { field, value } => {
                    let field = AString::try_from(*field).map_err(|e| {
                        SessionError::Protocol(format!("invalid header {field:?}: {e:?}"))
                    })?;
                    let value = AString::try_from(*value).map_err(|e| {
                        SessionError::Protocol(format!("invalid value {value:?}: {e:?}"))
                    })?;
                    SearchKey::Header(field, value)
                }
            });
        }
        let keys = Vec1::try_from(keys).map_err(|_| {
            SessionError::Protocol("UID SEARCH needs at least one criterion".into())
        })?;
        self.uid_search_keys(keys)
    }

    fn uid_search(&mut self, key: SearchKey<'_>) -> Result<Vec<u32>, SessionError> {
        self.uid_search_keys(Vec1::from(key))
    }

    fn uid_search_keys(&mut self, keys: Vec1<SearchKey<'_>>) -> Result<Vec<u32>, SessionError> {
        let body = CommandBody::search(None, keys, true);
        let outcome = self.command("UID SEARCH", body)?;
        let mut out = Vec::new();
        for d in outcome.data {
            if let Data::Search(uids, ..) = d {
                out.extend(uids.iter().map(|n| n.get()));
            }
        }
        Ok(out)
    }

    /// `UID COPY`. Returns the destination UID when the server reported a
    /// COPYUID for exactly one message (UIDPLUS), else `None`.
    pub fn uid_copy(&mut self, set: &str, mailbox: &str) -> Result<Option<CopyUid>, SessionError> {
        let body = CommandBody::copy(set, mailbox, true).map_err(|e| {
            SessionError::Protocol(format!("invalid UID COPY {set:?} -> {mailbox:?}: {e:?}"))
        })?;
        self.command("UID COPY", body).map(|o| copy_uid_of(&o))
    }

    /// `UID MOVE`. Returns the destination UID when the server reported a
    /// COPYUID (RFC 6851: an untagged OK code) for exactly one message.
    pub fn uid_move(&mut self, set: &str, mailbox: &str) -> Result<Option<CopyUid>, SessionError> {
        let mailbox_value = Mailbox::try_from(mailbox).map_err(|e| {
            SessionError::Protocol(format!("invalid destination mailbox {mailbox:?}: {e:?}"))
        })?;
        let sequence_set = SequenceSet::try_from(set)
            .map_err(|e| SessionError::Protocol(format!("invalid UID MOVE set {set:?}: {e:?}")))?;
        let body = CommandBody::Move {
            sequence_set,
            mailbox: mailbox_value,
            uid: true,
        };
        self.command("UID MOVE", body).map(|o| copy_uid_of(&o))
    }

    pub fn uid_expunge(&mut self, set: &str) -> Result<(), SessionError> {
        let sequence_set = SequenceSet::try_from(set).map_err(|e| {
            SessionError::Protocol(format!("invalid UID EXPUNGE set {set:?}: {e:?}"))
        })?;
        self.command("UID EXPUNGE", CommandBody::ExpungeUid { sequence_set })
            .map(|_| ())
    }

    pub fn expunge(&mut self) -> Result<(), SessionError> {
        self.command("EXPUNGE", CommandBody::Expunge).map(|_| ())
    }

    /// APPEND `raw` (a complete RFC822 message) to `mailbox` with `flags` set.
    /// The flag list is not hardcoded, matching `MailProvider::append`.
    pub fn append(
        &mut self,
        mailbox: &str,
        flags: &[SessionFlag],
        raw: &[u8],
    ) -> Result<(), SessionError> {
        let wire_flags: Vec<Flag> = flags.iter().copied().map(session_flag_to_wire).collect();
        let body = CommandBody::append(mailbox, wire_flags, None, raw.to_vec())
            .map_err(|e| SessionError::Protocol(format!("invalid APPEND to {mailbox:?}: {e:?}")))?;
        self.command("APPEND", body).map(|_| ())
    }

    /// Try to classify one untagged item already buffered in `read_buf`,
    /// without reading the socket. `Ok(None)` means "need more bytes" — the
    /// caller (`read_one_untagged`, unconditionally; `idle_wait`, with a
    /// timed read in between) reads more and retries. Factored out of
    /// `read_one_untagged` so `idle_wait` can interleave this with a TIMED
    /// read instead of an unconditional blocking one.
    fn try_decode_one_untagged(&mut self) -> Result<Option<UntaggedItem>, SessionError> {
        loop {
            let Some(response) = self.try_decode_response("IDLE")? else {
                return Ok(None);
            };
            return match response {
                Response::Data(Data::Exists(n)) => Ok(Some(UntaggedItem::MailboxUpdate {
                    exists: Some(n),
                    expunged_seq: None,
                })),
                Response::Data(Data::Expunge(n)) => Ok(Some(UntaggedItem::MailboxUpdate {
                    exists: None,
                    expunged_seq: Some(n.get()),
                })),
                Response::Data(Data::Vanished { known_uids, .. }) => {
                    Ok(Some(UntaggedItem::Vanished {
                        uid_ranges: sequence_set_to_uid_ranges(&known_uids).collect(),
                    }))
                }
                Response::Data(_) => Ok(Some(UntaggedItem::Other)),
                Response::Status(Status::Untagged(_)) => continue,
                // Same rule as `read_until_tagged`: a BYE before IDLE's tagged
                // completion means the server is hanging up — the connection
                // is gone (closed, an I/O error), not a decode failure.
                Response::Status(Status::Bye(_)) => {
                    self.closed = true;
                    Err(SessionError::Io(
                        "IDLE: server sent BYE and closed the connection while idling".into(),
                    ))
                }
                Response::Status(Status::Tagged(_)) => {
                    Err(self
                        .poison("unexpected tagged completion while reading an untagged response"))
                }
                Response::CommandContinuationRequest(_) => continue,
            };
        }
    }

    /// Like `append`, but also reports the server's UIDPLUS APPENDUID
    /// response code when present. `None` means the server didn't report
    /// one — the caller must resolve the UID another way (e.g.
    /// `uid_search_by_message_id`, already on this type).
    pub fn append_get_uid(
        &mut self,
        mailbox: &str,
        flags: &[SessionFlag],
        raw: &[u8],
    ) -> Result<Option<AppendUid>, SessionError> {
        let wire_flags: Vec<Flag> = flags.iter().copied().map(session_flag_to_wire).collect();
        let body = CommandBody::append(mailbox, wire_flags, None, raw.to_vec())
            .map_err(|e| SessionError::Protocol(format!("invalid APPEND to {mailbox:?}: {e:?}")))?;
        let outcome = self.command("APPEND", body)?;
        Ok(outcome.tagged_code.and_then(|code| match code {
            Code::AppendUid { uid_validity, uid } => Some(AppendUid {
                uid_validity: uid_validity.get(),
                uid: uid.get(),
            }),
            _ => None,
        }))
    }

    /// Reads exactly one untagged item with no command in flight — the
    /// primitive IDLE needs. Blocks indefinitely; `idle_wait` (below, bounded
    /// to streams with `ReadTimeout`) is the timed sibling the actual IDLE
    /// loop uses so it can periodically check its shutdown/nudge channel.
    pub fn read_one_untagged(&mut self) -> Result<UntaggedItem, SessionError> {
        self.poison_check()?;
        let result = (|| loop {
            if let Some(item) = self.try_decode_one_untagged()? {
                return Ok(item);
            }
            self.read_more()?;
        })();
        self.close_on_io(result)
    }

    /// IDLE runs outside `command()`, so it applies `command()`'s rule
    /// itself: an I/O error while IDLE is in flight leaves the stream in an
    /// unknown state, so the session is closed for good (`is_closed`).
    fn close_on_io<T>(&mut self, result: Result<T, SessionError>) -> Result<T, SessionError> {
        if matches!(result, Err(SessionError::Io(_))) {
            self.closed = true;
        }
        result
    }

    /// Send IDLE (RFC 2177) and block for the server's `+` continuation
    /// confirming idling has begun. Must be paired with exactly one later
    /// `idle_done` before any other command is sent on this session.
    pub fn idle_start(&mut self) -> Result<(), SessionError> {
        self.poison_check()?;
        if self.idling {
            return Err(SessionError::Protocol(
                "idle_start called while already idling".into(),
            ));
        }
        let tag = self.fresh_tag();
        let command = Command::new(tag.as_str(), CommandBody::Idle)
            .map_err(|e| SessionError::Protocol(format!("invalid IDLE command: {e:?}")))?;
        let sent = self
            .send_fragments("IDLE", &command)
            .and_then(|()| self.await_continuation("IDLE"));
        self.close_on_io(sent)?;
        self.idling = true;
        Ok(())
    }

    /// Send DONE and read IDLE's tagged completion, returning the session to
    /// the ordinary command state.
    pub fn idle_done(&mut self) -> Result<(), SessionError> {
        if !self.idling {
            return Err(SessionError::Protocol(
                "idle_done called without an active IDLE".into(),
            ));
        }
        self.idling = false;
        // A connection already closed or poisoned mid-IDLE fails fast here
        // rather than writing DONE into a dead or desynchronised stream.
        self.poison_check()?;
        let done = (|| {
            self.stream.write_all(b"DONE\r\n").map_err(io_err)?;
            self.stream.flush().map_err(io_err)?;
            self.read_until_tagged("IDLE").map(|_| ())
        })();
        self.close_on_io(done)
    }
}

impl<S: Read + Write + ReadTimeout> Session<S> {
    /// Wait for the next untagged item while idling, or `Ok(None)` if
    /// `timeout` elapses first with nothing on the wire. Must only be called
    /// between `idle_start` and `idle_done`. Bounded strictly by
    /// `std::io::ErrorKind::WouldBlock`/`TimedOut` — std's documented mapping
    /// for a `set_read_timeout` elapsing — so a real transport failure is
    /// never silently swallowed as "just a timeout".
    pub fn idle_wait(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<UntaggedItem>, SessionError> {
        if !self.idling {
            return Err(SessionError::Protocol(
                "idle_wait called outside an active IDLE".into(),
            ));
        }
        self.poison_check()?;
        self.stream
            .set_read_timeout(Some(timeout))
            .map_err(io_err)?;
        let outcome = (|| loop {
            if let Some(item) = self.try_decode_one_untagged()? {
                return Ok(Some(item));
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(SessionError::Io("connection closed while idling".into())),
                Ok(n) => self.read_buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) => return Err(io_err(e)),
            }
        })();
        // Always clear the timeout afterwards — idle_done/any later command
        // must not inherit a short deadline.
        let _ = self.stream.set_read_timeout(self.default_read_timeout);
        self.close_on_io(outcome)
    }
}

// ----------------------------------------------------------------------
// LIST helpers
// ----------------------------------------------------------------------

fn mailbox_to_string(mailbox: &Mailbox<'_>) -> String {
    match mailbox {
        Mailbox::Inbox => "INBOX".to_string(),
        Mailbox::Other(other) => String::from_utf8_lossy(other.as_ref()).into_owned(),
    }
}

fn list_item_from_parts(
    attrs: &[FlagNameAttribute<'_>],
    delimiter: Option<char>,
    mailbox: &Mailbox<'_>,
) -> MailboxListItem {
    let special_use: Vec<String> = attrs.iter().map(|a| a.to_string()).collect();
    let no_select = special_use
        .iter()
        .any(|s| s.eq_ignore_ascii_case("\\Noselect") || s.eq_ignore_ascii_case("\\NonExistent"));
    MailboxListItem {
        path: mailbox_to_string(mailbox),
        delimiter: delimiter.map(|c| c.to_string()),
        special_use,
        no_select,
    }
}

// ----------------------------------------------------------------------
// FETCH helpers (ported from ImapProvider's decode_imap_text /
// decode_message_ids / remote_address / bodystructure_has_attachment)
// ----------------------------------------------------------------------

fn decode_nstring_text(n: &NString<'_>) -> Option<String> {
    let bytes = n.0.as_ref()?.as_ref();
    let s = String::from_utf8_lossy(bytes).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// [`decode_nstring_text`] for the human-readable ENVELOPE fields — the
/// subject and the address display names — which a standards-following
/// server returns as RFC 2047 encoded words (Gmail sends raw UTF-8, which
/// passes through untouched). Decoded exactly once, here: nothing downstream
/// decodes descriptor text again. Not used for the mailbox/host halves of an
/// address or for Message-IDs, which are not free text.
fn decode_envelope_text(n: &NString<'_>) -> Option<String> {
    let raw = decode_nstring_text(n)?;
    let s = decode_encoded_words(&raw).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn nstring_bytes(n: &NString<'_>) -> Option<Vec<u8>> {
    n.0.as_ref().map(|s| s.as_ref().to_vec())
}

/// Extract bracketed `<id>` tokens. Known simplification vs a fully spec-exact
/// parser: a malformed, non-bracketed multi-id field collapses to one entry
/// (the whole trimmed text) instead of being split on whitespace — real
/// servers always bracket Message-IDs, so this only affects hostile input,
/// and never panics on it either way.
fn decode_message_ids(text: Option<&str>) -> Vec<String> {
    let Some(text) = text else {
        return Vec::new();
    };
    let mut ids = Vec::new();
    let mut remainder = text;
    while let Some(start) = remainder.find('<') {
        let after_start = &remainder[start + 1..];
        let Some(end) = after_start.find('>') else {
            break;
        };
        let id = after_start[..end].trim().to_string();
        if !id.is_empty() && !ids.contains(&id) {
            ids.push(id);
        }
        remainder = &after_start[end + 1..];
    }
    if ids.is_empty() && !text.trim().is_empty() {
        ids.push(text.trim().to_string());
    }
    ids
}

fn session_address(a: &Address<'_>) -> Option<SessionAddress> {
    let mailbox = decode_nstring_text(&a.mailbox)?;
    // RFC 3501 §7.4.2: host NIL with a mailbox name starts an RFC 5322 group
    // ("undisclosed-recipients:;"); that is a label, not an address. The
    // NIL-mailbox group terminator already returned None above.
    let host = decode_nstring_text(&a.host)?;
    let address = format!("{mailbox}@{host}");
    Some(SessionAddress {
        name: decode_envelope_text(&a.name),
        address,
    })
}

fn session_addresses(addrs: &[Address<'_>]) -> Vec<SessionAddress> {
    addrs.iter().filter_map(session_address).collect()
}

fn session_envelope(e: &Envelope<'_>) -> SessionEnvelope {
    SessionEnvelope {
        subject: decode_envelope_text(&e.subject),
        from: session_addresses(&e.from),
        to: session_addresses(&e.to),
        cc: session_addresses(&e.cc),
        bcc: session_addresses(&e.bcc),
        message_id: decode_message_ids(decode_nstring_text(&e.message_id).as_deref())
            .into_iter()
            .next(),
        in_reply_to: decode_message_ids(decode_nstring_text(&e.in_reply_to).as_deref()),
        date_epoch: decode_nstring_text(&e.date)
            .and_then(|d| mail_parser::DateTime::parse_rfc822(&d))
            .map(|dt| dt.to_timestamp()),
    }
}

fn istring_eq_ignore_ascii_case(s: &IString<'_>, key: &str) -> bool {
    s.as_ref().eq_ignore_ascii_case(key.as_bytes())
}

fn params_contain_key(params: &[(IString<'_>, IString<'_>)], key: &str) -> bool {
    params
        .iter()
        .any(|(k, _)| istring_eq_ignore_ascii_case(k, key))
}

fn disposition_type_is(d: Option<&Disposition<'_>>, key: &str) -> bool {
    d.and_then(|d| d.disposition.as_ref())
        .is_some_and(|(ty, _)| istring_eq_ignore_ascii_case(ty, key))
}

fn disposition_params_contain(d: Option<&Disposition<'_>>, key: &str) -> bool {
    d.and_then(|d| d.disposition.as_ref())
        .is_some_and(|(_, params)| params_contain_key(params, key))
}

fn bodystructure_has_attachment(bs: &BodyStructure<'_>) -> bool {
    match bs {
        BodyStructure::Single {
            body,
            extension_data,
        } => {
            let disposition = extension_data.as_ref().and_then(|e| e.tail.as_ref());
            single_part_has_attachment(body, disposition)
        }
        BodyStructure::Multi {
            bodies,
            extension_data,
            ..
        } => {
            let disposition = extension_data.as_ref().and_then(|e| e.tail.as_ref());
            disposition_type_is(disposition, "attachment")
                || bodies.as_ref().iter().any(bodystructure_has_attachment)
        }
    }
}

fn single_part_has_attachment(body: &Body<'_>, disposition: Option<&Disposition<'_>>) -> bool {
    let basic: &BasicFields<'_> = &body.basic;
    if disposition_type_is(disposition, "attachment") {
        return true;
    }
    if params_contain_key(&basic.parameter_list, "name")
        || disposition_params_contain(disposition, "filename")
        || disposition_params_contain(disposition, "name")
    {
        return true;
    }
    if disposition_type_is(disposition, "inline") {
        return false;
    }
    if basic.description.0.is_some() {
        return true;
    }
    match &body.specific {
        SpecificFields::Text { .. } => false,
        SpecificFields::Basic { r#type, .. } => !istring_eq_ignore_ascii_case(r#type, "image"),
        SpecificFields::Message { .. } => true,
    }
}

/// Outcome of [`repair_quoted_8bit`].
#[derive(Debug, PartialEq, Eq)]
enum Repair {
    /// The first response held quoted strings with bytes a strict decoder
    /// rejects; here is the buffer with those strings rewritten as literals.
    Rewritten(Vec<u8>),
    /// A quoted string needs rewriting but the response is not fully
    /// buffered yet — read more and retry.
    NeedMore,
    /// Nothing this repair can fix.
    Nothing,
}

/// Rewrite a leading `"* VANISHED"` line (optionally `"* VANISHED (EARLIER)"`)
/// into `"* 1 VANISHED"` — the pinned `imap-codec` 2.0.0-alpha.7's grammar bug
/// (see `decode_response`'s doc comment) requires a sequence number before
/// `VANISHED` that RFC 7162 never sends and that the resulting `Data::Vanished`
/// then discards, so the synthetic `1` changes nothing observable. Only
/// touches the FIRST line in `buf`; anything else is untouched. Pure and
/// panic-free: bounds-checked, and a line that isn't `* VANISHED...` (case-
/// insensitive) returns `Nothing`.
fn repair_vanished_seq_prefix(buf: &[u8]) -> Repair {
    let Some(end) = find_crlf(buf) else {
        // Cap how long we wait for a first CRLF so a hostile/never-ending
        // line can't stall the session forever; the codec's own Incomplete
        // path already governs the ordinary case.
        return if buf.len() < 8192 {
            Repair::NeedMore
        } else {
            Repair::Nothing
        };
    };
    let line = &buf[..end];
    const PREFIX: &[u8] = b"* ";
    if line.len() <= PREFIX.len() || !line[..PREFIX.len()].eq_ignore_ascii_case(PREFIX) {
        return Repair::Nothing;
    }
    let rest = &line[PREFIX.len()..];
    const VANISHED: &[u8] = b"VANISHED";
    if rest.len() < VANISHED.len() || !rest[..VANISHED.len()].eq_ignore_ascii_case(VANISHED) {
        return Repair::Nothing;
    }
    let mut out = Vec::with_capacity(buf.len() + 2);
    out.extend_from_slice(PREFIX);
    out.extend_from_slice(b"1 ");
    out.extend_from_slice(rest);
    out.extend_from_slice(&buf[end..]);
    Repair::Rewritten(out)
}

/// Rewrite quoted strings that contain 8-bit or control bytes (which
/// `imap-codec` rejects, RFC 3501 quoted = 7-bit TEXT-CHAR) into
/// `{len}\r\n<bytes>` literals, for the FIRST response in `buf` only. Literal
/// bodies already in the response are skipped untouched. Pure and panic-free:
/// every index is bounds-checked and a malformed `{n}` marker just ends the
/// scan with `Nothing`. A NUL byte still cannot be carried (literals forbid
/// it), so the caller's second decode fails and the session poisons as before.
fn repair_quoted_8bit(buf: &[u8]) -> Repair {
    let mut out: Vec<u8> = Vec::with_capacity(buf.len() + 16);
    let mut i = 0;
    let mut changed = false;
    let mut pending_bad_quote = false;
    while i < buf.len() {
        match buf[i] {
            b'"' => {
                // Scan the quoted string, unescaping into `raw`.
                let mut raw = Vec::new();
                let mut j = i + 1;
                let mut bad = false;
                let mut closed = false;
                while j < buf.len() {
                    match buf[j] {
                        b'\\' => {
                            let Some(&next) = buf.get(j + 1) else { break };
                            raw.push(next);
                            j += 2;
                        }
                        b'"' => {
                            closed = true;
                            j += 1;
                            break;
                        }
                        b => {
                            if b >= 0x80 || (b < 0x20 && b != b'\t') {
                                bad = true;
                            }
                            raw.push(b);
                            j += 1;
                        }
                    }
                }
                if !closed {
                    return if bad || pending_bad_quote {
                        Repair::NeedMore
                    } else {
                        Repair::Nothing
                    };
                }
                if bad {
                    // A literal may carry CR/LF but never NUL; give up on NUL.
                    if raw.contains(&0) {
                        return Repair::Nothing;
                    }
                    out.extend_from_slice(format!("{{{}}}\r\n", raw.len()).as_bytes());
                    out.extend_from_slice(&raw);
                    changed = true;
                    pending_bad_quote = true;
                } else {
                    out.extend_from_slice(&buf[i..j]);
                }
                i = j;
            }
            b'{' => {
                // Literal marker `{n}` or `{n+}` followed by CRLF: copy it and
                // its body verbatim.
                let mut j = i + 1;
                let mut n: usize = 0;
                let mut digits = 0;
                while let Some(&d) = buf.get(j) {
                    if d.is_ascii_digit() {
                        n = n.saturating_mul(10).saturating_add((d - b'0') as usize);
                        digits += 1;
                        j += 1;
                    } else {
                        break;
                    }
                }
                if buf.get(j) == Some(&b'+') {
                    j += 1;
                }
                if digits == 0 || buf.get(j) != Some(&b'}') || buf.get(j + 1) != Some(&b'\r') {
                    out.push(buf[i]);
                    i += 1;
                    continue;
                }
                let body_start = j + 3; // past `}` `\r` `\n`
                let Some(body_end) = body_start.checked_add(n) else {
                    return Repair::Nothing;
                };
                if buf.len() < body_end {
                    return if pending_bad_quote {
                        Repair::NeedMore
                    } else {
                        Repair::Nothing
                    };
                }
                out.extend_from_slice(&buf[i..body_end]);
                i = body_end;
            }
            b'\r' if buf.get(i + 1) == Some(&b'\n') => {
                // End of the first response: append the untouched rest.
                out.extend_from_slice(&buf[i..]);
                return if changed {
                    Repair::Rewritten(out)
                } else {
                    Repair::Nothing
                };
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // No CRLF yet: the response is still arriving.
    if pending_bad_quote {
        Repair::NeedMore
    } else {
        Repair::Nothing
    }
}

/// Length of the `* <n> FETCH (` prefix when `buf` starts with one (the
/// keyword compared ASCII-case-insensitively), else `None`.
fn fetch_items_start(buf: &[u8]) -> Option<usize> {
    let rest = buf.strip_prefix(b"* ")?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let keyword = rest.get(digits..digits + 8)?;
    keyword
        .eq_ignore_ascii_case(b" FETCH (")
        .then_some(2 + digits + 8)
}

/// Remove every top-level `BODYSTRUCTURE (...)` / `BODY (...)` item from the
/// FIRST response in `buf`, provided that response is an untagged FETCH, and
/// return the buffer with the rest of the stream untouched.
///
/// Why: servers send BODYSTRUCTUREs that break RFC 3501's grammar (Gmail,
/// 2026-09-22: a `message/rfc822` part whose embedded body is a multipart
/// with ZERO sub-parts, `("ALTERNATIVE" ("BOUNDARY" "x") NIL NIL)`). The
/// strict decoder rejects the whole line, which used to fail the whole UID
/// FETCH batch and poison the session. Dropping the item keeps the rest of
/// the message (UID, flags, envelope); the caller sees no BODYSTRUCTURE and
/// reports `has_attachments: None`.
///
/// Why not rewrite the bad structure into a valid one: servers can break the
/// body grammar in many ways, and a patched structure would report invented
/// parts. Dropping the item is shape-agnostic and never fabricates data.
///
/// Framing safety: the scan tracks paren depth and skips quoted strings and
/// `{n}` literal bodies, and only rewrites once the response's closing `)`
/// plus CRLF is buffered. A CR/LF at depth > 0 outside a literal, a missing
/// CRLF after the closing paren, or a malformed literal marker returns
/// `Nothing` (so the session still poisons). The rewrite is only a candidate:
/// if the failure lay anywhere outside the dropped item, the caller's
/// re-decode fails again and the session poisons exactly as before. Pure and
/// panic-free: every index is bounds-checked.
fn repair_drop_bodystructure(buf: &[u8]) -> Repair {
    let Some(mut i) = fetch_items_start(buf) else {
        return Repair::Nothing;
    };
    // Depth 1 = inside the FETCH item list, where item names live.
    let mut depth: usize = 1;
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    let mut open_cut: Option<usize> = None;
    // Out of buffered bytes: only worth waiting for the rest of the response
    // when there is (or will be) an item to drop. The codec already failed,
    // and it fails at the first bad byte — so a fault inside a BODYSTRUCTURE
    // means we have already seen that item's opening.
    let starved = |open: &Option<usize>, cuts: &Vec<(usize, usize)>| {
        if open.is_some() || !cuts.is_empty() {
            Repair::NeedMore
        } else {
            Repair::Nothing
        }
    };
    let response_end = loop {
        let Some(&b) = buf.get(i) else {
            return starved(&open_cut, &cuts);
        };
        match b {
            b'"' => {
                let mut j = i + 1;
                loop {
                    match buf.get(j) {
                        None => return starved(&open_cut, &cuts),
                        Some(b'\\') => j += 2,
                        Some(b'"') => {
                            j += 1;
                            break;
                        }
                        Some(b'\r') | Some(b'\n') => return Repair::Nothing,
                        Some(_) => j += 1,
                    }
                }
                i = j;
            }
            b'{' => {
                // `{n}` or `{n+}` then CRLF, then n bytes to skip verbatim.
                let mut j = i + 1;
                let mut n: usize = 0;
                let mut digits = 0;
                while let Some(&d) = buf.get(j) {
                    if !d.is_ascii_digit() {
                        break;
                    }
                    n = n.saturating_mul(10).saturating_add((d - b'0') as usize);
                    digits += 1;
                    j += 1;
                }
                if buf.get(j) == Some(&b'+') {
                    j += 1;
                }
                if buf.len() < j + 3 {
                    return starved(&open_cut, &cuts);
                }
                if digits == 0 || &buf[j..j + 3] != b"}\r\n" {
                    return Repair::Nothing;
                }
                let Some(body_end) = (j + 3).checked_add(n) else {
                    return Repair::Nothing;
                };
                if buf.len() < body_end {
                    return starved(&open_cut, &cuts);
                }
                i = body_end;
            }
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
                if depth == 1 {
                    if let Some(start) = open_cut.take() {
                        cuts.push((start, i));
                    }
                }
                if depth == 0 {
                    if buf.len() < i + 2 {
                        return starved(&open_cut, &cuts);
                    }
                    if &buf[i..i + 2] != b"\r\n" {
                        return Repair::Nothing;
                    }
                    break i;
                }
            }
            b'\r' | b'\n' => return Repair::Nothing,
            _ if depth == 1
                && open_cut.is_none()
                && b.is_ascii_alphabetic()
                && matches!(buf.get(i.wrapping_sub(1)), Some(b' ') | Some(b'(')) =>
            {
                let len = buf[i..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == b'.')
                    .count();
                let name = &buf[i..i + len];
                let is_structure = name.eq_ignore_ascii_case(b"BODYSTRUCTURE")
                    || name.eq_ignore_ascii_case(b"BODY");
                if is_structure && buf.get(i + len..i + len + 2) == Some(b" (") {
                    open_cut = Some(i);
                    i += len + 1; // the `(` is counted by the next iteration
                } else {
                    i += len;
                }
            }
            _ => i += 1,
        }
    };
    if cuts.is_empty() {
        return Repair::Nothing;
    }
    let mut out = Vec::with_capacity(buf.len());
    let mut copied = 0;
    for (start, end) in cuts {
        // Take one separating space with the item: the one before it (unless
        // an earlier dropped item already took it), else the one after it —
        // the case when the item opened the list.
        let (start, end) = if start > copied && buf[start - 1] == b' ' {
            (start - 1, end)
        } else if buf.get(end) == Some(&b' ') {
            (start, end + 1)
        } else {
            (start, end)
        };
        out.extend_from_slice(&buf[copied..start]);
        copied = end;
    }
    debug_assert!(copied <= response_end);
    out.extend_from_slice(&buf[copied..]);
    Repair::Rewritten(out)
}

/// Validate a numeric section path into an `imap_types` `Part`.
fn section_part(section: &[u32]) -> Result<Part, SessionError> {
    let nums = section
        .iter()
        .map(|n| NonZeroU32::new(*n))
        .collect::<Option<Vec<_>>>()
        .and_then(|v| Vec1::try_from(v).ok())
        .ok_or_else(|| SessionError::Protocol(format!("invalid section path {section:?}")))?;
    Ok(Part(nums))
}

fn fetch_item_to_name(item: FetchItem) -> MessageDataItemName<'static> {
    match item {
        FetchItem::Uid => MessageDataItemName::Uid,
        FetchItem::Flags => MessageDataItemName::Flags,
        FetchItem::Rfc822Size => MessageDataItemName::Rfc822Size,
        FetchItem::Envelope => MessageDataItemName::Envelope,
        FetchItem::InternalDate => MessageDataItemName::InternalDate,
        FetchItem::BodyStructure => MessageDataItemName::BodyStructure,
        FetchItem::BodyPeek => MessageDataItemName::BodyExt {
            section: None,
            partial: None,
            peek: true,
        },
    }
}

fn session_flag(f: &Flag<'_>) -> Option<SessionFlag> {
    match f {
        Flag::Answered => Some(SessionFlag::Answered),
        Flag::Deleted => Some(SessionFlag::Deleted),
        Flag::Draft => Some(SessionFlag::Draft),
        Flag::Flagged => Some(SessionFlag::Flagged),
        Flag::Seen => Some(SessionFlag::Seen),
        Flag::Extension(_) | Flag::Keyword(_) => None,
    }
}

fn session_flag_to_wire(f: SessionFlag) -> Flag<'static> {
    match f {
        SessionFlag::Answered => Flag::Answered,
        SessionFlag::Deleted => Flag::Deleted,
        SessionFlag::Draft => Flag::Draft,
        SessionFlag::Flagged => Flag::Flagged,
        SessionFlag::Seen => Flag::Seen,
    }
}

fn fetched_message_from_items(items: &[MessageDataItem<'_>]) -> FetchedMessage {
    let mut m = FetchedMessage {
        uid: None,
        flags: Vec::new(),
        keywords: Vec::new(),
        size_bytes: None,
        envelope: None,
        internal_date_epoch: None,
        has_attachments: None,
        body: None,
    };
    for item in items {
        match item {
            MessageDataItem::Uid(n) => m.uid = Some(n.get()),
            MessageDataItem::Flags(flags) => {
                m.flags.clear();
                m.keywords.clear();
                for f in flags.iter() {
                    match f {
                        FlagFetch::Flag(Flag::Keyword(atom)) => {
                            m.keywords.push(atom.as_ref().to_string())
                        }
                        FlagFetch::Flag(flag) => m.flags.extend(session_flag(flag)),
                        FlagFetch::Recent => {}
                    }
                }
            }
            MessageDataItem::Rfc822Size(n) => m.size_bytes = Some(*n),
            MessageDataItem::InternalDate(dt) => {
                m.internal_date_epoch = Some(dt.as_ref().timestamp());
            }
            MessageDataItem::Envelope(envelope) => m.envelope = Some(session_envelope(envelope)),
            MessageDataItem::Body(bs) | MessageDataItem::BodyStructure(bs) => {
                m.has_attachments = Some(bodystructure_has_attachment(bs));
            }
            MessageDataItem::BodyExt { data, .. } => m.body = nstring_bytes(data),
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_rewrites_an_8bit_quoted_string_as_a_literal() {
        let input = b"* 1 FETCH (ENVELOPE (NIL \"a\xFFb\" NIL))\r\nrest";
        let Repair::Rewritten(out) = repair_quoted_8bit(input) else {
            panic!("expected a rewrite");
        };
        assert_eq!(
            out,
            b"* 1 FETCH (ENVELOPE (NIL {3}\r\na\xFFb NIL))\r\nrest".to_vec()
        );
    }

    #[test]
    fn repair_unescapes_before_measuring_and_leaves_clean_quotes_alone() {
        let input = b"* X \"ok\" \"q\\\"\xC3\\\\\"\r\n";
        let Repair::Rewritten(out) = repair_quoted_8bit(input) else {
            panic!("expected a rewrite");
        };
        assert_eq!(out, b"* X \"ok\" {4}\r\nq\"\xC3\\\r\n".to_vec());
        assert_eq!(repair_quoted_8bit(b"* X \"ok\"\r\n"), Repair::Nothing);
    }

    #[test]
    fn repair_skips_literal_bodies_and_waits_for_a_partial_response() {
        // A literal body containing a quote and 8-bit bytes is not a quoted string.
        assert_eq!(
            repair_quoted_8bit(b"* 1 FETCH (BODY[] {4}\r\n\"\xFF\"x)\r\n"),
            Repair::Nothing
        );
        assert_eq!(repair_quoted_8bit(b"* X \"a\xFFb"), Repair::NeedMore);
        assert_eq!(
            repair_quoted_8bit(b"* X \"a\xFFb\" no crlf yet"),
            Repair::NeedMore
        );
        // NUL cannot travel in a literal; malformed markers never panic.
        assert_eq!(
            repair_quoted_8bit(b"* X \"a\x00\xFF\"\r\n"),
            Repair::Nothing
        );
        assert_eq!(
            repair_quoted_8bit(b"{99999999999999999999}\r\n"),
            Repair::Nothing
        );
        assert_eq!(repair_quoted_8bit(b"{"), Repair::Nothing);
    }

    /// The 2026-09-22 Gmail pilot capture (addresses/subject/ids redacted,
    /// grammar preserved): the message/rfc822 part's embedded body is
    /// `("ALTERNATIVE" ...)`, a multipart with zero sub-parts.
    const GMAIL_ZERO_PART_MULTIPART: &[u8] = b"* 75427 FETCH (UID 76873 BODYSTRUCTURE ((\"TEXT\" \"PLAIN\" (\"CHARSET\" \"utf-8\") NIL NIL \"QUOTED-PRINTABLE\" 227 5 NIL NIL NIL)(\"MESSAGE\" \"DELIVERY-STATUS\" NIL NIL NIL \"7BIT\" 346 NIL NIL NIL)(\"MESSAGE\" \"RFC822\" NIL NIL NIL \"7BIT\" 2458 (\"Mon, 01 Jan 2026 00:00:00 +0000\" \"Test subject\" ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"sender\" \"example.com\")) ((NIL NIL \"recipient\" \"example.com\")) ((NIL NIL \"cc1\" \"example.com\")(NIL NIL \"cc2\" \"example.com\")) NIL \"<msgid1@example.com>\" \"<msgid2@example.com>\") (\"ALTERNATIVE\" (\"BOUNDARY\" \"boundary-marker\") NIL NIL) 50 NIL NIL NIL) \"REPORT\" (\"BOUNDARY\" \"outer-boundary-marker\" \"REPORT-TYPE\" \"delivery-status\") NIL NIL))\r\n";

    fn strict_decode_ok(buf: &[u8]) -> bool {
        ResponseCodec::default().decode(buf).is_ok()
    }

    #[test]
    fn the_pilot_bodystructure_is_rejected_only_for_its_zero_part_multipart() {
        assert!(!strict_decode_ok(GMAIL_ZERO_PART_MULTIPART));
        // Give the embedded multipart one part and the same line decodes, so
        // the zero-part multipart is the sole grammar violation.
        let patched = String::from_utf8_lossy(GMAIL_ZERO_PART_MULTIPART).replace(
            "(\"ALTERNATIVE\"",
            "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 0 0) \"ALTERNATIVE\"",
        );
        assert!(strict_decode_ok(patched.as_bytes()));
    }

    #[test]
    fn drop_bodystructure_keeps_the_rest_of_the_fetch_and_the_stream() {
        let mut input = GMAIL_ZERO_PART_MULTIPART.to_vec();
        input.extend_from_slice(b"* 2 FETCH (UID 9)\r\n");
        let Repair::Rewritten(out) = repair_drop_bodystructure(&input) else {
            panic!("expected a rewrite");
        };
        assert_eq!(
            out,
            b"* 75427 FETCH (UID 76873)\r\n* 2 FETCH (UID 9)\r\n".to_vec()
        );
        assert!(strict_decode_ok(&out));
    }

    #[test]
    fn drop_bodystructure_handles_item_position_case_and_literals() {
        // First item, lowercase keyword, a literal whose body holds `)` and
        // CRLF, and a BODY[] item (not a structure) that must survive.
        let input = b"* 1 fetch (bodystructure (\"A\" {4}\r\n)\r\n) x) BODY (\"B\") UID 3 BODY[] {2}\r\n())\r\n";
        let Repair::Rewritten(out) = repair_drop_bodystructure(input) else {
            panic!("expected a rewrite");
        };
        assert_eq!(out, b"* 1 fetch (UID 3 BODY[] {2}\r\n())\r\n".to_vec());
        assert!(strict_decode_ok(&out));
    }

    /// A stream that hands out one pre-scripted chunk per `read` (so a test
    /// controls exactly where TCP segment boundaries fall) and drops writes.
    struct ChunkedStream(std::collections::VecDeque<Vec<u8>>);

    impl Read for ChunkedStream {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let Some(mut chunk) = self.0.pop_front() else {
                return Ok(0);
            };
            let n = chunk.len().min(out.len());
            out[..n].copy_from_slice(&chunk[..n]);
            if n < chunk.len() {
                self.0.push_front(chunk.split_off(n));
            }
            Ok(n)
        }
    }

    impl Write for ChunkedStream {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn group_start_in_envelope_is_not_a_recipient() {
        let fetch = "* 1 FETCH (UID 5 ENVELOPE (NIL \"Hola\" ((NIL NIL \"a\" \"x.test\")) NIL NIL ((NIL NIL \"undisclosed-recipients\" NIL)(NIL NIL NIL NIL)) NIL NIL NIL NIL))\r\nC1 OK done\r\n";
        let chunks = vec![b"* OK ready\r\n".to_vec(), fetch.as_bytes().to_vec()];
        let mut session = Session::connect(ChunkedStream(chunks.into())).expect("greeting");
        let fetched = session
            .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::Envelope])
            .expect("fetch");
        let env = fetched[0].envelope.as_ref().expect("envelope");
        assert_eq!(env.from.len(), 1);
        assert!(
            env.to.is_empty(),
            "group label is not an address: {:?}",
            env.to
        );
    }

    #[test]
    fn a_split_line_needing_both_repairs_still_yields_the_batch() {
        // One FETCH line with an 8-bit quoted subject (repair 1) AND the
        // pilot's zero-part multipart (repair 2), delivered in two reads split
        // inside the BODYSTRUCTURE, followed by a clean neighbour.
        let bad = String::from_utf8_lossy(GMAIL_ZERO_PART_MULTIPART)
            .replace(
                "(UID 76873 ",
                "(UID 76873 ENVELOPE (NIL \"Caf\u{e9}\" NIL NIL NIL NIL NIL NIL NIL NIL) ",
            )
            .into_bytes();
        let split = bad.len() / 2;
        let chunks = vec![
            b"* OK ready\r\n".to_vec(),
            bad[..split].to_vec(),
            [&bad[split..], &b"* 2 FETCH (UID 9 BODYSTRUCTURE (\"text\" \"plain\" NIL NIL NIL \"7bit\" 1 1))\r\nC1 OK done\r\n"[..]].concat(),
        ];
        let mut session = Session::connect(ChunkedStream(chunks.into())).expect("greeting");
        let fetched = session
            .uid_fetch("1:*", &[FetchItem::Uid, FetchItem::BodyStructure])
            .expect("both repairs apply; the batch survives");
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].uid, Some(76873));
        assert_eq!(fetched[0].has_attachments, None);
        assert_eq!(
            fetched[0]
                .envelope
                .as_ref()
                .and_then(|e| e.subject.clone())
                .as_deref(),
            Some("Caf\u{e9}"),
            "the lossless 8-bit repair still ran"
        );
        assert_eq!(fetched[1].uid, Some(9));
        assert_eq!(fetched[1].has_attachments, Some(false));
        assert!(!session.poisoned);
    }

    #[test]
    fn drop_bodystructure_refuses_what_it_cannot_frame() {
        // Not a FETCH, or a FETCH without a structure item.
        assert_eq!(
            repair_drop_bodystructure(b"* LIST () \"/\" INBOX\r\n"),
            Repair::Nothing
        );
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (UID 7 INTERNALDATE NIL)\r\n"),
            Repair::Nothing
        );
        // A bare CRLF inside the structure means the framing is broken.
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (UID 7 BODYSTRUCTURE (\"A\"\r\n\"B\"))\r\n"),
            Repair::Nothing
        );
        // Garbage instead of CRLF after the closing paren.
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (UID 7 BODYSTRUCTURE (\"A\"))junk\r\n"),
            Repair::Nothing
        );
        // Truncated inside the structure: wait for the rest of the response.
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (UID 7 BODYSTRUCTURE ((\"A\""),
            Repair::NeedMore
        );
        // Truncated before any structure item: nothing to wait for.
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (UID 7 FL"),
            Repair::Nothing
        );
        // Malformed literal markers never panic.
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (BODY ({99999999999999999999}\r\n))\r\n"),
            Repair::Nothing
        );
        assert_eq!(
            repair_drop_bodystructure(b"* 1 FETCH (BODY ({x}\r\n))\r\n"),
            Repair::Nothing
        );
    }
}
