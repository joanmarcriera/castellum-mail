// Castellum - shared domain models / wire types for the native core.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// I/O-free so it can be shared by the core, the FFI layer, and provider/db
// crates. DTOs derive serde so they cross the JSON FFI boundary directly.

use serde::{Deserialize, Serialize};

pub mod filter;
pub use filter::{
    header_name_status, is_credential_like_value, normalize_header_name, BoolOp, FilterCondition,
    FilterField, FilterFieldBuiltin, FilterGroup, FilterOperator, FilterValue, FlagName,
    HeaderCaptureSource, HeaderCoverage, HeaderNameStatus, RuleAction, RuleTrigger, SavedFilter,
    SavedFilterKind, StoredHeader, HEADER_BUILTIN_FIELDS, HEADER_CREDENTIAL_VALUE_PREFIXES,
    HEADER_MAX_PER_MESSAGE, HEADER_MAX_PER_NAME, HEADER_NAME_MAX_LEN, HEADER_NOT_STORED,
    HEADER_NOT_STORED_PREFIXES, HEADER_NOT_STORED_SUBSTRINGS, HEADER_VALUE_CUT_MARKER,
    HEADER_VALUE_MAX_BYTES,
};

/// The native core version string. Single source of truth for what the FFI
/// reports to Swift. Tied to this crate's Cargo package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Status codes returned across the C ABI. The integer values are part of the
/// stable ABI contract and MUST match `native/crates/castellum-ffi/include/castellum.h`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastellumStatus {
    /// Operation succeeded.
    Ok = 0,
    /// A caller argument was null, empty, or otherwise invalid.
    InvalidArgument = 1,
    /// The supplied handle is unknown (never opened, or already closed).
    InvalidHandle = 2,
    /// A filesystem / I/O error occurred.
    Io = 3,
    /// The resource is already open (reserved for future use).
    AlreadyOpen = 4,
    /// A network / provider failure (connect, TLS, auth, or protocol error).
    Network = 5,
    /// An unexpected internal error (e.g. a caught panic).
    Internal = 99,
}

impl CastellumStatus {
    /// The raw ABI integer for this status.
    pub fn code(self) -> i32 {
        self as i32
    }
}

// ---------------------------------------------------------------------------
// Domain models (shared DTOs). Dependency-free; timestamps are Unix epoch
// seconds (i64) so they cross the DB and FFI without a date library.
// ---------------------------------------------------------------------------

/// A mail account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub display_name: Option<String>,
    /// Provider kind, e.g. "imap" (kept as a string until a real enum is needed).
    pub provider: String,
}

/// A mailbox/folder within an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: String,
    pub account_id: String,
    /// Provider-native mailbox path (e.g. "INBOX", "Archive/2026").
    pub path: String,
    /// Human display name.
    pub name: String,
    /// Semantic role if known ("inbox", "sent", "archive", ...).
    pub role: Option<String>,
    /// Provider hierarchy delimiter for this mailbox, if reported.
    #[serde(default)]
    pub delimiter: Option<String>,
    /// Whether the provider reports this mailbox as subscribed/active.
    #[serde(default)]
    pub subscribed: bool,
    /// Whether the folder can be selected and hold messages.
    #[serde(default = "default_true")]
    pub selectable: bool,
    /// Whether the current discovery policy syncs this mailbox without an
    /// explicit user filter.
    #[serde(default = "default_true")]
    pub sync_by_default: bool,
}

fn default_true() -> bool {
    true
}

/// Incremental-sync cursor state for one mailbox (IMAP UIDVALIDITY/UIDNEXT etc.).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyncCursor {
    pub uid_validity: Option<i64>,
    pub uid_next: Option<i64>,
    /// The HIGHESTMODSEQ reconciled up to (RFC 7162), the next
    /// `SELECT..QRESYNC`'s baseline. `None` = never captured; a value <= 0 =
    /// invalidated by a local-only flag change (see castellum-db's
    /// `invalidate_mailbox_modseq`). Only a positive value is QRESYNC-eligible.
    pub highest_modseq: Option<i64>,
    /// The highest UID committed so far by an in-progress CHUNKED descriptor
    /// import, in THIS row's `uid_validity` space. `None` =
    /// no chunked import in progress: never started, or this mailbox's
    /// descriptor step already finished this pass (`persist_cursor` always
    /// clears it back to `None`). Deliberately separate from `uid_next`,
    /// which only ever holds a value from a fully completed pass — see
    /// migration 0040's comment for why conflating the two would make a
    /// resumed-but-incomplete import look complete.
    pub descriptor_checkpoint_uid: Option<i64>,
}

/// An address participant on a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// "from" | "to" | "cc" | "bcc" | "reply-to".
    pub kind: String,
    pub name: Option<String>,
    pub address: String,
}

