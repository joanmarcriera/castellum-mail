// Castellum - tag (IMAP keyword) definitions, keyword storage and mute state (S42).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Tag *definitions* (display name, colour, order) are local, profile-wide,
// keyed by the lower-cased keyword (`normalise_keyword`). The keyword itself
// round-trips to IMAP servers as a `Flag::Keyword` atom; every keyword a
// server reports is stored in `message_keywords` regardless of whether a
// local definition exists (plan Assumption 2 — "all keywords are stored,
// only some are shown"). See project-docs/plans/2026-09-25-tags-and-mute.md.

use crate::repo::{row_to_header, write_enabled_on, HEADER_COLUMNS};
use crate::{Database, DbError};
use castellum_models::{KeywordInUse, MessageHeader, TagApplyOutcome, TagCount, TagDefinition};
use rusqlite::{params, params_from_iter, OptionalExtension};
use std::collections::{HashMap, HashSet};

/// Chunk size for `IN (...)` queries — see `ai::EMBEDDINGS_QUERY_CHUNK`; the
/// same SQLite bound-variable ceiling applies here.
const KEYWORD_QUERY_CHUNK: usize = 500;

/// Shared by `tag_counts` and `messages_for_tag` (MM-8 T2): a tagged message
/// counts and lists unless it sits in Trash or Junk, or is snoozed (every list
/// hides snoozed mail, see `mailbox_counts`). `{m}` is the messages alias.
fn tag_visible_sql(m: &str) -> String {
    format!(
        "{m}.mailbox_id NOT IN (SELECT id FROM mailboxes WHERE role IN ('trash','junk')) \
         AND NOT EXISTS (SELECT 1 FROM snoozed_messages sn WHERE sn.message_id = {m}.id)"
    )
}

/// True when message `m` is a lesser copy of one email that carries `key` in
/// another visible mailbox (same account and RFC 5322 Message-ID, as the
/// thread rebuild collapses copies, #3458). Gmail stores All Mail as role
/// `archive`, so the Inbox copy wins; ties go to the lower id. Mail without a
/// Message-ID is never merged.
fn tag_duplicate_sql(m: &str, key: &str) -> String {
    let vis2 = tag_visible_sql("m2");
    format!(
        "({m}.rfc822_message_id IS NOT NULL AND EXISTS (
            SELECT 1 FROM message_keywords k2
            JOIN messages m2 ON m2.id = k2.message_id
            WHERE k2.keyword_key = {key}
              AND m2.account_id = {m}.account_id
              AND m2.rfc822_message_id = {m}.rfc822_message_id
              AND m2.id <> {m}.id
              AND {vis2}
              AND (
                (SELECT role IS 'archive' FROM mailboxes WHERE id = m2.mailbox_id)
                  < (SELECT role IS 'archive' FROM mailboxes WHERE id = {m}.mailbox_id)
                OR ((SELECT role IS 'archive' FROM mailboxes WHERE id = m2.mailbox_id)
                  = (SELECT role IS 'archive' FROM mailboxes WHERE id = {m}.mailbox_id)
                    AND m2.id < {m}.id))))"
    )
}

/// Protocol/system keywords the tag UI never shows (plan Assumption 2).
/// Matched case-insensitively; the raw keyword is still stored and
/// searchable, only hidden from the tag list/picker.
const HIDDEN_KEYWORDS: &[&str] = &[
    "$Forwarded",
    "$MDNSent",
    "$Junk",
    "$NotJunk",
    "Junk",
    "NonJunk",
    "$Phishing",
    "$NotPhishing",
    "$MailFlagBit0",
    "$MailFlagBit1",
    "$MailFlagBit2",
    "$Submitted",
    "$SubmitPending",
    "$Muted",
];

/// ASCII-lowercased keyword — the stable key tag definitions and
/// `message_keywords` rows are matched by (IMAP keywords are
/// case-insensitive on the wire, RFC 9051 §2.3.2).
pub fn normalise_keyword(keyword: &str) -> String {
    keyword.to_ascii_lowercase()
}

/// Whether `keyword` is a protocol/system keyword the tag UI should never
/// show (plan Assumption 2). Case-insensitive.
pub fn is_hidden_keyword(keyword: &str) -> bool {
    HIDDEN_KEYWORDS
        .iter()
        .any(|h| h.eq_ignore_ascii_case(keyword))
}

/// Lower-cased `HIDDEN_KEYWORDS`, i.e. the `message_keywords.keyword_key`
/// values the tag UI never shows. SQL callers bind these so the list lives
/// in one place (same definition as [`is_hidden_keyword`]).
pub(crate) fn hidden_keyword_keys() -> impl Iterator<Item = String> {
    HIDDEN_KEYWORDS.iter().map(|h| normalise_keyword(h))
}

