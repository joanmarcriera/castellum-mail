// Castellum - direct-SQLite message fixtures for S66's List-Unsubscribe
// hardening tests.
//
// Why direct SQLite, not a real sync: every public way to get a message row
// into a profile (`CastellumCore.syncAccount`, `.importEml`) requires a
// reachable IMAP server (GreenMail locally) — there is no offline seam in
// the FFI surface for inserting a message directly (checked: castellum.h
// has no such call, `Database::enqueue_eml_import` only queues an
// IMAP-APPEND-and-resync round trip, per its own doc comment). Per the S66
// plan's Assumption 3 fallback, this writes directly into the profile's
// `castellum.db` (opened separately, alongside Rust's own live connection —
// safe because `Database::open` runs in WAL mode, which supports multiple
// connections in one process) using ONLY the columns
// `Database::get_message_detail` actually reads (all optional fields are
// joined with LEFT JOIN — see castellum-db/src/repo.rs — so this fixture
// never has to track schema growth it doesn't care about). The inserted row
// is then read back through the REAL FFI path (`CastellumCore.getMessage`),
// so every S66 test exercises Rust's actual decode/serialize path, not a
// reimplementation of it.
//
// Column list source: native/crates/castellum-db/src/migrations/0002_core_
// mail.sql (accounts, mailboxes, messages base columns) and .../0027_list_
// unsubscribe.sql (the three list_unsubscribe_* columns). Only NOT NULL
// columns without a SQL default are ever supplied.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation
import SQLite3

enum DirectFixtureError: Error {
    case sqlite(String)
}

enum DirectFixture {
    /// Opens `castellum.db` inside `profileDirectory` (must already exist —
    /// i.e. a `CastellumCore`/`AppModel` was already constructed against
    /// this directory, which runs the migrations) and runs `body` with a raw
    /// connection, closing it afterwards regardless of outcome.
    private static func withConnection<T>(
        profileDirectory: URL, _ body: (OpaquePointer) throws -> T
    ) throws -> T {
        let path = profileDirectory.appendingPathComponent("castellum.db").path
        var db: OpaquePointer?
        guard sqlite3_open(path, &db) == SQLITE_OK, let db else {
            throw DirectFixtureError.sqlite("sqlite3_open(\(path)) failed")
        }
        defer { sqlite3_close(db) }
        sqlite3_exec(db, "PRAGMA foreign_keys=ON;", nil, nil, nil)
        return try body(db)
    }

    private static func run(_ db: OpaquePointer, _ sql: String, _ bind: (OpaquePointer) -> Void) throws {
        var stmt: OpaquePointer?
        guard sqlite3_prepare_v2(db, sql, -1, &stmt, nil) == SQLITE_OK, let stmt else {
            let msg = String(cString: sqlite3_errmsg(db))
            throw DirectFixtureError.sqlite("prepare failed: \(msg) — \(sql)")
        }
        defer { sqlite3_finalize(stmt) }
        bind(stmt)
        guard sqlite3_step(stmt) == SQLITE_DONE else {
            let msg = String(cString: sqlite3_errmsg(db))
            throw DirectFixtureError.sqlite("step failed: \(msg) — \(sql)")
        }
    }

    /// Binds `text` at 1-based `index`, or NULL when it's nil. `SQLITE_
    /// TRANSIENT` (-1) makes SQLite copy the bytes, so the Swift string can
    /// be freed immediately after — required since these calls are not
    /// wrapped in `String.withCString` here.
    private static func bindText(_ stmt: OpaquePointer, _ index: Int32, _ text: String?) {
        if let text {
            sqlite3_bind_text(stmt, index, text, -1, unsafeBitCast(-1, to: sqlite3_destructor_type.self))
        } else {
            sqlite3_bind_null(stmt, index)
        }
    }

