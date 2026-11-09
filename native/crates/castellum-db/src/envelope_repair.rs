// Castellum - one-off repair of ENVELOPE text stored RFC 2047-encoded (X1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Builds before stage X1 stored a descriptor's subject and display names as
// the IMAP server sent them, so on a standards-following server the rows hold
// `=?UTF-8?Q?...?=` text. New mail is decoded at the provider;
// this repairs what is already stored.
//
// Shape:
//   - One call = one bounded batch: the next `limit` messages in id order,
//     in ONE transaction. The connection mutex is released between batches,
//     so the UI never waits for more than one batch.
//   - Progress is a cursor in `app_meta`, written in the same transaction as
//     the batch's row changes: an interrupted run (quit, crash) resumes at
//     the first unprocessed message, and nothing is half-applied.
//   - When the scan reaches the end it records a done marker and every later
//     call returns at once. One-shot on purpose: after X1 no row is stored
//     encoded, and re-running over decoded text could wrongly "decode" a
//     subject that legitimately contains something shaped like an encoded
//     word.
//   - A row is written only when decoding actually changes its text. Rows
//     with no encoded word are never touched.
//   - A repaired message has its FTS row rewritten (subject, sender) and is
//     marked thread-dirty, so the caller's incremental thread rebuild
//     refreshes `threads.subject`.
//
// The decoder is passed in (`castellum-db` sits below the IMAP provider that
// owns it): `decode(text)` returns `Some(decoded)` only when `text` holds an
// encoded word that decodes to something different.

use crate::repo::{ensure_fts_rowid, mark_threads_dirty};
use crate::{Database, DbError};
use rusqlite::{params, OptionalExtension};
use std::collections::BTreeSet;

/// `app_meta` key: present once the whole profile has been scanned.
const DONE_KEY: &str = "envelope_text_repair_done";
/// `app_meta` key: id of the last message a committed batch processed.
const CURSOR_KEY: &str = "envelope_text_repair_cursor";
/// Cheap SQL pre-filter for "may hold an encoded word".
const ENCODED_WORD_LIKE: &str = "%=?%?=%";

/// What one [`Database::repair_encoded_envelope_text_batch`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvelopeTextRepairBatch {
    /// Messages this batch looked at (0 once the repair is done).
    pub scanned: usize,
    /// Messages with at least one value rewritten.
    pub messages_repaired: usize,
    /// Subjects rewritten.
    pub subjects_repaired: usize,
    /// Display names rewritten (`messages.from_name` and
    /// `message_addresses.name`).
    pub names_repaired: usize,
    /// Accounts with a repaired message: their threads need a rebuild.
    pub affected_accounts: Vec<String>,
    /// The scan has reached the end of the profile (now, or on an earlier
    /// run); there is nothing left to do.
    pub done: bool,
}

/// `Some(new_value)` when decoding changes `value` (`new_value` is `None` if
/// nothing but white space is left, as at ingest); `None` to leave it alone.
fn repaired(
    value: Option<&str>,
    decode: &dyn Fn(&str) -> Option<String>,
) -> Option<Option<String>> {
    let value = value?;
    if !value.contains("=?") {
        return None;
    }
    let decoded = decode(value)?;
    let decoded = decoded.trim();
    if decoded == value {
        return None;
    }
    Some((!decoded.is_empty()).then(|| decoded.to_string()))
}

