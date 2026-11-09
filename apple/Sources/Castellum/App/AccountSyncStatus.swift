// Castellum - per-account sync status presentation.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Classification comes only from the Rust core's durable ErrorKind tag. Raw
// provider text is retained for the local Diagnostics window, but is never
// inspected to decide the state or user-facing wording.

import Foundation

struct AccountSyncTarget: Equatable, Sendable {
    let accountId: String
    let email: String
    let host: String
    let port: Int

    init?(configJSON: String) {
        guard let config = try? AccountConfig.decode(fromJSON: configJSON) else { return nil }
        accountId = config.accountId
        email = config.email
        host = config.host
        port = config.port
    }

    var endpoint: String { "\(host):\(port)" }
}

enum AccountSyncState: String, Equatable, Sendable {
    case syncing
    case ok
    case unreachable
    case authenticationFailed
    case tlsFailed
    case protocolFailed
    case databaseFailed
    case unsupported
    case internalFailed

    var label: String {
        switch self {
        case .syncing: return "syncing"
        case .ok: return "ok"
        case .unreachable: return "unreachable"
        case .authenticationFailed: return "authentication failed"
        case .tlsFailed: return "secure connection failed"
        case .protocolFailed: return "server response failed"
        case .databaseFailed: return "local database failed"
        case .unsupported: return "unsupported server"
        case .internalFailed: return "sync failed"
        }
    }

    var isFailure: Bool {
        self != .syncing && self != .ok
    }
}

struct AccountSyncStatus: Identifiable, Equatable, Sendable {
    var id: String { accountId }
    let accountId: String
    let state: AccountSyncState
    /// Rust `ErrorKind::as_str()` value. Nil for syncing/success.
    let errorKind: String?
    let message: String
    /// Local-only FFI error text. Never exported or used for classification.
    let rawError: String?

    /// Failures (and only failures) offer a manual retry.
    var offersRetry: Bool { state.isFailure }

    /// Text for the "Copy details" action: the localised message plus the
    /// core's stable kind tag. Deliberately NEVER `rawError` — provider text
    /// can carry server identifiers or message content (CLAUDE.md gotcha 5);
    /// the unredacted detail stays in the local Diagnostics window.
    var copyableText: String {
        guard let errorKind else { return message }
        return "\(message) [\(errorKind)]"
    }

    static func syncing(_ target: AccountSyncTarget) -> AccountSyncStatus {
        AccountSyncStatus(
            accountId: target.accountId,
            state: .syncing,
            errorKind: nil,
            message: "Syncing \(target.email)…",
            rawError: nil
        )
    }

    static func ok(_ target: AccountSyncTarget) -> AccountSyncStatus {
        AccountSyncStatus(
            accountId: target.accountId,
            state: .ok,
            errorKind: nil,
            message: "\(target.email) is up to date.",
            rawError: nil
        )
    }

    /// The core's kind token from a thrown sync error, or nil. A first-ever
    /// connect fails before the account row exists, so there is no local
    /// diagnostics row to read the kind from; the core then puts it in front of
    /// the thrown text as `"<kind>: <detail>"` (e.g. `auth: LOGIN failed`).
    /// Only the closed set of core kinds is accepted; anything else is nil.
    static func errorKind(fromThrown reason: String) -> String? {
        guard let range = reason.range(of: ": ") else { return nil }
        var token = String(reason[..<range.lowerBound])
        // The body-fetch path throws "<kind> error: <detail>" ("network error: …").
        if token.hasSuffix(" error") { token = String(token.dropLast(" error".count)) }
        let known: Set<String> = ["auth", "tls", "network", "protocol", "unsupported"]
        return known.contains(token) ? token : nil
    }

    /// Reading-pane wording for a failed body download. Derived only from the
    /// core's kind token; the thrown text itself (OS errors, server replies)
    /// never reaches the UI. Unknown kinds get neutral body-specific wording.
    static func bodyFetchMessage(target: AccountSyncTarget?, thrown: String) -> String {
        let kind = errorKind(fromThrown: thrown)
        guard let target, let kind, kind != "unsupported" else {
            return String(localized: "Couldn’t download this message.")
        }
        let status = failure(target: target, errorKind: kind, rawError: nil)
        return status.state == .internalFailed ? String(localized: "Couldn’t download this message.") : status.message
    }

