// Castellum - write-safety UI policy.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Presentation-side counterpart to the core's fail-closed evaluation mode
// (migration 0019, `accounts.write_enabled DEFAULT 0`). The core is the safety
// boundary — nothing here can permit a write the core would refuse. What this
// adds is honesty: the user can SEE that an account is read-only, LEAVE that
// mode only through an explicit confirmation, and get a useful dialog instead
// of a raw error string when a write is refused.
//
// Pure and injectable so every rule below is unit-tested (AccountWritePolicyUITests);
// the views hold no policy of their own.

import Foundation

/// Whether an account's writes reach the mail server.
enum AccountWriteState: Equatable {
    /// Evaluation mode: the core refuses every server mutation. This is also
    /// what an account we have not probed yet reports — fail closed.
    case readOnly
    /// The user has explicitly enabled writes for this account.
    case writesEnabled

    var isReadOnly: Bool { self == .readOnly }
}

enum AccountWritePolicy {

    // MARK: - state

    /// Resolve an account's write state from the probed map.
    ///
    /// FAIL CLOSED, deliberately: a missing entry means "we have not heard back
    /// from the core yet", and showing such an account as writable — even for
    /// the moment before the probe lands — would be the one lie this whole
    /// feature exists to prevent. It mirrors `db::write_enabled_on`, where only
    /// a stored `Some(1)` permits writes.
    static func state(for accountId: String, in probed: [String: Bool]) -> AccountWriteState {
        probed[accountId] == true ? .writesEnabled : .readOnly
    }

    // MARK: - classifying the core's refusal

    /// The stable marker the Rust core prefixes an evaluation-mode refusal with
    /// (`castellum-db` `repo.rs::EVALUATION_MODE_PREFIX`). Pinned on the Rust
    /// side by `evaluation_mode_message_keeps_the_prefix_the_swift_ui_matches`.
    static let evaluationModePrefix = "evaluation mode:"

    /// True when a failed write was refused because the account is read-only,
    /// rather than because something actually went wrong. Matches the core's
    /// leading marker only — a message that merely mentions the words further
    /// in (an IMAP server's own text, say) is a real failure and must surface
    /// as one.
    static func isEvaluationModeRefusal(_ error: Error) -> Bool {
        guard case .message(let reason)? = error as? CastellumError else { return false }
        return reason.hasPrefix(evaluationModePrefix)
    }

    // MARK: - user-facing copy (British English)

    static let badgeLabel = String(localized: "Read-only")

    static func badgeHelp(for email: String) -> String {
        String(localized: "\(email) is in evaluation mode: Castellum makes no changes to the mail server. Click to enable writes.")
    }

    static let writesEnabledHelp = String(
        localized: "Writes are enabled: archiving, deleting, flagging and sending reach the mail server. Click to return to read-only."
    )

    static let enableConfirmationTitle = String(localized: "Enable Writes for This Account?")

    static func enableConfirmationMessage(email: String) -> String {
        String(
            localized: "Castellum will start making real changes to the mail server for \(email) — archiving, deleting, flagging, marking read and sending will all be applied remotely.\n\nUntil now this account has been in evaluation mode, where those actions were refused. You can return to read-only at any time."
        )
    }

    static let enableConfirmButton = String(localized: "Enable Writes")

    static let disableConfirmationTitle = String(localized: "Return to Read-only?")

    static func disableConfirmationMessage(email: String) -> String {
        String(
            localized: "\(email) goes back to read-only: Castellum will stop making changes to the mail server. Archiving, deleting, flagging and sending will be refused again, and operations still queued for this account will not be sent while it stays read-only."
        )
    }

    static let disableConfirmButton = String(localized: "Return to Read-only")

    /// Shown when a write was attempted on a read-only account. `verb` is the
    /// action the user asked for ("Archive", "Delete", …).
    static func blockedNoticeMessage(verb: String, email: String) -> String {
        String(
            localized: "\(verb) was not applied: \(email) is in evaluation mode, so Castellum makes no changes to the mail server.\n\nEnable writes for this account to act on your mail."
        )
    }

    static let blockedNoticeTitle = String(localized: "This Account Is Read-only")

    static let keepReadOnlyButton = String(localized: "Keep Read-only")

    /// Appended to the refusal when Castellum can repeat the refused action
    /// itself (Send from a compose window) once writes are on.
    static let blockedNoticeRetrySuffix = String(
        localized: "If you enable writes, Castellum retries it straight away.")

    // MARK: all accounts (WS1, DECISIONS.md 2026-10-05)

    static let enableAllButton = String(localized: "Enable Writes for All Accounts…")

    static let enableAllHelp = String(
        localized: "Leave evaluation mode for every account that is still read-only. Asks for confirmation first; accounts added later still start read-only.")

