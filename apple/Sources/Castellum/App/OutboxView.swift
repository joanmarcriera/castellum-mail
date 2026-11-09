// Castellum - Outbox window: undelivered write-path operations with manual
// retry/cancel recovery (failed_permanent = "needs attention").
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Combine
import SwiftUI

// MARK: - View model

@MainActor
final class OutboxViewModel: ObservableObject {
    struct AccountOps: Identifiable {
        let id: String
        let email: String
        let ops: [OperationRecord]
        /// DR1: this account's local drafts by id, so a `send` op can name
        /// its message (subject + recipient) instead of just "Send message".
        var drafts: [Int64: Draft] = [:]
        /// OB1: this account's messages waiting in the Outbox (from the core's
        /// `outboxSends`), each shown as a Messages row.
        var sends: [OutboxSend] = []

        /// The sends that get a Messages row.
        var messageRows: [OutboxSend] { sends.filter(OutboxSendPresentation.listsAsMessage) }

        /// Operations still shown as operation rows: a `send` op whose
        /// message already has a Messages row is not listed twice.
        var operationRows: [OperationRecord] {
            outboxOperationRows(ops: ops, messageDraftIds: Set(messageRows.map(\.draftId)))
        }

        var isEmpty: Bool { messageRows.isEmpty && operationRows.isEmpty }
    }

    enum LoadState {
        case loading
        case loaded([AccountOps])
        case empty
        case unavailable(String)
    }

    @Published private(set) var loadState: LoadState = .loading
    /// Last retry/cancel failure, shown inline; cleared by the next action.
    @Published private(set) var actionError: String?
    /// OB1: messages a Send Now is running for (row spinners).
    @Published private(set) var sendingDraftIds: Set<Int64> = []
    /// OB1: Send All is running (toolbar spinner).
    @Published private(set) var isSendingAll = false
    /// OB1: the outcome of the last Send Now / Send All, in plain words.
    @Published private(set) var actionNotice: String?

    private let fetch: () async throws -> [AccountOps]
    private let retryOp: (Int64) async throws -> Void
    private let cancelOp: (Int64) async throws -> Void
    /// S25: "Send now" on a `held` op — `(accountId, draftId)`, draft id not
    /// op id (see `outboxDraftId`); releases the crash-safety hold and drains.
    private let sendNowOp: (String, Int64) async throws -> Void
    /// S25: "Edit" on a `held` op — `(accountId, draftId)`; undoes it (back
    /// to `draft` status) and reopens it in a compose window.
    private let editOp: (String, Int64) async throws -> Void
    /// HP1: "Reschedule" a scheduled send — `(accountId, draftId, newTime)`.
    private let rescheduleOp: (String, Int64, Date) async throws -> Void
    /// HP1: "Cancel" / "Back to Drafts" on a scheduled send —
    /// `(accountId, draftId)`; the draft returns intact, nothing is sent.
    private let backToDraftsOp: (String, Int64) async throws -> Void
    /// OB1: Send Now on one message — `(accountId, draftId)`; attempts exactly
    /// that message immediately.
    private let sendMessageNowOp: (String, Int64) async throws -> SendReport
    /// OB1: Send All — every account with messages waiting.
    private let sendAllOp: () async -> [Result<SendReport, Error>]
    /// Fired after a successful retry so the caller can kick the replayer.
    private let afterRetry: @MainActor () -> Void
    /// Fired after cancellation so the caller can refresh the outbox badge.
    /// Cancelling must never start a replay or any provider network work.
    private let afterCancel: @MainActor () -> Void

