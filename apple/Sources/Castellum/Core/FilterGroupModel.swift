// Castellum - nested smart-mailbox conditions, Swift side.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Mirrors Rust `castellum_models::FilterGroup` (native/crates/castellum-models/
// src/filter.rs) exactly as it crosses the C ABI: a group has an `op`
// (all/any/none), `conditions` and nested `groups`. The Rust core is the only
// evaluator; this file is data + pure editing rules (which operators a field
// offers, what a blank condition looks like, whether the tree may be saved),
// unit-tested in FilterGroupModelTests so the editor UI stays a thin shell.
//
// Every Rust variant decodes, including ones the editor does not offer
// (draft/deleted flags, mailbox/account ids, gist topic): a stored group must
// never fail to load just because the UI cannot edit one of its rows.

import Foundation

enum FilterBoolOp: String, Codable, CaseIterable, Identifiable {
    case and, or, none
    var id: String { rawValue }
    /// Reads as "Match [all] of the following".
    var label: String {
        switch self {
        case .and: return String(localized: "all")
        case .or: return String(localized: "any")
        case .none: return String(localized: "none")
        }
    }
}

enum FilterFieldKind: String, Codable, CaseIterable, Identifiable {
    case from, to, cc, bcc, subject, body
    case isRead = "is_read"
    case isStarred = "is_starred"
    case isAnswered = "is_answered"
    case isDraft = "is_draft"
    case isDeleted = "is_deleted"
    case date
    case sizeBytes = "size_bytes"
    case hasAttachments = "has_attachments"
    case mailboxId = "mailbox_id"
    case accountId = "account_id"
    case attachmentName = "attachment_name"
    case attachmentType = "attachment_type"
    case tag
    case gistTopic = "gist_topic"
    /// MM-1c: any stored header, named by the row's `headerName`. On the wire
    /// the field is the object `{"header":"List-Id"}` (see `FilterConditionDTO`).
    case header

    var id: String { rawValue }

    /// The fields the editor offers, in menu order. Mailbox / account
    /// restriction lives in the separate scope line, not as a condition row.
    static let editable: [FilterFieldKind] = [
        .from, .to, .cc, .bcc, .subject, .body,
        .isRead, .isStarred, .isAnswered, .hasAttachments,
        .date, .sizeBytes, .attachmentName, .attachmentType,
        .tag, .header,
    ]

    /// The field picker's options for one row: `offered`, plus the row's own
    /// field when `offered` leaves it out (a rule that already holds a Header
    /// condition, hand-written or saved by a newer build, while the Rules
    /// editor hides Header). Without it the picker would have no tag for the
    /// row's selection and show it blank.
    static func pickerOptions(offered: [FilterFieldKind], current: FilterFieldKind) -> [FilterFieldKind] {
        offered.contains(current) ? offered : offered + [current]
    }

    var label: String {
        switch self {
        case .from: return String(localized: "From")
        case .to: return String(localized: "To")
        case .cc: return String(localized: "Cc")
        case .bcc: return String(localized: "Bcc")
        case .subject: return String(localized: "Subject")
        case .body: return String(localized: "Body")
        case .isRead: return String(localized: "Read")
        case .isStarred: return String(localized: "Starred")
        case .isAnswered: return String(localized: "Answered")
        case .isDraft: return String(localized: "Draft")
        case .isDeleted: return String(localized: "Deleted")
        case .date: return String(localized: "Date")
        case .sizeBytes: return String(localized: "Size")
        case .hasAttachments: return String(localized: "Has attachments")
        case .mailboxId: return String(localized: "Mailbox")
        case .accountId: return String(localized: "Account")
        case .attachmentName: return String(localized: "Attachment name")
        case .attachmentType: return String(localized: "Attachment type")
        case .tag: return String(localized: "Tag")
        case .gistTopic: return String(localized: "Topic")
        case .header: return String(localized: "Header")
        }
    }

    /// Operator wording. Only Tag differs from `FilterOp.label`: its `is`
    /// operator means "has any tag / has no tag" (a bool), not equality.
    func label(for op: FilterOp) -> String {
        if self == .tag, op == .is { return String(localized: "has") }
        // Header: "is present / is absent"; text operators read as usual.
        if self == .header, op == .equals { return String(localized: "equals") }
        return op.label
    }