/// Metadata for one incoming MIME attachment. Payload bytes and local file
/// paths deliberately live outside this descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentDescriptor {
    /// Stable order within the parsed message attachment list.
    pub ordinal: i64,
    pub filename: Option<String>,
    /// Normalised MIME type such as `application/pdf`, when present.
    pub content_type: Option<String>,
    /// Decoded MIME-part size when the full body has been parsed.
    pub size_bytes: Option<i64>,
    /// Dot-separated IMAP BODYSTRUCTURE section path (e.g. "1", "2.1"),
    /// computed from the parsed MIME tree so an on-demand download (S32) can
    /// `UID FETCH <uid> (BODY.PEEK[<section>])` without re-fetching the
    /// whole message. Empty only for a row persisted before this field
    /// existed (migration 0021 backfills `''`, never NULL) — such a row must
    /// be re-hydrated before it can be downloaded.
    #[serde(default)]
    pub section: String,
    /// The MIME `Content-ID` header, bracket-stripped, when present —
    /// identifies an inline part referenced by `cid:` from an HTML body.
    #[serde(default)]
    pub content_id: Option<String>,
    /// `Content-Disposition: inline` (vs `attachment`). Used to distinguish
    /// an inline image from a regular attachment when forwarding
    /// (S33-T5).
    #[serde(default)]
    pub is_inline: bool,
    /// The part's `Content-Transfer-Encoding`, lower-cased ("base64",
    /// "quoted-printable", "7bit", ...), when declared. IMAP
    /// `BODY[<section>]` returns a part still in this encoding, so the
    /// on-demand download must decode it before caching (S32). `None` =
    /// undeclared, which MIME defines as 7bit (identity).
    #[serde(default)]
    pub transfer_encoding: Option<String>,
}

impl AttachmentDescriptor {
    /// True when this part should raise the paperclip. An inline part with
    /// a `Content-ID` (a `cid:` image in a multipart/related body) is body
    /// furniture, not an attachment. Shared by the parser and the DB upsert
    /// so the two can never disagree.
    pub fn is_real_attachment(&self) -> bool {
        !(self.is_inline && self.content_id.is_some())
    }
}

/// SPF/DKIM/DMARC verification result for one mechanism, reduced from the
/// account's own mail server's `Authentication-Results` header. `None` means
/// "no verification available" — either the header was absent, or its
/// `authserv-id` did not match the account's configured host (see
/// `castellum_mail::extract_trust_signals` for why an unmatched header is
/// never shown as a pass). NEVER treat `None` as a failure in UI copy — it
/// usually just means the server didn't check, not that anything is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustResult {
    Pass,
    Fail,
    #[default]
    None,
}

/// SPF/DKIM/DMARC trust signals for one message, computed once at sync time
/// (S48). See `castellum_mail::trust` for the extraction and its threat model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TrustSignals {
    #[serde(default)]
    pub spf: TrustResult,
    #[serde(default)]
    pub dkim: TrustResult,
    #[serde(default)]
    pub dmarc: TrustResult,
    /// The Authentication-Results `authserv-id` this verdict came from, for
    /// the reading-pane details popover ("checked by imap.fastmail.com").
    /// `None` when no usable header was found at all.
    #[serde(default)]
    pub authserv_id: Option<String>,
}

/// A remembered "always load remote content from this sender" decision
/// (S48), scoped per account since the same From address may warrant
/// different trust on a work vs. personal account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderRemoteContentPolicy {
    pub account_id: String,
    /// Stored and matched lower-cased; see `Database::set_sender_remote_content_allowed`.
    pub from_address: String,
    pub allowed: bool,
    pub updated_at: String,
}

/// A message to insert/upsert (envelope + optional bodies), as produced by sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    pub id: String,
    pub account_id: String,
    pub mailbox_id: String,
    /// Provider UID within the mailbox (IMAP UID).
    pub provider_uid: Option<i64>,
    pub rfc822_message_id: Option<String>,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    /// Unix epoch seconds when sent (from Date header) if known.
    pub sent_at: Option<i64>,
    pub snippet: Option<String>,
    pub size_bytes: Option<i64>,
    pub has_attachments: bool,
    /// `None` means attachment descriptors have not been hydrated. `Some`
    /// means a full-body parse produced the authoritative descriptor list,
    /// including `Some(vec![])` for a body with no attachments.
    #[serde(default)]
    pub attachments: Option<Vec<AttachmentDescriptor>>,
    pub is_read: bool,
    /// Remote \Flagged state (starred). Pulled on sync so stars survive
    /// re-syncs, fresh installs and second devices.
    pub is_starred: bool,
    pub addresses: Vec<Address>,
    pub text_body: Option<String>,
    pub sanitized_html: Option<String>,
    /// Raw HTML part, unsanitized (inert at rest); source for future
    /// re-sanitization and forward-quoting.
    pub html_body: Option<String>,
    /// True when sanitized_html still references remote (http/https) sources.
    pub has_remote_content: bool,
    /// Message-ids this message references (for thread linking).
    pub references: Vec<String>,
}

