// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Combine
import XCTest
@testable import Castellum

private func op(
    id: Int64,
    kind: String = "mark_read",
    status: String = "queued",
    attempts: Int64 = 0,
    lastError: String? = nil,
    payload: String = #"{"message_id":"acc:INBOX:1"}"#,
    notBeforeEpoch: Int64? = nil
) -> OperationRecord {
    OperationRecord(
        id: id,
        accountId: "acc",
        kind: kind,
        payloadJson: payload,
        status: status,
        attempts: attempts,
        lastError: lastError,
        updatedAtEpoch: 1_788_000_000,
        notBeforeEpoch: notBeforeEpoch
    )
}

private enum SendLaterTestError: Error { case boom }

private func accountOps(_ ops: [OperationRecord]) -> [OutboxViewModel.AccountOps] {
    [OutboxViewModel.AccountOps(id: "acc", email: "a@x.test", ops: ops)]
}

@MainActor
final class OutboxViewModelTests: XCTestCase {
    private func makeVM(
        fetch: @escaping () async throws -> [OutboxViewModel.AccountOps],
        retry: @escaping (Int64) async throws -> Void = { _ in },
        cancel: @escaping (Int64) async throws -> Void = { _ in },
        afterRetry: @escaping @MainActor () -> Void = {},
        afterCancel: @escaping @MainActor () -> Void = {}
    ) -> OutboxViewModel {
        OutboxViewModel(
            fetch: fetch, retryOp: retry, cancelOp: cancel,
            afterRetry: afterRetry, afterCancel: afterCancel)
    }

    func testLoadFiltersEmptyAccountsAndLoads() async {
        let vm = makeVM(fetch: {
            [
                OutboxViewModel.AccountOps(id: "a1", email: "a@x.test", ops: [op(id: 1)]),
                OutboxViewModel.AccountOps(id: "a2", email: "b@x.test", ops: []),
            ]
        })
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded(let accounts) = vm.loadState else {
            return XCTFail("Expected .loaded; got \(vm.loadState)")
        }
        XCTAssertEqual(accounts.map(\.id), ["a1"], "op-free accounts are hidden")
    }

    /// #3036: when the queue drains behind an open window, the queue-change
    /// signal reloads and the finished op disappears without a manual Refresh.
    func testReloadsWhenQueueChangeSignalFires() async {
        var drained = false
        let revision = PassthroughSubject<Int, Never>()
        let vm = makeVM(fetch: { accountOps(drained ? [] : [op(id: 1)]) })
        vm.observeQueue(revision.eraseToAnyPublisher())
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded = vm.loadState else {
            return XCTFail("Expected .loaded; got \(vm.loadState)")
        }
        drained = true
        revision.send(1)  // the model's revision bump; no direct queueDidChange call
        for _ in 0..<10 { await Task.yield() }
        if case .empty = vm.loadState {} else {
            XCTFail("Expected .empty after the queue drained; got \(vm.loadState)")
        }
    }

    /// #3036: a queued send that turns failed leaves both counts equal; only
    /// the revision signal reloads, and the row must change with no direct call.
    func testRevisionSignalReloadsWhenCountsStayEqual() async {
        var failed = false
        let revision = PassthroughSubject<Int, Never>()
        let vm = makeVM(fetch: { accountOps([op(id: 1, status: failed ? "failed" : "queued")]) })
        vm.observeQueue(revision.eraseToAnyPublisher())
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        failed = true
        revision.send(2)
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded(let accounts) = vm.loadState else {
            return XCTFail("Expected .loaded; got \(vm.loadState)")
        }
        XCTAssertEqual(accounts.first?.ops.first?.status, "failed")
    }

    func testLoadEmptyAndUnavailable() async {
        let empty = makeVM(fetch: { accountOps([]) })
        empty.load()
        for _ in 0..<10 { await Task.yield() }
        if case .empty = empty.loadState {} else {
            XCTFail("Expected .empty; got \(empty.loadState)")
        }

        let failing = makeVM(fetch: { throw CastellumError.internalError })
        failing.load()
        for _ in 0..<10 { await Task.yield() }
        if case .unavailable = failing.loadState {} else {
            XCTFail("Expected .unavailable; got \(failing.loadState)")
        }
    }

    func testRetryInvokesCoreKicksReplayAndReloads() async {
        var retried: [Int64] = []
        var kicked = 0
        var cancelRefreshes = 0
        var fetches = 0
        let vm = makeVM(
            fetch: {
                fetches += 1
                return accountOps([op(id: 7, status: "failed_permanent", attempts: 8)])
            },
            retry: { retried.append($0) },
            afterRetry: { kicked += 1 },
            afterCancel: { cancelRefreshes += 1 }
        )
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.retry(7)
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(retried, [7])
        XCTAssertEqual(kicked, 1, "a successful retry must kick the replayer exactly once")
        XCTAssertEqual(cancelRefreshes, 0, "retry uses the retry callback, not the cancel one")
        XCTAssertEqual(fetches, 2, "the list reloads after the action")
        XCTAssertNil(vm.actionError)
    }

