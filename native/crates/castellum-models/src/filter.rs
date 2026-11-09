// Castellum - declarative filter/rule schema (S40).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// One schema, evaluated identically whether it backs a one-off search, a
// saved smart mailbox, or a rule's trigger condition. Transcribed from the
// approved design memo (project-docs/rules-filters-native-2026-09-17.md §3,
// approved as written in .run-project/DECISION-S6.md, 2026-09-23). Lives
// here (not castellum-db) so both castellum-db (compiles it to SQL) and
// castellum-core (owns rule evaluation/actions) can depend on it without a
// cycle.

use serde::{Deserialize, Serialize};

/// A group of conditions, ANDed or ORed together, optionally nesting further
/// groups with their own (possibly different) operator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterGroup {
    pub op: BoolOp,
    #[serde(default)]
    pub conditions: Vec<FilterCondition>,
    /// Nested groups, same op semantics — `groups` and `conditions` are
    /// joined together by `op` at compile time (filter_sql::compile_group).
    #[serde(default)]
    pub groups: Vec<FilterGroup>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoolOp {
    /// "all": every member must match.
    And,
    /// "any": at least one member must match.
    Or,
    /// "none": no member may match (MM-1). Compiles to `NOT (a OR b ...)`.
    /// Additive: stored S40 JSON only ever contains `and`/`or`, so old
    /// filters still parse; an OLDER build fails to parse a row holding
    /// `none` (see DECISIONS.md 2026-09-30).
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterCondition {
    pub field: FilterField,
    pub operator: FilterOperator,
    pub value: FilterValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterField {
    From,
    To,
    Cc, // -> message_addresses.kind + address/name (LIKE)
    Subject,
    Body, // -> message_fts MATCH (subject|body column)
    IsRead,
    IsStarred,
    IsAnswered,
    IsDraft,
    IsDeleted,      // -> message_flags.*
    Date,           // -> messages.sent_at (epoch seconds)
    SizeBytes,      // -> messages.size_bytes
    HasAttachments, // -> messages.has_attachments
    MailboxId,      // -> messages.mailbox_id
    /// -> messages.account_id (MM-1). Equals / In.
    AccountId,
    /// -> message_addresses kind `bcc` (MM-1).
    Bcc,
    /// -> message_attachments.filename (MM-1). Only known for messages whose
    /// attachment descriptors were fetched.
    AttachmentName,
    /// -> message_attachments.content_type (MM-1). Same caveat.
    AttachmentType,
    /// -> message_keywords (MM-1b). `Equals` + text = the message carries that
    /// tag (keyword key or display name, any case); `Is` + bool = has any tag
    /// (`true`) or none (`false`). "Is not" is a `none` group around it.
    Tag,
    /// -> new message_gist_topics.topic (memo §7 — conditional on S0
    /// follow-through). Reserved in the schema/compiler so the enum stays
    /// closed and total; evaluating this field returns a `DbError`, never a
    /// panic and never silent no-match — see `filter_sql::compile_condition`.
    GistTopic,
    /// -> message_headers (MM-1c, #2710). The payload is the header name as the
    /// user typed it (kept for display); the compiler lower-cases it through
    /// [`normalize_header_name`]. Serialised as `{"header":"List-Id"}`, so the
    /// unit variants above still round-trip as bare strings. `Contains` /
    /// `Equals` / `StartsWith` + text, or `Is` + bool (present / absent). Only
    /// known for messages whose headers were captured (hydrated bodies).
    Header(String),
}

/// Longest accepted header name (RFC 5322 puts no limit; this bounds input).
pub const HEADER_NAME_MAX_LEN: usize = 64;
/// A stored header value is at most this many bytes: a longer one is cut on a
/// char boundary and ends with [`HEADER_VALUE_CUT_MARKER`].
pub const HEADER_VALUE_MAX_BYTES: usize = 1024;
/// At most this many occurrences of one header name are stored per message.
pub const HEADER_MAX_PER_NAME: usize = 8;
/// At most this many header rows are stored per message.
pub const HEADER_MAX_PER_MESSAGE: usize = 64;

/// Header names that have their own filter field. The editor points there.
pub const HEADER_BUILTIN_FIELDS: &[(&str, FilterFieldBuiltin)] = &[
    ("from", FilterFieldBuiltin::From),
    ("to", FilterFieldBuiltin::To),
    ("cc", FilterFieldBuiltin::Cc),
    ("bcc", FilterFieldBuiltin::Bcc),
    ("subject", FilterFieldBuiltin::Subject),
    ("date", FilterFieldBuiltin::Date),
];

/// Copyable tag for the built-in fields a header name can alias (the full
/// [`FilterField`] is no longer `Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterFieldBuiltin {
    From,
    To,
    Cc,
    Bcc,
    Subject,
    Date,
}

impl FilterFieldBuiltin {
    /// The matching built-in [`FilterField`].
    pub fn field(self) -> FilterField {
        match self {
            Self::From => FilterField::From,
            Self::To => FilterField::To,
            Self::Cc => FilterField::Cc,
            Self::Bcc => FilterField::Bcc,
            Self::Subject => FilterField::Subject,
            Self::Date => FilterField::Date,
        }
    }
}

/// Header names never stored (exact, lower-case): routing/signature noise,
/// message identity already held elsewhere, MIME structure, and anything that
/// can carry credentials or tokens. Mail content: this list is the privacy
/// boundary for the header store.
pub const HEADER_NOT_STORED: &[&str] = &[
    "received",
    "x-received",
    "received-spf",
    "dkim-signature",
    "x-google-dkim-signature",
    "domainkey-signature",
    "authentication-results",
    "arc-seal",
    "arc-message-signature",
    "arc-authentication-results",
    "x-gm-message-state",
    "x-google-smtp-source",
    "autocrypt",
    "x-face",
    "face",
    "message-id",
    "in-reply-to",
    "references",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    // Credential-bearing (defence in depth: a mail header should never carry
    // these, but a misbehaving relay or a forwarded HTTP dump might).
    "authorization",
    "proxy-authorization",
    "www-authenticate",
    "proxy-authenticate",
    "cookie",
    "set-cookie",
];

/// Header-name prefixes never stored (lower-case).
pub const HEADER_NOT_STORED_PREFIXES: &[&str] = &[
    "x-ms-exchange-",
    "x-microsoft-antispam",
    "x-forefront-",
    // Credential-bearing families.
    "x-auth",
    "x-api-key",
    "x-access-token",
    "x-csrf",
    "x-xsrf",
];

/// Substrings that mark a header name as credential-bearing wherever they sit
/// (`X-Amz-Security-Token`, `X-Vault-Token`, `X-Shopify-Hmac-Sha256` via
/// `hmac`, `X-Session-Id`, `X-Jwt-Assertion`, `X-Goog-Authenticated-User`
/// via `auth`, ...). Deliberately broad: `auth` also drops harmless names
/// such as `X-Author`; a false "not stored" costs a condition, a false
/// "storable" can put a credential on disk.
pub const HEADER_NOT_STORED_SUBSTRINGS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "api-key",
    "credential",
    "signature",
    "hmac",
    "cookie",
    "session",
    "bearer",
    "jwt",
    "auth",
];