/// A row for list views (envelope only, no body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHeader {
    pub id: String,
    pub account_id: String,
    pub mailbox_id: String,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    pub sent_at: Option<i64>,
    pub snippet: Option<String>,
    pub has_attachments: bool,
    /// True once message body text/html has been hydrated into local storage.
    #[serde(default)]
    pub has_body: bool,
    pub is_read: bool,
    pub is_starred: bool,
    /// Number of messages in this message's thread (1 if standalone/unthreaded).
    pub thread_count: i64,
    /// Visible IMAP keywords on this message (S42) — hidden system keywords
    /// (`is_hidden_keyword`) are filtered out before this ever reaches JSON.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// True when this message's thread is muted (S42).
    #[serde(default)]
    pub thread_muted: bool,
    /// True when the sender address is on the VIP list (S59, migration 0048).
    #[serde(default)]
    pub from_vip: bool,
    /// DR1: the recipients ("To") as display text, for mailboxes whose list
    /// shows WHO a message went to rather than who sent it (Drafts, Sent).
    /// `None` = not such a mailbox (the list shows the sender); `Some("")` =
    /// such a mailbox but no recipient yet (an unaddressed draft).
    #[serde(default)]
    pub to_display: Option<String>,
}

/// A VIP sender (S59); `address` is the lower-cased, trimmed key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VipSender {
    pub address: String,
    pub display_name: Option<String>,
    pub created_at: i64,
}

/// One undelivered new-mail notice drained from `notification_outbox` (S59).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingNotification {
    pub outbox_id: i64,
    pub account_id: String,
    pub message_id: String,
    pub mailbox_id: String,
    pub thread_id: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    pub subject: Option<String>,
    pub snippet: Option<String>,
    pub sent_at: Option<i64>,
    pub from_vip: bool,
}

/// A snoozed message: the ordinary header plus its due time. Deliberately a
/// separate struct (not a field bolted onto `MessageHeader`) so every other
/// consumer of `MessageHeader`'s JSON shape is unaffected — only
/// `list_snoozed_messages` ever produces this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnoozedMessageHeader {
    #[serde(flatten)]
    pub header: MessageHeader,
    pub snoozed_until_epoch: i64,
}

/// One message that just became due (`Database::unsnooze_due`), identifying
/// both the message and the account whose sidebar counts/lists need a
/// refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnsnoozedMessage {
    pub message_id: String,
    pub account_id: String,
}

/// A full message for the reading pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDetail {
    pub header: MessageHeader,
    pub rfc822_message_id: Option<String>,
    pub addresses: Vec<Address>,
    pub text_body: Option<String>,
    pub sanitized_html: Option<String>,
    /// True when sanitized_html references remote sources (show the
    /// "Load remote images" banner when blocking is on).
    pub has_remote_content: bool,
    /// Incoming attachment metadata, in MIME attachment order. Empty for a
    /// descriptor-only (not-yet-hydrated) message.
    #[serde(default)]
    pub attachments: Vec<AttachmentDescriptor>,
    /// SPF/DKIM/DMARC trust signals (S48). Defaults to all-`None` for a
    /// message synced before this stage (no `message_trust` row yet).
    #[serde(default)]
    pub trust: TrustSignals,
    /// RFC 2369 mailto: unsubscribe target, if the message carries one.
    /// Populated only once the message is hydrated (see migration 0024's
    /// doc comment) — `None` for a descriptor-only row even if the server
    /// copy has the header.
    #[serde(default)]
    pub list_unsubscribe_mailto: Option<String>,
    /// RFC 2369 https: unsubscribe target, if the message carries one.
    #[serde(default)]
    pub list_unsubscribe_url: Option<String>,
    /// True only when `list_unsubscribe_url` is set AND the message opts
    /// into RFC 8058 one-click (`List-Unsubscribe-Post:
    /// List-Unsubscribe=One-Click`).
    #[serde(default)]
    pub list_unsubscribe_one_click: bool,
    /// The thread this message belongs to (`threads.id` — see migration
    /// 0004's doc comment), if thread projection has run. Added on
    /// `MessageDetail` rather than `MessageHeader` (S54) so every OTHER
    /// consumer of `MessageHeader`'s shape — list views, search results,
    /// snoozed-message rows — is unaffected; only the reading pane needs a
    /// thread id, to key the S54 extraction panel. `#[serde(default)]` for
    /// decode resilience against an older core payload.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// The message's calendar invite (S55), if it carries a `text/calendar`
    /// part — parsed once at hydration and persisted, so this is a cheap
    /// read, not a re-parse. `None` for a descriptor-only row, or a
    /// hydrated one with no such part.
    #[serde(default)]
    pub calendar_invite: Option<CalendarInvite>,
    /// The message's own `References` chain (oldest first), so a reply can send
    /// `References: <parent's chain> <parent's Message-ID>` and deep threads keep threading in
    /// clients that thread on References (#3443). Empty when none was recorded.
    #[serde(default)]
    pub references: Vec<String>,
}

