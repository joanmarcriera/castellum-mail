// Castellum - mailto: URI parsing for List-Unsubscribe (S44).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// Parses a `mailto:` URI's address and optional `subject` query parameter.
/// Never throws — a malformed URI just yields no usable address, and the
/// caller (`AppModel.confirmUnsubscribeByMailto`) already guards on it being
/// non-nil before this is called.
enum UnsubscribeMailto {
    static func parse(_ mailto: String) -> (address: String, subject: String?) {
        guard let components = URLComponents(string: mailto) else { return (mailto, nil) }
        let address = components.path
        let rawSubject = components.queryItems?.first { $0.name.lowercased() == "subject" }?.value
        return (address, rawSubject.flatMap(sanitizedSubject))
    }

    /// Longest `subject=` honoured (sent and shown).
    static let maxSubjectLength = 200

    /// Subject with control characters (CR/LF, bidi/format controls) removed
    /// and length capped, so a hostile header cannot inject lines or blow up
    /// the confirm sheet. Empty after cleaning means no subject.
    static func sanitizedSubject(_ raw: String) -> String? {
        let cleaned = String(String.UnicodeScalarView(raw.unicodeScalars.filter {
            $0.properties.generalCategory != .control && $0.properties.generalCategory != .format
        }))
        let capped = String(cleaned.prefix(maxSubjectLength))
        return capped.isEmpty ? nil : capped
    }

    /// Longest `body=` honoured. A List-Unsubscribe body is a few words
    /// ("unsubscribe"); the cap keeps a hostile header from making us send
    /// a large attacker-chosen payload.
    static let maxBodyLength = 500

    /// Address, `subject=` and `body=` (percent-decoded by URLComponents,
    /// case-insensitive names, capped) of a mailto: URI. LISTSERV-style lists
    /// read the command from the body (#3050).
    static func parseMessage(_ mailto: String) -> (address: String, subject: String?, body: String?) {
        let (address, subject) = parse(mailto)
        guard let components = URLComponents(string: mailto) else { return (address, subject, nil) }
        let raw = components.queryItems?.first { $0.name.lowercased() == "body" }?.value
        let body = raw.map { String($0.prefix(maxBodyLength)) }
        return (address, subject, (body?.isEmpty ?? true) ? nil : body)
    }
}
