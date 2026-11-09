// Castellum - client-side rules, Swift side.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Mirrors Rust `castellum_models::{SavedFilter, RuleAction, RuleTrigger}` as
// they cross the C ABI. The core is the only evaluator and the only thing
// that enqueues operations; this file is data plus the pure draft rules the
// editor sheet needs (what a rule must contain before it can be saved), unit
// tested in RuleModelsTests.

import Foundation

/// Rust `RuleAction`: unit variants are bare strings ("mark_read"), struct
/// variants are one-key objects ({"move":{"mailbox_id":"..."}}).
enum RuleActionDTO: Codable, Hashable {
    case move(mailboxId: String)
    case addStar
    case removeStar
    case markRead
    case markUnread
    case archive
    case trash
    /// S42 `AddTag { keyword }`; the editor offers it from the defined tags
    /// ("Add Action > Add tag", #3146), and a stored rule carrying one must
    /// decode either way, or the whole list fails.
    case addTag(keyword: String)
    /// #3146 `RemoveTag { keyword }`: "Add Action > Remove tag".
    case removeTag(keyword: String)

    private struct AnyKey: CodingKey {
        var stringValue: String
        var intValue: Int? { nil }
        init(_ s: String) { stringValue = s }
        init?(stringValue: String) { self.stringValue = stringValue }
        init?(intValue: Int) { nil }
    }

    init(from decoder: Decoder) throws {
        let single = try decoder.singleValueContainer()
        if let name = try? single.decode(String.self) {
            switch name {
            case "mark_read": self = .markRead
            case "mark_unread": self = .markUnread
            case "archive": self = .archive
            case "trash": self = .trash
            default:
                throw DecodingError.dataCorruptedError(in: single, debugDescription: "unknown rule action \(name)")
            }
            return
        }
        let keyed = try decoder.container(keyedBy: AnyKey.self)
        guard let key = keyed.allKeys.first else {
            throw DecodingError.dataCorruptedError(in: single, debugDescription: "empty rule action")
        }
        // The app's decoder uses `.convertFromSnakeCase`, which also rewrites
        // these dynamic keys ("add_flag" arrives as "addFlag"), so accept both.
        switch key.stringValue {
        case "move":
            let inner = try keyed.nestedContainer(keyedBy: AnyKey.self, forKey: key)
            let idKey = inner.allKeys.first { $0.stringValue == "mailbox_id" || $0.stringValue == "mailboxId" }
            guard let idKey else {
                throw DecodingError.dataCorruptedError(in: single, debugDescription: "move without mailbox_id")
            }
            self = .move(mailboxId: try inner.decode(String.self, forKey: idKey))
        case "add_flag", "addFlag": self = .addStar
        case "remove_flag", "removeFlag": self = .removeStar
        case "add_tag", "addTag":
            let inner = try keyed.nestedContainer(keyedBy: AnyKey.self, forKey: key)
            self = .addTag(keyword: try inner.decode(String.self, forKey: AnyKey("keyword")))
        case "remove_tag", "removeTag":
            let inner = try keyed.nestedContainer(keyedBy: AnyKey.self, forKey: key)
            self = .removeTag(keyword: try inner.decode(String.self, forKey: AnyKey("keyword")))
        default:
            throw DecodingError.dataCorruptedError(in: single, debugDescription: "unknown rule action \(key.stringValue)")
        }
    }

    func encode(to encoder: Encoder) throws {
        switch self {
        case .markRead, .markUnread, .archive, .trash:
            var c = encoder.singleValueContainer()
            try c.encode(name)
        case .move(let mailboxId):
            var c = encoder.container(keyedBy: AnyKey.self)
            var inner = c.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey("move"))
            try inner.encode(mailboxId, forKey: AnyKey("mailbox_id"))
        case .addStar, .removeStar:
            var c = encoder.container(keyedBy: AnyKey.self)
            var inner = c.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey(self == .addStar ? "add_flag" : "remove_flag"))
            try inner.encode("starred", forKey: AnyKey("flag"))
        case .addTag(let keyword):
            var c = encoder.container(keyedBy: AnyKey.self)
            var inner = c.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey("add_tag"))
            try inner.encode(keyword, forKey: AnyKey("keyword"))
        case .removeTag(let keyword):
            var c = encoder.container(keyedBy: AnyKey.self)
            var inner = c.nestedContainer(keyedBy: AnyKey.self, forKey: AnyKey("remove_tag"))
            try inner.encode(keyword, forKey: AnyKey("keyword"))
        }
    }

    private var name: String {
        switch self {
        case .markRead: return "mark_read"
        case .markUnread: return "mark_unread"
        case .archive: return "archive"
        case .trash: return "trash"
        default: return ""
        }
    }

    /// Actions that remove the message from its mailbox locally — only one
    /// of these can run per match (mirrors the core).
    var removesMessage: Bool {
        switch self {
        case .move, .archive, .trash: return true
        default: return false
        }
    }

    func label(mailboxName: (String) -> String?) -> String {
        switch self {
        case .move(let id):
            if let name = mailboxName(id) { return String(localized: "Move to \(name)") }
            return String(localized: "Move to a folder that no longer exists")
        case .addStar: return String(localized: "Star")
        case .removeStar: return String(localized: "Remove star")
        case .markRead: return String(localized: "Mark as read")
        case .markUnread: return String(localized: "Mark as unread")
        case .archive: return String(localized: "Archive")
        case .trash: return String(localized: "Move to Trash")
        case .addTag(let keyword): return String(localized: "Add tag \(keyword)")
        case .removeTag(let keyword): return String(localized: "Remove tag \(keyword)")
        }
    }
}

