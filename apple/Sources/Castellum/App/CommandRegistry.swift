// Castellum - central command & keyboard registry: the single source of truth
// for command identity, titles, menu placement, key bindings, help text, and
// focus policy. The shortcuts sheet derives from this table; the application
// menus migrate onto it next (they must agree with it in the meantime, pinned
// by tests).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit

// MARK: - Identity

/// Stable command identifiers. Raw values are persistent API — never rename.
enum CommandID: String, CaseIterable {
    // Compose / search
    case compose, focusSearch, sendComposer
    // Send Later (HP1): act on the compose window being written
    case sendLater, sendLaterInOneHour, sendLaterThisEvening, sendLaterTomorrowMorning, sendLaterMonday
    // Snippets (MM-7): insert into the compose window being written; open the library window
    case insertSnippet, showSnippets
    // Message actions
    case reply, replyAll, forward, replyWithAi, summarize, viewSource, exportEml, importEml, printMessage
    case markRead, markUnread, snoozeLaterToday, snoozeTomorrow, snoozeThisWeekend, snoozeNextWeek, findSimilar
    case toggleRead, markAllRead, toggleFlag, tagMessage, toggleMuteThread
    // MM-8: bare digits 1-9 toggle the first nine tags (Settings order) on the selection
    case tagSlot1, tagSlot2, tagSlot3, tagSlot4, tagSlot5, tagSlot6, tagSlot7, tagSlot8, tagSlot9
    case toggleVipSender
    case archive, delete, emptyTrash, snooze, unsnooze
    case undoRemoval
    case moveToFolder
    case markJunk, markNotJunk
    // Windows / navigation
    case showOutbox, sendAllOutbox, fitColumns, showStatus, showAiActivity, showMainWindow
    case layoutWidescreen, layoutClassic, showCorrespondence
    case headerCompact, headerExtended, headerAll
    // Privacy: load remote images in every message (the Settings > General toggle, from the View menu)
    case toggleRemoteImages
    // Plain-text viewer: open every folded quote and signature of the message on screen (MM-17)
    case expandAllQuotes
    case nextSplit, previousSplit
    case showContacts, showProjects, assignProject
    case showShortcuts, showDiagnostics
    case showCommandPalette
    case checkForUpdates
    // Message-list navigation (MM-12: notifications like every other command)
    case nextMessage, previousMessage, openMessage, openInWindow
    // g-chord mailbox jumps
    case gotoInbox, gotoArchive, gotoSent, gotoTrash, gotoDrafts, gotoJunk
    // g l: open a tag's message list through the tag picker (MM-8)
    case gotoTag
}

extension CommandID {
    /// 1...9 for `tagSlot1`...`tagSlot9`; nil for every other command.
    var tagSlotIndex: Int? {
        switch self {
        case .tagSlot1: return 1
        case .tagSlot2: return 2
        case .tagSlot3: return 3
        case .tagSlot4: return 4
        case .tagSlot5: return 5
        case .tagSlot6: return 6
        case .tagSlot7: return 7
        case .tagSlot8: return 8
        case .tagSlot9: return 9
        default: return nil
        }
    }

    /// The command for tag slot `n` (1...9).
    static func tagSlot(_ n: Int) -> CommandID? {
        allCases.first { $0.tagSlotIndex == n }
    }

    /// Special-use mailbox role for the `g …` mailbox jumps; nil for every
    /// other command (`g c` / `g p` open windows via their notifications).
    var mailboxRole: String? {
        switch self {
        case .gotoInbox: return "inbox"
        case .gotoArchive: return "archive"
        case .gotoSent: return "sent"
        case .gotoTrash: return "trash"
        case .gotoDrafts: return "drafts"
        case .gotoJunk: return "junk"
        default: return nil
        }
    }
}

// MARK: - Bindings

/// A menu-bar key equivalent (global; AppKit dispatches it before views).
struct MenuChord: Hashable {
    /// Display key: a single character ("u", "/") or "⌫" for delete.
    let key: String
    let command: Bool
    let shift: Bool
    let control: Bool
    let option: Bool

    init(_ key: String, command: Bool = false, shift: Bool = false,
         control: Bool = false, option: Bool = false) {
        self.key = key
        self.command = command
        self.shift = shift
        self.control = control
        self.option = option
    }

    /// Standard macOS ordering: ⌃ ⌥ ⇧ ⌘ then the key.
    var display: String {
        var out = ""
        if control { out += "⌃" }
        if option { out += "⌥" }
        if shift { out += "⇧" }
        if command { out += "⌘" }
        return out + key.uppercased()
    }
}

// MARK: - Spec