/// Per-mailbox message counts for sidebar badges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxCounts {
    pub mailbox_id: String,
    pub total: i64,
    pub unread: i64,
}

/// Where a group smart mailbox looks (MM-1). Each empty list = unrestricted;
/// when both are set a message must satisfy both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartMailboxScope {
    #[serde(default)]
    pub account_ids: Vec<String>,
    #[serde(default)]
    pub mailbox_ids: Vec<String>,
    /// HP-2: show this group mailbox as a tab of the Split Inbox. Splits
    /// live in the same `scope_json` column as the rest of the scope, so
    /// they need no migration; a row written before HP-2 parses as `false`.
    #[serde(default)]
    pub inbox_split: bool,
}

/// HP-2: one tab of the Split Inbox with its exact counts over the chosen
/// Inbox mailboxes. Each message lands in exactly one bucket: the first
/// split (sidebar order) whose conditions it matches, or Other
/// (`id == "other"`, always the last entry) when it matches none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxSplitCount {
    pub id: String,
    pub name: String,
    pub total: i64,
    pub unread: i64,
}

/// A saved S39 search-grammar query pinned in the sidebar (S41). Always run
/// cross-account (see `castellum_db::Database::search_messages_across_accounts`)
/// — the grammar has no account-scoping operator, so there is nothing to
/// scope by even for a query saved while one mailbox was selected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartMailbox {
    pub id: String,
    pub name: String,
    /// S39 text query. Empty for a group mailbox (`group` is `Some`).
    pub query: String,
    /// MM-1: nested all/any/none conditions. `None` = a plain text-query
    /// mailbox (every pre-MM-1 row).
    #[serde(default)]
    pub group: Option<FilterGroup>,
    /// MM-1: optional restriction to chosen accounts / mailboxes. Only
    /// applies to group mailboxes; empty lists mean "everywhere".
    #[serde(default)]
    pub scope: SmartMailboxScope,
    /// Sidebar sort position, 0-based, flat (not per-column like `Project`).
    pub sort_order: i64,
    /// Optional SF Symbol name; `None` falls back to a generic icon in the UI.
    #[serde(default)]
    pub symbol: Option<String>,
    pub cached_total: i64,
    pub cached_unread: i64,
    /// `true` when `cached_total`/`cached_unread` hit the recomputation cap
    /// and are a lower bound, not an exact count.
    pub counts_capped: bool,
    /// Epoch seconds of the last count recompute; 0 = never computed.
    pub counts_updated_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What `Database::reset_account` found blocking an unforced reset (S35,
///): counts only, never message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetAccountBlockers {
    /// `drafts.status = 'outbox'` rows for the account: queued to send but
    /// not yet delivered.
    pub unsent_outbox_drafts: i64,
    /// `operations` rows for the account not yet `succeeded`/`cancelled`
    /// (queued, running, held, failed, failed_permanent).
    pub pending_operations: i64,
}

impl ResetAccountBlockers {
    pub fn is_empty(&self) -> bool {
        self.unsent_outbox_drafts == 0 && self.pending_operations == 0
    }
}

/// A folder `Database::apply_mailbox_selection` would refuse to remove
/// unforced, because a non-terminal operation still references it (the tracker
/// #2617 follow-up review). Unlike a UIDVALIDITY reset, removing a folder
/// from the sync selection leaves server UIDs valid, so silently
/// cancelling would drop live, still-undelivered user intent (e.g. a drag
/// from this folder to one that's kept) — the caller shows this and lets
/// the user choose to wait for the pending sync/send or force it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxSelectionBlocker {
    /// The folder's path, as stored (not yet removed).
    pub path: String,
    /// Non-terminal operations (queued, running, held, failed and
    /// failed_permanent) that touch this folder as a move SOURCE (a move
    /// destination never counts), a mark_all_read/mark_read/flag target, or an
    /// empty_trash/delete_permanent target. `send`/`sent_copy` never count.
    pub pending_operations: i64,
}

