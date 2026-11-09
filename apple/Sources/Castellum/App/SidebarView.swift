// Castellum - source list: All Inboxes ▸ accounts ▸ mailboxes.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// The sidebar's selectable identity: either a concrete per-account mailbox
/// or the unified (cross-account) group for a mailbox role (S34's
/// "All Inboxes"). A single `List(selection:)` binding needs one Hashable
/// type spanning both kinds of row.
enum SidebarSelection: Hashable {
    case mailbox(Mailbox)
    case unified(role: String)
    case smart(SmartMailbox)
    case snoozed
    /// S59: the unified inbox filtered by `is:vip` (a search, not a mailbox).
    case vips
    /// MM-8: every message carrying one defined tag.
    case tag(TagDefinition)
}

/// Sidebar badge text for one smart mailbox's cached unread count. `nil`
/// hides the badge (nothing unread); a capped count shows "<cap>+" rather
/// than an exact — but possibly stale-looking-precise — number. Pure so
/// it's unit-tested rather than eyeballed, mirroring `UnifiedInboxBadge`.
enum SmartMailboxBadge {
    static func label(unread: Int64, capped: Bool) -> String? {
        guard unread > 0 else { return nil }
        return capped ? "\(unread)+" : "\(unread)"
    }
}

/// Text for the sidebar Tags rows (MM-8). Pure so it is unit-tested.
enum TagSidebarBadge {
    /// `nil` hides the badge (nothing unread).
    static func label(unread: Int64) -> String? {
        unread > 0 ? "\(unread)" : nil
    }

    /// Row tooltip: the total, plus the provenance hint (T5) when some of the
    /// tagged mail carries the tag on this Mac only.
    static func help(total: Int64, localOnly: Int64) -> String {
        let all = String(localized: "\(total) messages")
        guard localOnly > 0 else { return all }
        return all + "\n" + String(localized: "\(localOnly) tagged on this Mac only")
    }

    /// VoiceOver text: "Work, tag, 3 unread".
    static func accessibilityLabel(name: String, unread: Int64) -> String {
        unread > 0
            ? String(localized: "\(name), tag, \(unread) unread")
            : String(localized: "\(name), tag")
    }
}

/// Unread-count aggregation for a unified sidebar group. Pure so the sum is
/// unit-tested rather than eyeballed — mirrors `AccountColumn`'s pure-helper
/// pattern in MessageListView.swift.
enum UnifiedInboxBadge {
    static func unreadCount(
        role: String, mailboxes: [String: [Mailbox]], counts: [String: MailboxCounts]
    ) -> Int {
        mailboxes.values
            .flatMap { $0 }
            .filter { $0.role == role }
            .reduce(0) { $0 + Int(counts[$1.id]?.unread ?? 0) }
    }
}

struct SidebarView: View {
    @ObservedObject var model: AppModel

    private func icon(_ role: String?) -> String {
        switch role {
        case "inbox": return "tray"
        case "sent": return "paperplane"
        case "drafts": return "doc"
        case "trash": return "trash"
        case "junk": return "xmark.bin"
        case "archive": return "archivebox"
        default: return "folder"
        }
    }

    private var hasAnyDraft: Bool {
        model.drafts.values.contains { !$0.isEmpty }
    }

    private var unifiedInboxUnread: Int {
        UnifiedInboxBadge.unreadCount(role: "inbox", mailboxes: model.mailboxes, counts: model.counts)
    }

    private var selection: Binding<SidebarSelection?> {
        Binding(
            get: {
                if let id = model.selectedSmartMailboxId,
                   let sm = model.smartMailboxes.first(where: { $0.id == id }) {
                    return .smart(sm)
                }
                if model.selectedIsSnoozed { return .snoozed }
                if let key = model.selectedTagKey,
                   let tag = model.tags.first(where: { $0.keywordKey == key }) {
                    return .tag(tag)
                }
                if model.isVipViewSelected { return .vips }
                if let role = model.selectedUnifiedRole { return .unified(role: role) }
                if let mb = model.selectedMailbox { return .mailbox(mb) }
                return nil
            },
            set: {
                // Leaving the VIPs view: drop its pseudo-query so the search field
                // and the sidebar highlight cannot disagree with the list.
                if $0 != .vips, model.searchText == "is:vip" { model.searchText = "" }
                switch $0 {
                case .mailbox(let mb): model.select(mailbox: mb)
                case .unified(let role): model.select(unifiedRole: role)
                case .smart(let sm): model.select(smartMailbox: sm)
                case .snoozed: model.select(snoozed: ())
                case .vips: model.selectVips()
                case .tag(let tag): model.select(tag: tag)
                case nil: break
                }
            }
        )
    }