struct CommandSpec {
    let id: CommandID
    /// User-facing menu title (British English).
    let title: String
    /// Cheat-sheet row label (defaults to `title` when nil).
    let help: String?
    /// Menu placement: (menu, section) — section breaks become dividers.
    /// Nil = the command has no menu item.
    let menu: (name: MenuName, section: Int)?
    /// Global menu-bar key equivalent.
    let menuChord: MenuChord?
    /// Message-list bare key. An uppercase letter means Shift+letter.
    let bareKey: Character?
    /// Second key of the two-key `g` chord (message-list context).
    let gChord: Character?
    /// Display-only extra keys for the cheat sheet ("↓", "Return", "⌘⏎").
    let extraKeys: [String]
    /// Notification the menu item / key handler posts; nil = handled locally
    /// (only the composer's ⌘⏎ Send, a SwiftUI shortcut in the compose window).
    let notification: Notification.Name?
    /// Destructive or bulk-mutating message/project actions must never fire
    /// from a global key equivalent while a text field/editor owns focus.
    let destructive: Bool
    /// Cheat-sheet grouping.
    let sheetGroup: SheetGroup?
    /// User key sequences from `keys` in keybindings.json (MM-12). When set it
    /// REPLACES this command's `bareKey` and `gChord` bindings (an empty array
    /// unbinds); nil = the built-in `bareKey`/`gChord` shorthand applies.
    let sequenceOverride: [KeySequence]?

    init(
        _ id: CommandID,
        title: String,
        help: String? = nil,
        menu: (name: MenuName, section: Int)? = nil,
        menuChord: MenuChord? = nil,
        bareKey: Character? = nil,
        gChord: Character? = nil,
        extraKeys: [String] = [],
        notification: Notification.Name? = nil,
        destructive: Bool = false,
        sheetGroup: SheetGroup? = nil,
        sequenceOverride: [KeySequence]? = nil
    ) {
        self.id = id
        self.title = title
        self.help = help
        self.menu = menu
        self.menuChord = menuChord
        self.bareKey = bareKey
        self.gChord = gChord
        self.extraKeys = extraKeys
        self.notification = notification
        self.destructive = destructive
        self.sheetGroup = sheetGroup
        self.sequenceOverride = sequenceOverride
    }

    /// Every key sequence that runs this command: the override when the user
    /// set one, otherwise the built-in shorthand converted: `bareKey` as a
    /// one-stroke sequence, then `gChord` as `g <key>`.
    var sequences: [KeySequence] {
        if let sequenceOverride { return sequenceOverride }
        var out: [KeySequence] = []
        if let bareKey, let seq = KeySequence(strokes: [KeyStroke(character: bareKey)]) { out.append(seq) }
        if let gChord,
           let seq = KeySequence(strokes: [KeyStroke(character: CommandRegistry.chordPrefix), KeyStroke(character: gChord)]) {
            out.append(seq)
        }
        return out
    }

    /// A copy with another title and cheat-sheet label (the palette resolves a
    /// tag slot to the tag's name this way).
    func retitled(_ title: String) -> CommandSpec {
        CommandSpec(id, title: title, help: title, menu: menu, menuChord: menuChord, bareKey: bareKey,
                    gChord: gChord, extraKeys: extraKeys, notification: notification,
                    destructive: destructive, sheetGroup: sheetGroup, sequenceOverride: sequenceOverride)
    }

    /// Cheat-sheet keycap strings for the non-chord bindings, in display
    /// order: bare key first (the fastest), then the menu chord, then extras.
    /// Deduplicated: a modifier-less menu chord that mirrors the bare key
    /// (Assign to Project's "p") renders one keycap, not two.
    var sheetKeys: [String] {
        // One-stroke sequences are keycaps; longer ones render in the
        // "Go to mailbox" group (see `sheetGroups(from:)`).
        var keys = sequences.filter { $0.strokes.count == 1 }.map(\.display)
        if let menuChord { keys.append(menuChord.display) }
        keys.append(contentsOf: extraKeys)
        var seen = Set<String>()
        return keys.filter { seen.insert($0).inserted }
    }
}

/// Application menus a command can live in (order = menu-bar order).
enum MenuName: CaseIterable {
    case app, file, search, message, view, window, people, projects, help
}

/// Cheat-sheet sections (order = display order).
enum SheetGroup: CaseIterable {
    case navigate, gotoMailbox, actOnSelection, compose, projectsPeople, help

    var title: String {
        switch self {
        case .navigate: return "Navigate"
        case .gotoMailbox: return "Go to mailbox"
        case .actOnSelection: return "Act on selection"
        case .compose: return "Compose"
        case .projectsPeople: return "Projects & people"
        case .help: return "Help"
        }
    }

    var symbol: String {
        switch self {
        case .navigate: return "arrow.up.arrow.down"
        case .gotoMailbox: return "folder"
        case .actOnSelection: return "envelope.open"
        case .compose: return "square.and.pencil"
        case .projectsPeople: return "person.2"
        case .help: return "questionmark.circle"
        }
    }
}

// MARK: - Registry