    func testCancelNeverKicksReplayAndRefreshesBadge() async {
        var cancelled: [Int64] = []
        var kickedReplay = 0
        var cancelRefreshes = 0
        let vm = makeVM(
            fetch: { accountOps([op(id: 5, status: "queued")]) },
            cancel: { cancelled.append($0) },
            afterRetry: { kickedReplay += 1 },
            afterCancel: { cancelRefreshes += 1 }
        )
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.cancel(5)
        for _ in 0..<10 { await Task.yield() }
        XCTAssertEqual(cancelled, [5])
        XCTAssertEqual(kickedReplay, 0, "cancelling must never start a replay pass")
        XCTAssertEqual(cancelRefreshes, 1, "cancelling refreshes the outbox badge")
        XCTAssertNil(vm.actionError)
    }

    func testCancelAllNeverKicksReplayAndRefreshesOnce() async {
        var cancelled: [Int64] = []
        var kickedReplay = 0
        var cancelRefreshes = 0
        let vm = makeVM(
            fetch: {
                accountOps([
                    op(id: 1, status: "queued"),
                    op(id: 3, status: "failed"),
                    op(id: 4, status: "failed_permanent"),
                ])
            },
            cancel: { cancelled.append($0) },
            afterRetry: { kickedReplay += 1 },
            afterCancel: { cancelRefreshes += 1 }
        )
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.cancelAllCancellable()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(cancelled.sorted(), [1, 3, 4])
        XCTAssertEqual(kickedReplay, 0, "Cancel All must never start a replay pass")
        XCTAssertEqual(cancelRefreshes, 1, "Cancel All refreshes the badge exactly once")
    }

    func testCancelErrorSurfacesWithoutCrashing() async {
        let vm = makeVM(
            fetch: { accountOps([op(id: 3, status: "failed")]) },
            cancel: { _ in throw CastellumError.internalError }
        )
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.cancel(3)
        for _ in 0..<10 { await Task.yield() }
        XCTAssertNotNil(vm.actionError)
    }

    func testCancelAllSkipsRunningOps() async {
        var cancelled: [Int64] = []
        let vm = makeVM(
            fetch: {
                accountOps([
                    op(id: 1, status: "queued"),
                    op(id: 2, status: "running"),
                    op(id: 3, status: "failed"),
                    op(id: 4, status: "failed_permanent"),
                ])
            },
            cancel: { cancelled.append($0) }
        )
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.cancelAllCancellable()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(
            cancelled.sorted(), [1, 3, 4],
            "running ops belong to the executor and are never bulk-cancelled"
        )
    }
}

final class OutboxPresentationTests: XCTestCase {
    func testKindLabelsAreHumanised() {
        XCTAssertEqual(outboxKindLabel("send"), "Send message")
        XCTAssertEqual(outboxKindLabel("sent_copy"), "Save copy to Sent")
        XCTAssertEqual(outboxKindLabel("mark_all_read"), "Mark all as read")
        XCTAssertEqual(outboxKindLabel("import_eml"), "Import message")
        // PR #165 round 2: Empty Trash rewrites a pending move-to-Trash into
        // this kind (see castellum-db repo.rs) — it needs its own label,
        // not the generic kind-string fallback.
        XCTAssertEqual(outboxKindLabel("delete_permanent"), "Delete permanently")
        XCTAssertEqual(outboxKindLabel("mystery_kind"), "mystery_kind")
    }

