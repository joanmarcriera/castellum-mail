// Castellum - the registry-driven message row context menu (spec slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `MessageContextMenu.entries` is pure, so order, enablement and the read-only
// rules are proven here without a window. The selection / first-responder
// behaviour of the real table is in `MessageListContextMenuSelectionTests`.

import AppKit
import XCTest
@testable import Castellum

final class MessageContextMenuTests: XCTestCase {
    private func context(
        count: Int = 1, unread: Bool = true, read: Bool = false, flagged: Bool = false,
        readOnly: [MessageMenuContext.ReadOnlyAccount] = [], allReadOnly: Bool = false,
        roles: Set<String> = ["inbox"], snoozed: Bool = false, body: Bool = true,
        ai: Bool = false, embeddingModel: Bool = true,
        destinations: [MessageMenuContext.MoveDestination] = [], recents: [String] = [],
        targetMailboxes: Set<String> = [], multiAccount: Bool = false, junk: Bool = true
    ) -> MessageMenuContext {
        MessageMenuContext(
            selectionCount: count, anyUnread: unread, anyRead: read, anyFlagged: flagged,
            allFlagged: flagged, bodyAvailable: body, aiEnabled: ai, readOnlyAccounts: readOnly,
            allTargetsReadOnly: allReadOnly, sourceRoles: roles, isSnoozedList: snoozed,
            threadMuted: false, senderIsVip: false, aiEmbeddingModelSet: embeddingModel,
            moveDestinations: destinations, recentMoveIds: recents, targetMailboxIds: targetMailboxes,
            multiAccount: multiAccount, junkMailboxAvailable: junk)
    }

    /// Every `.command` in the tree, flattened.
    private func commands(_ entries: [MessageMenuEntry])
        -> [(id: CommandID, title: String, enabled: Bool, reason: String?)]
    {
        entries.flatMap { e -> [(CommandID, String, Bool, String?)] in
            switch e {
            case .command(let id, let title, let enabled, let reason): return [(id, title, enabled, reason)]
            case .submenu(_, let inner): return commands(inner).map { ($0.id, $0.title, $0.enabled, $0.reason) }
            default: return []
            }
        }.map { (id: $0.0, title: $0.1, enabled: $0.2, reason: $0.3) }
    }

    private func topLevelCount(_ entries: [MessageMenuEntry]) -> Int {
        entries.filter { !$0.isSeparator }.count
    }

    func testEveryCommandEntryHasASpecAndANotification() {
        let entries = MessageContextMenu.entries(for: context(read: true))
        XCTAssertFalse(commands(entries).isEmpty)
        for c in commands(entries) {
            XCTAssertNotNil(CommandRegistry.all.first { $0.id == c.id }, "\(c.id) has no CommandSpec")
            XCTAssertNotNil(CommandRegistry.spec(for: c.id).notification, "\(c.id) has no notification")
        }
    }

    func testTopLevelHasAtMostFourteenEntriesFifteenOnMixedReadState() {
        XCTAssertLessThanOrEqual(topLevelCount(MessageContextMenu.entries(for: context())), 14)
        let mixed = MessageContextMenu.entries(for: context(unread: true, read: true))
        XCTAssertLessThanOrEqual(topLevelCount(mixed), 15)
        XCTAssertEqual(commands(mixed).filter { [.markRead, .markUnread].contains($0.id) }.count, 2)
    }

    func testReadStateOffersOnlyTheUsefulVerb() {
        let allUnread = commands(MessageContextMenu.entries(for: context(unread: true, read: false)))
        XCTAssertTrue(allUnread.contains { $0.id == .markRead })
        XCTAssertFalse(allUnread.contains { $0.id == .markUnread })
        let allRead = commands(MessageContextMenu.entries(for: context(unread: false, read: true)))
        XCTAssertTrue(allRead.contains { $0.id == .markUnread })
        XCTAssertFalse(allRead.contains { $0.id == .markRead })
    }