enum CommandRegistry {
    /// The authoritative command table. Order within a (menu, section) pair
    /// is menu item order; order within a sheet group is row order.
    static let all: [CommandSpec] = [
        // Application ---------------------------------------------------------
        CommandSpec(.checkForUpdates, title: "Check for Updates…",
                    menu: (.app, 0),
                    notification: .castellumCheckForUpdates),

        // Message-list navigation (local dispatch; first so the cheat sheet's
        // Navigate group keeps its accepted J/K/Return/Search row order)
        CommandSpec(.nextMessage, title: "Next Message",
                    help: "Next message",
                    bareKey: "j", extraKeys: ["↓"],
                    notification: .castellumNextMessage,
                    sheetGroup: .navigate),
        CommandSpec(.previousMessage, title: "Previous Message",
                    help: "Previous message",
                    bareKey: "k", extraKeys: ["↑"],
                    notification: .castellumPreviousMessage,
                    sheetGroup: .navigate),
        CommandSpec(.openMessage, title: "Open Message",
                    help: "Open in reading pane",
                    extraKeys: ["Return"],
                    notification: .castellumOpenMessage,
                    sheetGroup: .navigate),
        CommandSpec(.openInWindow, title: "Open in Window",
                    help: "Open in its own window",
                    extraKeys: ["Double-click"],
                    notification: .castellumOpenInWindow,
                    sheetGroup: .navigate),

        // Compose / search ---------------------------------------------------
        CommandSpec(.compose, title: "New Message",
                    menu: (.file, 0),
                    menuChord: MenuChord("n", command: true),
                    bareKey: "c",
                    notification: .castellumCompose,
                    sheetGroup: .compose),
        CommandSpec(.importEml, title: "Import…",
                    help: "Import .eml files into the selected mailbox",
                    menu: (.file, 1),
                    notification: .castellumImportEml),
        CommandSpec(.exportEml, title: "Export as .eml…",
                    menu: (.file, 1),
                    notification: .castellumExportEml),
        CommandSpec(.printMessage, title: "Print…",
                    menu: (.file, 2),
                    menuChord: MenuChord("p", command: true),
                    notification: .castellumPrintMessage),
        CommandSpec(.focusSearch, title: "Search Mail",
                    help: "Search",
                    menu: (.search, 0),
                    menuChord: MenuChord("k", command: true),
                    notification: .castellumFocusSearch,
                    sheetGroup: .navigate),
        // Message actions ----------------------------------------------------
        CommandSpec(.reply, title: "Reply",
                    menu: (.message, 0),
                    menuChord: MenuChord("r", command: true),
                    bareKey: "r",
                    notification: .castellumReply,
                    sheetGroup: .compose),
        CommandSpec(.replyAll, title: "Reply All",
                    help: "Reply all",
                    menu: (.message, 0),
                    menuChord: MenuChord("r", command: true, shift: true),
                    bareKey: "R",
                    notification: .castellumReplyAll,
                    sheetGroup: .compose),
        // No bare key: bare f is Flag / Unflag.
        CommandSpec(.forward, title: "Forward",
                    menu: (.message, 0),
                    menuChord: MenuChord("f", command: true, shift: true),
                    notification: .castellumForward,
                    sheetGroup: .compose),
        CommandSpec(.replyWithAi, title: "Reply with AI",
                    help: "Reply with AI — drafts immediately",
                    menu: (.message, 0),
                    notification: .castellumReplyWithAi),
        // Send Later (HP1). The picker command has a menu item and a chord; the
        // four presets are palette/key-binding commands. All of them act on the
        // compose window being written (see ComposeWindowController).
        CommandSpec(.sendLater, title: String(localized: "Send Later…"),
                    help: String(localized: "Send later (in composer)"),
                    menu: (.message, 0),
                    menuChord: MenuChord("l", command: true, option: true),
                    notification: .castellumSendLater,
                    sheetGroup: .compose),
        CommandSpec(.sendLaterInOneHour, title: String(localized: "Send in One Hour"),
                    notification: .castellumSendLaterInOneHour),
        CommandSpec(.sendLaterThisEvening, title: String(localized: "Send This Evening (18:00)"),
                    notification: .castellumSendLaterThisEvening),
        CommandSpec(.sendLaterTomorrowMorning, title: String(localized: "Send Tomorrow Morning (08:00)"),
                    notification: .castellumSendLaterTomorrowMorning),
        CommandSpec(.sendLaterMonday, title: String(localized: "Send Monday Morning (08:00)"),
                    notification: .castellumSendLaterMonday),
        // MM-7: insert a snippet into the compose window being written. Not destructive, so it
        // fires while the body has focus (routed by ComposeWindowController like Send Later).
        CommandSpec(.insertSnippet, title: String(localized: "Insert Snippet…"),
                    help: String(localized: "Insert snippet (in composer)"),
                    menu: (.message, 0),
                    menuChord: MenuChord("i", command: true, option: true),
                    notification: .castellumInsertSnippet,
                    sheetGroup: .compose),
        CommandSpec(.sendComposer, title: "Send",
                    help: "Send (in composer)",
                    extraKeys: ["⌘⏎"],
                    sheetGroup: .compose),
        CommandSpec(.summarize, title: "Summarize",
                    menu: (.message, 1),
                    menuChord: MenuChord("s", command: true, shift: true),
                    notification: .castellumSummarize,
                    sheetGroup: .actOnSelection),
        CommandSpec(.toggleRead, title: "Toggle Read/Unread",
                    help: "Toggle read / unread",
                    menu: (.message, 2),
                    menuChord: MenuChord("u", command: true, shift: true),
                    bareKey: "u",
                    notification: .castellumToggleRead,
                    sheetGroup: .actOnSelection),
        // Explicit read / unread verbs (row context menu): toggleRead stays for
        // the chord and the bare `u` key.
        CommandSpec(.markRead, title: "Mark as Read",
                    menu: (.message, 2),
                    notification: .castellumMarkRead,
                    destructive: false),
        CommandSpec(.markUnread, title: "Mark as Unread",
                    menu: (.message, 2),
                    notification: .castellumMarkUnread),
        CommandSpec(.markAllRead, title: "Mark All as Read",
                    help: "Mark all as read",
                    menu: (.message, 2),
                    menuChord: MenuChord("a", command: true, control: true),
                    notification: .castellumMarkAllRead,
                    destructive: true,
                    sheetGroup: .actOnSelection),
        CommandSpec(.toggleFlag, title: "Flag / Unflag",
                    help: "Flag / unflag (★)",
                    menu: (.message, 2),
                    menuChord: MenuChord("l", command: true, shift: true),
                    bareKey: "f",
                    notification: .castellumToggleFlag,
                    sheetGroup: .actOnSelection),
        CommandSpec(.tagMessage, title: "Tag…",
                    help: "Tag…",
                    menu: (.message, 2),
                    menuChord: MenuChord("t", command: true, option: true),
                    bareKey: "l",
                    notification: .castellumTagMessage,
                    sheetGroup: .actOnSelection),
        CommandSpec(.toggleMuteThread, title: "Mute / Unmute Conversation",
                    help: "Mute / unmute conversation",
                    menu: (.message, 2),
                    bareKey: "m",
                    notification: .castellumToggleMuteThread,
                    sheetGroup: .actOnSelection),
        CommandSpec(.toggleVipSender, title: "Add / Remove Sender as VIP",
                    help: "Add or remove the sender as a VIP",
                    menu: (.message, 2),
                    bareKey: "v",
                    notification: .castellumToggleVipSender,
                    sheetGroup: .actOnSelection),
        CommandSpec(.snooze, title: "Snooze…",
                    help: "Snooze",
                    menu: (.message, 2),
                    bareKey: "b",
                    notification: .castellumSnooze,
                    sheetGroup: .actOnSelection),
        // Snooze presets: palette / row-menu commands acting on the selection
        // (no menu-bar placement, like the sendLater presets).
        CommandSpec(.snoozeLaterToday, title: SnoozePreset.laterToday.title,
                    notification: .castellumSnoozeLaterToday),
        CommandSpec(.snoozeTomorrow, title: SnoozePreset.tomorrow.title,
                    notification: .castellumSnoozeTomorrow),
        CommandSpec(.snoozeThisWeekend, title: SnoozePreset.thisWeekend.title,
                    notification: .castellumSnoozeThisWeekend),
        CommandSpec(.snoozeNextWeek, title: SnoozePreset.nextWeek.title,
                    notification: .castellumSnoozeNextWeek),
        CommandSpec(.findSimilar, title: "Find Similar",
                    notification: .castellumFindSimilar),
        CommandSpec(.unsnooze, title: "Move to Inbox Now",
                    help: "Un-snooze (from the Snoozed list)",
                    menu: (.message, 2),
                    bareKey: "B",
                    notification: .castellumUnsnooze,
                    sheetGroup: .actOnSelection),
        // Undo for Archive / Delete / Move (#3022). No `menuChord`: Cmd-Z belongs to
        // the system Edit > Undo item, which the UndoManager registration drives.
        CommandSpec(.undoRemoval, title: "Undo Archive or Delete",
                    help: "Undo the last archive, delete or move",
                    menu: (.message, 3),
                    bareKey: "z",
                    notification: .castellumUndoRemoval,
                    sheetGroup: .actOnSelection),
        CommandSpec(.archive, title: "Archive",
                    menu: (.message, 3),
                    menuChord: MenuChord("e", command: true),
                    bareKey: "e",
                    notification: .castellumArchive,
                    destructive: true,
                    sheetGroup: .actOnSelection),
        CommandSpec(.delete, title: "Delete",
                    help: "Delete (to Trash)",
                    menu: (.message, 3),
                    menuChord: MenuChord("⌫", command: true),
                    bareKey: "#",
                    notification: .castellumDelete,
                    destructive: true,
                    sheetGroup: .actOnSelection),
        // Move to Folder (#3022 S4): undoable like Archive and Delete.
        CommandSpec(.moveToFolder, title: "Move to Folder…",
                    help: "Move to folder",
                    menu: (.message, 3),
                    menuChord: MenuChord("m", command: true, control: true),
                    bareKey: "V",
                    notification: .castellumMoveToFolder,
                    sheetGroup: .actOnSelection),
        // Junk / Not Junk (row context menu, #3149 slice 3): a move to the Junk
        // mailbox / back to the Inbox through the same guarded, undoable path.
        CommandSpec(.markJunk, title: "Mark as Junk",
                    menu: (.message, 3),
                    notification: .castellumMarkJunk,
                    destructive: true),
        CommandSpec(.markNotJunk, title: "Not Junk",
                    menu: (.message, 3),
                    notification: .castellumMarkNotJunk,
                    destructive: true),
        CommandSpec(.emptyTrash, title: "Empty Trash…",
                    menu: (.message, 3),
                    notification: .castellumEmptyTrash,
                    destructive: true),
        CommandSpec(.showOutbox, title: "Outbox",
                    menu: (.message, 4),
                    notification: .castellumShowOutbox),
        // OB1: send every waiting message now (Mail's Send shortcut).
        CommandSpec(.sendAllOutbox, title: "Send All Waiting Messages",
                    help: "Send every message waiting in the Outbox now",
                    menu: (.message, 4),
                    menuChord: MenuChord("d", command: true, shift: true),
                    notification: .castellumSendAllOutbox),

        // View ----------------------------------------------------------------
        CommandSpec(.showCommandPalette, title: "Command Palette…",
                    help: "Command palette — find and run any command",
                    menu: (.view, 0),
                    menuChord: MenuChord("k", command: true, shift: true),
                    notification: .castellumShowCommandPalette,
                    sheetGroup: .help),
        CommandSpec(.fitColumns, title: "Fit Columns",
                    menu: (.view, 0),
                    notification: .castellumFitColumns),
        CommandSpec(.layoutWidescreen, title: "Widescreen Layout",
                    help: "Message list beside the reading pane",
                    menu: (.view, 2),
                    notification: .castellumLayoutWidescreen),
        CommandSpec(.layoutClassic, title: "Classic Layout",
                    help: "Message list above the reading pane",
                    menu: (.view, 2),
                    notification: .castellumLayoutClassic),
        // MM-13: header display modes (View > Header). Option-Command-1/2/3.
        CommandSpec(.headerCompact, title: "Compact Headers",
                    help: "Show only subject, sender and date",
                    menu: (.view, 5),
                    menuChord: MenuChord("1", command: true, option: true),
                    notification: .castellumHeaderCompact),
        CommandSpec(.headerExtended, title: "Extended Headers",
                    help: "Show To and Cc as well",
                    menu: (.view, 5),
                    menuChord: MenuChord("2", command: true, option: true),
                    notification: .castellumHeaderExtended),
        CommandSpec(.headerAll, title: "All Headers",
                    help: "Show every header, including Bcc, Reply-To and Message-ID",
                    menu: (.view, 5),
                    menuChord: MenuChord("3", command: true, option: true),
                    notification: .castellumHeaderAll),
        CommandSpec(.showCorrespondence, title: "Show Correspondence with Sender",
                    help: "All mail to and from the selected message's sender, with statistics",
                    menu: (.view, 3),
                    notification: .castellumShowCorrespondence),
        // HP-2: Split Inbox tabs (the same chords as next/previous tab in Safari).
        CommandSpec(.nextSplit, title: "Next Split",
                    help: "Next Split Inbox tab",
                    menu: (.view, 4),
                    menuChord: MenuChord("]", command: true, shift: true),
                    notification: .castellumNextSplit),
        CommandSpec(.previousSplit, title: "Previous Split",
                    help: "Previous Split Inbox tab",
                    menu: (.view, 4),
                    menuChord: MenuChord("[", command: true, shift: true),
                    notification: .castellumPreviousSplit),
        // MM-13: the same switch as Settings > General > "Always load remote images". Off by default;
        // a per-message "Load" and a per-sender "Always allow" stay in the message banner.
        CommandSpec(.toggleRemoteImages, title: String(localized: "Load Remote Images"),
                    help: String(localized: "Load remote images in all messages (off = blocked; trackers stay out)"),
                    menu: (.view, 6),
                    notification: .castellumToggleRemoteImages),
        CommandSpec(.viewSource, title: "Message Source",
                    help: "Raw source and headers",
                    menu: (.view, 1),
                    menuChord: MenuChord("u", command: true, option: true),
                    notification: .castellumViewSource),
        CommandSpec(.expandAllQuotes, title: String(localized: "Expand All Quotes"),
                    help: String(localized: "Open every folded quote and signature in this message"),
                    menu: (.view, 1),
                    notification: .castellumExpandAllQuotes),

        // Window ---------------------------------------------------------------
        CommandSpec(.showStatus, title: "Status",
                    help: "Sync status dashboard",
                    menu: (.window, 0),
                    notification: .castellumShowStatus),
        CommandSpec(.showAiActivity, title: "AI Activity",
                    help: "AI jobs — summaries, drafts",
                    menu: (.window, 0),
                    notification: .castellumShowAiActivity),
        CommandSpec(.showSnippets, title: String(localized: "Snippets"),
                    help: String(localized: "Snippets library"),
                    menu: (.window, 0),
                    notification: .castellumShowSnippets),
        // FP44: the way back after the main window was closed (routed by
        // AppCommandRouter, which outlives the main window).
        CommandSpec(.showMainWindow, title: "Main Window",
                    help: "Show the main window",
                    menu: (.window, 1),
                    menuChord: MenuChord("0", command: true),
                    notification: .castellumShowMainWindow),

        // g-chord mailbox jumps (before the window chords so the cheat sheet's
        // "Go to mailbox" group lists mailboxes first, as accepted in PR #70)
        CommandSpec(.gotoInbox, title: "Go to Inbox", help: "Inbox", gChord: "i",
                    notification: .castellumGotoInbox),
        CommandSpec(.gotoArchive, title: "Go to Archive", help: "Archive", gChord: "a",
                    notification: .castellumGotoArchive),
        CommandSpec(.gotoSent, title: "Go to Sent", help: "Sent", gChord: "s",
                    notification: .castellumGotoSent),
        CommandSpec(.gotoTrash, title: "Go to Trash", help: "Trash", gChord: "t",
                    notification: .castellumGotoTrash),
        CommandSpec(.gotoDrafts, title: "Go to Drafts", help: "Drafts", gChord: "d",
                    notification: .castellumGotoDrafts),
        CommandSpec(.gotoJunk, title: "Go to Junk", help: "Junk", gChord: "j",
                    notification: .castellumGotoJunk),
        // MM-8: g l opens the tag picker in go-to mode (type a name, Return).
        CommandSpec(.gotoTag, title: "Go to Tag…", help: "Tag (open its message list)", gChord: "l",
                    notification: .castellumGotoTag),

        // People / projects ----------------------------------------------------
        CommandSpec(.showContacts, title: "Contacts",
                    help: "Contacts window",
                    menu: (.people, 0),
                    menuChord: MenuChord("c", command: true, shift: true),
                    gChord: "c",
                    notification: .castellumShowContacts,
                    sheetGroup: .projectsPeople),
        CommandSpec(.showProjects, title: "Projects Board",
                    help: "Projects Board window",
                    menu: (.projects, 0),
                    menuChord: MenuChord("p", command: true, shift: true),
                    gChord: "p",
                    notification: .castellumShowProjects,
                    sheetGroup: .projectsPeople),
        CommandSpec(.assignProject, title: "Assign to Project",
                    help: "Assign to project",
                    menu: (.projects, 0),
                    menuChord: MenuChord("p"),
                    bareKey: "p",
                    notification: .castellumAssignProject,
                    sheetGroup: .projectsPeople),

        // Help -----------------------------------------------------------------
        CommandSpec(.showShortcuts, title: "Keyboard Shortcuts",
                    help: "Show this cheat sheet",
                    menu: (.help, 0),
                    menuChord: MenuChord("/", command: true),
                    bareKey: "?",
                    notification: .castellumShowShortcuts,
                    sheetGroup: .help),
        CommandSpec(.showDiagnostics, title: "Diagnostics",
                    menu: (.help, 1),
                    notification: .castellumShowDiagnostics),
    ] + tagSlotSpecs

