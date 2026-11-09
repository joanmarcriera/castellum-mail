// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// OB1: the Outbox's Messages rows, Send Now / Send All, the launch and
// network-return triggers, and the per-account SMTP gate. Pure: no core, no
// network, no Keychain.

import XCTest
@testable import Castellum

private func send(
    draftId: Int64 = 7,
    operationId: Int64? = 70,
    status: String = "queued",
    attempts: Int64 = 0,
    errorKind: String? = nil,
    automaticRetry: Bool = true,
    scheduled: Bool = false,
    subject: String = "Quarterly figures",
    recipients: [String] = ["bob@example.test"]
) -> OutboxSend {
    OutboxSend(
        draftId: draftId, accountId: "acc", operationId: operationId,
        subject: subject, recipients: recipients, status: status,
        attempts: attempts, errorKind: errorKind, automaticRetry: automaticRetry,
        nextAttemptEpoch: nil, scheduled: scheduled, updatedAtEpoch: 1_788_000_000)
}

private func sendOp(id: Int64, draftId: Int64, status: String = "queued") -> OperationRecord {
    OperationRecord(
        id: id, accountId: "acc", kind: "send",
        payloadJson: #"{"draft_id":\#(draftId)}"#, status: status, attempts: 0,
        lastError: nil, updatedAtEpoch: 1_788_000_000, notBeforeEpoch: nil)
}

@MainActor
final class OutboxSendsTests: XCTestCase {

    // MARK: decoding the core's JSON

