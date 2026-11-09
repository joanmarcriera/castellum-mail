// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The menu bar counter (MM-9): title per scope, zero = icon only, off = no item,
/// independence from the Dock badge, Focus "Show Dock badge" ignored, and removal.
/// Every controller gets a `RecordingStatusItemPort`: no test creates a real NSStatusItem.
@MainActor
final class MenuBarCounterTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-menubar-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func prefs() -> Preferences {
        Preferences(defaults: UserDefaults(suiteName: "castellum-menubar-\(UUID().uuidString)")!)
    }

    private func inbox(_ account: String) -> Mailbox {
        Mailbox(
            id: "\(account):INBOX", accountId: account, path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    private func load(_ model: AppModel, a: Int64, b: Int64) {
        model.mailboxes = ["a": [inbox("a")], "b": [inbox("b")]]
        model.counts = [
            "a:INBOX": MailboxCounts(mailboxId: "a:INBOX", total: 9, unread: a),
            "b:INBOX": MailboxCounts(mailboxId: "b:INBOX", total: 9, unread: b),
        ]
    }

    private func smart(_ id: String, unread: Int64) -> SmartMailbox {
        SmartMailbox(
            id: id, name: "S", query: "", group: nil, scope: nil, sortOrder: 0, symbol: nil,
            cachedTotal: unread, cachedUnread: unread, countsCapped: false, countsUpdatedAt: 0,
            createdAt: 0, updatedAt: 0)
    }

    private func make(
        _ model: AppModel, _ prefs: Preferences, port: RecordingStatusItemPort,
        focus: FocusFilterState = .inactive
    ) -> MenuBarCounterController {
        MenuBarCounterController(
            model: model, preferences: prefs, focusState: { focus }, debounceMilliseconds: 0, port: port)
    }

    func testOffByDefaultCreatesNoItem() {
        let model = makeModel()
        let port = RecordingStatusItemPort()
        let c = make(model, prefs(), port: port)
        load(model, a: 3, b: 4)
        c.recompute()
        XCTAssertFalse(port.visible)
        XCTAssertEqual(c.currentTitle, "off")
        XCTAssertTrue(port.titles.isEmpty)
    }

    func testTitleFollowsTheScope() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 3, b: 4)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertTrue(port.visible)
        XCTAssertEqual(c.currentTitle, "7")
        model.smartMailboxes = [smart("s1", unread: 21)]
        p.menuBarSmartMailboxId = "s1"
        p.menuBarCounterMode = .smartMailbox
        c.recompute()
        XCTAssertEqual(c.currentTitle, "21")
        XCTAssertEqual(port.titles.last, "21")
    }

    func testZeroShowsTheIconOnly() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 0, b: 0)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertTrue(port.visible, "zero unread keeps the icon")
        XCTAssertEqual(c.currentTitle, "")
        XCTAssertEqual(port.titles.last, "")
    }

    func testTurningItOffRemovesTheItemAndItComesBack() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 1, b: 1)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertTrue(port.visible)
        p.menuBarCounterMode = .off
        c.recompute()
        XCTAssertFalse(port.visible)
        XCTAssertEqual(c.currentTitle, "off")
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertTrue(port.visible)
        XCTAssertEqual(c.currentTitle, "2")
    }

    func testStopRemovesTheItem() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 1, b: 0)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        c.stop()
        XCTAssertFalse(port.visible)
        c.recompute()
        XCTAssertFalse(port.visible, "a stopped controller never recreates the item")
    }

    func testIndependentOfTheDockMode() {
        let model = makeModel()
        let p = prefs()
        p.dockBadgeMode = .off
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 2, b: 0)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertEqual(c.currentTitle, "2")
        // And the other way: a Dock badge does not turn the menu bar on.
        let off = RecordingStatusItemPort()
        let c2 = make(model, prefs(), port: off)
        c2.recompute()
        XCTAssertFalse(off.visible)
    }

    func testFocusHideDockBadgeIsIgnored() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let focus = FocusFilterState(allowedAccountIds: nil, vipsOnly: false, showDockBadge: false)
        let c = make(model, p, port: port, focus: focus)
        load(model, a: 5, b: 0)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        XCTAssertEqual(c.currentTitle, "5")
    }

    func testExcludedAccountIsLeftOut() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 3, b: 4)
        p.menuBarCounterMode = .inboxUnread
        p.setCountsInCounters(false, accountId: "a")
        c.recompute()
        XCTAssertEqual(c.currentTitle, "4")
    }

    func testUserRemovingTheItemSetsThePreferenceOff() {
        let model = makeModel()
        let p = prefs()
        let port = RecordingStatusItemPort()
        let c = make(model, p, port: port)
        load(model, a: 3, b: 0)
        p.menuBarCounterMode = .inboxUnread
        c.recompute()
        port.onUserRemoved?()   // what the real port calls on a Cmd-drag out of the menu bar
        XCTAssertEqual(p.menuBarCounterMode, .off)
        XCTAssertFalse(port.visible)
        XCTAssertEqual(c.currentTitle, "off")
    }

    func testDefaultPortUnderXCTestCreatesNoStatusItem() {
        let model = makeModel()
        let c = MenuBarCounterController(model: model, preferences: prefs(), debounceMilliseconds: 0)
        c.recompute()
        XCTAssertEqual(c.currentTitle, "off")
    }
}