/// Header VALUES that look like an HTTP credential (`Bearer <token>`,
/// `Basic <base64>`), compared ASCII-case-insensitively against the start of
/// the cleaned value. Such a value is never stored, whatever the header name,
/// and the capture is then marked partial (something was dropped).
pub const HEADER_CREDENTIAL_VALUE_PREFIXES: &[&str] = &["bearer ", "basic "];

/// Whether a cleaned header value starts like an HTTP credential (see
/// [`HEADER_CREDENTIAL_VALUE_PREFIXES`]).
pub fn is_credential_like_value(value: &str) -> bool {
    HEADER_CREDENTIAL_VALUE_PREFIXES.iter().any(|p| {
        value
            .get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
    })
}

/// Appended to a stored header value that was cut at
/// [`HEADER_VALUE_MAX_BYTES`]. U+0080 is a C1 control character, which the
/// extractor strips from every value, so no uncut value can contain it: an
/// `Equals` on a cut value can therefore never match (the full value is not
/// known), while `Contains` / `StartsWith` still match inside the kept prefix.
/// A cut also marks the capture partial, so a non-match on it is UNKNOWN.
pub const HEADER_VALUE_CUT_MARKER: char = '\u{80}';

/// Lower-cased, trimmed name (a single trailing `:` is tolerated), or `None`
/// when it is empty, longer than [`HEADER_NAME_MAX_LEN`], or not an RFC 5322
/// field name (printable ASCII 33..=126 except `:`).
pub fn normalize_header_name(raw: &str) -> Option<String> {
    let t = raw.trim();
    let t = t.strip_suffix(':').unwrap_or(t).trim_end();
    if t.is_empty() || t.len() > HEADER_NAME_MAX_LEN {
        return None;
    }
    if !t.bytes().all(|b| (33..=126).contains(&b) && b != b':') {
        return None;
    }
    Some(t.to_ascii_lowercase())
}