    func testDecodesTheCoreOutboxJSON() throws {
        let json = #"""
        [{"draft_id":3,"account_id":"acc","operation_id":9,"subject":"Hi",
          "recipients":["b@x.test"],"status":"failed_permanent","attempts":1,
          "error_kind":"auth","automatic_retry":false,"next_attempt_epoch":null,
          "scheduled":false,"updated_at_epoch":1788000000}]
        """#
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let rows = try decoder.decode([OutboxSend].self, from: Data(json.utf8))
        XCTAssertEqual(rows.first?.errorKind, "auth")
        XCTAssertEqual(rows.first?.automaticRetry, false)
        XCTAssertEqual(rows.first?.operationId, 9)
    }

    func testSendReportDecodesBlockedAndDefaultsItToZero() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let withBlocked = try decoder.decode(
            SendReport.self, from: Data(#"{"account_id":"a","sent":0,"failed":0,"blocked":1}"#.utf8))
        XCTAssertEqual(withBlocked.blocked, 1)
        let old = try decoder.decode(
            SendReport.self, from: Data(#"{"account_id":"a","sent":1,"failed":0}"#.utf8))
        XCTAssertEqual(old.blocked, 0)
    }

    // MARK: presentation

    func testQueuedFailedAndStrandedSendsAllGetAMessagesRow() {
        XCTAssertTrue(OutboxSendPresentation.listsAsMessage(send(status: "queued")))
        XCTAssertTrue(OutboxSendPresentation.listsAsMessage(send(status: "running")))
        XCTAssertTrue(OutboxSendPresentation.listsAsMessage(send(status: "failed")))
        XCTAssertTrue(OutboxSendPresentation.listsAsMessage(send(status: "failed_permanent")))
        XCTAssertTrue(OutboxSendPresentation.listsAsMessage(send(operationId: nil, status: "stranded")))
        // Send Later rows keep their own decisions (Reschedule, Send Again…).
        XCTAssertFalse(OutboxSendPresentation.listsAsMessage(send(status: "queued", scheduled: true)))
        XCTAssertFalse(OutboxSendPresentation.listsAsMessage(send(status: "held")))
        XCTAssertFalse(OutboxSendPresentation.listsAsMessage(send(status: "needs_confirmation")))
    }

    func testAFailureReadsInPlainWordsWithNoServerText() {
        let failed = send(status: "failed", attempts: 2, errorKind: "network")
        XCTAssertEqual(
            OutboxSendPresentation.reason(failed, endpoint: "smtp.example.test:587"),
            "Couldn’t reach smtp.example.test:587 — the connection failed.")
        XCTAssertEqual(
            OutboxSendPresentation.statusLine(failed),
            "Not sent — Castellum will try again automatically.")
        XCTAssertNil(OutboxSendPresentation.advice(failed), "it retries by itself")
        XCTAssertTrue(OutboxSendPresentation.needsAttention(failed))
        XCTAssertNil(OutboxSendPresentation.reason(send(), endpoint: nil), "never failed: no reason")
    }

    func testAPermanentFailureSaysWhatToDoAndStillOffersSendNow() {
        let refused = send(status: "failed_permanent", attempts: 1, errorKind: "auth", automaticRetry: false)
        XCTAssertEqual(OutboxSendPresentation.statusLine(refused), "Not sent — Castellum has stopped trying.")
        XCTAssertEqual(
            OutboxSendPresentation.reason(refused, endpoint: nil),
            "Sign-in to the SMTP server failed — check the SMTP username or password.")
        XCTAssertEqual(
            OutboxSendPresentation.advice(refused),
            "Check the account’s sign-in in Settings › Accounts, then choose Send Now.")
        XCTAssertTrue(OutboxSendPresentation.canSendNow(refused))
        XCTAssertFalse(OutboxSendPresentation.canSendNow(send(status: "running")), "in flight")
    }

    func testRowTextAndTheSidebarCount() {
        XCTAssertEqual(OutboxSendPresentation.subject(send(subject: "  ")), "(No subject)")
        XCTAssertEqual(
            OutboxSendPresentation.recipients(send(recipients: ["a@x.test", "b@x.test"])),
            "To: a@x.test, b@x.test")
        XCTAssertEqual(OutboxSendPresentation.count([
            send(draftId: 1), send(draftId: 2, status: "failed"),
            send(draftId: 3, status: "queued", scheduled: true),
        ]), 2)
    }

    // MARK: the view model

    func testAQueuedSendIsListedOnceAsAMessageNotTwice() async {
        let vm = OutboxViewModel(
            fetch: {
                [OutboxViewModel.AccountOps(
                    id: "acc", email: "a@x.test",
                    ops: [sendOp(id: 70, draftId: 7)],
                    sends: [send(draftId: 7, operationId: 70)])]
            },
            retryOp: { _ in }, cancelOp: { _ in }, afterRetry: {}, afterCancel: {})
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded(let accounts) = vm.loadState else {
            return XCTFail("Expected .loaded; got \(vm.loadState)")
        }
        XCTAssertEqual(accounts.first?.messageRows.map(\.draftId), [7])
        XCTAssertEqual(accounts.first?.operationRows.count, 0, "its send op is not listed again")
        XCTAssertTrue(vm.hasMessages)
    }

    func testAStrandedSendAloneStillMakesTheOutboxNonEmpty() async {
        let vm = OutboxViewModel(
            fetch: {
                [OutboxViewModel.AccountOps(
                    id: "acc", email: "a@x.test", ops: [],
                    sends: [send(operationId: nil, status: "stranded")])]
            },
            retryOp: { _ in }, cancelOp: { _ in }, afterRetry: {}, afterCancel: {})
        vm.load()
        for _ in 0..<10 { await Task.yield() }
        guard case .loaded = vm.loadState else {
            return XCTFail("a stranded message must not read as an empty Outbox")
        }
    }

    func testSendNowAttemptsExactlyThatMessage() async {
        var attempted: [(String, Int64)] = []
        let vm = OutboxViewModel(
            fetch: { [OutboxViewModel.AccountOps(
                id: "acc", email: "a@x.test", ops: [],
                sends: [send(draftId: 7), send(draftId: 8)])] },
            retryOp: { _ in }, cancelOp: { _ in },
            sendMessageNowOp: { account, draft in
                attempted.append((account, draft))
                return SendReport(accountId: account, sent: 1, failed: 0)
            },
            afterRetry: {}, afterCancel: {})
        vm.sendMessageNow(accountId: "acc", draftId: 8)
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(attempted.map(\.1), [8])
        XCTAssertEqual(attempted.map(\.0), ["acc"])
        XCTAssertEqual(vm.actionNotice, "Sent 1 message(s).")
        XCTAssertTrue(vm.sendingDraftIds.isEmpty, "the row spinner clears")
    }

    func testSendAllReportsFailuresInPlainWords() async {
        var calls = 0
        let vm = OutboxViewModel(
            fetch: { [] }, retryOp: { _ in }, cancelOp: { _ in },
            sendAllOp: {
                calls += 1
                return [.success(SendReport(accountId: "acc", sent: 1, failed: 1))]
            },
            afterRetry: {}, afterCancel: {})
        vm.sendAll()
        for _ in 0..<20 { await Task.yield() }
        XCTAssertEqual(calls, 1)
        XCTAssertEqual(vm.actionNotice, "Sent 1 message(s); 1 could not be sent.")
        XCTAssertFalse(vm.isSendingAll)
    }

    // MARK: automatic attempts

    func testLaunchAttemptsQueuedSendsOncePerAccount() {
        var drained: [String] = []
        let auto = OutboxAutoSender { drained.append($0) }
        auto.accountsLoaded(["a", "b"])
        // reloadAccounts runs again (a sync finished, an account was edited).
        auto.accountsLoaded(["a", "b"])
        XCTAssertEqual(drained, ["a", "b"], "once per account per launch")
        // An account added later gets its own launch attempt.
        auto.accountsLoaded(["a", "b", "c"])
        XCTAssertEqual(drained, ["a", "b", "c"])
    }

    func testNetworkReturnDrainsEveryKnownAccount() {
        var drained: [String] = []
        let auto = OutboxAutoSender { drained.append($0) }
        auto.accountsLoaded(["a"])
        drained.removeAll()
        auto.networkChanged(isOnline: true)    // first report at launch: no drain
        XCTAssertEqual(drained, [])
        auto.networkChanged(isOnline: false)
        auto.networkChanged(isOnline: true)    // came back
        auto.networkChanged(isOnline: true)    // still online: nothing new
        XCTAssertEqual(drained, ["a"])
    }

    // MARK: one SMTP pass per account

    func testTheGateRunsOnePassPerAccountAtATime() async {
        let gate = AccountSerialGate()
        await gate.acquire("acc")
        var secondRan = false
        let second = Task { @MainActor in
            await gate.acquire("acc")
            secondRan = true
            gate.release("acc")
        }
        for _ in 0..<10 { await Task.yield() }
        XCTAssertFalse(secondRan, "waits for the first pass")
        await gate.acquire("other")             // other accounts are not held up
        gate.release("other")
        gate.release("acc")
        await second.value
        XCTAssertTrue(secondRan)
        XCTAssertFalse(gate.isBusy("acc"))
    }

    // MARK: Send Later sweep (#3453)

    private func scheduledOp(status: String, notBefore: Int64?) -> OperationRecord {
        OperationRecord(
            id: 1, accountId: "acc", kind: "send", payloadJson: #"{"draft_id":1}"#,
            status: status, attempts: 1, lastError: nil, updatedAtEpoch: 0,
            notBeforeEpoch: notBefore, scheduled: true)
    }

    func testSweepPicksUpADueQueuedScheduledSend() {
        XCTAssertTrue(AppModel.sendLaterSweepIsDue([scheduledOp(status: "queued", notBefore: 100)], now: 200))
        XCTAssertFalse(AppModel.sendLaterSweepIsDue([scheduledOp(status: "queued", notBefore: 300)], now: 200))
    }

    func testSweepRetriesAFailedScheduledSend() {
        // The first attempt failed (non-permanent): the sweep must drain again so the
        // core can retry once the backoff has elapsed, instead of waiting for a relaunch.
        XCTAssertTrue(AppModel.sendLaterSweepIsDue([scheduledOp(status: "failed", notBefore: 100)], now: 200))
    }

    func testSweepNeverTouchesPermanentHeldOrUnscheduledSends() {
        for st in ["failed_permanent", "needs_confirmation", "held", "running"] {
            XCTAssertFalse(AppModel.sendLaterSweepIsDue([scheduledOp(status: st, notBefore: 100)], now: 200), st)
        }
        XCTAssertFalse(AppModel.sendLaterSweepIsDue([sendOp(id: 1, draftId: 1, status: "failed")], now: 200))
    }
}
