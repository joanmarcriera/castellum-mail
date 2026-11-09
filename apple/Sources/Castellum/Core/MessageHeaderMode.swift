// Castellum - message header display modes.
//
// MailMate shows a message's headers in three levels. Castellum's reader does
// the same, chosen from View > Header (and Settings), persisted per profile:
//
//   compact   - subject, sender and date only
//   extended  - + To and Cc (what the reader always showed before)
//   all       - + Bcc, Reply-To, Message-ID, and the full raw header block when
//               the message's source is already cached on this Mac (no network
//               unless the user asks for "Load full headers")
//
// This file is the pure part: which rows each mode shows, and how a header
// block is cut out of raw RFC 822 text.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum MessageHeaderMode: String, CaseIterable, Identifiable, Sendable {
    case compact, extended, all

    var id: String { rawValue }
    static let defaultMode: MessageHeaderMode = .extended

    var label: String {
        switch self {
        case .compact: return String(localized: "Compact Headers")
        case .extended: return String(localized: "Extended Headers")
        case .all: return String(localized: "All Headers")
        }
    }

    /// The recipient kinds (`MessageDetail.addresses[].kind`) shown as lines
    /// under the sender, in display order.
    var recipientKinds: [String] {
        switch self {
        case .compact: return []
        case .extended: return ["to", "cc"]
        case .all: return ["to", "cc", "bcc", "reply-to"]
        }
    }

    /// Whether Message-ID and the raw header block are shown.
    var showsTechnicalHeaders: Bool { self == .all }

    /// The row label for a recipient kind (shared by the reader and print).
    static func recipientLabel(_ kind: String) -> String {
        switch kind {
        case "to": return "To"
        case "cc": return "Cc"
        case "bcc": return "Bcc"
        case "reply-to": return "Reply-To"
        default: return kind
        }
    }
}

enum RawHeaderBlock {
    /// Longest header block shown (a runaway "headers" with no blank line in
    /// a corrupt source would otherwise render the whole file).
    static let maxLines = 400

    /// The header block of raw RFC 822 text: everything before the first blank
    /// line, folded continuation lines kept as they are. Total: input without
    /// a blank line is capped at `maxLines`; CR LF and LF are both handled.
    static func extract(from raw: String) -> String {
        // Swift treats "\r\n" as ONE Character, so normalise before splitting.
        let text = raw.replacingOccurrences(of: "\r\n", with: "\n")
        var lines: [Substring] = []
        for line in text.split(separator: "\n", omittingEmptySubsequences: false) {
            if line.isEmpty { break }
            lines.append(line)
            if lines.count >= maxLines { break }
        }
        return lines.joined(separator: "\n")
    }
}