/// What the store does with a header name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderNameStatus {
    /// Kept, and a condition on it can match.
    Storable,
    /// Has a dedicated field; the editor points there.
    BuiltInField(FilterFieldBuiltin),
    /// Deliberately never kept (denylist).
    NotStored,
    /// Not a valid header name.
    Invalid,
}

/// Classify an already-normalised (lower-case) header name.
fn classify_normalized(n: &str) -> HeaderNameStatus {
    if let Some((_, f)) = HEADER_BUILTIN_FIELDS.iter().find(|(name, _)| *name == n) {
        return HeaderNameStatus::BuiltInField(*f);
    }
    if HEADER_NOT_STORED.contains(&n)
        || HEADER_NOT_STORED_PREFIXES.iter().any(|p| n.starts_with(p))
        || HEADER_NOT_STORED_SUBSTRINGS.iter().any(|s| n.contains(s))
    {
        return HeaderNameStatus::NotStored;
    }
    HeaderNameStatus::Storable
}

/// Classify a raw (user-typed or on-the-wire) header name. The single source
/// of truth: the extractor, the SQL compiler and (over the C ABI) the Swift
/// editor all use it.
pub fn header_name_status(raw: &str) -> HeaderNameStatus {
    match normalize_header_name(raw) {
        None => HeaderNameStatus::Invalid,
        Some(n) => classify_normalized(&n),
    }
}

/// One header occurrence kept for a message (see `castellum_mail::headers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredHeader {
    /// Lower-cased name.
    pub name: String,
    /// 0-based document order among occurrences of the same name.
    pub ordinal: u32,
    /// Unfolded, RFC 2047-decoded, trimmed, at most [`HEADER_VALUE_MAX_BYTES`].
    pub value: String,
}

/// Where a message's header capture came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderCaptureSource {
    /// Parsed from a hydrated body.
    Body,
    /// A header-only fetch (slice 2 backfill).
    HeaderFetch,
}

impl HeaderCaptureSource {
    /// The value stored in `message_header_capture.source`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
            Self::HeaderFetch => "header_fetch",
        }
    }
}

/// Messages whose headers are stored vs all messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderCoverage {
    pub captured: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Contains,
    Equals,
    StartsWith,
    Before,
    After,
    OnOrBefore,
    OnOrAfter, // date fields only
    Gt,
    Lt,
    Gte,
    Lte, // numeric fields only
    Is,  // boolean fields only
    In,  // value is a list (e.g. GistTopic in [...])
    /// Date field, value = N days: sent within the last N days (MM-1).
    /// Relative to "now" at evaluation time, so a smart mailbox stays current.
    WithinLastDays,
    /// Date field, value = N days: sent more than N days ago (MM-1).
    OlderThanDays,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FilterValue {
    Text(String),
    Number(i64),
    Bool(bool),
    List(Vec<String>),
}

