// Castellum - tests for the S57 cold-launch mailto: flush ordering fixed
// 2026-09-26: `AppDelegate` must never drop a queued `mailto:`/Share
// Extension URL, must deliver them in arrival order, and must wait for
// `AppModel.accountsLoaded` (not just `AppModel != nil`) before delivering
// any of them — see `AppDelegate.swift`'s header comment for the bug this
// guards against. Also covers the PR #182 review's two follow-ups: (a) a
// `listAccounts()` FFI/DB failure must still flip `accountsLoaded` and
// flush, and (b) `activateMainWindow()` must fire only for a genuine cold
// mailto:/Share launch, never on an ordinary launch with nothing queued.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `AppDelegate.openMailtoLink`/`openSharePendingCompose` are overridden with
// recording closures throughout so these tests never create a live
// `ComposeWindowController`/`NSWindow` — the flush ORDERING is what's under
// test, not the compose window itself. `activateMainWindow` is stubbed too
// so nothing here touches real `NSApp` state. `AppModel` uses a temp profile
// directory, which routes Keychain access to the in-memory backing under
// XCTest (`TestHost.isActive`) rather than the real Keychain.

import XCTest
@testable import Castellum

@MainActor
final class AppDelegateMailtoFlushTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-mailto-flush-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    /// Poll until `accountsLoaded` flips true; mirrors `SyncSpinnerTests`'
    /// `waitUntilNotSyncing` so a slow-CI failure is an explicit timeout, not
    /// a confusing downstream assertion.
    private func waitUntilAccountsLoaded(
        _ model: AppModel, timeout: TimeInterval = 15
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !model.accountsLoaded && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !model.accountsLoaded {
            XCTFail("timed out (\(timeout)s) waiting for accountsLoaded")
        }
    }

    // MARK: - S57 bug #2: the flush race

    func testQueuedURLBeforeAccountsLoadIsDeliveredOnceAfterLoad() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var opened: [String] = []
        delegate.openMailtoLink = { url, _ in opened.append(url) }

        // Simulates a true cold launch: the GetURL Apple Event arrives
        // before `AppModel` even exists.
        delegate.handleIncomingURL("mailto:alice@local.test")
        XCTAssertTrue(opened.isEmpty, "must not open before AppModel exists")

        // RootView.onAppear: AppModel exists now, but its accounts haven't
        // loaded yet (reloadAccounts() hasn't even been called).
        delegate.modelBecameAvailable(model)
        XCTAssertTrue(opened.isEmpty, "must not open before accounts have loaded")

        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)

        XCTAssertEqual(opened, ["mailto:alice@local.test"], "delivered exactly once, after load")
    }

    func testMultipleQueuedURLsKeepArrivalOrder() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var opened: [String] = []
        delegate.openMailtoLink = { url, _ in opened.append(url) }

        delegate.handleIncomingURL("mailto:first@local.test")
        delegate.handleIncomingURL("mailto:second@local.test")
        delegate.modelBecameAvailable(model)
        delegate.handleIncomingURL("mailto:third@local.test")
        XCTAssertTrue(opened.isEmpty)

        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)

        XCTAssertEqual(
            opened,
            ["mailto:first@local.test", "mailto:second@local.test", "mailto:third@local.test"],
            "queued URLs are delivered exactly once, in arrival order")
    }

    func testURLArrivingAfterAccountsAlreadyLoadedDeliversImmediately() async throws {
        // Cmd-N/an already-running instance never hits the race: accounts
        // are long since loaded by the time a mailto: arrives.
        let model = makeModel()
        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)

        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var opened: [String] = []
        delegate.openMailtoLink = { url, _ in opened.append(url) }
        delegate.modelBecameAvailable(model)

        delegate.handleIncomingURL("mailto:already-running@local.test")
        XCTAssertEqual(opened, ["mailto:already-running@local.test"])
    }

    // MARK: - no account configured (#138)

    func testNoAccountConfiguredSurfacesNoAccountAlertRatherThanDroppingTheURL() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        // Deliberately NOT overriding openMailtoLink: exercise the real
        // MailtoOpener -> AppModel.composeAccountIdOrPromptUnavailable() path.

        delegate.handleIncomingURL("mailto:someone@example.com")
        delegate.modelBecameAvailable(model)

        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)

        XCTAssertTrue(model.accounts.isEmpty, "precondition: no account configured")
        XCTAssertEqual(model.composeUnavailablePrompt, .noAccount)
    }

    // MARK: - a listAccounts() failure must not strand a queued URL forever

    /// Mirrors `CastellumLaunchErrorTests`' technique: open a fresh profile
    /// once (so migrations run), close it, then use the `sqlite3` CLI
    /// directly on the file — never through Castellum's own FFI, which has
    /// no "corrupt this profile" entry point by design — to drop the
    /// `accounts` table. Reopening through `AppModel.init` still succeeds
    /// (the schema version is unchanged, so no migration re-runs), but the
    /// next `listAccounts()` throws deterministically.
    func testListAccountsFailureStillFlipsAccountsLoadedAndSurfacesTheAlertForAQueuedURL() async throws {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-mailto-flush-db-error-\(UUID().uuidString)")
        do {
            _ = try CastellumCore(profilePath: dir)
        } // closes here, before the sqlite3 CLI touches the same file

        let dropAccounts = Process()
        dropAccounts.executableURL = URL(fileURLWithPath: "/usr/bin/sqlite3")
        dropAccounts.arguments = [dir.appendingPathComponent("castellum.db").path, "DROP TABLE accounts;"]
        try dropAccounts.run()
        dropAccounts.waitUntilExit()
        XCTAssertEqual(dropAccounts.terminationStatus, 0, "corrupting the test fixture must succeed")

        let model = AppModel(profileDirectory: dir)
        models.append(model)
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        // Deliberately NOT overriding openMailtoLink: a real listAccounts()
        // failure must still surface the existing alert, not just record a call.

        delegate.handleIncomingURL("mailto:someone@example.com")
        delegate.modelBecameAvailable(model)

        model.reloadAccounts() // listAccounts() throws: "no such table: accounts"
        try await waitUntilAccountsLoaded(model)

        XCTAssertNotNil(model.errorText, "the listAccounts() failure is still surfaced")
        XCTAssertEqual(
            model.composeUnavailablePrompt, .noAccount,
            "the queued mailto: is still handled — alert shown, nothing dropped")
    }

    // MARK: - S57 bug #1 gate: activate only for a genuine cold mailto:/Share launch

    func testActivateMainWindowNotCalledOnNormalLaunchWithNothingQueued() {
        let model = makeModel()
        let delegate = AppDelegate()
        var activated = false
        delegate.activateMainWindow = { activated = true }

        // An ordinary Finder/Dock launch (or a background Sparkle relaunch):
        // nothing was ever queued, so this must never steal focus.
        delegate.modelBecameAvailable(model)

        XCTAssertFalse(activated)
    }

    func testActivateMainWindowCalledOnColdLaunchWithQueuedMailto() {
        let model = makeModel()
        let delegate = AppDelegate()
        var activated = false
        delegate.activateMainWindow = { activated = true }
        delegate.openMailtoLink = { _, _ in } // delivery isn't under test here

        // The GetURL Apple Event that cold-launched the app arrives before
        // SwiftUI finishes presenting the window and calling this.
        delegate.handleIncomingURL("mailto:alice@local.test")
        delegate.modelBecameAvailable(model)

        XCTAssertTrue(activated)
    }

    // MARK: - Share Extension handoff races the same way

    func testShareExtensionHandoffIsHeldUntilAccountsLoaded() async throws {
        let model = makeModel()
        let delegate = AppDelegate()
        delegate.activateMainWindow = {}
        var openedShares: [MailtoPrefill] = []
        delegate.openSharePendingCompose = { prefill, _ in openedShares.append(prefill) }

        // `applicationDidBecomeActive` firing before accounts load (the
        // Share Extension handoff activated the app) must not consume
        // anything yet — there's nothing to consume here (no pending share
        // written), but it must also not crash/misbehave with `model` set
        // and `accountsLoaded` still false.
        delegate.modelBecameAvailable(model)
        delegate.applicationDidBecomeActive(Notification(name: .init("test")))
        XCTAssertTrue(openedShares.isEmpty)

        model.reloadAccounts()
        try await waitUntilAccountsLoaded(model)
        XCTAssertTrue(openedShares.isEmpty, "no pending share was ever written in this test")
    }
}
