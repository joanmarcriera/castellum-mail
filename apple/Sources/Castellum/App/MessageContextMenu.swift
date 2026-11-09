// Castellum - the message-row context menu as a pure, testable description.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `MessageContextMenu.entries(for:specs:)` turns a `MessageMenuContext` (a
// plain value summarising the right-clicked selection) into a list of
// `MessageMenuEntry`s. AppKit only renders it (`MessageListView.Coordinator`),
// so order, enablement and read-only rules are unit-tested with no window and
// the UI harness can read the menu without popping a modal one.
//
// Every command item maps to a registered `CommandSpec` and fires the SAME
// `Notification.Name` as the menu bar, bare keys and the palette; nothing here
// calls `AppModel` directly, so `CommandDispatcher`'s destructive-command
// guard and the write-safety prompt apply unchanged.
//
// Slice 1 of project-docs/specs/2026-10-12/context-menu.md: existing commands.
// Slice 4: "AI on this Mac ▸" and "Project ▸" submenus with visible reasons;
// plus the existing Move to Folder… sheet.
// Slice 2: "Move to ▸" with recents and the account's folders (the existing
// guarded move path, `AppModel.moveSelected(toMailboxId:)`).
// Slice 3: Mark as Junk / Not Junk (the same move path, aimed at the account's
// Junk-role mailbox or back to the Inbox; no keyword writes, see DECISIONS).

import Foundation

/// What the menu needs to know about the right-clicked selection.
struct MessageMenuContext: Equatable {
    var selectionCount: Int
    var anyUnread: Bool
    var anyRead: Bool
    var anyFlagged: Bool
    var allFlagged: Bool
    /// A selected message's body is (or is about to be) available to Reply.
    var bodyAvailable: Bool
    var aiEnabled: Bool
    /// Evaluation-mode (read-only) accounts among the targets: (id, email).
    var readOnlyAccounts: [ReadOnlyAccount]
    /// True when EVERY target lives on a read-only account.
    var allTargetsReadOnly: Bool
    /// Roles of the mailboxes the targets live in ("inbox", "drafts", ...).
    var sourceRoles: Set<String>
    var isSnoozedList: Bool
    /// State-dependent titles for the single (primary) message.
    var threadMuted: Bool
    var senderIsVip: Bool
    /// An embedding model is chosen in Settings > AI (Find Similar needs
    /// one); defaults to true so contexts built without it stay valid.
    var aiEmbeddingModelSet: Bool = true
    /// Folders the selection may be moved to (the clicked selection's account;
    /// empty when unknown or when the selection spans accounts), already
    /// filtered to selectable, non-protected mailboxes.
    var moveDestinations: [MoveDestination] = []
    /// Recent destination ids of that account, newest first (may hold stale ids).
    var recentMoveIds: [String] = []
    /// Mailboxes the targets live in; a destination is not offered when it is
    /// the mailbox of EVERY target (the core treats that as a no-op).
    var targetMailboxIds: Set<String> = []
    /// The selection spans more than one account (mailbox ids are per account).
    var multiAccount: Bool = false
    /// At least one target's account has a role-`junk` mailbox.
    var junkMailboxAvailable: Bool = false

    struct MoveDestination: Equatable {
        let id: String
        let name: String
    }

    struct ReadOnlyAccount: Equatable {
        let id: String
        let email: String
    }

    static let empty = MessageMenuContext(
        selectionCount: 0, anyUnread: false, anyRead: false, anyFlagged: false,
        allFlagged: false, bodyAvailable: false, aiEnabled: false, readOnlyAccounts: [],
        allTargetsReadOnly: false, sourceRoles: [], isSnoozedList: false,
        threadMuted: false, senderIsVip: false)
}