fn default_enabled() -> bool {
    true
}

/// A named, saved filter — backs both smart mailboxes and rule triggers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFilter {
    pub id: String,
    /// `None` = applies across all accounts (mirrors
    /// `search_messages_across_accounts`'s `None`-means-all-accounts
    /// convention, memo §2).
    pub account_id: Option<String>,
    pub name: String,
    pub kind: SavedFilterKind,
    pub group: FilterGroup,
    /// "date" | "size" | "subject"; ignored when the group contains an FTS
    /// condition (BM25 ranking wins — filter_sql's caller decides).
    pub sort_field: Option<String>,
    pub sort_desc: bool,
    /// Only present when `kind == Rule`.
    pub trigger: Option<RuleTrigger>,
    /// Empty for a plain smart mailbox.
    #[serde(default)]
    pub actions: Vec<RuleAction>,
    /// MM-3: position among rules (ascending). Rules run in this order.
    #[serde(default)]
    pub sort_order: i64,
    /// MM-3: after this rule matches a message, later rules skip it.
    #[serde(default)]
    pub stop_processing: bool,
    /// MM-3: a disabled rule is skipped on arrival; "Run now" still works.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedFilterKind {
    SmartMailbox,
    Rule,
}

/// When a rule fires. `OnSync` is the memo's original (and only) variant —
/// evaluated after each sync pass, batched over that pass's newly-synced
/// message ids (Assumption 3). `Manual` is a documented, additive deviation
/// from the approved memo text: it is not in the memo's §3 enum, but the
/// "Run now" action (Assumption 4) needs a trigger-kind-agnostic path for a
/// rule that has never been (or need not be) wired to `OnSync` — keeping it
/// as an explicit enum variant documents the two ways a rule can fire rather
/// than smuggling a manual run in as an untyped bypass. See the plan's
/// Deviations log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleTrigger {
    OnSync,
    Manual,
}

/// The action set is EXACTLY what the approved memo specifies (§3, Move /
/// AddFlag(Starred) / MarkRead) — Forward and Delete are deliberately absent
/// because only these three kinds have an existing queued mutator in the
/// `operations` table today (`"move"`, `"flag"`, `"mark_read"`, handled by
/// `castellum-core/src/replay.rs` at lines 424, 412 and 387 respectively).
/// Adding a fourth action kind without an existing mutator is explicitly
/// future work, not part of this schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Move {
        mailbox_id: String,
    },
    AddFlag {
        flag: FlagName,
    },
    MarkRead,
    /// MM-3: `RemoveFlag`, `MarkUnread`, `Archive`, `Trash` reuse the same
    /// queued mutators a user action already uses (`set_flagged(false)`,
    /// `mark_read(false)`, `archive_message`, `delete_message` = move to
    /// Trash). Tagging is S42's `AddTag` below; copy, mute, notify and
    /// run-command are NOT here: see DECISIONS.md 2026-09-30 (MM-3).
    RemoveFlag {
        flag: FlagName,
    },
    MarkUnread,
    Archive,
    Trash,
    /// S42: applies one IMAP keyword via the queued `keyword` op
    /// (`Database::set_message_keyword`) — the tag write path added
    /// alongside the mutators the comment above lists.
    AddTag {
        keyword: String,
    },
    /// #3146: removes one IMAP keyword through the same
    /// `Database::set_message_keyword(.., add = false)` path the tag picker
    /// uses: the local row always goes, and a `keyword` op with
    /// `add: false` is queued only for a server-origin keyword on a writable
    /// account. Like AddTag it needs no write permission on its own.
    RemoveTag {
        keyword: String,
    },
}

impl RuleAction {
    /// The keyword a tag action changes, `None` for every other action.
    pub fn tag_keyword(&self) -> Option<&str> {
        match self {
            RuleAction::AddTag { keyword } | RuleAction::RemoveTag { keyword } => Some(keyword),
            _ => None,
        }
    }

