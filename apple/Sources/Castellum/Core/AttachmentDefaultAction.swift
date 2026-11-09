// Castellum - default attachment action + bulk-save planning (GUI-6,
//).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure decisions behind AttachmentStripView, kept out of the view so they
// are unit-tested (AttachmentDefaultActionTests):
//   * what clicking a downloaded attachment does — the user's remembered
//     default, or "ask" until they tick "Always do this" once;
//   * which attachments Save All / Save as Zip must still download first.

import Foundation

enum AttachmentDefaultAction: String, CaseIterable, Identifiable, Codable {
    case open, saveAs, quickLook

    var id: String { rawValue }

    var label: String {
        switch self {
        case .open: return String(localized: "Open")
        case .saveAs: return String(localized: "Save As…")
        case .quickLook: return String(localized: "Quick Look")
        }
    }
}

/// What a click on a downloaded attachment should do.
enum AttachmentPrimaryClick: Equatable {
    case perform(AttachmentDefaultAction)
    /// No default chosen yet: ask, offering "Always do this".
    case ask

    static func resolve(preference: AttachmentDefaultAction?) -> AttachmentPrimaryClick {
        preference.map(AttachmentPrimaryClick.perform) ?? .ask
    }
}

/// Bulk-save planning. `Save All` and `Save as Zip` work on every
/// attachment of the message, not only the ones already in the local cache.
enum AttachmentBulkPlan {
    /// The per-attachment status the plan needs (mirrors
    /// `AppModel.AttachmentDownloadState` without its payloads).
    enum Status: Equatable {
        case ready, downloading, checking, idle, failed
    }

    /// Ordinals that still need a download to be started: not local and not
    /// already on their way. `.checking` is left alone (a cache probe is in
    /// flight and will resolve to ready or idle).
    static func ordinalsToDownload(_ statuses: [(ordinal: Int64, status: Status)]) -> [Int64] {
        statuses.filter { $0.status == .idle || $0.status == .failed }.map(\.ordinal)
    }

    /// What to do with attachments sitting `.idle` while the bulk save waits.
    /// An idle attachment we never started is a cache probe that settled to
    /// "not cached" -> start it. An idle attachment we DID start went back
    /// to idle only because the user cancelled it -> the bulk save must stop,
    /// not silently restart the download.
    static func idleDecision(idle: [Int64], started: Set<Int64>) -> (start: [Int64], userCancelled: Bool) {
        if idle.contains(where: { started.contains($0) }) { return ([], true) }
        return (idle, false)
    }

    enum Progress: Equatable {
        case allReady
        case waiting
        case failed(count: Int)
    }

    /// Where a bulk save stands given the current statuses.
    static func progress(_ statuses: [Status]) -> Progress {
        let failed = statuses.filter { $0 == .failed }.count
        if failed > 0 { return .failed(count: failed) }
        return statuses.allSatisfy { $0 == .ready } ? .allReady : .waiting
    }
}
