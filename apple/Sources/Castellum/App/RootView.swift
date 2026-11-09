// Castellum - read shell: sidebar + (message list ↔ reading pane).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI
import UniformTypeIdentifiers

struct RootView: View {
    private static let updateChecker = UpdateChecker()

    // Owned by CastellumApp (shared with the Settings scene) — see
    // CastellumApp.swift for why.
    @ObservedObject var model: AppModel
    @State private var showSetup = false
    @State private var showSaveSearch = false
    @State private var showReading = true
    @State private var showEmptyTrashConfirm = false
    @State private var showShortcuts = false
    @State private var showPalette = false
    @State private var showAssignProject = false
    @State private var showMoveToFolder = false
    @State private var hostWindow: NSWindow?
    @State private var showSnoozePicker = false
    @State private var showTagPicker = false
    @State private var updatePresentation: UpdatePresentation?
    @State private var isCheckingForUpdates = false
    @FocusState private var readingFocused: Bool
    @ObservedObject private var prefs = Preferences.shared

    var body: some View {
        NavigationSplitView {
            SidebarView(model: model)
                // Capped so widening the sidebar cannot demand a window wider
                // than any supported screen; an unbounded drag made SwiftUI
                // grow the window past the screen edge, which the screen
                // clamp then fought (see ClampFightGuard). 420 is wide enough
                // for long mailbox names without eating the message list.
                .navigationSplitViewColumnWidth(min: 180, ideal: 220, max: 420)
        } detail: {
            // MM-4: widescreen = list beside the message (original), classic =
            // list above it. Same two panes either way; only the divider axis
            // and minimum sizes differ.
            VStack(spacing: 0) {
                // F3: the notice banners live INSIDE the detail column, in the
                // layout flow above the list. As a window-level
                // `.safeAreaInset` on the NavigationSplitView they were drawn
                // over the list's column header (the split view's columns
                // ignore the inset), with the header labels showing through.
                topBanner
                if let context = model.correspondence {
                    CorrespondenceBar(
                        context: context, stats: model.correspondenceStats,
                        onClose: { model.closeCorrespondence() })
                    Divider()
                }
                if prefs.viewerLayout == .widescreen {
                    // Window minimum, measured by the UI harness: the sidebar
                    // column (228 pt as laid out) is counted TWICE, once as its
                    // column and once as the detail column's leading inset,
                    // plus the two pane minimums. 480 + 360 made it
                    // 228 + 228 + 841 = 1297 pt, wider than a 1280-pt display
                    // (FP03). 420 + 360 gives about 1237. Classic stacks the
                    // panes, so its minimum is 228 + 228 + 480 = 936.
                    HSplitView {
                        listPane.frame(minWidth: 420)
                        if showReading { readingPane.frame(minWidth: 360) }
                    }
                } else {
                    VSplitView {
                        listPane.frame(minWidth: 480, minHeight: 160)
                        if showReading { readingPane.frame(minWidth: 480, minHeight: 200) }
                    }
                }
            }
        }
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                SearchFieldView(text: $model.searchText, onChangeApply: { model.runSearch() })
                    .frame(width: 220)
                // MM-2: where a typed search looks.
                Menu {
                    Picker("Search in", selection: $model.searchScope) {
                        ForEach(SearchScope.allCases) { scope in
                            Label(scope.label, systemImage: scope.symbol).tag(scope)
                        }
                    }
                    .pickerStyle(.inline)
                } label: {
                    Image(systemName: model.searchScope.symbol)
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
                .help("Search in: \(model.searchScope.label)")
                .accessibilityLabel("Search scope: \(model.searchScope.label)")
                Button { showSaveSearch = true } label: { Image(systemName: "plus.magnifyingglass") }
                    .help("Save this search as a smart mailbox")
                    .accessibilityLabel("Save this search as a smart mailbox")
                    .disabled(model.searchText.trimmingCharacters(in: .whitespaces).isEmpty)
                Button { showReading.toggle() } label: { Image(systemName: "sidebar.right") }
                    .help(Text(String(localized: showReading ? "Hide the reading pane" : "Show the reading pane")))
                    .accessibilityLabel(String(localized: showReading ? "Hide the reading pane" : "Show the reading pane"))
                Button { openCompose(mode: nil) } label: { Image(systemName: "square.and.pencil") }
                    .help("New message (⌘N)")
                    .accessibilityLabel("New message")
                Button { openCompose(mode: .reply) } label: {
                    Image(systemName: "arrowshape.turn.up.left")
                }
                    .help("Reply (⌘R)")
                    .accessibilityLabel("Reply")
                    .disabled(!messageBodyActionsAvailable(model.detail))
                Button { openCompose(mode: .replyAll) } label: {
                    Image(systemName: "arrowshape.turn.up.left.2")
                }
                    .help("Reply All (⇧⌘R)")
                    .accessibilityLabel("Reply All")
                    .disabled(!messageBodyActionsAvailable(model.detail))
                Button { openCompose(mode: .forward) } label: {
                    Image(systemName: "arrowshape.turn.up.right")
                }
                    .help("Forward (⇧⌘F)")
                    .accessibilityLabel("Forward")
                    .disabled(!messageBodyActionsAvailable(model.detail))
                // S36: a message sitting in a Drafts-role mailbox that has no
                // local compose-draft row yet (composed from another
                // client, or synced before this build existed) — hydrate one
                // via `castellum_open_server_draft` and open it for editing.
                if model.selectedMailbox?.role == "drafts" {
                    Button { editServerDraft() } label: { Image(systemName: "pencil") }
                        .help("Edit Draft")
                        .accessibilityLabel("Edit Draft")
                        .disabled(!messageBodyActionsAvailable(model.detail)
                                  || model.detail?.rfc822MessageId == nil)
                }
                Button { model.archiveSelected() } label: { Image(systemName: "archivebox") }
                    .help("Archive (⌘E)")
                    .accessibilityLabel("Archive")
                    .disabled(model.selectedMessage == nil)
                Button { model.deleteSelected() } label: { Image(systemName: "trash") }
                    .help("Delete to Trash (⌘⌫)")
                    .accessibilityLabel("Delete to Trash")
                    .disabled(model.selectedMessage == nil)
                Button { showSetup = true } label: { Image(systemName: "plus") }
                    .help("Add account")
                    .accessibilityLabel("Add account")
                OutboxIndicator(model: model)
                if model.isSyncing { ProgressView().controlSize(.small) }
            }
        }
        .sheet(isPresented: $showSetup) { AccountSetupSheet(model: model) }
        .sheet(isPresented: $showSaveSearch) {
            SmartMailboxEditorSheet(model: model, existing: nil, prefilledQuery: model.searchText)
        }
        #if DEBUG
        // UIT-1: lets the UI-script runner present these sheets through their real
        // @State (see App/UIHarness/UIScriptHook.swift). Absent from Release.
        .uiScriptHook("addAccount") { _ in showSetup = true }
        .uiScriptHook("saveSearch") { _ in showSaveSearch = true }
        #endif
        .modifier(menuNotificationHandlers)
        // Window-level bare keys (e/#/u/f/c/r/R/p/?, g-chords): resolved from
        // the CommandRegistry and posted like menu chords, so they work
        // anywhere in this window while no text editor has focus — and
        // destructive ones stay behind the CommandDispatcher guard above.
        .background(BareKeyMonitor(model: model))
        .background(WindowAccessor(window: $hostWindow))
        .onChange(of: hostWindow) { model.mainWindowForUndo = $0 }   // #3022: where Cmd-Z undo registers
        // S25 undo-send: a non-modal banner while a delayed send's undo
        // window is open. Bottom, like a toast — never blocks the window.
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let pending = model.undoSendBanner.pending {
                UndoSendBannerView(
                    pending: pending,
                    onUndo: { model.undoBannerSend() },
                    onDismiss: { model.undoSendBanner.dismiss() })
            } else if let failed = model.sendFailedNotice {
                SendFailedNoticeView(
                    message: failed,
                    onOpenOutbox: { model.sendFailedNotice = nil; model.openOutbox() },
                    onDismiss: { model.sendFailedNotice = nil })
            } else if let notice = model.undoTooLateNotice {
                // Undo lost the race — the send already went out. Never shown
                // together with the banner above (it's dismissed first).
                UndoTooLateNoticeView(
                    message: notice,
                    onDismiss: { model.undoTooLateNotice = nil })
            } else if let shown = model.removalUndoBanner.shown {
                // #3022: lowest priority, so it never overlaps the undo-send banner.
                RemovalUndoBannerView(
                    shown: shown,
                    onUndo: { model.undoRemovalFromBanner() },
                    onDismiss: { model.removalUndoBanner.dismiss() })
            }
        }
        .sheet(isPresented: $showShortcuts) {
            ShortcutsView(groups: CommandRegistry.sheetGroups(from: model.keyBindings.effectiveSpecs))
        }
        .sheet(isPresented: $showPalette) {
            CommandPaletteView(model: model, onDismiss: { showPalette = false })
        }
        .sheet(item: $updatePresentation) { presentation in
            UpdateCheckSheet(presentation: presentation)
        }
        .sheet(item: $model.unsubscribeCandidate) { candidate in
            UnsubscribeConfirmSheet(detail: candidate, model: model)
        }
        // S53-UI: "Find similar" — a distinct sheet, never the main list or
        // the live search field. `findSimilarAnchor` drives presentation
        // (non-nil = shown); it is set the instant the gesture fires and
        // cleared on dismiss (see AppModel.findSimilar/dismissFindSimilar).
        .sheet(item: $model.findSimilarAnchor) { anchor in
            FindSimilarResultsSheet(model: model, anchor: anchor)
        }
        .popover(isPresented: $showAssignProject) {
            AssignToProjectPopover(
                model: model,
                messageIds: model.currentSelectionIds(),
                onDismiss: { showAssignProject = false }
            )
        }
        .popover(isPresented: $showMoveToFolder) {
            MoveToFolderPicker(model: model, onDismiss: { showMoveToFolder = false })
        }
        .popover(isPresented: $showSnoozePicker) {
            SnoozePickerPopover(
                model: model,
                messageIds: model.currentSelectionIds(),
                onDismiss: { showSnoozePicker = false }
            )
        }
        .popover(isPresented: $showTagPicker) {
            TagPickerView(model: model, onDismiss: { showTagPicker = false })
        }
        .popover(isPresented: $model.goToTagPickerShown) {
            TagPickerView(model: model, mode: .goTo, onDismiss: { model.goToTagPickerShown = false })
        }
        .confirmationDialog(
            model.emptyTrashConfirmationTitle,
            isPresented: $showEmptyTrashConfirm, titleVisibility: .visible
        ) {
            Button("Empty Trash", role: .destructive) { model.emptyTrash() }
            Button("Cancel", role: .cancel) {}
        }
        // Delete pressed inside Trash: permanent, so it needs an explicit
        // confirmation that says it cannot be undone (S73 / #2297).
        .confirmationDialog(
            model.pendingPermanentDelete?.title ?? "",
            isPresented: Binding(
                get: { model.pendingPermanentDelete != nil },
                set: { if !$0 { model.cancelPermanentDelete() } }),
            titleVisibility: .visible
        ) {
            Button("Delete Permanently", role: .destructive) { model.confirmPermanentDelete() }
            Button("Cancel", role: .cancel) { model.cancelPermanentDelete() }
        } message: {
            Text(PermanentDeleteRequest.message)
        }
        // No launch-time sync, probes or network under XCTest (see TestHost).
        .task { if !TestHost.isActive { model.bootstrap() } }
        .task { if !TestHost.suppressesLaunchSideEffects { await runAutomaticUpdateCheckAfterLaunch() } }
        // Keep the main window inside the screen: zoom + sidebar/reading-pane
        // toggles can grow the frame past visibleFrame.
        .background(WindowScreenClampAccessor())
        .alert("Error", isPresented: Binding(
            get: { model.errorText != nil },
            set: { if !$0 { model.errorText = nil } })
        ) {
            Button("OK", role: .cancel) { model.errorText = nil }
        } message: {
            Text(model.errorText ?? "")
        }
        .alert("Unsubscribe", isPresented: Binding(
            get: { model.unsubscribeStatusText != nil },
            set: { if !$0 { model.unsubscribeStatusText = nil } })
        ) {
            Button("OK", role: .cancel) { model.unsubscribeStatusText = nil }
        } message: {
            Text(model.unsubscribeStatusText ?? "")
        }
        // The write-safety dialogs (refusal notice, enable/disable
        // confirmations) are NOT presented from this view: they belong to
        // `WriteSafetyPresenter`, which shows them on the window the user
        // acted in. They used to be a legacy alert-with-item modifier right
        // here, and the one below silently replaced it.
        // Keep at most ONE such modifier on this view.
        //
        // Compose couldn't open (no account, or one just added still on its
        // first sync) — actionable two-button alert, not a dead-end OK
        // (Marc hit this repeatedly pilot-testing a large mailbox's first
        // sync; see `AppModel.composeAccountIdOrPromptUnavailable`).
        .alert(item: Binding(
            get: { model.composeUnavailablePrompt },
            set: { model.composeUnavailablePrompt = $0 })
        ) { prompt in
            composeUnavailableAlert(prompt)
        }
    }

    /// The alert for one `ComposeUnavailablePrompt`.
    private func composeUnavailableAlert(_ prompt: ComposeUnavailablePrompt) -> Alert {
        switch prompt {
        case .noAccount:
            return Alert(
                title: Text("Can't Compose"),
                message: Text("Add an account before composing."),
                primaryButton: .default(Text("Open Accounts…")) {
                    model.openAccountsSettings(accountId: nil)
                },
                secondaryButton: .cancel())
        case .firstSyncInProgress(let accountId):
            return Alert(
                title: Text("Can't Compose Yet"),
                message: Text("The account you just added is still completing its first sync. Compose will be available as soon as it finishes — check progress in Settings → Accounts."),
                primaryButton: .default(Text("Open Accounts…")) {
                    model.openAccountsSettings(accountId: accountId)
                },
                secondaryButton: .cancel())
        }
    }

    /// The menu/chord notification wiring, extracted so the `body` modifier
    /// chain stays within the type-checker's budget (it timed out inline).
    /// Every action goes through `CommandDispatcher`, which blocks
    /// registry-marked destructive commands while a text field/editor owns
    /// focus (⌘⌫ while typing in compose must not delete the selection).
    /// The single non-modal notice slot above the list (sync failure, smart
    /// mailbox notice; first match wins).
    @ViewBuilder private var topBanner: some View {
        if let status = model.syncBanner, status.state.isFailure {
            SyncFailureBanner(
                status: status, onDismiss: model.dismissSyncBanner,
                onRetry: { model.retrySync(accountId: status.accountId) })
        } else if let notice = model.smartMailboxNotice {
            SmartMailboxNoticeBanner(message: notice, onDismiss: { model.smartMailboxNotice = nil })
        }
    }

    private var listPane: some View {
        VStack(spacing: 0) {
            // HP-2: Split Inbox tabs sit directly above the list they switch.
            if model.isInboxSplitStripVisible {
                InboxSplitTabStrip(model: model)
                Divider()
            }
            MessageListView(model: model, onOpen: openSelectedMessage)
                .overlay {
                    if let text = MessageListEmptyState.text(
                        messageCount: model.messages.count, searchText: model.searchText,
                        isSmartMailbox: model.selectedSmartMailboxId != nil,
                        hasSelection: model.selectedMailbox != nil || model.isUnifiedList
                            || model.selectedSmartMailboxId != nil || model.selectedIsSnoozed
                            || model.correspondence != nil,
                        isSyncing: model.isSyncing,
                        mailboxNotSynced: model.selectedMailboxIsNotSynced) {
                        Text(text)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                            .padding()
                            .allowsHitTesting(false)
                            .accessibilityIdentifier("messageList.emptyState")
                    }
                }
            if !model.searchText.isEmpty,
               let note = BodyHydrationPresentation.searchNote(model.searchBodyCoverage) {
                Divider()
                Text(note)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 5)
                    .accessibilityIdentifier("messageList.searchBodyCoverageNote")
            }
        }
    }

    private var readingPane: some View {
        ReadingPaneView(detail: model.detail, model: model,
                        aiCoordinator: model.aiCoordinator,
                        isHydratingBody: model.isHydratingSelectedBody,
                        bodyHydrationError: model.selectedBodyHydrationError,
                        onLoadBody: { model.retryBodyHydrationForSelection() },
                        inlineImages: { model.inlineImageSource(for: $0) })
            .focusable()
            .focused($readingFocused)
    }

    private var menuNotificationHandlers: NotificationHandlers {
        let dispatch = CommandDispatcher()
        func route(
            _ name: Notification.Name, _ action: @escaping () -> Void
        ) -> (Notification.Name, () -> Void) {
            (name, { dispatch.perform(name, action: action) })
        }
        return NotificationHandlers(
            handlers: [
                route(.castellumToggleRead, { model.toggleReadUnread() }),
                route(.castellumMarkRead, { model.setReadStateOnSelection(isRead: true) }),
                route(.castellumMarkUnread, { model.setReadStateOnSelection(isRead: false) }),
                route(.castellumSnoozeLaterToday, { model.snoozeSelected(preset: .laterToday) }),
                route(.castellumSnoozeTomorrow, { model.snoozeSelected(preset: .tomorrow) }),
                route(.castellumSnoozeThisWeekend, { model.snoozeSelected(preset: .thisWeekend) }),
                route(.castellumSnoozeNextWeek, { model.snoozeSelected(preset: .nextWeek) }),
                route(.castellumOpenInWindow, {
                    if let m = model.selectedMessage { model.openStandaloneWindow(for: m) }
                }),
                route(.castellumFindSimilar, {
                    if let anchor = model.selectedMessage { model.findSimilar(anchor: anchor) }
                }),
                route(.castellumMarkAllRead, { model.markAllReadInCurrentMailbox() }),
                route(.castellumArchive, { model.archiveSelected() }),
                route(.castellumUndoRemoval, { model.undoLastRemoval() }),
                route(.castellumDelete, { model.deleteSelected() }),
                route(.castellumToggleFlag, { model.toggleFlagSelected() }),
                route(.castellumTagMessage, { showTagPicker = true }),
                // MM-12: the `g i`... mailbox jumps are ordinary commands; each selects its role.
                route(.castellumGotoInbox, { model.selectMailbox(role: "inbox") }),
                route(.castellumGotoArchive, { model.selectMailbox(role: "archive") }),
                route(.castellumGotoSent, { model.selectMailbox(role: "sent") }),
                route(.castellumGotoTrash, { model.selectMailbox(role: "trash") }),
                route(.castellumGotoDrafts, { model.selectMailbox(role: "drafts") }),
                route(.castellumGotoJunk, { model.selectMailbox(role: "junk") }),
                route(.castellumGotoTag, { if !model.tags.isEmpty { model.goToTagPickerShown = true } }),
                route(.castellumToggleMuteThread, { model.toggleMuteSelectedThread() }),
                route(.castellumToggleVipSender, { model.toggleVipForSelectedSender() }),
                // Confirm only when there IS a Trash to erase; otherwise let
                // emptyTrash() explain why nothing can happen (FP22).
                route(.castellumEmptyTrash, {
                    if model.emptyTrashAccountId != nil {
                        showEmptyTrashConfirm = true
                    } else {
                        model.emptyTrash()
                    }
                }),
                route(.castellumLayoutWidescreen, { prefs.viewerLayout = .widescreen }),
                route(.castellumHeaderCompact, { prefs.headerMode = .compact }),
                route(.castellumHeaderExtended, { prefs.headerMode = .extended }),
                route(.castellumHeaderAll, { prefs.headerMode = .all }),
                route(.castellumToggleRemoteImages, { prefs.alwaysLoadRemoteImages.toggle() }),
                route(.castellumExpandAllQuotes, { model.expandAllQuotes() }),
                route(.castellumLayoutClassic, { prefs.viewerLayout = .classic }),
                route(.castellumNextSplit, { model.selectAdjacentInboxSplit(offset: 1) }),
                route(.castellumPreviousSplit, { model.selectAdjacentInboxSplit(offset: -1) }),
                route(.castellumShowCorrespondence, {
                    if let d = model.detail,
                       let party = Correspondence.otherParty(of: d, myAddresses: model.myAddresses) {
                        model.showCorrespondence(address: party.address, name: party.name)
                    }
                }),
                // The two sheets are mutually exclusive: a request while the
                // other is up is dropped, never queued to pop up when that
                // sheet is dismissed (FP22).
                route(.castellumShowShortcuts, {
                    guard !showPalette else { return }
                    showShortcuts = true
                }),
                route(.castellumShowCommandPalette, {
                    guard !showShortcuts else { return }
                    showPalette = true
                }),
                // New message, Contacts, Projects, Diagnostics, Outbox, Status
                // and AI Activity are routed by AppCommandRouter instead: they
                // must keep working after this window closes (FP44).
                route(.castellumReply, { openCompose(mode: .reply) }),
                route(.castellumReplyAll, { openCompose(mode: .replyAll) }),
                route(.castellumForward, { openCompose(mode: .forward) }),
                route(.castellumReplyWithAi, { openComposeReplyWithAi() }),
                route(.castellumAssignProject, { showAssignProject = true }),
                route(.castellumMoveToFolder, { showMoveToFolder = true }),
                route(.castellumMarkJunk, { model.setJunkOnSelection(true) }),
                route(.castellumMarkNotJunk, { model.setJunkOnSelection(false) }),
                route(.castellumShowSnippets, { SnippetEditorWindowController.show(prefs: prefs, model: model) }),
                route(.castellumSnooze, { showSnoozePicker = true }),
                route(.castellumUnsnooze, { model.unsnoozeSelected() }),
                route(.castellumSummarize, { model.summarizeSelectedMessage() }),
                route(.castellumViewSource, { viewMessageSource() }),
                route(.castellumExportEml, { exportSelectedAsEml() }),
                route(.castellumImportEml, { presentImportPanel() }),
                route(.castellumPrintMessage, { printSelectedMessage() }),
                route(.castellumCheckForUpdates, {
                    Task { await runUpdateCheck(manual: true) }
                }),
            ] + (1...9).map { n in route(.castellumTagSlot(n), { model.toggleTagSlot(n) }) }
        )
    }

    @MainActor
    private func runAutomaticUpdateCheckAfterLaunch() async {
        try? await Task.sleep(nanoseconds: 10_000_000_000)
        guard !Task.isCancelled else { return }
        let prefs = Preferences.shared
        guard Self.updateChecker.shouldPerformAutomaticCheck(
            schedule: prefs.updateCheckSchedule,
            lastCheck: prefs.lastUpdateCheck
        ) else { return }
        await runUpdateCheck(manual: false)
    }

    @MainActor
    private func runUpdateCheck(manual: Bool) async {
        guard !isCheckingForUpdates else { return }
        isCheckingForUpdates = true
        defer { isCheckingForUpdates = false }

        do {
            // lastUpdateCheck is stamped only after a successful check, so an
            // offline launch is retried rather than skipped for 24 hours.
            let result = try await Self.updateChecker.checkRecordingSuccess(
                currentVersion: UpdateChecker.currentAppVersion(),
                record: { Preferences.shared.lastUpdateCheck = $0 }
            )
            switch result {
            case .updateAvailable(let version):
                updatePresentation = .available(version: version)
            case .upToDate(let currentVersion):
                if manual {
                    updatePresentation = .upToDate(currentVersion: currentVersion)
                }
            }
        } catch {
            // Daily checks are deliberately quiet; a person who explicitly
            // chose the menu command gets a small, actionable result instead.
            if manual { updatePresentation = .failed }
        }
    }

    /// Return in the list: make sure the reading pane is shown and focus it.
    private func openSelectedMessage() {
        showReading = true
        readingFocused = true
    }

    /// Fetch and show the selected message's full RFC822 source — always a
    /// fresh IMAP round trip unless a prior view already cached the file
    /// (see this stage's Assumption 1: no raw source survives sync).
    private func viewMessageSource() {
        guard let detail = model.detail else { return }
        let messageId = detail.header.id
        let accountId = detail.header.accountId
        let title = detail.header.subject.map { "Source — \($0)" } ?? "Message Source"
        Task {
            do {
                let path = try await model.fetchRawSourcePath(messageId: messageId, accountId: accountId)
                let text = loadRawSourceText(path: path)
                MessageSourceWindow.show(text: text, title: title, messageId: messageId)
            } catch {
                model.errorText = "Couldn't load the message source: \(String(describing: error))"
            }
        }
    }

    /// Export the current selection (multi-select if any, else the primary
    /// selection) as `.eml`. Headers are fetched via each message's own
    /// account — `currentSelectionIds()` may span accounts once S34 ships.
    private func exportSelectedAsEml() {
        let ids = model.currentSelectionIds()
        guard !ids.isEmpty else { return }
        let items: [ExportableMessage] = ids.compactMap { id in
            let header = model.messages.first { $0.id == id }
                ?? (model.selectedMessage?.id == id ? model.selectedMessage : nil)
            guard let header else { return nil }
            return ExportableMessage(
                id: header.id,
                accountId: header.accountId,
                filename: sanitizedEmlFilename(subject: header.subject, sentAt: header.sentAt))
        }
        EmlExport.present(items: items, model: model)
    }

    private func printSelectedMessage() {
        guard let detail = model.detail else { return }
        MessagePrintController.printMessage(detail, mode: prefs.headerMode)
    }

    /// S36: open the "Edit Draft" toolbar action's target — see the button
    /// above for when it's enabled.
    private func editServerDraft() {
        // DR1: the core looks the draft up by the LOCAL message id; this used
        // to pass the RFC 822 Message-ID, which matched nothing, so Edit
        // Draft silently did nothing.
        guard messageBodyActionsAvailable(model.detail),
              let detail = model.detail,
              detail.rfc822MessageId != nil
        else { return }
        model.openServerDraftForEditing(accountId: detail.header.accountId, messageId: detail.header.id)
    }

    private func presentImportPanel() {
        guard let mailbox = model.actionMailbox else {
            model.errorText = "Select a mailbox before importing."
            return
        }
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        if let emlType = UTType(filenameExtension: "eml") {
            panel.allowedContentTypes = [emlType]
        }
        panel.begin { response in
            guard response == .OK else { return }
            model.importEmlFiles(urls: panel.urls, mailboxId: mailbox.id, accountId: mailbox.accountId)
        }
    }

    /// The account's reply formats (MM-7): its custom attribution, if any.
    private func replyTemplates(for accountId: String) -> ReplyTemplates {
        ReplyTemplates(
            attribution: prefs.templateLibrary.attribution(for: accountId),
            forwardHeader: prefs.templateLibrary.forwardHeaderFormat(for: accountId),
            rewrapQuotes: prefs.quoteRewrapFixed,
            myName: model.accounts.first { $0.id == accountId }?.displayName)
    }

    /// Open a compose window: blank (`mode == nil`) or prefilled as a reply /
    /// reply-all to the message currently in the reading pane.
    /// Every window receives the AI coordinator so ComposeView can show its
    /// "Draft with AI" affordance either way — enabled for a reply (there is
    /// an original message to draft from), disabled for a blank message.
    private func openCompose(mode: ReplyMode?) {
        if let mode {
            guard messageBodyActionsAvailable(model.detail),
                  let detail = model.detail else { return }
            let accountId = detail.header.accountId
            // MM-17 (D9): Reply and Reply All quote the text selected in the reading pane, when it
            // belongs to this very message; Forward keeps the whole message.
            let selection = model.readingSelection.flatMap { $0.messageId == detail.header.id ? $0.text : nil }
            let prefill = ReplyPrefill.make(detail: detail, mode: mode,
                                            selfEmail: model.selfEmail(for: accountId),
                                            aliases: model.selfAliases(for: accountId),
                                            templates: replyTemplates(for: accountId),
                                            selection: mode == .forward ? nil : selection)
            ComposeWindowController.show(
                model: model,
                session: ComposeSession(accountId: accountId, prefill: prefill,
                                        signature: Preferences.shared.signature(for: accountId)),
                aiCoordinator: model.aiCoordinator)
        } else {
            guard let accountId = model.composeAccountIdOrPromptUnavailable() else { return }
            ComposeWindowController.show(
                model: model,
                session: ComposeSession(accountId: accountId,
                                        signature: Preferences.shared.signature(for: accountId)),
                aiCoordinator: model.aiCoordinator)
        }
    }

    /// Opens a reply window for the selected message and starts an AI draft
    /// immediately — the same `draftReply` call `AiDraftSection`'s own
    /// "Draft with AI" button makes, just triggered from the Message menu /
    /// reading pane instead of typed guidance first. No-ops when AI is off
    /// or nothing is selected, matching the other Message-menu commands.
    private func openComposeReplyWithAi() {
        guard prefs.aiEnabled,
              messageBodyActionsAvailable(model.detail),
              let detail = model.detail else { return }
        let accountId = detail.header.accountId
        let prefill = ReplyPrefill.make(detail: detail, mode: .reply,
                                        selfEmail: model.selfEmail(for: accountId),
                                        aliases: model.selfAliases(for: accountId),
                                        templates: replyTemplates(for: accountId))
        ComposeWindowController.show(
            model: model,
            session: ComposeSession(accountId: accountId, prefill: prefill,
                                    signature: Preferences.shared.signature(for: accountId)),
            aiCoordinator: model.aiCoordinator)
        model.aiCoordinator.draftReply(messageId: detail.header.id, instruction: nil)
    }
}