    func testSingleOnlyEntriesAreDisabledOnAMultiSelection() {
        let all = commands(MessageContextMenu.entries(for: context(count: 2)))
        for id in [CommandID.reply, .replyAll, .forward, .openInWindow, .viewSource, .printMessage,
                   .showCorrespondence, .findSimilar, .summarize] {
            let hit = all.first { $0.id == id }
            XCTAssertNotNil(hit, "\(id) must be present, not hidden")
            XCTAssertEqual(hit?.enabled, false, "\(id) needs one message")
        }
        // The bulk verbs stay enabled.
        for id in [CommandID.archive, .delete, .toggleFlag, .tagMessage, .markRead] {
            XCTAssertEqual(all.first { $0.id == id }?.enabled, true, "\(id)")
        }
        // And on a single message they are enabled.
        let one = commands(MessageContextMenu.entries(for: context(count: 1)))
        XCTAssertEqual(one.first { $0.id == .reply }?.enabled, true)
    }

    func testAllReadOnlyDisablesExactlyTheServerMutatingItemsWithReasons() {
        let acct = MessageMenuContext.ReadOnlyAccount(id: "a", email: "me@example.com")
        let entries = MessageContextMenu.entries(
            for: context(unread: true, read: true, readOnly: [acct], allReadOnly: true))
        let all = commands(entries)
        let disabled = Set(all.filter { !$0.enabled && MessageContextMenu.serverMutating.contains($0.id) }.map(\.id))
        XCTAssertEqual(disabled, [.archive, .moveToFolder, .delete, .markRead, .markUnread, .toggleFlag, .tagMessage,
                        .markJunk])
        for c in all where MessageContextMenu.serverMutating.contains(c.id) {
            XCTAssertFalse(c.enabled, "\(c.id)")
            XCTAssertFalse((c.reason ?? "").isEmpty, "\(c.id) needs a tooltip reason")
            XCTAssertTrue(c.reason?.contains("me@example.com") == true)
        }
        // Local-only items are not touched by read-only mode.
        XCTAssertEqual(all.first { $0.id == .snoozeTomorrow }?.enabled, true)
        XCTAssertEqual(all.first { $0.id == .reply }?.enabled, true)
        // And the Enable Writes item is appended, last.
        XCTAssertEqual(entries.last, .enableWrites(email: "me@example.com", accountId: "a"))
    }

    func testMixedReadOnlySelectionLeavesMutatingItemsEnabled() {
        let acct = MessageMenuContext.ReadOnlyAccount(id: "a", email: "me@example.com")
        let entries = MessageContextMenu.entries(
            for: context(count: 2, readOnly: [acct], allReadOnly: false))
        for c in commands(entries) where MessageContextMenu.serverMutating.contains(c.id) {
            XCTAssertTrue(c.enabled, "\(c.id) must stay enabled for a mixed selection")
        }
    }

    func testSnoozeSubmenuHasFourPresetsAndCustom() {
        let entries = MessageContextMenu.entries(for: context())
        guard let sub = entries.compactMap({ e -> [MessageMenuEntry]? in
            if case .submenu(let t, let inner) = e, t == "Snooze" { return inner } else { return nil }
        }).first else { return XCTFail("no Snooze submenu") }
        XCTAssertEqual(commands(sub).map(\.id),
                       [.snoozeLaterToday, .snoozeTomorrow, .snoozeThisWeekend, .snoozeNextWeek, .snooze])
    }

    func testSnoozedListReplacesSnoozeWithMoveToInboxNow() {
        let all = commands(MessageContextMenu.entries(for: context(snoozed: true)))
        XCTAssertTrue(all.contains { $0.id == .unsnooze })
        XCTAssertFalse(all.contains { $0.id == .snoozeTomorrow })
    }

    func testDraftsDisableTheReplyGroup() {
        let all = commands(MessageContextMenu.entries(for: context(roles: ["drafts"])))
        for id in [CommandID.reply, .replyAll, .forward] { XCTAssertEqual(all.first { $0.id == id }?.enabled, false) }
    }