impl Database {
    /// Whether the one-off encoded-text repair has completed for this profile.
    pub fn envelope_text_repair_done(&self) -> Result<bool, DbError> {
        let conn = self.lock_conn();
        Ok(conn
            .query_row(
                "SELECT 1 FROM app_meta WHERE key = ?1",
                params![DONE_KEY],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Repair the next `limit` messages (see the module comment). Call until
    /// the result says `done`; safe to stop at any point and resume later.
    pub fn repair_encoded_envelope_text_batch(
        &self,
        limit: usize,
        decode: &dyn Fn(&str) -> Option<String>,
    ) -> Result<EnvelopeTextRepairBatch, DbError> {
        let mut conn = self.lock_conn();
        let tx = conn.transaction()?;
        let mut batch = EnvelopeTextRepairBatch::default();

        let already_done = tx
            .query_row(
                "SELECT 1 FROM app_meta WHERE key = ?1",
                params![DONE_KEY],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if already_done {
            batch.done = true;
            return Ok(batch);
        }
        let cursor: String = tx
            .query_row(
                "SELECT value FROM app_meta WHERE key = ?1",
                params![CURSOR_KEY],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_default();

        // The window: the next `limit` messages by primary key. Bounded by
        // rows scanned, not rows matched, so a profile with nothing to
        // repair still never holds the connection for a whole-table scan.
        type Row = (String, String, Option<String>, Option<String>);
        let window: Vec<Row> = tx
            .prepare(
                "SELECT id, account_id, subject, from_name FROM messages
                 WHERE id > ?1 ORDER BY id LIMIT ?2",
            )?
            .query_map(params![cursor, limit.max(1) as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<_, _>>()?;
        batch.scanned = window.len();

        let mut affected = BTreeSet::new();
        if let Some((last_id, ..)) = window.last() {
            // Encoded display names in the same window, one indexed range
            // read (`idx_message_addresses_msg`).
            let names: Vec<(i64, String, String)> = tx
                .prepare(
                    "SELECT id, message_id, name FROM message_addresses
                     WHERE message_id > ?1 AND message_id <= ?2 AND name LIKE ?3",
                )?
                .query_map(params![cursor, last_id, ENCODED_WORD_LIKE], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?
                .collect::<Result<_, _>>()?;
            let mut touched: BTreeSet<&str> = BTreeSet::new();
            for (address_row, message_id, name) in &names {
                if let Some(new_name) = repaired(Some(name), decode) {
                    tx.execute(
                        "UPDATE message_addresses SET name = ?2 WHERE id = ?1",
                        params![address_row, new_name],
                    )?;
                    batch.names_repaired += 1;
                    touched.insert(message_id);
                }
            }

            for (id, account_id, subject, from_name) in &window {
                let new_subject = repaired(subject.as_deref(), decode);
                let new_from_name = repaired(from_name.as_deref(), decode);
                if new_subject.is_none() && new_from_name.is_none() {
                    if touched.contains(id.as_str()) {
                        batch.messages_repaired += 1;
                        affected.insert(account_id.clone());
                    }
                    continue;
                }
                batch.subjects_repaired += usize::from(new_subject.is_some());
                batch.names_repaired += usize::from(new_from_name.is_some());
                batch.messages_repaired += 1;
                affected.insert(account_id.clone());

                let subject = new_subject.unwrap_or_else(|| subject.clone());
                let from_name = new_from_name.unwrap_or_else(|| from_name.clone());
                tx.execute(
                    "UPDATE messages SET subject = ?2, from_name = ?3 WHERE id = ?1",
                    params![id, subject, from_name],
                )?;
                // Same indexed text `upsert_message` writes, on the same
                // stable FTS rowid; the body column is left as it is.
                let (indexed_subject, indexed_sender): (String, String) = tx.query_row(
                    "SELECT COALESCE(subject, ''),
                            TRIM(COALESCE(from_name, '') || ' ' || COALESCE(from_address, ''))
                     FROM messages WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let fts_rowid = ensure_fts_rowid(&tx, id)?;
                let updated = tx.execute(
                    "UPDATE message_fts SET subject = ?2, sender = ?3 WHERE rowid = ?1",
                    params![fts_rowid, indexed_subject, indexed_sender],
                )?;
                if updated == 0 {
                    tx.execute(
                        "INSERT INTO message_fts (rowid, message_id, subject, sender, body)
                         VALUES (?1, ?2, ?3, ?4, '')",
                        params![fts_rowid, id, indexed_subject, indexed_sender],
                    )?;
                }
                // `threads.subject` is a copy of the root message's subject.
                mark_threads_dirty(&tx, account_id, &[id.as_str()])?;
            }

            tx.execute(
                "INSERT INTO app_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![CURSOR_KEY, last_id],
            )?;
        }

        if window.len() < limit.max(1) {
            tx.execute(
                "INSERT INTO app_meta (key, value) VALUES (?1, '1')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![DONE_KEY],
            )?;
            tx.execute("DELETE FROM app_meta WHERE key = ?1", params![CURSOR_KEY])?;
            batch.done = true;
        }
        batch.affected_accounts = affected.into_iter().collect();
        tx.commit()?;
        Ok(batch)
    }
}
