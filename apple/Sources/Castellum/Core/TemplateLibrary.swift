// Castellum - the user's templates and snippets.
//
// Presentation-side, like `SignatureLibrary`: one Codable blob in UserDefaults
// (key `templateLibrary`); the Rust core only ever sees the finished body and
// subject. Slice 1 holds the per-account reply attribution; slice 2 adds the
// snippet library; slice 3 the inline `;abbrev` + Tab switch (OFF by default);
// slice 4 the per-account forward header. Every key decodes as optional, so a
// blob written by an older build still loads.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// A named, reusable piece of text with placeholders (see `TemplateFormatter`).
struct SnippetItem: Codable, Equatable, Identifiable {
    /// Limits, enforced by `TemplateLibrary.validate`.
    static let maxNameLength = 80
    static let maxAbbreviationLength = 24
    static let maxTextBytes = 64 * 1024

    let id: String
    /// Shown in the picker, 1...80 characters.
    var name: String
    /// For the inline `;abbrev` trigger: `[a-z0-9-]{1,24}`, unique ignoring case.
    var abbreviation: String?
    /// nil = every account.
    var accountId: String?
    /// Fills an EMPTY Subject field when the snippet is inserted (slice 4). Single line.
    var subject: String?
    /// The format string (at most 64 KiB).
    var text: String
}

struct TemplateLibrary: Codable, Equatable {
    /// accountId -> reply attribution format. Absent = the built-in line.
    private(set) var replyAttribution: [String: String] = [:]
    /// accountId -> forward header block format (slice 4). Absent = the built-in block.
    private(set) var forwardHeader: [String: String] = [:]
    private(set) var snippets: [SnippetItem] = []
    /// Slice 3: expand `;abbrev` + Tab in the compose body. OFF until Marc decides.
    var inlineTriggerEnabled = false

    init() {}

    private enum CodingKeys: String, CodingKey {
        case replyAttribution, forwardHeader, snippets, inlineTriggerEnabled
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        replyAttribution = try c.decodeIfPresent([String: String].self, forKey: .replyAttribution) ?? [:]
        forwardHeader = try c.decodeIfPresent([String: String].self, forKey: .forwardHeader) ?? [:]
        snippets = try c.decodeIfPresent([SnippetItem].self, forKey: .snippets) ?? []
        inlineTriggerEnabled = try c.decodeIfPresent(Bool.self, forKey: .inlineTriggerEnabled) ?? false
    }

    // MARK: reply attribution and forward header (per account)

    /// The account's custom format, or nil for the built-in `On <date>, <who> wrote:`.
    func attribution(for accountId: String) -> String? {
        replyAttribution[accountId]
    }

    /// Set (or, when blank, clear) the account's reply attribution format.
    mutating func setAttribution(_ format: String?, for accountId: String) {
        replyAttribution[accountId] = Self.nonBlank(format)
    }

    func forwardHeaderFormat(for accountId: String) -> String? {
        forwardHeader[accountId]
    }

    mutating func setForwardHeader(_ format: String?, for accountId: String) {
        forwardHeader[accountId] = Self.nonBlank(format)
    }

    private static func nonBlank(_ format: String?) -> String? {
        guard let format, !format.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return format
    }

    // MARK: snippets

