// Castellum - drains the core's notification outbox and posts banners (S59).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The Rust core decides what is new and writes it to `notification_outbox` at
// the end of every sync pass, including IDLE-triggered ones (no callback
// crosses the FFI: Swift pulls). This loop calls `takePendingNotifications`
// every few seconds (an indexed `WHERE delivered_at IS NULL LIMIT 50`, cheap
// and durable across restarts) and once after every sync, runs the pure
// `NotificationPolicy`, and posts through the `UserNotificationCentre`
// protocol. Delivery is at-most-once. Notification content never leaves this
// Mac. Clicking a banner opens the message; "Mark as Read" goes through the
// automatic mark-read path, so it never hits the evaluation-mode refusal an
// explicit toggle raises.

import AppKit
import Foundation
import UserNotifications

@MainActor
final class NotificationCoordinator: NSObject, UNUserNotificationCenterDelegate {
    private unowned let model: AppModel
    private let centre: UserNotificationCentre
    private let focusState: () -> FocusFilterState
    private let preferences: Preferences
    private let defaults: UserDefaults
    private let interval: UInt64
    private var loop: Task<Void, Never>?
    private var draining = false

    static let authorisationRequestedKey = "notificationsAuthorisationRequested"

    init(
        model: AppModel,
        centre: UserNotificationCentre = NotificationCentreFactory.make(),
        focusState: @escaping () -> FocusFilterState = { FocusFilterStore.load() },
        preferences: Preferences? = nil,
        defaults: UserDefaults = .standard,
        interval: UInt64 = 3_000_000_000
    ) {
        self.model = model
        self.centre = centre
        self.focusState = focusState
        self.preferences = preferences ?? Preferences.shared
        self.defaults = defaults
        self.interval = interval
        super.init()
    }

    deinit { loop?.cancel() }

    /// Start the drain loop. Registers the Mark as Read category and becomes
    /// the system delegate only outside XCTest, so a test run never touches
    /// the real notification centre.
    func start() {
        centre.registerCategories()
        if !TestHost.suppressesLaunchSideEffects {
            UNUserNotificationCenter.current().delegate = self
        }
        loop?.cancel()
        loop = Task { [weak self] in
            while let self, !Task.isCancelled {
                await self.drain()
                try? await Task.sleep(nanoseconds: self.interval)
            }
        }
    }

    func stop() {
        loop?.cancel()
        loop = nil
    }

    /// Last IDLE pass epoch seen by `drain`. Starts at 0, which is also the
    /// core's initial value, so a pass that finished before the first drain
    /// still triggers one reload.
    private var lastIdlePassEpoch: UInt64 = 0

    /// Pure decision: has an IDLE pass finished since `last` was recorded?
    static func shouldReloadForIdlePass(last: UInt64, current: UInt64) -> Bool {
        current != last
    }

    /// One-shot drain, used right after a sync lands (does not wait for the timer).
    func drainSoon() {
        Task { await drain() }
    }

    // MARK: - Draining

    /// Take the pending notices, apply the policy and post. Always consumes
    /// what it takes (a `.off` mode still marks them delivered, so switching
    /// notifications back on never replays old mail).
    func drain() async {
        guard !draining else { return }
        draining = true
        defer { draining = false }
        await ensureAuthorisation()

        let core = model.core
        let pending = (try? await Task.detached { try core.takePendingNotifications(limit: 50) }.value) ?? []
        // An IDLE-thread resync can rewrite rows (hydrated previews and bodies,
        // sort keys) with no arrival, so the outbox is empty and the open list
        // would stay stale until the mailbox is re-selected (#3040). The core
        // counts finished passes; reload when that count moved.
        let epoch = (try? await Task.detached { try core.idlePassEpoch() }.value) ?? lastIdlePassEpoch
        let passChangedRows = Self.shouldReloadForIdlePass(last: lastIdlePassEpoch, current: epoch)
        lastIdlePassEpoch = epoch
        guard !pending.isEmpty else {
            if passChangedRows { model.reloadCurrentMessages() }
            return
        }

        let plans = NotificationPolicy.plan(
            pending: pending,
            mode: preferences.newMailNotifications,
            excludedAccountIds: preferences.notificationsExcludedAccountIds,
            focus: focusState(),
            showPreview: preferences.notificationsShowPreview,
            sound: preferences.notificationsPlaySound)
        for plan in plans { await post(plan) }

        // IDLE-delivered mail changed counts and lists: keep the sidebar and
        // the Dock badge fresh without waiting for a Swift-side sync.
        for accountId in Set(pending.map(\.accountId)) { model.reloadCounts(accountId) }
        model.reloadVips()
        model.reloadCurrentMessages()
    }