    /// Wording of the true/false picker; Tag reads "any tag / no tag".
    func boolLabel(_ value: Bool) -> String {
        if self == .tag { return value ? String(localized: "any tag") : String(localized: "no tag") }
        if self == .header { return value ? String(localized: "present") : String(localized: "absent") }
        return value ? "true" : "false"
    }

    enum ValueKind: Equatable { case text, bool, days, absoluteDate, size, list }

    var operators: [FilterOp] {
        switch self {
        case .from, .to, .cc, .bcc, .attachmentName, .attachmentType:
            return [.contains, .equals, .startsWith]
        case .subject:
            return [.contains, .equals, .startsWith]
        case .body:
            return [.contains]
        case .isRead, .isStarred, .isAnswered, .isDraft, .isDeleted, .hasAttachments:
            return [.is]
        case .date:
            return [.withinLastDays, .olderThanDays, .before, .after]
        case .sizeBytes:
            return [.gt, .lt]
        case .mailboxId, .accountId:
            return [.inList]
        case .tag:
            // equals = carries that tag; is = has any tag (true) / none (false).
            // "Is not" is the enclosing group's `none` match.
            return [.equals, .is]
        case .gistTopic:
            return [.inList]
        case .header:
            // contains / equals / starts with a text, or is present / absent.
            return [.contains, .equals, .startsWith, .is]
        }
    }

    func valueKind(for op: FilterOp) -> ValueKind {
        switch self {
        case .isRead, .isStarred, .isAnswered, .isDraft, .isDeleted, .hasAttachments: return .bool
        case .tag, .header: return op == .is ? .bool : .text
        case .date: return (op == .withinLastDays || op == .olderThanDays) ? .days : .absoluteDate
        case .sizeBytes: return .size
        case .mailboxId, .accountId, .gistTopic: return .list
        default: return .text
        }
    }
}

enum FilterOp: String, Codable, CaseIterable, Identifiable {
    case contains, equals
    case startsWith = "starts_with"
    case before, after
    case onOrBefore = "on_or_before"
    case onOrAfter = "on_or_after"
    case gt, lt, gte, lte
    case `is`
    case inList = "in"
    case withinLastDays = "within_last_days"
    case olderThanDays = "older_than_days"

    var id: String { rawValue }

    var label: String {
        switch self {
        case .contains: return String(localized: "contains")
        case .equals: return String(localized: "is")
        case .startsWith: return String(localized: "starts with")
        case .before: return String(localized: "is before")
        case .after: return String(localized: "is after")
        case .onOrBefore: return String(localized: "is on or before")
        case .onOrAfter: return String(localized: "is on or after")
        case .gt, .gte: return String(localized: "is larger than")
        case .lt, .lte: return String(localized: "is smaller than")
        case .is: return String(localized: "is")
        case .inList: return String(localized: "is one of")
        case .withinLastDays: return String(localized: "is within the last (days)")
        case .olderThanDays: return String(localized: "is older than (days)")
        }
    }
}

/// Rust `FilterValue` is `#[serde(untagged)]`: a bare string, number, bool or
/// string list. Decoding tries the strict types first so `false` is never
/// read as text and `5` never as a string.
enum FilterValueDTO: Codable, Hashable {
    case text(String)
    case number(Int64)
    case bool(Bool)
    case list([String])

    init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if let b = try? c.decode(Bool.self) { self = .bool(b); return }
        if let n = try? c.decode(Int64.self) { self = .number(n); return }
        if let s = try? c.decode(String.self) { self = .text(s); return }
        if let l = try? c.decode([String].self) { self = .list(l); return }
        throw DecodingError.dataCorruptedError(in: c, debugDescription: "unsupported filter value")
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .text(let s): try c.encode(s)
        case .number(let n): try c.encode(n)
        case .bool(let b): try c.encode(b)
        case .list(let l): try c.encode(l)
        }
    }
}

struct FilterConditionDTO: Codable, Hashable, Identifiable {
    /// Local identity for `ForEach`; never encoded.
    var id = UUID()
    var field: FilterFieldKind
    var op: FilterOp
    var value: FilterValueDTO
    /// MM-1c: the header name as the user typed it; only meaningful (and only
    /// encoded) when `field == .header`. Never trimmed or lower-cased here:
    /// `FilterHeaderPolicy` / the core are the single source for name rules.
    var headerName: String?

    private enum CodingKeys: String, CodingKey {
        case field, op = "operator", value
    }

