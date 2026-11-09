// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-9 slice 1 preferences: counter scope keys and per-account rules persist,
/// an undecodable blob reads as defaults without being overwritten, and rules for
/// unknown account ids survive.
@MainActor
final class NotificationPreferencesTests: XCTestCase {
    private func defaults() -> UserDefaults {
        UserDefaults(suiteName: "castellum-notifprefs-\(UUID().uuidString)")!
    }

    func testDefaults() {
        let p = Preferences(defaults: defaults())
        XCTAssertEqual(p.dockBadgeMode, .inboxUnread)
        XCTAssertEqual(p.menuBarCounterMode, .off, "the menu bar item is opt-in")
        XCTAssertNil(p.dockBadgeSmartMailboxId)
        XCTAssertNil(p.menuBarSmartMailboxId)
        XCTAssertTrue(p.notificationAccountRules.isEmpty)
        XCTAssertEqual(p.dockScope, .inboxUnread)
        XCTAssertEqual(p.menuBarScope, .off)
        XCTAssertTrue(p.accountRule("any").countsInCounters)
    }

    func testRoundTrip() {
        let d = defaults()
        let p = Preferences(defaults: d)
        p.dockBadgeSmartMailboxId = "s1"
        p.dockBadgeMode = .smartMailbox
        p.menuBarSmartMailboxId = "s2"
        p.menuBarCounterMode = .smartMailbox
        p.setCountsInCounters(false, accountId: "acct-1")
        let q = Preferences(defaults: d)
        XCTAssertEqual(q.dockScope, .smartMailbox(id: "s1"))
        XCTAssertEqual(q.menuBarScope, .smartMailbox(id: "s2"))
        XCTAssertFalse(q.accountRule("acct-1").countsInCounters)
        XCTAssertTrue(q.accountRule("acct-2").countsInCounters)
    }

    func testReIncludingAnAccountDropsTheEntry() {
        let p = Preferences(defaults: defaults())
        p.setCountsInCounters(false, accountId: "a")
        XCTAssertEqual(p.notificationAccountRules.count, 1)
        p.setCountsInCounters(true, accountId: "a")
        XCTAssertTrue(p.notificationAccountRules.isEmpty)
    }

    func testUndecodableBlobReadsAsDefaultsAndIsNotOverwritten() {
        let d = defaults()
        let junk = Data("not json".utf8)
        d.set(junk, forKey: "notificationAccountRules")
        let p = Preferences(defaults: d)
        XCTAssertTrue(p.notificationAccountRules.isEmpty)
        XCTAssertEqual(d.data(forKey: "notificationAccountRules"), junk, "untouched until the user edits")
        p.setCountsInCounters(false, accountId: "a")
        XCTAssertNotEqual(d.data(forKey: "notificationAccountRules"), junk)
    }

    func testUnknownAccountIdIsKeptAndFutureFieldsAreTolerated() throws {
        let d = defaults()
        let json = #"{"deleted-account":{"countsInCounters":false,"banners":"vipsOnly"},"other":{}}"#
        d.set(Data(json.utf8), forKey: "notificationAccountRules")
        let p = Preferences(defaults: d)
        XCTAssertFalse(p.accountRule("deleted-account").countsInCounters)
        XCTAssertTrue(p.accountRule("other").countsInCounters, "a missing key defaults to counting")
        p.setCountsInCounters(false, accountId: "new")
        XCTAssertEqual(Set(p.notificationAccountRules.keys), ["deleted-account", "other", "new"])
    }

    func testUnknownModeStringFallsBack() {
        let d = defaults()
        d.set("bogus", forKey: "menuBarCounterMode")
        d.set("bogus", forKey: "dockBadgeMode")
        let p = Preferences(defaults: d)
        XCTAssertEqual(p.menuBarCounterMode, .off)
        XCTAssertEqual(p.dockBadgeMode, .inboxUnread)
    }

    func testDeletingASmartMailboxClearsBothCounterChoices() {
        let p = Preferences(defaults: defaults())
        p.dockBadgeSmartMailboxId = "s1"
        p.dockBadgeMode = .smartMailbox
        p.menuBarSmartMailboxId = "s1"
        p.menuBarCounterMode = .smartMailbox
        p.forgetSmartMailbox("other")
        XCTAssertEqual(p.dockScope, .smartMailbox(id: "s1"))
        p.forgetSmartMailbox("s1")
        XCTAssertNil(p.dockBadgeSmartMailboxId)
        XCTAssertNil(p.menuBarSmartMailboxId)
        XCTAssertEqual(p.dockScope, .inboxUnread, "an unchosen smart scope counts the inbox")
        XCTAssertEqual(p.menuBarScope, .inboxUnread)
    }

    /// An account left out of the counters stays re-includable with one account
    /// (the Settings toggle is shown for >= 1 account; this is its backing logic).
    func testExcludedAccountCanBeReIncluded() {
        let p = Preferences(defaults: defaults())
        p.setCountsInCounters(false, accountId: "only")
        XCTAssertFalse(p.accountRule("only").countsInCounters)
        p.setCountsInCounters(true, accountId: "only")
        XCTAssertTrue(p.accountRule("only").countsInCounters)
    }
}