/// One row of the described menu.
indirect enum MessageMenuEntry: Equatable {
    case command(CommandID, title: String, enabled: Bool, reason: String?)
    case submenu(title: String, entries: [MessageMenuEntry])
    /// "Enable Writes for <email>…" - opens the existing WS1 confirmation.
    case enableWrites(email: String, accountId: String)
    /// One "Move to ▸" destination; invoking it moves the selection to `mailboxId`.
    case moveTo(mailboxId: String, title: String, enabled: Bool, reason: String?)
    case separator

    var title: String? {
        switch self {
        case .command(_, let title, _, _), .submenu(let title, _): return title
        case .enableWrites(let email, _): return MessageContextMenu.enableWritesTitle(email: email)
        case .moveTo(_, let title, _, _): return title
        case .separator: return nil
        }
    }

    var isSeparator: Bool { if case .separator = self { return true } else { return false } }
}

/// One slot of the ordering table; the ONLY place the menu order lives.
enum MessageMenuSlot: Equatable {
    case command(CommandID)
    case readState          // Mark as Read and/or Mark as Unread
    case flag               // Flag / Unflag
    case moveTo             // Move to ▸ (recents, folders, Other…)
    case junk               // Mark as Junk / Not Junk
    case snooze             // Snooze ▸ or, in the Snoozed list, Move to Inbox Now
    case mute               // Mute / Unmute Conversation (state-dependent)
    case vip                // Add / Remove Sender as VIP (state-dependent)
    case more([MessageMenuSlot])
    /// "AI on this Mac ▸": Summarise, Reply with AI, Find Similar.
    case ai
    /// "Project ▸": Assign to Project….
    case project
}

enum MessageContextMenu {
    /// Menu order, grouped; a separator is drawn between groups.
    static let layout: [[MessageMenuSlot]] = [
        [.command(.reply), .command(.replyAll), .command(.forward)],
        [.command(.archive), .moveTo, .readState, .flag, .snooze],
        [.command(.tagMessage), .junk, .ai, .project,
         .more([.mute, .vip, .command(.showCorrespondence), .command(.openInWindow),
                .command(.exportEml), .command(.printMessage), .command(.viewSource)])],
        [.command(.delete)],
    ]

    /// Commands that change server state; disabled when every target is read-only.
    /// Snooze is local-only (`Database::snooze_message` has no evaluation-mode
    /// check), so the snooze presets are deliberately NOT here.
    static let serverMutating: Set<CommandID> = [
        .archive, .moveToFolder, .delete, .markRead, .markUnread, .toggleFlag, .tagMessage,
        .markJunk, .markNotJunk,
    ]

    /// The local-AI commands grouped under "AI on this Mac ▸" (slice 4).
    static let aiCommands: [CommandID] = [.summarize, .replyWithAi, .findSimilar]

    /// Commands that need exactly one message; disabled on a multi-selection.
    static let singleOnly: Set<CommandID> = [
        .reply, .replyAll, .forward, .replyWithAi, .openInWindow, .viewSource,
        .printMessage, .exportEml, .showCorrespondence, .toggleVipSender,
        .toggleMuteThread, .findSimilar, .summarize,
    ]

    static let snoozePresets: [CommandID] = [
        .snoozeLaterToday, .snoozeTomorrow, .snoozeThisWeekend, .snoozeNextWeek,
    ]

    static func enableWritesTitle(email: String) -> String {
        String(format: String(localized: "Enable Writes for %@…"), email)
    }