/// A background sync problem must not seize keyboard focus or block launch.
/// One optional slot in AppModel means a burst of account failures still shows
/// at most one dismissable banner.
private struct SyncFailureBanner: View {
    let status: AccountSyncStatus
    let onDismiss: () -> Void
    let onRetry: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            // Persistent until dismissed or the next successful sync, fully
            // readable (no truncation) and selectable.
            Text(status.message)
                .font(.callout)
                .fixedSize(horizontal: false, vertical: true)
                // F3: without a floor, SwiftUI measures the window's minimum
                // height with the text squeezed to ~0 pt wide (one word per
                // line, ~1000 pt tall), so the window jumped taller when the
                // banner appeared and Classic could not fit it on a laptop
                // screen. 160 pt keeps the minimum a handful of lines.
                .frame(minWidth: 160, alignment: .leading)
                .textSelection(.enabled)
            Spacer(minLength: 12)
            if status.offersRetry {
                Button("Retry sync", action: onRetry)
                    .buttonStyle(.link)
            }
            Button("Copy details") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(status.copyableText, forType: .string)
            }
            .buttonStyle(.link)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .help("Dismiss sync notice")
            .accessibilityLabel("Dismiss sync notice")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Color.orange.opacity(0.12))
        .overlay(alignment: .bottom) { Divider() }
        .accessibilityElement(children: .contain)
        #if DEBUG
        // UIT-1 (F3): exposes the banner's frame so S73 can assert it never
        // intersects the list's column header.
        .uiProbe("syncBanner", role: "banner", label: "Sync failure banner")
        #endif
    }
}

