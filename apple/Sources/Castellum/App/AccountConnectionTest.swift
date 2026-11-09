// Castellum - account-setup "Test Connection".
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The Add/Re-authenticate sheet can verify IMAP + SMTP credentials BEFORE an
// account is saved or synced (native/TASKS.md: "a 'Test Connection' step that
// shows this legible error before saving anything"). The Rust side
// (`castellum_test_connection`) does the network work and persists nothing;
// this file holds the pure, unit-tested pieces the sheet uses:
//   * `AccountConfig.connectionTestJSON()` — the request, with the same SMTP
//     smart defaults (`effectiveSMTP`) sending would use, so the test checks
//     exactly the settings that will be saved;
//   * `ConnectionTestPresentation` — maps each leg's redacted error kind to
//     one actionable sentence. Like `AccountSyncStatus`, classification uses
//     only the stable kind tag, never server text (which the core never
//     returns anyway).

import Foundation

extension AccountConfig {
    /// Request JSON for `castellum_test_connection`: flat IMAP fields plus a
    /// nested `smtp` object built from `effectiveSMTP`. JSONSerialization so
    /// every value is escaped safely (same approach as `sendConfigJSON`).
    func connectionTestJSON() throws -> String {
        let smtp = effectiveSMTP
        let dict: [String: Any] = [
            "host": host,
            "port": port,
            "username": username,
            "password": password,
            "use_tls": useTls,
            "imap_security": effectiveIMAPSecurity.rawValue,
            "allow_invalid_certs": allowInvalidCerts ?? false,
            "smtp": [
                "host": smtp.host,
                "port": smtp.port,
                "username": smtp.username,
                "password": smtp.password,
                "use_tls": smtp.useTls,
                "allow_invalid_certs": allowInvalidCerts ?? false,
            ] as [String: Any],
        ]
        let data = try JSONSerialization.data(withJSONObject: dict)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return json
    }
}

/// One line of the sheet's test-result block.
struct ConnectionTestLine: Equatable {
    let ok: Bool
    let text: String
}

enum ConnectionTestPresentation {
    /// IMAP then SMTP (when tested), each as a single actionable sentence.
    static func lines(for report: ConnectionTestReport, config: AccountConfig) -> [ConnectionTestLine] {
        var out = [imapLine(report: report, config: config)]
        if let smtp = report.smtp {
            let endpoint = config.effectiveSMTP.host + ":" + String(config.effectiveSMTP.port)
            out.append(line(for: smtp, protocolName: "SMTP", endpoint: endpoint,
                            successText: "Sending: signed in to \(endpoint)."))
        }
        return out
    }

    /// True when both legs passed — the sheet's "ready to add" signal.
    static func allPassed(_ report: ConnectionTestReport) -> Bool {
        report.imap.ok && (report.smtp?.ok ?? true)
    }

    private static func imapLine(report: ConnectionTestReport, config: AccountConfig) -> ConnectionTestLine {
        let folders = report.mailboxes.filter(\.selectable).count
        return line(for: report.imap, protocolName: "IMAP", endpoint: config.imapEndpoint,
                    successText: "Incoming mail: signed in to \(config.imapEndpoint) — "
                        + String(localized: "\(folders) folders found."))
    }

    /// Maps one leg's redacted `errorKind` to wording that says what to
    /// check next. Unknown/future tags fail closed to neutral wording rather
    /// than being guessed at.
    static func line(
        for leg: ConnectionLegResult, protocolName: String, endpoint: String, successText: String
    ) -> ConnectionTestLine {
        guard !leg.ok else { return ConnectionTestLine(ok: true, text: successText) }
        let text: String
        switch leg.errorKind {
        case "auth":
            text = "\(protocolName) sign-in to \(endpoint) failed — check the username and password. "
                + "Gmail, iCloud and Fastmail need an app-specific password, not your normal one."
        case SmtpAuthDisabledGuidance.kind:
            // MS1: the sign-in worked; sending is switched off server-side.
            text = "\(SmtpAuthDisabledGuidance.headline) \(SmtpAuthDisabledGuidance.explanation)"
        case "network":
            text = "Can’t reach \(endpoint) (\(protocolName)) — check the server name and port."
        case "tls":
            text = "Secure connection to \(endpoint) (\(protocolName)) failed — check the port and TLS setting."
        case "protocol":
            text = "\(endpoint) gave an unexpected \(protocolName) response."
        case "unsupported":
            text = "\(endpoint) doesn’t support a capability Castellum needs for \(protocolName)."
        default:
            text = "\(protocolName) test against \(endpoint) failed."
        }
        return ConnectionTestLine(ok: false, text: text)
    }
}