    func testNoSelectionYieldsNoMenu() {
        XCTAssertEqual(MessageContextMenu.entries(for: .empty), [])
    }

    func testDeleteIsLastAndFlagTitleFollowsState() {
        let entries = MessageContextMenu.entries(for: context(flagged: true))
        XCTAssertEqual(commands(entries).last?.id, .delete)
        XCTAssertEqual(commands(entries).first { $0.id == .toggleFlag }?.title, "Unflag")
        let plain = MessageContextMenu.entries(for: context(flagged: false))
        XCTAssertEqual(commands(plain).first { $0.id == .toggleFlag }?.title, "Flag")
    }

    // MARK: - slice 4: AI on this Mac ▸, Project ▸, Move to Folder…

    private func submenu(_ title: String, in entries: [MessageMenuEntry]) -> [MessageMenuEntry]? {
        for case .submenu(let t, let inner) in entries where t == title { return inner }
        return nil
    }

    func testAiSubmenuGroupsTheLocalAiCommandsWithReasonsWhenAiIsOff() throws {
        let entries = MessageContextMenu.entries(for: context(ai: false))
        let ai = try XCTUnwrap(submenu("AI on this Mac", in: entries))
        let items = commands(ai)
        XCTAssertEqual(items.map(\.id), [.summarize, .replyWithAi, .findSimilar])
        for item in items {
            XCTAssertFalse(item.enabled, "\(item.id)")
            XCTAssertEqual(item.reason, "Turn on AI in Settings → AI.")
        }
        // Find Similar is no longer buried under More.
        let more = try XCTUnwrap(submenu("More", in: entries))
        XCTAssertNil(MessageContextMenu.find(.findSimilar, in: more))
    }

    func testWithAiOnFindSimilarExplainsAMissingEmbeddingModel() throws {
        let on = commands(try XCTUnwrap(submenu("AI on this Mac",
            in: MessageContextMenu.entries(for: context(ai: true)))))
        XCTAssertTrue(on.allSatisfy(\.enabled), "\(on)")
        let noModel = commands(try XCTUnwrap(submenu("AI on this Mac",
            in: MessageContextMenu.entries(for: context(ai: true, embeddingModel: false)))))
        let find = try XCTUnwrap(noModel.first { $0.id == .findSimilar })
        XCTAssertFalse(find.enabled)
        XCTAssertEqual(find.reason, "Choose an embedding model in Settings → AI.")
        XCTAssertEqual(noModel.first { $0.id == .summarize }?.enabled, true)
    }

    func testSingleOnlyRulesApplyInsideTheAiSubmenu() throws {
        let items = commands(try XCTUnwrap(submenu("AI on this Mac",
            in: MessageContextMenu.entries(for: context(count: 3, ai: true)))))
        for item in items {
            XCTAssertFalse(item.enabled, "\(item.id)")
            XCTAssertEqual(item.reason, "Select a single message to use this.")
        }
    }

    func testProjectSubmenuOffersAssignToProject() throws {
        let items = commands(try XCTUnwrap(submenu("Project",
            in: MessageContextMenu.entries(for: context(count: 2)))))
        XCTAssertEqual(items.map(\.id), [.assignProject])
        XCTAssertTrue(items[0].enabled, "assigning several messages is allowed")
    }

    func testWithoutADestinationListMoveToFolderSitsNextToArchive() {
        let entries = MessageContextMenu.entries(for: context())
        let top = entries.compactMap { e -> CommandID? in
            if case .command(let id, _, _, _) = e { return id } else { return nil }
        }
        let archive = top.firstIndex(of: .archive)
        XCTAssertNotNil(archive)
        XCTAssertEqual(top.firstIndex(of: .moveToFolder), archive.map { $0 + 1 })
    }

    // MARK: Move to ▸ (slice 2)