    /// MM-8: `tagSlot1`...`tagSlot9`, bare keys `1`...`9`. Generated in a loop
    /// (T6): they show in the cheat sheet and the palette and can be rebound
    /// through `KeyBindingOverride` like any other command. The palette
    /// shows the resolved tag name and hides empty slots; the router passes a
    /// digit through when its slot has no tag (`KeySequenceRouter.isEnabled`).
    static let tagSlotSpecs: [CommandSpec] = (1...9).compactMap { n in
        guard let id = CommandID.tagSlot(n) else { return nil }
        return CommandSpec(id, title: "Toggle Tag \(n)",
                           help: "Toggle tag \(n) (Settings order)",
                           bareKey: Character(String(n)),
                           notification: Notification.Name.castellumTagSlot(n),
                           sheetGroup: .actOnSelection)
    }

    static func spec(for id: CommandID) -> CommandSpec {
        // CommandRegistryTests pins that every CommandID has exactly one entry.
        all.first { $0.id == id }!
    }

    /// The bare key that starts the built-in `g …` mailbox-jump chords. It is
    /// no longer special-cased by the router: `g` is simply a one-stroke prefix
    /// of the two-stroke sequences, and binding `g` to a command is reported as
    /// a `shadowedByPrefix` problem (K7).
    static let chordPrefix: Character = "g"

