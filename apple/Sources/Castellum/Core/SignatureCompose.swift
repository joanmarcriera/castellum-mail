// Castellum - choosing a signature per message, and HTML signatures
//. Design: project-docs/signature-picker-design.md.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure string logic, no UI and no storage:
// - `SignatureSwitch` wraps `SignatureSupport.swapForAccountChange` (left
//   untouched, PR #225 owns its delimiter detection) and reports whether a
//   swap actually happened, so the compose picker can refuse visibly instead
//   of silently doing nothing to a signature the user edited.
// - `SignatureHTML` derives a plain-text version from HTML and splices a
//   signature's HTML into the Markdown source the core renders for the
//   text/html part. The core's compose sanitiser then cleans the whole
//   document, so signature HTML is held to the same policy as the body.

import Foundation

enum SignatureSwitch {
    struct Outcome: Equatable {
        var body: String
        /// False when the body was left alone because the signature in it was
        /// edited (or a hand-typed delimiter is there): the choice was refused.
        var applied: Bool
    }

    /// Replaces the `old` signature block in `body` with `new`'s (either may
    /// be `""`, meaning none). Never touches an edited block.
    ///
    /// `insertWhenRemoved`: when the old block was deleted outright (no
    /// delimiter line left), insert `new` as if there had been none. The
    /// compose picker passes true (the user asked for a signature); a From
    /// change passes false, keeping the S26 rule that a signature the user
    /// removed is not re-added behind their back.
    static func apply(from old: String, to new: String, in body: String,
                      insertWhenRemoved: Bool) -> Outcome {
        let swapped = SignatureSupport.swapForAccountChange(from: old, to: new, in: body)
        if swapped != body { return Outcome(body: swapped, applied: true) }
        let oldBlock = SignatureSupport.block(for: old)
        if oldBlock == SignatureSupport.block(for: new) { return Outcome(body: body, applied: true) }
        if insertWhenRemoved, !oldBlock.isEmpty, !hasDelimiterLine(body) {
            return Outcome(body: SignatureSupport.insert(signature: new, into: body), applied: true)
        }
        return Outcome(body: body, applied: false)
    }

    /// True when some line reads as a signature delimiter: `--` (trailing
    /// spaces aside) or the en/em dash smart-dash substitution makes of it
    /// (the same look-alikes PR #225 recognises).
    static func hasDelimiterLine(_ body: String) -> Bool {
        // `isNewline`, not "\n": in Swift "\r\n" is ONE Character, so a
        // CRLF body never splits on "\n" at all.
        body.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline).contains { line in
            var l = Substring(line)
            while let last = l.last, last.isWhitespace { l.removeLast() }
            return l == "--" || l == "\u{2013}" || l == "\u{2014}"
        }
    }

    /// The saved signature a reopened draft still carries: the one whose
    /// block is the draft's signature region (see `blockRange`); the latest
    /// in the body wins, and on a tie the longest, so "Ada" is never chosen
    /// for a draft carrying "Ada\nLovelace".
    static func recognise(in body: String, among items: [SignatureItem]) -> SignatureItem? {
        items.compactMap { item in blockRange(of: item.plainText, in: body).map { (item, $0) } }
            .max { a, b in
                a.1.lowerBound != b.1.lowerBound
                    ? a.1.lowerBound < b.1.lowerBound
                    : a.0.plainText.count < b.0.plainText.count
            }?.0
    }

    /// Range of `signature`'s exact block when it IS a whole signature region
    /// of `body`: it starts a line, and is followed only by whitespace to the
    /// end or by a blank line (the gap above the quote). The LAST such
    /// occurrence wins, so a pasted or forwarded copy higher up is not taken
    /// for the real signature. Nil for an empty signature or none found.
    static func blockRange(of signature: String, in body: String) -> Range<String.Index>? {
        let block = SignatureSupport.block(for: signature)
        guard !block.isEmpty else { return nil }
        var upper = body.endIndex
        while let r = body.range(of: block, options: .backwards, range: body.startIndex..<upper) {
            let startsLine = r.lowerBound == body.startIndex || body[body.index(before: r.lowerBound)].isNewline
            if startsLine && endsRegion(body, at: r.upperBound) { return r }
            guard r.upperBound > body.startIndex else { break }
            upper = body.index(before: r.upperBound)
        }
        return nil
    }

    /// True when `body` from `i` is whitespace only, or a newline followed by
    /// a blank line.
    private static func endsRegion(_ body: String, at i: String.Index) -> Bool {
        let rest = body[i...]
        if rest.allSatisfy(\.isWhitespace) { return true }
        guard rest.first?.isNewline == true else { return false }
        var j = rest.index(after: rest.startIndex)
        while j < rest.endIndex, rest[j] == " " || rest[j] == "\t" { j = rest.index(after: j) }
        return j < rest.endIndex && rest[j].isNewline
    }
}

enum SignatureHTML {
    /// The caption the editor shows; mirrors `sanitize_compose_html` in
    /// `native/crates/castellum-mail/src/compose_markdown.rs`.
    static let allowedTagsSummary = "p br strong em del code pre blockquote ul ol li h1–h6 hr a table thead tbody tr th td"