    /// True for the actions that take the message out of its folder (Move,
    /// Archive, Trash). A rule may carry at most one of them: once one has
    /// run, the local row is gone and a second could only fail.
    pub fn removes_message(&self) -> bool {
        matches!(
            self,
            RuleAction::Move { .. } | RuleAction::Archive | RuleAction::Trash
        )
    }
}

/// Which `message_flags` column a rule can set via the queued `flag`
/// operation. Deliberately just one variant: `is_starred` is the only flag
/// with a queued write path today (`Repo::set_flagged`,
/// `castellum-db/src/repo.rs`, which inserts an `operations` row with
/// `kind = 'flag'`; the replay-side handler is `replay.rs`, and its payload
/// is a boolean toggle, not a flag-name selector). `is_read` goes through the
/// separate `mark_read` op instead (`RuleAction::MarkRead` above);
/// `is_answered`/`is_draft`/`is_deleted` have no queued mutator — adding a
/// second variant here is future work, not something this schema can
/// execute yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagName {
    Starred,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_group() -> FilterGroup {
        // AND containing an OR containing two conditions — exercises nesting
        // + both BoolOp variants in one structure.
        FilterGroup {
            op: BoolOp::And,
            conditions: vec![FilterCondition {
                field: FilterField::IsRead,
                operator: FilterOperator::Is,
                value: FilterValue::Bool(false),
            }],
            groups: vec![FilterGroup {
                op: BoolOp::Or,
                conditions: vec![
                    FilterCondition {
                        field: FilterField::Subject,
                        operator: FilterOperator::Contains,
                        value: FilterValue::Text("invoice".into()),
                    },
                    FilterCondition {
                        field: FilterField::From,
                        operator: FilterOperator::Contains,
                        value: FilterValue::Text("billing@".into()),
                    },
                ],
                groups: vec![],
            }],
        }
    }

    fn round_trips<T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug>(v: &T) {
        let json = serde_json::to_string(v).expect("serialize");
        let back: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(v, &back, "lossless round trip for {json}");
    }

    #[test]
    fn saved_filter_with_nested_group_round_trips_losslessly() {
        let filter = SavedFilter {
            id: "f1".into(),
            account_id: None,
            name: "Unread invoices".into(),
            kind: SavedFilterKind::Rule,
            group: sample_group(),
            sort_field: Some("date".into()),
            sort_desc: true,
            trigger: Some(RuleTrigger::OnSync),
            actions: vec![
                RuleAction::AddFlag {
                    flag: FlagName::Starred,
                },
                RuleAction::MarkRead,
                RuleAction::RemoveFlag {
                    flag: FlagName::Starred,
                },
                RuleAction::MarkUnread,
                RuleAction::Archive,
                RuleAction::Trash,
            ],
            sort_order: 3,
            stop_processing: true,
            enabled: false,
            created_at: "2026-09-23T00:00:00Z".into(),
            updated_at: "2026-09-23T00:00:00Z".into(),
        };
        round_trips(&filter);
    }

    #[test]
    fn manual_trigger_round_trips() {
        round_trips(&RuleTrigger::Manual);
    }

    #[test]
    fn move_action_round_trips_with_mailbox_id() {
        round_trips(&RuleAction::Move {
            mailbox_id: "acc:Archive".into(),
        });
    }

    #[test]
    fn every_filter_field_round_trips() {
        let fields = [
            FilterField::From,
            FilterField::To,
            FilterField::Cc,
            FilterField::Subject,
            FilterField::Body,
            FilterField::IsRead,
            FilterField::IsStarred,
            FilterField::IsAnswered,
            FilterField::IsDraft,
            FilterField::IsDeleted,
            FilterField::Date,
            FilterField::SizeBytes,
            FilterField::HasAttachments,
            FilterField::MailboxId,
            FilterField::Tag,
            FilterField::GistTopic,
        ];
        for f in fields {
            round_trips(&f);
        }
    }

    #[test]
    fn every_filter_operator_round_trips() {
        let ops = [
            FilterOperator::Contains,
            FilterOperator::Equals,
            FilterOperator::StartsWith,
            FilterOperator::Before,
            FilterOperator::After,
            FilterOperator::OnOrBefore,
            FilterOperator::OnOrAfter,
            FilterOperator::Gt,
            FilterOperator::Lt,
            FilterOperator::Gte,
            FilterOperator::Lte,
            FilterOperator::Is,
            FilterOperator::In,
        ];
        for op in ops {
            round_trips(&op);
        }
    }

    #[test]
    fn filter_value_variants_round_trip_untagged() {
        round_trips(&FilterValue::Text("x".into()));
        round_trips(&FilterValue::Number(42));
        round_trips(&FilterValue::Bool(true));
        round_trips(&FilterValue::List(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn smart_mailbox_kind_has_no_trigger_or_actions() {
        let filter = SavedFilter {
            id: "sm1".into(),
            account_id: Some("acc1".into()),
            name: "Big attachments".into(),
            kind: SavedFilterKind::SmartMailbox,
            group: FilterGroup {
                op: BoolOp::And,
                conditions: vec![],
                groups: vec![],
            },
            sort_field: None,
            sort_desc: false,
            trigger: None,
            actions: vec![],
            sort_order: 0,
            stop_processing: false,
            enabled: true,
            created_at: "2026-09-23T00:00:00Z".into(),
            updated_at: "2026-09-23T00:00:00Z".into(),
        };
        round_trips(&filter);
    }

    /// S40-era stored JSON (only and/or, no `groups`/new fields) must keep
    /// parsing after MM-1's additive variants.
    #[test]
    fn legacy_stored_filter_json_still_parses() {
        let json = r#"{"op":"or","conditions":[{"field":"subject","operator":"contains","value":"invoice"}]}"#;
        let g: FilterGroup = serde_json::from_str(json).unwrap();
        assert_eq!(g.op, BoolOp::Or);
        assert!(g.groups.is_empty());
    }

    #[test]
    fn mm1_variants_round_trip_as_snake_case() {
        let g = FilterGroup {
            op: BoolOp::None,
            conditions: vec![
                FilterCondition {
                    field: FilterField::AccountId,
                    operator: FilterOperator::In,
                    value: FilterValue::List(vec!["a".into()]),
                },
                FilterCondition {
                    field: FilterField::Date,
                    operator: FilterOperator::WithinLastDays,
                    value: FilterValue::Number(7),
                },
                FilterCondition {
                    field: FilterField::AttachmentType,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("pdf".into()),
                },
                FilterCondition {
                    field: FilterField::Bcc,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("x".into()),
                },
                FilterCondition {
                    field: FilterField::AttachmentName,
                    operator: FilterOperator::Contains,
                    value: FilterValue::Text("x".into()),
                },
                FilterCondition {
                    field: FilterField::Date,
                    operator: FilterOperator::OlderThanDays,
                    value: FilterValue::Number(1),
                },
            ],
            groups: vec![],
        };
        let json = serde_json::to_string(&g).unwrap();
        assert!(
            json.contains("\"none\"")
                && json.contains("within_last_days")
                && json.contains("older_than_days")
        );
        assert!(json.contains("account_id") && json.contains("attachment_type"));
        assert_eq!(serde_json::from_str::<FilterGroup>(&json).unwrap(), g);
    }

    /// A pre-MM-3 stored/serialised rule has none of the new fields: it must
    /// come back enabled, unordered and non-stopping.
    #[test]
    fn saved_filter_without_mm3_fields_defaults_to_enabled_and_non_stopping() {
        let json = r#"{"id":"f","account_id":null,"name":"n","kind":"rule",
            "group":{"op":"and","conditions":[]},"sort_field":null,"sort_desc":true,
            "trigger":"on_sync","actions":["mark_read"],
            "created_at":"","updated_at":""}"#;
        let f: SavedFilter = serde_json::from_str(json).unwrap();
        assert!(f.enabled && !f.stop_processing);
        assert_eq!(f.sort_order, 0);
        assert_eq!(f.actions, vec![RuleAction::MarkRead]);
    }

    #[test]
    fn header_field_serialises_as_object_and_unit_fields_stay_bare() {
        let c = FilterCondition {
            field: FilterField::Header("List-Id".into()),
            operator: FilterOperator::Contains,
            value: FilterValue::Text("dev".into()),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains(r#""field":{"header":"List-Id"}"#), "{json}");
        assert_eq!(serde_json::from_str::<FilterCondition>(&json).unwrap(), c);
        let legacy = r#"{"field":"subject","operator":"contains","value":"x"}"#;
        assert_eq!(
            serde_json::from_str::<FilterCondition>(legacy)
                .unwrap()
                .field,
            FilterField::Subject
        );
    }

    #[test]
    fn header_name_normalisation() {
        assert_eq!(
            normalize_header_name(" List-Id ").as_deref(),
            Some("list-id")
        );
        assert_eq!(
            normalize_header_name("List-Id:").as_deref(),
            Some("list-id")
        );
        assert_eq!(
            normalize_header_name("X-GitHub-Reason").as_deref(),
            Some("x-github-reason")
        );
        assert_eq!(normalize_header_name(""), None);
        assert_eq!(normalize_header_name(":"), None);
        assert_eq!(normalize_header_name("a b"), None);
        assert_eq!(normalize_header_name("a:b"), None);
        assert_eq!(normalize_header_name("Näme"), None);
        assert_eq!(
            normalize_header_name(&"a".repeat(64)).map(|s| s.len()),
            Some(64)
        );
        assert_eq!(normalize_header_name(&"a".repeat(65)), None);
    }

    #[test]
    fn header_name_status_table() {
        use HeaderNameStatus::*;
        assert_eq!(header_name_status("List-Id"), Storable);
        assert_eq!(header_name_status("X-Mailer"), Storable);
        assert_eq!(header_name_status("Reply-To"), Storable);
        assert_eq!(
            header_name_status("FROM"),
            BuiltInField(FilterFieldBuiltin::From)
        );
        assert_eq!(
            header_name_status("Subject"),
            BuiltInField(FilterFieldBuiltin::Subject)
        );
        assert_eq!(header_name_status("Received"), NotStored);
        assert_eq!(header_name_status("DKIM-Signature"), NotStored);
        assert_eq!(
            header_name_status("X-MS-Exchange-Organization-SCL"),
            NotStored
        );
        assert_eq!(header_name_status("X-Forefront-Antispam-Report"), NotStored);
        assert_eq!(header_name_status("Message-ID"), NotStored);
        assert_eq!(header_name_status("bad name"), Invalid);
        assert_eq!(header_name_status(&"x".repeat(65)), Invalid);
    }

    /// Privacy: credential-bearing names are never storable.
    #[test]
    fn credential_like_headers_are_never_storable() {
        for n in [
            "Authorization",
            "Proxy-Authorization",
            "Cookie",
            "Set-Cookie",
            "X-Auth-Token",
            "X-Api-Key",
            "X-Amz-Security-Token",
            "X-Vault-Token",
            "X-Webhook-Secret",
            "X-Hub-Signature-256",
            "X-Shopify-Hmac-Sha256",
            "X-Session-Id",
            "X-Jwt-Assertion",
            "X-Bearer",
            "X-Cookie-Jar",
            "X-Goog-Authenticated-User-Email",
            "X-OAuth-Scopes",
        ] {
            assert_eq!(header_name_status(n), HeaderNameStatus::NotStored, "{n}");
        }
    }

    /// Privacy: values shaped like an HTTP credential are recognised whatever
    /// their case; ordinary values are not.
    #[test]
    fn credential_like_values_are_recognised() {
        for v in ["Bearer abc", "bearer abc", "BASIC dXNlcjpwYXNz", "Basic x"] {
            assert!(is_credential_like_value(v), "{v}");
        }
        for v in ["Bearer", "Basics of mail", "Re: bearer abc", "", "é"] {
            assert!(!is_credential_like_value(v), "{v}");
        }
    }
}
