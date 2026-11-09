// Castellum - message list: a real multi-column NSTableView.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Named columns the user can resize, reorder, show/hide (right-click header), and
// double-click a divider to auto-fit. Layout persists via autosave. Return opens
// the selected message; arrows navigate (selection loads the reading pane).

import SwiftUI
import AppKit
import os

// Not `private`: MessageListColumnTests asserts the account column is actually
// wired in here, which is the regression a pure-helper test cannot catch.
let kColumns: [(id: String, title: String, menu: String, width: CGFloat, min: CGFloat, max: CGFloat)] = [
    ("unread", "", String(localized: "Unread"), 16, 16, 16),
    ("star", "", String(localized: "Flag"), 16, 16, 16),
    ("tags", String(localized: "Tags"), String(localized: "Tags"), 44, 20, 160),
    // #3042: From/Subject/Account/Received minimums are what the narrowest list pane
    // (420 pt) can hold with Account forced on in All Inboxes; see MessageListColumnLayout.
    ("from", String(localized: "From"), String(localized: "From"), 160, 64, 400),
    ("subject", String(localized: "Subject"), String(localized: "Subject"), 260, 84, 800),
    ("preview", String(localized: "Preview"), String(localized: "Preview"), 240, 80, 1200),
    ("account", String(localized: "Account"), String(localized: "Account"), 110, 40, 300),
    ("thread", String(localized: "Thread"), String(localized: "Thread"), 58, 44, 120),
    ("attach", "", String(localized: "Attachments"), 24, 24, 24),
    // 80 pt holds the widest label, "12 Jun 2024" (77 pt in its field).
    ("date", String(localized: "Received"), String(localized: "Received"), 96, 80, 160),
]

/// A tag definition's named palette colour (S42) — IMAP has no colour
/// standard, so this mapping is entirely local. Shared by the list's Tags
/// column, `TagPickerView` and `TagsSettingsTab`.
enum TagColour: String, CaseIterable {
    case red, orange, yellow, green, blue, purple, grey

    /// `NSColor` for the dot/swatch. An unrecognised stored value (a colour
    /// name from a future version, or hand-edited data) falls back to grey
    /// rather than crashing or showing nothing.
    var nsColor: NSColor {
        switch self {
        case .red: return .systemRed
        case .orange: return .systemOrange
        case .yellow: return .systemYellow
        case .green: return .systemGreen
        case .blue: return .systemBlue
        case .purple: return .systemPurple
        case .grey: return .systemGray
        }
    }

    /// `raw` as stored in `TagDefinition.colour`; unknown/empty -> `.grey`.
    static func named(_ raw: String) -> TagColour {
        TagColour(rawValue: raw) ?? .grey
    }

    /// Round-robins through the palette by how many tags already exist —
    /// "New Tag…"'s default colour, per the plan.
    static func next(after count: Int) -> TagColour {
        allCases[count % allCases.count]
    }
}

/// The list's Tags column cell (S42): one coloured "●" per visible keyword
/// on the message (max 4, then "+n"), followed by a mute glyph. Pure so the
/// rendering and its accessibility label are unit-testable without a live
/// `NSTableView` — `MessageListColumnTests` exercises both.
enum TagsCell {
    static let maxDots = 4

    /// Colour for `keyword`, matched case-insensitively against every
    /// `TagDefinition.keyword` — a keyword with no local definition (a
    /// server sent it, nobody's named it yet) falls back to grey, same as
    /// an unrecognised stored colour.
    static func colour(forKeyword keyword: String, tags: [TagDefinition]) -> TagColour {
        guard let def = tags.first(where: { $0.keyword.caseInsensitiveCompare(keyword) == .orderedSame })
        else { return .grey }
        return TagColour.named(def.colour)
    }

    static func attributedString(for h: MessageHeader, tags: [TagDefinition]) -> NSAttributedString {
        let out = NSMutableAttributedString()
        let font = NSFont.systemFont(ofSize: 11)
        for keyword in h.keywords.prefix(maxDots) {
            out.append(NSAttributedString(
                string: "●",
                attributes: [.font: font, .foregroundColor: colour(forKeyword: keyword, tags: tags).nsColor]))
        }
        let overflow = h.keywords.count - maxDots
        if overflow > 0 {
            out.append(NSAttributedString(
                string: "+\(overflow)",
                attributes: [.font: NSFont.systemFont(ofSize: 9), .foregroundColor: NSColor.secondaryLabelColor]))
        }
        if h.threadMuted {
            if out.length > 0 {
                out.append(NSAttributedString(string: " "))
            }
            let attachment = NSTextAttachment()
            attachment.image = NSImage(
                systemSymbolName: "bell.slash", accessibilityDescription: nil)?
                .withSymbolConfiguration(.init(pointSize: 10, weight: .regular))
            out.append(NSAttributedString(attachment: attachment))
        }
        return out
    }

    /// "Tags: Work, Later. Muted." (either clause omitted when absent; both
    /// absent -> "").
    static func accessibilityLabel(for h: MessageHeader, tags: [TagDefinition]) -> String {
        var parts: [String] = []
        if !h.keywords.isEmpty {
            parts.append(String(localized: "Tags: \(h.keywords.joined(separator: ", "))."))
        }
        if h.threadMuted {
            parts.append(String(localized: "Muted."))
        }
        return parts.joined(separator: " ")
    }
}

/// The Subject cell (#2245). While the Tags column is hidden (a new install hides
/// it so Received fits the pane, #3042) the tag dots and the mute glyph lead the
/// subject instead, so tagging or muting a message always shows on its row.
/// With the Tags column visible the subject stays plain: the dots are not drawn twice.
enum SubjectCell {
    /// nil = plain subject (Tags column visible, or nothing to show).
    static func attributedString(subject: String, header h: MessageHeader, tags: [TagDefinition],
                                 font: NSFont, tagsColumnVisible: Bool) -> NSAttributedString? {
        guard !tagsColumnVisible, !h.keywords.isEmpty || h.threadMuted else { return nil }
        let out = NSMutableAttributedString(attributedString: TagsCell.attributedString(for: h, tags: tags))
        out.append(NSAttributedString(
            string: " " + subject, attributes: [.font: font, .foregroundColor: NSColor.labelColor]))
        return out
    }

    /// "Subject", plus the Tags column's own wording when the dots are in this cell.
    static func accessibilityLabel(header h: MessageHeader, tags: [TagDefinition], tagsColumnVisible: Bool) -> String {
        let base = String(localized: "Subject")
        guard !tagsColumnVisible else { return base }
        let extra = TagsCell.accessibilityLabel(for: h, tags: tags)
        return extra.isEmpty ? base : base + ". " + extra
    }
}