/// Per-mailbox body-hydration progress (#3631 G5): of the stored messages that
/// have a provider UID, how many already have a local body row. Surfaced as
/// "hydrated / total" because hydration is budgeted and FTS body coverage lags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxBodyProgress {
    pub mailbox_id: String,
    /// Stored messages with a provider UID (the hydratable population).
    pub total: i64,
    /// Of those, the ones with a `message_bodies` row (an authoritative
    /// empty-body marker counts as hydrated).
    pub hydrated: i64,
}

/// Body-hydration coverage of a search scope (#3633 G13): how many stored
/// messages in the scope have no local body yet, so a body match in them
/// cannot be found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyHydrationScope {
    /// Stored messages with a provider UID in the scope.
    pub total: i64,
    /// Of those, the ones with no `message_bodies` row.
    pub missing: i64,
}

/// Per-mailbox body-hydration backlog: how many stored messages still have no
/// local body row. Drives the bounded background hydration runner; the queue
/// itself is the durable checkpoint (hydrated rows drop out of it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxBodyBacklog {
    pub mailbox_id: String,
    /// Provider-native mailbox path (what `select` needs).
    pub path: String,
    /// Semantic role if known — used to hydrate Inbox-like folders first.
    pub role: Option<String>,
    /// Messages with a provider UID but no `message_bodies` row.
    pub missing: i64,
    /// Most recent `sent_at` among the missing rows (Unix epoch seconds);
    /// orders equally-ranked mailboxes by recent activity.
    pub last_activity: i64,
}

/// A queued/replayable provider mutation (write-path operation queue).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: i64,
    pub account_id: String,
    /// Operation kind, e.g. "mark_read".
    pub kind: String,
    /// Kind-specific arguments as a JSON object string.
    pub payload_json: String,
    /// "queued" | "running" | "succeeded" | "failed" | "failed_permanent" |
    /// "cancelled". `failed` retries with backoff; `failed_permanent` is the
    /// terminal "needs user" state once attempts are exhausted.
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    /// `updated_at` as Unix epoch seconds — drives the replay retry backoff.
    pub updated_at_epoch: i64,
    /// Durable earliest-send time (Unix epoch seconds), undo-send window
    /// (migration 0020). `None` for every op kind except a delayed `send`;
    /// the sender skips a `send` op until this has passed. See
    /// `castellum_db::repo::due_for_attempt` (via `castellum_core::replay`).
    #[serde(default)]
    pub not_before_epoch: Option<i64>,
    /// `true` for a `send` op the user scheduled with Send Later (HP1): its
    /// payload carries `"scheduled": true`. Distinguishes a long wait the user
    /// chose from the short undo-send window, so the crash-safety rules differ
    /// (a scheduled op survives quit and relaunch; see DECISIONS.md).
    /// Extra statuses: `held` with a `last_error` starting "may have been
    /// sent" = interrupted in flight (ANY send, scheduled or not);
    /// `needs_confirmation` = a scheduled send more than the late-send grace
    /// past its time at launch.
    #[serde(default)]
    pub scheduled: bool,
}

/// One message waiting in the Outbox (OB1): a draft whose status is `outbox`,
/// joined to its live `send` operation. This is what the Outbox lists, so a
/// queued, in-flight or failed send can never be invisible.
///
/// It deliberately carries NO server text: `error_kind` is the closed set of
/// kind tokens (`auth`, `tls`, `network`, `protocol`, `unsupported`,
/// `internal`) and the UI turns it into plain words.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxSend {
    pub draft_id: i64,
    pub account_id: String,
    /// The live `send` operation, or `None` for a stranded draft (its
    /// operation was cancelled or lost; only Send Now can move it).
    pub operation_id: Option<i64>,
    pub subject: String,
    /// To then Cc addresses, in order (Bcc is never listed).
    pub recipients: Vec<String>,
    /// The operation's status (`queued`, `running`, `failed`,
    /// `failed_permanent`, `held`, `needs_confirmation`) or `stranded`.
    pub status: String,
    pub attempts: i64,
    /// Why the last attempt failed, as a kind token; `None` if it never failed.
    pub error_kind: Option<String>,
    /// Whether Castellum will try again by itself (launch, network return,
    /// backoff). `false` for a permanent failure and for anything waiting on
    /// the user's decision; Send Now still works on those.
    pub automatic_retry: bool,
    /// When the next automatic attempt becomes due (epoch seconds): the end of
    /// the backoff for a failed send, the chosen time for a scheduled one.
    pub next_attempt_epoch: Option<i64>,
    /// `true` for a Send Later message (see `OperationRecord::scheduled`).
    pub scheduled: bool,
    pub updated_at_epoch: i64,
}