    /// Rust serialises `FilterField::Header(name)` as `{"header": name}` and
    /// every unit field as a bare string.
    private struct HeaderFieldObject: Codable { let header: String }

    init(field: FilterFieldKind, op: FilterOp, value: FilterValueDTO, headerName: String? = nil) {
        self.field = field
        self.op = op
        self.value = value
        self.headerName = headerName
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        if let object = try? c.decode(HeaderFieldObject.self, forKey: .field) {
            field = .header
            headerName = object.header
        } else {
            field = try c.decode(FilterFieldKind.self, forKey: .field)
            headerName = nil
        }
        op = try c.decode(FilterOp.self, forKey: .op)
        value = try c.decode(FilterValueDTO.self, forKey: .value)
        // Whole-day dates (#3455): in memory an absolute date is always the
        // local start of its day. This also corrects smart mailboxes saved
        // before the fix, which hold the clock time they were created at.
        if field.valueKind(for: op) == .absoluteDate, case .number(let n) = value {
            value = .number(Self.startOfDay(n))
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        if field == .header {
            try c.encode(HeaderFieldObject(header: headerName ?? ""), forKey: .field)
        } else {
            try c.encode(field, forKey: .field)
        }
        try c.encode(op, forKey: .op)
        if field.valueKind(for: op) == .absoluteDate, case .number(let n) = value {
            try c.encode(FilterValueDTO.number(Self.wireDate(op: op, day: n)), forKey: .value)
        } else {
            try c.encode(value, forKey: .value)
        }
    }

    // MARK: whole-day date semantics (#3455)

    /// The local start of the day `epoch` falls on.
    static func startOfDay(_ epoch: Int64, calendar: Calendar = .current) -> Int64 {
        Int64(calendar.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(epoch))).timeIntervalSince1970)
    }

    /// The bound the core compares `sent_at` against (`filter_sql.rs`
    /// `date_sql`: before `<`, after `>`, on or before `<=`, on or after
    /// `>=`) so each operator covers whole local days of `day`:
    /// before = earlier than the day starts; on or after = from its start;
    /// after and on or before = against its last second (the next day's
    /// start, found through the calendar so 23- and 25-hour days are right,
    /// minus one). Decoding floors either bound back to the same day.
    static func wireDate(op: FilterOp, day: Int64, calendar: Calendar = .current) -> Int64 {
        let start = startOfDay(day, calendar: calendar)
        switch op {
        case .after, .onOrBefore:
            let startDate = Date(timeIntervalSince1970: TimeInterval(start))
            guard let next = calendar.date(byAdding: .day, value: 1, to: startDate) else { return start }
            return Int64(calendar.startOfDay(for: next).timeIntervalSince1970) - 1
        default:
            return start
        }
    }

    static func == (a: Self, b: Self) -> Bool {
        a.field == b.field && a.op == b.op && a.value == b.value && a.headerName == b.headerName
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(field)
        hasher.combine(op)
        hasher.combine(value)
        hasher.combine(headerName)
    }

    /// A new row for `field`: its first operator and an empty value of the
    /// right shape.
    static func blank(field: FilterFieldKind) -> FilterConditionDTO {
        let op = field.operators[0]
        return FilterConditionDTO(
            field: field, op: op, value: defaultValue(field: field, op: op),
            headerName: field == .header ? "" : nil)
    }

    static func defaultValue(field: FilterFieldKind, op: FilterOp) -> FilterValueDTO {
        switch field.valueKind(for: op) {
        case .text: return .text("")
        case .bool: return .bool(true)
        case .days: return .number(7)
        case .absoluteDate: return .number(startOfDay(Int64(Date().timeIntervalSince1970)))
        case .size: return .number(1_000_000)
        case .list: return .list([])
        }
    }

    /// Switching field keeps the row but resets operator and value to that
    /// field's defaults (a text value makes no sense under a size operator).
    mutating func changeField(to newField: FilterFieldKind) {
        guard newField != field else { return }
        self = FilterConditionDTO.blank(field: newField)
    }

    /// Switching operator within a field re-shapes the value only when its
    /// kind changes (e.g. relative days <-> absolute date).
    mutating func changeOp(to newOp: FilterOp) {
        let before = field.valueKind(for: op)
        op = newOp
        if field.valueKind(for: newOp) != before {
            value = FilterConditionDTO.defaultValue(field: field, op: newOp)
        }
    }

    /// Core limit on `within_last_days` / `older_than_days` (filter_sql.rs).
    static let maxRelativeDays: Int64 = 36_500

    /// Whether the row says something the core can evaluate. Without a policy
    /// (not loaded yet) a Header row only needs a non-blank name; the core
    /// still validates it at save time.
    var isComplete: Bool { isComplete(policy: nil) }

    /// As `isComplete`, and a Header row also needs a name the policy calls
    /// storable (not invalid, not a built-in field, not on the denylist).
    func isComplete(policy: FilterHeaderPolicy?) -> Bool {
        if field == .header {
            let name = headerName ?? ""
            if name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return false }
            if let policy, policy.status(of: name) != .storable { return false }
        }
        switch value {
        case .text(let s): return !s.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        case .list(let l): return !l.isEmpty
        case .number(let n):
            // Relative-day rows are bounded by the core (0-36500 days); the
            // editor field clamps, but a loaded/decoded value may not be.
            if field == .date, op == .withinLastDays || op == .olderThanDays {
                return (0...Self.maxRelativeDays).contains(n)
            }
            return n >= 0
        case .bool: return true
        }
    }
}