enum RuleTriggerDTO: String, Codable, CaseIterable, Identifiable {
    case onSync = "on_sync"
    case manual
    var id: String { rawValue }
    var label: String {
        switch self {
        case .onSync: return String(localized: "When mail arrives, and on demand")
        case .manual: return String(localized: "On demand only")
        }
    }
}

/// A stored filter (`kind` is "rule" or "smart_mailbox"); the Rules UI only
/// shows rules.
struct SavedRule: Codable, Identifiable, Hashable {
    let id: String
    let accountId: String?
    let name: String
    let kind: String
    let group: FilterGroupDTO
    /// Kept so an edit or toggle round-trips them unchanged (the editor has no
    /// control for either).
    let sortField: String?
    let sortDesc: Bool
    let trigger: RuleTriggerDTO?
    let actions: [RuleActionDTO]
    let sortOrder: Int64
    let stopProcessing: Bool
    let enabled: Bool
}

struct RulePreviewDTO: Codable {
    let matches: [MessageHeader]
    let actions: [RuleActionDTO]
    /// Exact number of messages Run Now would act on (counted in SQL with
    /// Trash/Junk/Drafts/Sent excluded); `matches` is capped at the limit.
    let total: Int
}

/// The editor's dry run of unsaved conditions: first matches + exact total.
struct RuleConditionsPreviewDTO: Codable {
    let matches: [MessageHeader]
    let total: Int
}

struct RuleRunReportDTO: Codable, Hashable {
    let filterId: String
    let matchedCount: Int
    let actionsEnqueued: Int
    /// Why the rule could not act (e.g. a read-only account); nil on success.
    let error: String?
    /// Run Now batches: the message id the next batch starts after; nil
    /// when the match set is done.
    let nextAfter: String?
    /// With `nextAfter`: how many candidates still sort after it.
    let remaining: Int?
}

/// Run Now never queues more than this many messages' actions per click
/// (mirrors castellum-core's RUN_NOW_MAX_MESSAGES); the confirmation says so
/// when a rule matches more, and Continue resumes where it stopped.
enum RuleRunNow {
    /// "1,204 more" / "More messages" for the report line of a capped run.
    static func remainingText(_ remaining: Int?) -> String {
        guard let remaining else { return String(localized: "More messages") }
        return String(localized: "\(remaining.formatted()) more")
    }

    static let maxMessages = 10_000
    static let batchSize: Int64 = 200
}

/// Where an unfinished Run Now stopped, so Continue can resume there.
struct RuleContinuation: Equatable {
    let ruleId: String
    /// The `after` cursor for the next batch (the last message id handled).
    let after: String
    /// Candidates still ahead of the cursor, when the core said.
    let remaining: Int?
}

/// Live state of a Run Now in progress (Settings > Rules shows it with a
/// Cancel button).
struct RuleRunProgress: Equatable {
    let ruleName: String
    /// How many messages this run will process at most.
    let target: Int
    var processed = 0
    var queued = 0
}

/// What the editor sheet edits, and when it may be saved.
struct RuleDraft: Equatable, Identifiable {
    /// The stored rule id, nil while creating.
    var id: String?
    var name = ""
    /// nil = every account (only then are account-specific actions unavailable).
    var accountId: String?
    var trigger: RuleTriggerDTO = .onSync
    var group: FilterGroupDTO = .starter()
    var actions: [RuleActionDTO] = []
    var stopProcessing = false
    var enabled = true
    var sortField: String?
    var sortDesc = true

    init() {}

