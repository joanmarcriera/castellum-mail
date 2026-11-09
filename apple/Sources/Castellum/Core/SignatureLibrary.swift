// Castellum - named signatures, several per account.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure value type: no UI, no storage. `Preferences` persists it as JSON in
// UserDefaults (presentation-side only, see DECISIONS.md 2026-09-17 and
// 2026-09-30). A new compose window starts with the account default
// (`defaultText(for:)`); the compose footer picker can switch to any other
// of the account's signatures for one message (, see
// project-docs/signature-picker-design.md). Automatic selection rules are
// MM-6; `SignatureItem` is the place those will hang off.

import Foundation

struct SignatureItem: Codable, Equatable, Identifiable {
    let id: String
    var name: String
    /// The account this signature belongs to (`Account.id`).
    var accountId: String
    /// Plain text; the `-- ` delimiter is added by `SignatureSupport`.
    var text: String
    /// Optional HTML version, stored verbatim as typed. Used
    /// only for the HTML alternative of a message sent with Markdown on, and
    /// sanitised there by the core's compose policy (`SignatureHTML`). `nil`
    /// in every blob written before #2286 (synthesised Codable treats a
    /// missing optional key as nil).
    var html: String?

    init(id: String, name: String, accountId: String, text: String, html: String? = nil) {
        self.id = id
        self.name = name
        self.accountId = accountId
        self.text = text
        self.html = html
    }

    /// The HTML version when it has any content, else nil.
    var htmlIfAny: String? {
        guard let html, !html.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return html
    }

    /// What compose puts in the (plain-text / Markdown) body: `text`, or,
    /// when that is blank and an HTML version exists, the HTML's text.
    var plainText: String {
        if !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return text }
        return htmlIfAny.map(SignatureHTML.plainText(from:)) ?? ""
    }
}

struct SignatureLibrary: Codable, Equatable {
    private(set) var items: [SignatureItem] = []
    /// accountId -> id of that account's default signature.
    private(set) var defaults: [String: String] = [:]

    init() {}

    /// Builds a library from the pre-GUI-4 storage (one text per account):
    /// each becomes a signature named `defaultName` and the account default.
    init(migratingLegacy legacy: [String: String], defaultName: String = "Signature") {
        for accountId in legacy.keys.sorted() {
            guard let text = legacy[accountId],
                  !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { continue }
            add(name: defaultName, accountId: accountId, text: text)
        }
    }

    func items(for accountId: String) -> [SignatureItem] {
        items.filter { $0.accountId == accountId }
    }

    func item(id: String) -> SignatureItem? {
        items.first { $0.id == id }
    }

    /// The account's default signature, if it has one.
    func defaultItem(for accountId: String) -> SignatureItem? {
        defaults[accountId].flatMap(item(id:))
    }

    /// The text compose inserts for `accountId`; `""` when none. Falls back
    /// to the HTML version's text when the plain text is blank.
    func defaultText(for accountId: String) -> String {
        defaultItem(for: accountId)?.plainText ?? ""
    }

    func isDefault(_ id: String) -> Bool {
        guard let item = item(id: id) else { return false }
        return defaults[item.accountId] == id
    }

    /// Adds a signature; it becomes the account default when the account had
    /// none. Returns the new id.
    @discardableResult
    mutating func add(name: String, accountId: String, text: String = "", id: String = UUID().uuidString) -> String {
        items.append(SignatureItem(id: id, name: name, accountId: accountId, text: text))
        if defaultItem(for: accountId) == nil { defaults[accountId] = id }
        return id
    }

    mutating func rename(id: String, to name: String) {
        guard let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].name = name
    }

    mutating func setText(id: String, _ text: String) {
        guard let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].text = text
    }

    /// Sets the HTML version; blank stores nil (no HTML version).
    mutating func setHTML(id: String, _ html: String) {
        guard let i = items.firstIndex(where: { $0.id == id }) else { return }
        items[i].html = html.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : html
    }

    /// The next free `base` / "`base` N" name for `accountId` (the caller
    /// passes the localised word for "Signature").
    func suggestedName(for accountId: String, base: String = "Signature") -> String {
        let names = Set(items(for: accountId).map(\.name))
        if !names.contains(base) { return base }
        var n = 2
        while names.contains("\(base) \(n)") { n += 1 }
        return "\(base) \(n)"
    }

    /// Moves a signature to another account. It loses default status on the
    /// old account (which falls back to its first remaining signature) and
    /// becomes the new account's default only if that account had none.
    mutating func move(id: String, toAccount accountId: String) {
        guard let i = items.firstIndex(where: { $0.id == id }), items[i].accountId != accountId else { return }
        let old = items[i].accountId
        items[i].accountId = accountId
        if defaults[old] == id { defaults[old] = nil }
        repairDefault(for: old)
        if defaultItem(for: accountId) == nil { defaults[accountId] = id }
    }

    mutating func setDefault(id: String) {
        guard let item = item(id: id) else { return }
        defaults[item.accountId] = id
    }

    /// Removes a signature; if it was the default the account's first
    /// remaining signature (if any) takes over.
    mutating func delete(id: String) {
        guard let item = item(id: id) else { return }
        items.removeAll { $0.id == id }
        if defaults[item.accountId] == id { defaults[item.accountId] = nil }
        repairDefault(for: item.accountId)
    }

    /// Drops every signature (and the default) of ONE account. Used when that account is
    /// removed; deliberately targeted, never "everything not in the live list", because
    /// signatures are user-authored with no other copy (#3439).
    mutating func removeAll(forAccount accountId: String) {
        items.removeAll { $0.accountId == accountId }
        defaults[accountId] = nil
    }

    /// Drops signatures for accounts that no longer exist.
    mutating func prune(keepingAccounts ids: Set<String>) {
        for item in items where !ids.contains(item.accountId) { delete(id: item.id) }
    }

    private mutating func repairDefault(for accountId: String) {
        if defaultItem(for: accountId) == nil {
            defaults[accountId] = items(for: accountId).first?.id   // nil removes the key
        }
    }
}