/// The VIP mark in the "from" cell (S59): an SF Symbol before the sender's
/// name. Deliberately not "★", which the star column already uses for
/// `\Flagged`.
enum VipSenderCell {
    static func attributedString(name: String, font: NSFont) -> NSAttributedString {
        let out = NSMutableAttributedString()
        let attachment = NSTextAttachment()
        attachment.image = NSImage(
            systemSymbolName: "person.crop.circle.badge.checkmark", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: font.pointSize - 1, weight: .regular))
        out.append(NSAttributedString(attachment: attachment))
        out.append(NSAttributedString(string: " "))
        out.append(NSAttributedString(
            string: name, attributes: [.font: font, .foregroundColor: NSColor.labelColor]))
        return out
    }

    /// "From" for a normal sender, "From, VIP" for a VIP.
    static func accessibilityLabel(isVip: Bool) -> String {
        isVip ? String(localized: "From, VIP") : String(localized: "From")
    }
}

/// The per-row source-account indicator.
///
/// Deliberately a quiet named column rather than a badge: Marc asked to see
/// "from which email source does a mail come" without losing list density, and
/// this becomes load-bearing once unified/smart views land. Pure so the rules
/// below are unit-tested rather than eyeballed.
enum AccountColumn {
    /// What the column shows for a message: the account's email, falling back
    /// to the raw id if the account list has not loaded yet (never blank — a
    /// blank cell would read as "no account" rather than "not loaded").
    static func label(forAccountId id: String, in accounts: [Account]) -> String {
        labels(for: accounts)[id] ?? id
    }

    /// account id -> label for every configured account.
    static func labels(for accounts: [Account]) -> [String: String] {
        Dictionary(accounts.map { ($0.id, $0.email) }, uniquingKeysWith: { first, _ in first })
    }

    /// The table column this indicator lives in. Named once so the column
    /// spec, the cell text and the styling cannot drift apart silently.
    static let columnId = "account"

    /// Appearance-adaptive tints, indexed by a stable hash of the account id so
    /// an account keeps its colour across launches and reorderings (the array
    /// index would not: accounts arrive in DB order).
    static let tints: [NSColor] = [
        .systemBlue, .systemPurple, .systemTeal,
        .systemPink, .systemOrange, .systemIndigo,
    ]

    /// FNV-1a over the id: deterministic across processes, unlike Swift's
    /// `hashValue`, which is seeded per-launch and would reshuffle colours on
    /// every restart.
    static func tintIndex(forAccountId id: String) -> Int {
        Int(hash64(id) % UInt64(tints.count))
    }

    /// FNV-1a/64. Separated from `tintIndex` so tests can pin the HASH rather
    /// than the palette-sized residue — adding a tint must not fail a test
    /// that exists to catch a change to the mixing constants.
    static func hash64(_ id: String) -> UInt64 {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in id.utf8 {
            hash ^= UInt64(byte)
            hash = hash &* 0x0000_0100_0000_01b3
        }
        return hash
    }

    /// Colour for the cell. Tinting only earns its place when there is more
    /// than one account to tell apart; with a single account it is decoration,
    /// so the text stays in the same secondary grey as Preview and Received.
    static func colour(forAccountId id: String, accountCount: Int) -> NSColor {
        accountCount > 1 ? tints[tintIndex(forAccountId: id)] : .secondaryLabelColor
    }
}

struct MessageListView: NSViewRepresentable {
    @ObservedObject var model: AppModel
    var onOpen: () -> Void = {}

    func makeCoordinator() -> Coordinator { Coordinator(model: model, onOpen: onOpen) }

    /// `UserDefaults` key AppKit stores the table's column layout under
    /// (autosave name "CastellumMessageList.v2"); pinned by `ColumnAutosaveTests`.
    static let autosaveDefaultsKey = "NSTableView Columns v3 CastellumMessageList.v2"
    /// One-shot guard for the #3042 migration of already-saved layouts (spec M1).
    static let dateFitAppliedKey = "CastellumMessageList.dateColumnFitApplied"

    /// The list's column gap (#3042). One place, so the tests' hosted table and
    /// the overhead `MessageListColumnLayout` models match the real one.
    static func applyColumnSpacing(_ table: NSTableView) {
        table.intercellSpacing = NSSize(
            width: MessageListColumnLayout.intercellWidth, height: table.intercellSpacing.height)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let table = KeyTableView()
        table.rowHeight = 22
        table.usesAlternatingRowBackgroundColors = false
        table.allowsColumnReordering = true
        table.allowsColumnResizing = true
        table.allowsColumnSelection = false
        table.allowsMultipleSelection = true
        // User-controlled widths only; no auto-resize (which fights autosave and
        // makes the last column drift wider on each relaunch).
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.style = .inset
        Self.applyColumnSpacing(table)
        // S49: drag .eml files onto the list to import them into the
        // selected mailbox.
        table.registerForDraggedTypes([.fileURL])

        for spec in kColumns {
            let c = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(spec.id))
            c.title = spec.title
            c.width = spec.width
            c.minWidth = spec.min
            c.maxWidth = spec.max
            c.resizingMask = [.userResizingMask]
            // #3043: Received, From and Subject sort by a header click. The prototype's
            // `ascending` is the direction of the FIRST click (AppKit flips it on the
            // next): newest first for dates, A to Z for text. `updateNSView` clears it
            // where a column cannot sort (From in Sent and Drafts).
            if let key = MessageSortKey(columnId: spec.id) {
                c.sortDescriptorPrototype = NSSortDescriptor(
                    key: key.rawValue, ascending: MessageSort.firstClick(on: key).ascending)
            }
            table.addTableColumn(c)
        }

        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        // Return posts the Open Message command (MM-12), so a key sequence and Return share one path.
        table.onReturn = { NotificationCenter.default.post(name: .castellumOpenMessage, object: nil) }
        table.target = context.coordinator
        table.doubleAction = #selector(Coordinator.doubleClicked)
        context.coordinator.table = table


        // Row right-click menu: registry-driven (MessageContextMenu), rendered
        // in `menuNeedsUpdate`. An empty menu (right-click on blank space)
        // shows nothing.
        let rowMenu = NSMenu()
        rowMenu.autoenablesItems = false
        rowMenu.delegate = context.coordinator
        table.menu = rowMenu