    private func dest(_ id: String, _ name: String) -> MessageMenuContext.MoveDestination { .init(id: id, name: name) }
    private let folders: [MessageMenuContext.MoveDestination] = [
        .init(id: "a:INBOX", name: "Inbox"), .init(id: "a:Archive", name: "Archive"),
        .init(id: "a:Bills", name: "Bills"), .init(id: "a:Work", name: "Work"),
    ]

    private func moveTitles(_ entries: [MessageMenuEntry]) -> [String] {
        MessageContextMenu.allMoveTargets(in: entries).compactMap(\.title)
    }

    func testMoveToSubmenuListsRecentsFirstThenFoldersThenOther() throws {
        let entries = MessageContextMenu.entries(for: context(
            destinations: folders, recents: ["a:Work", "a:Bills"], targetMailboxes: ["a:INBOX"]))
        let sub = try XCTUnwrap(submenu("Move to", in: entries))
        // Recents (newest first), separator, the rest; the current mailbox is not offered.
        XCTAssertEqual(sub.map { $0.title ?? "-" }, ["Work", "Bills", "-", "Archive", "-", "Other…"])
        XCTAssertEqual(moveTitles(entries), ["Work", "Bills", "Archive"])
        XCTAssertEqual(commands(sub).map(\.id), [.moveToFolder], "Other… is the existing picker command")
    }

    func testMoveToPrunesStaleRecentsAndCapsAtFive() throws {
        let many = (1...8).map { dest("a:F\($0)", "F\($0)") }
        let entries = MessageContextMenu.entries(for: context(
            destinations: many, recents: ["a:GONE"] + many.map(\.id), targetMailboxes: ["a:INBOX"]))
        let sub = try XCTUnwrap(submenu("Move to", in: entries))
        let firstSeparator = try XCTUnwrap(sub.firstIndex { $0.isSeparator })
        XCTAssertEqual(firstSeparator, 5, "at most five recents, stale ids pruned")
        XCTAssertFalse(moveTitles(entries).contains("GONE"))
    }

    func testMoveToDoesNotOfferTheMailboxEveryTargetIsIn() {
        let entries = MessageContextMenu.entries(for: context(
            count: 2, destinations: folders, recents: ["a:INBOX"], targetMailboxes: ["a:INBOX"]))
        XCTAssertFalse(moveTitles(entries).contains("Inbox"))
        // Targets in two mailboxes: both are still valid destinations for the others.
        let mixed = MessageContextMenu.entries(for: context(
            count: 2, destinations: folders, targetMailboxes: ["a:INBOX", "a:Bills"]))
        XCTAssertTrue(moveTitles(mixed).contains("Inbox"))
        XCTAssertTrue(moveTitles(mixed).contains("Bills"))
    }

    func testMoveToOnAReadOnlyAccountIsOneDisabledItemWithTheReason() throws {
        let acct = MessageMenuContext.ReadOnlyAccount(id: "a", email: "me@example.com")
        let entries = MessageContextMenu.entries(for: context(
            readOnly: [acct], allReadOnly: true, destinations: folders, targetMailboxes: ["a:INBOX"]))
        XCTAssertNil(submenu("Move to", in: entries), "no destinations are offered when writes are refused")
        let move = try XCTUnwrap(commands(entries).first { $0.id == .moveToFolder })
        XCTAssertFalse(move.enabled)
        XCTAssertEqual(move.reason, MessageContextMenu.readOnlyReason(
            context(readOnly: [acct], allReadOnly: true)), "same reason Archive shows")
        XCTAssertEqual(commands(entries).first { $0.id == .archive }?.reason, move.reason)
    }

    func testMoveToIsDisabledForAMultiAccountSelectionAndForDrafts() throws {
        let multi = commands(MessageContextMenu.entries(for: context(
            count: 2, destinations: [], multiAccount: true)))
        let m = try XCTUnwrap(multi.first { $0.id == .moveToFolder })
        XCTAssertFalse(m.enabled)
        XCTAssertEqual(m.reason, "Select messages from one account to move them.")
        let drafts = commands(MessageContextMenu.entries(for: context(
            roles: ["drafts"], destinations: folders, targetMailboxes: ["a:Drafts"])))
        XCTAssertEqual(drafts.first { $0.id == .moveToFolder }?.enabled, false)
        XCTAssertFalse((drafts.first { $0.id == .moveToFolder }?.reason ?? "").isEmpty)
    }