    var body: some View {
        List(selection: selection) {
            // Only earns its place once there is more than one account to
            // unify — with one account it would be an exact duplicate of
            // that account's own Inbox row (mirrors AccountColumn's
            // single-account-is-not-tinted rule).
            if model.accounts.count > 1 {
                Section {
                    Label("All Inboxes", systemImage: "tray.2")
                        .badge(unifiedInboxUnread)
                        .tag(SidebarSelection.unified(role: "inbox"))
                }
            }
            // S59: VIPs, shown once any sender is marked (press v on a message).
            if !model.vips.isEmpty {
                Section {
                    Label("VIPs", systemImage: "person.crop.circle.badge.checkmark")
                        .badge(Int(model.vipUnreadCount))
                        .tag(SidebarSelection.vips)
                }
            }
            if !model.smartMailboxes.isEmpty {
                Section("Smart Mailboxes") {
                    ForEach(model.smartMailboxes) { sm in
                        SmartMailboxRow(smartMailbox: sm, model: model)
                            .tag(SidebarSelection.smart(sm))
                    }
                    .onMove { indices, newOffset in
                        model.moveSmartMailboxes(fromOffsets: indices, toOffset: newOffset)
                    }
                }
            }
            // MM-8: one row per defined tag, hidden when none is defined.
            if !model.tags.isEmpty {
                Section("Tags") {
                    ForEach(model.tags) { tag in
                        TagRow(tag: tag, count: model.tagCounts[tag.keywordKey])
                            .tag(SidebarSelection.tag(tag))
                    }
                }
            }
            // OB1: the Outbox, with its count, whenever a message is waiting.
            if model.outboxMessageCount > 0 {
                Section {
                    Button { model.openOutbox() } label: {
                        Label(String(localized: "Outbox"), systemImage: "tray.and.arrow.up")
                    }
                    .buttonStyle(.plain)
                    .badge(model.outboxMessageCount)
                    .help(String(localized: "Messages waiting to send — open the Outbox"))
                    .accessibilityIdentifier("sidebar.outbox")
                }
            }
            Section {
                Label("Snoozed", systemImage: "zzz")
                    .badge(model.snoozedMessages.count)
                    .tag(SidebarSelection.snoozed)
            }
            Section("Active Work") {
                Button { model.openProjectsBoard() } label: {
                    Label("Project Board", systemImage: "rectangle.3.group")
                }
                .buttonStyle(.plain)
                .badge(model.projects.filter { !$0.archived && $0.status != "done" }.count)
                .help("Open the Castellum project board (⇧⌘P)")
            }
            ForEach(model.accounts) { account in
                AccountSection(account: account, model: model, icon: icon)
            }
        }
        .listStyle(.sidebar)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            SyncStatusLines(model: model)
        }
        .overlay {
            if model.accounts.isEmpty && !hasAnyDraft {
                VStack(spacing: 6) {
                    Image(systemName: "envelope")
                        .font(.largeTitle)
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                    Text("No account").font(.headline)
                    Text("Use + to add one.").font(.caption).foregroundStyle(.secondary)
                }
                .accessibilityElement(children: .combine)
            }
        }
    }
}

/// The always-visible sync line, one per account, at the bottom of the
/// sidebar (#2832). The `TimelineView` only re-evaluates the text every
/// `redrawInterval` seconds so "Checked 8 s ago" stays honest; it never
/// triggers a sync. Click opens the Status dashboard.
private struct SyncStatusLines: View {
    @ObservedObject var model: AppModel

    var body: some View {
        if !model.accounts.isEmpty {
            TimelineView(.periodic(from: .now, by: SyncStatusLineBuilder.redrawInterval)) { context in
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(model.accounts) { account in
                        let line = model.syncStatusLine(for: account.id, now: context.date)
                        Button { model.openStatusDashboard() } label: {
                            HStack(spacing: 4) {
                                Image(systemName: symbol(line.kind))
                                    .font(.system(size: 7))
                                    .foregroundStyle(colour(line.kind))
                                    .accessibilityHidden(true)
                                if model.accounts.count > 1 {
                                    Text(account.sidebarLabel).fontWeight(.medium)
                                }
                                Text(line.text)
                                    .foregroundStyle(line.kind == .error ? Color.red : Color.secondary)
                                    .lineLimit(1)
                                    .truncationMode(.tail)
                                Spacer(minLength: 0)
                            }
                            .font(.caption2)
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain)
                        .help(String(localized: "\(account.email): \(line.text). Click to open Status."))
                        .accessibilityIdentifier("sidebar.syncLine.\(account.id)")
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.bar)
            }
        }
    }

