// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Foundation

/// HP5a: deterministic "forgotten attachment" detector (EN / ES / CA keyword
/// list, no model, no network). Pure: a subject and a body in, the first
/// trigger word out (nil when there is none).
///
/// Usage: `ForgottenAttachment.triggerWord(subject:body:)`. The caller decides
/// whether an attachment is present; this only reads the user's own words.
///
/// Only text the user wrote is scanned: quoted lines (`>`), everything from a
/// reply attribution line down (`On ... wrote:`, `El ... escribió:`,
/// `El ... va escriure:`, or the app's own `<who> wrote:` followed by a quote)
/// everything from a forwarded-message separator down (the forwarded original
/// is not the user's own words) and everything from a signature delimiter
/// (`-- ` line) down are skipped.
enum ForgottenAttachment {
    /// Keywords, stored already folded (lower case, no diacritics).
    /// English / Spanish / Catalan; `adjunté` folds to `adjunte`.
    private static let keywords: Set<String> = [
        "attached", "attachment", "attachments", "attaching", "enclosed",
        "adjunto", "adjunta", "adjuntos", "adjuntas", "adjunte", "adjuntado", "anexo",
        "adjunt", "adjunts", "adjuntes", "adjuntat", "annex",
        // Sender-side forms only (no infinitives or imperatives).
        "adjuntem", "annexo", "annexat", "annexos",
        "adjuntamos", "adjuntada", "adjuntadas", "adjuntados", "anexa", "anexos", "anexado",
    ]

    /// The separator line the app writes above a forwarded original
    /// (`ReplyPrefill.forwardPrefill` uses it, so both stay in step).
    static let forwardedSeparator = "---------- Forwarded message ---------"

    /// Attribution-line endings, folded.
    private static let attributionSuffixes = ["wrote:", "escribio:", "va escriure:"]

    /// `knownAttribution` is the exact attribution line the app generated for
    /// this reply (a custom format need not end in `wrote:`); it stops the scan
    /// like any other attribution.
    static func triggerWord(subject: String, body: String, knownAttribution: String? = nil) -> String? {
        firstKeyword(in: subject)
            ?? firstKeyword(in: authoredPart(of: body, knownAttribution: knownAttribution))
    }

    /// The lines of `body` the user wrote: stops at a signature delimiter or
    /// an attribution line, skips `>` quoted lines.
    static func authoredPart(of body: String, knownAttribution: String? = nil) -> String {
        let known = knownAttribution?.trimmingCharacters(in: .whitespacesAndNewlines)
        let lines = body.components(separatedBy: .newlines)
        var kept: [String] = []
        for (index, line) in lines.enumerated() {
            let trimmedEnd = line.replacingOccurrences(
                of: "\\s+$", with: "", options: .regularExpression)
            if trimmedEnd == "--" { break }
            if isForwardSeparator(trimmedEnd) { break }
            if let known, !known.isEmpty,
               trimmedEnd.trimmingCharacters(in: .whitespaces) == known { break }
            let next = index + 1 < lines.count ? lines[index + 1] : nil
            if isAttribution(trimmedEnd, next: next) { break }
            if line.trimmingCharacters(in: .whitespaces).hasPrefix(">") { continue }
            kept.append(line)
        }
        return kept.joined(separator: "\n")
    }

    /// A line of at least two dashes at each end around "forwarded message"
    /// (`---------- Forwarded message ---------`, `-----Forwarded message-----`),
    /// or Apple Mail's `Begin forwarded message:`.
    private static func isForwardSeparator(_ line: String) -> Bool {
        let t = fold(line.trimmingCharacters(in: .whitespaces))
        if t.hasPrefix("begin forwarded message") { return true }
        return t.hasPrefix("--") && t.hasSuffix("--") && t.contains("forwarded message")
    }

    private static func fold(_ s: String) -> String {
        s.folding(options: [.diacriticInsensitive, .caseInsensitive], locale: nil)
    }

    /// `On … wrote:` / `El … escribió:` / `El … va escriure:`, or any
    /// `… wrote:` line directly above a `>` quoted line (the app's own form).
    private static func isAttribution(_ line: String, next: String?) -> Bool {
        let folded = fold(line.trimmingCharacters(in: .whitespaces))
        guard folded.count <= 200,
              attributionSuffixes.contains(where: { folded.hasSuffix($0) }) else { return false }
        if folded.hasPrefix("on ") || folded.hasPrefix("el ") { return true }
        return next?.trimmingCharacters(in: .whitespaces).hasPrefix(">") ?? false
    }

    /// First whole word (letters/digits run) whose folded form is a keyword;
    /// returned as the user typed it. Apostrophes split words, so `l'adjunt`
    /// matches `adjunt`.
    private static func firstKeyword(in text: String) -> String? {
        var token = ""
        func hit() -> String? {
            defer { token = "" }
            return !token.isEmpty && keywords.contains(fold(token)) ? token : nil
        }
        for ch in text {
            if ch.isLetter || ch.isNumber {
                token.append(ch)
            } else if let found = hit() {
                return found
            }
        }
        return hit()
    }
}