struct FilterGroupDTO: Codable, Hashable, Identifiable {
    var id = UUID()
    var op: FilterBoolOp
    var conditions: [FilterConditionDTO]
    var groups: [FilterGroupDTO]

    private enum CodingKeys: String, CodingKey { case op, conditions, groups }

    init(op: FilterBoolOp = .and, conditions: [FilterConditionDTO] = [], groups: [FilterGroupDTO] = []) {
        self.op = op
        self.conditions = conditions
        self.groups = groups
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        op = try c.decode(FilterBoolOp.self, forKey: .op)
        conditions = try c.decodeIfPresent([FilterConditionDTO].self, forKey: .conditions) ?? []
        groups = try c.decodeIfPresent([FilterGroupDTO].self, forKey: .groups) ?? []
    }

    static func == (a: Self, b: Self) -> Bool {
        a.op == b.op && a.conditions == b.conditions && a.groups == b.groups
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(op)
        hasher.combine(conditions)
        hasher.combine(groups)
    }

    /// The group a fresh "Conditions" editor opens with: match all of one
    /// blank subject row.
    static func starter() -> FilterGroupDTO {
        FilterGroupDTO(op: .and, conditions: [.blank(field: .subject)])
    }

    // MARK: limits (mirror castellum-db filter_sql MAX_GROUP_DEPTH / MAX_CONDITIONS)

    // The core allows depth 8 / 256 conditions, but folds the scope in as one
    // extra level and up to two extra conditions, so the editor stops short.
    static let maxDepth = 7
    static let maxConditions = 254
    /// The editor stops offering "add group" one level short of the core's
    /// limit so the tree can always still be saved.
    static let maxEditorDepth = 4

    /// 1 for a group with no nested groups.
    var depth: Int { 1 + (groups.map(\.depth).max() ?? 0) }
    /// Conditions plus nested groups (the core caps both together).
    var nodeCount: Int { conditions.count + groups.reduce(0) { $0 + 1 + $1.nodeCount } }
    var conditionCount: Int { conditions.count + groups.reduce(0) { $0 + $1.conditionCount } }

    /// Saveable: at least one row somewhere, every row complete, within limits.
    var isValidForSave: Bool { isValidForSave(policy: nil) }

    /// As `isValidForSave`, also holding Header rows to the loaded policy.
    func isValidForSave(policy: FilterHeaderPolicy?) -> Bool {
        conditionCount > 0 && pruningEmptyGroups().nodeCount <= Self.maxConditions && depth <= Self.maxDepth
            && allRowsComplete(policy: policy)
    }

    private func allRowsComplete(policy: FilterHeaderPolicy?) -> Bool {
        conditions.allSatisfy { $0.isComplete(policy: policy) }
            && groups.allSatisfy { $0.allRowsComplete(policy: policy) }
    }

    /// Whether any row (recursively) is a Header condition.
    var containsHeaderCondition: Bool {
        conditions.contains { $0.field == .header } || groups.contains(where: \.containsHeaderCondition)
    }

    /// Groups that hold no rows at all (removed by `pruningEmptyGroups`).
    func pruningEmptyGroups() -> FilterGroupDTO {
        var copy = self
        copy.groups = groups.map { $0.pruningEmptyGroups() }
            .filter { !$0.conditions.isEmpty || !$0.groups.isEmpty }
        return copy
    }
}