    /// The command whose built-in one-key shorthand is `key` and that has a
    /// notification. Defaults to the built-in table; production routing goes
    /// through `KeySequenceTable` (S45, MM-12).
    static func command(forBareKey key: Character, in specs: [CommandSpec] = all) -> CommandSpec? {
        specs.first { $0.bareKey == key && $0.notification != nil }
    }

    /// The command bound to the second key of a built-in `g …` chord. Same
    /// default-specs shape as `command(forBareKey:in:)`.
    static func command(forGChord key: Character, in specs: [CommandSpec] = all) -> CommandSpec? {
        specs.first { $0.gChord == key }
    }

    /// Commands that act on the compose window being written. A bare key
    /// pressed in the main window would reach the wrong (or an unseen) compose
    /// window, so these are never sequence-bindable (Critic note 6).
    static let composeWindowCommands: Set<CommandID> = [
        .sendLater, .sendLaterInOneHour, .sendLaterThisEvening, .sendLaterTomorrowMorning,
        .sendLaterMonday,
        // Its observer (ComposeWindowController) uses the same "frontmost or last
        // compose window" fallback as Send Later, so a bare key in the main window
        // would open the snippet picker in a compose window the user cannot see.
        .insertSnippet,
    ]

    /// Commands whose keys the user cannot change: ⌘⏎ Send is a SwiftUI
    /// shortcut in another window (K10), plus the compose-window commands.
    /// `testEveryCommandIsBindableOrPinnedFixed` pins this set.
    static let fixedBindingCommands: Set<CommandID> = composeWindowCommands.union([CommandID.sendComposer])