/// Non-modal notice when the selected smart mailbox couldn't run (deleted
/// elsewhere, or a genuine DB error) — an empty list with an explanation,
/// never a crash or a silently stale list. Modelled directly on
/// SyncFailureBanner, above.
private struct SmartMailboxNoticeBanner: View {
    let message: String
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.circle")
                .foregroundStyle(.secondary)
            Text(message)
                .font(.callout)
                .lineLimit(2)
            Spacer(minLength: 12)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
            .help("Dismiss")
            .accessibilityLabel("Dismiss")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Color.secondary.opacity(0.12))
        .overlay(alignment: .bottom) { Divider() }
        .accessibilityElement(children: .contain)
    }
}

private enum UpdatePresentation: Identifiable {
    case available(version: String)
    case upToDate(currentVersion: String)
    case failed

    var id: String {
        switch self {
        case .available(let version): return "available-\(version)"
        case .upToDate(let version): return "current-\(version)"
        case .failed: return "failed"
        }
    }
}

private struct UpdateCheckSheet: View {
    let presentation: UpdatePresentation
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            switch presentation {
            case .available(let version):
                Text("Castellum \(version) is available")
                    .font(.title2).bold()
                Text("Download the latest signed disk image from GitHub.")
                    .foregroundStyle(.secondary)
                HStack {
                    Spacer()
                    Button("Later", role: .cancel) { dismiss() }
                        .keyboardShortcut(.cancelAction)
                    Button("Download") {
                        NSWorkspace.shared.open(UpdateChecker.downloadURL)
                        dismiss()
                    }
                    .keyboardShortcut(.defaultAction)
                }
            case .upToDate(let currentVersion):
                Text("Software Update").font(.title2).bold()
                Text("You're up to date (\(currentVersion))")
                HStack {
                    Spacer()
                    Button("OK") { dismiss() }
                        .keyboardShortcut(.defaultAction)
                }
            case .failed:
                Text("Software Update").font(.title2).bold()
                Text("Couldn’t check for updates. Please try again later.")
                HStack {
                    Spacer()
                    Button("OK") { dismiss() }
                        .keyboardShortcut(.defaultAction)
                }
            }
        }
        .padding(24)
        .frame(width: 420)
    }
}