    init(
        fetch: @escaping () async throws -> [AccountOps],
        retryOp: @escaping (Int64) async throws -> Void,
        cancelOp: @escaping (Int64) async throws -> Void,
        sendNowOp: @escaping (String, Int64) async throws -> Void = { _, _ in },
        editOp: @escaping (String, Int64) async throws -> Void = { _, _ in },
        rescheduleOp: @escaping (String, Int64, Date) async throws -> Void = { _, _, _ in },
        backToDraftsOp: @escaping (String, Int64) async throws -> Void = { _, _ in },
        sendMessageNowOp: @escaping (String, Int64) async throws -> SendReport = { a, _ in
            SendReport(accountId: a, sent: 0, failed: 0)
        },
        sendAllOp: @escaping () async -> [Result<SendReport, Error>] = { [] },
        afterRetry: @escaping @MainActor () -> Void,
        afterCancel: @escaping @MainActor () -> Void
    ) {
        self.fetch = fetch
        self.retryOp = retryOp
        self.cancelOp = cancelOp
        self.sendNowOp = sendNowOp
        self.editOp = editOp
        self.rescheduleOp = rescheduleOp
        self.backToDraftsOp = backToDraftsOp
        self.sendMessageNowOp = sendMessageNowOp
        self.sendAllOp = sendAllOp
        self.afterRetry = afterRetry
        self.afterCancel = afterCancel
    }

    private var queueSubscription: AnyCancellable?

    /// Reload in place whenever `publisher` emits (the queue was recomputed).
    /// The window is wired to `AppModel.$outboxRevision` this way, so a
    /// background drain or failure refreshes it with no manual call (#3036).
    func observeQueue(_ publisher: AnyPublisher<Int, Never>) {
        queueSubscription = publisher
            .receive(on: DispatchQueue.main)
            .sink { [weak self] _ in self?.queueDidChange() }
    }

    convenience init(model: AppModel) {
        let core = model.core
        self.init(
            fetch: { @MainActor [weak model] in
                guard let model else { return [] }
                let accounts = model.accounts
                return try await Task.detached {
                    try accounts.map { account in
                        AccountOps(
                            id: account.id,
                            email: account.email,
                            ops: try core.pendingOperations(accountId: account.id),
                            drafts: Dictionary(
                                ((try? core.listDrafts(accountId: account.id)) ?? []).map { ($0.id, $0) },
                                uniquingKeysWith: { first, _ in first }),
                            sends: try core.outboxSends(accountId: account.id)
                        )
                    }
                }.value
            },
            retryOp: { id in
                try await Task.detached { try core.retryOperation(id: id) }.value
            },
            cancelOp: { id in
                try await Task.detached { try core.cancelOperation(id: id) }.value
            },
            sendNowOp: { [weak model] accountId, draftId in
                _ = try await Task.detached { try core.sendNow(draftId: draftId) }.value
                await MainActor.run {
                    model?.refreshPendingCount()
                    model?.drainSendPending(accountId: accountId)
                }
            },
            editOp: { [weak model] accountId, draftId in
                let inTime = try await Task.detached { try core.undoSend(draftId: draftId) }.value
                guard inTime else { return }
                await MainActor.run {
                    model?.refreshPendingCount()
                    model?.reopenDraftForEditing(accountId: accountId, draftId: draftId)
                }
            },
            rescheduleOp: { [weak model] accountId, draftId, date in
                try await model?.rescheduleSend(accountId: accountId, draftId: draftId, sendAt: date)
            },
            backToDraftsOp: { [weak model] accountId, draftId in
                try await model?.sendBackToDrafts(accountId: accountId, draftId: draftId)
            },
            sendMessageNowOp: { [weak model] accountId, draftId in
                guard let model else { return SendReport(accountId: accountId, sent: 0, failed: 0) }
                return try await model.sendOutboxMessageNow(accountId: accountId, draftId: draftId)
            },
            sendAllOp: { [weak model] in
                await model?.sendAllOutboxEverywhere() ?? []
            },
            afterRetry: { [weak model] in
                model?.replayNow()
                model?.refreshPendingCount()
            },
            afterCancel: { [weak model] in
                model?.refreshPendingCount()
            }
        )
        observeQueue(model.$outboxRevision.dropFirst().eraseToAnyPublisher())
    }

    func load() {
        loadState = .loading
        Task { await doLoad() }
    }

    /// The queue changed underneath an open window (an op drained, failed or
    /// was queued by the replayer or a send pass): refresh in place. Unlike
    /// `load()` it keeps the current rows on screen (no `.loading` flash) so
    /// completed work stops showing as Queued with a live Cancel (#3036).
    func queueDidChange() {
        Task { await doLoad() }
    }

    private var loadGeneration = 0

