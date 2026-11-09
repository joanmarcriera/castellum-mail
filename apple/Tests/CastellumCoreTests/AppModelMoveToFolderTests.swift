// Castellum - Move to Folder with undo (, slice 4): the planning
// rules, and the AppModel path against a real profile (same fixture style as
// AppModelUndoRemovalTests; no IMAP server).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class MoveToFolderPlanTests: XCTestCase {
    private func row(_ id: String, account: String, mailbox: String) -> MessageHeader {
        MessageHeader(
            id: id, accountId: account, mailboxId: mailbox, subject: id, fromName: nil, fromAddress: nil,
            sentAt: nil, snippet: nil, hasAttachments: false, hasBody: nil, isRead: false,
            isStarred: false, threadCount: 1)
    }

    private func box(_ id: String, _ name: String, role: String? = nil, selectable: Bool? = true,
                     account: String = "a1") -> Mailbox {
        Mailbox(id: id, accountId: account, path: name, name: name, role: role, delimiter: "/",
                subscribed: true, selectable: selectable, syncByDefault: true)
    }

    func testEligibility() {
        let rows = [row("m1", account: "a1", mailbox: "a1:INBOX"), row("m2", account: "a2", mailbox: "a2:INBOX"),
                    row("m3", account: "a1", mailbox: "a1:INBOX")]
        XCTAssertEqual(MoveToFolderPlan.eligibility(ids: [], rows: rows), .noSelection)
        XCTAssertEqual(MoveToFolderPlan.eligibility(ids: ["gone"], rows: rows), .noSelection)
        XCTAssertEqual(MoveToFolderPlan.eligibility(ids: ["m1", "m3"], rows: rows), .ok(accountId: "a1"))
        XCTAssertEqual(MoveToFolderPlan.eligibility(ids: ["m1", "m2"], rows: rows), .multipleAccounts)
    }

    func testDestinationsAreSelectableSpecialUseFirstThenByName() {
        let boxes = ["a1": [
            box("z", "Zebra"), box("t", "Trash", role: "trash"), box("n", "Noselect", selectable: false),
            box("i", "INBOX", role: "inbox"), box("b", "Banana"), box("a", "Archive", role: "archive"),
        ], "a2": [box("x", "Other", account: "a2")]]
        XCTAssertEqual(
            MoveToFolderPlan.destinations(accountId: "a1", mailboxes: boxes).map(\.name),
            ["INBOX", "Archive", "Trash", "Banana", "Zebra"])
        XCTAssertEqual(MoveToFolderPlan.destinations(accountId: "a1", mailboxes: boxes, query: "an").map(\.name), ["Banana"])
        XCTAssertEqual(MoveToFolderPlan.destinations(accountId: "a2", mailboxes: boxes).map(\.name), ["Other"])
        XCTAssertTrue(MoveToFolderPlan.destinations(accountId: "none", mailboxes: boxes).isEmpty)
    }

    func testMessagesAlreadyInTheDestinationAreSkipped() {
        let rows = [row("m1", account: "a1", mailbox: "a1:INBOX"), row("m2", account: "a1", mailbox: "a1:Projects")]
        XCTAssertEqual(MoveToFolderPlan.idsToMove(ids: ["m1", "m2"], rows: rows, destinationId: "a1:Projects"), ["m1"])
        XCTAssertTrue(MoveToFolderPlan.idsToMove(ids: ["m2"], rows: rows, destinationId: "a1:Projects").isEmpty,
                      "source folder equal to destination is rejected")
    }

    func testCommandIsRegisteredWithItsKeysAndAvailability() {
        let spec = CommandRegistry.spec(for: .moveToFolder)
        XCTAssertEqual(spec.notification, .castellumMoveToFolder)
        XCTAssertEqual(spec.bareKey, "V")
        XCTAssertEqual(spec.menuChord?.display, "⌃⌘M")
        XCTAssertFalse(spec.destructive, "it only opens a picker; the move itself is undoable")
    }
}

@MainActor
final class AppModelMoveToFolderTests: XCTestCase {
    private var models: [AppModel] = []
    private let account = "acc-move"

