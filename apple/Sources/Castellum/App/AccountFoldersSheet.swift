// Castellum - edit an existing account's synced folders.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// S62/#160 let a user pick folders + an initial-history window before an
// account's FIRST sync (AccountSetupSheet's "First sync" section). This sheet
// is the residual: Settings > Accounts > "Folders…" opens it for an
// ALREADY-ADDED account, reusing the same picker component (`FolderChecklist`)
// and the same policy functions (`SyncScope`) — nothing here re-implements
// either.
//
// On appear it probes the account's OWN server with its stored credentials
// (`AccountConfig.connectionTestJSON()`, the exact call Test Connection
// uses) to list the live folder set, pre-ticking the account's current
// selection (`SyncScope.currentSelection`). The password field never
// appears here — it travels through unread from the stored config, and
// `SyncScope.applyFolderEdit`/`applyHistoryOnlyEdit` only ever touch
// `mailboxes`/`initialSyncSince` on a COPY of it, so it cannot drift.
//
// If the probe fails (server unreachable, wrong password since setup, …)
// the sheet falls back to a read-only view of the STORED selection with a
// clear explanation, per the task's decided default — never an empty list,
// and never a picker with nothing to select against. The initial-history
// window can still be changed in that case (it needs no live folder list);
// the folder selection itself cannot. A stored path the live probe never
// finds (a folder renamed or removed server-side) is called out separately,
// in red, even once the probe succeeds.
//
// Saving never touches credentials and never triggers a sync
// (`AppModel.updateFolderSelection` -> `saveAccountWithoutSync`, the same
// "Save anyway" path every other config edit uses) — only the mailbox
// filter/history window are persisted, and every folder no longer selected
// is pruned locally right away (`Database.apply_mailbox_selection`).
//
// Follow-up review fixes:
//   * Save is disabled while `model.accountSyncStatuses[accountId]` is
//     `.syncing` — an in-flight sync can race a folder-removal purge
//     (`AppModel.updateFolderSelection`'s doc comment has the detail).
//   * A non-terminal operation still touching a folder about to be removed
//     blocks the save (never silently cancelled — removing a folder leaves
//     server UIDs valid, so the op is still live intent). `pendingBlockers`
//     backs the resulting alert: "Sync now and try again" or the explicit,
//     destructive "Discard those changes and remove", which retries with
//     `force: true`.
//   * Save requires a non-empty resolved selection (`resolvedKeepPaths`),
//     not just a non-empty ticked set — ticking only non-selectable/unknown
//     rows would otherwise resolve to nothing and silently remove everything.

import SwiftUI

struct AccountFoldersSheet: View {
    @ObservedObject var model: AppModel
    let accountId: String
    @Environment(\.dismiss) private var dismiss

    private enum ProbePhase {
        case loading
        case failed(String)
        case done(ConnectionTestReport)
    }

    @State private var config: AccountConfig?
    @State private var probePhase: ProbePhase = .loading
    @State private var selection: Set<String> = []
    @State private var historyChoice: InitialHistory = .all
    @State private var historyTouched = false
    @State private var isSaving = false
    /// Non-nil = the last Save attempt was refused because these folders
    /// still have undelivered changes; drives the confirmation alert.
    @State private var pendingBlockers: [MailboxSelectionBlocker]?

