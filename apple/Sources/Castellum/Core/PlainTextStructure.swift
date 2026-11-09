// Castellum - structure of a plain-text mail body (MM-17, slices 3 and 4).
//
// Pure, no UI: splits a body into prose, quoted blocks (by `>` depth) and a trailing signature,
// decides what starts folded in the viewer, strips the sender's signature and re-wraps
// hard-wrapped prose for a reply quote. The flowed (RFC 3676) codec is Rust
// (`castellum-mail/src/flowed.rs`); everything here works on text that is already unflowed.
//
// Signature rules (decision D8): the viewer folds on the strict Usenet delimiter `-- ` (dash dash
// space) and, more leniently, on a bare `--` line close to the end of the message; Reply strips
// only on the strict one, because stripping is destructive. A signature block ends where quoted
// text begins, so a reply that has its own signature ABOVE the quote (as Castellum writes them)
// does not swallow the quote into the signature.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum PlainTextStructure {
    enum Block: Equatable {
        case text(lines: [String])
        /// Lines keep their `>` markers.
        case quote(depth: Int, lines: [String])
        /// From the delimiter line to the end of the signature.
        case signature(lines: [String])

        var lines: [String] {
            switch self {
            case .text(let l), .quote(_, let l), .signature(let l): return l
            }
        }
    }

    /// A quote block of this many lines or more starts collapsed (D10).
    static let quoteFoldMinLines = 8
    /// A signature of this many lines or more (delimiter included) starts collapsed (D10).
    static let signatureFoldMinLines = 5
    /// A bare `--` only counts as a signature delimiter this close to the end of the message.
    static let lenientSignatureMaxTailLines = 15
    /// Bodies longer than this skip tinting and folding in the viewer (rendered flat).
    static let maxStructuredBytes = 2 * 1024 * 1024

    // MARK: lines

    static func lines(of body: String) -> [String] {
        body.replacingOccurrences(of: "\r\n", with: "\n").replacingOccurrences(of: "\r", with: "\n")
            .components(separatedBy: "\n")
    }

    /// Quote depth of a line: the number of leading `>` markers, allowing one space between
    /// markers (`> > text` is depth 2). Also returns the index where the content starts (after the
    /// markers and one following space).
    static func quoteDepth(of line: String) -> (depth: Int, contentStart: String.Index) {
        var depth = 0
        var i = line.startIndex
        while i < line.endIndex {
            if line[i] == ">" {
                depth += 1
                i = line.index(after: i)
            } else if line[i] == " ", depth > 0, line.index(after: i) < line.endIndex, line[line.index(after: i)] == ">" {
                i = line.index(after: i)
            } else {
                break
            }
        }
        if depth > 0, i < line.endIndex, line[i] == " " { i = line.index(after: i) }
        return (depth, i)
    }

    // MARK: signature

    /// Exactly `-- ` (the trailing space is what makes it the delimiter); a trailing CR is ignored.
    private static func isStrictDelimiter(_ line: String) -> Bool { line == "-- " }

    /// `--` or `-- ` after trimming (the lenient form the viewer accepts).
    private static func isLenientDelimiter(_ line: String) -> Bool {
        line.trimmingCharacters(in: .whitespaces) == "--"
    }

    /// Index of the signature delimiter line (the last one, outside quotes), or nil.
    private static func signatureStart(in lines: [String], strictOnly: Bool) -> Int? {
        var found: Int?
        for (i, line) in lines.enumerated() where quoteDepth(of: line).depth == 0 {
            if isStrictDelimiter(line) {
                found = i
            } else if !strictOnly, isLenientDelimiter(line), lines.count - i - 1 <= lenientSignatureMaxTailLines {
                found = found ?? i
            }
        }
        return found
    }

    /// Where the signature that starts at `start` ends (exclusive): the end of the message, or the
    /// start of the attribution/quote that follows it (the blank line before the first quoted line,
    /// else the quoted line itself).
    private static func signatureEnd(in lines: [String], from start: Int) -> Int {
        guard let firstQuote = (start + 1..<lines.count).first(where: { quoteDepth(of: lines[$0]).depth > 0 })
        else { return lines.count }
        if let blank = (start + 1..<firstQuote).last(where: { lines[$0].trimmingCharacters(in: .whitespaces).isEmpty }) {
            return blank
        }
        return firstQuote
    }

    // MARK: parse

    /// The body as blocks: prose, quotes (same-depth runs) and at most one signature.
    static func parse(_ body: String) -> [Block] {
        guard !body.isEmpty else { return [] }
        let all = lines(of: body)
        let sigStart = signatureStart(in: all, strictOnly: false)
        let sigEnd = sigStart.map { signatureEnd(in: all, from: $0) }
        var blocks: [Block] = []
        var current: [String] = []
        var currentDepth = 0
        func flush() {
            guard !current.isEmpty else { return }
            blocks.append(currentDepth > 0 ? .quote(depth: currentDepth, lines: current) : .text(lines: current))
            current = []
        }
        var index = 0
        while index < all.count {
            if let sigStart, let sigEnd, index == sigStart {
                flush()
                blocks.append(.signature(lines: Array(all[sigStart..<sigEnd])))
                index = sigEnd
                continue
            }
            let depth = quoteDepth(of: all[index]).depth
            if !current.isEmpty, depth != currentDepth { flush() }
            currentDepth = depth
            current.append(all[index])
            index += 1
        }
        flush()
        return blocks
    }

    /// Whether `block` starts collapsed in the viewer (D10).
    static func startsFolded(_ block: Block) -> Bool {
        switch block {
        case .text: return false
        case .quote(_, let lines): return lines.count >= quoteFoldMinLines
        case .signature(let lines): return lines.count >= signatureFoldMinLines
        }
    }

    /// The label of a folded block: "14 quoted lines" / "Signature".
    static func foldLabel(_ block: Block) -> String {
        switch block {
        case .text: return ""
        case .quote(_, let lines): return String(localized: "\(lines.count) quoted lines")
        case .signature: return String(localized: "Signature")
        }
    }

    // MARK: reply helpers (slice 4)

    /// `body` without the sender's signature: from the strict `-- ` delimiter to the end of that
    /// signature (not through quoted text that follows it). No strict delimiter = unchanged.
    static func strippingSignature(_ body: String) -> String {
        let all = lines(of: body)
        guard let start = signatureStart(in: all, strictOnly: true) else { return body }
        let end = signatureEnd(in: all, from: start)
        // The blank line that separated the signature from what follows would double up with the one
        // before the delimiter: keep a single separator.
        var resume = end
        if resume < all.count, resume > 0, start > 0,
           all[resume].trimmingCharacters(in: .whitespaces).isEmpty,
           all[start - 1].trimmingCharacters(in: .whitespaces).isEmpty { resume += 1 }
        var kept = Array(all[..<start]) + Array(all[resume...])
        // Trailing blank lines left where the signature was.
        while let last = kept.last, last.trimmingCharacters(in: .whitespaces).isEmpty { kept.removeLast() }
        return kept.joined(separator: "\n")
    }

    /// Joins hard-wrapped prose lines of one quote depth into paragraphs (decision M3). Conservative:
    /// line A joins line B only when A is 60 to 80 characters (markers excluded) and does not end in
    /// two spaces, neither is blank, B does not start with whitespace, a list marker, `|` or `>`, and
    /// neither contains a tab or a run of 3+ spaces. Code, tables, lists, short lines and ASCII art
    /// are left alone, and nothing joins across quote depths. Text the core already unflowed has no
    /// short lines to join, so it passes through unchanged.
    static func rewrapped(_ body: String) -> String {
        let all = lines(of: body)
        var out: [String] = []
        var lastDepth = -1
        var lastPhysicalLength = 0
        for line in all {
            let (depth, start) = quoteDepth(of: line)
            let content = String(line[start...])
            if depth == lastDepth, let previous = out.last, canJoin(previousLength: lastPhysicalLength, previous: previousContent(previous, depth: depth), next: content) {
                out[out.count - 1] = previous.trimmingTrailingSpaces() + " " + content
                lastPhysicalLength = content.count
                continue
            }
            out.append(line)
            lastDepth = depth
            lastPhysicalLength = content.count
        }
        return out.joined(separator: "\n")
    }

    private static func previousContent(_ line: String, depth: Int) -> String {
        String(line[quoteDepth(of: line).contentStart...])
    }

    private static func canJoin(previousLength: Int, previous: String, next: String) -> Bool {
        guard (60...80).contains(previousLength) else { return false }
        if previous.hasSuffix("  ") { return false }
        let a = previous.trimmingCharacters(in: .whitespaces)
        let b = next
        guard !a.isEmpty, !b.trimmingCharacters(in: .whitespaces).isEmpty else { return false }
        guard let first = b.first, !first.isWhitespace, first != "|", first != ">" else { return false }
        if startsListItem(b) { return false }
        for s in [a, b] where s.contains("\t") || s.contains("   ") { return false }
        return true
    }

    private static func startsListItem(_ s: String) -> Bool {
        if let c = s.first, "-*+•".contains(c), s.dropFirst().first == " " { return true }
        // "1. text", "12) text", "a) text"
        let prefix = s.prefix(while: { $0.isLetter || $0.isNumber })
        guard !prefix.isEmpty, prefix.count <= 3 else { return false }
        let rest = s.dropFirst(prefix.count)
        return (rest.hasPrefix(".") || rest.hasPrefix(")")) && rest.dropFirst().first == " "
            && (prefix.allSatisfy(\.isNumber) || prefix.count == 1)
    }
}

private extension String {
    func trimmingTrailingSpaces() -> String {
        var s = self
        while s.last == " " { s.removeLast() }
        return s
    }
}
