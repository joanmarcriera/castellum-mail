// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S62 first-sync scope: history window dates, folder-filter policy, and the
/// config round trip the Rust core reads (`mailboxes`, `initial_sync_since`).
final class AccountSyncScopeTests: XCTestCase {
    private var utc: Calendar {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "UTC")!
        return c
    }

    private func date(_ y: Int, _ m: Int, _ d: Int) -> Date {
        utc.date(from: DateComponents(year: y, month: m, day: d, hour: 12))!
    }

    func testHistoryWindowsProduceCoreDates() {
        let now = date(2026, 9, 23)
        XCTAssertEqual(InitialHistory.oneMonth.sinceDate(now: now, calendar: utc), "2026-08-23")
        XCTAssertEqual(InitialHistory.threeMonths.sinceDate(now: now, calendar: utc), "2026-06-23")
        XCTAssertEqual(InitialHistory.twelveMonths.sinceDate(now: now, calendar: utc), "2025-09-23")
        XCTAssertNil(InitialHistory.all.sinceDate(now: now, calendar: utc))
    }

    func testMonthEndClampsInsteadOfOverflowing() {
        XCTAssertEqual(InitialHistory.oneMonth.sinceDate(now: date(2026, 3, 31), calendar: utc), "2026-02-28")
    }

    private func mb(_ path: String, selectable: Bool = true, byDefault: Bool) -> ConnectionTestMailbox {
        ConnectionTestMailbox(path: path, name: path, role: nil, selectable: selectable, syncByDefault: byDefault)
    }

    private var server: [ConnectionTestMailbox] {
        [mb("INBOX", byDefault: true), mb("[Gmail]", selectable: false, byDefault: false),
         mb("[Gmail]/All Mail", byDefault: false), mb("Sent", byDefault: true), mb("Old", byDefault: false)]
    }

    func testDefaultSelectionIsTheCoresDefaultSet() {
        XCTAssertEqual(SyncScope.defaultSelection(server), ["INBOX", "Sent"])
    }

    func testLeavingTheDefaultsStoresNoFilter() {
        XCTAssertNil(SyncScope.mailboxFilter(selected: ["INBOX", "Sent"], mailboxes: server),
                     "defaults must stay a live policy, not freeze into a list")
    }

    func testDeviatingStoresAnExplicitListInServerOrder() {
        XCTAssertEqual(SyncScope.mailboxFilter(selected: ["Old", "INBOX"], mailboxes: server), ["INBOX", "Old"])
        XCTAssertEqual(SyncScope.mailboxFilter(selected: ["INBOX"], mailboxes: server), ["INBOX"])
    }

    func testNonSelectableAndUnknownPathsAreNeverStored() {
        XCTAssertEqual(
            SyncScope.mailboxFilter(selected: ["INBOX", "[Gmail]", "Gone", "[Gmail]/All Mail"], mailboxes: server),
            ["INBOX", "[Gmail]/All Mail"])
    }

    func testPickerShowsDefaultsFirstAndHidesContainers() {
        XCTAssertEqual(SyncScope.orderedForPicker(server).map(\.path),
                       ["INBOX", "Sent", "[Gmail]/All Mail", "Old"])
    }

    private func config() -> AccountConfig {
        AccountConfig(accountId: "a", email: "a@x", host: "imap.x", port: 993, username: "a@x",
                      password: "p", useTls: true, allowInvalidCerts: nil, smtpHost: nil, smtpPort: nil,
                      smtpUsername: nil, smtpPassword: nil, smtpUseTls: nil, displayName: nil)
    }

    func testScopeEncodesWithTheKeysTheCoreReads() throws {
        var c = config()
        c.mailboxes = ["INBOX", "Old"]
        c.initialSyncSince = "2026-06-23"
        let dict = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(c.encodedJSON().utf8)) as? [String: Any])
        XCTAssertEqual(dict["mailboxes"] as? [String], ["INBOX", "Old"])
        XCTAssertEqual(dict["initial_sync_since"] as? String, "2026-06-23")
        XCTAssertEqual(try AccountConfig.decode(fromJSON: c.encodedJSON()), c)
    }

    func testNoScopeOmitsBothKeysAndOldConfigsStillDecode() throws {
        let json = try config().encodedJSON()
        XCTAssertFalse(json.contains("mailboxes"))
        XCTAssertFalse(json.contains("initial_sync_since"))
        let legacy = #"{"account_id":"a","email":"a@x","host":"h","port":993,"username":"u","password":"p","use_tls":true}"#
        let decoded = try AccountConfig.decode(fromJSON: legacy)
        XCTAssertNil(decoded.mailboxes)
        XCTAssertNil(decoded.initialSyncSince)
    }

    func testConnectionSettingsIgnoreScope() {
        var scoped = config()
        scoped.mailboxes = ["INBOX"]
        scoped.initialSyncSince = "2026-01-01"
        XCTAssertEqual(scoped.connectionSettings, config().connectionSettings)
        var otherHost = config()
        otherHost.host = "imap.y"
        XCTAssertNotEqual(otherHost.connectionSettings, config().connectionSettings)
    }

    // MARK: - Editing an EXISTING account's folders

    func testCurrentSelectionIsTheStoredExplicitListWhenPresent() {
        XCTAssertEqual(SyncScope.currentSelection(stored: ["INBOX", "Old"], mailboxes: server), ["INBOX", "Old"])
    }

    func testCurrentSelectionFallsBackToTheLiveDefaultsWhenNil() {
        XCTAssertEqual(SyncScope.currentSelection(stored: nil, mailboxes: server), ["INBOX", "Sent"])
    }

    func testCurrentSelectionRoundTripsThroughMailboxFilter() {
        // Re-opening the sheet without touching anything must reproduce
        // exactly what would be (re-)stored — nil stays nil, an explicit list
        // stays that list.
        for stored in [nil, ["INBOX"], ["INBOX", "Old"]] as [[String]?] {
            let selection = SyncScope.currentSelection(stored: stored, mailboxes: server)
            XCTAssertEqual(SyncScope.mailboxFilter(selected: selection, mailboxes: server), stored)
        }
    }

    func testResolvedKeepPathsIsTheDefaultSetWhenSelectionMatchesDefaults() {
        XCTAssertEqual(SyncScope.resolvedKeepPaths(selected: ["INBOX", "Sent"], mailboxes: server), ["INBOX", "Sent"])
    }

    func testResolvedKeepPathsIsTheExplicitSelectionOtherwise() {
        XCTAssertEqual(SyncScope.resolvedKeepPaths(selected: ["INBOX"], mailboxes: server), ["INBOX"])
        XCTAssertEqual(
            SyncScope.resolvedKeepPaths(selected: ["Old", "INBOX"], mailboxes: server), ["INBOX", "Old"])
    }

    func testResolvedKeepPathsNeverIncludesNonSelectableOrUnknownPaths() {
        XCTAssertEqual(
            SyncScope.resolvedKeepPaths(selected: ["INBOX", "[Gmail]", "Gone"], mailboxes: server), ["INBOX"])
    }

    func testApplyFolderEditChangesOnlyMailboxesAndInitialSyncSince() {
        var original = config()
        original.mailboxes = ["INBOX"]
        original.initialSyncSince = "2020-01-01"
        original.displayName = "Kept"

        let updated = SyncScope.applyFolderEdit(
            to: original, selected: ["INBOX", "Sent", "Old"], mailboxes: server, initialSyncSince: "2026-06-23")

        XCTAssertEqual(updated.mailboxes, ["INBOX", "Sent", "Old"])
        XCTAssertEqual(updated.initialSyncSince, "2026-06-23")
        XCTAssertEqual(updated.connectionSettings, original.connectionSettings, "no credential/connection drift")
        XCTAssertEqual(updated.displayName, "Kept")
    }

    func testApplyFolderEditLeavingDefaultsTickedStoresNoFilter() {
        var original = config()
        original.mailboxes = ["INBOX", "Old"] // a prior deviation
        let updated = SyncScope.applyFolderEdit(
            to: original, selected: ["INBOX", "Sent"], mailboxes: server, initialSyncSince: nil)
        XCTAssertNil(updated.mailboxes, "returning to the defaults must drop the explicit list again")
    }

    func testApplyHistoryOnlyEditNeverTouchesMailboxesOrCredentials() {
        var original = config()
        original.mailboxes = ["INBOX"]
        original.password = "secret"
        let updated = SyncScope.applyHistoryOnlyEdit(to: original, initialSyncSince: "2026-01-01")
        XCTAssertEqual(updated.mailboxes, ["INBOX"], "unreachable-server path must not touch the folder filter")
        XCTAssertEqual(updated.initialSyncSince, "2026-01-01")
        XCTAssertEqual(updated.password, "secret")
        XCTAssertEqual(updated.connectionSettings, original.connectionSettings)
    }

    // MARK: - follow-up review: a renamed/removed stored folder

    func testStaleStoredPathsFindsPathsMissingFromTheLiveList() {
        XCTAssertEqual(SyncScope.staleStoredPaths(stored: ["INBOX", "Renamed"], mailboxes: server), ["Renamed"])
    }

    func testStaleStoredPathsIsEmptyWhenEveryStoredPathIsStillLive() {
        XCTAssertEqual(SyncScope.staleStoredPaths(stored: ["INBOX", "Old"], mailboxes: server), [])
    }

    func testStaleStoredPathsIsEmptyForTheDefaultsMarker() {
        XCTAssertEqual(SyncScope.staleStoredPaths(stored: nil, mailboxes: server), [])
    }

    func testStaleStoredPathsMatchesCaseInsensitively() {
        XCTAssertEqual(SyncScope.staleStoredPaths(stored: ["inbox"], mailboxes: server), [])
    }
}