    private func makeModel(writes: Bool = true) throws -> (AppModel, UndoManager) {
        let dir = TestScratch.root.appendingPathComponent("castellum-move-folder-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: account, email: "me@local.test",
            mailboxId: "\(account):INBOX", role: "inbox")
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: account, email: "me@local.test",
            mailboxId: "\(account):Projects", role: nil, path: "Projects")
        var headers: [MessageHeader] = []
        for uid in [Int64(1), 2] {
            let id = "\(account):INBOX:\(uid)"
            try DirectFixture.seedMessage(
                profileDirectory: dir, id: id, accountId: account, mailboxId: "\(account):INBOX",
                subject: "m\(uid)", fromAddress: "a@local.test",
                listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
            try DirectFixture.setProviderUid(profileDirectory: dir, messageId: id, uid: uid)
            headers.append(MessageHeader(
                id: id, accountId: account, mailboxId: "\(account):INBOX", subject: "m\(uid)",
                fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
                hasBody: nil, isRead: false, isStarred: false, threadCount: 1))
        }
        try model.core.setAccountWriteEnabled(accountId: account, enabled: writes)
        model.reloadMailboxes(account)
        model.messages = headers
        let um = UndoManager()
        model.undoManagerProvider = { um }
        return (model, um)
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ cond: () -> Bool, timeout: TimeInterval = 8) async {
        let end = Date().addingTimeInterval(timeout)
        while !cond() && Date() < end { try? await Task.sleep(nanoseconds: 20_000_000) }
    }

    private func ops(_ model: AppModel, kind: String) throws -> [OperationRecord] {
        try model.core.pendingOperations(accountId: account).filter { $0.kind == kind }
    }

    func testMoveQueuesOneMoveOpAndRegistersAMoveUndo() async throws {
        let (model, um) = try makeModel()
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1", "\(account):INBOX:2"]
        model.moveSelected(toMailboxId: "\(account):Projects")
        await waitUntil { !model.removalUndoStack.isEmpty }
        let top = try XCTUnwrap(model.removalUndoStack.top)
        XCTAssertEqual(top.verb, .move)
        XCTAssertEqual(top.count, 2)
        let moves = try ops(model, kind: "move")
        XCTAssertEqual(moves.count, 2)
        for op in moves { XCTAssertTrue(op.payloadJson.contains("Projects"), op.payloadJson) }
        XCTAssertTrue(model.messages.isEmpty, "rows leave the list at once")
        XCTAssertEqual(model.removalUndoBanner.shown?.verb, .move)
        while um.groupingLevel > 0 { um.endUndoGrouping() }
        XCTAssertEqual(um.undoActionName, RemovalVerb.move.actionName, "Edit > Undo Move")
        model.undoLastRemoval()
        await waitUntil { ((try? self.ops(model, kind: "undo_move").count) ?? 0) >= 2 }
        XCTAssertEqual(try ops(model, kind: "undo_move").count, 2)
    }

