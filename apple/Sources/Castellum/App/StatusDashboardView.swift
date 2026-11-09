// Castellum - Status dashboard: one place to see every background system's
// health — accounts, AI, Outbox, update check — and fix it.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

// MARK: - Row model

/// One account row's presentation data, assembled off-actor from the account
/// list, its stored (Keychain) config, `AppModel.accountSyncStatuses`, and the
/// core's local diagnostics.
struct StatusAccountRow: Identifiable {
    let id: String
    let email: String
    let imapEndpoint: String
    let smtpEndpoint: String
    /// nil = no sync has been attempted for this account yet this launch AND
    /// the core has no durable record of one either.
    let state: AccountSyncState?
    let stateLabel: String
    /// Rust-formatted timestamp string, shown verbatim (mirrors DiagnosticsView).
    let lastSyncAt: String?
    let lastErrorKind: String?
    /// The readable failure message (same text as the sync banner) when the
    /// account's current state is a failure; nil otherwise.
    let failureMessage: String?

    /// VoiceOver / harness label: address, state and (on failure) the message.
    var accessibilityText: String {
        let base = String(localized: "\(email): \(stateLabel)")
        guard let failureMessage else { return base }
        return "\(base). \(failureMessage)"
    }

    /// Single place a row is derived from per-account state, so the dashboard
    /// and the banner can never disagree. `status` is the model's CURRENT
    /// `AccountSyncStatus` for the account (failure beats any progress).
    static func make(
        account: Account,
        config: AccountConfig?,
        status: AccountSyncStatus?,
        diagnostic: LocalAccountDiagnostics?
    ) -> StatusAccountRow {
        let smtp = config?.effectiveSMTP
        let failing = status?.state.isFailure ?? false
        return StatusAccountRow(
            id: account.id,
            email: account.email,
            imapEndpoint: config.map { "\($0.host):\($0.port)" } ?? "—",
            smtpEndpoint: smtp.map { "\($0.host):\($0.port)" } ?? "—",
            state: status?.state,
            stateLabel: status?.state.label ?? "not yet synced",
            lastSyncAt: diagnostic?.lastSuccessAt,
            lastErrorKind: status?.errorKind ?? diagnostic?.lastErrorKind,
            failureMessage: failing ? status?.message : nil
        )
    }
}

// MARK: - View model

@MainActor
final class StatusDashboardViewModel: ObservableObject {
    @Published private(set) var rows: [StatusAccountRow] = []
    @Published private(set) var isLoadingRows = true
    @Published var aiTest = AiConnectionTestState()

    private weak var model: AppModel?
    private var loadGeneration = 0

    init(model: AppModel) {
        self.model = model
    }

    /// Rebuilds the account rows from the model's current in-memory state
    /// plus the core's local diagnostics (fetched off-actor).
    /// `statuses` MUST be passed when called from a `$accountSyncStatuses`
    /// sink: a `@Published` publisher fires in `willSet`, so reading
    /// `model.accountSyncStatuses` there still returns the OLD value (that
    /// stale read kept a failed account on "Syncing" - review pack D3).
    func loadRows(statuses: [String: AccountSyncStatus]? = nil) {
        guard let model else { return }
        isLoadingRows = true
        loadGeneration += 1
        let generation = loadGeneration
        let core = model.core
        let accounts = model.accounts
        let statuses = statuses ?? model.accountSyncStatuses
        // Keychain reads are per-account blocking calls — off-actor, like
        // every other Keychain access in this app (see AppModel.reSyncAll).
        let configs: [String: AccountConfig] = Dictionary(
            uniqueKeysWithValues: accounts.compactMap { account -> (String, AccountConfig)? in
                guard let json = model.storedConfig(for: account.id),
                      let config = try? AccountConfig.decode(fromJSON: json)
                else { return nil }
                return (account.id, config)
            }
        )
        Task.detached {
            let diagnostics = (try? core.localDiagnostics().accounts) ?? []
            let diagnosticsById = Dictionary(
                uniqueKeysWithValues: diagnostics.map { ($0.accountId, $0) })
            let built: [StatusAccountRow] = accounts.map { account in
                StatusAccountRow.make(
                    account: account,
                    config: configs[account.id],
                    status: statuses[account.id],
                    diagnostic: diagnosticsById[account.id])
            }
            await MainActor.run {
                // A newer load superseded this one; do not overwrite it.
                guard generation == self.loadGeneration else { return }
                self.rows = built
                self.isLoadingRows = false
            }
        }
    }

