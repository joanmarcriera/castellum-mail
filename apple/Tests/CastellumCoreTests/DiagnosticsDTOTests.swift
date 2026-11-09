// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Proves the Swift diagnostics DTOs match the Rust core's JSON shapes
/// exactly (snake_case, optionality), and exercises the real FFI on a temp
/// profile: the bundle must decode, be read-only, and carry no identifiers.
final class DiagnosticsDTOTests: XCTestCase {
    private func decode<T: Decodable>(_ type: T.Type, _ json: String) throws -> T {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return try d.decode(T.self, from: Data(json.utf8))
    }

    func testSupportBundleDecodes() throws {
        let json = """
        {
          "bundle_schema": 1,
          "generated_at": "2026-07-10 12:00:00",
          "schema_version": 16,
          "core_version": "0.1.0",
          "integrity": {"ok": true, "issue_count": 0, "first_issue": null},
          "accounts": [{
            "label": "account-1",
            "provider": "imap",
            "capabilities": ["IMAP4rev1", "UIDPLUS"],
            "last_attempt_at": "2026-07-10 11:59:00",
            "last_success_at": null,
            "last_error_kind": "auth",
            "last_error": "authentication failed",
            "last_error_phase": "connect",
            "consecutive_failures": 3,
            "operations": {
              "queued": 1, "running": 0, "succeeded": 10,
              "failed": 2, "failed_permanent": 0, "cancelled": 0, "other": 0
            },
            "mailboxes": [{
              "label": "inbox",
              "role": "inbox",
              "last_attempt_at": null,
              "last_success_at": "2026-07-09 08:00:00",
              "last_error_kind": null,
              "last_error": null,
              "last_error_phase": null,
              "consecutive_failures": 0,
              "total_messages": 120,
              "messages_with_body": 100,
              "messages_without_body": 20,
              "has_sync_cursor": true
            }]
          }]
        }
        """
        let bundle = try decode(SupportBundle.self, json)
        XCTAssertEqual(bundle.bundleSchema, 1)
        XCTAssertEqual(bundle.schemaVersion, 16)
        XCTAssertTrue(bundle.integrity.ok)
        XCTAssertEqual(bundle.accounts.count, 1)

        let acct = bundle.accounts[0]
        XCTAssertEqual(acct.label, "account-1")
        XCTAssertEqual(acct.capabilities, ["IMAP4rev1", "UIDPLUS"])
        XCTAssertEqual(acct.lastErrorKind, "auth")
        XCTAssertEqual(acct.lastErrorPhase, "connect")
        XCTAssertEqual(acct.consecutiveFailures, 3)
        XCTAssertEqual(acct.operations.succeeded, 10)
        XCTAssertEqual(acct.operations.failedPermanent, 0)

        let mb = acct.mailboxes[0]
        XCTAssertEqual(mb.label, "inbox")
        XCTAssertEqual(mb.totalMessages, 120)
        XCTAssertEqual(mb.messagesWithoutBody, 20)
        XCTAssertTrue(mb.hasSyncCursor)
    }

    func testIntegrityReportDecodesIssueShape() throws {
        let json = """
        {"ok": false, "issue_count": 2, "first_issue": "wrong # of entries in index x"}
        """
        let report = try decode(IntegrityReport.self, json)
        XCTAssertFalse(report.ok)
        XCTAssertEqual(report.issueCount, 2)
        XCTAssertEqual(report.firstIssue, "wrong # of entries in index x")
    }

    // MARK: - LastSyncKpi ( follow-up: castellum_last_sync_kpi)