    private func doLoad() async {
        loadGeneration &+= 1
        let generation = loadGeneration
        do {
            let all = try await fetch()
            guard generation == loadGeneration else { return }  // a newer load superseded this one
            let nonEmpty = all.filter { !$0.isEmpty }
            loadState = nonEmpty.isEmpty ? .empty : .loaded(nonEmpty)
        } catch {
            loadState = .unavailable(error.localizedDescription)
        }
    }

    func retry(_ id: Int64) {
        Task {
            do {
                actionError = nil
                try await retryOp(id)
                afterRetry()
                await doLoad()
            } catch {
                actionError = "Retry failed: \(error.localizedDescription)"
            }
        }
    }

    func cancel(_ id: Int64) {
        Task {
            do {
                actionError = nil
                try await cancelOp(id)
                afterCancel()
                await doLoad()
            } catch {
                actionError = "Cancel failed: \(error.localizedDescription)"
            }
        }
    }

    /// S25 "Send now" on a `held` op: releases the crash-safety hold.
    func sendNow(accountId: String, draftId: Int64) {
        Task {
            do {
                actionError = nil
                try await sendNowOp(accountId, draftId)
                await doLoad()
            } catch {
                actionError = "Send now failed: \(error.localizedDescription)"
            }
        }
    }

    /// S25 "Edit" on a `held` op: undoes it and reopens the draft.
    func edit(accountId: String, draftId: Int64) {
        Task {
            do {
                actionError = nil
                try await editOp(accountId, draftId)
                await doLoad()
            } catch {
                actionError = "Edit failed: \(error.localizedDescription)"
            }
        }
    }

    /// HP1 "Reschedule" on a scheduled send.
    func reschedule(accountId: String, draftId: Int64, to date: Date) {
        Task {
            do {
                actionError = nil
                try await rescheduleOp(accountId, draftId, date)
                await doLoad()
            } catch {
                actionError = String(localized: "Reschedule failed: \(error.localizedDescription)")
            }
        }
    }

    /// HP1 "Cancel" / "Back to Drafts" on a scheduled send.
    func backToDrafts(accountId: String, draftId: Int64) {
        Task {
            do {
                actionError = nil
                try await backToDraftsOp(accountId, draftId)
                await doLoad()
            } catch {
                actionError = String(localized: "Back to Drafts failed: \(error.localizedDescription)")
            }
        }
    }

    /// OB1 Send Now on one message: attempts exactly that message now.
    func sendMessageNow(accountId: String, draftId: Int64) {
        guard !sendingDraftIds.contains(draftId) else { return }
        sendingDraftIds.insert(draftId)
        Task {
            actionError = nil
            actionNotice = nil
            do {
                let report = try await sendMessageNowOp(accountId, draftId)
                actionNotice = outboxSendNotice([report])
            } catch {
                actionError = String(localized: "Send Now failed: \(error.localizedDescription)")
            }
            sendingDraftIds.remove(draftId)
            await doLoad()
        }
    }

    /// OB1 Send All: every waiting message of every account, now.
    func sendAll() {
        guard !isSendingAll else { return }
        isSendingAll = true
        Task {
            actionError = nil
            actionNotice = nil
            let results = await sendAllOp()
            var reports: [SendReport] = []
            for result in results {
                switch result {
                case .success(let report): reports.append(report)
                case .failure(let error):
                    actionError = actionError
                        ?? String(localized: "Send All failed: \(error.localizedDescription)")
                }
            }
            actionNotice = outboxSendNotice(reports)
            isSendingAll = false
            await doLoad()
        }
    }

    /// Whether there is anything Send All could send.
    var hasMessages: Bool {
        guard case .loaded(let accounts) = loadState else { return false }
        return accounts.contains { !$0.messageRows.isEmpty }
    }

    /// Cancel every listed op that is currently cancellable. Running ops are
    /// refused by the core and simply remain listed.
    func cancelAllCancellable() {
        guard case .loaded(let accounts) = loadState else { return }
        let ids = accounts.flatMap(\.operationRows)
            .filter { outboxCanCancel(status: $0.status) && !$0.isScheduledSend }
            .map(\.id)
        Task {
            actionError = nil
            var firstError: String?
            for id in ids {
                do { try await cancelOp(id) } catch {
                    firstError = firstError ?? error.localizedDescription
                }
            }
            if let firstError { actionError = "Cancel failed: \(firstError)" }
            afterCancel()
            await doLoad()
        }
    }
}