/// A recipient of a message WE sent (or queued), distilled from the sent/outbox
/// drafts: the autocomplete source that works before the server's Sent copy has
/// synced back into `messages` (and when the Sent mailbox is never synced).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentRecipient {
    /// Lower-cased, trimmed address.
    pub email: String,
    /// Most recently used non-empty display name for the address.
    pub name: Option<String>,
    /// Epoch seconds of the most recent send to this address.
    pub last_sent_at: i64,
    /// How many sent/queued drafts addressed it (to, cc or bcc).
    pub sent_count: i64,
}

/// One composer address entry (`{name?, email}`) inside a draft's to/cc/bcc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAddress {
    #[serde(default)]
    pub name: Option<String>,
    pub email: String,
}

/// One outgoing attachment's metadata (no payload bytes; see
/// `Database::draft_attachment_bytes`, fetched separately at send time so
/// listing a draft's attachments stays cheap).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftAttachmentMeta {
    pub id: i64,
    pub ordinal: i64,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    #[serde(default)]
    pub is_inline: bool,
    #[serde(default)]
    pub content_id: Option<String>,
}

/// A compose draft to create or update, as sent by the UI. `id` is `None` to
/// create a new draft, `Some` to update an existing one (only while its status
/// is still `draft`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDraft {
    #[serde(default)]
    pub id: Option<i64>,
    pub account_id: String,
    pub to: Vec<DraftAddress>,
    #[serde(default)]
    pub cc: Vec<DraftAddress>,
    #[serde(default)]
    pub bcc: Vec<DraftAddress>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body_text: String,
    /// Sanitized, already-rendered HTML alternative for `body_text` when
    /// Markdown is on for this message (S46); `None` = plain text only, and
    /// `build_rfc822` takes the pre-S46 single-part path byte for byte.
    #[serde(default)]
    pub body_html: Option<String>,
    /// The identity/alias this message sends from, if the From picker chose
    /// one other than the account's own address (S47). `None` = the
    /// account's own address, today's behaviour, unchanged.
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub from_name: Option<String>,
    /// RFC822 Message-ID this draft replies to, if any.
    #[serde(default)]
    pub in_reply_to: Option<String>,
    /// Full References header value for the reply chain, if any.
    #[serde(default)]
    pub references_header: Option<String>,
}

/// A stored compose draft. `status` walks draft -> outbox (queued to send) ->
/// sent (delivered via SMTP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Draft {
    pub id: i64,
    pub account_id: String,
    pub to: Vec<DraftAddress>,
    pub cc: Vec<DraftAddress>,
    pub bcc: Vec<DraftAddress>,
    pub subject: String,
    pub body_text: String,
    /// See `NewDraft::body_html`.
    pub body_html: Option<String>,
    /// See `NewDraft::from_email`/`from_name`.
    pub from_email: Option<String>,
    pub from_name: Option<String>,
    pub in_reply_to: Option<String>,
    pub references_header: Option<String>,
    /// "draft" | "outbox" | "sent".
    pub status: String,
    /// `updated_at` as Unix epoch seconds (list ordering / UI display).
    pub updated_at_epoch: i64,
    /// The server Drafts-role mailbox path this draft is mirrored to, or
    /// `None` if it has never been saved to the server.
    #[serde(default)]
    pub server_mailbox_path: Option<String>,
    /// The mirrored copy's UID in `server_mailbox_path`, when known. `None`
    /// with `server_mailbox_path` set means a save succeeded but neither
    /// APPENDUID nor the Message-ID search fallback could confirm the UID
    /// yet — the next save will re-resolve it via `server_message_id`.
    #[serde(default)]
    pub server_uid: Option<i64>,
    #[serde(default)]
    pub server_uid_validity: Option<i64>,
    /// RFC822 Message-ID of the currently live server copy.
    #[serde(default)]
    pub server_message_id: Option<String>,
    #[serde(default)]
    pub server_saved_at_epoch: Option<i64>,
}

/// A conversation summary for thread-list views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: String,
    pub account_id: String,
    pub subject: Option<String>,
    pub last_message_at: Option<i64>,
    pub message_count: i64,
    pub unread_count: i64,
}

/// A person aggregated from message addresses (plus user-created entries).
/// Derived fields are recomputed by every refresh; `display_name`, `notes`,
/// `is_hidden` and `is_user_created` are user-owned and preserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    pub id: i64,
    /// Normalized (lowercase, trimmed) email address; unique.
    pub email: String,
    /// Best-pick name from mail headers (most frequent non-empty, recency ties).
    pub derived_name: Option<String>,
    /// User override; never touched by refresh.
    pub display_name: Option<String>,
    pub notes: String,
    pub is_hidden: bool,
    /// True when the email matches one of our own accounts.
    pub is_self: bool,
    pub is_user_created: bool,
    /// Epoch seconds of the oldest/newest message mentioning this address.
    pub first_seen_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    /// Messages FROM this address.
    pub inbound_count: i64,
    /// to/cc/bcc rows on messages sent from one of our accounts.
    pub outbound_count: i64,
}