        // Header: double-click a divider to auto-fit; right-click to show/hide.
        let header = ColumnHeaderView()
        header.autofit = { [weak c = context.coordinator] i in c?.autofit(columnIndex: i) }
        table.headerView = header
        let menu = NSMenu()
        for spec in kColumns where spec.id != "unread" && spec.id != "star" {
            let item = NSMenuItem(title: spec.menu, action: #selector(Coordinator.toggleColumn(_:)), keyEquivalent: "")
            item.target = context.coordinator
            item.representedObject = table.tableColumns.first { $0.identifier.rawValue == spec.id }
            item.state = .on
            menu.addItem(item)
        }
        menu.delegate = context.coordinator   // ticks are recomputed in menuNeedsUpdate (#3448)
        header.menu = menu

        // Persist column widths/order/visibility across launches. The stored
        // payload is an array of {Identifier, Width, Hidden} — verified by
        // decoding the real one, and pinned by
        // ColumnAutosaveTests.testAColumnAbsentFromSavedDataStaysVisible.
        //
        // The name is deliberately NOT bumped for the account column. Bumping
        // orphans the whole saved layout and drops every user back to factory
        // widths — and it would have to happen again for every future column.
        // A one-shot migration adds the new column instead, leaving every other
        // column's saved width, order and hidden state intact.
        // Capture BEFORE the autosave name is set (and before the migrations
        // below write columns back): "no saved layout" decides whether this
        // is a brand-new install that gets the pane-aware first-run layout.
        context.coordinator.savedLayoutExisted =
            UserDefaults.standard.object(forKey: MessageListView.autosaveDefaultsKey) != nil
        table.autosaveName = "CastellumMessageList.v2"
        table.autosaveTableColumns = true
        Self.migrateAccountColumnIn(table)
        Self.migrateTagsColumnIn(table)

        // Sync the show/hide menu to the RESTORED state. `Hidden` is persisted,
        // so hardcoding `.on` above meant a column hidden in a previous launch
        // came back with a tick beside it — and the user's first click would
        // then UN-hide it while the tick stayed put.
        for item in menu.items {
            guard let col = item.representedObject as? NSTableColumn else { continue }
            item.state = col.isHidden ? .off : .on
        }

        // MM-12: j/k and Open Message are registry commands (key sequences post
        // these); the list only performs the movement.
        for (name, selector) in [
            (Notification.Name.castellumNextMessage, #selector(Coordinator.selectNextRow)),
            (.castellumPreviousMessage, #selector(Coordinator.selectPreviousRow)),
            (.castellumOpenMessage, #selector(Coordinator.openSelectedRow)),
        ] {
            NotificationCenter.default.addObserver(
                context.coordinator, selector: selector, name: name, object: nil)
        }

        // "Fit Columns" menu command → auto-fit every column.
        NotificationCenter.default.addObserver(
            context.coordinator,
            selector: #selector(Coordinator.fitAllColumns),
            name: .castellumFitColumns, object: nil)

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true

        // S64 infinite scroll: ask the model for the next keyset page once
        // the visible rect gets near the bottom of what is loaded.
        // `postsBoundsChangedNotifications` must be turned on explicitly —
        // AppKit doesn't post it by default, to avoid the cost for scroll
        // views that never need it.
        scroll.contentView.postsBoundsChangedNotifications = true
        // #3042: the pane width is only known after the first layout pass, so
        // the pane-aware column fit runs once from the first non-zero frame.
        scroll.contentView.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(
            context.coordinator, selector: #selector(Coordinator.paneFrameDidChange),
            name: NSView.frameDidChangeNotification, object: scroll.contentView)
        NotificationCenter.default.addObserver(
            context.coordinator, selector: #selector(Coordinator.boundsDidChange),
            name: NSView.boundsDidChangeNotification, object: scroll.contentView)

        return scroll
    }

    /// Make the account column visible once for users whose saved layout
    /// predates it, without discarding the rest of that layout.
    ///
    /// Runs after `autosaveName` is set, so it acts on the RESTORED columns.
    /// Guarded by its own defaults key rather than by "is it hidden?", so a
    /// user who deliberately hides the column keeps it hidden.
    private static func migrateAccountColumnIn(_ table: NSTableView) {
        let key = "CastellumMessageList.accountColumnAdded"
        guard !UserDefaults.standard.bool(forKey: key) else { return }
        if let col = table.tableColumns.first(where: { $0.identifier.rawValue == "account" }) {
            col.isHidden = false
            if col.width < 60 { col.width = 110 }
        }
        UserDefaults.standard.set(true, forKey: key)
    }

    /// Same one-shot migration as `migrateAccountColumnIn`, for the Tags
    /// column (S42) — a saved layout that predates it must not have it
    /// hidden by default, but a user who deliberately hides it afterwards
    /// keeps it hidden.
    private static func migrateTagsColumnIn(_ table: NSTableView) {
        let key = "CastellumMessageList.tagsColumnAdded"
        guard !UserDefaults.standard.bool(forKey: key) else { return }
        if let col = table.tableColumns.first(where: { $0.identifier.rawValue == "tags" }) {
            col.isHidden = false
            if col.width < 20 { col.width = 44 }
        }
        UserDefaults.standard.set(true, forKey: key)
    }

    func updateNSView(_ nsView: NSScrollView, context: Context) {
        let coord = context.coordinator
        coord.onOpen = onOpen // keep the closure fresh across SwiftUI re-renders
        // The account column reads `accounts`, which is a SEPARATE @Published
        // property from `messages`. Guarding on rows alone threw away the
        // update when an account was added, removed or renamed, so the column
        // kept a stale email — or a stale "single account, no tint" — until the
        // message array happened to change.
        //
        // S64: `coord.generation != model.messagesGeneration` replaces the old
        // `coord.rows != model.messages` array comparison — an O(1) integer
        // compare instead of an O(n) element-wise `Equatable` walk, which at
        // 10k+ loaded rows cost milliseconds on every SwiftUI render.
        let labels = AccountColumn.labels(for: model.accounts)
        // The Tags column reads `model.tags`, also separate from `messages`: a recolour,
        // rename or delete in Settings must repaint the dots now, without a full reload
        // that would lose scroll and selection (#3448). Sort order is not part of it.
        let tagsSignature = model.tags.map { "\($0.keywordKey)|\($0.colour)|\($0.displayName)" }
        let tagsChanged = coord.tagsSignature != tagsSignature
        // Any cross-account list shows Account, but only when there is more than one account to
        // tell apart: a single-account user who hid it is not overridden (#3459).
        let showAccountColumn = model.isCrossAccountList && model.accounts.count > 1
        coord.tagsSignature = tagsSignature
        if let table = nsView.documentView as? NSTableView { coord.syncSortIndicator(table: table) }
        guard coord.generation != model.messagesGeneration || coord.accountLabels != labels
            || coord.lastIsUnified != showAccountColumn
        else {
            if tagsChanged, let table = nsView.documentView as? NSTableView {
                coord.reloadTagsColumn(table: table)
            }
            return
        }
        coord.accountLabels = labels
        coord.generation = model.messagesGeneration
        guard let table = nsView.documentView as? NSTableView else {
            coord.rows = model.messages
            return
        }
        coord.syncAccountColumnVisibility(table: table, unified: showAccountColumn)
        coord.lastIsUnified = showAccountColumn
        coord.apply(model: model, table: table)
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate {
        let model: AppModel
        var onOpen: () -> Void
        var rows: [MessageHeader] = []
        /// account id -> label, refreshed from `model.accounts` in
        /// `updateNSView`. Cached rather than looked up per cell so a change to
        /// `accounts` is DETECTABLE (it drives the reload guard); the O(n)
        /// lookup itself was never the cost.
        var accountLabels: [String: String] = [:]
        var isReloading = false
        weak var table: NSTableView?
        /// The `model.messagesGeneration` value as of the last `apply` —
        /// `updateNSView`'s O(1) reload guard (S64).
        var generation = -1
        /// S64 GUI launch probe: true once this coordinator has emitted its
        /// one `PERF gui_first_list` line. Guards against a second render
        /// re-emitting it (or trying to re-terminate the app) — the probe
        /// only cares about the FIRST non-empty list.
        private var didEmitFirstListPerfSignal = false
        private static let perfSignposter = OSSignposter(subsystem: "uk.co.riera.castellum", category: "Perf")

        /// Set true only while this coordinator itself forced the Account
        /// column visible for a unified list; lets `syncAccountColumnVisibility`
        /// restore EXACTLY the state it found (hidden or visible) rather than
        /// guessing, so a user's own hide/show choice is never overwritten.
        var accountColumnForcedVisible = false

        /// "keyword|colour|name" per tag as of the last `updateNSView` (reload guard for the
        /// Tags column, #3448).
        var tagsSignature: [String] = []

        /// Repaint only the Tags column's cells (selection and scroll position survive).
        func reloadTagsColumn(table: NSTableView) {
            let col = table.column(withIdentifier: NSUserInterfaceItemIdentifier("tags"))
            guard col >= 0, !table.tableColumns[col].isHidden, table.numberOfRows > 0 else { return }
            table.reloadData(
                forRowIndexes: IndexSet(integersIn: 0..<table.numberOfRows),
                columnIndexes: IndexSet(integer: col))
        }

        /// The `model.isUnifiedList` value as of the last `updateNSView` call.
        /// `updateNSView`'s own reload guard only fires on a `rows`/
        /// `accountLabels` change; toggling "All Inboxes" on and off across two
        /// mailboxes that happen to render byte-identical visible rows would
        /// otherwise never re-run `syncAccountColumnVisibility` at all. Widening
        /// that guard to also compare this flag (Step 3) closes the gap.
        var lastIsUnified = false

        init(model: AppModel, onOpen: @escaping () -> Void) {
            self.model = model
            self.onOpen = onOpen
        }

        deinit { NotificationCenter.default.removeObserver(self) }

        // MARK: - #3043 header-click sort

        /// True while the coordinator itself writes the table's sort descriptors (the
        /// model is pushing its sort in), so `sortDescriptorsDidChange` does not echo
        /// it back. Also true until the first sync: AppKit may restore descriptors from
        /// a previous launch, and the model, not AppKit, owns the sort.
        private var isApplyingSort = true

        /// Make the table follow the model: arrow on the active column, and sortable
        /// header prototypes only where the list can sort by that column.
        func syncSortIndicator(table: NSTableView) {
            isApplyingSort = true
            defer { isApplyingSort = false }
            let sortable = model.sortableColumnIds
            for col in table.tableColumns {
                let id = col.identifier.rawValue
                guard let key = MessageSortKey(columnId: id) else { continue }
                let has = col.sortDescriptorPrototype != nil
                if sortable.contains(id), !has {
                    col.sortDescriptorPrototype = NSSortDescriptor(
                        key: key.rawValue, ascending: MessageSort.firstClick(on: key).ascending)
                } else if !sortable.contains(id), has {
                    col.sortDescriptorPrototype = nil
                }
            }
            // nil = the list is in its natural order (search relevance, Snoozed due order): no arrow.
            guard let sort = model.displaySort else {
                if !table.sortDescriptors.isEmpty { table.sortDescriptors = [] }
                return
            }
            let want = NSSortDescriptor(key: sort.key.rawValue, ascending: sort.ascending)
            if let have = table.sortDescriptors.first, have.key == want.key, have.ascending == want.ascending { return }
            table.sortDescriptors = [want]
        }

        /// The user clicked a sortable header (AppKit already flipped the descriptor).
        func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
            guard !isApplyingSort, let d = tableView.sortDescriptors.first,
                  let key = d.key.flatMap(MessageSortKey.init(rawValue:)) else { return }
            model.setMessageSort(MessageSort(key: key, ascending: d.ascending))
        }

        // MARK: - S49 .eml drag-and-drop import

        private func emlFileURLs(from info: NSDraggingInfo) -> [URL] {
            let urls = info.draggingPasteboard.readObjects(forClasses: [NSURL.self], options: nil) as? [URL] ?? []
            return urls.filter { $0.pathExtension.lowercased() == "eml" }
        }

        func tableView(
            _ tableView: NSTableView, validateDrop info: NSDraggingInfo,
            proposedRow row: Int, proposedDropOperation dropOperation: NSTableView.DropOperation
        ) -> NSDragOperation {
            guard model.actionMailbox != nil, !emlFileURLs(from: info).isEmpty else { return [] }
            tableView.setDropRow(-1, dropOperation: .on) // whole-list target, not a specific row
            return .copy
        }

        func tableView(
            _ tableView: NSTableView, acceptDrop info: NSDraggingInfo,
            row: Int, dropOperation: NSTableView.DropOperation
        ) -> Bool {
            guard let mailbox = model.actionMailbox else { return false }
            let urls = emlFileURLs(from: info)
            guard !urls.isEmpty else { return false }
            model.importEmlFiles(urls: urls, mailboxId: mailbox.id, accountId: mailbox.accountId)
            return true
        }

        /// The plain string shown in a column (used for cells + autofit sizing).
        private func text(_ columnID: String, _ h: MessageHeader) -> String {
            switch columnID {
            case "unread": return h.isRead ? "" : "●"
            case "star": return h.isStarred ? "★" : ""
            // Autofit sizing only — the real cell below sets an attributed
            // string (coloured dots + a mute glyph); this is a same-length
            // plain-text stand-in so `autofit` still measures a sane width.
            case "tags": return String(repeating: "●", count: min(h.keywords.count, 4))
                + (h.threadMuted ? "!" : "")
            case "from": return messageListSenderText(h)
            case "subject": return h.subject ?? "(no subject)"
            case "preview": return h.snippet ?? ""
            case "account": return accountLabels[h.accountId] ?? h.accountId
            case "thread": return h.threadCount > 1 ? "▾\(h.threadCount)" : ""
            case "attach": return h.hasAttachments ? "📎" : ""
            case "date": return relativeDate(h.sentAt)
            default: return ""
            }
        }

        func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            guard let tableColumn else { return nil }
            let h = rows[row]
            let id = tableColumn.identifier
            let field = (tableView.makeView(withIdentifier: id, owner: self) as? NSTextField) ?? {
                let f = NSTextField(labelWithString: "")
                f.identifier = id
                f.lineBreakMode = .byTruncatingTail
                f.cell?.truncatesLastVisibleLine = true
                return f
            }()
            field.stringValue = text(id.rawValue, h)

            switch id.rawValue {
            case "unread":
                field.textColor = .controlAccentColor
                field.font = .systemFont(ofSize: 10)
                field.alignment = .center
                // The glyph ("●"/"") is decoration; VoiceOver would otherwise
                // read it as "black circle" or announce a silent empty stop.
                field.setAccessibilityElement(!h.isRead)
                field.setAccessibilityLabel(String(localized: "Unread"))
            case "star":
                field.textColor = .systemYellow
                field.font = .systemFont(ofSize: 11)
                field.alignment = .center
                field.setAccessibilityElement(h.isStarred)
                field.setAccessibilityLabel(String(localized: "Flag"))
            case "from":
                field.font = .systemFont(ofSize: 13, weight: h.isRead ? .regular : .semibold)
                field.textColor = .labelColor
                field.alignment = .left
                if h.fromVip {
                    field.attributedStringValue = VipSenderCell.attributedString(
                        name: text("from", h), font: field.font ?? .systemFont(ofSize: 13))
                }
                field.setAccessibilityElement(true)
                field.setAccessibilityLabel(VipSenderCell.accessibilityLabel(isVip: h.fromVip))
            case "subject":
                field.font = .systemFont(ofSize: 13, weight: h.isRead ? .regular : .medium)
                field.textColor = .labelColor
                field.alignment = .left
                let tagsVisible = tableView.tableColumns.first { $0.identifier.rawValue == "tags" }
                    .map { !$0.isHidden } ?? false
                if let dotted = SubjectCell.attributedString(
                    subject: text("subject", h), header: h, tags: model.tags,
                    font: field.font ?? .systemFont(ofSize: 13), tagsColumnVisible: tagsVisible) {
                    field.attributedStringValue = dotted
                }
                field.setAccessibilityElement(true)
                field.setAccessibilityLabel(SubjectCell.accessibilityLabel(
                    header: h, tags: model.tags, tagsColumnVisible: tagsVisible))
            case "preview":
                field.font = .systemFont(ofSize: 13)
                field.textColor = .secondaryLabelColor
                field.alignment = .left
                field.setAccessibilityElement(true)
                field.setAccessibilityLabel(String(localized: "Preview"))
            case "account":
                field.font = .systemFont(ofSize: 11)
                field.textColor = AccountColumn.colour(
                    forAccountId: h.accountId, accountCount: accountLabels.count)
                field.alignment = .left
                field.setAccessibilityElement(true)
                field.setAccessibilityLabel(String(localized: "Account"))
            case "thread":
                field.font = .systemFont(ofSize: 11)
                field.textColor = .secondaryLabelColor
                field.alignment = .center
                // Visual glyph stays "▾N"; VoiceOver gets a spoken count instead
                // of "down triangle N" and is skipped entirely for non-threads.
                field.setAccessibilityElement(h.threadCount > 1)
                field.setAccessibilityLabel(String(localized: "Thread"))
                field.setAccessibilityValue(String(h.threadCount))
            case "tags":
                field.attributedStringValue = TagsCell.attributedString(for: h, tags: model.tags)
                field.alignment = .left
                field.setAccessibilityElement(!h.keywords.isEmpty || h.threadMuted)
                field.setAccessibilityLabel(TagsCell.accessibilityLabel(for: h, tags: model.tags))
            case "attach":
                field.font = .systemFont(ofSize: 11)
                field.alignment = .center
                field.setAccessibilityElement(h.hasAttachments)
                field.setAccessibilityLabel(String(localized: "Attachments"))
            case "date":
                field.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
                field.textColor = .secondaryLabelColor
                field.alignment = .right
                field.setAccessibilityElement(true)
                field.setAccessibilityLabel(String(localized: "Received"))
            default:
                break
            }
            return field
        }

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !isReloading, let table else { return }
            let idxs = table.selectedRowIndexes
            model.selectedMessageIds = idxs.compactMap { $0 < rows.count ? rows[$0].id : nil }
            let primaryRow = table.selectedRow
            guard primaryRow >= 0, primaryRow < rows.count else {
                model.clearSelection()
                return
            }
            if idxs.count == 1 && !selectingForContextMenu {
                model.select(message: rows[primaryRow])          // reading pane + auto-mark
            } else {
                model.showDetail(for: rows[primaryRow])           // multi: show, don't auto-mark
            }
        }

        /// Ids of the currently selected rows (used to preserve selection across
        /// a reload). Reads the CURRENT `rows`, so call before reassigning them.
        func selectedRowIds() -> [String] {
            guard let table else { return [] }
            return table.selectedRowIndexes.compactMap { $0 < rows.count ? rows[$0].id : nil }
        }

        /// After a reload: re-select the surviving previously-selected rows. If the
        /// whole prior selection was REMOVED and this was a pure removal from the
        /// same list (archive/delete, not a mailbox switch or a sync that added
        /// messages), auto-advance to the next row and open it.
        func restoreOrAdvance(oldRows: [MessageHeader], selectedIds: [String], primaryIndex: Int) {
            guard let table else { return }
            let wanted = Set(selectedIds)
            let surviving = IndexSet(rows.enumerated().compactMap { wanted.contains($0.element.id) ? $0.offset : nil })
            if !surviving.isEmpty {
                table.selectRowIndexes(surviving, byExtendingSelection: false)
                if let first = surviving.first { table.scrollRowToVisible(first) }
                // `isReloading` suppresses the selection callback, so when the
                // reload dropped PART of a multi-selection the model would keep
                // acting (Delete, Archive, flag) on rows no longer in the list.
                // Shrink it to the survivors, deferred past this view update.
                let survivingIds = surviving.map { rows[$0].id }
                let previousIds = selectedIds.filter { id in rows.contains { $0.id == id } }
                if survivingIds.count != selectedIds.count || previousIds != survivingIds {
                    DispatchQueue.main.async { [weak self] in
                        guard let self, let table = self.table else { return }
                        let current = table.selectedRowIndexes.compactMap {
                            $0 < self.rows.count ? self.rows[$0] : nil
                        }
                        let currentIds = current.map(\.id)
                        guard currentIds != self.model.selectedMessageIds else { return }
                        self.model.selectedMessageIds = currentIds
                        guard let primary = current.first else { return }
                        if let open = self.model.selectedMessage?.id, currentIds.contains(open) { return }
                        if current.count > 1 {
                            self.model.showDetail(for: primary)
                        } else {
                            self.model.select(message: primary)
                        }
                    }
                }
                return
            }
            let oldIds = Set(oldRows.map(\.id))
            let newIds = Set(rows.map(\.id))
            let removalOnly = !selectedIds.isEmpty && !rows.isEmpty
                && newIds.isSubset(of: oldIds) && newIds.count < oldIds.count
            guard removalOnly, primaryIndex >= 0 else {
                table.deselectAll(nil)
                // `isReloading` suppresses `tableViewSelectionDidChange`, so
                // this deselect is invisible to the model unless we say so.
                // Search results are not a subset of the old rows, so a search
                // that drops the selected message lands here — and left the
                // model believing a message was still open, keeping its AI
                // summary alive with nowhere to show it.
                let departedIds = wanted
                DispatchQueue.main.async { [weak self] in
                    guard let self,
                          let selectedId = self.model.selectedMessage?.id,
                          departedIds.contains(selectedId) else { return }
                    self.model.clearSelection()
                }
                return
            }
            let next = min(primaryIndex, rows.count - 1)
            table.selectRowIndexes(IndexSet(integer: next), byExtendingSelection: false)
            table.scrollRowToVisible(next)
            // The reload's own selection callback is suppressed (isReloading), so
            // open the advanced message explicitly — deferred past this view update.
            let message = rows[next]
            DispatchQueue.main.async { [weak self] in self?.model.select(message: message) }
        }

        /// Applies a `model.messages` change to `table` (S64). Factored out
        /// of `updateNSView` so `MessageListScrollPerfTests` can drive it
        /// directly against an off-screen table. An APPEND (scrolled near
        /// the end, the next keyset page arrived) uses `insertRows` — no
        /// `reloadData`, so scroll position and selection are untouched.
        /// Everything else (mailbox switch, search, optimistic content
        /// change, sync reconcile) reloads and restores/advances selection
        /// by message id, exactly as before Task 4.
        func apply(model: AppModel, table: NSTableView) {
            if case .appended(let range) = model.lastMessagesChange, rows.count == range.lowerBound {
                rows = model.messages
                table.insertRows(at: IndexSet(integersIn: range), withAnimation: [])
                emitFirstListPerfSignalIfNeeded(model: model)
                return
            }
            // Preserve the selection BY MESSAGE ID; if the selected message was
            // removed (archive/delete), auto-advance to the next one
            // (Superhuman triage flow).
            let oldRows = rows
            // A rebuilt table (e.g. switching Widescreen/Classic re-creates the
            // list view) starts with nothing selected while the model still
            // holds the selection: fall back to the model's ids so the row
            // highlight and auto-advance survive the rebuild.
            var selectedIds = selectedRowIds()
            if selectedIds.isEmpty, oldRows.isEmpty { selectedIds = model.selectedMessageIds }
            // The model is the source of truth once it has been cleared (a mailbox switch calls
            // clearSelection): the table must not re-select a row that survives in the new
            // list, or it stays highlighted with an empty reading pane (#3448).
            if model.selectedMessage == nil, model.selectedMessageIds.isEmpty { selectedIds = [] }
            let primaryIndex = table.selectedRow
            rows = model.messages
            isReloading = true
            table.reloadData()
            restoreOrAdvance(oldRows: oldRows, selectedIds: selectedIds, primaryIndex: primaryIndex)
            isReloading = false
            emitFirstListPerfSignalIfNeeded(model: model)
        }

        /// S64 manual GUI launch probe (`scripts/perf/launch-probe.sh`,
        /// `TestHost.isPerfProbe`): the first time this coordinator applies a
        /// NON-EMPTY message list, print one `PERF gui_first_list
        /// measured=<ms>ms` line (measured from `CastellumApp.launchStart`,
        /// the earliest Swift code in the process) and an `OSSignposter`
        /// event of the same name (visible in Instruments), then terminate —
        /// the probe's whole job is this one number. A no-op outside the
        /// probe, so it costs nothing in production or under XCTest.
        /// `TestHost.isPerfProbe` is DEV/TEST-ONLY (see its doc comment) —
        /// this method must never fire, and the app must never terminate
        /// itself this way, in a shipped build.
        private func emitFirstListPerfSignalIfNeeded(model: AppModel) {
            guard TestHost.isPerfProbe, !didEmitFirstListPerfSignal, !model.messages.isEmpty else { return }
            didEmitFirstListPerfSignal = true
            let elapsed = ContinuousClock.now - CastellumApp.launchStart
            let ms = Double(elapsed.components.seconds) * 1000 + Double(elapsed.components.attoseconds) / 1e15
            Self.perfSignposter.emitEvent("FirstListRendered")
            print(String(format: "PERF gui_first_list measured=%.1fms", ms))
            NSApp.terminate(nil)
        }

        /// S64 infinite scroll: fires on every scroll/resize of the list's
        /// clip view. Cheap on purpose (an int compare inside the model call)
        /// since it runs on every frame of a drag-scroll, not just at rest.
        @objc func boundsDidChange(_ notification: Notification) {
            guard let table else { return }
            let visible = table.rows(in: table.visibleRect)
            guard visible.upperBound > 0 else { return }
            model.loadMoreMessagesIfNeeded(lastVisibleRow: visible.upperBound - 1)
        }

        /// Forces the Account column visible while a unified (cross-account)
        /// list is showing; restores it to hidden afterwards ONLY if this
        /// coordinator was the one that forced it (S34 follow-on).
        func syncAccountColumnVisibility(table: NSTableView, unified: Bool) {
            guard let col = table.tableColumns.first(where: { $0.identifier.rawValue == AccountColumn.columnId })
            else { return }
            if unified {
                guard col.isHidden else { return }
                col.isHidden = false
                accountColumnForcedVisible = true
                // Showing Account can push Received off-screen again (#3042).
                if let width = paneWidth(table) { relayout(table, available: width, mode: .shrink) }
            } else if accountColumnForcedVisible {
                col.isHidden = true
                accountColumnForcedVisible = false
            }
        }

        func openSelected() {
            guard let idx = table?.selectedRow, idx >= 0, idx < rows.count else { return }
            model.select(message: rows[idx])
            // DR1: Return on a draft opens it in the composer for editing.
            if model.openDraft(from: rows[idx]) { return }
            onOpen()
        }

        /// Double-click a row: open the message in its own window.
        @objc func doubleClicked() {
            guard let idx = table?.clickedRow, idx >= 0, idx < rows.count else { return }
            // DR1: a draft opens in the composer, not a read-only window.
            if model.openDraft(from: rows[idx]) { return }
            model.openStandaloneWindow(for: rows[idx])
        }

        // MARK: - Row context menu (registry-driven; context-menu spec slice 1)

        /// True while a right-click is reconciling the selection: the selection
        /// callback then shows the row (`showDetail`) without scheduling
        /// auto-mark-read, because a right-click is not "opening" the message.
        var selectingForContextMenu = false
        /// The entries of the menu last prepared (read by the UIT-1 harness).
        private(set) var lastRowMenu: [MessageMenuEntry] = []

        /// Entries for a right-click on `row`; `[]` for blank space or a stale row.
        /// Acts on the whole selection when `row` is inside it, otherwise the
        /// clicked row becomes the selection first. Makes the table first
        /// responder so `CommandDispatcher`'s text-editing guard cannot silently
        /// drop Archive / Delete when the search field had focus.
        func prepareRowMenu(forRow row: Int) -> [MessageMenuEntry] {
            guard let table, row >= 0, row < rows.count, row < table.numberOfRows else {
                lastRowMenu = []
                return []
            }
            if !table.selectedRowIndexes.contains(row) {
                selectingForContextMenu = true
                table.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
                selectingForContextMenu = false
            }
            table.window?.makeFirstResponder(table)
            lastRowMenu = MessageContextMenu.entries(for: model.messageMenuContext())
            return lastRowMenu
        }

        func menuNeedsUpdate(_ menu: NSMenu) {
            if menu === table?.headerView?.menu {
                syncHeaderMenuTicks(menu)
                return
            }
            guard menu === table?.menu else { return }
            menu.removeAllItems()
            Self.render(prepareRowMenu(forRow: table?.clickedRow ?? -1), into: menu, target: self)
        }

        private static func render(_ entries: [MessageMenuEntry], into menu: NSMenu, target: Coordinator) {
            menu.autoenablesItems = false
            for entry in entries {
                switch entry {
                case .separator:
                    menu.addItem(.separator())
                case .command(_, let title, let enabled, let reason):
                    let item = NSMenuItem(title: title, action: #selector(rowMenuItemClicked(_:)), keyEquivalent: "")
                    item.target = target
                    item.representedObject = entry
                    item.isEnabled = enabled
                    item.toolTip = reason
                    menu.addItem(item)
                case .moveTo(_, let title, let enabled, let reason):
                    let item = NSMenuItem(title: title, action: #selector(rowMenuItemClicked(_:)), keyEquivalent: "")
                    item.target = target
                    item.representedObject = entry
                    item.isEnabled = enabled
                    item.toolTip = reason
                    menu.addItem(item)
                case .enableWrites:
                    let item = NSMenuItem(title: entry.title ?? "", action: #selector(rowMenuItemClicked(_:)), keyEquivalent: "")
                    item.target = target
                    item.representedObject = entry
                    menu.addItem(item)
                case .submenu(let title, let inner):
                    let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
                    let sub = NSMenu(title: title)
                    render(inner, into: sub, target: target)
                    item.submenu = sub
                    menu.addItem(item)
                }
            }
        }

        @objc func rowMenuItemClicked(_ sender: NSMenuItem) {
            guard let entry = sender.representedObject as? MessageMenuEntry else { return }
            invokeRowMenuEntry(entry)
        }

        /// Runs a menu entry exactly as a click would: posts the command's own
        /// notification (the menu-bar / palette path), or opens the existing
        /// write-safety confirmation for "Enable Writes for ...".
        func invokeRowMenuEntry(_ entry: MessageMenuEntry) {
            switch entry {
            case .command(let id, _, let enabled, _):
                guard enabled, let name = CommandRegistry.spec(for: id).notification else { return }
                NotificationCenter.default.post(name: name, object: nil)
            case .moveTo(let mailboxId, _, let enabled, _):
                // The same guarded, undoable path as the Move to Folder… picker.
                guard enabled else { return }
                model.moveSelected(toMailboxId: mailboxId)
            case .enableWrites(_, let accountId):
                if let account = model.accounts.first(where: { $0.id == accountId }) {
                    model.requestWritePolicyChange(for: account)
                }
            case .submenu, .separator:
                break
            }
        }

        /// Next / Previous Message and Open Message arrive as notifications from
        /// the key-sequence router (j, k, Return by default), exactly like every
        /// other command, so each can be rebound in keybindings.json.
        @objc func selectNextRow() { moveSelection(by: 1) }
        @objc func selectPreviousRow() { moveSelection(by: -1) }
        @objc func openSelectedRow() { openSelected() }

        /// Move the selection by `delta` rows (clamped), keeping it visible.
        private func moveSelection(by delta: Int) {
            guard let table, !rows.isEmpty else { return }
            let current = table.selectedRow
            let next = current < 0
                ? (delta > 0 ? 0 : rows.count - 1)
                : max(0, min(rows.count - 1, current + delta))
            table.selectRowIndexes(IndexSet(integer: next), byExtendingSelection: false)
            table.scrollRowToVisible(next)
        }

        /// Auto-fit every column (the "Fit Columns" menu command), then shrink
        /// the elastic ones so the Received column stays inside the pane (#3042);
        /// if even their minimums overflow, hide Preview, Account, Thread, Tags in
        /// that order (never the Account column a unified list forces).
        /// `available` defaults to the live clip-view width.
        @objc func fitAllColumns() { fitColumns(available: nil) }

        func fitColumns(available: CGFloat?) {
            guard let table else { return }
            for i in 0..<table.tableColumns.count { autofit(columnIndex: i) }
            if let width = available ?? paneWidth(table) {
                let keep: Set<String> = accountColumnForcedVisible ? [AccountColumn.columnId] : []
                relayout(table, available: width, mode: .fit(keep: keep))
            }
        }

        // MARK: - #3042 pane-aware column layout

        /// True when a column layout was already saved before this launch
        /// (captured in `makeNSView` before AppKit's restore). Defaults to true
        /// so a coordinator built outside `makeNSView` never hides columns.
        var savedLayoutExisted = true
        /// Where the one-shot guard lives; tests inject an isolated suite.
        var layoutDefaults: UserDefaults = .standard
        /// True when selecting a message can squeeze the list to its minimum width:
        /// Widescreen's split view does that at every window size (measured: 500 → 420
        /// at the default window, 790 → 420 at 1800 pt). Classic stacks the panes, so
        /// its list keeps the window's width. Tests inject either answer.
        var listCanBeSqueezed: () -> Bool = { Preferences.shared.viewerLayout == .widescreen }
        private var didApplyFirstLayout = false

        /// Visible width of the list pane, or nil before the first layout pass.
        func paneWidth(_ table: NSTableView) -> CGFloat? {
            guard let w = table.enclosingScrollView?.contentView.bounds.width, w > 0 else { return nil }
            return w
        }

        private var paneWork: DispatchWorkItem?

        /// SwiftUI/split-view layout settles over several passes (the first non-zero
        /// clip width was ~30 pt wider than the final 500 pt pane), so every pane
        /// resize waits until the width has been quiet for a moment and uses the
        /// width it has THEN. The first one runs the one-shot layout; every later
        /// one (a window resize, or the split view squeezing the list from 500 to
        /// 420 pt when a message opens) runs `keepReceivedVisible` (#3042).
        @objc func paneFrameDidChange(_ notification: Notification) {
            guard let clip = notification.object as? NSClipView, clip.bounds.width > 0 else { return }
            paneWork?.cancel()
            let work = DispatchWorkItem { [weak self, weak clip] in
                guard let self, let clip, clip.bounds.width > 0 else { return }
                if self.didApplyFirstLayout {
                    self.keepReceivedVisible(available: clip.bounds.width)
                } else {
                    self.didApplyFirstLayout = true
                    self.applyFirstLayoutIfNeeded(available: clip.bounds.width)
                }
            }
            paneWork = work
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.2, execute: work)
        }

        /// After the first layout: when the pane has narrowed and Received's right edge
        /// is outside it, shrink the elastic columns back in. It never hides a column and
        /// never widens one, so a user's own layout is only touched when the dates would
        /// otherwise be cut off.
        func keepReceivedVisible(available: CGFloat) {
            guard let table, let i = table.tableColumns.firstIndex(where: { $0.identifier.rawValue == "date" }),
                  !table.tableColumns[i].isHidden,
                  table.rect(ofColumn: i).maxX > available + 0.5 else { return }
            relayout(table, available: available, mode: .shrink)
        }

        /// Runs once per launch from the first settled pane width. Known limit: a new
        /// user who launches at the default size and later widens the window keeps the
        /// compact first-run layout (Preview/Account/Thread/Tags hidden) until they
        /// use Fit Columns or re-show columns.
        /// - No saved layout: new-install `firstRunLayout` (may hide Preview, Account, Thread, Tags).
        /// - Saved layout, guard key unset, Received beyond the pane: `shrinkToFit`
        ///   only (spec M1; hides nothing). The key is set either way.
        /// Runs after `migrateAccountColumnIn`/`migrateTagsColumnIn` (which un-hide
        /// those columns on an unset key, including a fresh install) so it wins,
        /// and marks both migrations done so they cannot re-show Account next launch.
        func applyFirstLayoutIfNeeded(available: CGFloat) {
            guard let table else { return }
            if !savedLayoutExisted {
                relayout(table, available: available, mode: .firstRun)
                layoutDefaults.set(true, forKey: "CastellumMessageList.accountColumnAdded")
                layoutDefaults.set(true, forKey: "CastellumMessageList.tagsColumnAdded")
                layoutDefaults.set(true, forKey: MessageListView.dateFitAppliedKey)
                // A unified list force-shows Account; make Received fit with it.
                if model.isCrossAccountList, model.accounts.count > 1, accountColumnForcedVisible == false {
                    syncAccountColumnVisibility(table: table, unified: true)
                }
                return
            }
            guard !layoutDefaults.bool(forKey: MessageListView.dateFitAppliedKey) else { return }
            layoutDefaults.set(true, forKey: MessageListView.dateFitAppliedKey)
            if let i = table.tableColumns.firstIndex(where: { $0.identifier.rawValue == "date" }),
               !table.tableColumns[i].isHidden,
               table.rect(ofColumn: i).maxX > available {
                relayout(table, available: available, mode: .shrink)
            }
        }

        /// Which `MessageListColumnLayout` pass `relayout` runs.
        enum RelayoutMode {
            case firstRun                   // new install: shrink, then hide for the narrowest pane
            case shrink                     // M1, unified Account, pane resize: shrink only
            case fit(keep: Set<String>)     // Fit Columns: shrink, then hide as a last resort
        }

        /// Maps the table's columns through `MessageListColumnLayout` and writes
        /// widths / hidden state back (and the header menu ticks).
        func relayout(_ table: NSTableView, available: CGFloat, mode: RelayoutMode) {
            let cols = table.tableColumns.map {
                MessageListColumnLayout.Column(
                    id: $0.identifier.rawValue, width: $0.width,
                    min: $0.minWidth, max: $0.maxWidth, hidden: $0.isHidden)
            }
            let out: [MessageListColumnLayout.Column]
            switch mode {
            case .firstRun:
                out = MessageListColumnLayout.firstRunLayout(
                    cols, available: available,
                    narrowest: listCanBeSqueezed() ? MessageListColumnLayout.narrowestPane : available)
            case .shrink: out = MessageListColumnLayout.shrinkToFit(cols, available: available)
            case .fit(let keep): out = MessageListColumnLayout.fitLayout(cols, available: available, keep: keep)
            }
            var tagsVisibilityChanged = false
            for (col, new) in zip(table.tableColumns, out) {
                if col.isHidden != new.hidden {
                    col.isHidden = new.hidden
                    if col.identifier.rawValue == "tags" { tagsVisibilityChanged = true }
                }
                if abs(col.width - new.width) > 0.01 { col.width = new.width }
            }
            if let menu = table.headerView?.menu {
                for item in menu.items {
                    guard let col = item.representedObject as? NSTableColumn else { continue }
                    item.state = col.isHidden ? .off : .on
                }
            }
            if tagsVisibilityChanged { reloadSubjectCells(table) }
        }

        /// The Subject cell carries the tag dots while the Tags column is hidden
        /// (#2245), so it is redrawn whenever that column is shown or hidden.
        /// Reloads only that column's cells: selection and scroll stay put.
        func reloadSubjectCells(_ table: NSTableView) {
            guard let i = table.tableColumns.firstIndex(where: { $0.identifier.rawValue == "subject" }),
                  table.numberOfRows > 0 else { return }
            table.reloadData(forRowIndexes: IndexSet(integersIn: 0..<table.numberOfRows),
                             columnIndexes: IndexSet(integer: i))
        }

        /// Show/hide ticks follow the columns' REAL state, whatever last changed it (the
        /// Account column is forced visible in All Inboxes, which used to leave its tick off
        /// and make the next click hide a column the user still saw). The forced column
        /// cannot be toggled while it is forced (#3448).
        func syncHeaderMenuTicks(_ menu: NSMenu) {
            for item in menu.items {
                guard let col = item.representedObject as? NSTableColumn else { continue }
                item.state = col.isHidden ? .off : .on
                let forced = accountColumnForcedVisible && col.identifier.rawValue == AccountColumn.columnId
                item.isEnabled = !forced
            }
        }

        @objc func toggleColumn(_ sender: NSMenuItem) {
            guard let col = sender.representedObject as? NSTableColumn else { return }
            if accountColumnForcedVisible && col.identifier.rawValue == AccountColumn.columnId { return }
            col.isHidden.toggle()
            sender.state = col.isHidden ? .off : .on
            if col.identifier.rawValue == "tags", let table { reloadSubjectCells(table) }
        }

        /// Auto-fit a column to the widest content (clamped to its min/max).
        func autofit(columnIndex: Int) {
            guard let table, columnIndex >= 0, columnIndex < table.tableColumns.count else { return }
            let col = table.tableColumns[columnIndex]
            let font = NSFont.systemFont(ofSize: 13, weight: .semibold) // heaviest → safe overestimate
            func width(_ s: String, _ f: NSFont) -> CGFloat {
                (s as NSString).size(withAttributes: [.font: f]).width
            }
            var maxW = width(col.title, .systemFont(ofSize: 12, weight: .semibold))
            for h in rows { maxW = max(maxW, width(text(col.identifier.rawValue, h), font)) }
            col.width = min(max(maxW + 16, col.minWidth), col.maxWidth)
        }
    }
}

/// NSTableView that reports Return / Enter presses (so a row can be "opened").
/// Bare keys (j/k, e, ...) never reach it: the window-level key-sequence router
/// consumes them first (MM-12).
@MainActor
final class KeyTableView: NSTableView {
    var onReturn: (@MainActor () -> Void)?

    override func keyDown(with event: NSEvent) {
        if event.keyCode == 36 || event.keyCode == 76 { // Return / keypad Enter
            onReturn?()
            return
        }
        super.keyDown(with: event)
    }
}

/// Header that auto-fits a column when its trailing divider is double-clicked.
@MainActor
final class ColumnHeaderView: NSTableHeaderView {
    var autofit: (@MainActor (Int) -> Void)?
    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2, let tv = tableView {
            let p = convert(event.locationInWindow, from: nil)
            for i in 0..<tv.numberOfColumns {
                if abs(p.x - headerRect(ofColumn: i).maxX) <= 4 {
                    autofit?(i)
                    return
                }
            }
        }
        super.mouseDown(with: event)
    }
}
