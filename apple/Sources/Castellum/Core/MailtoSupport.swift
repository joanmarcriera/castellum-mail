// Castellum - full mailto: URI parsing for the default mail handler (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// RFC 6068 parsing for compose prefill: to (path, comma-separated,
// percent-decoded) plus cc/bcc/subject/body query parameters. Deliberately
// separate from `UnsubscribeMailto` (S44), which only ever needs one address
// plus an optional subject for List-Unsubscribe and stays minimal for that
// call site — this one seeds a full compose window instead.

import Foundation

/// Plain-text fields lifted from a `mailto:` URI, ready to seed a compose
/// window verbatim (`ComposeSession`). No address validation here — that
/// happens once, on Send, exactly like anything the user types by hand.
struct MailtoPrefill: Equatable {
    var to: String = ""
    var cc: String = ""
    var bcc: String = ""
    var subject: String = ""
    var body: String = ""

    var isEmpty: Bool { self == MailtoPrefill() }
}

enum MailtoParser {
    /// Parses a `mailto:` URI (from a link, an Apple Event, or "Open With").
    /// Returns nil only when `urlString` isn't a `mailto:` URI at all; a bare
    /// `mailto:` with no recipient and no query items still parses to an
    /// all-empty `MailtoPrefill` (opens a blank compose window), matching how
    /// every other mail client treats it.
    static func parse(_ urlString: String) -> MailtoPrefill? {
        guard let components = URLComponents(string: urlString),
              components.scheme?.lowercased() == "mailto"
        else { return nil }

        // RFC 6068: the path is a comma-separated list of percent-encoded
        // addresses; URLComponents decodes `.path` for us. A `to=` query
        // item (some senders use one instead of, or as well as, the path)
        // is appended rather than replacing it.
        var to = components.path
        var cc = ""
        var bcc = ""
        var subject = ""
        var body = ""

        for item in components.queryItems ?? [] {
            guard let value = item.value else { continue }
            switch item.name.lowercased() {
            case "to": to = join(to, value)
            case "cc": cc = join(cc, value)
            case "bcc": bcc = join(bcc, value)
            case "subject": subject = value
            case "body": body = value
            default: break   // headers, in-reply-to, etc.: v1 ignores them
            }
        }

        return MailtoPrefill(to: to, cc: cc, bcc: bcc, subject: subject, body: body)
    }

    private static func join(_ existing: String, _ value: String) -> String {
        existing.isEmpty ? value : existing + "," + value
    }
}