/// An account identity/alias (S47) — an EXTRA From address a compose
/// window's From picker can offer alongside `accounts.email`. The account's
/// own address is never itself a row here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityRecord {
    pub id: i64,
    pub account_id: String,
    pub email: String,
    pub display_name: Option<String>,
    /// MM-6: Reply-To header added to mail sent from this identity.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// MM-6: an address silently added to the ENVELOPE of every message sent
    /// from this identity (never to the headers).
    #[serde(default)]
    pub bcc: Option<String>,
    /// MM-6: this account's default identity for new mail (at most one).
    #[serde(default)]
    pub is_default: bool,
}

/// Request payload to create an identity.
#[derive(Debug, Deserialize)]
pub struct NewIdentity {
    pub account_id: String,
    pub email: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub bcc: Option<String>,
    #[serde(default)]
    pub is_default: bool,
}

// ---------------------------------------------------------------------------
// Projects (slice 8b).
// ---------------------------------------------------------------------------

/// A project (Kanban card + container for linked emails and contacts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub notes: String,
    /// "active" | "waiting" | "someday" | "done"
    pub status: String,
    pub color: Option<String>,
    pub position: i64,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_activity_at: i64,
}

/// Lightweight card for the Kanban board list (no linked-item bodies).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCard {
    pub id: String,
    pub name: String,
    pub notes: String,
    pub status: String,
    pub color: Option<String>,
    pub position: i64,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_activity_at: i64,
    pub email_count: i64,
    pub contact_count: i64,
}

/// A contact linked to a project, with its origin flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectContactLink {
    pub contact_id: i64,
    pub email: String,
    pub derived_name: Option<String>,
    pub display_name: Option<String>,
    /// "manual" | "auto"
    pub origin: String,
    pub linked_at: i64,
}

/// A project with its linked email headers and contact links (reading pane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDetail {
    pub project: Project,
    pub emails: Vec<MessageHeader>,
    pub contacts: Vec<ProjectContactLink>,
}

/// Lightweight tag returned by `projects_for_message` (reverse lookup).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTag {
    pub project_id: String,
    pub name: String,
    pub color: Option<String>,
}

/// Outcome of a contact-aggregation refresh pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateReport {
    pub contacts_upserted: i64,
    pub edges_rebuilt: i64,
    pub elapsed_ms: u64,
    /// Longest single hold of the profile's DB connection during this
    /// refresh (S72): the worst-case wait any other caller -- a sync's
    /// upserts, the UI's reads -- could have seen because of it.
    #[serde(default)]
    pub max_lock_hold_ms: u64,
}

/// A co-occurrence edge between two contacts (canonical `contact_a < contact_b`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub contact_a: i64,
    pub contact_b: i64,
    /// Recency-decayed co-occurrence weight (same-message + 0.25× thread-only).
    pub weight: f64,
    /// Number of shared messages (same-message co-occurrences only).
    pub message_count: i64,
    pub last_seen_at: Option<i64>,
}

/// One attendee (or organizer-as-attendee) line from a parsed `.ics`
/// `ATTENDEE` property (S55). Every field is best-effort — a hostile or
/// minimal producer may omit any of them except the address itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarInviteAttendee {
    /// The bare email address (the `mailto:` scheme is stripped).
    pub email: String,
    pub name: Option<String>,
    /// `CHAIR` | `REQ-PARTICIPANT` | `OPT-PARTICIPANT` | `NON-PARTICIPANT`, verbatim.
    pub role: Option<String>,
    /// `NEEDS-ACTION` | `ACCEPTED` | `DECLINED` | `TENTATIVE` | `DELEGATED`, verbatim.
    pub part_stat: Option<String>,
}

/// A `DTSTART`/`DTEND` value kept as the ORIGINAL iCalendar wire form rather
/// than resolved to an absolute instant — resolving a `TZID` needs the IANA
/// timezone database, which Foundation already has on the Swift side; Rust
/// staying with the raw form avoids a second (and possibly divergent) copy of
/// that database in the native core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarInviteDateTime {
    /// The `DTSTART`/`DTEND` value exactly as written, e.g. `20260925T140000Z`,
    /// `20260925T140000` or (all-day) `20260925`.
    pub raw: String,
    /// The `TZID` parameter, when the value used `FORM #3` (local time with a
    /// timezone reference) instead of UTC or a floating local time.
    pub tzid: Option<String>,
    /// True when `raw` is UTC (`Z`-suffixed, no `TZID`).
    pub is_utc: bool,
    /// True when this is a `VALUE=DATE` (all-day) value, not a date-time.
    pub is_date_only: bool,
}