    // MARK: Junk / Not Junk (slice 3)

    func testJunkItemIsMarkAsJunkOutsideJunkAndNotJunkInside() {
        let outside = commands(MessageContextMenu.entries(for: context()))
        XCTAssertEqual(outside.first { $0.id == .markJunk }?.title, "Mark as Junk")
        XCTAssertNil(outside.first { $0.id == .markNotJunk })
        let inside = commands(MessageContextMenu.entries(for: context(roles: ["junk"])))
        XCTAssertEqual(inside.first { $0.id == .markNotJunk }?.title, "Not Junk")
        XCTAssertEqual(inside.first { $0.id == .markNotJunk }?.enabled, true)
        XCTAssertNil(inside.first { $0.id == .markJunk })
        // A selection spanning Junk and the Inbox offers Mark as Junk (it skips what is in Junk).
        let mixed = commands(MessageContextMenu.entries(for: context(count: 2, roles: ["junk", "inbox"])))
        XCTAssertNotNil(mixed.first { $0.id == .markJunk })
    }

    func testJunkIsDisabledWithAReasonWhenTheAccountHasNoJunkFolder() throws {
        let j = try XCTUnwrap(commands(MessageContextMenu.entries(for: context(junk: false)))
            .first { $0.id == .markJunk })
        XCTAssertFalse(j.enabled)
        XCTAssertEqual(j.reason, "This account has no Junk folder.")
        // Not Junk goes to the Inbox, so it needs no Junk mailbox record to be enabled.
        let back = commands(MessageContextMenu.entries(for: context(roles: ["junk"], junk: false)))
        XCTAssertEqual(back.first { $0.id == .markNotJunk }?.enabled, true)
    }

    func testMarkAsJunkIsDisabledForADraftsSourceWithTheMoveToReason() throws {
        let j = try XCTUnwrap(commands(MessageContextMenu.entries(for: context(roles: ["drafts"])))
            .first { $0.id == .markJunk })
        XCTAssertFalse(j.enabled)
        XCTAssertEqual(j.reason, "Drafts can’t be moved from here. Open the draft to edit it.")
    }

    func testJunkOnAReadOnlyAccountCarriesTheReadOnlyReason() throws {
        let acct = MessageMenuContext.ReadOnlyAccount(id: "a", email: "me@example.com")
        let j = try XCTUnwrap(commands(MessageContextMenu.entries(
            for: context(readOnly: [acct], allReadOnly: true))).first { $0.id == .markJunk })
        XCTAssertFalse(j.enabled)
        XCTAssertTrue(j.reason?.contains("me@example.com") == true)
    }

    func testJunkAndMoveSpecsAreRegisteredAndRoutedLikeEveryOtherCommand() {
        for id in [CommandID.markJunk, .markNotJunk] {
            XCTAssertNotNil(CommandRegistry.spec(for: id).notification)
            XCTAssertTrue(MessageContextMenu.serverMutating.contains(id))
        }
        XCTAssertTrue(CommandRegistry.spec(for: .markJunk).destructive, "it removes rows from the list")
    }
}

/// Selection reconciliation on a real (off-screen) table.
@MainActor
final class MessageListContextMenuSelectionTests: XCTestCase {
    private var models: [AppModel] = []

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func header(_ id: String) -> MessageHeader {
        MessageHeader(id: id, accountId: "a", mailboxId: "a:INBOX", subject: id, fromName: nil,
                      fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
                      hasBody: true, isRead: false, isStarred: false, threadCount: 1)
    }

