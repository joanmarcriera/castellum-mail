// Castellum - launch-time profile-open refusals.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `CastellumCore.init(profilePath:)` can fail for many reasons, but two are
// launch-blocking in a way the user can actually act on rather than just a
// crash: the profile's schema is newer than this build knows (a downgrade —
// see `castellum_db::OpenError::SchemaTooNew`), or a migration failed to
// apply (`castellum_db::OpenError::MigrationFailed`). Everything else (a bad
// path, a corrupt file, disk full, ...) stays an unrecoverable `fatalError`
// in `AppModel.init`, unchanged.
//
// Classified the same way `AccountWritePolicy.isEvaluationModeRefusal`
// classifies a write refusal: the Rust core prefixes its already-composed,
// non-technical message with a stable marker (`castellum_db::
// SCHEMA_TOO_NEW_PREFIX` / `MIGRATION_FAILED_PREFIX`, pinned by
// `open_error_prefixes_stay_stable_for_the_swift_classifier`), and this type
// only strips that marker before showing the rest — it does not re-compose
// the sentence, per the C ABI rule that Rust owns strings.

import Foundation

/// One of the two profile-open failures the launch path can present as a
/// clear alert instead of crashing.
enum CastellumLaunchError: Equatable {
    case schemaTooNew(message: String)
    case migrationFailed(message: String)

    /// Must match `castellum_db::SCHEMA_TOO_NEW_PREFIX` verbatim.
    static let schemaTooNewPrefix = "schema too new: "
    /// Must match `castellum_db::MIGRATION_FAILED_PREFIX` verbatim.
    static let migrationFailedPrefix = "migration failed: "

    /// Classify an error thrown by `CastellumCore.init(profilePath:)`.
    /// `nil` for anything else — including a plain `.io`, which is not
    /// launch-recoverable and must still reach `AppModel`'s `fatalError`.
    static func classify(_ error: Error) -> CastellumLaunchError? {
        guard case .message(let reason)? = error as? CastellumError else { return nil }
        if reason.hasPrefix(schemaTooNewPrefix) {
            return .schemaTooNew(message: String(reason.dropFirst(schemaTooNewPrefix.count)))
        }
        if reason.hasPrefix(migrationFailedPrefix) {
            return .migrationFailed(message: String(reason.dropFirst(migrationFailedPrefix.count)))
        }
        return nil
    }

    /// Alert title (British English, matching `AccountWritePolicy`'s copy).
    var title: String {
        switch self {
        case .schemaTooNew:
            return String(localized: "This Profile Needs a Newer Version of Castellum")
        case .migrationFailed:
            return String(localized: "Castellum Couldn't Upgrade Your Mail Database")
        }
    }

    /// The non-technical detail sentence — already fully composed by the
    /// Rust core (found/supported schema numbers, or the backup path), with
    /// only its classification marker stripped.
    var message: String {
        switch self {
        case .schemaTooNew(let message), .migrationFailed(let message):
            return message
        }
    }
}