    private func symbol(_ kind: SyncStatusLine.Kind) -> String {
        kind == .syncing ? "arrow.triangle.2.circlepath" : "circle.fill"
    }

    private func colour(_ kind: SyncStatusLine.Kind) -> Color {
        switch kind {
        case .live: return .green
        case .error: return .red
        case .syncing, .checked: return .secondary
        case .unknown: return .secondary.opacity(0.5)
        }
    }
}

/// One local draft in the sidebar. `draft` status reopens the compose window
/// for editing; `outbox` status is shown as queued (no longer editable).
private struct DraftRow: View {
    let draft: Draft
    @ObservedObject var model: AppModel

    private var row: DraftListRow { DraftListRow(draft: draft) }
    private var title: String { row.subject }

    /// Subject over recipient (DR1): the recipient is what tells two
    /// drafts apart, so it is always on the row.
    private func label(symbol: String) -> some View {
        Label {
            VStack(alignment: .leading, spacing: 1) {
                Text(title).lineLimit(1)
                Text(row.recipient)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        } icon: {
            Image(systemName: symbol)
        }
    }

    var body: some View {
        if draft.status == "draft" {
            Button {
                ComposeWindowController.show(model: model, session: ComposeSession(draft: draft))
            } label: {
                label(symbol: "square.and.pencil")
            }
            .buttonStyle(.plain)
            .help("Open draft")
        } else {
            label(symbol: "tray.and.arrow.up")
                .foregroundStyle(.secondary)
                .help("Queued in Outbox — sends on the next delivery pass")
        }
    }
}

/// One smart mailbox's sidebar row: name + unread badge, with Edit…/Delete
/// via context menu (drag-reorder is the section's own `.onMove`, above).
private struct SmartMailboxRow: View {
    let smartMailbox: SmartMailbox
    @ObservedObject var model: AppModel
    @State private var showEditor = false
    @State private var showDeleteConfirm = false

    var body: some View {
        Label(smartMailbox.name, systemImage: smartMailbox.symbol ?? "line.3.horizontal.decrease.circle")
            .badge(SmartMailboxBadge.label(unread: smartMailbox.cachedUnread, capped: smartMailbox.countsCapped)
                .map(Text.init))
            .contextMenu {
                Button("Edit…") { showEditor = true }
                Button("Delete", role: .destructive) { showDeleteConfirm = true }
            }
            .sheet(isPresented: $showEditor) {
                SmartMailboxEditorSheet(model: model, existing: smartMailbox)
            }
            .alert("Delete \u{201c}\(smartMailbox.name)\u{201d}?", isPresented: $showDeleteConfirm) {
                Button("Delete", role: .destructive) { model.deleteSmartMailbox(smartMailbox.id) }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("This only removes the saved search from the sidebar \u{2014} it does not delete any mail.")
            }
    }
}

/// One tag's sidebar row: colour dot, name, unread badge (MM-8).
private struct TagRow: View {
    let tag: TagDefinition
    let count: TagCount?

    var body: some View {
        let unread = count?.unread ?? 0
        Label {
            Text(tag.displayName).lineLimit(1).truncationMode(.tail)
        } icon: {
            Image(nsImage: TagColourDot.image(TagColour.named(tag.colour).nsColor, diameter: 10))
                .accessibilityHidden(true)
        }
        .badge(TagSidebarBadge.label(unread: unread).map(Text.init))
        .help(TagSidebarBadge.help(total: count?.total ?? 0, localOnly: count?.localOnly ?? 0))
        .accessibilityElement(children: .combine)
        .accessibilityLabel(TagSidebarBadge.accessibilityLabel(name: tag.displayName, unread: unread))
        .accessibilityIdentifier("sidebar.tag.\(tag.keywordKey)")
    }
}

/// One mailbox row. `.help` is applied only when there is a tooltip, so rows
/// with nothing to say do not carry an empty one (#3631).
private struct MailboxRow: View {
    let mailbox: Mailbox
    @ObservedObject var model: AppModel
    let icon: (String?) -> String