    func testMoveToTheFolderTheyAreAlreadyInQueuesNothing() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.moveSelected(toMailboxId: "\(account):INBOX")
        try? await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertTrue(try ops(model, kind: "move").isEmpty)
        XCTAssertTrue(model.removalUndoStack.isEmpty)
        XCTAssertEqual(model.messages.count, 2, "nothing was removed from the list")
    }

    func testMoveOnAReadOnlyAccountIsRefusedAndRestoresTheRows() async throws {
        let (model, _) = try makeModel(writes: false)
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.moveSelected(toMailboxId: "\(account):Projects")
        try? await Task.sleep(nanoseconds: 400_000_000)
        XCTAssertTrue(try ops(model, kind: "move").isEmpty, "the core refused the write")
        XCTAssertTrue(model.removalUndoStack.isEmpty, "a refused move is not undoable")
    }

    func testMoveToAnUnknownFolderIsIgnored() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.moveSelected(toMailboxId: "other-account:INBOX")
        try? await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertTrue(try ops(model, kind: "move").isEmpty)
    }

    // MARK: recents (context menu slice 2)

    func testRecentsRecordedOnlyAfterASuccessfulMove() async throws {
        let (model, _) = try makeModel()
        let prefs = Preferences.shared
        prefs.clearRecentMoveDestinations(accountId: account)
        defer { prefs.clearRecentMoveDestinations(accountId: account) }
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.moveSelected(toMailboxId: "\(account):Projects")
        await waitUntil { !model.removalUndoStack.isEmpty }
        XCTAssertEqual(prefs.recentMoveDestinations(accountId: account).first, "\(account):Projects")
    }

    func testRecentsUntouchedWhenTheMoveIsRefused() async throws {
        let (model, _) = try makeModel(writes: false)
        Preferences.shared.clearRecentMoveDestinations(accountId: account)
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.moveSelected(toMailboxId: "\(account):Projects")
        try? await Task.sleep(nanoseconds: 400_000_000)
        XCTAssertTrue(Preferences.shared.recentMoveDestinations(accountId: account).isEmpty)
    }

    func testRecentListIsNewestFirstDeduplicatedAndBounded() {
        var list: [String] = []
        for id in ["a", "b", "c", "d", "e", "f", "c"] { list = Preferences.noting(id, in: list) }
        XCTAssertEqual(list, ["c", "f", "e", "d", "b"])
    }

    func testProtectedRolesAreNeverOfferedAsMoveDestinations() async throws {
        let (model, _) = try makeModel()
        for (name, role) in [("Drafts", "drafts"), ("Sent", "sent"), ("Trash", "trash"), ("Junk", "junk")] {
            try DirectFixture.seedAccountAndMailbox(
                profileDirectory: model.profileDirectory, accountId: account, email: "me@local.test",
                mailboxId: "\(account):\(name)", role: role, path: name)
        }
        model.reloadMailboxes(account)
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 6 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        XCTAssertEqual(Set(model.messageMenuContext().moveDestinations.map(\.id)),
                       ["\(account):INBOX", "\(account):Projects"],
                       "Drafts, Sent, Trash and Junk are never destinations")
    }

    func testMenuContextOffersOnlyAllowedFoldersOfTheSelectionAccount() async throws {
        let (model, _) = try makeModel()
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        let ctx = model.messageMenuContext()
        XCTAssertEqual(Set(ctx.moveDestinations.map(\.id)), ["\(account):INBOX", "\(account):Projects"])
        XCTAssertEqual(ctx.targetMailboxIds, ["\(account):INBOX"])
        XCTAssertFalse(ctx.multiAccount)
        XCTAssertFalse(ctx.junkMailboxAvailable, "no Junk-role mailbox seeded")
    }
}

