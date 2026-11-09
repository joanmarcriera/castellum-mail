// Castellum - the Dock icon's unread badge (S59, scoped in MM-9).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The label is derived from numbers AppModel already holds (the per-mailbox
// unread counts, the same sum `UnifiedInboxBadge` shows in the sidebar; a smart
// mailbox's `cachedUnread`, the number its sidebar row shows), or from one FFI
// count in VIP mode. It honours the Focus filter (allowed accounts,
// "Show Dock badge") and the per-account "count in badges" flag. The scope is
// Inbox unread, VIP unread or one smart mailbox (`Preferences.dockScope`).
// Tests never touch the real Dock tile: under `TestHost.isActive` the default
// `setLabel` is a no-op.
//
// `CounterFeed` is the shared engine (also used by `MenuBarCounterController`):
// it watches the model and preferences, debounces, fetches the VIP count when
// that scope needs it, and hands the finished text to a sink.

import AppKit
import Combine
import Foundation

/// Pure: the Dock label for a legacy mode and the numbers already in AppModel
/// (kept for the S59 call sites; new code uses `CounterLabel.text`).
enum DockBadge {
    static let cap: Int64 = CounterLabel.cap

    static func label(
        mode: DockBadgeMode, focus: FocusFilterState,
        inboxUnreadByAccount: [String: Int64], vipUnread: Int64?
    ) -> String? {
        CounterLabel.text(
            scope: CounterScope(mode: mode, smartMailboxId: nil), focus: focus,
            inboxUnreadByAccount: inboxUnreadByAccount, vipUnread: vipUnread, smartMailboxes: [])
    }
}

extension UnifiedInboxBadge {
    /// Per-account form of `unreadCount(role:mailboxes:counts:)`: account id
    /// to the unread total of that account's mailboxes with `role`.
    static func unreadByAccount(
        role: String, mailboxes: [String: [Mailbox]], counts: [String: MailboxCounts]
    ) -> [String: Int64] {
        var out: [String: Int64] = [:]
        for (accountId, boxes) in mailboxes {
            let n = boxes.filter { $0.role == role }
                .reduce(Int64(0)) { $0 + (counts[$1.id]?.unread ?? 0) }
            out[accountId] = n
        }
        return out
    }
}

/// Watches AppModel + Preferences and delivers the counter text for a scope.
/// One instance per counter (Dock, menu bar): independent scopes, one counting path.
@MainActor
final class CounterFeed {
    private unowned let model: AppModel
    private let preferences: Preferences
    private let focusState: () -> FocusFilterState
    private let scope: () -> CounterScope
    private let honoursDockFlag: Bool
    private let deliver: (String?) -> Void
    private var cancellables = Set<AnyCancellable>()
    private var vipUnread: Int64?
    private var vipTask: Task<Void, Never>?

    init(
        model: AppModel, preferences: Preferences,
        focusState: @escaping () -> FocusFilterState,
        debounceMilliseconds: Int, honoursDockFlag: Bool,
        scope: @escaping () -> CounterScope,
        scopePrefs: [AnyPublisher<Void, Never>],
        deliver: @escaping (String?) -> Void
    ) {
        self.model = model
        self.preferences = preferences
        self.focusState = focusState
        self.scope = scope
        self.honoursDockFlag = honoursDockFlag
        self.deliver = deliver

        let interval = RunLoop.SchedulerTimeType.Stride.milliseconds(debounceMilliseconds)
        Publishers.MergeMany([
            model.$counts.map { _ in () }.eraseToAnyPublisher(),
            model.$mailboxes.map { _ in () }.eraseToAnyPublisher(),
            // The smart scope reads the sidebar's cached counts.
            model.$smartMailboxes.map { _ in () }.eraseToAnyPublisher(),
            model.$accounts.map { _ in () }.eraseToAnyPublisher(),
            // VIP scope counts unread mail from VIP senders: adding or removing a
            // VIP changes the count with no sync (#3450). A trigger only;
            // recompute() still fetches the Focus-filtered count itself.
            model.$vips.map { _ in () }.eraseToAnyPublisher(),
            preferences.$notificationAccountRules.map { _ in () }.eraseToAnyPublisher(),
            NotificationCenter.default.publisher(for: .castellumFocusFilterChanged)
                .map { _ in () }.eraseToAnyPublisher(),
        ] + scopePrefs)
        .debounce(for: interval, scheduler: RunLoop.main)
        .sink { [weak self] in self?.recompute() }
        .store(in: &cancellables)
    }

    deinit { vipTask?.cancel() }

    /// Recompute now. Inbox and smart scopes are pure functions of what AppModel
    /// holds; VIP scope fetches the count off-main and applies it when it lands.
    func recompute() {
        let focus = focusState()
        let scope = scope()
        if scope == .vipUnread, !(honoursDockFlag && !focus.showDockBadge) {
            let core = model.core
            let ids = CounterLabel.includedAccountIds(
                allAccountIds: model.accounts.map(\.id), focus: focus, excluded: excludedAccountIds)
            vipTask?.cancel()
            vipTask = Task { [weak self] in
                let n = try? await Task.detached { try core.vipUnreadCount(accountIds: ids) }.value
                guard !Task.isCancelled, let self else { return }
                self.vipUnread = n
                self.apply(scope: scope, focus: focus)
            }
            return
        }
        apply(scope: scope, focus: focus)
    }

    private var excludedAccountIds: Set<String> {
        Set(preferences.notificationAccountRules.filter { !$0.value.countsInCounters }.keys)
    }

    private func apply(scope: CounterScope, focus: FocusFilterState) {
        deliver(CounterLabel.text(
            scope: scope, focus: focus,
            inboxUnreadByAccount: UnifiedInboxBadge.unreadByAccount(
                role: "inbox", mailboxes: model.mailboxes, counts: model.counts),
            vipUnread: vipUnread, smartMailboxes: model.smartMailboxes,
            excludedAccountIds: excludedAccountIds, honoursDockFlag: honoursDockFlag))
    }
}

@MainActor
final class DockBadgeController {
    private let setLabel: (String?) -> Void
    private var feed: CounterFeed?
    /// Last label handed to `setLabel`, so an unchanged badge is not re-set.
    private(set) var currentLabel: String?

    init(
        model: AppModel,
        preferences: Preferences? = nil,
        focusState: @escaping () -> FocusFilterState = { FocusFilterStore.load() },
        debounceMilliseconds: Int = 300,
        setLabel: ((String?) -> Void)? = nil
    ) {
        let prefs = preferences ?? Preferences.shared
        // Never touch the real Dock tile under XCTest.
        self.setLabel = setLabel ?? (TestHost.isActive ? { _ in } : { NSApp.dockTile.badgeLabel = $0 })
        feed = CounterFeed(
            model: model, preferences: prefs, focusState: focusState,
            debounceMilliseconds: debounceMilliseconds, honoursDockFlag: true,
            scope: { prefs.dockScope },
            scopePrefs: [
                prefs.$dockBadgeMode.map { _ in () }.eraseToAnyPublisher(),
                prefs.$dockBadgeSmartMailboxId.map { _ in () }.eraseToAnyPublisher(),
            ],
            deliver: { [weak self] label in self?.apply(label) })
    }

    func recompute() { feed?.recompute() }

    private func apply(_ label: String?) {
        guard label != currentLabel else { return }
        currentLabel = label
        setLabel(label)
    }
}