    /// `<b>`/`<i>` become `<strong>`/`<em>`: the compose sanitiser only keeps
    /// the latter, and the former are what people type by hand.
    static func normalised(_ html: String) -> String {
        var out = html
        for (from, to) in [("b", "strong"), ("i", "em")] {
            out = replacing(#"<(/?)\#(from)(\s[^>]*)?>"#, in: out, with: "<$1\(to)>")
        }
        return out
    }

    /// The signature HTML as one CommonMark HTML block: wrapped in `<div>`
    /// (a block-level start tag, so it can interrupt anything) with blank
    /// lines removed (a blank line would end the block and the rest would be
    /// parsed as Markdown — indented lines as code). The wrapper is not in
    /// the compose allowlist, so the sanitiser unwraps it.
    static func markdownBlock(_ html: String) -> String {
        let lines = normalised(html)
            .split(omittingEmptySubsequences: false, whereSeparator: \.isNewline)   // "\r\n" is one Character
            .filter { !$0.allSatisfy(\.isWhitespace) }
        return "<div>\n" + lines.joined(separator: "\n") + "\n</div>\n"
    }

    /// The Markdown source to render for the HTML part: `body` with the
    /// unmodified plain block of the inserted signature (`-- \n<plain>`)
    /// replaced by the delimiter and the signature's HTML. Unchanged when
    /// there is no HTML, or the block was edited (the user's text renders as
    /// typed; nothing is substituted behind their back).
    static func markdownSource(body: String, plain: String, html: String?) -> String {
        guard let html, !html.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return body }
        guard let range = SignatureSwitch.blockRange(of: plain, in: body) else { return body }
        return body.replacingCharacters(in: range, with: "-- \n\n" + markdownBlock(html))
    }

    /// The editor preview: the delimiter and the signature, as the HTML part
    /// will show them (render with `CastellumCore.renderMarkdownPreview`).
    static func previewSource(_ html: String) -> String {
        "-- \n\n" + markdownBlock(html)
    }

    /// A plain-text rendition of signature HTML, for the text body when no
    /// plain version was written: line-breaking tags become newlines, other
    /// tags go, common entities are decoded, blank-line runs collapse.
    static func plainText(from html: String) -> String {
        var s = html.replacingOccurrences(of: "\r\n", with: "\n")
        // What the compose sanitiser removes with its content (comments,
        // script, style) must not reach the plain text either, or the two
        // parts would disagree (and a comment would leak into the text part).
        s = replacing(#"(?s)<!--.*?(-->|$)"#, in: s, with: "")
        s = replacing(#"(?s)<(script|style)\b.*?(</\1\s*>|$)"#, in: s, with: "")
        s = replacing(#"\s+"#, in: s, with: " ")   // source whitespace is not layout
        s = replacing(#"<br\s*/?>"#, in: s, with: "\n")
        s = replacing(#"<hr\b[^>]*>"#, in: s, with: "\n")
        // Paragraph-like blocks end with a blank line, line-like ones with a break.
        s = replacing(#"</(p|h[1-6]|blockquote|pre|table|ul|ol)\s*>"#, in: s, with: "\n\n")
        s = replacing(#"</(div|li|tr)\s*>"#, in: s, with: "\n")
        s = replacing(#"<[^>]*>"#, in: s, with: "")
        s = decodeEntities(s)
        // No control characters in the text part (a raw NUL, or one decoded
        // from &#0;); line breaks are ours, inserted above.
        s = String(String.UnicodeScalarView(s.unicodeScalars.filter {
            $0 == "\n" || $0.properties.generalCategory != .control
        }))
        let lines = s.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline)
            .map { replacing(#" {2,}"#, in: $0.trimmingCharacters(in: .whitespaces), with: " ") }
        var out: [String] = []
        for line in lines where !(line.isEmpty && (out.last?.isEmpty ?? true)) { out.append(line) }
        return out.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private static func decodeEntities(_ s: String) -> String {
        var out = s
        for (entity, char) in [("&nbsp;", " "), ("&lt;", "<"), ("&gt;", ">"), ("&quot;", "\""),
                               ("&#39;", "'"), ("&apos;", "'"), ("&mdash;", "—"), ("&ndash;", "–")] {
            out = out.replacingOccurrences(of: entity, with: char, options: .caseInsensitive)
        }
        // Numeric references (&#233; &#xE9;), then &amp; last so "&amp;lt;" stays "&lt;".
        let regex = try! NSRegularExpression(pattern: #"&#(x?)([0-9a-fA-F]+);"#)
        let ns = out as NSString
        var result = ""
        var last = 0
        for m in regex.matches(in: out, range: NSRange(location: 0, length: ns.length)) {
            result += ns.substring(with: NSRange(location: last, length: m.range.location - last))
            let hex = ns.substring(with: m.range(at: 1)).lowercased() == "x"
            let digits = ns.substring(with: m.range(at: 2))
            if let v = UInt32(digits, radix: hex ? 16 : 10), let scalar = Unicode.Scalar(v) {
                // A control character (&#0;, &#7;, ...) is dropped; an encoded
                // tab or line break is just whitespace, like in the source.
                if scalar.properties.generalCategory != .control {
                    result += String(Character(scalar))
                } else if scalar.properties.isWhitespace {
                    result += " "
                }
            } else {
                result += ns.substring(with: m.range)
            }
            last = m.range.location + m.range.length
        }
        result += ns.substring(from: last)
        return result.replacingOccurrences(of: "&amp;", with: "&", options: .caseInsensitive)
    }

    private static func replacing(_ pattern: String, in s: String, with template: String) -> String {
        // Patterns are literals in this file; a bad one is a programming error.
        let regex = try! NSRegularExpression(pattern: pattern, options: [.caseInsensitive])
        return regex.stringByReplacingMatches(
            in: s, range: NSRange(location: 0, length: (s as NSString).length), withTemplate: template)
    }
}