    func testLastSyncKpiDecodesTheDocumentedShape() throws {
        let json = """
        {
          "available": true,
          "account_id": "acct-1",
          "timestamp_unix": 1700000000,
          "message_count": 1234,
          "wall_ms": 5432.1,
          "phases": {
            "imap.list_mailboxes": {"count": 1, "total_ms": 12.3, "p50_ms": 12.3, "p95_ms": 12.3, "max_ms": 12.3},
            "imap.fetch_descriptors": {"count": 1, "total_ms": 8000.0, "p50_ms": 8000.0, "p95_ms": 8000.0, "max_ms": 8000.0},
            "db.upsert_descriptor": {"count": 1234, "total_ms": 180.5, "p50_ms": 0.14, "p95_ms": 0.3, "max_ms": 1.2},
            "db.rebuild_threads": {"count": 1, "total_ms": 40.0, "p50_ms": 40.0, "p95_ms": 40.0, "max_ms": 40.0}
          }
        }
        """
        let kpi = try decode(LastSyncKpi.self, json)
        XCTAssertTrue(kpi.available)
        XCTAssertEqual(kpi.accountId, "acct-1")
        XCTAssertEqual(kpi.timestampUnix, 1_700_000_000)
        XCTAssertEqual(kpi.messageCount, 1234)
        XCTAssertEqual(kpi.wallMs, 5432.1, accuracy: 0.001)
        XCTAssertEqual(kpi.phases.count, 4)
        XCTAssertEqual(kpi.phases["db.upsert_descriptor"]?.count, 1234)
        XCTAssertEqual(kpi.phases["db.upsert_descriptor"]?.p95Ms ?? -1, 0.3, accuracy: 0.001)

        // Rollup helpers the "Performance" section relies on.
        XCTAssertEqual(kpi.totalMs(forPhasePrefix: "imap."), 12.3 + 8000.0, accuracy: 0.001)
        XCTAssertEqual(kpi.totalMs(forPhasePrefix: "db."), 180.5 + 40.0, accuracy: 0.001)

        // 1234 messages / 5432.1 ms == ~227 msgs/sec.
        let rate = try XCTUnwrap(kpi.messagesPerSecond)
        XCTAssertEqual(rate, 1234.0 / (5432.1 / 1000.0), accuracy: 0.01)
    }

    /// Proves the "success but null" FFI shape decodes into Swift `nil`
    /// without a bespoke error case: `castellum_last_sync_kpi` writes the
    /// literal string "null" (not a failure) when no sync has completed yet,
    /// and `CastellumCore.lastSyncKpi()` handles that by decoding into
    /// `LastSyncKpi?` rather than `LastSyncKpi` (see its doc comment).
    func testLastSyncKpiOptionalDecodesJsonNullAsNil() throws {
        let kpi = try decode(LastSyncKpi?.self, "null")
        XCTAssertNil(kpi)
    }

    func testLastSyncKpiMessagesPerSecondIsNilForZeroWallTime() throws {
        let json = """
        {"available": true, "account_id": "a", "timestamp_unix": 0, "message_count": 0, "wall_ms": 0, "phases": {}}
        """
        let kpi = try decode(LastSyncKpi.self, json)
        XCTAssertNil(kpi.messagesPerSecond, "zero wall time must not produce a divide-by-zero/infinite rate")
    }

    // MARK: - live FFI on a temp profile

    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-diagnostics-test-\(UUID().uuidString)")
    }

    func testSupportBundleAndIntegrityCheckOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())

        let bundle = try core.supportBundle()
        XCTAssertEqual(bundle.bundleSchema, 1)
        XCTAssertGreaterThanOrEqual(bundle.schemaVersion, 16)
        XCTAssertFalse(bundle.coreVersion.isEmpty)
        XCTAssertTrue(bundle.integrity.ok)
        XCTAssertTrue(bundle.accounts.isEmpty)

        let integrity = try core.integrityCheck()
        XCTAssertTrue(integrity.ok)
        XCTAssertEqual(integrity.issueCount, 0)
        XCTAssertNil(integrity.firstIssue)
    }

    func testRedactDiagnosticTextOverFFI() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let redacted = try core.redactDiagnosticText(
            "contact alice@example.test password=SENTINEL_NOT_A_REAL_SECRET"
        )
        XCTAssertFalse(redacted.contains("alice@example.test"))
        XCTAssertFalse(redacted.contains("SENTINEL"))
    }

    /// A brand-new profile has never synced: `castellum_last_sync_kpi`
    /// returns CASTELLUM_OK with the JSON string "null", which must decode to
    /// Swift `nil` (a normal empty state), not throw.
    func testLastSyncKpiOverFFIIsNilForAFreshProfile() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        let kpi = try core.lastSyncKpi()
        XCTAssertNil(kpi)
    }

    /// `castellum_log_directory` is a direct string return (no
    /// CastellumStatus out-param) — this proves the memory ownership/nullable
    /// pointer handling in `CastellumCore.logDirectory()` round-trips through
    /// the real FFI for a valid handle without a leak or crash. The directory
    /// itself may not exist on disk yet (only the FIRST profile opened in
    /// this test-host PROCESS gets tracing initialised — see
    /// native/crates/castellum-core/src/logging.rs's Once-guard doc comment),
    /// so this asserts the returned PATH shape, not filesystem existence.
    func testLogDirectoryOverFFIReturnsALogsSubdirectoryOfTheProfile() throws {
        let profilePath = tempProfile()
        let core = try CastellumCore(profilePath: profilePath)
        let logDir = try XCTUnwrap(core.logDirectory())
        XCTAssertEqual(logDir.lastPathComponent, "logs")
        XCTAssertEqual(logDir.deletingLastPathComponent().standardizedFileURL, profilePath.standardizedFileURL)
    }
}