    /// Inserts one `accounts` row and one `mailboxes` row (id "<accountId>:INBOX"),
    /// idempotent via `INSERT OR IGNORE` so a suite that seeds several
    /// messages under the same account can call this once per account.
    static func seedAccountAndMailbox(
        profileDirectory: URL, accountId: String, email: String, mailboxId: String,
        role: String? = nil, path: String = "INBOX"
    ) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                "INSERT OR IGNORE INTO accounts (id, email, provider) VALUES (?1, ?2, 'imap')"
            ) { stmt in
                bindText(stmt, 1, accountId)
                bindText(stmt, 2, email)
            }
            try run(
                db,
                """
                INSERT OR IGNORE INTO mailboxes (id, account_id, path, name, role)
                VALUES (?1, ?2, ?4, ?4, ?3)
                """
            ) { stmt in
                bindText(stmt, 1, mailboxId)
                bindText(stmt, 2, accountId)
                bindText(stmt, 3, role)
                bindText(stmt, 4, path)
            }
        }
    }

    /// S59: inserts one undelivered `notification_outbox` row for `messageId`
    /// (the core normally writes these at the end of a sync pass).
    static func seedOutbox(profileDirectory: URL, messageId: String, accountId: String) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                """
                INSERT INTO notification_outbox (account_id, message_id, created_at)
                VALUES (?1, ?2, ?3)
                """
            ) { stmt in
                bindText(stmt, 1, accountId)
                bindText(stmt, 2, messageId)
                sqlite3_bind_int64(stmt, 3, Int64(Date().timeIntervalSince1970))
            }
        }
    }

    /// Inserts one `messages` row with the given List-Unsubscribe fields
    /// (nil / false = the header was absent, matching fixture (c) in the
    /// S66 plan). `accountId`/`mailboxId` must already exist (call
    /// `seedAccountAndMailbox` first).
    static func seedMessage(
        profileDirectory: URL, id: String, accountId: String, mailboxId: String,
        subject: String, fromAddress: String,
        listUnsubscribeMailto: String?, listUnsubscribeUrl: String?,
        listUnsubscribeOneClick: Bool,
        // S64: MessageListPagingTests seeds 1,200 rows that must sort into a
        // stable, distinct newest-first order for keyset paging — the
        // original one-arg-per-call default (`Date().timeIntervalSince1970`,
        // same instant for every row in a tight loop) can't provide that.
        sentAt: Int64? = nil
    ) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                """
                INSERT INTO messages (
                    id, account_id, mailbox_id, subject, from_name, from_address,
                    sent_at, snippet, list_unsubscribe_mailto, list_unsubscribe_url,
                    list_unsubscribe_one_click
                ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10)
                """
            ) { stmt in
                bindText(stmt, 1, id)
                bindText(stmt, 2, accountId)
                bindText(stmt, 3, mailboxId)
                bindText(stmt, 4, subject)
                bindText(stmt, 5, fromAddress)
                sqlite3_bind_int64(stmt, 6, sentAt ?? Int64(Date().timeIntervalSince1970))
                bindText(stmt, 7, subject)
                bindText(stmt, 8, listUnsubscribeMailto)
                bindText(stmt, 9, listUnsubscribeUrl)
                sqlite3_bind_int(stmt, 10, listUnsubscribeOneClick ? 1 : 0)
            }
        }
    }
    /// DR1: a `message_addresses` row (`kind` = from|to|cc|bcc) for a seeded message.
    static func seedAddress(
        profileDirectory: URL, messageId: String, kind: String, name: String?,
        address: String, ordinal: Int32 = 0
    ) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                """
                INSERT INTO message_addresses (message_id, kind, name, address, ordinal)
                VALUES (?1, ?2, ?3, ?4, ?5)
                """
            ) { stmt in
                bindText(stmt, 1, messageId)
                bindText(stmt, 2, kind)
                bindText(stmt, 3, name)
                bindText(stmt, 4, address)
                sqlite3_bind_int(stmt, 5, ordinal)
            }
        }
    }

    /// FP43: a server-origin keyword row (what a sync records) on a seeded message.
    static func seedServerKeyword(
        profileDirectory: URL, messageId: String, keyword: String
    ) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                """
                INSERT INTO message_keywords (message_id, keyword_key, keyword, origin)
                VALUES (?1, lower(?2), ?2, 'server')
                """
            ) { stmt in
                bindText(stmt, 1, messageId)
                bindText(stmt, 2, keyword)
            }
        }
    }

    /// DR1: gives a seeded message the IMAP uid a server-saved draft has.
    static func setProviderUid(profileDirectory: URL, messageId: String, uid: Int64) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(db, "UPDATE messages SET provider_uid = ?2 WHERE id = ?1") { stmt in
                bindText(stmt, 1, messageId)
                sqlite3_bind_int64(stmt, 2, uid)
            }
        }
    }

    /// DR1: marks a local draft as mirrored to the server Drafts mailbox at
    /// `path`/`uid` (what a successful server save records).
    static func markDraftServerSaved(
        profileDirectory: URL, draftId: Int64, path: String, uid: Int64
    ) throws {
        try withConnection(profileDirectory: profileDirectory) { db in
            try run(
                db,
                "UPDATE drafts SET server_mailbox_path = ?2, server_uid = ?3 WHERE id = ?1"
            ) { stmt in
                sqlite3_bind_int64(stmt, 1, draftId)
                bindText(stmt, 2, path)
                sqlite3_bind_int64(stmt, 3, uid)
            }
        }
    }
}