    /// Four unread rows [A..D] in a table hosted in a window, so first-responder is observable.
    private func setUp4() -> (MessageListView.Coordinator, NSTableView, AppModel, NSWindow) {
        let model = AppModel(profileDirectory: TestScratch.root
            .appendingPathComponent("castellum-ctxmenu-\(UUID().uuidString)"))
        models.append(model)
        model.messages = ["A", "B", "C", "D"].map(header)
        let coord = MessageListView.Coordinator(model: model, onOpen: {})
        let table = KeyTableView()
        table.addTableColumn(NSTableColumn(identifier: NSUserInterfaceItemIdentifier("from")))
        table.allowsMultipleSelection = true
        table.dataSource = coord
        table.delegate = coord
        coord.table = table
        coord.apply(model: model, table: table)
        let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
        scroll.documentView = table
        let search = NSSearchField(frame: NSRect(x: 0, y: 0, width: 100, height: 20))
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 500, height: 400),
                              styleMask: [.titled], backing: .buffered, defer: true)
        window.isReleasedWhenClosed = false
        window.contentView?.addSubview(scroll)
        window.contentView?.addSubview(search)
        window.makeFirstResponder(search)
        return (coord, table, model, window)
    }

    func testRightClickInsideTheSelectionKeepsIt() {
        let (coord, table, model, _) = setUp4()
        table.selectRowIndexes(IndexSet(0...2), byExtendingSelection: false)
        model.selectedMessageIds = ["A", "B", "C"]
        _ = coord.prepareRowMenu(forRow: 1)
        XCTAssertEqual(table.selectedRowIndexes, IndexSet(0...2))
        XCTAssertEqual(model.selectedMessageIds, ["A", "B", "C"])
    }

    func testRightClickOutsideReplacesSelectionWithoutScheduleAutoMarkRead() {
        let (coord, table, model, _) = setUp4()
        table.selectRowIndexes(IndexSet(0...1), byExtendingSelection: false)
        let entries = coord.prepareRowMenu(forRow: 3)
        XCTAssertFalse(entries.isEmpty)
        XCTAssertEqual(table.selectedRowIndexes, IndexSet(integer: 3))
        XCTAssertEqual(model.selectedMessageIds, ["D"])
        XCTAssertEqual(model.selectedMessage?.id, "D", "the row is shown (showDetail)")
        XCTAssertNil(model.autoMarkTimer, "a right-click must not schedule auto-mark-read")
        XCTAssertFalse(coord.selectingForContextMenu, "the reconcile flag is cleared afterwards")
    }

    func testTableBecomesFirstResponder() {
        let (coord, table, _, window) = setUp4()
        XCTAssertFalse(window.firstResponder === table)
        _ = coord.prepareRowMenu(forRow: 0)
        XCTAssertTrue(window.firstResponder === table,
                      "otherwise a focused search field makes CommandDispatcher drop Archive/Delete")
    }

    func testBlankAreaAndStaleRowGiveNoMenu() {
        let (coord, _, _, _) = setUp4()
        XCTAssertEqual(coord.prepareRowMenu(forRow: -1), [])
        XCTAssertEqual(coord.prepareRowMenu(forRow: 99), [])
    }

    func testInvokingACommandEntryPostsItsNotification() {
        let (coord, _, _, _) = setUp4()
        _ = coord.prepareRowMenu(forRow: 0)
        guard let entry = MessageContextMenu.find(.markRead, in: coord.lastRowMenu) else {
            return XCTFail("Mark as Read missing")
        }
        let posted = expectation(forNotification: .castellumMarkRead, object: nil)
        coord.invokeRowMenuEntry(entry)
        wait(for: [posted], timeout: 1)
    }

    func testNoClickedRowHandlerRemains() throws {
        // Spec "Done when": the old clickedRow-based Find Similar handler is gone.
        let src = try String(contentsOfFile: #filePath
            .replacingOccurrences(of: "Tests/CastellumCoreTests/MessageContextMenuTests.swift",
                                  with: "Sources/Castellum/App/MessageListView.swift"), encoding: .utf8)
        XCTAssertFalse(src.contains("findSimilarClicked"))
    }
}