    static let enableAllConfirmationTitle = String(localized: "Enable Writes for All Accounts?")

    /// `emails` are the read-only accounts the confirmation covers — listed in
    /// full, because this one click changes every one of them.
    static func enableAllConfirmationMessage(emails: [String]) -> String {
        let list = emails.joined(separator: "\n")
        return String(
            localized: "Castellum will start making real changes to the mail server for these accounts — archiving, deleting, flagging, marking read and sending will all be applied remotely:\n\n\(list)\n\nThis covers only the accounts listed. An account you add later still starts read-only.")
    }

    static let enableAllConfirmButton = String(localized: "Enable Writes for All")

    // MARK: after enabling from a refusal

    static let retryNoticeTitle = String(localized: "Writes Enabled")

    /// Shown when writes were enabled from a refusal Castellum cannot repeat
    /// itself (the selection may have moved on): the user repeats the action.
    static func retryNoticeMessage(verb: String, email: String) -> String {
        String(
            localized: "\(email) now makes changes to the mail server.\n\n\(verb) was not applied earlier. Repeat it now.")
    }

    static let writesEnabledLabel = String(localized: "Writes enabled")

    // MARK: - the dialog, as data

    /// Everything the write-safety dialog shows for `prompt`, as plain data so
    /// tests can assert on it and ONE presenter (`WriteSafetyPresenter`) can
    /// show it on whichever window the user acted in.
    ///
    /// Invariants (tested): every prompt has a dismissing button, and a button
    /// that enables writes is never the default (Return) button — leaving
    /// evaluation mode takes a deliberate click.
    static func alertSpec(for prompt: WriteSafetyPrompt, willRetry: Bool) -> WriteSafetyAlertSpec {
        typealias B = WriteSafetyAlertSpec.Button
        let cancel = String(localized: "Cancel")
        switch prompt {
        case .blocked(let accountId, let email, let verb):
            var message = blockedNoticeMessage(verb: verb, email: email)
            if willRetry { message += "\n\n" + blockedNoticeRetrySuffix }
            return WriteSafetyAlertSpec(
                title: blockedNoticeTitle, message: message, isWarning: true,
                buttons: [
                    B(title: enableConfirmButton, action: .enable([accountId]), isDestructive: true),
                    B(title: keepReadOnlyButton, action: .dismiss, isDefault: true),
                ])
        case .confirmEnable(let accountId, let email):
            return WriteSafetyAlertSpec(
                title: enableConfirmationTitle,
                message: enableConfirmationMessage(email: email), isWarning: true,
                buttons: [
                    B(title: enableConfirmButton, action: .enable([accountId]), isDestructive: true),
                    B(title: cancel, action: .dismiss, isDefault: true),
                ])
        case .confirmDisable(let accountId, let email):
            return WriteSafetyAlertSpec(
                title: disableConfirmationTitle,
                message: disableConfirmationMessage(email: email), isWarning: false,
                buttons: [
                    B(title: disableConfirmButton, action: .disable(accountId), isDefault: true),
                    B(title: cancel, action: .dismiss),
                ])
        case .confirmEnableAll(let accounts):
            return WriteSafetyAlertSpec(
                title: enableAllConfirmationTitle,
                message: enableAllConfirmationMessage(emails: accounts.map(\.email)),
                isWarning: true,
                buttons: [
                    B(title: enableAllConfirmButton, action: .enable(accounts.map(\.id)),
                      isDestructive: true),
                    B(title: cancel, action: .dismiss, isDefault: true),
                ])
        case .retryNotice(_, let email, let verb):
            return WriteSafetyAlertSpec(
                title: retryNoticeTitle,
                message: retryNoticeMessage(verb: verb, email: email), isWarning: false,
                buttons: [B(title: String(localized: "OK"), action: .dismiss, isDefault: true)])
        }
    }
}

/// One account named by an all-accounts confirmation: the snapshot the user
/// is shown and confirms, never "whatever accounts exist later".
struct WriteSafetyAccountRef: Equatable {
    let id: String
    let email: String
}

/// A write-safety dialog as data: see `AccountWritePolicy.alertSpec`.
struct WriteSafetyAlertSpec: Equatable {
    /// What a button does once clicked; carried out by
    /// `AppModel.resolveWriteSafetyPrompt`.
    enum Action: Equatable {
        /// Leave evaluation mode for exactly these accounts.
        case enable([String])
        /// Put this account back into evaluation mode.
        case disable(String)
        /// Change nothing.
        case dismiss
    }

    struct Button: Equatable {
        let title: String
        let action: Action
        var isDestructive = false
        /// Answers Return. Never set on an enabling button.
        var isDefault = false
    }

