// Castellum - pure scheduling decision for AppModel's periodic background
// re-sync loop. Kept separate from AppModel so the interval /
// backoff / in-flight-skip rules are unit-testable without a live core,
// Keychain, or a real timer — mirrors `UpdateChecker.shouldPerformAutomaticCheck`.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum PeriodicSyncScheduler {
    /// Re-sync a healthy (or never-attempted) account this often.
    static let normalInterval: TimeInterval = 5 * 60
    /// Back off to this interval while the account's last sync attempt failed —
    /// hammering a server or network that just told us it's unreachable every
    /// 5 minutes helps no one.
    static let backoffInterval: TimeInterval = 15 * 60
    /// While IDLE is actively watching this account (S31), the periodic loop
    /// only needs to be a SAFETY NET (a dropped notification, a missed
    /// nudge) — same cadence as the failure backoff, since both mean "don't
    /// hammer, but don't go silent for a long stretch either."
    static let idleSafetyNetInterval: TimeInterval = 15 * 60

    /// Everything the due-check needs for one account, snapshotted by the
    /// caller from `AppModel`'s in-memory state (no I/O here).
    struct AccountState: Equatable {
        /// When this account's periodic loop last attempted a sync (nil = never).
        var lastAttempt: Date?
        /// True when the account's most recent sync outcome was a failure of
        /// any kind (`AccountSyncState.isFailure`) — network, auth, TLS, etc.
        /// The 15-minute backoff applies uniformly rather than only for the
        /// narrower `.unreachable` case, since none of those are worth
        /// retrying every 5 minutes while they persist.
        var isUnreachable: Bool
        /// True while a sync for this account is already in flight
        /// (`accountSyncStatuses[id]?.state == .syncing`, or any `performSync`
        /// pass still running for it -- S72: a superseded attempt keeps running
        /// after a newer one has already published its status). Always skipped —
        /// the periodic loop must never queue a second sync on top of one
        /// still running.
        var isSyncing: Bool
        /// True while this account has a running IDLE thread (S31). Does not
        /// stop the periodic loop outright — see `idleSafetyNetInterval` —
        /// because a silently-dropped IDLE notification must never mean the
        /// account goes stale forever.
        var idleActive: Bool
    }

    /// Is this account due for a periodic re-sync at `now`?
    static func isDue(_ state: AccountState, now: Date) -> Bool {
        guard !state.isSyncing else { return false }
        guard let last = state.lastAttempt else { return true }
        let interval: TimeInterval
        if state.isUnreachable {
            interval = backoffInterval
        } else if state.idleActive {
            interval = idleSafetyNetInterval
        } else {
            interval = normalInterval
        }
        return now.timeIntervalSince(last) >= interval
    }
}