/// Mark as Junk / Not Junk planning (context menu slice 3): pure, no core.
final class JunkPlanTests: XCTestCase {
    private func box(_ account: String, _ name: String, _ role: String?) -> Mailbox {
        Mailbox(id: "\(account):\(name)", accountId: account, path: name, name: name, role: role,
                delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }
    private func row(_ id: String, _ account: String, _ mailbox: String) -> MessageHeader {
        MessageHeader(
            id: id, accountId: account, mailboxId: "\(account):\(mailbox)", subject: id,
            fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: false, isStarred: false, threadCount: 1)
    }

    func testJunkGoesToTheJunkMailboxPerAccountAndSkipsWhatIsAlreadyThere() {
        let boxes = ["a": [box("a", "INBOX", "inbox"), box("a", "Junk", "junk")],
                     "b": [box("b", "INBOX", "inbox"), box("b", "Spam", "junk")]]
        let plan = AppModel.junkPlan(
            isJunk: true, rows: [row("1", "a", "INBOX"), row("2", "a", "Junk"), row("3", "b", "INBOX")],
            mailboxes: boxes)
        XCTAssertEqual(plan.batches, [.init(ids: ["1"], destinationId: "a:Junk"),
                                      .init(ids: ["3"], destinationId: "b:Spam")])
        XCTAssertTrue(plan.accountsWithoutJunk.isEmpty)
    }

    func testNotJunkMovesOnlyJunkMessagesBackToTheInbox() {
        let boxes = ["a": [box("a", "INBOX", "inbox"), box("a", "Junk", "junk")]]
        let plan = AppModel.junkPlan(
            isJunk: false, rows: [row("1", "a", "INBOX"), row("2", "a", "Junk")], mailboxes: boxes)
        XCTAssertEqual(plan.batches, [.init(ids: ["2"], destinationId: "a:INBOX")])
    }

    func testJunkIsDecidedByTheRowsMailboxRoleNotTheFirstJunkMailbox() {
        let boxes = ["a": [box("a", "INBOX", "inbox"), box("a", "Junk", "junk"), box("a", "Spam", "junk")]]
        let mark = AppModel.junkPlan(
            isJunk: true, rows: [row("1", "a", "Spam"), row("2", "a", "INBOX")], mailboxes: boxes)
        XCTAssertEqual(mark.batches, [.init(ids: ["2"], destinationId: "a:Junk")], "already in a junk-role box")
        let unmark = AppModel.junkPlan(
            isJunk: false, rows: [row("1", "a", "Spam"), row("2", "a", "INBOX")], mailboxes: boxes)
        XCTAssertEqual(unmark.batches, [.init(ids: ["1"], destinationId: "a:INBOX")])
    }

    func testAccountWithoutAJunkMailboxQueuesNothingAndIsReported() {
        let boxes = ["a": [box("a", "INBOX", "inbox")], "b": [box("b", "INBOX", "inbox"), box("b", "Junk", "junk")]]
        let plan = AppModel.junkPlan(
            isJunk: true, rows: [row("1", "a", "INBOX"), row("2", "b", "INBOX")], mailboxes: boxes)
        XCTAssertEqual(plan.accountsWithoutJunk, ["a"])
        XCTAssertEqual(plan.batches, [.init(ids: ["2"], destinationId: "b:Junk")])
    }
}

/// Mark as Junk / Not Junk through the real model + core (context menu slice 3).
@MainActor
final class JunkAppModelTests: XCTestCase {
    private var models: [AppModel] = []
    private let account = "acc-junk"

    private func makeModel(writes: Bool = true, junkMailbox: Bool = true) throws -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-junk-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: account, email: "me@local.test",
            mailboxId: "\(account):INBOX", role: "inbox")
        if junkMailbox {
            try DirectFixture.seedAccountAndMailbox(
                profileDirectory: dir, accountId: account, email: "me@local.test",
                mailboxId: "\(account):Junk", role: "junk", path: "Junk")
        }
        let id = "\(account):INBOX:1"
        try DirectFixture.seedMessage(
            profileDirectory: dir, id: id, accountId: account, mailboxId: "\(account):INBOX",
            subject: "spammy", fromAddress: "a@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        try DirectFixture.setProviderUid(profileDirectory: dir, messageId: id, uid: 1)
        try model.core.setAccountWriteEnabled(accountId: account, enabled: writes)
        model.reloadMailboxes(account)
        model.messages = [MessageHeader(
            id: id, accountId: account, mailboxId: "\(account):INBOX", subject: "spammy",
            fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: false, isStarred: false, threadCount: 1)]
        model.undoManagerProvider = { UndoManager() }
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ cond: () -> Bool, timeout: TimeInterval = 8) async {
        let end = Date().addingTimeInterval(timeout)
        while !cond() && Date() < end { try? await Task.sleep(nanoseconds: 20_000_000) }
    }

    private func moves(_ model: AppModel) throws -> [OperationRecord] {
        try model.core.pendingOperations(accountId: account).filter { $0.kind == "move" }
    }

    func testMarkAsJunkQueuesOneMoveToJunkRemovesTheRowAndIsUndoable() async throws {
        let model = try makeModel()
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.setJunkOnSelection(true)
        await waitUntil { !model.removalUndoStack.isEmpty }
        let ops = try moves(model)
        XCTAssertEqual(ops.count, 1)
        XCTAssertTrue(ops[0].payloadJson.contains("Junk"), ops[0].payloadJson)
        XCTAssertTrue(model.messages.isEmpty, "the row leaves the list at once")
        XCTAssertEqual(model.removalUndoStack.top?.verb, .junk)
        XCTAssertTrue(model.messageMenuContext() == .empty, "nothing selected any more")
    }

