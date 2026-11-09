// Castellum - the always-visible per-account sync line at the bottom of the
// sidebar.
//
// "Is it syncing? is it healthy?" used to be answerable only while a sync
// spun or inside the Status dashboard; nothing said "all is well" between
// syncs. This is the pure presentation: it picks ONE line per account from
// state the app already holds, so redrawing it every few seconds costs no
// server round-trip (Gmail rate-limits; network cadence stays IDLE push +
// the 5 min poll in `PeriodicSyncScheduler`).
//
//   syncing  -> "Syncing INBOX: 1,240 / 78,000"   (SyncProgressPresentation)
//   failure  -> the red one-line error
//   IDLE ok  -> "Live"                            (push connection healthy)
//   else     -> "Checked 8 s ago"                 (polling fallback)
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

struct SyncStatusLine: Equatable {
    enum Kind: Equatable { case syncing, error, live, checked, unknown }

    let kind: Kind
    let text: String
}

enum SyncStatusLineBuilder {
    /// Redraw cadence of the sidebar line. A UI timer only: nothing here or in
    /// the view it drives touches the network.
    static let redrawInterval: TimeInterval = 5

    /// Choose the line for one account.
    /// - Parameters:
    ///   - status: the account's current sync status, if any.
    ///   - progress: the in-process progress snapshot of the running pass.
    ///   - idleHealthy: the core's PROVEN-idling flag (`isIdleRunning`).
    ///   - lastChecked: when the last successful pass finished.
    static func make(
        status: AccountSyncStatus?,
        progress: SyncProgress?,
        idleHealthy: Bool,
        lastChecked: Date?,
        now: Date
    ) -> SyncStatusLine {
        if status?.state == .syncing {
            if let progress, !progress.done {
                let label = SyncProgressPresentation.label(for: progress)
                return SyncStatusLine(kind: .syncing, text: String(localized: "Syncing \(label)"))
            }
            return SyncStatusLine(kind: .syncing, text: String(localized: "Syncing…"))
        }
        if let status, status.state.isFailure {
            return SyncStatusLine(kind: .error, text: status.message)
        }
        if idleHealthy {
            return SyncStatusLine(kind: .live, text: String(localized: "Live"))
        }
        if let lastChecked {
            let age = age(from: lastChecked, to: now)
            return SyncStatusLine(kind: .checked, text: String(localized: "Checked \(age) ago"))
        }
        return SyncStatusLine(kind: .unknown, text: String(localized: "Not checked yet"))
    }

    /// "8 s", "3 min", "2 h", "5 d". Never negative (a clock step backwards
    /// reads as "0 s").
    static func age(from then: Date, to now: Date) -> String {
        let seconds = max(0, Int(now.timeIntervalSince(then)))
        switch seconds {
        case ..<60: return "\(seconds) s"
        case ..<3600: return "\(seconds / 60) min"
        case ..<86_400: return "\(seconds / 3600) h"
        default: return "\(seconds / 86_400) d"
        }
    }

    /// The core's `datetime('now')` text ("2026-10-16 20:47:02", UTC), as seen
    /// in `AccountDiagnostics.lastSuccessAt`. Nil when it is not that shape.
    static func parseCoreTimestamp(_ text: String?) -> Date? {
        guard let text else { return nil }
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = TimeZone(identifier: "UTC")
        formatter.dateFormat = "yyyy-MM-dd HH:mm:ss"
        return formatter.date(from: text)
    }
}