// MARK: - Presentation helpers (pure; pinned by tests)

/// OB1: operation rows minus the `send` ops whose message has a Messages row.
func outboxOperationRows(ops: [OperationRecord], messageDraftIds: Set<Int64>) -> [OperationRecord] {
    ops.filter { op in
        guard op.kind == "send", let draftId = outboxDraftId(payloadJson: op.payloadJson) else { return true }
        return !messageDraftIds.contains(draftId)
    }
}

/// OB1: one plain line summing up a Send Now / Send All pass, or nil when
/// nothing was attempted (a read-only refusal raises its own dialog).
func outboxSendNotice(_ reports: [SendReport]) -> String? {
    let sent = reports.reduce(0) { $0 + $1.sent }
    let failed = reports.reduce(0) { $0 + $1.failed }
    switch (sent, failed) {
    case (0, 0): return nil
    case (_, 0): return String(localized: "Sent \(sent) message(s).")
    case (0, _): return String(localized: "\(failed) message(s) could not be sent — see the reason below each one.")
    default: return String(localized: "Sent \(sent) message(s); \(failed) could not be sent.")
    }
}

func outboxKindLabel(_ kind: String) -> String {
    switch kind {
    case "mark_read": return "Mark read/unread"
    case "mark_all_read": return "Mark all as read"
    case "flag": return "Flag change"
    case "move": return "Move message"
    case "undo_move": return "Undo move"
    case "empty_trash": return "Empty Trash"
    case "delete_permanent": return "Delete permanently"
    case "send": return "Send message"
    case "sent_copy": return "Save copy to Sent"
    case "import_eml": return "Import message"
    default: return kind
    }
}

/// A short target hint pulled from the op payload (message id, mailbox path…).
/// Purely local display — payloads never leave the machine through this view.
func outboxTargetHint(payloadJson: String) -> String? {
    guard let data = payloadJson.data(using: .utf8),
          let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    else { return nil }
    for key in ["message_id", "mailbox_id", "dest_path", "src_path", "path"] {
        if let value = object[key] as? String, !value.isEmpty {
            return value
        }
    }
    return nil
}

/// G9 (#3632): a failed `sent_copy` op means the message itself WAS sent;
/// only its copy in the Sent mailbox is missing. Say so, so the row does
/// not read as an unsent message. nil for every other op.
func outboxSentCopyNote(kind: String, status: String) -> String? {
    guard kind == "sent_copy", status == "failed" || status == "failed_permanent" else { return nil }
    return String(localized: "Your message was sent. Only its copy in the Sent mailbox is missing; Retry saves it there.")
}

/// "1 attempt" / "2 attempts" (RP13).
func outboxAttemptsText(_ attempts: Int64) -> String {
    String(localized: "\(attempts) attempts")
}

/// The core's contract (castellum.h): a `held` send whose `last_error` starts
/// with this was being delivered when the app quit or crashed. Whether it
/// reached the recipients is unknown, so the core never retries it by itself.
let outboxMayHaveBeenSentPrefix = "may have been sent"

/// Whether a held send is the "was in flight at a crash" kind (any send, not
/// only a scheduled one), as opposed to the S25 hold of a send whose undo
/// window was interrupted, which is known to be unsent.
func outboxHeldMayHaveBeenSent(status: String, lastError: String?) -> Bool {
    status == "held" && (lastError?.hasPrefix(outboxMayHaveBeenSentPrefix) ?? false)
}