    /// Notification commands the palette does not list: the list-navigation
    /// keys and the role jumps were key-only before MM-12 gave them
    /// notifications (listing them is a separate product decision).
    static let paletteExcluded: Set<CommandID> = [
        .nextMessage, .previousMessage, .openMessage,
        .gotoInbox, .gotoArchive, .gotoSent, .gotoTrash, .gotoDrafts, .gotoJunk,
    ]

    /// Can a key sequence run this command?
    static func isBindable(_ spec: CommandSpec) -> Bool {
        spec.notification != nil && !composeWindowCommands.contains(spec.id)
    }

    /// Menu chords macOS or the standard App/Edit/Window menus own that the
    /// registry does not define (Settings, Quit, Close, Hide, Hide Others,
    /// Minimise, Undo/Redo, Cut/Copy/Paste, Select All, Find, Full Screen).
    /// No registry command may use one (FP48).
    static let reservedMenuChords: Set<String> = Set([
        MenuChord(",", command: true), MenuChord("q", command: true),
        MenuChord("w", command: true), MenuChord("h", command: true),
        MenuChord("h", command: true, option: true), MenuChord("m", command: true),
        MenuChord("z", command: true), MenuChord("z", command: true, shift: true),
        MenuChord("x", command: true), MenuChord("c", command: true),
        MenuChord("v", command: true), MenuChord("a", command: true),
        MenuChord("f", command: true), MenuChord("f", command: true, control: true),
    ].map(\.display))