    /// The snippets offered for `accountId`: every global one and that account's own, by name.
    func snippets(for accountId: String) -> [SnippetItem] {
        snippets
            .filter { $0.accountId == nil || $0.accountId == accountId }
            .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    func snippet(id: String) -> SnippetItem? { snippets.first { $0.id == id } }

    /// The snippet an inline `;abbrev` names for `accountId` (global or the account's own;
    /// the account's own wins), ignoring case.
    func snippet(abbreviation: String, accountId: String) -> SnippetItem? {
        let key = abbreviation.lowercased()
        let matches = snippets.filter { $0.abbreviation?.lowercased() == key
            && ($0.accountId == nil || $0.accountId == accountId) }
        return matches.first { $0.accountId == accountId } ?? matches.first
    }

    /// Why `item` cannot be stored as it is; empty = fine. A duplicate abbreviation is
    /// checked against every OTHER snippet, whatever its account (the trigger would be ambiguous).
    func validate(_ item: SnippetItem) -> [TemplateProblem] {
        var problems: [TemplateProblem] = []
        let name = item.name.trimmingCharacters(in: .whitespacesAndNewlines)
        if name.isEmpty {
            problems.append(TemplateProblem(message: String(localized: "A snippet needs a name."), source: ""))
        } else if name.count > SnippetItem.maxNameLength {
            problems.append(TemplateProblem(
                message: String(localized: "The name is longer than \(SnippetItem.maxNameLength) characters."), source: ""))
        }
        if let abbreviation = item.abbreviation, !abbreviation.isEmpty {
            if !Self.isValidAbbreviation(abbreviation) {
                problems.append(TemplateProblem(
                    message: String(localized: "An abbreviation uses a-z, 0-9 and - only, up to \(SnippetItem.maxAbbreviationLength) characters."),
                    source: abbreviation))
            } else if snippets.contains(where: { $0.id != item.id && $0.abbreviation?.lowercased() == abbreviation.lowercased() }) {
                problems.append(TemplateProblem(
                    message: String(localized: "Another snippet already uses this abbreviation."), source: abbreviation))
            }
        }
        if item.text.utf8.count > SnippetItem.maxTextBytes {
            problems.append(TemplateProblem(message: String(localized: "The text is longer than 64 KiB."), source: ""))
        }
        if let subject = item.subject, subject.contains(where: \.isNewline) {
            problems.append(TemplateProblem(message: String(localized: "A subject is a single line."), source: ""))
        }
        return problems
    }

    static func isValidAbbreviation(_ s: String) -> Bool {
        guard (1...SnippetItem.maxAbbreviationLength).contains(s.count) else { return false }
        return s.unicodeScalars.allSatisfy { ("a"..."z").contains($0) || ("0"..."9").contains($0) || $0 == "-" }
    }

    /// Add a snippet; returns it. The caller validates first (`validate`); a new, untitled
    /// snippet with an empty text is fine, so the editor can create it and let the user fill it in.
    @discardableResult
    mutating func add(name: String, accountId: String?, text: String = "") -> SnippetItem {
        let item = SnippetItem(
            id: UUID().uuidString, name: name, abbreviation: nil, accountId: accountId, subject: nil, text: text)
        snippets.append(item)
        return item
    }

    /// Store an edited snippet. Refused (nothing changes) while `validate` reports problems,
    /// which are returned; a snippet that is not in the library is ignored.
    @discardableResult
    mutating func update(_ item: SnippetItem) -> [TemplateProblem] {
        let problems = validate(item)
        guard problems.isEmpty, let index = snippets.firstIndex(where: { $0.id == item.id }) else { return problems }
        var stored = item
        if stored.abbreviation?.isEmpty == true { stored.abbreviation = nil }
        if stored.subject?.isEmpty == true { stored.subject = nil }
        snippets[index] = stored
        return []
    }

    mutating func delete(id: String) {
        snippets.removeAll { $0.id == id }
    }

    /// A name for a new snippet that no other snippet of that scope uses ("Snippet", "Snippet 2", ...).
    func suggestedName(accountId: String?, base: String) -> String {
        let taken = Set(snippets.filter { $0.accountId == accountId }.map { $0.name.lowercased() })
        if !taken.contains(base.lowercased()) { return base }
        var n = 2
        while taken.contains("\(base) \(n)".lowercased()) { n += 1 }
        return "\(base) \(n)"
    }

    /// Drop formats and snippets of accounts that no longer exist (global snippets stay).
    /// Parity with `SignatureLibrary`; not wired to account removal (stale entries are inert).
    mutating func prune(keepingAccounts ids: Set<String>) {
        replyAttribution = replyAttribution.filter { ids.contains($0.key) }
        forwardHeader = forwardHeader.filter { ids.contains($0.key) }
        snippets.removeAll { $0.accountId != nil && !ids.contains($0.accountId!) }
    }
}