/// (label, needsAttention) for an op status. `lastError` tells the two kinds
/// of `held` send apart.
func outboxStatusLabel(status: String, attempts: Int64, lastError: String? = nil) -> (String, Bool) {
    if outboxHeldMayHaveBeenSent(status: status, lastError: lastError) {
        return outboxScheduledLabel(.mayHaveBeenSent)
    }
    // MS1: an SMTP-AUTH-disabled refusal is terminal on its first attempt;
    // "gave up after 1 attempts" would hide why. Say what stopped it.
    if status == "failed_permanent", SmtpAuthDisabledGuidance.applies(to: lastError) {
        return (String(localized: "Needs attention — \(SmtpAuthDisabledGuidance.headline)"), true)
    }
    switch status {
    case "queued":
        return attempts > 0 ? ("Queued — retrying (\(outboxAttemptsText(attempts)))", false) : ("Queued", false)
    case "running": return ("Sending…", false)
    case "failed": return ("Failed — will retry (\(outboxAttemptsText(attempts)))", true)
    case "failed_permanent": return ("Needs attention — gave up after \(outboxAttemptsText(attempts))", true)
    // S25 crash-safety hold: a delayed send whose undo window couldn't be
    // proven to have stayed open under a continuously-live app. Not a
    // failure — the message is safe and unsent; it just needs a person.
    case "held": return ("Held — interrupted before Undo's window ended", true)
    default: return (status, false)
    }
}

func outboxCanRetry(status: String) -> Bool {
    status == "failed" || status == "failed_permanent"
}

func outboxCanCancel(status: String) -> Bool {
    status == "queued" || status == "failed" || status == "failed_permanent"
}

/// Only a `held` op (S25 crash-safety hold) offers Send now / Edit.
func outboxCanSendNow(status: String) -> Bool { status == "held" }
func outboxCanEditHeld(status: String) -> Bool { status == "held" }

// MARK: Send Later (HP1)

/// How a scheduled send is presented. `queued` = waiting for its time;
/// `held` = interrupted in flight at a crash (may have been sent);
/// `needs_confirmation` = its time passed while the app was closed by more than
/// the late-send grace. The last two are NEVER sent without the user's choice.
enum OutboxScheduledState: Equatable {
    case waiting(sendAt: Int64)
    case sending
    case mayHaveBeenSent
    case needsConfirmation(sendAt: Int64?)
    /// SMTP failed for a scheduled send that the core is still retrying
    /// (`failed`). A `failed_permanent` one deliberately stays on the generic
    /// Retry/Cancel row: the core's Send Now / Back to Drafts only accept a
    /// `failed` op, so offering them there would silently do nothing.
    case failed(attempts: Int64)
}

func outboxScheduledState(_ op: OperationRecord) -> OutboxScheduledState? {
    guard op.isScheduledSend else { return nil }
    switch op.status {
    case "queued": return op.notBeforeEpoch.map { .waiting(sendAt: $0) }
    case "running": return .sending
    case "held": return .mayHaveBeenSent
    case "needs_confirmation": return .needsConfirmation(sendAt: op.notBeforeEpoch)
    case "failed": return .failed(attempts: op.attempts)
    default: return nil
    }
}

enum OutboxScheduledAction: Equatable {
    case sendNow, reschedule, backToDrafts
}

/// The buttons a scheduled row offers. A may-have-been-sent row cannot be
/// rescheduled: it needs an explicit Send Again / Back to Drafts decision.
func outboxScheduledActions(_ state: OutboxScheduledState) -> [OutboxScheduledAction] {
    switch state {
    case .waiting: return [.sendNow, .reschedule, .backToDrafts]
    case .needsConfirmation: return [.sendNow, .reschedule, .backToDrafts]
    case .mayHaveBeenSent: return [.sendNow, .backToDrafts]
    case .failed: return [.sendNow, .reschedule, .backToDrafts]
    case .sending: return []
    }
}

/// (label, needsAttention) for a scheduled row.
func outboxScheduledLabel(_ state: OutboxScheduledState) -> (String, Bool) {
    switch state {
    case .waiting(let at):
        return (String(localized: "Scheduled for \(sendLaterDisplay(at))"), false)
    case .sending:
        return (String(localized: "Sending…"), false)
    case .mayHaveBeenSent:
        return (String(localized: "May have been sent — it was being delivered when Castellum quit. Check Sent before sending again."), true)
    case .failed(let attempts):
        return (String(localized: "Scheduled send failed — will retry (\(outboxAttemptsText(attempts)))"), true)
    case .needsConfirmation(let at):
        if let at {
            return (String(localized: "Needs confirmation — it was due \(sendLaterDisplay(at)) while Castellum was closed."), true)
        }
        return (String(localized: "Needs confirmation — it was due while Castellum was closed."), true)
    }
}

