// Castellum - one draft row per compose window, and close keeps the server copy current.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Bug hunt 2026-10-12:
// - fast typing in a new compose window started one INSERT per keystroke
//   until the first save returned, leaving orphan draft rows;
// - closing within the 2 s debounce cancelled the pending server-draft save,
//   so the server Drafts copy never got the last edits.
// The save is injected here, so a test can hold the first INSERT open.

import XCTest
@testable import Castellum

@MainActor
final class ComposeDraftCreationTests: XCTestCase {
    /// A stand-in for `AppModel.saveDraft` that records each call and only
    /// reports when the test says so.
    @MainActor
    private final class DeferredSave {
        var calls = 0
        var pending: [@MainActor (Int64?) -> Void] = []
        func save(_ done: @escaping @MainActor (Int64?) -> Void) {
            calls += 1
            pending.append(done)
        }
        func finish(_ id: Int64?) {
            let all = pending
            pending = []
            for done in all { done(id) }
        }
    }

    func testTwoEditsBeforeTheFirstSaveReturnsInsertOnlyOneRow() {
        let s = ComposeSession(accountId: "acct")
        let store = DeferredSave()
        var answers: [Int64?] = []

        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        XCTAssertEqual(store.calls, 1, "one INSERT while the first is in flight")
        XCTAssertTrue(s.isCreatingDraft)
        XCTAssertTrue(answers.isEmpty)

        store.finish(42)
        XCTAssertEqual(answers, [42, 42, 42], "every caller gets the one row")
        XCTAssertEqual(s.draftId, 42)
        XCTAssertFalse(s.isCreatingDraft)

        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        XCTAssertEqual(store.calls, 1, "an existing row is never inserted again")
        XCTAssertEqual(answers.last, 42)
    }

    func testAFailedFirstSaveAnswersEveryoneAndLetsTheNextEditTryAgain() {
        let s = ComposeSession(accountId: "acct")
        let store = DeferredSave()
        var answers: [Int64?] = []
        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        store.finish(nil)
        XCTAssertEqual(answers, [nil, nil])
        XCTAssertNil(s.draftId)
        XCTAssertFalse(s.isCreatingDraft)

        s.ensureDraftSaved(save: store.save) { answers.append($0) }
        XCTAssertEqual(store.calls, 2)
        store.finish(7)
        XCTAssertEqual(s.draftId, 7)
    }

    /// Close and Send go through this: they must see the id the in-flight
    /// INSERT produces, so they update that row instead of inserting another.
    func testWorkQueuedBehindTheFirstSaveSeesItsId() {
        let s = ComposeSession(accountId: "acct")
        let store = DeferredSave()
        var seen: [Int64?] = []
        s.ensureDraftSaved(save: store.save) { _ in }
        s.afterPendingDraftCreation { seen.append(s.draftId) }
        XCTAssertTrue(seen.isEmpty, "deferred while the INSERT is in flight")
        store.finish(9)
        XCTAssertEqual(seen, [9])
    }

    func testWorkRunsAtOnceWhenNoSaveIsInFlight() {
        let s = ComposeSession(accountId: "acct")
        var ran = false
        s.afterPendingDraftCreation { ran = true }
        XCTAssertTrue(ran)
    }

    // MARK: - close flushes the pending server-draft save

    func testFlushRunsAPendingServerDraftSaveInsteadOfDroppingIt() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-draft-flush-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        defer {
            try? model.accountStore.deleteAll()
            try? FileManager.default.removeItem(at: dir)
        }
        XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
        let s = ComposeSession(accountId: "acct")
        s.draftId = 5

        XCTAssertFalse(model.hasPendingServerDraftSave(draftId: 5))
        model.scheduleServerDraftSave(session: s, draftId: 5)
        XCTAssertTrue(model.hasPendingServerDraftSave(draftId: 5))

        model.flushServerDraftSave(session: s, draftId: 5)
        XCTAssertFalse(model.hasPendingServerDraftSave(draftId: 5))
        XCTAssertEqual(model.serverDraftSaveState[5], .saving,
                       "the save ran now rather than being cancelled")

        // Nothing pending: a second flush does nothing.
        model.serverDraftSaveState[5] = .idle
        model.flushServerDraftSave(session: s, draftId: 5)
        XCTAssertEqual(model.serverDraftSaveState[5], .idle)
    }
}
