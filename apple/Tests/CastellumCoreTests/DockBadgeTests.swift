// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The Dock badge (S59): the pure label and the controller that feeds it. No
/// test here can reach the real Dock tile: every controller gets a capturing
/// `setLabel`, and the default setter is a no-op under `TestHost`.
@MainActor
final class DockBadgeTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-dock-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func focus(
        allowed: Set<String>? = nil, dock: Bool = true
    ) -> FocusFilterState {
        FocusFilterState(allowedAccountIds: allowed, vipsOnly: false, showDockBadge: dock)
    }

    // MARK: pure label

    func testZeroUnreadHasNoBadge() {
        XCTAssertNil(
            DockBadge.label(
                mode: .inboxUnread, focus: .inactive, inboxUnreadByAccount: ["a": 0], vipUnread: nil))
    }

    func testCountsSumAcrossAccounts() {
        XCTAssertEqual(
            DockBadge.label(
                mode: .inboxUnread, focus: .inactive,
                inboxUnreadByAccount: ["a": 3, "b": 4], vipUnread: nil), "7")
    }

    func testFocusAllowedAccountsRestrictTheSum() {
        XCTAssertEqual(
            DockBadge.label(
                mode: .inboxUnread, focus: focus(allowed: ["b"]),
                inboxUnreadByAccount: ["a": 3, "b": 4], vipUnread: nil), "4")
    }

    func testFocusCanHideTheBadge() {
        XCTAssertNil(
            DockBadge.label(
                mode: .inboxUnread, focus: focus(dock: false),
                inboxUnreadByAccount: ["a": 3], vipUnread: nil))
    }

    func testVipModeUsesTheVipCountAndOffShowsNothing() {
        XCTAssertEqual(
            DockBadge.label(
                mode: .vipUnread, focus: .inactive, inboxUnreadByAccount: ["a": 99], vipUnread: 2), "2")
        XCTAssertNil(
            DockBadge.label(
                mode: .vipUnread, focus: .inactive, inboxUnreadByAccount: ["a": 99], vipUnread: nil))
        XCTAssertNil(
            DockBadge.label(
                mode: .off, focus: .inactive, inboxUnreadByAccount: ["a": 99], vipUnread: 2))
    }

    func testHugeCountsAreCapped() {
        XCTAssertEqual(
            DockBadge.label(
                mode: .inboxUnread, focus: .inactive,
                inboxUnreadByAccount: ["a": 12_345], vipUnread: nil), "9999+")
    }

    func testPerAccountHelperOnlyCountsTheRole() {
        let inbox = Mailbox(
            id: "a:INBOX", accountId: "a", path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        let sent = Mailbox(
            id: "a:Sent", accountId: "a", path: "Sent", name: "Sent", role: "sent",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        let counts = [
            "a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 10, unread: 3),
            "a:Sent": MailboxCounts(mailboxId: "a:Sent", total: 10, unread: 8),
        ]
        XCTAssertEqual(
            UnifiedInboxBadge.unreadByAccount(role: "inbox", mailboxes: ["a": [inbox, sent]], counts: counts),
            ["a": 3])
    }

    // MARK: controller

    private func inboxMailbox(_ account: String) -> Mailbox {
        Mailbox(
            id: "\(account):INBOX", accountId: account, path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testControllerRecomputesWhenCountsChange() throws {
        let model = makeModel()
        let defaults = UserDefaults(suiteName: "castellum-dock-\(UUID().uuidString)")!
        let prefs = Preferences(defaults: defaults)
        var labels: [String?] = []
        let controller = DockBadgeController(
            model: model, preferences: prefs, focusState: { .inactive }, debounceMilliseconds: 0,
            setLabel: { labels.append($0) })
        model.mailboxes = ["a": [inboxMailbox("a")]]
        model.counts = ["a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 5, unread: 3)]
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "3")
        model.counts = ["a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 6, unread: 5)]
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "5")
        // An unchanged badge is not re-set.
        let before = labels.count
        controller.recompute()
        XCTAssertEqual(labels.count, before)
        // Mode off clears it.
        prefs.dockBadgeMode = .off
        controller.recompute()
        XCTAssertNil(labels.last ?? "x")
    }

    func testControllerReactsToPublishedChangesWithoutAnExplicitRecompute() async throws {
        let model = makeModel()
        let prefs = Preferences(defaults: UserDefaults(suiteName: "castellum-dock-\(UUID().uuidString)")!)
        var labels: [String?] = []
        let controller = DockBadgeController(
            model: model, preferences: prefs, focusState: { .inactive }, debounceMilliseconds: 10,
            setLabel: { labels.append($0) })
        model.mailboxes = ["a": [inboxMailbox("a")]]
        model.counts = ["a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 5, unread: 2)]
        let deadline = Date().addingTimeInterval(5)
        while (labels.last ?? nil) != "2" && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertEqual(labels.last ?? nil, "2")
        withExtendedLifetime(controller) {}
    }

    /// Audit #3450: in VIP mode the badge went stale after adding or removing a VIP
    /// (no sync follows). The VIP list is now a recompute trigger.
    func testVipListChangeTriggersARecompute() async throws {
        let model = makeModel()
        let prefs = Preferences(defaults: UserDefaults(suiteName: "castellum-dock-\(UUID().uuidString)")!)
        prefs.dockBadgeMode = .vipUnread
        var recomputes = 0
        let controller = DockBadgeController(
            model: model, preferences: prefs, focusState: { recomputes += 1; return .inactive },
            debounceMilliseconds: 0, setLabel: { _ in })
        try await Task.sleep(nanoseconds: 200_000_000)
        let before = recomputes
        model.setVip(address: "boss@example.com", displayName: "Boss", vip: true)
        let deadline = Date().addingTimeInterval(5)
        while recomputes == before && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertGreaterThan(recomputes, before, "adding a VIP must refresh the Dock badge")
        withExtendedLifetime(controller) {}
    }

    // MARK: MM-9 scopes

    private func smart(_ id: String, unread: Int64, capped: Bool = false) -> SmartMailbox {
        SmartMailbox(
            id: id, name: "Smart \(id)", query: "", group: nil, scope: nil, sortOrder: 0, symbol: nil,
            cachedTotal: unread, cachedUnread: unread, countsCapped: capped, countsUpdatedAt: 0,
            createdAt: 0, updatedAt: 0)
    }

    private func text(
        _ scope: CounterScope, focus: FocusFilterState = .inactive,
        inbox: [String: Int64] = ["a": 3, "b": 4], vip: Int64? = nil,
        smart: [SmartMailbox] = [], excluded: Set<String> = [], dock: Bool = true
    ) -> String? {
        CounterLabel.text(
            scope: scope, focus: focus, inboxUnreadByAccount: inbox, vipUnread: vip,
            smartMailboxes: smart, excludedAccountIds: excluded, honoursDockFlag: dock)
    }

    func testSmartScopeShowsTheSidebarNumberAndIgnoresAccountLimits() {
        let boxes = [smart("s1", unread: 12)]
        XCTAssertEqual(text(.smartMailbox(id: "s1"), smart: boxes), "12")
        // Same text the sidebar row builds.
        XCTAssertEqual(SmartMailboxBadge.label(unread: 12, capped: false), "12")
        // Focus account limit and the per-account flag do not apply to a smart mailbox.
        XCTAssertEqual(
            text(.smartMailbox(id: "s1"), focus: focus(allowed: ["z"]), smart: boxes, excluded: ["a", "b"]), "12")
    }

    func testCappedSmartCountKeepsThePlusAndZeroHides() {
        XCTAssertEqual(text(.smartMailbox(id: "s1"), smart: [smart("s1", unread: 1000, capped: true)]), "1000+")
        XCTAssertNil(text(.smartMailbox(id: "s1"), smart: [smart("s1", unread: 0)]))
        XCTAssertEqual(text(.smartMailbox(id: "s1"), smart: [smart("s1", unread: 50_000)]), "9999+")
    }

    func testUnknownSmartMailboxFallsBackToInboxUnread() {
        XCTAssertEqual(text(.smartMailbox(id: "gone"), smart: [smart("s1", unread: 99)]), "7")
    }

    func testExcludedAccountLeavesTheInboxCount() {
        XCTAssertEqual(text(.inboxUnread, excluded: ["a"]), "4")
        XCTAssertNil(text(.inboxUnread, excluded: ["a", "b"]))
    }

    func testExclusionAndFocusIntersect() {
        XCTAssertEqual(text(.inboxUnread, focus: focus(allowed: ["a", "b"]), excluded: ["a"]), "4")
        XCTAssertNil(text(.inboxUnread, focus: focus(allowed: ["a"]), excluded: ["a"]))
    }

    func testVipIdsAreFocusIntersectExclusionAndNilWhenUnfiltered() {
        XCTAssertNil(CounterLabel.includedAccountIds(allAccountIds: ["a", "b"], focus: .inactive, excluded: []))
        XCTAssertEqual(
            CounterLabel.includedAccountIds(allAccountIds: ["a", "b", "c"], focus: .inactive, excluded: ["b"]), ["a", "c"])
        XCTAssertEqual(
            CounterLabel.includedAccountIds(
                allAccountIds: ["a", "b", "c"], focus: focus(allowed: ["a", "b"]), excluded: ["b"]), ["a"])
        XCTAssertEqual(
            CounterLabel.includedAccountIds(allAccountIds: ["a"], focus: focus(allowed: []), excluded: []), [])
    }

    func testVipScopeUsesTheSuppliedCountAndCaps() {
        XCTAssertEqual(text(.vipUnread, vip: 5), "5")
        XCTAssertEqual(text(.vipUnread, vip: 20_000), "9999+")
        XCTAssertNil(text(.vipUnread, vip: nil))
    }

    func testFocusDockFlagHidesTheDockButNotTheMenuBar() {
        XCTAssertNil(text(.inboxUnread, focus: focus(dock: false), dock: true))
        XCTAssertEqual(text(.inboxUnread, focus: focus(dock: false), dock: false), "7")
    }

    func testScopeFoldsModeAndId() {
        XCTAssertEqual(CounterScope(mode: .smartMailbox, smartMailboxId: "s"), .smartMailbox(id: "s"))
        XCTAssertEqual(CounterScope(mode: .smartMailbox, smartMailboxId: nil), .inboxUnread)
        XCTAssertEqual(CounterScope(mode: .off, smartMailboxId: "s"), .off)
        XCTAssertEqual(CounterScope(mode: .vipUnread, smartMailboxId: nil), .vipUnread)
    }

    func testDockControllerFollowsASmartMailboxAndAnExcludedAccount() {
        let model = makeModel()
        let prefs = Preferences(defaults: UserDefaults(suiteName: "castellum-dock-\(UUID().uuidString)")!)
        var labels: [String?] = []
        let controller = DockBadgeController(
            model: model, preferences: prefs, focusState: { .inactive }, debounceMilliseconds: 0,
            setLabel: { labels.append($0) })
        model.mailboxes = ["a": [inboxMailbox("a")], "b": [inboxMailbox("b")]]
        model.counts = [
            "a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 5, unread: 3),
            "b:INBOX": MailboxCounts(mailboxId: "b:INBOX", total: 5, unread: 4),
        ]
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "7")
        prefs.setCountsInCounters(false, accountId: "b")
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "3")
        model.smartMailboxes = [smart("s1", unread: 11)]
        prefs.dockBadgeSmartMailboxId = "s1"
        prefs.dockBadgeMode = .smartMailbox
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "11")
        // The sidebar's cached count moving moves the Dock badge too (one counting path).
        model.smartMailboxes = [smart("s1", unread: 2)]
        controller.recompute()
        XCTAssertEqual(labels.last ?? nil, "2")
    }
}