/// Installs one `NotificationCenter` handler per (name, action) pair. Keeps
/// RootView's `body` small enough for the type-checker (a long inline
/// `.onReceive` chain exceeded its budget once Contacts joined the menu).
private struct NotificationHandlers: ViewModifier {
    let handlers: [(name: Notification.Name, action: () -> Void)]

    func body(content: Content) -> some View {
        content.background {
            ForEach(0..<handlers.count, id: \.self) { i in
                Color.clear.onReceive(
                    NotificationCenter.default.publisher(for: handlers[i].name)
                ) { _ in handlers[i].action() }
            }
        }
    }
}

/// Outbox status: shows queued write-path ops and a spinner while replaying.
/// Hidden entirely when there's nothing pending and no replay in flight.
private struct OutboxIndicator: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let state = outboxIndicatorState(
            pendingCount: model.pendingCount, isReplaying: model.isReplaying)
        if state.visible {
            Button {
                model.openOutbox()
            } label: {
                HStack(spacing: 4) {
                    if state.spinner {
                        ProgressView().controlSize(.small)
                    } else {
                        Image(systemName: "tray.and.arrow.up")
                    }
                    if model.pendingCount > 0 { Text("\(model.pendingCount)") }
                }
            }
            .buttonStyle(.borderless)
            .help(model.isReplaying
                  ? "Sending queued changes… Click to open the Outbox."
                  : "\(model.pendingCount) change(s) waiting to send. Click to open the Outbox.")
            .accessibilityLabel("Outbox: \(model.pendingCount) pending changes")
        }
    }
}