    func retry(accountId: String) {
        model?.retrySync(accountId: accountId)
    }

    /// Endpoint-only config for `ai_list_models` (mirrors `SettingsView`'s
    /// AI-tab probe — reused here rather than duplicated logic, just against
    /// the already-open app core instead of a second temporary handle).
    private struct ListModelsConfig: Encodable { let endpoint: String }

    func runAiTest() {
        guard let model else { return }
        aiTest.testStarted()
        let endpoint = Preferences.shared.aiEndpoint
        guard let data = try? JSONEncoder().encode(ListModelsConfig(endpoint: endpoint)),
              let configJSON = String(data: data, encoding: .utf8)
        else {
            aiTest.testFailed(message: "Could not encode endpoint")
            return
        }
        let core = model.core
        Task.detached {
            do {
                let result = try core.aiListModels(configJSON: configJSON)
                await MainActor.run { self.aiTest.testSucceeded(models: result) }
            } catch {
                await MainActor.run {
                    self.aiTest.testFailed(message: error.localizedDescription)
                }
            }
        }
    }
}

// MARK: - View

struct StatusDashboardView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var prefs = Preferences.shared
    @StateObject private var vm: StatusDashboardViewModel

    init(model: AppModel) {
        _model = ObservedObject(wrappedValue: model)
        _vm = StateObject(wrappedValue: StatusDashboardViewModel(model: model))
    }

    var body: some View {
        VStack(spacing: 0) {
            List {
                Section("Accounts") { accountsSection }
                Section("AI") { aiRow }
                Section("Outbox") { outboxRow }
                Section("Updates") { updateRow }
            }
            .listStyle(.inset(alternatesRowBackgrounds: true))
            Divider()
            bottomBar
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
        }
        .frame(minWidth: 520, minHeight: 340)
        .task { vm.loadRows() }
        .onReceive(model.$accountSyncStatuses) { vm.loadRows(statuses: $0) }
    }

    // MARK: Accounts

    @ViewBuilder
    private var accountsSection: some View {
        if vm.isLoadingRows && vm.rows.isEmpty {
            HStack {
                ProgressView().controlSize(.small)
                Text("Loading…").foregroundStyle(.secondary)
            }
        } else if vm.rows.isEmpty {
            Text("No accounts configured.").foregroundStyle(.secondary)
        } else {
            ForEach(vm.rows) { row in
                StatusAccountRowView(row: row, model: model, vm: vm)
            }
        }
    }

    // MARK: AI

    private var aiRow: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            aiStateIcon
            VStack(alignment: .leading, spacing: 2) {
                Text(prefs.aiEnabled ? "Enabled" : "Disabled")
                    .font(.callout)
                Text(prefs.aiEndpoint)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                if let err = vm.aiTest.testError {
                    Text(err)
                        .font(.caption2)
                        .foregroundStyle(.red)
                        .lineLimit(2)
                } else if vm.aiTest.testSucceeded {
                    Text("Reachable — \(vm.aiTest.models.count) model(s)")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            Button(vm.aiTest.isTesting ? "Testing…" : "Test Connection") { vm.runAiTest() }
                .buttonStyle(.borderless)
                .disabled(vm.aiTest.isTesting || !prefs.aiEnabled)
            Button("AI Settings…") { model.openSettingsTab(.ai) }
                .buttonStyle(.borderless)
        }
        .padding(.vertical, 2)
    }

    @ViewBuilder
    private var aiStateIcon: some View {
        if !prefs.aiEnabled {
            Image(systemName: "circle").foregroundStyle(.secondary)
        } else if vm.aiTest.isTesting {
            ProgressView().controlSize(.small)
        } else if vm.aiTest.testSucceeded {
            Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
        } else if vm.aiTest.testError != nil {
            Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
        } else {
            Image(systemName: "questionmark.circle").foregroundStyle(.secondary)
        }
    }

    // MARK: Outbox

    private var outboxRow: some View {
        HStack(spacing: 10) {
            Image(systemName: model.pendingCount > 0 ? "tray.and.arrow.up" : "checkmark.circle.fill")
                .foregroundStyle(model.pendingCount > 0 ? Color.orange : Color.green)
            Text(model.pendingCount > 0
                 ? "\(model.pendingCount) change(s) pending"
                 : "Up to date")
                .font(.callout)
            if model.isReplaying {
                ProgressView().controlSize(.small)
            }
            Spacer()
            Button("Open Outbox…") { model.openOutbox() }
                .buttonStyle(.borderless)
        }
        .padding(.vertical, 2)
    }

    // MARK: Update check

    private var updateRow: some View {
        HStack(spacing: 10) {
            Image(systemName: "arrow.triangle.2.circlepath")
                .foregroundStyle(.secondary)
            Text(lastCheckLabel)
                .font(.callout)
            Spacer()
            Button("Check for Updates…") {
                NotificationCenter.default.post(name: .castellumCheckForUpdates, object: nil)
            }
            .buttonStyle(.borderless)
        }
        .padding(.vertical, 2)
    }

    private var lastCheckLabel: String {
        guard let last = prefs.lastUpdateCheck else { return "Never checked" }
        return "Last checked \(last.formatted(date: .abbreviated, time: .shortened))"
    }

    // MARK: Bottom bar

    private var bottomBar: some View {
        HStack {
            Spacer()
            Button("Diagnostics…") { model.openDiagnostics() }
                .accessibilityLabel("Open Diagnostics")
        }
    }
}

