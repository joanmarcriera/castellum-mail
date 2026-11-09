// Castellum - AppModel undo for Archive / Delete, driven
// against a real profile: the account, mailbox and messages are seeded
// straight into SQLite (`DirectFixture`) and the archive/undo run through the
// real FFI, so the operation ids and the queued `undo_move` ops are the
// core's, not a fake's. No IMAP server is involved: replay finds no
// credentials and leaves the queue alone.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class AppModelUndoRemovalTests: XCTestCase {
    private var models: [AppModel] = []
    private let account = "acc-undo"

    private func makeModel(uids: [Int64] = [1], writes: Bool = true) throws -> (AppModel, UndoManager) {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-undo-removal-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: dir, accountId: account, email: "me@local.test",
            mailboxId: "\(account):INBOX", role: "inbox")
        var headers: [MessageHeader] = []
        for uid in uids {
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

    func testArchivePushesUndoEntryWithOperationIds() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        let top = try XCTUnwrap(model.removalUndoStack.top)
        XCTAssertEqual(top.verb, .archive)
        XCTAssertEqual(top.operationIds, try ops(model, kind: "move").map(\.id))
        XCTAssertEqual(top.accountIds, [account])
        XCTAssertTrue(model.paletteContext.removalUndoAvailable)
    }

    func testMultiSelectArchiveIsOneUndoStep() async throws {
        let (model, _) = try makeModel(uids: [1, 2, 3])
        model.selectedMessageIds = (1...3).map { "\(account):INBOX:\($0)" }
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        XCTAssertEqual(model.removalUndoStack.depth, 1)
        XCTAssertEqual(model.removalUndoStack.top?.operationIds.count, 3)
        XCTAssertEqual(model.removalUndoStack.top?.count, 3)
    }

    func testUndoPopsEntryAndCallsCoreOnce() async throws {
        let (model, _) = try makeModel(uids: [1, 2])
        model.selectedMessageIds = ["\(account):INBOX:1", "\(account):INBOX:2"]
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        model.undoLastRemoval()
        model.undoLastRemoval() // second press: nothing left, must not queue again
        await waitUntil { ((try? self.ops(model, kind: "undo_move").count) ?? 0) >= 2 }
        XCTAssertTrue(model.removalUndoStack.isEmpty)
        XCTAssertEqual(try ops(model, kind: "undo_move").count, 2, "one undo_move per message, once")
    }

    func testUndoManagerActionNameAndSingleFire() async throws {
        let (model, um) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        while um.groupingLevel > 0 { um.endUndoGrouping() }
        XCTAssertTrue(um.canUndo)
        XCTAssertEqual(um.undoActionName, RemovalVerb.archive.actionName)
        um.undo() // what Edit > Undo / Cmd-Z does
        await waitUntil { ((try? self.ops(model, kind: "undo_move").count) ?? 0) >= 1 }
        XCTAssertTrue(model.removalUndoStack.isEmpty)
        model.undoLastRemoval() // the bare z key after Cmd-Z: no double fire
        try? await Task.sleep(nanoseconds: 200_000_000)
        XCTAssertEqual(try ops(model, kind: "undo_move").count, 1)
        XCTAssertFalse(um.canUndo)
    }

    // MARK: - banner (slice 2)

    func testArchiveOffersTheBannerAndUndoSwitchesItToReturning() async throws {
        let (model, _) = try makeModel(uids: [1, 2])
        model.selectedMessageIds = ["\(account):INBOX:1", "\(account):INBOX:2"]
        model.archiveSelected()
        await waitUntil { model.removalUndoBanner.isVisible }
        XCTAssertEqual(model.removalUndoBanner.shown?.count, 2)
        XCTAssertEqual(model.removalUndoBanner.shown?.verb, .archive)
        XCTAssertEqual(model.removalUndoBanner.shown?.phase, .offering(remaining: 8))
        model.undoRemovalFromBanner()
        await waitUntil { ((try? self.ops(model, kind: "undo_move").count) ?? 0) >= 2 }
        guard case .returning = model.removalUndoBanner.shown?.phase else {
            return XCTFail("expected Returning after Undo, got \(String(describing: model.removalUndoBanner.shown))")
        }
    }

    func testRefusedUndoDropsTheBanner() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { model.removalUndoBanner.isVisible }
        try model.core.setAccountWriteEnabled(accountId: account, enabled: false)
        model.undoRemovalFromBanner()
        await waitUntil { !model.removalUndoBanner.isVisible }
        XCTAssertFalse(model.removalUndoBanner.isVisible)
        XCTAssertTrue(model.removalUndoStack.isEmpty)
    }

    func testDeleteRegistersADeleteUndo() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.deleteSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        XCTAssertEqual(model.removalUndoStack.top?.verb, .delete)
    }

    func testUndoRefusedInEvaluationModeReportsWriteFailure() async throws {
        let (model, _) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        try model.core.setAccountWriteEnabled(accountId: account, enabled: false)
        model.undoLastRemoval()
        await waitUntil { model.errorText != nil || model.writeSafetyPrompt != nil }
        XCTAssertTrue(
            model.errorText?.contains("Undo") == true || model.writeSafetyPrompt != nil,
            "a refused undo must be reported, not swallowed")
        XCTAssertEqual(try ops(model, kind: "undo_move").count, 0)
    }

    func testRefusedArchiveRegistersNoUndo() async throws {
        let (model, um) = try makeModel(writes: false)
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { model.errorText != nil || model.writeSafetyPrompt != nil }
        XCTAssertTrue(model.removalUndoStack.isEmpty)
        XCTAssertFalse(um.canUndo)
    }

    func testRemovingAnAccountDropsItsUndoEntries() async throws {
        let (model, um) = try makeModel()
        model.selectedMessageIds = ["\(account):INBOX:1"]
        model.archiveSelected()
        await waitUntil { !model.removalUndoStack.isEmpty }
        // The auto group closes at the end of the run-loop event in the app; in
        // a test it is still open and would keep an empty group (canUndo true).
        while um.groupingLevel > 0 { um.endUndoGrouping() }
        model.removeAccount(accountId: account)
        XCTAssertTrue(model.removalUndoStack.isEmpty)
        XCTAssertFalse(um.canUndo)
    }
}
