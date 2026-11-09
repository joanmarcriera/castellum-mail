// Castellum - the sidebar sync line: pure selection logic.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SyncStatusLineTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_800_000_000)
    private let target = AccountSyncTarget(configJSON: """
        {"account_id":"a","email":"me@x.test","host":"imap.x.test","port":993,"username":"me","password":"p","use_tls":true,"allow_invalid_certs":false}
        """)

    private func progress(done: Bool = false) -> SyncProgress {
        SyncProgress(
            accountId: "a", mailboxesCompleted: 0, mailboxesTotal: 1,
            currentMailbox: SyncMailboxProgress(path: "INBOX", fetched: 1240, total: 78000),
            aggregateFetched: 1240, aggregateTotal: 78000, done: done)
    }

    private func make(
        _ status: AccountSyncStatus?, progress: SyncProgress? = nil, idle: Bool = false,
        checked: TimeInterval? = nil
    ) -> SyncStatusLine {
        SyncStatusLineBuilder.make(
            status: status, progress: progress, idleHealthy: idle,
            lastChecked: checked.map { now.addingTimeInterval(-$0) }, now: now)
    }

    func testHealthyIdleReadsLive() throws {
        let t = try XCTUnwrap(target)
        let line = make(.ok(t), idle: true, checked: 400)
        XCTAssertEqual(line.kind, .live)
    }

    func testPollingFallbackShowsHowLongAgo() throws {
        let t = try XCTUnwrap(target)
        let line = make(.ok(t), idle: false, checked: 8)
        XCTAssertEqual(line.kind, .checked)
        XCTAssertTrue(line.text.contains("8 s"), line.text)
    }

    func testSyncingShowsTheMailboxProgress() throws {
        let t = try XCTUnwrap(target)
        let line = make(.syncing(t), progress: progress(), idle: true)
        XCTAssertEqual(line.kind, .syncing, "syncing outranks Live")
        XCTAssertTrue(line.text.contains("INBOX"), line.text)
        XCTAssertTrue(line.text.contains(NumberFormatter.localizedString(from: 78000, number: .decimal)), line.text)
    }

    func testSyncingWithoutASnapshotYet() throws {
        let t = try XCTUnwrap(target)
        XCTAssertEqual(make(.syncing(t)).kind, .syncing)
        XCTAssertEqual(make(.syncing(t), progress: progress(done: true)).kind, .syncing)
    }

    func testFailureIsTheRedErrorLineAndBeatsLive() throws {
        let t = try XCTUnwrap(target)
        let failed = AccountSyncStatus.failure(target: t, errorKind: "auth", rawError: nil)
        let line = make(failed, idle: true, checked: 5)
        XCTAssertEqual(line.kind, .error)
        XCTAssertEqual(line.text, failed.message)
    }

    func testNeverCheckedAndNoStatus() {
        XCTAssertEqual(make(nil).kind, .unknown)
    }

    func testAgeUnitsAndClockSkew() {
        let t = now
        XCTAssertEqual(SyncStatusLineBuilder.age(from: t.addingTimeInterval(-8), to: t), "8 s")
        XCTAssertEqual(SyncStatusLineBuilder.age(from: t.addingTimeInterval(-180), to: t), "3 min")
        XCTAssertEqual(SyncStatusLineBuilder.age(from: t.addingTimeInterval(-7200), to: t), "2 h")
        XCTAssertEqual(SyncStatusLineBuilder.age(from: t.addingTimeInterval(-3 * 86_400), to: t), "3 d")
        XCTAssertEqual(SyncStatusLineBuilder.age(from: t.addingTimeInterval(60), to: t), "0 s")
    }

    func testCoreTimestampParsing() {
        let date = SyncStatusLineBuilder.parseCoreTimestamp("2026-10-16 20:47:02")
        XCTAssertEqual(date, Date(timeIntervalSince1970: 1_792_183_622))
        XCTAssertNil(SyncStatusLineBuilder.parseCoreTimestamp("yesterday"))
        XCTAssertNil(SyncStatusLineBuilder.parseCoreTimestamp(nil))
    }

    func testTheRedrawTickIsSlowEnoughToCostNothing() {
        XCTAssertGreaterThanOrEqual(SyncStatusLineBuilder.redrawInterval, 5)
    }
}