    /// Every problem in `specs`, as named values: the invariants
    /// `CommandRegistryTests` pins for the built-in table (unique menu chords,
    /// reserved chords, unique keys), now over key SEQUENCES, so a sequence that
    /// is a strict prefix of another is reported instead of silently shadowing
    /// it (K7), plus a sequence on a command no sequence can run and a menu
    /// chord on a command with no menu item (K11). `KeyBindingsStore` runs the
    /// same check after each override. Empty = no problems.
    static func problems(in specs: [CommandSpec]) -> [KeyBindingProblem] {
        var out: [KeyBindingProblem] = []
        let menuOwners = Dictionary(grouping: specs.compactMap { spec in spec.menuChord.map { ($0.display, spec.id) } },
                                    by: \.0)
        for display in menuOwners.keys.sorted() where menuOwners[display]!.count > 1 {
            let ids = menuOwners[display]!.map { $0.1.rawValue }
            out.append(KeyBindingProblem(
                commandId: ids.last, kind: .duplicate,
                message: String(localized: "menu shortcut \(display) is bound to more than one command (\(ids.joined(separator: ", ")))")))
        }
        // FP48: a chord macOS or the standard menus already own would be
        // shown on the item but never reach it.
        for spec in specs {
            if let chord = spec.menuChord, reservedMenuChords.contains(chord.display) {
                out.append(KeyBindingProblem(
                    commandId: spec.id.rawValue, kind: .reservedChord,
                    message: String(localized: "menu shortcut \(chord.display) is reserved by macOS (\(spec.id.rawValue))")))
            }
            if spec.menu == nil, let chord = spec.menuChord,
               chord != all.first(where: { $0.id == spec.id })?.menuChord {
                out.append(KeyBindingProblem(
                    commandId: spec.id.rawValue, kind: .noMenuItem,
                    message: String(localized: "\(spec.id.rawValue) has no menu item, so a menu shortcut can never fire; bind it with \"keys\" instead.")))
            }
            if !isBindable(spec), !spec.sequences.isEmpty {
                out.append(KeyBindingProblem(
                    commandId: spec.id.rawValue, kind: .notBindable,
                    message: String(localized: "\(spec.id.rawValue) cannot be bound to a key sequence.")))
            }
        }
        // Sequences: exact duplicates across (or within) commands, then prefixes.
        var owners: [[KeyStroke]: [CommandID]] = [:]
        var order: [KeySequence] = []
        for spec in specs where isBindable(spec) {
            for seq in spec.sequences {
                if owners[seq.strokes] == nil { order.append(seq) }
                if owners[seq.strokes, default: []].contains(spec.id) { continue }
                owners[seq.strokes, default: []].append(spec.id)
            }
        }
        for seq in order where owners[seq.strokes]!.count > 1 {
            let ids = owners[seq.strokes]!.map(\.rawValue)
            out.append(KeyBindingProblem(
                commandId: ids.last, kind: .duplicate,
                message: String(localized: "key '\(seq.display)' is bound to more than one command (\(ids.joined(separator: ", ")))")))
        }
        for seq in order {
            for n in 1..<max(seq.strokes.count, 1) {
                let head = Array(seq.strokes.prefix(n))
                guard let shadowing = owners[head] else { continue }
                let headText = KeySequence(strokes: head)!.display
                for hidden in owners[seq.strokes]! {
                    out.append(KeyBindingProblem(
                        commandId: hidden.rawValue, kind: .shadowedByPrefix,
                        message: String(localized: "key '\(headText)' (\(shadowing.map(\.rawValue).joined(separator: ", "))) is a prefix of '\(seq.display)' (\(hidden.rawValue)), which could never be reached; a prefix cannot also run a command.")))
                }
            }
        }
        return out
    }