/// Derive an IMAP-atom-safe keyword from a user-typed display name (plan
/// Assumption 4): ASCII letters, digits and `` $ _ - . + `` are kept,
/// everything else (spaces, non-ASCII, atom-specials) becomes `_`; a leading
/// `\` in the ORIGINAL name is dropped (so `"\Seen"` -> `"Seen"`, not
/// `"_Seen"`). If nothing but underscores survives the mapping (e.g. an
/// all-non-ASCII name), falls back to the next free `$CastellumTag<n>`,
/// probed through `taken` (so the caller can check its own table).
pub fn keyword_for_display_name(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut mapped = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '$' | '_' | '-' | '.' | '+') {
            mapped.push(c);
        } else {
            mapped.push('_');
        }
    }
    if name.starts_with('\\') && !mapped.is_empty() {
        mapped.remove(0);
    }
    let has_content = mapped.chars().any(|c| c != '_');
    if has_content {
        return mapped;
    }
    let mut n: u64 = 1;
    loop {
        let candidate = format!("$CastellumTag{n}");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Unix epoch seconds now (fallback to 0 on a pre-1970 clock).
fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn row_to_tag_definition(r: &rusqlite::Row<'_>) -> rusqlite::Result<TagDefinition> {
    Ok(TagDefinition {
        keyword: r.get(0)?,
        keyword_key: r.get(1)?,
        display_name: r.get(2)?,
        colour: r.get(3)?,
        sort_order: r.get(4)?,
    })
}

/// Clears the QRESYNC modseq of the mailbox holding `message_id`, so the
/// next sync does a full flag scan (see `repo::invalidate_mailbox_modseq`).
fn invalidate_message_mailbox_modseq(
    tx: &rusqlite::Transaction<'_>,
    message_id: &str,
) -> Result<(), DbError> {
    let mailbox_id: Option<String> = tx
        .query_row(
            "SELECT mailbox_id FROM messages WHERE id = ?1",
            params![message_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(mailbox_id) = mailbox_id {
        crate::repo::invalidate_mailbox_modseq(tx, &mailbox_id)?;
    }
    Ok(())
}

/// One message's stored keyword rows: key -> (stored spelling, origin).
type KeywordRows = HashMap<String, (String, String)>;

/// Loads `message_keywords` rows matching `where_clause`, grouped by message.
fn load_keyword_rows(
    conn: &rusqlite::Connection,
    where_clause: &str,
    args: impl rusqlite::Params,
) -> Result<HashMap<String, KeywordRows>, DbError> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT message_id, keyword_key, keyword, origin FROM message_keywords WHERE {where_clause}"
    ))?;
    let rows = stmt.query_map(args, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out: HashMap<String, KeywordRows> = HashMap::new();
    for row in rows {
        let (id, key, keyword, origin) = row?;
        out.entry(id).or_default().insert(key, (keyword, origin));
    }
    Ok(out)
}

/// Diffs `keywords` (what the server reports) against `before` (what is
/// stored for `message_id`) and writes only the difference; see
/// [`Database::replace_server_keywords`] for the rules. Returns whether
/// anything changed.
fn apply_server_keywords(
    conn: &rusqlite::Connection,
    message_id: &str,
    before: &KeywordRows,
    keywords: &[String],
) -> Result<bool, DbError> {
    let wanted: HashMap<String, String> = keywords
        .iter()
        .map(|k| (normalise_keyword(k), k.clone()))
        .collect();
    let mut changed = false;
    for (key, (_, origin)) in before {
        if origin == "server" && !wanted.contains_key(key) {
            conn.prepare_cached(
                "DELETE FROM message_keywords WHERE message_id = ?1 AND keyword_key = ?2",
            )?
            .execute(params![message_id, key])?;
            changed = true;
        }
    }
    for (key, keyword) in &wanted {
        match before.get(key) {
            Some((existing_spelling, origin)) if origin == "server" => {
                if existing_spelling != keyword {
                    conn.execute(
                        "UPDATE message_keywords SET keyword = ?3
                         WHERE message_id = ?1 AND keyword_key = ?2",
                        params![message_id, key, keyword],
                    )?;
                    changed = true;
                }
            }
            Some(_local_row) => {
                conn.execute(
                    "UPDATE message_keywords SET origin = 'server', keyword = ?3
                     WHERE message_id = ?1 AND keyword_key = ?2",
                    params![message_id, key, keyword],
                )?;
                changed = true;
            }
            None => {
                conn.execute(
                    "INSERT INTO message_keywords (message_id, keyword_key, keyword, origin)
                     VALUES (?1, ?2, ?3, 'server')",
                    params![message_id, key, keyword],
                )?;
                changed = true;
            }
        }
    }
    Ok(changed)
}

impl Database {
    /// Create (when `def.keyword` is empty) or update a tag definition,
    /// keyed by the derived/normalised `keyword_key`. Rejects a duplicate
    /// `display_name` (the table's UNIQUE index) with a friendly message
    /// instead of the raw SQLite constraint text.
    pub fn upsert_tag_definition(&self, def: &TagDefinition) -> Result<TagDefinition, DbError> {
        let conn = self.lock_conn();
        let is_create = def.keyword.is_empty();
        let keyword = if is_create {
            keyword_for_display_name(&def.display_name, &|candidate| {
                let key = normalise_keyword(candidate);
                conn.query_row(
                    "SELECT 1 FROM tag_definitions WHERE keyword_key = ?1",
                    params![key],
                    |_| Ok(()),
                )
                .optional()
                .unwrap_or(None)
                .is_some()
            })
        } else {
            def.keyword.clone()
        };
        let keyword_key = normalise_keyword(&keyword);
        // FP43: the tag list hides protocol keywords, so a user tag derived
        // to one of them (e.g. "Junk") would silently do nothing. Refuse it
        // on create; do not exempt defined tags from the hidden filter.
        if is_create && is_hidden_keyword(&keyword) {
            return Err(DbError(format!(
                "\"{}\" is reserved by mail servers; choose another name.",
                def.display_name
            )));
        }

        // On create, ANY existing row with this display name OR this derived
        // keyword_key is a clash. The key check matters because keyword
        // derivation is deterministic and the upsert below conflicts on
        // `keyword_key` (case-insensitive, and "To do"/"To_do" both map to
        // `to_do`): "work" vs "Work" would otherwise silently rename and
        // recolour the existing tag instead of being rejected. On update,
        // only a DIFFERENT row sharing the name is a clash — the tag being
        // updated may keep it.
        let clashing: Option<String> = if is_create {
            conn.query_row(
                "SELECT keyword_key FROM tag_definitions
                 WHERE display_name = ?1 OR keyword_key = ?2",
                params![def.display_name, keyword_key],
                |r| r.get(0),
            )
            .optional()?
        } else {
            conn.query_row(
                "SELECT keyword_key FROM tag_definitions
                 WHERE display_name = ?1 AND keyword_key != ?2",
                params![def.display_name, keyword_key],
                |r| r.get(0),
            )
            .optional()?
        };
        if clashing.is_some() {
            return Err(DbError(format!(
                "A tag named \"{}\" already exists",
                def.display_name
            )));
        }

        let now = now_epoch();
        conn.execute(
            "INSERT INTO tag_definitions
                 (keyword_key, keyword, display_name, colour, sort_order, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(keyword_key) DO UPDATE SET
                 keyword = excluded.keyword,
                 display_name = excluded.display_name,
                 colour = excluded.colour,
                 sort_order = excluded.sort_order,
                 updated_at = excluded.updated_at",
            params![
                keyword_key,
                keyword,
                def.display_name,
                def.colour,
                def.sort_order,
                now
            ],
        )?;

        Ok(TagDefinition {
            keyword,
            keyword_key,
            display_name: def.display_name.clone(),
            colour: def.colour.clone(),
            sort_order: def.sort_order,
        })
    }

    pub fn list_tag_definitions(&self) -> Result<Vec<TagDefinition>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT keyword, keyword_key, display_name, colour, sort_order
             FROM tag_definitions ORDER BY sort_order, display_name",
        )?;
        let rows = stmt.query_map([], row_to_tag_definition)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Returns whether a row was deleted.
    pub fn delete_tag_definition(&self, keyword_key: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let n = conn.execute(
            "DELETE FROM tag_definitions WHERE keyword_key = ?1",
            params![keyword_key],
        )?;
        Ok(n > 0)
    }

    /// Every stored keyword (server spelling) for each of `ids`, chunked at
    /// `KEYWORD_QUERY_CHUNK` ids per statement (see
    /// `ai::get_message_embeddings` for why: SQLite's bound-variable
    /// ceiling). A message with no keywords is simply absent from the map.
    pub fn keywords_for_messages(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>, DbError> {
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let conn = self.lock_conn();
        for chunk in ids.chunks(KEYWORD_QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT message_id, keyword FROM message_keywords
                 WHERE message_id IN ({placeholders}) ORDER BY message_id"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (message_id, keyword) = row?;
                out.entry(message_id).or_default().push(keyword);
            }
        }
        Ok(out)
    }

    /// Replaces every `origin='server'` keyword row for `message_id` with
    /// `keywords`, leaving `origin='local'` rows alone — EXCEPT that a local
    /// row is upgraded to `'server'` when the server now reports the same
    /// keyword (so a fallback tag becomes a real one once the mailbox
    /// allows it, without ever creating a duplicate row). Returns whether
    /// anything changed, so callers can skip a no-op FTS/search refresh.
    pub fn replace_server_keywords(
        &self,
        message_id: &str,
        keywords: &[String],
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let before = load_keyword_rows(&conn, "message_id = ?1", params![message_id])?
            .remove(message_id)
            .unwrap_or_default();
        apply_server_keywords(&conn, message_id, &before, keywords)
    }

    /// Batch form of [`replace_server_keywords`] for a whole mailbox pass
    /// (the RFC 4549 full scan): ONE lock, ONE `SELECT` of the mailbox's
    /// keyword rows, an in-memory diff, and every write in ONE transaction
    /// — instead of a lock + uncached SELECT + autocommit per message.
    /// Messages whose stored set already matches cost no write at all.
    /// Returns how many messages changed.
    pub fn replace_server_keywords_batch(
        &self,
        account_id: &str,
        mailbox_id: &str,
        items: &[(&str, &[String])],
    ) -> Result<usize, DbError> {
        if items.is_empty() {
            return Ok(0);
        }
        let mut conn = self.lock_conn();
        let mut before = load_keyword_rows(
            &conn,
            "message_id IN (SELECT id FROM messages WHERE account_id = ?1 AND mailbox_id = ?2)",
            params![account_id, mailbox_id],
        )?;
        let tx = conn.transaction()?;
        let mut changed = 0;
        for (message_id, keywords) in items {
            let rows = before.remove(*message_id).unwrap_or_default();
            if apply_server_keywords(&tx, message_id, &rows, keywords)? {
                changed += 1;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Normalised keys of the `origin='server'` keyword rows of one message —
    /// what the server is (as far as this Mac knows) already reporting.
    /// Local-only tags are excluded: a server report never touches them, so
    /// they must not count as a difference from it.
    pub fn server_keyword_keys(&self, message_id: &str) -> Result<HashSet<String>, DbError> {
        let conn = self.lock_conn();
        let rows = load_keyword_rows(&conn, "message_id = ?1", params![message_id])?
            .remove(message_id)
            .unwrap_or_default();
        Ok(rows
            .into_iter()
            .filter(|(_, (_, origin))| origin == "server")
            .map(|(key, _)| key)
            .collect())
    }

    /// Demotes a keyword to `origin='local'` for one message — used when a
    /// queued write is refused by the mailbox's PERMANENTFLAGS or the
    /// account is in evaluation mode (never queues a STORE either way).
    /// Only ever UPDATEs an existing row, never inserts: when the user
    /// added then removed the tag before replay ran, the row is already gone
    /// and inserting here would resurrect a tag they deleted. Returns whether
    /// a row was demoted.
    pub fn demote_keyword_to_local(
        &self,
        message_id: &str,
        keyword: &str,
    ) -> Result<bool, DbError> {
        let key = normalise_keyword(keyword);
        let conn = self.lock_conn();
        let n = conn.execute(
            "UPDATE message_keywords SET origin = 'local'
             WHERE message_id = ?1 AND keyword_key = ?2",
            params![message_id, key],
        )?;
        Ok(n > 0)
    }

    /// A `keyword` add was refused (NotPermanent) after its message had
    /// already been moved away (`enqueue_move` deleted the row, so
    /// `demote_keyword_to_local` has nothing to demote and the tag would be
    /// lost). Append the tag to the stash of the newest non-cancelled `move`
    /// op whose payload carries the source coordinates the keyword op was
    /// stamped with, so it re-attaches as a Mac-only tag when the message
    /// arrives. Returns false (and does nothing) when no such move exists:
    /// the user removed the tag (including a later queued removal), or the
    /// move was cancelled.
    pub fn stash_keyword_on_pending_move(
        &self,
        account_id: &str,
        op_id: i64,
        message_id: &str,
        src_path: &str,
        uid: i64,
        keyword: &str,
    ) -> Result<bool, DbError> {
        let key = normalise_keyword(keyword);
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        // The user removed the tag again after adding it (a later `add:false`
        // op for the same message and keyword): do not resurrect it.
        let removed_later: bool = tx.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM operations
                 WHERE account_id = ?1 AND kind = 'keyword' AND id > ?2
                   AND status <> 'cancelled'
                   AND json_extract(payload_json, '$.message_id') = ?3
                   AND lower(json_extract(payload_json, '$.keyword')) = lower(?4)
                   AND json_extract(payload_json, '$.add') = 0)",
            params![account_id, op_id, message_id, keyword],
            |r| r.get(0),
        )?;
        if removed_later {
            return Ok(false);
        }
        let found: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, COALESCE(json_extract(payload_json, '$.local_keywords'), '[]')
                 FROM operations
                 WHERE account_id = ?1 AND kind = 'move' AND status <> 'cancelled'
                   AND json_extract(payload_json, '$.src_path') = ?2
                   AND json_extract(payload_json, '$.uid') = ?3
                 ORDER BY id DESC LIMIT 1",
                params![account_id, src_path, uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((op_id, json)) = found else {
            return Ok(false);
        };
        let mut pairs: Vec<(String, String)> = serde_json::from_str(&json).unwrap_or_default();
        if !pairs.iter().any(|(k, _)| *k == key) {
            pairs.push((key, keyword.to_string()));
        }
        let pairs_json = serde_json::to_string(&pairs)
            .map_err(|e| DbError(format!("serialise local_keywords: {e}")))?;
        tx.execute(
            "UPDATE operations
             SET payload_json = json_set(payload_json, '$.local_keywords', json(?2))
             WHERE id = ?1",
            params![op_id, pairs_json],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Add or remove one IMAP keyword on a message (Task 5): one transaction,
    /// the same shape as `Repo::set_flagged`.
    ///
    /// `add == true`: upserts the row. Write-enabled ⇒ `origin = 'server'`
    /// and a `keyword` op is queued (replay attempts the STORE and may demote
    /// it back to local via `demote_keyword_to_local` — see `replay.rs`);
    /// evaluation mode ⇒ `origin = 'local'`, no op, kept on this Mac only.
    ///
    /// `add == false`: deletes the row. An op is queued only when the
    /// deleted row was `origin = 'server'` AND writes are enabled — removing
    /// a local-only tag (or one on a read-only account) never touches the
    /// server, since the server was never told about it in the first place.
    /// True when adding `keyword` to `message_id` would change nothing: the
    /// message already carries it and either the server knows it (`origin =
    /// 'server'`) or the account is read-only (a local tag stays local).
    /// A local tag on a now write-enabled account is NOT "applied": it still
    /// has to be promoted with one queued op. Used by the rules engine so a
    /// re-run does not queue the same STORE again (audit #3452).
    pub fn keyword_already_applied(
        &self,
        message_id: &str,
        keyword: &str,
    ) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let key = normalise_keyword(keyword);
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT k.origin, COALESCE(a.write_enabled, 0)
                 FROM message_keywords k
                 JOIN messages m ON m.id = k.message_id
                 LEFT JOIN accounts a ON a.id = m.account_id
                 WHERE k.message_id = ?1 AND k.keyword_key = ?2",
                params![message_id, key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(matches!(row, Some((origin, write)) if origin == "server" || write == 0))
    }

    /// True when `message_id` carries `keyword` locally, whatever its origin
    /// (#3146: a RemoveTag rule with nothing to remove is a no-op).
    pub fn message_has_keyword(&self, message_id: &str, keyword: &str) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        let key = normalise_keyword(keyword);
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM message_keywords WHERE message_id = ?1 AND keyword_key = ?2",
            params![message_id, key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    pub fn set_message_keyword(
        &self,
        message_id: &str,
        keyword: &str,
        add: bool,
    ) -> Result<TagApplyOutcome, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let account_id: Option<String> = tx
            .query_row(
                "SELECT account_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(account_id) = account_id else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        let key = normalise_keyword(keyword);
        let write_enabled = write_enabled_on(&tx, &account_id)?;

        let outcome = if add {
            let origin = if write_enabled { "server" } else { "local" };
            if !write_enabled {
                // Overwriting a server-reported tag with a Mac-only one hides
                // the server's row; a QRESYNC delta will not re-report it.
                let prior: Option<String> = tx
                    .query_row(
                        "SELECT origin FROM message_keywords WHERE message_id = ?1 AND keyword_key = ?2",
                        params![message_id, key],
                        |r| r.get(0),
                    )
                    .optional()?;
                if prior.as_deref() == Some("server") {
                    invalidate_message_mailbox_modseq(&tx, message_id)?;
                }
            }
            tx.execute(
                "INSERT INTO message_keywords (message_id, keyword_key, keyword, origin)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(message_id, keyword_key) DO UPDATE SET
                     keyword = excluded.keyword, origin = excluded.origin",
                params![message_id, key, keyword, origin],
            )?;
            if write_enabled {
                let payload = serde_json::json!({
                    "message_id": message_id,
                    "keyword": keyword,
                    "add": true,
                })
                .to_string();
                tx.execute(
                    "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'keyword', ?2)",
                    params![account_id, payload],
                )?;
                TagApplyOutcome {
                    synced: true,
                    reason: None,
                }
            } else {
                TagApplyOutcome {
                    synced: false,
                    reason: Some("evaluation_mode".into()),
                }
            }
        } else {
            let existing_origin: Option<String> = tx
                .query_row(
                    "SELECT origin FROM message_keywords WHERE message_id = ?1 AND keyword_key = ?2",
                    params![message_id, key],
                    |r| r.get(0),
                )
                .optional()?;
            tx.execute(
                "DELETE FROM message_keywords WHERE message_id = ?1 AND keyword_key = ?2",
                params![message_id, key],
            )?;
            if existing_origin.as_deref() == Some("server") && write_enabled {
                let payload = serde_json::json!({
                    "message_id": message_id,
                    "keyword": keyword,
                    "add": false,
                })
                .to_string();
                tx.execute(
                    "INSERT INTO operations (account_id, kind, payload_json) VALUES (?1, 'keyword', ?2)",
                    params![account_id, payload],
                )?;
                TagApplyOutcome {
                    synced: true,
                    reason: None,
                }
            } else if existing_origin.as_deref() == Some("server") {
                // Server-origin row, but writes are disabled: removed
                // locally; the server still has it until writes are enabled.
                // The server message is unchanged, so QRESYNC would never
                // re-report it: force the next sync to a full scan.
                invalidate_message_mailbox_modseq(&tx, message_id)?;
                TagApplyOutcome {
                    synced: false,
                    reason: Some("evaluation_mode".into()),
                }
            } else {
                // Local-only row (or none at all): nothing left to sync.
                TagApplyOutcome {
                    synced: true,
                    reason: None,
                }
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// Mute/unmute a message's whole conversation (every message sharing its
    /// `thread_id`, or just the message itself if it has none yet). Local
    /// only, keyed by RFC 5322 Message-ID (`COALESCE(rfc822_message_id, 'id:'
    /// || id)`) rather than the thread id, so it survives `rebuild_threads`
    /// and `rebuild_threads_incremental` (a root-changing merge or split gives
    /// the conversation a new `t:<root>` id; both rebuild paths recompute
    /// `threads.is_muted` from these anchors — see `compute_thread_groups`).
    /// Also sets `threads.is_muted` on the current thread row immediately, so
    /// the UI need not wait for the next sync. Serialised with each rebuild
    /// transaction by the connection mutex, so a mute committed mid-rebuild is
    /// either read by the next chunk or lands on the thread ids it wrote.
    pub fn mute_thread(&self, message_id: &str, muted: bool) -> Result<(), DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let row: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT account_id, thread_id FROM messages WHERE id = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((account_id, thread_id)) = row else {
            return Err(DbError(format!("unknown message id: {message_id}")));
        };
        let anchors: Vec<String> = match &thread_id {
            Some(tid) => tx
                .prepare(
                    "SELECT COALESCE(rfc822_message_id, 'id:' || id) FROM messages
                     WHERE thread_id = ?1",
                )?
                .query_map(params![tid], |r| r.get(0))?
                .collect::<Result<_, _>>()?,
            None => tx
                .prepare(
                    "SELECT COALESCE(rfc822_message_id, 'id:' || id) FROM messages WHERE id = ?1",
                )?
                .query_map(params![message_id], |r| r.get(0))?
                .collect::<Result<_, _>>()?,
        };
        let now = now_epoch();
        for anchor in &anchors {
            if muted {
                tx.execute(
                    "INSERT INTO thread_mutes (account_id, anchor, muted_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(account_id, anchor) DO NOTHING",
                    params![account_id, anchor, now],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM thread_mutes WHERE account_id = ?1 AND anchor = ?2",
                    params![account_id, anchor],
                )?;
            }
        }
        if let Some(tid) = &thread_id {
            tx.execute(
                "UPDATE threads SET is_muted = ?1 WHERE id = ?2",
                params![muted, tid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Populates `MessageHeader.keywords` on every header (S42), hidden
    /// system keywords filtered out. Deliberately NOT a join on the listing
    /// queries themselves (that would multiply rows for a message with
    /// several keywords) — one extra indexed `keywords_for_messages` query
    /// per page instead, via `Repo::row_to_header`'s callers.
    pub(crate) fn attach_keywords(&self, headers: &mut [MessageHeader]) -> Result<(), DbError> {
        if headers.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = headers.iter().map(|h| h.id.clone()).collect();
        let map = self.keywords_for_messages(&ids)?;
        for h in headers.iter_mut() {
            if let Some(kw) = map.get(&h.id) {
                h.keywords = kw
                    .iter()
                    .filter(|k| !is_hidden_keyword(k))
                    .cloned()
                    .collect();
            }
        }
        Ok(())
    }

    /// The subset of `message_ids` whose thread is currently muted, chunked
    /// at `KEYWORD_QUERY_CHUNK` (see `keywords_for_messages`).
    pub fn muted_among(&self, message_ids: &[String]) -> Result<Vec<String>, DbError> {
        let mut out = Vec::new();
        if message_ids.is_empty() {
            return Ok(out);
        }
        let conn = self.lock_conn();
        for chunk in message_ids.chunks(KEYWORD_QUERY_CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT m.id FROM messages m JOIN threads t ON t.id = m.thread_id
                 WHERE t.is_muted = 1 AND m.id IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| r.get::<_, String>(0))?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// Every distinct visible keyword currently on any message, with a
    /// message count (Task 8) — Settings' "In use on your servers" adopt
    /// list. Hidden protocol/system keywords (`HIDDEN_KEYWORDS`) are
    /// excluded; the underlying rows are never filtered, only this listing.
    pub fn list_keywords_in_use(&self) -> Result<Vec<KeywordInUse>, DbError> {
        let conn = self.lock_conn();
        let mut stmt = conn.prepare(
            "SELECT keyword_key, keyword, COUNT(*) FROM message_keywords
             GROUP BY keyword_key, keyword ORDER BY keyword",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (keyword_key, keyword, count) = row?;
            if is_hidden_keyword(&keyword) {
                continue;
            }
            out.push(KeywordInUse {
                keyword,
                keyword_key,
                count,
            });
        }
        Ok(out)
    }

    /// `{keyword_key, total, unread, local_only}` for every DEFINED tag, in
    /// `tag_definitions`' sort order — the sidebar/menu counts. A tag
    /// nobody has used yet still appears, with zeros. Counts follow the same
    /// rule as [`Database::messages_for_tag`] (MM-8 T2): every account and
    /// folder except Trash and Junk, so a badge never disagrees with the list
    /// it opens. `local_only` counts rows whose tag never reached a server.
    pub fn tag_counts(&self) -> Result<Vec<TagCount>, DbError> {
        let conn = self.lock_conn();
        let visible = tag_visible_sql("m");
        let dup = tag_duplicate_sql("m", "mk.keyword_key");
        let mut stmt = conn.prepare(&format!(
            "SELECT d.keyword_key,
                    COUNT(m.id),
                    COALESCE(SUM(CASE WHEN m.id IS NOT NULL
                          AND COALESCE(f.is_read, 0) = 0 THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN m.id IS NOT NULL
                          AND mk.origin = 'local' THEN 1 ELSE 0 END), 0)
             FROM tag_definitions d
             LEFT JOIN message_keywords mk ON mk.keyword_key = d.keyword_key
             LEFT JOIN messages m ON m.id = mk.message_id AND {visible} AND NOT {dup}
             LEFT JOIN message_flags f ON f.message_id = m.id
             GROUP BY d.keyword_key
             ORDER BY d.sort_order, d.display_name"
        ))?;
        let rows = stmt.query_map([], |r| {
            Ok(TagCount {
                keyword_key: r.get(0)?,
                total: r.get(1)?,
                unread: r.get(2)?,
                local_only: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Headers carrying the DEFINED tag `keyword` (matched by
    /// [`normalise_keyword`]), newest first (`COALESCE(sent_at, 0) DESC, id`),
    /// keyset-paged: `after` is `(sent key, id)` of the last row of the
    /// previous page (`sent key` = `sent_at` or 0), so rows removed or added
    /// meanwhile never shift a page. The sidebar Tags list (MM-8). Same
    /// visibility and de-duplication rule as [`Database::tag_counts`]. A keyword with no definition (or a hidden
    /// protocol keyword) yields `[]`, mirroring the sidebar showing defined
    /// tags only (T3).
    pub fn messages_for_tag(
        &self,
        keyword: &str,
        limit: i64,
        after: Option<(i64, &str)>,
    ) -> Result<Vec<MessageHeader>, DbError> {
        let key = normalise_keyword(keyword);
        let (after_sent, after_id, has_after) = match after {
            Some((s, id)) => (s, id, 1_i64),
            None => (0, "", 0),
        };
        let visible = tag_visible_sql("m");
        let dup = tag_duplicate_sql("m", "?1");
        if is_hidden_keyword(&key) {
            return Ok(Vec::new());
        }
        let mut headers: Vec<MessageHeader> = {
            let conn = self.lock_conn();
            let mut stmt = conn.prepare(&format!(
                "SELECT {HEADER_COLUMNS}
                 FROM messages m
                 LEFT JOIN message_flags f ON f.message_id = m.id
                 LEFT JOIN threads t ON t.id = m.thread_id
                 LEFT JOIN message_bodies b ON b.message_id = m.id
                 WHERE m.id IN (SELECT mk.message_id FROM message_keywords mk
                                WHERE mk.keyword_key = ?1)
                   AND EXISTS (SELECT 1 FROM tag_definitions d WHERE d.keyword_key = ?1)
                   AND {visible}
                   AND NOT {dup}
                   AND (?4 = 0 OR COALESCE(m.sent_at, 0) < ?3
                        OR (COALESCE(m.sent_at, 0) = ?3 AND m.id > ?5))
                 ORDER BY COALESCE(m.sent_at, 0) DESC, m.id
                 LIMIT ?2"
            ))?;
            let rows = stmt.query_map(
                params![key, limit.max(0), after_sent, has_after, after_id],
                row_to_header,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        self.attach_keywords(&mut headers)?;
        self.attach_recipients(&mut headers)?;
        Ok(headers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn fresh_db() -> Database {
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = castellum_testutil::scratch_root().join(format!(
            "castellum-tags-test-{}-{}.db",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_file(&path);
        Database::open(&path).expect("open")
    }

    /// The bare minimum accounts/mailboxes/messages rows a `message_keywords`
    /// FK insert needs (foreign_keys=ON — see `Database::open`).
    fn seed_minimal_message(db: &Database, message_id: &str) {
        let conn = db.lock_conn();
        conn.execute(
            "INSERT INTO accounts (id, email, provider) VALUES ('acc1', 'a@x.test', 'imap')",
            [],
        )
        .expect("seed account");
        conn.execute(
            "INSERT INTO mailboxes (id, account_id, path, name) VALUES ('mb1', 'acc1', 'INBOX', 'INBOX')",
            [],
        )
        .expect("seed mailbox");
        conn.execute(
            "INSERT INTO messages (id, account_id, mailbox_id) VALUES (?1, 'acc1', 'mb1')",
            params![message_id],
        )
        .expect("seed message");
    }

    /// Every derived keyword must be a valid IMAP atom: RFC 9051 ATOM-CHAR
    /// excludes atom-specials ("(", ")", "{", SP, CTL, list-wildcards,
    /// quoted-specials, resp-specials); our allowed set (ASCII alnum plus
    /// `$ _ - . +`) is a strict subset, so this check is sufficient without
    /// pulling in imap-types as a dev-dependency just for this assertion.
    fn is_atom_safe(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '$' | '_' | '-' | '.' | '+'))
    }

    #[test]
    fn keyword_for_display_name_maps_disallowed_chars_and_falls_back_when_empty() {
        let never_taken = |_: &str| false;
        assert_eq!(keyword_for_display_name("Work", &never_taken), "Work");
        assert_eq!(keyword_for_display_name("To do", &never_taken), "To_do");
        assert_eq!(keyword_for_display_name("Café", &never_taken), "Caf_");
        assert_eq!(
            keyword_for_display_name("(urgent)", &never_taken),
            "_urgent_"
        );
        assert_eq!(keyword_for_display_name("\\Seen", &never_taken), "Seen");
        assert_eq!(
            keyword_for_display_name("日本", &never_taken),
            "$CastellumTag1"
        );
        // $CastellumTag1 already taken -> next free n.
        let one_taken = |c: &str| c == "$CastellumTag1";
        assert_eq!(
            keyword_for_display_name("日本", &one_taken),
            "$CastellumTag2"
        );

        for input in ["Work", "To do", "Café", "(urgent)", "\\Seen", "日本"] {
            assert!(
                is_atom_safe(&keyword_for_display_name(input, &never_taken)),
                "not atom-safe: {input:?}"
            );
        }
    }

    #[test]
    fn is_hidden_keyword_matches_case_insensitively() {
        assert!(is_hidden_keyword("$junk"));
        assert!(is_hidden_keyword("$JUNK"));
        assert!(is_hidden_keyword("$Muted"));
        assert!(!is_hidden_keyword("Work"));
    }

    /// FP43: a name whose keyword is a hidden protocol keyword would make an
    /// invisible tag, so creation refuses it.
    #[test]
    fn upsert_tag_definition_rejects_hidden_keyword_names() {
        let db = fresh_db();
        let mk = |name: &str| TagDefinition {
            keyword: String::new(),
            keyword_key: String::new(),
            display_name: name.into(),
            colour: "blue".into(),
            sort_order: 0,
        };
        for name in ["Junk", "junk", "NonJunk", "$Forwarded"] {
            let err = db
                .upsert_tag_definition(&mk(name))
                .expect_err("reserved name must be refused");
            assert!(err.0.contains("reserved"), "{name}: {}", err.0);
        }
        db.upsert_tag_definition(&mk("Junk mail"))
            .expect("a longer name is fine");
    }

    #[test]
    fn upsert_tag_definition_derives_keyword_and_rejects_duplicate_name() {
        let db = fresh_db();
        let created = db
            .upsert_tag_definition(&TagDefinition {
                keyword: String::new(),
                keyword_key: String::new(),
                display_name: "Client X".into(),
                colour: "blue".into(),
                sort_order: 0,
            })
            .expect("create");
        assert_eq!(created.keyword, "Client_X");
        assert_eq!(created.keyword_key, "client_x");

        let err = db
            .upsert_tag_definition(&TagDefinition {
                keyword: String::new(),
                keyword_key: String::new(),
                display_name: "Client X".into(),
                colour: "red".into(),
                sort_order: 1,
            })
            .expect_err("duplicate display name must be rejected");
        assert!(
            err.0.contains("Client X") && err.0.contains("already exists"),
            "{}",
            err.0
        );

        let listed = db.list_tag_definitions().expect("list");
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn create_rejects_a_keyword_key_clash_instead_of_overwriting() {
        let db = fresh_db();
        let mk = |name: &str, colour: &str| TagDefinition {
            keyword: String::new(),
            keyword_key: String::new(),
            display_name: name.into(),
            colour: colour.into(),
            sort_order: 0,
        };
        db.upsert_tag_definition(&mk("Work", "blue"))
            .expect("create");
        db.upsert_tag_definition(&mk("To do", "green"))
            .expect("create");
        // "work" derives the same case-insensitive key as "Work"; "To_do"
        // maps to the same key as "To do". Neither may rewrite the original.
        for (name, existing) in [("work", "Work"), ("To_do", "To do")] {
            let err = db
                .upsert_tag_definition(&mk(name, "red"))
                .expect_err("key clash must be rejected");
            assert!(err.0.contains("already exists"), "{}", err.0);
            let listed = db.list_tag_definitions().expect("list");
            let orig = listed
                .iter()
                .find(|t| t.display_name == existing)
                .expect("original kept");
            assert_ne!(orig.colour, "red", "{existing} was recoloured");
        }
        assert_eq!(db.list_tag_definitions().expect("list").len(), 2);
    }

    #[test]
    fn adopting_a_keyword_keeps_its_server_spelling() {
        let db = fresh_db();
        let adopted = db
            .upsert_tag_definition(&TagDefinition {
                keyword: "Follow/up".into(),
                keyword_key: String::new(),
                display_name: "Follow/up".into(),
                colour: "red".into(),
                sort_order: 0,
            })
            .expect("adopt");
        assert_eq!(adopted.keyword, "Follow/up");
        assert_eq!(adopted.keyword_key, "follow/up");
    }

    #[test]
    fn demote_never_inserts_and_only_updates_an_existing_row() {
        let db = fresh_db();
        seed_minimal_message(&db, "msg1");
        // No row: nothing is created.
        assert!(!db.demote_keyword_to_local("msg1", "Work").unwrap());
        assert!(db
            .keywords_for_messages(&["msg1".to_string()])
            .unwrap()
            .is_empty());
        // A server row is demoted, and then survives an empty server report.
        db.replace_server_keywords("msg1", &["Work".to_string()])
            .unwrap();
        assert!(db.demote_keyword_to_local("msg1", "work").unwrap());
        db.replace_server_keywords("msg1", &[]).unwrap();
        assert_eq!(
            db.keywords_for_messages(&["msg1".to_string()]).unwrap()["msg1"],
            vec!["Work".to_string()]
        );
    }

    #[test]
    fn batch_replace_matches_per_message_semantics_in_one_pass() {
        let db = fresh_db();
        seed_minimal_message(&db, "m1");
        {
            let conn = db.lock_conn();
            for id in ["m2", "m3"] {
                conn.execute(
                    "INSERT INTO messages (id, account_id, mailbox_id) VALUES (?1, 'acc1', 'mb1')",
                    params![id],
                )
                .unwrap();
            }
        }
        db.replace_server_keywords("m1", &["Old".to_string()])
            .unwrap();
        db.set_message_keyword("m2", "Mine", true).unwrap(); // local-only row
        let (a, b, c) = (
            vec!["New".to_string()],
            vec!["$Label1".to_string()],
            vec!["Same".to_string()],
        );
        db.replace_server_keywords("m3", &c).unwrap();
        let changed = db
            .replace_server_keywords_batch(
                "acc1",
                "mb1",
                &[("m1", &a[..]), ("m2", &b[..]), ("m3", &c[..])],
            )
            .unwrap();
        assert_eq!(changed, 2, "m3 already matched and must not count");
        let map = db
            .keywords_for_messages(&["m1".into(), "m2".into(), "m3".into()])
            .unwrap();
        assert_eq!(map["m1"], vec!["New".to_string()]);
        let mut m2 = map["m2"].clone();
        m2.sort();
        assert_eq!(m2, vec!["$Label1".to_string(), "Mine".to_string()]);
        assert_eq!(map["m3"], vec!["Same".to_string()]);
        assert_eq!(db.server_keyword_keys("m2").unwrap().len(), 1);
    }

    #[test]
    fn replace_server_keywords_keeps_local_rows_and_upgrades_matching_ones() {
        let db = fresh_db();
        seed_minimal_message(&db, "msg1");
        db.set_message_keyword("msg1", "PersonalNote", true)
            .expect("seed local row");
        db.set_message_keyword("msg1", "Work", true).expect("seed");

        // The server now also reports "Work" (case differs on the wire) and
        // a brand-new "$Label1"; "PersonalNote" is never mentioned.
        let changed = db
            .replace_server_keywords("msg1", &["work".to_string(), "$Label1".to_string()])
            .expect("replace");
        assert!(changed);

        let map = db
            .keywords_for_messages(&["msg1".to_string()])
            .expect("keywords_for_messages");
        let mut got = map.get("msg1").cloned().unwrap_or_default();
        got.sort();
        let mut want = vec![
            "PersonalNote".to_string(),
            "work".to_string(),
            "$Label1".to_string(),
        ];
        want.sort();
        assert_eq!(got, want);

        // Re-running with the same set is a no-op.
        let changed_again = db
            .replace_server_keywords("msg1", &["work".to_string(), "$Label1".to_string()])
            .expect("replace again");
        assert!(!changed_again);
    }

    // #3457: tag_counts selected a column (messages.is_read) that lives in
    // message_flags, so every real call failed; only an invalid handle was tested.
    #[test]
    fn tag_counts_counts_total_and_unread_per_defined_tag() {
        let db = fresh_db();
        seed_minimal_message(&db, "m1");
        {
            let conn = db.lock_conn();
            conn.execute(
                "INSERT INTO messages (id, account_id, mailbox_id) VALUES ('m2', 'acc1', 'mb1')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message_flags (message_id, is_read, is_starred) VALUES ('m1', 1, 0), ('m2', 0, 0)",
                [],
            )
            .unwrap();
        }
        db.upsert_tag_definition(&TagDefinition {
            keyword: String::new(),
            keyword_key: String::new(),
            display_name: "Work".into(),
            colour: "blue".into(),
            sort_order: 0,
        })
        .unwrap();
        db.upsert_tag_definition(&TagDefinition {
            keyword: String::new(),
            keyword_key: String::new(),
            display_name: "Unused".into(),
            colour: "grey".into(),
            sort_order: 1,
        })
        .unwrap();
        db.set_message_keyword("m1", "Work", true).unwrap();
        db.set_message_keyword("m2", "Work", true).unwrap();
        let counts = db.tag_counts().expect("tag_counts must run on a real db");
        let work = counts.iter().find(|c| c.keyword_key == "work").unwrap();
        assert_eq!((work.total, work.unread), (2, 1));
        let unused = counts.iter().find(|c| c.keyword_key == "unused").unwrap();
        assert_eq!((unused.total, unused.unread), (0, 0));
    }

    /// MM-8 fixture: Work on 5 messages across two accounts: 2 unread in
    /// Inbox (one origin local), 1 read in a second account's Archive, 1 in
    /// Trash and 1 in Junk (both must be excluded from count and list).
    fn seed_tag_fixture() -> Database {
        let db = fresh_db();
        {
            let conn = db.lock_conn();
            conn.execute_batch(
                "INSERT INTO accounts (id, email, provider) VALUES ('a1','a@x.test','imap'),('a2','b@x.test','imap');
                 INSERT INTO mailboxes (id, account_id, path, name, role) VALUES
                   ('a1inbox','a1','INBOX','INBOX','inbox'),
                   ('a1trash','a1','Trash','Trash','trash'),
                   ('a1junk','a1','Junk','Junk','junk'),
                   ('a2arch','a2','Archive','Archive','archive');
                 INSERT INTO messages (id, account_id, mailbox_id, sent_at) VALUES
                   ('m1','a1','a1inbox',100),('m2','a1','a1inbox',300),
                   ('m3','a2','a2arch',200),('m4','a1','a1trash',400),('m5','a1','a1junk',500);
                 INSERT INTO message_flags (message_id, is_read, is_starred) VALUES
                   ('m1',0,0),('m2',0,0),('m3',1,0),('m4',0,0),('m5',0,0);",
            )
            .expect("seed fixture");
        }
        for (name, order) in [("Work", 0), ("Empty", 1)] {
            db.upsert_tag_definition(&TagDefinition {
                keyword: String::new(),
                keyword_key: String::new(),
                display_name: name.into(),
                colour: "blue".into(),
                sort_order: order,
            })
            .unwrap();
        }
        for id in ["m1", "m2", "m3", "m4", "m5"] {
            db.set_message_keyword(id, "Work", true).unwrap();
        }
        // acc defaults to evaluation mode, so every row is origin 'local';
        // promote m2 and m3 to a server-confirmed origin.
        db.lock_conn()
            .execute(
                "UPDATE message_keywords SET origin = 'server' WHERE message_id IN ('m2','m3')",
                [],
            )
            .unwrap();
        db
    }

    #[test]
    fn tag_counts_exclude_trash_and_junk_and_report_local_only() {
        let db = seed_tag_fixture();
        let counts = db.tag_counts().unwrap();
        let work = counts.iter().find(|c| c.keyword_key == "work").unwrap();
        // m1, m2 (inbox, unread) + m3 (read, other account); m4/m5 excluded.
        assert_eq!((work.total, work.unread, work.local_only), (3, 2, 1));
        let empty = counts.iter().find(|c| c.keyword_key == "empty").unwrap();
        assert_eq!((empty.total, empty.unread, empty.local_only), (0, 0, 0));
        assert_eq!(counts[0].keyword_key, "work", "sort_order is kept");
    }

    #[test]
    fn messages_for_tag_matches_total_pages_and_hides_unknown() {
        let db = seed_tag_fixture();
        let all = db.messages_for_tag("work", 100, None).unwrap();
        let ids: Vec<&str> = all.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["m2", "m3", "m1"], "newest first, no trash/junk");
        let total = db
            .tag_counts()
            .unwrap()
            .iter()
            .find(|c| c.keyword_key == "work")
            .unwrap()
            .total;
        assert_eq!(all.len() as i64, total, "list and badge agree");
        assert_eq!(all[0].keywords, vec!["Work"], "keywords are attached");

        // Case-insensitive, and keyset pages neither overlap nor skip.
        let p1 = db.messages_for_tag("WORK", 2, None).unwrap();
        let last = p1.last().unwrap();
        let p2 = db
            .messages_for_tag("work", 2, Some((last.sent_at.unwrap_or(0), &last.id)))
            .unwrap();
        let paged: Vec<&str> = p1.iter().chain(p2.iter()).map(|h| h.id.as_str()).collect();
        assert_eq!(paged, ids);

        assert!(db.messages_for_tag("empty", 10, None).unwrap().is_empty());
        assert!(db
            .messages_for_tag("nosuchtag", 10, None)
            .unwrap()
            .is_empty());
        assert!(db
            .messages_for_tag("$Forwarded", 10, None)
            .unwrap()
            .is_empty());
    }

    fn work_total(db: &Database) -> (i64, i64) {
        let c = db.tag_counts().unwrap();
        let w = c.iter().find(|c| c.keyword_key == "work").unwrap();
        (w.total, w.unread)
    }

    #[test]
    fn snoozed_tagged_mail_is_hidden_from_count_and_list() {
        let db = seed_tag_fixture();
        db.snooze_message("m1", 4_000_000_000).unwrap();
        assert_eq!(work_total(&db), (2, 1));
        let ids: Vec<String> = db
            .messages_for_tag("work", 100, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, vec!["m2", "m3"]);
        db.unsnooze_message("m1").unwrap();
        assert_eq!(work_total(&db), (3, 2));
    }

    #[test]
    fn one_email_in_inbox_and_all_mail_counts_and_lists_once_inbox_copy_wins() {
        let db = seed_tag_fixture();
        {
            let conn = db.lock_conn();
            // Gmail All Mail (role archive) holds a copy of m1 with the same Message-ID.
            conn.execute_batch(
                "UPDATE messages SET rfc822_message_id = '<one@x>' WHERE id = 'm1';
                 INSERT INTO mailboxes (id, account_id, path, name, role)
                   VALUES ('a1all','a1','[Gmail]/All Mail','All Mail','archive');
                 INSERT INTO messages (id, account_id, mailbox_id, sent_at, rfc822_message_id)
                   VALUES ('m0','a1','a1all',100,'<one@x>');
                 INSERT INTO message_flags (message_id, is_read, is_starred) VALUES ('m0',0,0);",
            )
            .unwrap();
        }
        db.set_message_keyword("m0", "Work", true).unwrap();
        // m0 sorts before m1 by id but lives in the archive-role mailbox.
        assert_eq!(work_total(&db), (3, 2), "m1+m0 are one email");
        let ids: Vec<String> = db
            .messages_for_tag("work", 100, None)
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, vec!["m2", "m3", "m1"]);
        // A copy of the same email that is not tagged elsewhere stays visible alone.
        db.set_message_keyword("m1", "Work", false).unwrap();
        assert_eq!(work_total(&db), (3, 2));
        assert!(db
            .messages_for_tag("work", 100, None)
            .unwrap()
            .iter()
            .any(|h| h.id == "m0"));
    }

    #[test]
    fn set_message_keyword_in_evaluation_mode_stays_local_with_zero_ops() {
        let db = fresh_db();
        seed_minimal_message(&db, "msg1");
        // acc1 defaults to write_enabled = 0 (evaluation mode).
        let outcome = db
            .set_message_keyword("msg1", "Work", true)
            .expect("set keyword");
        assert_eq!(
            outcome,
            TagApplyOutcome {
                synced: false,
                reason: Some("evaluation_mode".into()),
            }
        );
        let map = db
            .keywords_for_messages(&["msg1".to_string()])
            .expect("keywords_for_messages");
        assert_eq!(map.get("msg1").cloned().unwrap_or_default(), vec!["Work"]);
        assert!(
            db.pending_operations("acc1")
                .expect("pending_operations")
                .is_empty(),
            "evaluation mode must queue zero ops"
        );
    }

    #[test]
    fn set_message_keyword_write_enabled_queues_one_keyword_op() {
        let db = fresh_db();
        seed_minimal_message(&db, "msg1");
        db.set_account_write_enabled("acc1", true)
            .expect("enable writes");
        let outcome = db
            .set_message_keyword("msg1", "Work", true)
            .expect("set keyword");
        assert_eq!(
            outcome,
            TagApplyOutcome {
                synced: true,
                reason: None,
            }
        );
        let ops = db.pending_operations("acc1").expect("pending_operations");
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, "keyword");
        let payload: serde_json::Value = serde_json::from_str(&ops[0].payload_json).unwrap();
        assert_eq!(payload["message_id"], "msg1");
        assert_eq!(payload["keyword"], "Work");
        assert_eq!(payload["add"], true);
    }

    #[test]
    fn removing_a_local_only_keyword_enqueues_nothing() {
        let db = fresh_db();
        seed_minimal_message(&db, "msg1");
        // Demoted (or evaluation-mode-added) keywords are origin='local':
        // add it while the account is still read-only, then enable writes.
        db.set_message_keyword("msg1", "Local", true)
            .expect("add local");
        db.set_account_write_enabled("acc1", true)
            .expect("enable writes");
        let outcome = db
            .set_message_keyword("msg1", "Local", false)
            .expect("remove keyword");
        assert_eq!(
            outcome,
            TagApplyOutcome {
                synced: true,
                reason: None,
            }
        );
        assert!(
            db.pending_operations("acc1")
                .expect("pending_operations")
                .is_empty(),
            "removing a local-only tag must never touch the server"
        );
        assert!(!db
            .keywords_for_messages(&["msg1".to_string()])
            .expect("keywords_for_messages")
            .contains_key("msg1"));
    }

    #[test]
    fn migration_0041_lands_at_the_expected_version() {
        // 0041 is applied on a fresh profile. Later migrations (0042, MM-1
        // smart-mailbox groups) sit above it, so pin "41 is applied" rather
        // than "latest == 41".
        assert!(crate::latest_schema_version() >= 41);
        let db = fresh_db();
        assert_eq!(
            db.schema_version().expect("schema_version"),
            crate::latest_schema_version()
        );
        // threads.is_muted must exist (ALTER TABLE in the migration).
        let conn = db.lock_conn();
        let applied: i64 = conn
            .query_row("SELECT COUNT(*) FROM _migrations WHERE id = 41", [], |r| {
                r.get(0)
            })
            .expect("_migrations row for 41");
        assert_eq!(applied, 1, "migration 0041 recorded as applied");
        conn.query_row("SELECT is_muted FROM threads WHERE 0", [], |_| Ok(()))
            .optional()
            .expect("threads.is_muted column exists");
    }
}
