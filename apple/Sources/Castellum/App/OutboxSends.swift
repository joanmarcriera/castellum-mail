// Castellum - Outbox messages (OB1): how a queued, failed or stranded send is
// presented, when queued sends are attempted by themselves (launch, network
// back), and the per-account gate that keeps two SMTP passes apart.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Rust owns the queue (`castellum_outbox_sends`, `castellum_send_draft_now`,
// `castellum_requeue_failed_sends`); everything here is presentation and
// trigger policy, kept pure where it can be so tests pin it directly.

import Foundation
import Network

// MARK: - Presentation (pure; pinned by OutboxSendsTests)

enum OutboxSendPresentation {
    /// Whether a send gets a Messages row. Scheduled sends that are still
    /// waiting, held (may have been sent) or needing confirmation keep their
    /// existing Send Later rows, which carry their own decisions.
    static func listsAsMessage(_ send: OutboxSend) -> Bool {
        switch send.status {
        case "stranded", "failed", "failed_permanent": return true
        case "queued", "running": return !send.scheduled
        default: return false
        }
    }

    /// Red row: the user should look at it.
    static func needsAttention(_ send: OutboxSend) -> Bool {
        ["failed", "failed_permanent", "stranded"].contains(send.status)
    }

    /// Send Now is offered for everything except a message being delivered
    /// right now (a second submission could send it twice).
    static func canSendNow(_ send: OutboxSend) -> Bool {
        send.status != "running"
    }

    static func subject(_ send: OutboxSend) -> String {
        let trimmed = send.subject.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? String(localized: "(No subject)") : trimmed
    }

    static func recipients(_ send: OutboxSend) -> String {
        send.recipients.isEmpty
            ? String(localized: "No recipients")
            : String(localized: "To: \(send.recipients.joined(separator: ", "))")
    }

    /// One plain line for the send's state.
    static func statusLine(_ send: OutboxSend) -> String {
        switch send.status {
        case "running":
            return String(localized: "Sending…")
        case "failed":
            return String(localized: "Not sent — Castellum will try again automatically.")
        case "failed_permanent":
            return String(localized: "Not sent — Castellum has stopped trying.")
        case "stranded":
            return String(localized: "Not queued — it will not be sent unless you choose Send Now.")
        default:
            return send.attempts > 0
                ? String(localized: "Waiting to send — trying again.")
                : String(localized: "Waiting to send.")
        }
    }

    /// Why the last attempt failed, in plain words, reusing the compose
    /// window's mapping (`SendFailurePresentation`). Only the KIND is ever
    /// read: the core never hands the Outbox server text.
    static func reason(_ send: OutboxSend, endpoint: String?) -> String? {
        guard let kind = send.errorKind else { return nil }
        if kind == "internal" {
            return String(localized: "Castellum couldn’t prepare this message for sending.")
        }
        return SendFailurePresentation.reason(for: "\(kind): ", endpoint: endpoint)
    }

    /// What to do about a failure Castellum will not retry by itself.
    static func advice(_ send: OutboxSend) -> String? {
        guard send.status == "failed_permanent" else { return nil }
        if send.errorKind == "auth" {
            return String(localized: "Check the account’s sign-in in Settings › Accounts, then choose Send Now.")
        }
        return String(localized: "Check the account’s settings in Settings › Accounts, then choose Send Now.")
    }

    /// The sidebar's Outbox count: every message with a Messages row.
    static func count(_ sends: [OutboxSend]) -> Int {
        sends.filter(listsAsMessage).count
    }
}

// MARK: - Automatic attempts

/// When queued sends are attempted without the user asking: once per account
/// per launch (as soon as its id is known) and again for every known account
/// each time the network comes back. The core's backoff decides what is
/// actually due, and a permanent failure is never due, so a trigger here can
/// never hammer a server that refused the sign-in.
@MainActor
final class OutboxAutoSender {
    private let drain: (String) -> Void
    private var attemptedAtLaunch: Set<String> = []
    private var known: [String] = []
    private var wasOnline: Bool?

    init(drain: @escaping (String) -> Void) {
        self.drain = drain
    }

    /// Accounts are known (launch, or an account was added): attempt each
    /// one's queued sends, at most once per launch.
    func accountsLoaded(_ ids: [String]) {
        known = ids
        for id in ids where !attemptedAtLaunch.contains(id) {
            attemptedAtLaunch.insert(id)
            drain(id)
        }
    }

    /// Network path update. Only an offline-to-online transition drains; the
    /// first report (launch) and repeated online reports do not.
    func networkChanged(isOnline: Bool) {
        defer { wasOnline = isOnline }
        guard isOnline, wasOnline == false else { return }
        for id in known { drain(id) }
    }
}

/// Wraps `NWPathMonitor` and reports online/offline on the main actor.
/// Started from `AppModel.bootstrap()` only, never under XCTest.
final class NetworkReachabilityMonitor {
    private let monitor = NWPathMonitor()
    private let queue = DispatchQueue(label: "uk.co.riera.castellum.reachability")

    func start(_ onChange: @escaping @MainActor (Bool) -> Void) {
        monitor.pathUpdateHandler = { path in
            let online = path.status == .satisfied
            Task { @MainActor in onChange(online) }
        }
        monitor.start(queue: queue)
    }

    func stop() { monitor.cancel() }

    deinit { monitor.cancel() }
}

// MARK: - One SMTP pass per account at a time

/// Serialises SMTP work per account: the launch drain, a network-return
/// drain, Send Now and Send All all wait their turn, so the same queued
/// message is never submitted by two passes at once. FIFO per account.
@MainActor
final class AccountSerialGate {
    private var busy: Set<String> = []
    private var waiters: [String: [CheckedContinuation<Void, Never>]] = [:]

    func acquire(_ id: String) async {
        if busy.contains(id) {
            await withCheckedContinuation { waiters[id, default: []].append($0) }
        } else {
            busy.insert(id)
        }
    }

    func release(_ id: String) {
        if var queue = waiters[id], !queue.isEmpty {
            let next = queue.removeFirst()
            waiters[id] = queue.isEmpty ? nil : queue
            next.resume()   // ownership passes straight to the next waiter
        } else {
            busy.remove(id)
        }
    }

    func isBusy(_ id: String) -> Bool { busy.contains(id) }
}