    func testMarkAsJunkOnAReadOnlyAccountIsRefusedWithTheRefusalAndKeepsTheRow() async throws {
        let model = try makeModel(writes: false)
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.setJunkOnSelection(true)
        await waitUntil { model.writeSafetyPrompt != nil || model.errorText != nil }
        // The standard refusal UI: the read-only prompt when the account is loaded in the
        // model, else the verb-named error line. Never a silent no-op.
        XCTAssertTrue(model.writeSafetyPrompt != nil || model.errorText?.contains("Mark as Junk failed") == true,
                      "got prompt=\(String(describing: model.writeSafetyPrompt)) error=\(String(describing: model.errorText))")
        XCTAssertTrue(try moves(model).isEmpty)
        XCTAssertEqual(model.messages.count, 1, "the row is restored/kept")
    }

    func testMarkAsJunkWithoutAJunkMailboxReportsAndQueuesNothing() async throws {
        let model = try makeModel(junkMailbox: false)
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 1 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        XCTAssertFalse(model.messageMenuContext().junkMailboxAvailable)
        model.setJunkOnSelection(true)
        XCTAssertEqual(model.errorText?.contains("no Junk folder"), true)
        XCTAssertTrue(try moves(model).isEmpty)
        XCTAssertEqual(model.messages.count, 1)
    }

    func testNotJunkWithNothingInJunkSaysSoInsteadOfDoingNothing() async throws {
        let model = try makeModel()
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.setJunkOnSelection(false)
        XCTAssertEqual(model.errorText, "Not Junk: none of the selected messages are in Junk.")
        XCTAssertTrue(try moves(model).isEmpty)
        XCTAssertEqual(model.messages.count, 1)
    }

    func testAMultiAccountJunkIsOneUndoEntryWithAllOperationIds() async throws {
        let model = try makeModel()
        let dir = model.profileDirectory
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: "acc-junk-2", email: "two@local.test",
            mailboxId: "acc-junk-2:INBOX", role: "inbox")
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: "acc-junk-2", email: "two@local.test",
            mailboxId: "acc-junk-2:Junk", role: "junk", path: "Junk")
        let id2 = "acc-junk-2:INBOX:1"
        try DirectFixture.seedMessage(
            profileDirectory: dir, id: id2, accountId: "acc-junk-2", mailboxId: "acc-junk-2:INBOX",
            subject: "spam2", fromAddress: "b@local.test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        try DirectFixture.setProviderUid(profileDirectory: dir, messageId: id2, uid: 1)
        try model.core.setAccountWriteEnabled(accountId: "acc-junk-2", enabled: true)
        model.reloadMailboxes("acc-junk-2")
        model.messages.append(MessageHeader(
            id: id2, accountId: "acc-junk-2", mailboxId: "acc-junk-2:INBOX", subject: "spam2",
            fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
            hasBody: nil, isRead: false, isStarred: false, threadCount: 1))
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 && (model.mailboxes["acc-junk-2"] ?? []).count >= 2 }
        model.selectedMessageIds = ["\(account):INBOX:1", id2]
        model.setJunkOnSelection(true)
        await waitUntil { !model.removalUndoStack.isEmpty }
        XCTAssertEqual(model.removalUndoStack.depth, 1, "one undo entry for the whole selection")
        XCTAssertEqual(model.removalUndoStack.top?.operationIds.count, 2)
        XCTAssertEqual(model.removalUndoStack.top?.count, 2)
    }

    func testMarkAsJunkWhenEverythingIsAlreadyInJunkSaysSo() async throws {
        let model = try makeModel()
        await waitUntil { (model.mailboxes[self.account] ?? []).count >= 2 }
        model.messages = model.messages.map {
            MessageHeader(
                id: $0.id, accountId: $0.accountId, mailboxId: "\(account):Junk", subject: $0.subject,
                fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil, hasAttachments: false,
                hasBody: nil, isRead: false, isStarred: false, threadCount: 1)
        }
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.setJunkOnSelection(true)
        XCTAssertEqual(model.errorText, "Mark as Junk: the selected messages are already in Junk.")
    }
}