/// The `draft_id` inside a `send`/`sent_copy` op payload, for the S25
/// Send now / Edit actions (which act on the draft, not the op id).
func outboxDraftId(payloadJson: String) -> Int64? {
    guard let data = payloadJson.data(using: .utf8),
          let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    else { return nil }
    if let n = object["draft_id"] as? NSNumber { return n.int64Value }
    return nil
}

/// Toolbar outbox indicator semantics: hidden when nothing is pending and no
/// replay is in flight; the spinner appears only while a replay actually runs.
func outboxIndicatorState(
    pendingCount: Int, isReplaying: Bool
) -> (visible: Bool, spinner: Bool) {
    (visible: pendingCount > 0 || isReplaying, spinner: isReplaying)
}

// MARK: - View

struct OutboxView: View {
    @StateObject private var vm: OutboxViewModel
    @ObservedObject private var model: AppModel
    @State private var selectedRow: Int64?
    @State private var confirmCancelAll = false

    init(model: AppModel) {
        _vm = StateObject(wrappedValue: OutboxViewModel(model: model))
        _model = ObservedObject(wrappedValue: model)
    }

    var body: some View {
        VStack(spacing: 0) {
            mainContent
            Divider()
            bottomBar
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
        }
        .frame(minWidth: 520, minHeight: 320)
        .task { vm.load() }
        // A Send All from the Message menu: reload once it finishes.
        .onChange(of: model.isSendingOutbox) { sending in
            if !sending { vm.load() }
        }
        // #3036: queue-change reloads are wired in OutboxViewModel.observeQueue
        // (model.$outboxRevision), which fires on every queue recomputation.
    }

    /// OB1 Send All (also in the Message menu, ⇧⌘D).
    private var sendAllButton: some View {
        Button {
            vm.sendAll()
        } label: {
            if vm.isSendingAll || model.isSendingOutbox {
                ProgressView().controlSize(.small)
            } else {
                Label(String(localized: "Send All"), systemImage: "paperplane")
            }
        }
        .disabled(!vm.hasMessages || vm.isSendingAll || model.isSendingOutbox)
        .help(String(localized: "Send every waiting message now (⇧⌘D)"))
        .accessibilityLabel(String(localized: "Send all waiting messages now"))
        .accessibilityIdentifier("outbox.sendAll")
    }