    func testTargetHintPrefersMessageIdAndSurvivesBadJson() {
        XCTAssertEqual(
            outboxTargetHint(payloadJson: #"{"message_id":"acc:INBOX:9","path":"X"}"#),
            "acc:INBOX:9"
        )
        XCTAssertEqual(
            outboxTargetHint(payloadJson: #"{"dest_path":"Sent"}"#),
            "Sent"
        )
        XCTAssertNil(outboxTargetHint(payloadJson: "not json"))
        XCTAssertNil(outboxTargetHint(payloadJson: "{}"))
    }

    func testStatusLabelsAndActionAvailability() {
        XCTAssertEqual(outboxStatusLabel(status: "queued", attempts: 0).0, "Queued")
        XCTAssertEqual(outboxStatusLabel(status: "failed", attempts: 1).0, "Failed — will retry (1 attempt)")
        XCTAssertEqual(outboxStatusLabel(status: "queued", attempts: 1).0, "Queued — retrying (1 attempt)")
        XCTAssertTrue(outboxStatusLabel(status: "failed", attempts: 2).1)
        XCTAssertTrue(
            outboxStatusLabel(status: "failed_permanent", attempts: 8).0
                .contains("Needs attention")
        )
        XCTAssertFalse(outboxStatusLabel(status: "running", attempts: 1).1)

        XCTAssertTrue(outboxCanRetry(status: "failed_permanent"))
        XCTAssertTrue(outboxCanRetry(status: "failed"))
        XCTAssertFalse(outboxCanRetry(status: "queued"))
        XCTAssertFalse(outboxCanRetry(status: "running"))

        XCTAssertTrue(outboxCanCancel(status: "queued"))
        XCTAssertTrue(outboxCanCancel(status: "failed_permanent"))
        XCTAssertFalse(outboxCanCancel(status: "running"))
    }

    ///: an ordinary (not scheduled) send that was in flight at
    /// a crash is held by the core with a "may have been sent" error. It must
    /// read as that, not as the unrelated undo-window hold, and never as a
    /// plain failure with Retry.
    func testHeldInstantSendThatMayHaveBeenSentIsPresentedAsSuch() {
        let inFlight = "may have been sent: the app quit while this message was being delivered"
        let (label, needsAttention) = outboxStatusLabel(status: "held", attempts: 1, lastError: inFlight)
        XCTAssertTrue(label.contains("May have been sent"), label)
        XCTAssertTrue(label.contains("Check Sent"), label)
        XCTAssertTrue(needsAttention)
        XCTAssertTrue(outboxHeldMayHaveBeenSent(status: "held", lastError: inFlight))

        // The S25 undo-window hold keeps its own wording: that one is unsent.
        let undoHold = "held: interrupted before the undo window completed"
        XCTAssertTrue(outboxStatusLabel(status: "held", attempts: 0, lastError: undoHold).0
            .contains("Undo"))
        XCTAssertFalse(outboxHeldMayHaveBeenSent(status: "held", lastError: undoHold))
        XCTAssertFalse(outboxHeldMayHaveBeenSent(status: "held", lastError: nil))
        // The prefix only means something on a held send.
        XCTAssertFalse(outboxHeldMayHaveBeenSent(status: "failed", lastError: inFlight))

        // The user decides: Send (Again) or take it back; no Retry, no Cancel.
        XCTAssertTrue(outboxCanSendNow(status: "held"))
        XCTAssertTrue(outboxCanEditHeld(status: "held"))
        XCTAssertFalse(outboxCanRetry(status: "held"))
        XCTAssertFalse(outboxCanCancel(status: "held"))
        // An ordinary send is never offered Reschedule (not a scheduled row).
        XCTAssertNil(outboxScheduledState(op(id: 1, kind: "send", status: "held")))
    }

    func testToolbarIndicatorHiddenWhenEmptyAndIdle() {
        let state = outboxIndicatorState(pendingCount: 0, isReplaying: false)
        XCTAssertFalse(state.visible, "an empty outbox with no replay shows nothing")
        XCTAssertFalse(state.spinner)
    }

    func testToolbarIndicatorShowsSpinnerOnlyWhileReplaying() {
        let replaying = outboxIndicatorState(pendingCount: 0, isReplaying: true)
        XCTAssertTrue(replaying.visible, "an active replay keeps the indicator visible")
        XCTAssertTrue(replaying.spinner)

        let queuedOnly = outboxIndicatorState(pendingCount: 3, isReplaying: false)
        XCTAssertTrue(queuedOnly.visible)
        XCTAssertFalse(queuedOnly.spinner, "queued ops without a replay show the badge, not a spinner")
    }

    // MARK: Send Later (HP1)

    private func scheduledOp(
        id: Int64 = 7, status: String = "queued", sendAt: Int64? = 1_900_000_000,
        lastError: String? = nil
    ) -> OperationRecord {
        OperationRecord(
            id: id, accountId: "acc", kind: "send", payloadJson: #"{"draft_id":42,"scheduled":true}"#,
            status: status, attempts: 0, lastError: lastError,
            updatedAtEpoch: 1_788_000_000, notBeforeEpoch: sendAt, scheduled: true)
    }

    func testScheduledStatesAreRecognisedAndDistinct() {
        XCTAssertEqual(outboxScheduledState(scheduledOp()), .waiting(sendAt: 1_900_000_000))
        XCTAssertEqual(outboxScheduledState(scheduledOp(status: "held")), .mayHaveBeenSent)
        XCTAssertEqual(outboxScheduledState(scheduledOp(status: "needs_confirmation")),
                       .needsConfirmation(sendAt: 1_900_000_000))
        XCTAssertNil(outboxScheduledState(op(id: 1, kind: "send")), "an ordinary send is not scheduled")
        // The three presentations differ, and the two that need the user are flagged.
        let waiting = outboxScheduledLabel(.waiting(sendAt: 1_900_000_000))
        let maybe = outboxScheduledLabel(.mayHaveBeenSent)
        let confirm = outboxScheduledLabel(.needsConfirmation(sendAt: 1_900_000_000))
        XCTAssertFalse(waiting.1)
        XCTAssertTrue(maybe.1)
        XCTAssertTrue(confirm.1)
        XCTAssertEqual(Set([waiting.0, maybe.0, confirm.0]).count, 3)
        XCTAssertTrue(maybe.0.contains("May have been sent"))
        XCTAssertTrue(confirm.0.contains("Needs confirmation"))
    }

    /// #2808 follow-up: a retrying failed scheduled send reads as a scheduled
    /// send; a failed_permanent one keeps the generic Retry/Cancel row (the
    /// core's Send Now / Back to Drafts refuse it, so they must not be offered).
    func testFailedScheduledSendHasScheduledWordingAndActions() {
        XCTAssertEqual(outboxScheduledState(scheduledOp(status: "failed")), .failed(attempts: 0))
        XCTAssertNil(outboxScheduledState(scheduledOp(status: "failed_permanent")),
                     "gave up: generic row with Retry/Cancel")
        XCTAssertNil(outboxScheduledState(op(id: 1, kind: "send", status: "failed")),
                     "an ordinary failed send keeps Retry/Cancel")
        let a = outboxScheduledLabel(.failed(attempts: 2))
        XCTAssertTrue(a.1)
        XCTAssertTrue(a.0.contains("Scheduled send failed"))
        XCTAssertEqual(outboxScheduledActions(.failed(attempts: 2)),
                       [.sendNow, .reschedule, .backToDrafts])
    }

    func testScheduledActionsPerState() {
        XCTAssertEqual(outboxScheduledActions(.waiting(sendAt: 1)), [.sendNow, .reschedule, .backToDrafts])
        XCTAssertEqual(outboxScheduledActions(.needsConfirmation(sendAt: 1)),
                       [.sendNow, .reschedule, .backToDrafts])
        XCTAssertEqual(outboxScheduledActions(.mayHaveBeenSent), [.sendNow, .backToDrafts],
                       "may-have-been-sent cannot be rescheduled, only decided")
        XCTAssertEqual(outboxScheduledActions(.sending), [])
    }

    @MainActor func testRescheduleBackToDraftsAndSendNowReachTheirClosures() async {
        var rescheduled: [(String, Int64, Date)] = []
        var backed: [(String, Int64)] = []
        var sentNow: [(String, Int64)] = []
        let when = Date(timeIntervalSince1970: 1_950_000_000)
        let vm = OutboxViewModel(
            fetch: { accountOps([self.scheduledOp()]) },
            retryOp: { _ in }, cancelOp: { _ in },
            sendNowOp: { sentNow.append(($0, $1)) },
            rescheduleOp: { rescheduled.append(($0, $1, $2)) },
            backToDraftsOp: { backed.append(($0, $1)) },
            afterRetry: {}, afterCancel: {})
        vm.reschedule(accountId: "acc", draftId: 42, to: when)
        vm.backToDrafts(accountId: "acc", draftId: 42)
        vm.sendNow(accountId: "acc", draftId: 42)
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(rescheduled.count, 1)
        XCTAssertEqual(rescheduled.first?.1, 42)
        XCTAssertEqual(rescheduled.first?.2, when)
        XCTAssertEqual(backed.map(\.1), [42])
        XCTAssertEqual(sentNow.map(\.1), [42])
        XCTAssertNil(vm.actionError)
    }

    @MainActor func testBackToDraftsFailureSurfacesWithoutCrashing() async {
        let vm = OutboxViewModel(
            fetch: { accountOps([self.scheduledOp()]) },
            retryOp: { _ in }, cancelOp: { _ in },
            backToDraftsOp: { _, _ in throw SendLaterTestError.boom },
            afterRetry: {}, afterCancel: {})
        vm.backToDrafts(accountId: "acc", draftId: 42)
        for _ in 0..<20 { await Task.yield() }
        XCTAssertNotNil(vm.actionError)
    }

    @MainActor func testCancelAllLeavesScheduledSendsAlone() async {
        var cancelled: [Int64] = []
        let vm = OutboxViewModel(
            fetch: { accountOps([op(id: 1), self.scheduledOp(id: 7)]) },
            retryOp: { _ in }, cancelOp: { cancelled.append($0) },
            afterRetry: {}, afterCancel: {})
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        vm.cancelAllCancellable()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(cancelled, [1], "a scheduled send goes back to Drafts, it is never silently cancelled")
    }
}