/// Rust `SmartMailboxScope`: empty lists mean "everywhere".
struct SmartMailboxScopeDTO: Codable, Hashable {
    var accountIds: [String] = []
    var mailboxIds: [String] = []
    /// HP-2: also show this smart mailbox as a tab of the Split Inbox.
    var inboxSplit: Bool = false
    static let everywhere = SmartMailboxScopeDTO()
    var isEverywhere: Bool { accountIds.isEmpty && mailboxIds.isEmpty }
}

extension SmartMailboxScopeDTO {
    /// Tolerates scopes written before HP-2 (no `inbox_split` key). In an
    /// extension so the memberwise initialiser stays available.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        accountIds = try c.decodeIfPresent([String].self, forKey: .accountIds) ?? []
        mailboxIds = try c.decodeIfPresent([String].self, forKey: .mailboxIds) ?? []
        inboxSplit = try c.decodeIfPresent(Bool.self, forKey: .inboxSplit) ?? false
    }
}

extension SmartMailboxScopeDTO {
    /// MM-1c slice 1: header conditions cannot split the Inbox (Rust's
    /// `validate_group_for_save` refuses it). The scope actually saved for
    /// `group`: `inboxSplit` is cleared when the group holds a Header row, so
    /// the editor's disabled toggle and the stored row always agree.
    func forSave(group: FilterGroupDTO) -> SmartMailboxScopeDTO {
        var copy = self
        if group.containsHeaderCondition { copy.inboxSplit = false }
        return copy
    }
}

extension SmartMailbox {
    /// HP-2: a group mailbox flagged to appear as a Split Inbox tab (the
    /// same rule Rust's `list_inbox_splits` applies).
    var isInboxSplit: Bool { group != nil && scope?.inboxSplit == true }
}


// MARK: - header policy (MM-1c)

/// What the core says about a header name, decoded from
/// `castellum_filter_header_policy`. The classification data lives in Rust
/// (`castellum_models::filter`); this type only applies it, as a pure function,
/// so the editor and its tests need no core and Swift keeps no list to drift.
struct FilterHeaderPolicy: Codable, Equatable {
    var maxNameLen: Int
    /// Lower-cased header name -> the field that replaces it ("from", ...).
    var builtinFields: [String: String]
    var notStored: [String]
    var notStoredPrefixes: [String]
    var notStoredSubstrings: [String]
    /// Lower-cased names seen in this profile, for completion.
    var knownNames: [String]

    // No explicit CodingKeys: the core's decoder converts snake_case keys
    // (max_name_len -> maxNameLen) like every other DTO here.

    enum Status: Equatable {
        case storable
        /// Has its own field; carries that field.
        case builtIn(FilterFieldKind)
        case notStored
        case invalid
    }

    /// Mirror of `normalize_header_name`: trimmed, one trailing ":" tolerated,
    /// RFC 5322 field-name characters only (printable ASCII except ":").
    func normalized(_ raw: String) -> String? {
        var t = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if t.hasSuffix(":") { t.removeLast() }
        t = t.trimmingCharacters(in: .whitespaces)
        guard !t.isEmpty, t.utf8.count <= maxNameLen else { return nil }
        guard t.utf8.allSatisfy({ (33...126).contains($0) && $0 != 58 }) else { return nil }
        return t.lowercased()
    }

    /// Mirror of `header_name_status`.
    func status(of raw: String) -> Status {
        guard let n = normalized(raw) else { return .invalid }
        if let field = builtinFields[n], let kind = FilterFieldKind(rawValue: field) { return .builtIn(kind) }
        if notStored.contains(n)
            || notStoredPrefixes.contains(where: { n.hasPrefix($0) })
            || notStoredSubstrings.contains(where: { n.contains($0) }) {
            return .notStored
        }
        return .storable
    }

    /// Known names starting with the (normalised) text typed so far, for completion.
    func completions(for typed: String, limit: Int = 6) -> [String] {
        let t = typed.trimmingCharacters(in: .whitespaces).lowercased()
        guard !t.isEmpty else { return [] }
        return Array(knownNames.filter { $0.hasPrefix(t) && $0 != t }.prefix(limit))
    }
}

/// `castellum_message_header_coverage`: messages whose headers are stored vs all.
struct HeaderCoverageDTO: Codable, Equatable {
    var captured: Int64
    var total: Int64
}
