// Castellum - AppModel.checkPendingMigrations against a real temp profile
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Exercises the REAL FFI-backed probe end to end, the same way
// `CastellumLaunchErrorTests` exercises the real migrating open — always
// against a disposable temp directory, never the real profile or Keychain.

import XCTest
@testable import Castellum

@MainActor
final class AppModelPendingMigrationsTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-pending-migrations-\(UUID().uuidString)")
    }

    /// A profile that doesn't exist yet has every migration technically
    /// "pending" against an empty schema — but `checkPendingMigrations`
    /// must report `.upToDate`, NOT `.pending`, so the launch preflight
    /// takes the old synchronous no-window path for a first-ever launch
    /// (and, incidentally, for the XCTest host and the S64 perf probe,
    /// which always point at a fresh temp profile too). There's nothing to
    /// protect on a brand-new profile, and `preflightProfileOpen`
    /// creating+migrating it from empty is fast enough to need no window.
    func testAFreshNonexistentProfileReportsUpToDateNotPending() {
        let dir = tempProfile()
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.path))

        XCTAssertEqual(
            AppModel.checkPendingMigrations(at: dir), .upToDate,
            "a profile that has never been opened must take the no-window path")
        XCTAssertFalse(
            FileManager.default.fileExists(atPath: dir.path),
            "the probe must never create the profile directory"
        )
    }

    /// Once fully migrated (the real, migrating open), the same directory
    /// reports `.upToDate`.
    func testAFullyMigratedProfileReportsUpToDate() throws {
        let dir = tempProfile()
        _ = try CastellumCore(profilePath: dir)

        XCTAssertEqual(AppModel.checkPendingMigrations(at: dir), .upToDate)

        try? FileManager.default.removeItem(at: dir)
    }

    /// An EXISTING profile with real migrations pending (a v37 fixture —
    /// migrations 38–42 reverted in place, same SQL as the Rust
    /// `a_heavy_migration_refuses_early_when_free_space_is_insufficient`
    /// fixture; every id >= 38 must go because the runner's watermark is
    /// MAX(id)) reports `.pending`, unlike the fresh-nonexistent case above
    /// — this is exactly the distinction `applied_version > 0` exists to
    /// draw.
    func testAnExistingProfileWithAPendingMigrationReportsPending() throws {
        let dir = tempProfile()
        let dbPath = dir.appendingPathComponent("castellum.db").path
        _ = try CastellumCore(profilePath: dir)

        let sqlite3CLI = Process()
        sqlite3CLI.executableURL = URL(fileURLWithPath: "/usr/bin/sqlite3")
        sqlite3CLI.arguments = [
            dbPath,
            """
            DROP TRIGGER messages_before_delete_fts;
            DROP TABLE message_fts_rowid;
            DROP TABLE message_fts;
            CREATE VIRTUAL TABLE message_fts USING fts5(
                message_id UNINDEXED, subject, sender, body, tokenize = 'unicode61');
            CREATE TRIGGER messages_after_delete_fts AFTER DELETE ON messages BEGIN
                DELETE FROM message_fts WHERE message_id = OLD.id;
            END;
            DROP TABLE thread_dirty_messages;
            ALTER TABLE thread_rebuild_state DROP COLUMN force_full;
            ALTER TABLE sync_cursors DROP COLUMN descriptor_checkpoint_uid;
            DROP TABLE message_decode_skips;
            ALTER TABLE smart_mailboxes DROP COLUMN group_json;
            ALTER TABLE smart_mailboxes DROP COLUMN scope_json;
            ALTER TABLE saved_filters DROP COLUMN sort_order;
            ALTER TABLE saved_filters DROP COLUMN stop_processing;
            ALTER TABLE saved_filters DROP COLUMN enabled;
            ALTER TABLE account_identities DROP COLUMN reply_to;
            ALTER TABLE account_identities DROP COLUMN bcc;
            ALTER TABLE account_identities DROP COLUMN is_default;
            DROP TABLE tag_definitions;
            DROP TABLE message_keywords;
            DROP TABLE thread_mutes;
            ALTER TABLE threads DROP COLUMN is_muted;
            DROP TABLE suggestions;
            DROP TABLE saved_views;
            DROP TABLE notification_outbox;
            DROP TABLE vip_senders;
            DROP TABLE project_timeline;
            DROP TABLE work_item_provenance;
            DROP INDEX idx_messages_account_rfc822;
            DROP INDEX idx_ai_jobs_embed_msg;
            ALTER TABLE calendar_invites DROP COLUMN location;
            ALTER TABLE calendar_invites DROP COLUMN description;
               ALTER TABLE calendar_invites DROP COLUMN recurrence_id_raw;
               ALTER TABLE calendar_invites DROP COLUMN recurrence_id_tzid;
               ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_utc;
               ALTER TABLE calendar_invites DROP COLUMN recurrence_id_is_date_only;
            DROP TABLE pending_refetch;
            DROP TABLE pending_rules;
            DROP TABLE message_header_capture;
            DROP TABLE message_headers;
            DROP TABLE header_names;
            DELETE FROM _migrations WHERE id IN (38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57);
            """,
        ]
        try sqlite3CLI.run()
        sqlite3CLI.waitUntilExit()
        XCTAssertEqual(sqlite3CLI.terminationStatus, 0, "stamping the v37 fixture must succeed")

        guard case .pending(let count, let heavy) = AppModel.checkPendingMigrations(at: dir) else {
            return XCTFail("expected .pending for an existing v37 profile with 38..57 outstanding")
        }
        XCTAssertEqual(count, 20, "38 through 57 are pending on the day this was written")
        XCTAssertTrue(heavy, "migration 38 is HEAVY")

        try? FileManager.default.removeItem(at: dir)
    }

    /// A profile stamped past this build's known migrations classifies as
    /// `.launchBlocking(.schemaTooNew)` from the cheap probe alone — the
    /// same real-launch-path fixture `CastellumLaunchErrorTests` uses for
    /// `preflightProfileOpen` itself.
    func testAProfileStampedNewerThanThisBuildKnowsIsLaunchBlocking() throws {
        let dir = tempProfile()
        let dbPath = dir.appendingPathComponent("castellum.db").path

        let futureVersion: Int64
        do {
            let core = try CastellumCore(profilePath: dir)
            futureVersion = try core.schemaVersion() + 1
        }

        let sqlite3CLI = Process()
        sqlite3CLI.executableURL = URL(fileURLWithPath: "/usr/bin/sqlite3")
        sqlite3CLI.arguments = [
            dbPath,
            "INSERT INTO _migrations (id, name) VALUES (\(futureVersion), 'from_swift_test');",
        ]
        try sqlite3CLI.run()
        sqlite3CLI.waitUntilExit()
        XCTAssertEqual(sqlite3CLI.terminationStatus, 0, "stamping the test fixture must succeed")

        guard case .launchBlocking(let launchError) = AppModel.checkPendingMigrations(at: dir) else {
            return XCTFail("expected .launchBlocking for a schema newer than this build supports")
        }
        guard case .schemaTooNew(let message) = launchError else {
            return XCTFail("expected .schemaTooNew, got \(launchError)")
        }
        XCTAssertTrue(message.contains("\(futureVersion)"))

        try? FileManager.default.removeItem(at: dir)
    }
}