/// A meeting invite parsed from one message's `text/calendar` part (S55).
/// Ephemeral fields (everything but `our_response`/`responded_at`) are
/// re-derived from the message body at hydration time, never re-parsed from
/// a re-fetched `.ics` — see `castellum_mail::calendar::parse_calendar_invite`.
/// `our_response`/`responded_at` are Castellum's OWN memory of what the user
/// already did (durable in `calendar_invites`); the calendar event itself
/// lives in macOS Calendar via EventKit, not here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarInvite {
    /// Stable across REQUEST/CANCEL/REPLY for the same meeting (RFC 5545 UID).
    pub uid: String,
    /// Higher on a rescheduled/updated invite; 0 if absent.
    pub sequence: i64,
    /// `REQUEST` | `CANCEL` | `REPLY` | `PUBLISH` | ..., upper-cased.
    pub method: String,
    pub summary: Option<String>,
    pub dtstart: Option<CalendarInviteDateTime>,
    pub dtend: Option<CalendarInviteDateTime>,
    pub organizer_email: Option<String>,
    pub organizer_name: Option<String>,
    pub attendees: Vec<CalendarInviteAttendee>,
    /// `LOCATION` and `DESCRIPTION`, unescaped display text (#3041).
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// `None` until the user acts; then `"accepted"` | `"declined"` | `"tentative"`.
    #[serde(default)]
    pub our_response: Option<String>,
    /// Unix epoch seconds of `our_response`, `None` until then.
    #[serde(default)]
    pub responded_at: Option<i64>,
    /// `RECURRENCE-ID`: which single occurrence of a recurring meeting this invite (or update,
    /// or cancellation) is about. `None` for the series itself. A reply must echo it or it
    /// answers the whole series (#3446).
    #[serde(default)]
    pub recurrence_id: Option<CalendarInviteDateTime>,
    /// Derived on read, never stored: `"updated"` when a later invite (higher SEQUENCE) for the
    /// same meeting is in the same account, `"cancelled"` when that later one is a CANCEL. The
    /// card says so and offers no Add to Calendar / Accept (#3446).
    #[serde(default)]
    pub superseded: Option<String>,
    /// Advisory: a later invite from the same organizer exists but its sender is not
    /// authenticated as that organizer (From mismatch or no DMARC pass), so it may be a forgery.
    /// Same values as `superseded`; the card says "possibly ..., unverified sender" and keeps
    /// the buttons.
    #[serde(default)]
    pub possibly_superseded: Option<String>,
}

/// A local, profile-wide tag definition (S42) — display name, colour and sort
/// order for one IMAP keyword. Keyed by `keyword_key` (the ASCII-lowercased
/// keyword, `castellum_db::tags::normalise_keyword`); `keyword` is the exact
/// spelling written to servers. Colours have no IMAP standard (Assumption 3
/// of the tags-and-mute plan), so they live only here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagDefinition {
    pub keyword: String,
    pub keyword_key: String,
    pub display_name: String,
    /// A named palette colour (same convention as `projects.color`), e.g.
    /// `"grey"`, `"red"`, `"blue"`.
    pub colour: String,
    pub sort_order: i64,
}

/// Result of applying a tag to a message: whether it reached the server, and
/// if not, why — never an error (plan Assumption 5: a tag is an
/// organisational aid first, so an evaluation-mode account or a keyword the
/// mailbox's PERMANENTFLAGS refuses just falls back to a local-only tag).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagApplyOutcome {
    pub synced: bool,
    /// `Some("evaluation_mode")` when `synced` is false (the account has
    /// writes disabled); `None` when it is true. A mailbox that refuses the
    /// keyword (`not_permanent`) is only discovered later, at replay, where
    /// the tag is quietly demoted to local-only — never reported here.
    pub reason: Option<String>,
}

/// A distinct visible IMAP keyword currently on at least one message, with
/// how many messages carry it (S42) — Settings' "In use on your servers"
/// adopt list surfaces keywords a server sent that have no local
/// `TagDefinition` yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordInUse {
    pub keyword: String,
    pub keyword_key: String,
    pub count: i64,
}

/// Message counts for one defined tag (S42) — the sidebar/menu badge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagCount {
    pub keyword_key: String,
    pub total: i64,
    pub unread: i64,
    /// Messages whose tag never reached a server (`origin = 'local'`) — the
    /// sidebar's provenance hint (MM-8 T5).
    #[serde(default)]
    pub local_only: i64,
}
