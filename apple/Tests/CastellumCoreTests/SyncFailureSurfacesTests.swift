// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import AppKit
import XCTest
@testable import Castellum

/// F4 (review pack U1 D1-D3): a failed sync must show on the Status dashboard
/// row and the Settings accounts row; the Signatures window has a minimum size.
@MainActor
final class SyncFailureSurfacesTests: XCTestCase {
    private let target = AccountSyncTarget(configJSON: """
        {"account_id":"a1","email":"person@example.test","host":"127.0.0.1","port":3999,
         "username":"person@example.test","password":"x","use_tls":false}
        """)!
    private let account = Account(id: "a1", email: "person@example.test", displayName: nil, provider: "imap")

    private func diagnostics(failures: Int64, lastSuccess: String?) -> LocalAccountDiagnostics {
        LocalAccountDiagnostics(
            accountId: "a1", email: "person@example.test", provider: "imap",
            lastAttemptAt: "t1", lastSuccessAt: lastSuccess, lastErrorKind: nil,
            lastError: nil, lastErrorPhase: nil, consecutiveFailures: failures, mailboxes: [])
    }

    // D3: the dashboard row follows the CURRENT status; a failure carries the banner's message.
    func testDashboardRowShowsFailureMessageAndOffersRetry() {
        let failed = AccountSyncStatus.failure(target: target, errorKind: "network", rawError: nil)
        let row = StatusAccountRow.make(
            account: account, config: nil, status: failed,
            diagnostic: diagnostics(failures: 0, lastSuccess: "t0"))
        XCTAssertEqual(row.state, .unreachable)
        XCTAssertEqual(row.failureMessage, failed.message)
        XCTAssertTrue(row.accessibilityText.contains("Can’t reach 127.0.0.1:3999"))
        XCTAssertTrue(failed.offersRetry, "failure rows offer Retry, not a spinner")
    }

    func testDashboardRowWhileSyncingHasNoFailureMessage() {
        let row = StatusAccountRow.make(
            account: account, config: nil, status: .syncing(target), diagnostic: nil)
        XCTAssertEqual(row.state, .syncing)
        XCTAssertNil(row.failureMessage)
    }

    // RP06: a just-synced account whose diagnostics have not loaded yet is not "unknown".
    func testOkStatusWithoutDiagnosticsIsNotUnknown() {
        XCTAssertEqual(AccountRowSummary.text(status: .ok(target), diagnostics: nil), "Last synced: just now")
        XCTAssertEqual(AccountRowSummary.text(status: nil, diagnostics: nil), "Last sync: unknown")
    }

    // D2: the Settings row shows the current failure, not "Last synced".
    func testSettingsRowShowsFailureOverLastSynced() {
        let failed = AccountSyncStatus.failure(target: target, errorKind: "auth", rawError: nil)
        let text = AccountRowSummary.text(
            status: failed, diagnostics: diagnostics(failures: 0, lastSuccess: "2026-10-01 10:00"))
        XCTAssertEqual(text, failed.message)
        XCTAssertFalse(text.contains("Last synced"))
        XCTAssertEqual(
            AccountRowSummary.text(status: .ok(target), diagnostics: diagnostics(failures: 0, lastSuccess: "T")),
            "Last synced: T")
    }

    // D1: minimum content size declared; an undersized frame comes back at the minimum.
    func testSignaturesWindowEnforcesMinimumContentSize() {
        let min = SignatureEditorWindowController.minimumContentSize
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100),
            styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: true)
        window.contentMinSize = .zero
        SignatureEditorWindowController.enforceMinimum(on: window)
        XCTAssertEqual(window.contentMinSize, min)
        let size = window.contentRect(forFrameRect: window.frame).size
        XCTAssertGreaterThanOrEqual(size.width, min.width)
        XCTAssertGreaterThanOrEqual(size.height, min.height)
    }
}