    @ViewBuilder
    private var mainContent: some View {
        switch vm.loadState {
        case .loading:
            VStack(spacing: 12) {
                ProgressView()
                Text("Loading outbox…").foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .empty:
            VStack(spacing: 12) {
                Image(systemName: "tray")
                    .font(.system(size: 32))
                    .foregroundStyle(.tertiary)
                Text("The outbox is empty.")
                    .foregroundStyle(.secondary)
                Text("Pending and failed changes appear here until they reach the server.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .unavailable(let message):
            VStack(spacing: 8) {
                Image(systemName: "exclamationmark.triangle")
                    .font(.system(size: 28))
                    .foregroundStyle(.orange)
                Text("Outbox unavailable").fontWeight(.medium)
                Text(message)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding()
        case .loaded(let accounts):
            List(selection: $selectedRow) {
                ForEach(accounts) { account in
                    if !account.messageRows.isEmpty {
                        Section {
                            ForEach(account.messageRows) { send in
                                OutboxMessageRow(
                                    send: send, vm: vm,
                                    endpoint: model.smtpEndpoint(for: account.id))
                                    .tag(-send.draftId)
                            }
                        } header: {
                            Text(String(localized: "Messages — \(account.email)")).font(.headline)
                        }
                    }
                    if !account.operationRows.isEmpty {
                    Section {
                        ForEach(account.operationRows) { op in
                            OutboxRow(
                                accountId: account.id, op: op, vm: vm,
                                draft: outboxDraftId(payloadJson: op.payloadJson)
                                    .flatMap { account.drafts[$0] })
                                .tag(op.id)
                        }
                    } header: {
                        Text(account.email).font(.headline)
                    }
                    }
                }
            }
            .listStyle(.inset(alternatesRowBackgrounds: true))
        }
    }

    private var bottomBar: some View {
        HStack(spacing: 12) {
            if let error = vm.actionError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .lineLimit(1)
            } else if let notice = vm.actionNotice {
                Label(notice, systemImage: "paperplane")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            if case .loaded = vm.loadState {
                Button("Cancel All…") { confirmCancelAll = true }
                    .accessibilityLabel("Cancel all pending operations")
                    .confirmationDialog(
                        "Cancel all pending changes? They will never be sent to the server.",
                        isPresented: $confirmCancelAll,
                        titleVisibility: .visible
                    ) {
                        Button("Cancel All Operations", role: .destructive) {
                            vm.cancelAllCancellable()
                        }
                        Button("Keep Them", role: .cancel) {}
                    }
            }
            // OB1: Send All lives in the window's own button bar (this window
            // has no toolbar); also in the Message menu with ⇧⌘D.
            sendAllButton
            Button("Refresh") { vm.load() }
                .accessibilityLabel("Reload the outbox")
        }
    }
}

private struct OutboxRow: View {
    let accountId: String
    let op: OperationRecord
    @ObservedObject var vm: OutboxViewModel
    /// DR1: the draft behind a send op, when it is still known locally.
    var draft: Draft?
    @State private var showReschedule = false

    var body: some View {
        let scheduledState = outboxScheduledState(op)
        let (statusText, needsAttention) = scheduledState.map(outboxScheduledLabel)
            ?? outboxStatusLabel(status: op.status, attempts: op.attempts, lastError: op.lastError)
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: rowSymbol(scheduledState, needsAttention: needsAttention))
                .foregroundStyle(needsAttention ? Color.red : Color.secondary)
                .font(.caption)
                .accessibilityIdentifier("outbox.row.\(op.id).state")
            // Scheduled sends read as "Scheduled message", not "Send message".
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(scheduledState == nil
                        ? outboxKindLabel(op.kind) : String(localized: "Scheduled message"))
                        .font(.callout)
                    if let hint = outboxTargetHint(payloadJson: op.payloadJson) {
                        Text(hint)
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                if let draft {
                    let row = DraftListRow(draft: draft)
                    Text(verbatim: String(localized: "\(row.subject) \u{2014} To: \(row.recipient)"))
                        .font(.caption)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Text(statusText)
                    .font(.caption)
                    .foregroundStyle(needsAttention ? Color.red : Color.secondary)
                if let note = outboxSentCopyNote(kind: op.kind, status: op.status) {
                    Text(note)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityIdentifier("outbox.row.\(op.id).sentCopyNote")
                }
                if let error = op.lastError, !error.isEmpty {
                    Text(error)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .lineLimit(2)
                }
                if SmtpAuthDisabledGuidance.applies(to: op.lastError) {
                    SmtpAuthDisabledGuidanceView(showsHeadline: false)
                        .padding(.top, 2)
                        .accessibilityIdentifier("outbox.row.\(op.id).smtpAuthDisabledHelp")
                }
            }
            Spacer()
            if let scheduledState, let draftId = outboxDraftId(payloadJson: op.payloadJson) {
                scheduledButtons(scheduledState, draftId: draftId)
            } else if outboxCanRetry(status: op.status) {
                Button("Retry") { vm.retry(op.id) }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Retry this operation")
            }
            if scheduledState == nil, outboxCanCancel(status: op.status) {
                Button("Cancel") { vm.cancel(op.id) }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Cancel this operation")
            }
            if scheduledState == nil, let draftId = outboxDraftId(payloadJson: op.payloadJson) {
                if outboxCanSendNow(status: op.status) {
                    // "Send Again" when it may already have gone out, so the
                    // choice reads as what it is: a possible second copy.
                    Button(outboxHeldMayHaveBeenSent(status: op.status, lastError: op.lastError)
                        ? "Send Again" : "Send Now") {
                        vm.sendNow(accountId: accountId, draftId: draftId)
                    }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Send this message now")
                }
                if outboxCanEditHeld(status: op.status) {
                    Button("Edit") { vm.edit(accountId: accountId, draftId: draftId) }
                        .buttonStyle(.borderless)
                        .accessibilityLabel("Edit this message before sending")
                }
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(String(localized: "\(outboxKindLabel(op.kind)): \(statusText)"))
    }

    /// A needs-confirmation row has its own symbol so it never reads as a plain
    /// failure or a plain waiting row.
    private func rowSymbol(_ state: OutboxScheduledState?, needsAttention: Bool) -> String {
        switch state {
        case .needsConfirmation: return "questionmark.circle.fill"
        case .waiting: return "calendar.badge.clock"
        default:
            return needsAttention
                ? "exclamationmark.circle.fill"
                : (op.status == "running" ? "arrow.triangle.2.circlepath" : "clock")
        }
    }

    /// Send Now / Reschedule / Cancel (or Back to Drafts) for a scheduled send.
    @ViewBuilder
    private func scheduledButtons(_ state: OutboxScheduledState, draftId: Int64) -> some View {
        ForEach(outboxScheduledActions(state), id: \.self) { action in
            switch action {
            case .sendNow:
                Button(state == .mayHaveBeenSent ? "Send Again" : "Send Now") {
                    vm.sendNow(accountId: accountId, draftId: draftId)
                }
                .buttonStyle(.borderless)
                .accessibilityLabel("Send this message now")
            case .reschedule:
                Button("Reschedule") { showReschedule = true }
                    .buttonStyle(.borderless)
                    .accessibilityLabel("Choose a new time to send this message")
                    .popover(isPresented: $showReschedule) {
                        SendLaterPicker(
                            onPick: { date in
                                showReschedule = false
                                vm.reschedule(accountId: accountId, draftId: draftId, to: date)
                            },
                            onCancel: { showReschedule = false })
                    }
            case .backToDrafts:
                Button(isWaiting(state) ? "Cancel" : "Back to Drafts") {
                    vm.backToDrafts(accountId: accountId, draftId: draftId)
                }
                .buttonStyle(.borderless)
                .accessibilityLabel("Cancel sending and move this message back to Drafts")
            }
        }
    }

    private func isWaiting(_ state: OutboxScheduledState) -> Bool {
        if case .waiting = state { return true }
        return false
    }
}

/// OB1: one message waiting in the Outbox — subject, recipients, its state
/// and, after a failure, the reason in plain words (never server text) and
/// what to do. Send Now attempts exactly this message.
private struct OutboxMessageRow: View {
    let send: OutboxSend
    @ObservedObject var vm: OutboxViewModel
    let endpoint: String?

    var body: some View {
        let attention = OutboxSendPresentation.needsAttention(send)
        let status = OutboxSendPresentation.statusLine(send)
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: attention
                  ? "exclamationmark.circle.fill"
                  : (send.status == "running" ? "arrow.triangle.2.circlepath" : "tray.and.arrow.up"))
                .foregroundStyle(attention ? Color.red : Color.secondary)
                .font(.caption)
                .accessibilityIdentifier("outbox.message.\(send.draftId).state")
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: OutboxSendPresentation.subject(send))
                    .font(.callout)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Text(verbatim: OutboxSendPresentation.recipients(send))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.tail)
                Text(verbatim: status)
                    .font(.caption)
                    .foregroundStyle(attention ? Color.red : Color.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if let reason = OutboxSendPresentation.reason(send, endpoint: endpoint) {
                    Text(verbatim: reason)
                        .font(.caption)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let advice = OutboxSendPresentation.advice(send) {
                    Text(verbatim: advice)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer()
            if vm.sendingDraftIds.contains(send.draftId) {
                ProgressView().controlSize(.small)
                    .accessibilityLabel(String(localized: "Sending…"))
            } else if OutboxSendPresentation.canSendNow(send) {
                Button(String(localized: "Send Now")) {
                    vm.sendMessageNow(accountId: send.accountId, draftId: send.draftId)
                }
                .buttonStyle(.borderless)
                .accessibilityLabel(String(localized: "Send this message now"))
                .accessibilityIdentifier("outbox.message.\(send.draftId).sendNow")
            }
            if let opId = send.operationId, outboxCanCancel(status: send.status) {
                Button(String(localized: "Cancel")) { vm.cancel(opId) }
                    .buttonStyle(.borderless)
                    .accessibilityLabel(String(localized: "Stop sending this message"))
            }
        }
        .accessibilityElement(children: .combine)
        .accessibilityLabel(String(localized: "\(OutboxSendPresentation.subject(send)): \(status)"))
    }
}
