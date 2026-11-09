// Castellum - placeholder templates for reply attributions.
//
// A format string such as `El ${date:d/M/yyyy}, ${sender.name} va escriure:`
// is parsed into literals and placeholders and rendered against a context
// (the original sender/date/subject, the writer's identity, "now").
//
// Syntax: `${name}`, `${name:argument}`, `${name|fallback}`, `${name:arg|fb}`;
// `$${` writes a literal `${`. Dates take Unicode (ICU) patterns.
//
// Safety: values come from received mail and are UNTRUSTED. They are
// substituted after tokenising and never re-expanded (a sender called
// `${cursor}` appears as that text); in single-line mode every line break in a
// format or value becomes a space; a value is capped at 998 characters.
// `parse`/`render` are total: nothing throws, a malformed format still renders.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

struct TemplateContext: Equatable {
    var senderName: String?
    var senderEmail: String?
    /// Unix seconds of the original message.
    var sentAt: Int64?
    var subject: String?
    var recipientName: String?
    var recipientEmail: String?
    var myName: String?
    var myEmail: String?
    var now: Date = Date()
    var locale: Locale = .current
    var timeZone: TimeZone = .current
}

enum TemplateToken: Equatable {
    case literal(String)
    case placeholder(name: String, argument: String?, fallback: String?, source: String)
}

struct RenderedTemplate: Equatable {
    var text: String
    /// UTF-16 offset of the first `${cursor}`; nil when absent (or single-line).
    var caretOffset: Int?
}

struct TemplateProblem: Equatable {
    var message: String
    var source: String
}

/// One documented placeholder; the single source for the help popover and
/// `project-docs/template-placeholders.md`.
struct PlaceholderDoc: Equatable {
    let name: String
    let meaning: String
    let example: String
}

enum TemplateFormatter {
    enum Mode { case singleLine, multiLine }

    /// Longest value inserted for one placeholder in single-line mode.
    static let maxValueLength = 998

    static let placeholders: [PlaceholderDoc] = [
        PlaceholderDoc(name: "sender", meaning: "Sender's name, else their address", example: "Ada Lovelace"),
        PlaceholderDoc(name: "sender.name", meaning: "Sender's display name", example: "Ada Lovelace"),
        PlaceholderDoc(name: "sender.first", meaning: "First word of the sender's name", example: "Ada"),
        PlaceholderDoc(name: "sender.email", meaning: "Sender's address", example: "ada@example.org"),
        PlaceholderDoc(name: "date", meaning: "Date of the original message", example: "16 Oct 2026, 09:30"),
        PlaceholderDoc(name: "date:PATTERN", meaning: "The same with a Unicode date pattern, e.g. ${date:d/M/yyyy}", example: "16/10/2026"),
        PlaceholderDoc(name: "subject", meaning: "Subject of the original (or the Subject field)", example: "Lunch?"),
        PlaceholderDoc(name: "recipient", meaning: "First To recipient of the message being written", example: "Grace Hopper"),
        PlaceholderDoc(name: "recipient.name", meaning: "Their display name", example: "Grace Hopper"),
        PlaceholderDoc(name: "recipient.first", meaning: "First word of their name", example: "Grace"),
        PlaceholderDoc(name: "recipient.email", meaning: "Their address", example: "grace@example.org"),
        PlaceholderDoc(name: "my.name", meaning: "Your name on this account", example: "Marc"),
        PlaceholderDoc(name: "my.email", meaning: "Your address on this account", example: "marc@example.org"),
        PlaceholderDoc(name: "today", meaning: "Today's date and time", example: "16 Oct 2026, 09:41"),
        PlaceholderDoc(name: "today:PATTERN", meaning: "Today with a Unicode date pattern", example: "2026-10-16"),
        PlaceholderDoc(name: "cursor", meaning: "Where the caret lands after inserting a snippet", example: ""),
    ]

    private static let knownNames: Set<String> = [
        "sender", "sender.name", "sender.first", "sender.email", "date", "subject",
        "recipient", "recipient.name", "recipient.first", "recipient.email",
        "my.name", "my.email", "today", "cursor",
    ]

    // MARK: parse

    /// Tokenise `format`. Total: an unclosed `${` is literal text.
    static func parse(_ format: String) -> [TemplateToken] {
        var tokens: [TemplateToken] = []
        var literal = ""
        let chars = Array(format)
        var i = 0
        func flush() {
            if !literal.isEmpty { tokens.append(.literal(literal)); literal = "" }
        }
        while i < chars.count {
            if chars[i] == "$", i + 2 < chars.count, chars[i + 1] == "$", chars[i + 2] == "{" {
                literal += "${"
                i += 3
                continue
            }
            if chars[i] == "$", i + 1 < chars.count, chars[i + 1] == "{" {
                guard let close = chars[(i + 2)...].firstIndex(of: "}") else {
                    literal += String(chars[i...])   // unclosed: the rest is literal
                    break
                }
                let inner = String(chars[(i + 2)..<close])
                let source = String(chars[i...close])
                flush()
                tokens.append(placeholder(inner: inner, source: source))
                i = close + 1
                continue
            }
            literal.append(chars[i])
            i += 1
        }
        flush()
        return tokens
    }

