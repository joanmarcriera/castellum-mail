// Castellum - Re-authenticate sheet: masked stored password + inline re-test
//.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure, unit-tested state behind the Re-authenticate sheet's password row:
//   * `ReauthPasswordField` — the password prefilled from the stored config,
//     masked by default, with a reveal toggle. `remask()` is called when the
//     window deactivates, `close()` when the sheet goes away (the value is
//     dropped from view state). Its description, debug description and
//     reflection all REDACT the value, so an accidental `print`, `dump`,
//     string interpolation or os_log of the view state cannot leak it.
//   * `ReauthTestOutcome` — maps the shared connection probe's result (PR
//     #163, `AppModel.testConnection`) to the lines shown next to the field.
//     Wording comes from `ConnectionTestPresentation`, which classifies on
//     the redacted error kind only — never server text or the password.

import Foundation

struct ReauthPasswordField: CustomStringConvertible, CustomDebugStringConvertible, CustomReflectable {
    /// The password text bound to the field. Never included in any
    /// description or reflection of this struct.
    var value: String
    /// False (masked) by default; true only after an explicit reveal.
    private(set) var isRevealed = false

    /// Prefills from an account's stored config JSON (`AppModel.storedConfig`).
    /// A missing or undecodable config leaves the field blank, so the user
    /// simply types the password as before.
    init(storedConfigJSON: String?) {
        if let json = storedConfigJSON, let config = try? AccountConfig.decode(fromJSON: json) {
            value = config.password
        } else {
            value = ""
        }
    }

    mutating func toggleReveal() { isRevealed.toggle() }

    /// Window deactivated / app resigned active: mask again, keep the text.
    mutating func remask() { isRevealed = false }

    /// Sheet closed: forget the password and mask.
    mutating func close() {
        value = ""
        isRevealed = false
    }

    var description: String { "ReauthPasswordField(<redacted>, revealed: \(isRevealed))" }
    var debugDescription: String { description }
    var customMirror: Mirror { Mirror(self, children: ["revealed": isRevealed]) }
}

/// Which leg of the shared probe an inline Test button reports on. One probe
/// round trip always checks both servers (`castellum_test_connection`; the
/// SMTP leg is a login only — nothing is ever sent); each button just shows
/// its own server's verdict next to its own password field (GUI-5).
enum ReauthLeg: Equatable {
    case both, imap, smtp
}

/// What the inline Test button shows next to the password field.
struct ReauthTestOutcome: Equatable {
    let succeeded: Bool
    let lines: [ConnectionTestLine]

    /// The outcome to show/act on for the CURRENT fields: nil when the
    /// report tested different connection settings (the user edited a field
    /// since), so a stale Works/Failed or a stale warning never lingers.
    static func matching(
        report: ConnectionTestReport, tested: AccountConfig, current: AccountConfig, leg: ReauthLeg = .both
    ) -> ReauthTestOutcome? {
        guard tested.connectionSettings == current.connectionSettings else { return nil }
        return map(.success(report), config: tested, leg: leg)
    }

    /// Maps a probe result. A thrown error becomes one neutral line built
    /// from the error's own description (the probe never carries credentials).
    static func map(
        _ result: Result<ConnectionTestReport, Error>, config: AccountConfig, leg: ReauthLeg = .both
    ) -> ReauthTestOutcome {
        switch result {
        case .success(let report):
            let all = ConnectionTestPresentation.lines(for: report, config: config)
            switch leg {
            case .both:
                return ReauthTestOutcome(succeeded: ConnectionTestPresentation.allPassed(report), lines: all)
            case .imap:
                return ReauthTestOutcome(succeeded: report.imap.ok, lines: Array(all.prefix(1)))
            case .smtp:
                // `lines` appends the SMTP line last, and only when tested.
                guard let smtp = report.smtp else {
                    return ReauthTestOutcome(
                        succeeded: false,
                        lines: [ConnectionTestLine(ok: false, text: "No SMTP settings were tested.")])
                }
                return ReauthTestOutcome(succeeded: smtp.ok, lines: Array(all.suffix(1)))
            }
        case .failure(let error):
            return ReauthTestOutcome(
                succeeded: false,
                lines: [ConnectionTestLine(ok: false, text: "The connection test couldn’t run: \(error.localizedDescription)")])
        }
    }
}

/// What the Re-authenticate sheet's Save button does.
/// Save persists the credentials WITHOUT requiring a passing test, so a
/// wrong-password account can still be saved and fixed later; the only
/// guard is a confirmation when the most recent test is known to have failed.
enum ReauthSaveDecision: Equatable {
    case saveNow
    case confirmFailedTest

    /// `lastTest` is the outcome of the sheet's latest probe against the
    /// CURRENT fields (nil = never tested, still testing, or fields edited
    /// since — nothing known, so no warning).
    static func decide(lastTest: ReauthTestOutcome?) -> ReauthSaveDecision {
        guard let lastTest, !lastTest.succeeded else { return .saveNow }
        return .confirmFailedTest
    }
}

/// Pure enable/visibility rules for the account sheet's footer buttons, kept
/// out of the view so they are unit-testable.
enum AccountSheetButtons {
    /// "Save anyway" is the Add-mode escape hatch after a failed sync. In
    /// Re-authenticate mode the dedicated "Save" already does the same thing
    /// (with a failed-test confirmation), so a second button is noise.
    static func showsSaveAnyway(isFailure: Bool, isReauth: Bool) -> Bool {
        isFailure && !isReauth
    }

    /// Re-authenticate "Save": off while incomplete, busy, or in the brief
    /// `.success` window (a save then would race the sheet closing).
    /// While connecting, Save and Test Connection are disabled anyway; leaving them out
    /// gives "Continue in Background" and the spinner room at the fixed sheet width,
    /// where they used to truncate to "Continue in..." (RP12).
    static func footerShowsEditActions(isConnecting: Bool) -> Bool { !isConnecting }

    static func canSave(fieldsComplete: Bool, isConnecting: Bool, isSuccess: Bool, isTesting: Bool) -> Bool {
        fieldsComplete && !isConnecting && !isSuccess && !isTesting
    }
}