    private var isAccountSyncing: Bool {
        model.accountSyncStatuses[accountId]?.state == .syncing
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(String(localized: "Folders for \(config?.email ?? accountId)")).font(.headline)

            folderSection

            Divider()

            historySection

            if isAccountSyncing {
                Text("Waiting for the current sync to finish…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                Spacer()
                if isSaving { ProgressView().controlSize(.small) }
                Button("Save") { save(force: false) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(width: 400)
        .onAppear { load() }
        .alert(
            "Changes Haven't Reached the Server Yet",
            isPresented: Binding(
                get: { pendingBlockers != nil },
                set: { if !$0 { pendingBlockers = nil } }
            ),
            presenting: pendingBlockers
        ) { _ in
            Button("Sync Now and Try Again") {
                pendingBlockers = nil
                model.retrySync(accountId: accountId)
                dismiss()
            }
            Button("Discard and Remove", role: .destructive) {
                pendingBlockers = nil
                save(force: true)
            }
            Button("Cancel", role: .cancel) { pendingBlockers = nil }
        } message: { blockers in
            Text(blockedMessage(for: blockers))
        }
    }

    private func blockedMessage(for blockers: [MailboxSelectionBlocker]) -> String {
        let byPath = blockers
            .sorted { $0.path < $1.path }
            .map { "'\($0.path)' (\($0.pendingOperations))" }
            .joined(separator: ", ")
        return String(localized: "These folders have changes not yet sent to the server: \(byPath). Sync now so they go through (changes that already failed permanently are not retried by syncing), or discard them and remove the folder(s) anyway.")
    }

    @ViewBuilder
    private var folderSection: some View {
        switch probePhase {
        case .loading:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Checking the server's folders…").font(.caption).foregroundStyle(.secondary)
            }
        case .failed(let message):
            VStack(alignment: .leading, spacing: 4) {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                Text("Showing the currently synced selection, read-only. Fix the connection (Re-authenticate…) and reopen Folders… to change it.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                readOnlyStoredSelection
            }
        case .done(let report):
            FolderChecklist(mailboxes: report.mailboxes, selection: $selection)
            if resolvedKeepPaths(for: report).isEmpty {
                Text("Choose at least one folder to sync.")
                    .font(.caption)
                    .foregroundStyle(.red)
            }
            renamedStoredPathsNote(for: report)
        }
    }

    /// The resolved, concrete keep-path list for the current ticks against
    /// `report`'s live folder list — the same gate `save()` and `canSave`
    /// use, so "nothing selectable is actually ticked" is caught in one
    /// place ( follow-up review: a non-empty `selection` that
    /// resolves to nothing must still block Save).
    private func resolvedKeepPaths(for report: ConnectionTestReport) -> [String] {
        SyncScope.resolvedKeepPaths(selected: selection, mailboxes: report.mailboxes)
    }

    /// A stored explicit path the live probe found no match for — a folder
    /// renamed or removed server-side since it was last selected. Shown so
    /// the empty-looking tick for it (nothing on the live list matches) has
    /// an explanation, not just a disabled Save ( follow-up
    /// review, decided default).
    @ViewBuilder
    private func renamedStoredPathsNote(for report: ConnectionTestReport) -> some View {
        let stale = SyncScope.staleStoredPaths(stored: config?.mailboxes, mailboxes: report.mailboxes)
        if !stale.isEmpty {
            Text("No longer on the server (renamed or removed there?): \(stale.joined(separator: ", "))")
                .font(.caption)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The stored selection, never an empty list even when it's the (nil)
    /// default set — the fallback the task's decided default asks for.
    @ViewBuilder
    private var readOnlyStoredSelection: some View {
        if let stored = config?.mailboxes, !stored.isEmpty {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(stored, id: \.self) { path in
                    Label(path, systemImage: "checkmark.square")
                        .font(.caption)
                }
            }
        } else {
            Text("Using the default folder set: Inbox, Sent, Drafts, Trash, Junk and subscribed folders.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var historySection: some View {
        VStack(alignment: .leading, spacing: 4) {
            Picker("New folder history", selection: $historyChoice) {
                ForEach(InitialHistory.allCases) { Text($0.label).tag($0) }
            }
            .onChange(of: historyChoice) { _ in historyTouched = true }
            Text("Applies only to a folder synced for the very first time — one already synced keeps its history.")
                .font(.caption2)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var canSave: Bool {
        guard !isSaving, !isAccountSyncing, config != nil else { return false }
        switch probePhase {
        case .done(let report): return !resolvedKeepPaths(for: report).isEmpty
        case .loading: return false
        case .failed: return historyTouched
        }
    }

    /// Loads the stored config, then probes the server for the live folder
    /// list using ITS credentials — the same call Test Connection makes,
    /// never a new password prompt.
    private func load() {
        guard let stored = model.storedConfig(for: accountId),
              let decoded = try? AccountConfig.decode(fromJSON: stored)
        else {
            probePhase = .failed("Stored credentials are unavailable for this account.")
            return
        }
        config = decoded
        historyChoice = InitialHistory.nearest(to: decoded.initialSyncSince)
        guard let json = try? decoded.connectionTestJSON() else {
            probePhase = .failed("Could not encode the account settings.")
            return
        }
        Task { @MainActor in
            let result = await model.testConnection(configJSON: json)
            switch result {
            case .success(let report) where report.imap.ok:
                probePhase = .done(report)
                selection = SyncScope.currentSelection(stored: decoded.mailboxes, mailboxes: report.mailboxes)
            case .success:
                probePhase = .failed("Could not sign in to list the server's folders.")
            case .failure(let error):
                probePhase = .failed("The connection couldn't run: \(error.localizedDescription)")
            }
        }
    }

    /// Persists the edit through `AppModel.updateFolderSelection` (folders
    /// probed live) or a plain history-only save (probe failed/unreachable —
    /// there is no live list to resolve a folder edit against, so the
    /// folder filter is carried through unchanged via `applyHistoryOnlyEdit`).
    /// Neither path touches credentials or triggers a sync. `force` is only
    /// ever `true` here from the blocked-changes alert's "Discard and
    /// Remove" button.
    private func save(force: Bool) {
        guard let config else { return }
        let newSince = historyTouched ? historyChoice.sinceDate() : config.initialSyncSince
        isSaving = true
        switch probePhase {
        case .done(let report):
            let updated = SyncScope.applyFolderEdit(
                to: config, selected: selection, mailboxes: report.mailboxes, initialSyncSince: newSince)
            guard let json = try? updated.encodedJSON() else {
                model.errorText = "Could not encode account config."
                isSaving = false
                return
            }
            let keepPaths = resolvedKeepPaths(for: report)
            model.updateFolderSelection(accountId: accountId, configJSON: json, keepPaths: keepPaths, force: force) { result in
                isSaving = false
                switch result {
                case .success:
                    dismiss()
                case .blocked(let blockers):
                    pendingBlockers = blockers
                case .failure(let message):
                    model.errorText = message
                }
            }
        case .loading, .failed:
            let updated = SyncScope.applyHistoryOnlyEdit(to: config, initialSyncSince: newSince)
            guard let json = try? updated.encodedJSON() else {
                model.errorText = "Could not encode account config."
                isSaving = false
                return
            }
            model.saveAccountWithoutSync(accountID: accountId, configJSON: json)
            isSaving = false
            dismiss()
        }
    }
}