    /// Map only the core's stable error-kind tag. `rawError` is deliberately
    /// carried through untouched so Diagnostics can reveal the detailed local
    /// failure without making presentation logic depend on provider wording.
    static func failure(
        target: AccountSyncTarget,
        errorKind: String?,
        rawError: String?
    ) -> AccountSyncStatus {
        let state: AccountSyncState
        let message: String
        switch errorKind {
        case "network":
            state = .unreachable
            message = String(localized: "Can’t reach \(target.endpoint) — the connection failed.")
        case "auth":
            state = .authenticationFailed
            message = String(localized: "Sign-in failed for \(target.email) — check the username or password.")
        case "tls":
            state = .tlsFailed
            message = String(localized: "Secure connection to \(target.endpoint) failed — check the TLS settings.")
        case "protocol":
            state = .protocolFailed
            message = String(localized: "\(target.endpoint) returned an unexpected IMAP response.")
        case "db":
            state = .databaseFailed
            message = String(localized: "Castellum couldn’t update local mail for \(target.email).")
        case "unsupported":
            state = .unsupported
            message = String(localized: "\(target.endpoint) doesn’t support a required IMAP capability.")
        case "internal", "redacted", nil:
            state = .internalFailed
            message = String(localized: "Castellum couldn’t sync \(target.email).")
        default:
            // Forward-compatible and fail-closed: an unknown future tag is not
            // guessed from raw text and gets neutral wording.
            state = .internalFailed
            message = String(localized: "Castellum couldn’t sync \(target.email).")
        }
        return AccountSyncStatus(
            accountId: target.accountId,
            state: state,
            errorKind: errorKind,
            message: message,
            rawError: rawError
        )
    }
}

/// The Settings > Accounts row's sync line. The live per-account status wins
/// over the diagnostics snapshot (loaded once when the tab appears), so a
/// failure after Re-authenticate shows the same readable message as the banner.
enum AccountRowSummary {
    static func text(status: AccountSyncStatus?, diagnostics: LocalAccountDiagnostics?) -> String {
        if let status, status.state.isFailure { return status.message }
        if status?.state == .syncing { return "Syncing…" }
        // A live ok with no diagnostics snapshot yet (a just-added or duplicated
        // account) has synced; never claim "unknown" (RP06).
        guard let diagnostics else { return status?.state == .ok ? "Last synced: just now" : "Last sync: unknown" }
        if diagnostics.consecutiveFailures > 0 {
            let kind = diagnostics.lastErrorKind.map { " (\($0))" } ?? ""
            return "Last sync failed\(kind)"
        }
        if let ts = diagnostics.lastSuccessAt { return "Last synced: \(ts)" }
        if diagnostics.lastAttemptAt != nil { return "Syncing…" }
        return "Never synced"
    }
}

/// Formats a `SyncProgress` snapshot into one short
/// human-readable line, for every surface that shows live sync progress —
/// the Status dashboard's account row, the sidebar status dot's tooltip, and
/// the Add-account sheet. Pure and unit-tested rather than eyeballed,
/// mirroring `SidebarView`'s `UnifiedInboxBadge`/`SmartMailboxBadge`.
enum SyncProgressPresentation {
    /// The primary line: the mailbox currently being fetched against its
    /// IMAP `EXISTS` total when known ("Inbox: 1,204 / 12,830"), falling
    /// back to a running aggregate once a total isn't available yet (between
    /// mailboxes, or before the first one has been selected), plus a
    /// trailing folder count whenever there's more than one mailbox to sync.
    static func label(for progress: SyncProgress) -> String {
        let folders = progress.mailboxesTotal > 1
            ? " · \(progress.mailboxesCompleted) of \(progress.mailboxesTotal) folders"
            : ""
        if let current = progress.currentMailbox, current.total > 0 {
            return "\(current.path): \(formatted(current.fetched)) / \(formatted(current.total))\(folders)"
        }
        if progress.aggregateFetched > 0 {
            return "\(formatted(progress.aggregateFetched)) messages so far\(folders)"
        }
        return "Starting…\(folders)"
    }