    var body: some View {
        let row = Label(mailbox.name, systemImage: icon(mailbox.role))
            .badge(Int(model.counts[mailbox.id]?.unread ?? 0))
        if let tip = BodyHydrationPresentation.tooltip(model.hydrationProgress[mailbox.id]) {
            row.help(tip)
        } else {
            row
        }
    }
}

/// One account's sidebar block: its mailboxes under a header carrying the
/// write-safety state, plus any local compose drafts. Extracted from
/// `SidebarView.body` so the `List` builder stays inside the type-checker's
/// budget (inlining it timed the compiler out).
private struct AccountSection: View {
    let account: Account
    @ObservedObject var model: AppModel
    let icon: (String?) -> String

    private var localDrafts: [Draft] {
        (model.drafts[account.id] ?? []).filter { $0.status != "sent" }
    }

    var body: some View {
        Section {
            ForEach(model.mailboxes[account.id] ?? []) { mb in
                MailboxRow(mailbox: mb, model: model, icon: icon)
                    .tag(SidebarSelection.mailbox(mb))
            }
        } header: {
            AccountHeader(account: account, model: model)
        }
        // Local compose drafts (not synced to IMAP). Editable drafts reopen in
        // a compose window; queued ones are shown read-only.
        if !localDrafts.isEmpty {
            Section("Local Drafts") {
                ForEach(localDrafts) { draft in
                    DraftRow(draft: draft, model: model)
                }
            }
        }
    }
}

/// Sidebar section header for one account: the address plus its write-safety
/// state. An account in evaluation mode is labelled READ-ONLY right where the
/// user looks for it, and the badge is the only way to leave that mode.
///
/// The confirmation itself is presented by `WriteSafetyPresenter` from
/// `AppModel.writeSafetyPrompt`: this header lives inside a `List` section,
/// whose backing view AppKit recycles on every reload, so a modal owned here
/// could be dismissed or never shown — unacceptable for the one checkpoint
/// before an account starts changing the real mail server.
private struct AccountHeader: View {
    let account: Account
    @ObservedObject var model: AppModel

    private var state: AccountWriteState { model.writeState(for: account.id) }
    private var syncStatus: AccountSyncStatus? { model.accountSyncStatuses[account.id] }

    var body: some View {
        HStack(spacing: 6) {
            Text(account.sidebarLabel)
                .help(account.email)
            Spacer(minLength: 4)
            if let syncStatus {
                // Click both retries this account immediately AND opens the
                // Status dashboard — one affordance for "what's wrong" and
                // "fix it now".
                Button {
                    model.retrySync(accountId: account.id)
                    model.openStatusDashboard()
                } label: {
                    Image(systemName: syncStatusSymbol(syncStatus.state))
                        .font(.caption2)
                        .foregroundStyle(syncStatusColour(syncStatus.state))
                }
                .buttonStyle(.plain)
                .help(syncStatusHelp(syncStatus))
                .accessibilityLabel(String(localized: "\(account.email): \(syncStatus.state.label). Retry and open Status."))
            }
            Button { model.requestWritePolicyChange(for: account) } label: {
                if state.isReadOnly {
                    Label(AccountWritePolicy.badgeLabel, systemImage: "lock")
                        .labelStyle(.titleAndIcon)
                        .font(.caption2)
                } else {
                    Image(systemName: "lock.open")
                        .font(.caption2)
                }
            }
            .buttonStyle(.plain)
            .foregroundStyle(state.isReadOnly ? Color.secondary : Color.accentColor)
            .disabled(model.isChangingWritePolicy)
            .help(state.isReadOnly
                  ? AccountWritePolicy.badgeHelp(for: account.email)
                  : AccountWritePolicy.writesEnabledHelp)
            .accessibilityLabel(
                state.isReadOnly
                    ? String(localized: "\(account.email): read-only, evaluation mode")
                    : String(localized: "\(account.email): writes enabled")
            )
        }
    }

    /// The status dot's tooltip: the existing status message, plus a live
    /// progress line while syncing so hovering during a long
    /// first sync gives more than "syncing" with no sense of scale.
    private func syncStatusHelp(_ status: AccountSyncStatus) -> String {
        guard status.state == .syncing, let progress = model.accountSyncProgress[status.accountId] else {
            return "\(status.message) Click to retry and open Status."
        }
        return "\(status.message) \(SyncProgressPresentation.label(for: progress)). Click to retry and open Status."
    }

    private func syncStatusSymbol(_ state: AccountSyncState) -> String {
        switch state {
        case .syncing: return "arrow.triangle.2.circlepath"
        case .ok: return "checkmark.circle.fill"
        default: return "exclamationmark.circle.fill"
        }
    }

    private func syncStatusColour(_ state: AccountSyncState) -> Color {
        switch state {
        case .syncing: return .secondary
        case .ok: return .green
        default: return .orange
        }
    }
}