    /// `problems(in:)` as plain sentences (S45's original `[String]` shape).
    static func conflicts(in specs: [CommandSpec]) -> [String] {
        problems(in: specs).map(\.message)
    }

    /// Registry entries whose menu key equivalent or notification make them
    /// destructive-guarded, keyed by notification for the dispatch layer.
    static let destructiveNotifications: Set<Notification.Name> = Set(
        all.filter(\.destructive).compactMap(\.notification)
    )

    // MARK: Cheat sheet derivation

    /// The shortcuts sheet content, derived from the registry:
    /// - `gotoMailbox` lists every command with a multi-key sequence (the
    ///   built-in `g` chords render as "G X" keycaps) in registry order;
    /// - every other group lists its member commands' non-chord keys.
    static func sheetGroups(from specs: [CommandSpec] = all) -> [ShortcutsView.Group] {
        SheetGroup.allCases.compactMap { group in
            let items: [ShortcutsView.Shortcut]
            if group == .gotoMailbox {
                items = specs.compactMap { spec in
                    let chords = spec.sequences.filter { $0.strokes.count > 1 }.map(\.display)
                    guard !chords.isEmpty else { return nil }
                    return ShortcutsView.Shortcut(chords, spec.help ?? spec.title)
                }
            } else {
                items = specs.compactMap { spec in
                    guard spec.sheetGroup == group, !spec.sheetKeys.isEmpty
                    else { return nil }
                    return ShortcutsView.Shortcut(spec.sheetKeys, spec.help ?? spec.title)
                }
            }
            return items.isEmpty ? nil : ShortcutsView.Group(
                title: group.title, symbol: group.symbol, items: items)
        }
    }
}

// MARK: - Focus policy & dispatch

enum CommandFocusPolicy {
    /// True when the responder is a text-editing target. `NSTextView` covers
    /// the field editor used by `NSTextField`/`NSSearchField` and SwiftUI
    /// `TextField`/`TextEditor`; checking `NSTextField` directly is a
    /// belt-and-braces fallback for fields asked before editing begins.
    static func isTextEditing(_ responder: NSResponder?) -> Bool {
        responder is NSTextView || responder is NSTextField
    }

    /// Does a text field/editor own focus in the current key window?
    @MainActor
    static var textEditingOwnsFocus: Bool {
        isTextEditing(NSApp.keyWindow?.firstResponder)
    }
}

/// Routes a command notification to its action, enforcing the registry's
/// destructive-command focus policy: a destructive action never fires while a
/// text field/editor owns focus (e.g. ⌘⌫ while typing in compose or search
/// must not delete the selected message).
@MainActor
struct CommandDispatcher {
    /// Injectable for tests; production uses the key-window first responder.
    var isTextEditing: @MainActor () -> Bool = { CommandFocusPolicy.textEditingOwnsFocus }

    func perform(_ notification: Notification.Name, action: () -> Void) {
        if CommandRegistry.destructiveNotifications.contains(notification),
           isTextEditing() {
            return
        }
        action()
    }
}