    private static func formatted(_ n: Int64) -> String {
        NumberFormatter.localizedString(from: NSNumber(value: n), number: .decimal)
    }
}

/// Why `AppModel.composeAccountIdOrPromptUnavailable()` couldn't hand back an
/// account to compose from. Presented as a two-button alert by `RootView`
/// (see `composeUnavailableAlert`) instead of the old OK-only dead end.
enum ComposeUnavailablePrompt: Identifiable, Equatable {
    /// Nothing is configured yet.
    case noAccount
    /// An account was added and is mid-first-sync — `accounts` (backed by
    /// `core.listAccounts()`) won't include it until that sync succeeds.
    case firstSyncInProgress(accountId: String)

    var id: String {
        switch self {
        case .noAccount: return "noAccount"
        case .firstSyncInProgress(let accountId): return "firstSyncInProgress-\(accountId)"
        }
    }
}

/// Presentation for a failed SMTP send/queue attempt, sharing
/// the sync classifier's ErrorKind vocabulary and its redaction stance: the
/// message names only the (locally-configured, trustworthy) SMTP endpoint and
/// a fixed kind-derived phrase — never raw provider/server text, matching
/// `AccountSyncStatus.failure`'s message above.
///
/// The Rust core (`castellum-core/src/send.rs`) records a classified send
/// failure — both `core.sendPending()`'s thrown error and a queued
/// `pending_operations.last_error` row — as `"<kind>: <detail>"`. Only the
/// leading kind token is ever read here; `<detail>` is intentionally
/// discarded for display (it stays visible, unclassified, in the Outbox's
/// own local per-operation text, which is unrelated to this presentation).
enum SendFailurePresentation {
    /// Full inline caption for ComposeView's outbox case.
    static func outboxMessage(rawReason: String, endpoint: String?) -> String {
        String(localized: "Not sent — kept in Outbox. \(reason(for: rawReason, endpoint: endpoint))")
    }

    /// What ComposeView shows for a send left in the Outbox: the caption,
    /// and whether to add the "SMTP AUTH is switched off" explanation and
    /// link under it (MS1).
    struct ComposePresentation: Equatable {
        let caption: String
        let showsSmtpAuthDisabledHelp: Bool
    }

    static func composePresentation(rawReason: String, endpoint: String?) -> ComposePresentation {
        ComposePresentation(
            caption: outboxMessage(rawReason: rawReason, endpoint: endpoint),
            showsSmtpAuthDisabledHelp: SmtpAuthDisabledGuidance.applies(to: rawReason))
    }

    static func reason(for rawReason: String, endpoint: String?) -> String {
        // Localised (OB1): the Outbox shows these too, in en, es and ca.
        let target = endpoint ?? String(localized: "the SMTP server")
        switch kind(of: rawReason) {
        case "network":
            return String(localized: "Couldn’t reach \(target) — the connection failed.")
        case "auth":
            return String(localized: "Sign-in to \(target) failed — check the SMTP username or password.")
        case SmtpAuthDisabledGuidance.kind:
            // No endpoint and no "will retry": the server is reachable and
            // the core has stopped retrying. The explanation and link follow
            // in `SmtpAuthDisabledGuidanceView`.
            return "\(SmtpAuthDisabledGuidance.headline) \(SmtpAuthDisabledGuidance.explanation)"
        case "tls":
            return String(localized: "Secure connection to \(target) failed — check the TLS settings.")
        case "protocol":
            return String(localized: "\(target) returned an unexpected response.")
        case "unsupported":
            return String(localized: "\(target) doesn’t support a required feature.")
        case "rejected":
            // A permanent 5xx refusal (G8): the core does not retry it.
            return String(localized: "\(target) refused this message — check the recipients, then choose Retry in the Outbox.")
        default:
            return String(localized: "Delivery to \(target) failed — it will retry automatically.")
        }
    }

    private static func kind(of rawReason: String) -> String? {
        guard let range = rawReason.range(of: ": ") else { return nil }
        return String(rawReason[..<range.lowerBound])
    }
}
