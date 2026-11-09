// Castellum - guidance for Microsoft's "SMTP AUTH is switched off" refusal (MS1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A Microsoft mailbox can accept the OAuth sign-in and IMAP, and still refuse
// every send with
//   535 5.7.139 Authentication unsuccessful, SmtpClientAuthentication is
//   disabled for the Mailbox. Visit https://aka.ms/smtp_auth_disabled
// The core classifies that reply as its own kind, `auth_smtp_disabled`
// (`castellum-provider-smtp`), stops retrying it, and this file is the one
// place the app turns the kind into words: what the setting is called, who
// can change it, and Microsoft's own page about it. Same shape as the
// "IMAP is switched off" guidance in `AccountSetupSheet`.
//
// Only the kind token is read. The server's text after it is never shown
// here (the Outbox keeps showing it verbatim, as for every operation).
// Research and the sending-route recommendation:
// project-docs/microsoft-send-2026-10.md.

import SwiftUI

enum SmtpAuthDisabledGuidance {
    /// The core's stable kind token (`redact::ErrorKind::AuthSmtpDisabled`).
    static let kind = "auth_smtp_disabled"

    /// The address the server's own reply gives; Microsoft redirects it to
    /// its current "Enable or disable SMTP AUTH" page.
    static let helpURL = URL(string: "https://aka.ms/smtp_auth_disabled")!

    static var headline: String {
        String(localized: "Microsoft has switched off SMTP sending for this mailbox.")
    }

    /// What the setting is called and who can change it. Used on its own
    /// where no message is waiting (the account sheet's connection test).
    static var explanation: String {
        String(localized: "Signing in again will not help. The setting is called Authenticated SMTP (SMTP AUTH). For a work or school mailbox, ask your administrator to turn on Authenticated SMTP for it in the Microsoft 365 admin centre (Users > Active users > Mail > Manage email apps). A personal Outlook.com mailbox has no such switch, so Castellum cannot send from it yet.")
    }

    /// What happens to the message that was refused.
    static var outboxNote: String {
        String(localized: "Your message stays in the Outbox; choose Retry there once the setting has changed.")
    }

    /// The full text shown under a failed send.
    static var detail: String { "\(explanation) \(outboxNote)" }

    static var linkTitle: String {
        String(localized: "Open Microsoft’s SMTP AUTH help")
    }

    /// True when a recorded send failure (`"<kind>: <detail>"`) or a bare
    /// kind tag is this refusal. Exact token match: never guessed from the
    /// server's wording.
    static func applies(to rawReason: String?) -> Bool {
        guard let rawReason else { return false }
        if rawReason == kind { return true }
        guard let range = rawReason.range(of: ": ") else { return false }
        return rawReason[..<range.lowerBound] == kind
    }
}

/// Headline, explanation and link, in the style of the account sheet's
/// "IMAP is switched off" block. `showsHeadline` is false where the caller
/// has already printed the headline as part of its own caption.
struct SmtpAuthDisabledGuidanceView: View {
    var showsHeadline = true

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if showsHeadline {
                Label(SmtpAuthDisabledGuidance.headline, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .font(.caption.weight(.semibold))
            }
            Text(SmtpAuthDisabledGuidance.detail)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            Link(SmtpAuthDisabledGuidance.linkTitle, destination: SmtpAuthDisabledGuidance.helpURL)
                .font(.caption)
        }
        .accessibilityElement(children: .contain)
    }
}