    private func post(_ plan: NotificationPlan) async {
        switch plan {
        case .message(let n, let showPreview, let sound):
            let title = (n.fromName?.isEmpty == false ? n.fromName : nil) ?? n.fromAddress ?? "New message"
            let subject = (n.subject?.isEmpty == false ? n.subject : nil) ?? "(no subject)"
            var info = ["messageId": n.messageId, "accountId": n.accountId, "mailboxId": n.mailboxId]
            if let thread = n.threadId { info["threadId"] = thread }
            try? await centre.post(
                id: "msg:" + n.messageId, title: title, subtitle: subject,
                body: showPreview ? n.snippet : nil, threadId: n.threadId,
                userInfo: info, sound: sound)
        case .summary(let accountId, let count, let firstOutboxId, let sound):
            let label = AccountColumn.label(forAccountId: accountId, in: model.accounts)
            try? await centre.post(
                id: "sum:\(accountId):\(firstOutboxId)", title: label,
                subtitle: nil, body: "\(count) new messages", threadId: accountId,
                userInfo: ["accountId": accountId], sound: sound)
        }
    }

    /// Ask for permission once per install, and only once an account exists
    /// (never at a blank first launch) and banners are not switched off.
    func ensureAuthorisation() async {
        guard !defaults.bool(forKey: Self.authorisationRequestedKey),
              !model.accounts.isEmpty,
              preferences.newMailNotifications != .off
        else { return }
        defaults.set(true, forKey: Self.authorisationRequestedKey)
        _ = await centre.requestAuthorisation()
    }

    // MARK: - Banner actions

    /// Default action (a click on the banner): open that message.
    ///
    /// A "N new messages" summary carries only the account: open that account's
    /// inbox, or All Inboxes when it cannot be resolved (removed account,
    /// mailboxes not loaded yet) - it used to do nothing (#3450).
    func handleOpen(userInfo: [String: String]) {
        if let id = userInfo["messageId"] {
            model.revealMessage(id: id)
        } else if let accountId = userInfo["accountId"] {
            if let inbox = model.mailboxes[accountId]?.first(where: { $0.role == "inbox" }) {
                model.select(mailbox: inbox)
            } else {
                model.select(unifiedRole: "inbox")
            }
        }
    }

    /// The "Mark as Read" action.
    func handleMarkRead(userInfo: [String: String]) {
        guard let id = userInfo["messageId"], let accountId = userInfo["accountId"] else { return }
        model.markReadAuto(messageId: id, accountId: accountId)
    }

    /// Whether a banner for `userInfo` should show while the app is running:
    /// not when the app is frontmost with that message's mailbox already open
    /// (the list shows it).
    func shouldPresent(userInfo: [String: String], appIsActive: Bool) -> Bool {
        guard appIsActive, let mailboxId = userInfo["mailboxId"] else { return true }
        return model.selectedMailbox?.id != mailboxId
    }

    // MARK: - UNUserNotificationCenterDelegate

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let info = (response.notification.request.content.userInfo as? [String: String]) ?? [:]
        let action = response.actionIdentifier
        Task { @MainActor in
            if action == NotificationIds.markReadAction {
                self.handleMarkRead(userInfo: info)
            } else if action == UNNotificationDefaultActionIdentifier {
                self.handleOpen(userInfo: info)
            }
            completionHandler()
        }
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        let info = (notification.request.content.userInfo as? [String: String]) ?? [:]
        Task { @MainActor in
            let active = NSApp.isActive
            completionHandler(
                self.shouldPresent(userInfo: info, appIsActive: active)
                    ? [.banner, .sound, .list] : [])
        }
    }
}