    init(_ rule: SavedRule) {
        id = rule.id
        name = rule.name
        accountId = rule.accountId
        trigger = rule.trigger ?? .manual
        group = rule.group
        actions = rule.actions
        stopProcessing = rule.stopProcessing
        enabled = rule.enabled
        sortField = rule.sortField
        sortDesc = rule.sortDesc
    }

    /// The core refuses a rule that adds and removes one tag (#3146);
    /// say so before Save instead of after.
    var addsAndRemovesTheSameTag: Bool {
        var added = Set<String>(), removed = Set<String>()
        for action in actions {
            switch action {
            case .addTag(let k): added.insert(k.lowercased())
            case .removeTag(let k): removed.insert(k.lowercased())
            default: break
            }
        }
        return !added.isDisjoint(with: removed)
    }

    /// Move names one account's mailbox id, so it needs a single account.
    var canOfferMove: Bool { accountId != nil }

    /// The name is mandatory; the editor flags the field itself, not only the
    /// caption under the form (#3146).
    var nameIsMissing: Bool { name.trimmingCharacters(in: .whitespaces).isEmpty }

    /// Everything that must hold before Save is enabled.
    var validationProblem: String? {
        if nameIsMissing {
            return String(localized: "Give the rule a name.")
        }
        if !group.isValidForSave {
            return String(localized: "Complete every condition (at least one is required).")
        }
        if actions.isEmpty {
            return String(localized: "Add at least one action.")
        }
        if addsAndRemovesTheSameTag {
            return String(localized: "A rule can’t both add and remove the same tag.")
        }
        if actions.filter(\.removesMessage).count > 1 {
            return String(localized: "Only one action that moves the message (Move, Archive, Trash) can run per match.")
        }
        if accountId == nil, actions.contains(where: { if case .move = $0 { return true } else { return false } }) {
            return String(localized: "Move needs a single account: choose one under Apply to.")
        }
        if let accountId, moveFoldersOfOtherAccounts.isEmpty == false {
            return String(localized: "Move folder belongs to another account: choose a folder of \(accountId)")
        }
        return nil
    }

    /// Move folders whose id does not start with this rule's `<account>:`
    /// (mailbox ids are `<account id>:<path>`), so a rule re-pointed at another
    /// account cannot keep a folder that can never match there.
    var moveFoldersOfOtherAccounts: [String] {
        guard let accountId else { return [] }
        return actions.compactMap { action in
            if case .move(let id) = action, !id.hasPrefix(accountId + ":") { return id }
            return nil
        }
    }

    /// The first Move folder that is not among `mailboxIds` (a loaded folder
    /// list), i.e. one that was renamed or deleted; nil when all exist.
    func missingMoveFolder(mailboxIds: [String]) -> String? {
        let known = Set(mailboxIds)
        for case .move(let id) in actions where !known.contains(id) { return id }
        return nil
    }

    var isValid: Bool { validationProblem == nil }

    /// The list order the core applies (row-removing actions last); the UI
    /// shows this so what is displayed is what runs.
    var actionsInRunOrder: [RuleActionDTO] {
        actions.filter { !$0.removesMessage } + actions.filter(\.removesMessage)
    }
}

struct RuleUpsertRequest: Encodable {
    var id: String?
    var accountId: String?
    let name: String
    let kind = "rule"
    let group: FilterGroupDTO
    let sortField: String?
    let sortDesc: Bool
    let trigger: RuleTriggerDTO
    let actions: [RuleActionDTO]
    let stopProcessing: Bool
    let enabled: Bool

    /// The enable switch: the stored rule exactly as it is, only `enabled`
    /// changed. Deliberately bypasses `RuleDraft`'s save validation — a rule
    /// that predates this editor (e.g. an all-accounts rule with a Move) must
    /// still be switchable off.
    init(rule: SavedRule, enabled: Bool) {
        id = rule.id
        accountId = rule.accountId
        name = rule.name
        group = rule.group
        sortField = rule.sortField
        sortDesc = rule.sortDesc
        trigger = rule.trigger ?? .manual
        actions = rule.actions
        stopProcessing = rule.stopProcessing
        self.enabled = enabled
    }

    init(_ draft: RuleDraft) {
        id = draft.id
        accountId = draft.accountId
        name = draft.name.trimmingCharacters(in: .whitespacesAndNewlines)
        group = draft.group.pruningEmptyGroups()
        sortField = draft.sortField
        sortDesc = draft.sortDesc
        trigger = draft.trigger
        actions = draft.actions
        stopProcessing = draft.stopProcessing
        enabled = draft.enabled
    }
}