    static func entries(
        for context: MessageMenuContext,
        specs: [CommandSpec] = CommandRegistry.all
    ) -> [MessageMenuEntry] {
        guard context.selectionCount > 0 else { return [] }
        let byId = Dictionary(specs.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        let palette = PaletteContext(
            hasSelectedMessage: true, messageBodyAvailable: context.bodyAvailable,
            aiEnabled: context.aiEnabled)

        func command(_ id: CommandID, title override: String? = nil) -> MessageMenuEntry? {
            guard let spec = byId[id] else { return nil }
            var enabled = CommandPaletteAvailability.isEnabled(id, context: palette)
            var reason: String?
            if singleOnly.contains(id), context.selectionCount > 1 {
                enabled = false
                reason = String(localized: "Select a single message to use this.")
            }
            if enabled, context.sourceRoles.contains("drafts"),
               [.reply, .replyAll, .forward].contains(id) {
                enabled = false
                reason = String(localized: "Open a draft with Return or double-click to edit it.")
            }
            if serverMutating.contains(id), context.allTargetsReadOnly {
                enabled = false
                reason = readOnlyReason(context)
            }
            if aiCommands.contains(id), reason == nil {
                // Say why before the click, not after (slice 4).
                if !context.aiEnabled {
                    enabled = false
                    reason = String(localized: "Turn on AI in Settings → AI.")
                } else if id == .findSimilar, !context.aiEmbeddingModelSet {
                    enabled = false
                    reason = String(localized: "Choose an embedding model in Settings → AI.")
                }
            }
            if !enabled, reason == nil,
               id == .reply || id == .replyAll || id == .forward || id == .replyWithAi {
                reason = String(localized: "The message body is still loading.")
            }
            return .command(id, title: override ?? spec.title, enabled: enabled, reason: reason)
        }

        func resolve(_ slot: MessageMenuSlot) -> [MessageMenuEntry] {
            switch slot {
            case .command(let id):
                return command(id).map { [$0] } ?? []
            case .readState:
                var out: [MessageMenuEntry] = []
                // Mixed (or unknown) read state offers both verbs.
                if context.anyUnread || !context.anyRead,
                   let e = command(.markRead, title: String(localized: "Mark as Read")) { out.append(e) }
                if context.anyRead || !context.anyUnread,
                   let e = command(.markUnread, title: String(localized: "Mark as Unread")) { out.append(e) }
                return out
            case .flag:
                let title = context.allFlagged ? String(localized: "Unflag") : String(localized: "Flag")
                return command(.toggleFlag, title: title).map { [$0] } ?? []
            case .moveTo:
                return moveToEntries(context, command: command)
            case .junk:
                let inJunk = context.sourceRoles == ["junk"]
                let id: CommandID = inJunk ? .markNotJunk : .markJunk
                let title = inJunk ? String(localized: "Not Junk") : String(localized: "Mark as Junk")
                guard case .command(_, let t, var enabled, var reason)? = command(id, title: title) else { return [] }
                // Not Junk goes back to the Inbox, which every account has; only
                // Junk needs a Junk-role mailbox (Castellum never invents one).
                if !inJunk, reason == nil {
                    if !context.junkMailboxAvailable {
                        enabled = false
                        reason = String(localized: "This account has no Junk folder.")
                    } else if context.sourceRoles.contains("drafts") {
                        enabled = false
                        reason = String(localized: "Drafts can’t be moved from here. Open the draft to edit it.")
                    }
                }
                return [.command(id, title: t, enabled: enabled, reason: reason)]
            case .snooze:
                if context.isSnoozedList {
                    return command(.unsnooze).map { [$0] } ?? []
                }
                var items: [MessageMenuEntry] = snoozePresets.compactMap { id in
                    command(id, title: byId[id]?.title)
                }
                items.append(.separator)
                if let custom = command(.snooze, title: String(localized: "Custom…")) { items.append(custom) }
                return [.submenu(title: String(localized: "Snooze"), entries: items)]
            case .mute:
                let title = context.threadMuted
                    ? String(localized: "Unmute Conversation") : String(localized: "Mute Conversation")
                return command(.toggleMuteThread, title: title).map { [$0] } ?? []
            case .vip:
                let title = context.senderIsVip
                    ? String(localized: "Remove Sender as VIP") : String(localized: "Add Sender as VIP")
                return command(.toggleVipSender, title: title).map { [$0] } ?? []
            case .more(let slots):
                let inner = slots.flatMap(resolve)
                return inner.isEmpty ? [] : [.submenu(title: String(localized: "More"), entries: inner)]
            case .ai:
                let inner = aiCommands.compactMap { command($0) }
                return inner.isEmpty ? [] : [.submenu(title: String(localized: "AI on this Mac"), entries: inner)]
            case .project:
                let inner = command(.assignProject).map { [$0] } ?? []
                return inner.isEmpty ? [] : [.submenu(title: String(localized: "Project"), entries: inner)]
            }
        }

        var out: [MessageMenuEntry] = []
        for group in layout {
            let items = group.flatMap(resolve)
            guard !items.isEmpty else { continue }
            if !out.isEmpty { out.append(.separator) }
            out.append(contentsOf: items)
        }
        if !context.readOnlyAccounts.isEmpty {
            out.append(.separator)
            for account in context.readOnlyAccounts {
                out.append(.enableWrites(email: account.email, accountId: account.id))
            }
        }
        return out
    }

    /// Roles that are never offered as a Move to destination: Trash and Junk have
    /// their own verbs and guards; Drafts and Sent are not filing places.
    static let protectedMoveRoles: Set<String> = ["drafts", "sent", "trash", "junk"]

    /// The "Move to ▸" slot. Cases that cannot move get ONE plain disabled item
    /// that carries the reason as its tooltip (a disabled submenu cannot).
    private static func moveToEntries(
        _ context: MessageMenuContext,
        command: (CommandID, String?) -> MessageMenuEntry?
    ) -> [MessageMenuEntry] {
        // `moveToFolder` itself already carries the read-only / palette rules.
        guard case .command(let id, let title, var enabled, var reason)? = command(.moveToFolder, nil) else { return [] }
        if reason == nil {
            if context.multiAccount {
                enabled = false
                reason = String(localized: "Select messages from one account to move them.")
            } else if context.sourceRoles.contains("drafts") {
                enabled = false
                reason = String(localized: "Drafts can’t be moved from here. Open the draft to edit it.")
            }
        }
        // Without a destination list (unknown account) fall back to the picker item alone.
        let usable = context.moveDestinations.filter {
            context.targetMailboxIds != [$0.id]
        }
        if !enabled || usable.isEmpty {
            return [.command(id, title: title, enabled: enabled, reason: reason)]
        }
        let known = Dictionary(usable.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        // Stale ids (folder deleted or renamed away) are pruned here, on read.
        let recents = context.recentMoveIds.compactMap { known[$0] }
            .prefix(Preferences.maxRecentMoveDestinations)
        let recentIds = Set(recents.map(\.id))
        var items: [MessageMenuEntry] = recents.map {
            .moveTo(mailboxId: $0.id, title: $0.name, enabled: true, reason: nil)
        }
        let rest = usable.filter { !recentIds.contains($0.id) }
        if !items.isEmpty, !rest.isEmpty { items.append(.separator) }
        items += rest.map { .moveTo(mailboxId: $0.id, title: $0.name, enabled: true, reason: nil) }
        items.append(.separator)
        items.append(.command(.moveToFolder, title: String(localized: "Other…"), enabled: true, reason: nil))
        return [.submenu(title: String(localized: "Move to"), entries: items)]
    }

    /// Every `.moveTo` destination in the tree, in menu order (harness + tests).
    static func allMoveTargets(in entries: [MessageMenuEntry]) -> [MessageMenuEntry] {
        entries.flatMap { entry -> [MessageMenuEntry] in
            switch entry {
            case .moveTo: return [entry]
            case .submenu(_, let inner): return allMoveTargets(in: inner)
            default: return []
            }
        }
    }

    /// First `.command` entry for `id`, searching submenus (harness + tests).
    static func find(_ id: CommandID, in entries: [MessageMenuEntry]) -> MessageMenuEntry? {
        for entry in entries {
            switch entry {
            case .command(let cid, _, _, _) where cid == id: return entry
            case .submenu(_, let inner): if let hit = find(id, in: inner) { return hit }
            default: break
            }
        }
        return nil
    }

    /// Tooltip for a server-mutating item that read-only mode disabled.
    static func readOnlyReason(_ context: MessageMenuContext) -> String {
        let emails = context.readOnlyAccounts.map(\.email).joined(separator: ", ")
        return String(format: String(localized: "%@ is in evaluation mode (read-only). Enable writes to use this."), emails)
    }
}
