// Castellum - what an unsubscribe action will actually do, decided ONCE (FP39 redo, #3114).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Usage: `UnsubscribePlan.make(for: detail)` is the single source of truth for
// both the reading pane's "Unsubscribe" affordance and the confirmation sheet,
// so the two can never disagree (#3046). Every field the sheet shows is exactly
// what is sent or opened - nothing is re-derived later - and every target
// that is not a plain https:// link to a real host name (or a single plain
// mailbox address) is refused with a reason instead of being offered.

import Foundation

enum UnsubscribePlan: Equatable {
    /// RFC 8058 one-click: Castellum POSTs `List-Unsubscribe=One-Click` to `url`.
    case oneClick(url: String, host: String)
    /// A short email: exactly this address, subject and body, through the normal send path.
    case mailto(address: String, subject: String, body: String)
    /// Nothing usable; `reason` is shown to the user.
    case refused(reason: String)

    /// Whether the reading pane should offer "Unsubscribe" at all.
    var isActionable: Bool {
        if case .refused = self { return false }
        return true
    }

    static func make(for detail: MessageDetail) -> UnsubscribePlan {
        var refusal: String?
        // One-click (or plain link) first: it is the standard, no-email route.
        if let raw = detail.listUnsubscribeUrl {
            switch validateHTTPS(raw) {
            case .success(let host):
                // An https list WITHOUT one-click has no in-app action (#3046:
                // the affordance must match what the sheet can do).
                if detail.listUnsubscribeOneClick {
                    return .oneClick(url: raw.trimmingCharacters(in: .whitespacesAndNewlines), host: host)
                }
                refusal = "This list does not support one-click unsubscribe, so there is nothing Castellum can send."
            case .failure(let why):
                refusal = why.message
            }
        }
        if let mailto = detail.listUnsubscribeMailto {
            switch validateMailto(mailto) {
            case .success(let m): return .mailto(address: m.address, subject: m.subject, body: m.body)
            case .failure(let why): refusal = refusal ?? why.message
            }
        }
        return .refused(reason: refusal ?? "This message has no recognised unsubscribe link.")
    }

    struct Rejection: Error, Equatable { let message: String }

    /// https only, a real DNS name (ASCII letters/digits/hyphens, at least one
    /// dot), no user:password@ part, no IP literal / localhost, no punycode or
    /// non-ASCII (lookalike) host labels. Returns the lower-cased host.
    static func validateHTTPS(_ raw: String) -> Result<String, Rejection> {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text.unicodeScalars.allSatisfy({ $0.value > 0x20 && $0.value < 0x7f }) else {
            return .failure(Rejection(message: "The unsubscribe link contains characters that could hide where it goes, so Castellum will not use it."))
        }
        guard let c = URLComponents(string: text), c.scheme?.lowercased() == "https" else {
            return .failure(Rejection(message: "The unsubscribe link is not a secure https:// link, so Castellum will not use it."))
        }
        if c.user != nil || c.password != nil {
            return .failure(Rejection(message: "The unsubscribe link contains a user name or password part, which is a common disguise for a different site, so Castellum will not use it."))
        }
        guard let host = c.host?.lowercased(), !host.isEmpty else {
            return .failure(Rejection(message: "The unsubscribe link has no website name."))
        }
        let labels = host.split(separator: ".", omittingEmptySubsequences: false)
        let validLabel: (Substring) -> Bool = { l in
            !l.isEmpty && l.count <= 63 && !l.hasPrefix("-") && !l.hasSuffix("-")
                && l.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || $0 == "-") }
        }
        guard labels.count >= 2, labels.allSatisfy(validLabel),
              labels.last.map({ !$0.allSatisfy(\.isNumber) }) == true,   // not an IPv4 address
              host != "localhost", !host.hasSuffix(".local"), !host.hasSuffix(".localhost")
        else {
            return .failure(Rejection(message: "The unsubscribe link does not point to an ordinary website name, so Castellum will not use it."))
        }
        if labels.contains(where: { $0.hasPrefix("xn--") }) {
            return .failure(Rejection(message: "The unsubscribe link uses an internationalised look-alike website name, so Castellum will not use it."))
        }
        return .success(host)
    }

    /// One plain mailbox; subject and body from the mailto: fields (defaults
    /// "unsubscribe" and an empty body). No extra recipients (`cc`, `bcc`,
    /// comma/semicolon lists), no control characters.
    static func validateMailto(_ raw: String) -> Result<(address: String, subject: String, body: String), Rejection> {
        let bad = Rejection(message: "The unsubscribe email address is not a single plain address, so Castellum will not use it.")
        guard let c = URLComponents(string: raw.trimmingCharacters(in: .whitespacesAndNewlines)),
              c.scheme?.lowercased() == "mailto" else { return .failure(bad) }
        let address = c.path
        let parts = address.split(separator: "@", omittingEmptySubsequences: false)
        guard parts.count == 2, !parts[0].isEmpty, parts[1].contains("."),
              address.unicodeScalars.allSatisfy({ $0.value > 0x20 && $0.value < 0x7f }),
              !address.contains(","), !address.contains(";"), !address.contains("<"), !address.contains(">")
        else { return .failure(bad) }
        let items = c.queryItems ?? []
        if items.contains(where: { ["cc", "bcc", "to"].contains($0.name.lowercased()) }) { return .failure(bad) }
        // Subject and body are sanitised and capped by the one shared parser.
        let parsed = UnsubscribeMailto.parseMessage(raw)
        return .success((address, parsed.subject ?? "unsubscribe", parsed.body ?? ""))
    }
}