    private static func placeholder(inner: String, source: String) -> TemplateToken {
        var name = inner
        var argument: String?
        var fallback: String?
        if let bar = name.firstIndex(of: "|") {
            fallback = String(name[name.index(after: bar)...])
            name = String(name[..<bar])
        }
        if let colon = name.firstIndex(of: ":") {
            argument = String(name[name.index(after: colon)...])
            name = String(name[..<colon])
        }
        return .placeholder(name: name, argument: argument, fallback: fallback, source: source)
    }

    // MARK: problems

    static func problems(in format: String, mode: Mode) -> [TemplateProblem] {
        var out: [TemplateProblem] = []
        for token in parse(format) {
            guard case let .placeholder(name, argument, _, source) = token else { continue }
            if !knownNames.contains(name) {
                out.append(TemplateProblem(message: String(localized: "Unknown placeholder"), source: source))
            } else if argument != nil, name != "date", name != "today" {
                out.append(TemplateProblem(message: String(localized: "This placeholder takes no argument"), source: source))
            } else if name == "cursor", mode == .singleLine {
                out.append(TemplateProblem(message: String(localized: "A cursor marker needs a snippet"), source: source))
            }
        }
        if format.contains("${"), !format.contains("}") {
            out.append(TemplateProblem(message: String(localized: "Missing closing brace"), source: "${"))
        }
        return out
    }

    // MARK: render

    static func render(_ format: String, context: TemplateContext, mode: Mode) -> RenderedTemplate {
        var text = ""
        var caret: Int?
        for token in parse(format) {
            switch token {
            case .literal(let s):
                text += s
            case let .placeholder(name, argument, fallback, source):
                if name == "cursor" {
                    if mode == .multiLine, caret == nil { caret = text.utf16.count }
                    continue
                }
                guard knownNames.contains(name) else { text += source; continue }  // T7
                var value = self.value(name: name, argument: argument, context: context) ?? ""
                if value.isEmpty { value = fallback ?? "" }
                if mode == .singleLine, value.count > maxValueLength {
                    value = String(value.prefix(maxValueLength))
                }
                text += value
            }
        }
        if mode == .singleLine {
            text = collapse(text)
            return RenderedTemplate(text: text, caretOffset: nil)
        }
        return RenderedTemplate(text: text, caretOffset: caret)
    }

    /// Every line break becomes a space, runs of whitespace collapse, ends trimmed.
    private static func collapse(_ s: String) -> String {
        let spaced = s.unicodeScalars.map { scalar -> Character in
            CharacterSet.whitespacesAndNewlines.contains(scalar)
                || scalar == "\u{2028}" || scalar == "\u{2029}" ? " " : Character(scalar)
        }
        var out = ""
        var lastWasSpace = true   // also trims the start
        for ch in spaced {
            if ch == " " {
                if !lastWasSpace { out.append(" ") }
                lastWasSpace = true
            } else {
                out.append(ch)
                lastWasSpace = false
            }
        }
        if out.hasSuffix(" ") { out.removeLast() }
        return out
    }

    private static func firstWord(_ name: String?) -> String? {
        name?.split(whereSeparator: { $0.isWhitespace }).first.map(String.init)
    }

    private static func nonEmpty(_ s: String?) -> String? {
        guard let s, !s.trimmingCharacters(in: .whitespaces).isEmpty else { return nil }
        return s
    }

    private static func value(name: String, argument: String?, context c: TemplateContext) -> String? {
        switch name {
        case "sender": return nonEmpty(c.senderName) ?? nonEmpty(c.senderEmail)
        case "sender.name": return nonEmpty(c.senderName)
        case "sender.first": return firstWord(c.senderName)
        case "sender.email": return nonEmpty(c.senderEmail)
        case "date":
            guard let sentAt = c.sentAt else { return nil }
            return formatDate(Date(timeIntervalSince1970: TimeInterval(sentAt)), pattern: argument, context: c)
        case "subject": return nonEmpty(c.subject)
        case "recipient": return nonEmpty(c.recipientName) ?? nonEmpty(c.recipientEmail)
        case "recipient.name": return nonEmpty(c.recipientName)
        case "recipient.first": return firstWord(c.recipientName)
        case "recipient.email": return nonEmpty(c.recipientEmail)
        case "my.name": return nonEmpty(c.myName)
        case "my.email": return nonEmpty(c.myEmail)
        case "today": return formatDate(c.now, pattern: argument, context: c)
        default: return nil
        }
    }

    private static func formatDate(_ date: Date, pattern: String?, context c: TemplateContext) -> String {
        let f = DateFormatter()
        f.locale = c.locale
        f.timeZone = c.timeZone
        if let pattern, !pattern.isEmpty {
            f.dateFormat = pattern
        } else {
            f.dateStyle = .medium
            f.timeStyle = .short
        }
        return f.string(from: date)
    }
}