private struct StatusAccountRowView: View {
    let row: StatusAccountRow
    @ObservedObject var model: AppModel
    @ObservedObject var vm: StatusDashboardViewModel

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            icon
            VStack(alignment: .leading, spacing: 2) {
                Text(row.email).font(.callout)
                Text("IMAP \(row.imapEndpoint) · SMTP \(row.smtpEndpoint)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack(spacing: 4) {
                    Text(row.stateLabel.capitalized)
                        .font(.caption2)
                        .foregroundStyle(isFailure ? Color.red : Color.secondary)
                    if let kind = row.lastErrorKind, isFailure {
                        Text("(\(kind))")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                    if let lastSync = row.lastSyncAt, !isFailure {
                        Text("· last synced \(lastSync)")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
                if isFailure, let message = row.failureMessage {
                    Text(message)
                        .font(.caption)
                        .foregroundStyle(Color.red)
                        .fixedSize(horizontal: false, vertical: true)
                }
                // Live progress for a first (or any long-running) sync —
                // read straight off the model rather than `row`, since a
                // snapshot moving every few hundred ms would otherwise need
                // `StatusDashboardViewModel.loadRows()` re-run constantly
                //. `nil` (no snapshot yet, or the sync
                // already finished) simply shows nothing extra.
                if isSyncing, let progress = model.accountSyncProgress[row.id] {
                    SyncProgressLabel(progress: progress)
                }
            }
            Spacer()
            if isSyncing {
                ProgressView().controlSize(.small)
            } else {
                retryButton
            }
            Button("Accounts…") { model.openAccountsSettings(accountId: row.id) }
                .buttonStyle(.borderless)
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(row.accessibilityText)
        #if DEBUG
        // UIT-1: SwiftUI text is invisible to the NSView walk; expose the row's
        // text (address, state, failure message) so scenarios can assert it.
        .uiProbe("statusRow", role: "staticText", label: row.accessibilityText, action: nil)
        #endif
    }

    private var retryButton: some View {
        let button = Button("Retry") { vm.retry(accountId: row.id) }
            .buttonStyle(.borderless)
            .accessibilityLabel("Retry sync for \(row.email)")
        #if DEBUG
        return button.uiProbe("retry", label: "Retry sync for \(row.email)") { vm.retry(accountId: row.id) }
        #else
        return button
        #endif
    }

    private var isFailure: Bool { row.state?.isFailure ?? false }
    private var isSyncing: Bool { row.state == .syncing }

    private var icon: some View {
        let name: String
        let colour: Color
        if row.state == .syncing {
            (name, colour) = ("arrow.triangle.2.circlepath", .secondary)
        } else if row.state == .ok {
            (name, colour) = ("checkmark.circle.fill", .green)
        } else if row.state != nil {
            (name, colour) = ("exclamationmark.circle.fill", .orange)
        } else {
            (name, colour) = ("circle", .secondary)
        }
        return Image(systemName: name).foregroundStyle(colour)
    }
}

/// One line of live sync progress — thin wrapper around the
/// pure `SyncProgressPresentation.label(for:)` formatter.
struct SyncProgressLabel: View {
    let progress: SyncProgress

    var body: some View {
        Text(SyncProgressPresentation.label(for: progress))
            .font(.caption2)
            .foregroundStyle(.secondary)
            .lineLimit(1)
    }
}