    let title: String
    let message: String
    /// Warning style for anything that starts changing the mail server.
    let isWarning: Bool
    let buttons: [Button]
}

/// A write-safety prompt awaiting the user.
///
/// All cases live in ONE `@Published` slot on `AppModel` and are presented by
/// ONE AppKit presenter (`WriteSafetyPresenter`), deliberately:
///  * only one modal can be up at a time, so a single slot makes that structural
///    rather than a matter of luck between competing `.alert` modifiers. That
///    luck ran out on 2026-09-23: a second legacy `.alert(item:)` on `RootView`
///    (compose-unavailable, #138) replaced this one, and the dialog was never
///    shown again until WS1 moved it out of SwiftUI;
///  * the refusal can come from any window (main, compose, Settings, a
///    standalone message), and the dialog belongs on the window the user
///    acted in, not on a main window that may be behind it or closed;
///  * a `List` section header is backed by an `NSTableView` view that AppKit
///    recycles on every reload (and this sidebar reloads on nearly every
///    `AppModel` publish — counts, sync state, messages). Presenting the
///    enable-writes confirmation from there risks it being dismissed or never
///    shown, and that confirmation is the ONLY checkpoint before an account
///    starts changing the real mail server.
enum WriteSafetyPrompt: Identifiable, Equatable {
    /// A write was refused because the account is in evaluation mode. `verb` is
    /// the action the user asked for, capitalised ("Archive", "Delete").
    case blocked(accountId: String, email: String, verb: String)
    /// The user asked to leave evaluation mode for this account.
    case confirmEnable(accountId: String, email: String)
    /// The user asked to put this account back into evaluation mode.
    case confirmDisable(accountId: String, email: String)
    /// The user asked, in Settings, to leave evaluation mode for every account
    /// that is still read-only. `accounts` is the snapshot being confirmed.
    case confirmEnableAll(accounts: [WriteSafetyAccountRef])
    /// Writes were just enabled from a refusal that could not be retried
    /// automatically: tell the user to repeat `verb`.
    case retryNotice(accountId: String, email: String, verb: String)

    /// The single account this prompt is about; nil for the all-accounts one.
    var accountId: String? {
        switch self {
        case .blocked(let id, _, _), .confirmEnable(let id, _), .confirmDisable(let id, _),
             .retryNotice(let id, _, _):
            return id
        case .confirmEnableAll:
            return nil
        }
    }

    var email: String? {
        switch self {
        case .blocked(_, let email, _), .confirmEnable(_, let email),
             .confirmDisable(_, let email), .retryNotice(_, let email, _):
            return email
        case .confirmEnableAll:
            return nil
        }
    }

    var id: String {
        switch self {
        case .blocked(let id, _, let verb): return "blocked-\(id)-\(verb)"
        case .confirmEnable(let id, _): return "enable-\(id)"
        case .confirmDisable(let id, _): return "disable-\(id)"
        case .confirmEnableAll(let accounts):
            return "enable-all-" + accounts.map(\.id).joined(separator: ",")
        case .retryNotice(let id, _, let verb): return "retry-\(id)-\(verb)"
        }
    }
}

/// Classifies a `resetAccount` refusal caused by an unsent outbox draft or a
/// non-terminal queued operation — the reset
/// counterpart to `AccountWritePolicy.isEvaluationModeRefusal`.
enum AccountResetPolicy {
    /// The stable marker `Database::RESET_BLOCKED_MSG_PREFIX` (Rust) uses.
    /// Pinned there by `reset_blocked_message_keeps_the_prefix_the_swift_ui_matches`.
    static let resetBlockedPrefix = "reset blocked:"

    static func isResetBlockedRefusal(_ error: Error) -> Bool {
        guard case .message(let reason)? = error as? CastellumError else { return false }
        return reason.hasPrefix(resetBlockedPrefix)
    }
}

/// Classifies an `applyMailboxSelection` refusal caused by a non-terminal
/// operation still referencing a folder being removed (
/// follow-up review) — the folder-edit counterpart to `AccountResetPolicy`.
enum MailboxSelectionPolicy {
    /// The stable marker `Database::MAILBOX_REMOVAL_BLOCKED_MSG_PREFIX`
    /// (Rust) uses. Pinned there by
    /// `mailbox_removal_blocked_message_keeps_the_prefix_the_swift_ui_matches`.
    static let removalBlockedPrefix = "mailbox removal blocked:"

    static func isRemovalBlockedRefusal(_ error: Error) -> Bool {
        guard case .message(let reason)? = error as? CastellumError else { return false }
        return reason.hasPrefix(removalBlockedPrefix)
    }
}
