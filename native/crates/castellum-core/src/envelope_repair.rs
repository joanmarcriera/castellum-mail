// Castellum - repair ENVELOPE text stored RFC 2047-encoded (stage X1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Builds before X1 stored descriptor subjects and display names exactly as
// the IMAP server sent them, so a profile synced from a standards-following
// server holds rows like `=?UTF-8?Q?Solutions_Architect_...?=`.
// New mail is decoded in the IMAP provider; this drives the one-off repair of
// what is already stored (`castellum_db::Database::
// repair_encoded_envelope_text_batch` does the row work and explains the
// cursor / done-marker protocol).
//
// Usage: call `Profile::repair_encoded_envelope_text_json` once per launch,
// OFF the launch path (the app does it after the first list is on screen,
// before the contacts refresh). It is idempotent and one-shot: after the
// first complete run it returns `{"already_done":true,...}` at once. It may
// be interrupted at any point; the next call resumes where it stopped.
//
// Hydrated rows are not a special case: their subject came from mail-parser,
// already decoded, so they hold no encoded word and are never written.

use crate::{CoreError, Profile};
use castellum_db::Database;
use castellum_provider_imap::encoded_words::decode_encoded_words_if_any;
use serde::Serialize;

/// Messages examined per transaction. Small enough that the connection is
/// never held for long (a batch with nothing to repair is one indexed range
/// read), large enough that a 100k-message profile is ~50 batches.
pub const REPAIR_BATCH_MESSAGES: usize = 2_000;

/// What one repair run did. All counts are zero when `already_done`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EnvelopeTextRepairReport {
    /// The repair had completed on an earlier run; nothing was read.
    pub already_done: bool,
    /// Transactions this run committed.
    pub batches: usize,
    /// Messages examined.
    pub scanned: usize,
    /// Messages with at least one value rewritten.
    pub messages_repaired: usize,
    /// Subjects rewritten.
    pub subjects_repaired: usize,
    /// Display names rewritten (sender and recipients).
    pub names_repaired: usize,
}

/// Run the repair to completion in [`REPAIR_BATCH_MESSAGES`] batches.
pub fn repair_encoded_envelope_text(db: &Database) -> Result<EnvelopeTextRepairReport, CoreError> {
    repair_encoded_envelope_text_in_batches(db, REPAIR_BATCH_MESSAGES)
}

/// [`repair_encoded_envelope_text`] with an explicit batch size (tests use a
/// tiny one to exercise the cursor).
pub fn repair_encoded_envelope_text_in_batches(
    db: &Database,
    batch_messages: usize,
) -> Result<EnvelopeTextRepairReport, CoreError> {
    let db_err = |e: castellum_db::DbError| CoreError::Db(e.to_string());
    let mut report = EnvelopeTextRepairReport::default();
    if db.envelope_text_repair_done().map_err(db_err)? {
        report.already_done = true;
        return Ok(report);
    }
    loop {
        let batch = db
            .repair_encoded_envelope_text_batch(batch_messages, &decode_encoded_words_if_any)
            .map_err(db_err)?;
        report.batches += 1;
        report.scanned += batch.scanned;
        report.messages_repaired += batch.messages_repaired;
        report.subjects_repaired += batch.subjects_repaired;
        report.names_repaired += batch.names_repaired;
        // Re-thread what this batch touched (`threads.subject` copies the
        // root's subject). The batch already marked those messages dirty in
        // its own transaction, so if we are interrupted right here the next
        // sync's rebuild picks them up.
        for account_id in &batch.affected_accounts {
            db.rebuild_threads_incremental(account_id).map_err(db_err)?;
        }
        if batch.done {
            return Ok(report);
        }
    }
}

impl Profile {
    /// Run the one-off encoded-text repair; returns a JSON
    /// [`EnvelopeTextRepairReport`].
    pub fn repair_encoded_envelope_text_json(&self) -> Result<String, CoreError> {
        let report = repair_encoded_envelope_text(&self.db)?;
        serde_json::to_string(&report).map_err(|_| CoreError::Io)
    }
}
