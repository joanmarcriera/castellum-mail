// Castellum - Preferences window (General + AI + Accounts tabs).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// Which Settings tab is frontmost. `AppModel.settingsTab` drives this so
/// `AppModel.openAccountsSettings` (the missing-credentials alert's "Open
/// Accounts…" button) can land on a specific tab across the
/// WindowGroup/Settings scene boundary.
enum SettingsTab: Hashable {
    case general, ai, accounts, rules, notifications, tags
}

struct SettingsView: View {
    @ObservedObject var prefs: Preferences
    @ObservedObject var model: AppModel

    var body: some View {
        TabView(selection: $model.settingsTab) {
            generalTab
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(SettingsTab.general)
            AiSettingsTab(prefs: prefs, model: model)
                .tabItem { Label("AI", systemImage: "sparkles") }
                .tag(SettingsTab.ai)
            AccountsTab(model: model)
                .tabItem { Label("Accounts", systemImage: "person.crop.circle") }
                .tag(SettingsTab.accounts)
            RulesSettingsTab(model: model)
                .tabItem { Label("Rules", systemImage: "wand.and.stars") }
                .tag(SettingsTab.rules)
            NotificationsSettingsTab(prefs: prefs, model: model)
                .tabItem { Label("Notifications", systemImage: "bell") }
                .tag(SettingsTab.notifications)
            TagsSettingsTab(model: model)
                .tabItem { Label("Tags", systemImage: "tag") }
                .tag(SettingsTab.tags)
        }
        .frame(minWidth: SettingsLayout.minWidth, idealWidth: SettingsLayout.idealWidth, maxWidth: SettingsLayout.maxWidth, minHeight: SettingsLayout.minHeight, idealHeight: SettingsLayout.idealHeight, maxHeight: SettingsLayout.maxHeight)
        .settingsWindowPolicy()
    }

    private var generalTab: some View {
        // ScrollView so no control is clipped when the window is short (S71).
        ScrollView {
            generalForm
        }
    }

    /// Built from `SettingsPane` / `SettingsSection` / `SettingsRow`, not a
    /// `Form`: a macOS columns Form does not keep its content column inside the
    /// width it is offered, so this pane was wider than its window at every
    /// size and its edges were clipped (, see SettingsSheetLayout).
    private var generalForm: some View {
        SettingsPane {
            SettingsSection("Reading") {
                SettingsRow("Mark messages read:") {
                    Picker("Mark messages read:", selection: $prefs.autoMarkReadMode) {
                        ForEach(AutoMarkReadMode.allCases) { mode in
                            Text(mode.label).tag(mode)
                        }
                    }
                    .pickerStyle(.radioGroup)
                    .labelsHidden()

                    if prefs.autoMarkReadMode == .afterDelay {
                        Stepper(
                            "Delay: \(prefs.markReadDelaySeconds) \(prefs.markReadDelaySeconds == 1 ? String(localized: "second") : String(localized: "seconds"))",
                            value: $prefs.markReadDelaySeconds, in: 1...30
                        )
                        .padding(.leading, 20)
                    }
                }
            }

            SettingsSection("Layout") {
                SettingsRow("Message list:") {
                    Picker("Message list:", selection: $prefs.viewerLayout) {
                        ForEach(ViewerLayout.allCases) { Text($0.label).tag($0) }
                    }
                    .pickerStyle(.radioGroup)
                    .labelsHidden()
                    Text("Also under View in the menu bar.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Markdown mail") {
                SettingsRow("HTML theme:") {
                    Picker("HTML theme:", selection: $prefs.markdownTheme) {
                        ForEach(MarkdownThemeChoice.allCases) { Text($0.label).tag($0) }
                    }
                    .labelsHidden()
                    .fixedSize()
                    Text("How the HTML copy of a Markdown message looks. Each message can override it in its compose window. The plain-text part is always your Markdown source.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Sending") {
                SettingsRow("Undo send:") {
                    Picker("Undo send:", selection: $prefs.undoSendDelay) {
                        ForEach(UndoSendDelay.allCases) { delay in
                            Text(delay.label).tag(delay)
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    Text("After Send, a banner offers Undo for this long before the message actually goes out.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                SettingsRow {
                    Toggle("Warn when a message mentions an attachment but has none",
                           isOn: $prefs.warnAboutForgottenAttachment)
                }
                SettingsRow {
                    Toggle("Send plain text as format=flowed", isOn: $prefs.sendFlowed)
                    Text("Wraps plain-text mail with soft line breaks (RFC 3676) so it reads well in any client and re-flows on narrow screens. Markdown mail is not affected.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Account Setup") {
                SettingsRow {
                    Toggle("Look up server settings automatically", isOn: $prefs.autoconfigLookupEnabled)
                    Text("For unknown domains, asks DNS, the domain’s own settings page and Mozilla’s provider database for server names. Only the domain after the @ is sent.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            SettingsSection("Attachments") {
                SettingsRow("Clicking an attachment:") {
                    Picker("Clicking an attachment:", selection: $prefs.attachmentDefaultAction) {
                        Text("Ask each time").tag(nil as AttachmentDefaultAction?)
                        ForEach(AttachmentDefaultAction.allCases) { action in
                            Text(action.label).tag(action as AttachmentDefaultAction?)
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    Text("Also available from the gear menu on a message's attachment strip.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Remote Content") {
                SettingsRow {
                    Toggle("Always load remote images", isOn: $prefs.alwaysLoadRemoteImages)
                    Text("When off, messages with remote images show a per-message Load button. Blocking remote images prevents senders from tracking when you read mail.")
                        .font(.caption)
                        .foregroundStyle(.secondary)

                    if !model.senderRemoteContentPolicies.isEmpty {
                        Text("Remote content senders").font(.caption).bold()
                        List(model.senderRemoteContentPolicies) { policy in
                            HStack {
                                Text(policy.fromAddress).font(.caption).textSelection(.enabled)
                                Spacer()
                                Text(policy.allowed ? "Always allowed" : "Always blocked")
                                    .font(.caption2).foregroundStyle(.secondary)
                                Button("Revoke") { model.revokeSenderRemoteContent(policy) }
                                    .font(.caption)
                                    .buttonStyle(.bordered)
                                    .controlSize(.small)
                            }
                        }
                        .frame(height: 100)
                    }
                }
            }

            SettingsSection("Spotlight") {
                SettingsRow {
                    Toggle(
                        "Index message subjects and senders in Spotlight",
                        isOn: Binding(
                            get: { model.spotlightIndexingEnabled },
                            set: { model.setSpotlightIndexingEnabled($0) }
                        )
                    )
                    Text("Off by default. When on, the subject and sender of your messages become searchable system-wide in Spotlight — message bodies, attachments and HTML content are never indexed, and everything stays on this Mac. Turning this off removes everything Castellum indexed.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Updates") {
                SettingsRow("Check for updates:") {
                    Picker("Check for updates:", selection: $prefs.updateCheckSchedule) {
                        ForEach(UpdateCheckSchedule.allCases) { schedule in
                            Text(schedule.label).tag(schedule)
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    Text(prefs.updateCheckSchedule.disclosure)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            SettingsSection("Key Bindings") {
                SettingsRow {
                    // Side by side when the pane is wide enough, stacked when it is not.
                    ViewThatFits(in: .horizontal) {
                        HStack { keyBindingButtons }
                        VStack(alignment: .leading) { keyBindingButtons }
                    }
                    Text(String(localized: "keybindings.json lets you rebind any menu shortcut or key. A command can have several keys, and a key can be a sequence such as \"g i\". \"Reveal\" writes a documented starting point (every command's current binding) the first time; edit its \"overrides\" section, save, then \"Reload\". A bad entry is skipped, never a crash."))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if !model.keyBindings.problems.isEmpty {
                        Text(String(localized: "\(model.keyBindings.problems.count) entries in keybindings.json were not applied."))
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }

            SettingsSection("Default Mail App") {
                SettingsRow { DefaultMailAppSection() }
            }
        }
        .padding(20)
        .onAppear { model.reloadSenderRemoteContentPolicies() }
    }

    @ViewBuilder private var keyBindingButtons: some View {
        Button("Reveal Key Bindings File…") { model.keyBindings.revealFileInFinder() }
        Button("Reload Key Bindings") {
            model.keyBindings.reload()
            if !model.keyBindings.warnings.isEmpty {
                model.errorText = model.keyBindings.warnings.joined(separator: "\n")
            }
            model.objectWillChange.send()
        }
    }
}

/// Pure state for the AI Settings "Test Connection" flow: success/failure
/// indicator + fetched model list, and the rules for clearing them when the
/// endpoint or chosen model changes. Extracted (mirroring `AiEventReducer`)
/// so those clearing rules are unit-testable without going through SwiftUI.
struct AiConnectionTestState: Equatable {
    var isTesting = false
    var testError: String?
    /// Set only by a completed, successful test; decoupled from `models` so
    /// picking a model afterwards doesn't make the picker's own data vanish.
    var testSucceeded = false
    var models: [AiModelInfo] = []

    /// A changed endpoint invalidates both any prior test result AND the
    /// model list it fetched — a different endpoint may serve different models.
    mutating func endpointChanged() {
        testError = nil
        testSucceeded = false
        models = []
    }

    /// A changed model invalidates only the success/failure indicator. It
    /// must NOT clear `models`: that list is the picker's own data source, so
    /// clearing it here would blank the picker the instant the user uses it.
    mutating func modelChanged() {
        testError = nil
        testSucceeded = false
    }

    /// Names for the Model / Embedding pickers: the fetched list, then any
    /// non-empty persisted choice not already in it (so the saved model shows
    /// before a test, and survives `endpointChanged()`, which clears `models`).
    func pickerModelNames(current: [String]) -> [String] {
        var names = models.map(\.name)
        for name in current where !name.isEmpty && !names.contains(name) { names.append(name) }
        return names
    }

    /// A new "Test Connection" attempt starts: clear any prior result.
    mutating func testStarted() {
        isTesting = true
        testError = nil
        testSucceeded = false
        models = []
    }

    mutating func testSucceeded(models: [AiModelInfo]) {
        self.models = models
        testError = nil
        testSucceeded = true
        isTesting = false
    }

    /// `message` is verbatim from the core (e.g. the egress-refused privacy
    /// text for a non-local endpoint) — callers must never reword it.
    mutating func testFailed(message: String) {
        testError = message
        testSucceeded = false
        isTesting = false
    }
}

// Extracted so its @State doesn't pollute SettingsView.
private struct AiSettingsTab: View {
    @ObservedObject var prefs: Preferences
    @ObservedObject var model: AppModel
    @State private var test = AiConnectionTestState()

    /// Names shown in the two pickers: what the last test fetched plus the
    /// persisted choices, so the configured model is visible before any test.
    private var pickerNames: [String] {
        test.pickerModelNames(current: [prefs.aiModel, prefs.aiEmbeddingModel])
    }

    // Built like generalTab (ScrollView + SettingsPane/Section/Row), not a
    // columns Form: a Form is wider than the window and cannot scroll to what
    // it clips at the 480 pt minimum (xg-1).
    var body: some View {
        ScrollView {
            SettingsPane {
                SettingsSection("Enable AI") {
                    SettingsRow {
                        Toggle("Enable AI features", isOn: $prefs.aiEnabled)
                        Text("AI features run exclusively on a local Ollama instance. No data is sent to the cloud.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        // First-launch auto-enable disclosure — set only
                        // on the launch the probe actually turned AI on; session-only
                        // (Preferences never persists it), so it never reappears later.
                        if let reason = prefs.aiAutoEnableReason {
                            Text(reason)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }

                if prefs.aiEnabled {
                    SettingsSection("Connection") {
                        SettingsRow("Endpoint:") {
                            // `prompt:` (not the titleKey positional, which macOS
                            // renders as persistent leading text inside a
                            // .roundedBorder field rather than a placeholder).
                            TextField("", text: $prefs.aiEndpoint, prompt: Text("http://127.0.0.1:11434"))
                                .textFieldStyle(.roundedBorder)
                                .labelsHidden()
                            Text("Local network only — public endpoints are refused by the core.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            if let detected = prefs.aiAutoDetectedEndpoint, detected == prefs.aiEndpoint {
                                Text("Detected at \(detected.replacingOccurrences(of: "http://", with: ""))")
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                        }

                        SettingsRow {
                            HStack(spacing: 8) {
                                // Both branches are LocalizedStringKey explicitly: a
                                // ternary of two string LITERALS yields a String, which
                                // binds to Button(_ title: S) — the NON-localising
                                // initialiser — so the two most-seen words in this pane
                                // could never be extracted, and no catalogue entry would
                                // ever apply to them.
                                Button(test.isTesting
                                    ? LocalizedStringKey("Testing…")
                                    : LocalizedStringKey("Test Connection")) { runTest() }
                                    .disabled(test.isTesting || prefs.aiEndpoint.isEmpty)
                                if test.isTesting { ProgressView().controlSize(.small) }
                            }
                            .padding(.top, 4)
                        }

                        if test.testSucceeded {
                            SettingsRow {
                                HStack(spacing: 6) {
                                    Image(systemName: "checkmark.circle.fill")
                                        .foregroundStyle(.green)
                                    Text("Connected — \(test.models.count) \(test.models.count == 1 ? String(localized: "model") : String(localized: "models"))")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                        }

                        if let err = test.testError {
                            SettingsRow {
                                HStack(alignment: .firstTextBaseline, spacing: 6) {
                                    Image(systemName: "xmark.circle.fill")
                                        .foregroundStyle(.red)
                                    // Verbatim — this is the egress-refused privacy text
                                    // (e.g. "public endpoints are refused"); never reword it.
                                    Text(err)
                                        .font(.caption)
                                        .foregroundStyle(.red)
                                        .textSelection(.enabled)
                                }
                            }
                        }

                        if !pickerNames.isEmpty {
                            SettingsRow("Model:") {
                                Picker("Model:", selection: $prefs.aiModel) {
                                    Text("Select a model…").tag("")
                                    ForEach(pickerNames, id: \.self) { name in
                                        Text(name).tag(name)
                                    }
                                }
                                .labelsHidden()
                                if !test.models.isEmpty {
                                    Text("\(test.models.count) \(test.models.count == 1 ? String(localized: "model") : String(localized: "models")) available at endpoint.")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                            // F1: the model that indexes mail for Find Similar. Distinct from the
                            // chat model: a chat model cannot embed.
                            SettingsRow("Embedding model:") {
                                Picker("Embedding model:", selection: $prefs.aiEmbeddingModel) {
                                    Text("None (no background indexing)").tag("")
                                    ForEach(pickerNames, id: \.self) { name in
                                        Text(name).tag(name)
                                    }
                                }
                                .labelsHidden()
                            }
                        }
                        SettingsRow { EmbeddingSettingsStatus(drainer: model.aiCoordinator.embeddings) }
                    }

                    SettingsSection("Activity") {
                        SettingsRow {
                            Button("Show AI Activity…") { model.openAiActivity() }
                            Text("Every summarise / draft-reply job this session, with progress and Cancel.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                }

                SettingsSection("Help") {
                    SettingsRow {
                        Link("Learn more…", destination: URL(string: "https://github.com/joanmarcriera/castellum-mail/blob/main/docs/help/ai.html")!)
                            .font(.caption)
                    }
                }
            }
            .padding(20)
        }
        .onChange(of: prefs.aiEndpoint) { _ in test.endpointChanged() }
        .onChange(of: prefs.aiModel) { _ in test.modelChanged() }
    }

    /// Endpoint-only config for `ai_list_models` (snake_case not needed:
    /// single lowercase key, but built via JSONEncoder for quote/backslash safety).
    private struct ListModelsConfig: Encodable {
        let endpoint: String
    }

    private func runTest() {
        test.testStarted()
        guard let data = try? JSONEncoder().encode(ListModelsConfig(endpoint: prefs.aiEndpoint)),
              let configJSON = String(data: data, encoding: .utf8) else {
            test.testFailed(message: "Could not encode endpoint")
            return
        }
        let profileDir = ProfileLocation.current().directory
        Task.detached {
            do {
                // Open a temporary second handle solely for this test call.
                // This is safe under today's preconditions: the profile DB runs
                // in WAL mode with a busy_timeout, migrations are idempotent
                // (re-open of an already-migrated profile is a no-op), and
                // ai_list_models itself never touches the DB — it only issues
                // the egress-guarded HTTP request. REVISIT if migrations ever
                // become non-idempotent or open-time side effects are added:
                // then Settings must borrow AppModel's core instead of opening
                // its own handle.
                let core = try CastellumCore(profilePath: profileDir)
                let result = try core.aiListModels(configJSON: configJSON)
                await MainActor.run {
                    self.test.testSucceeded(models: result)
                }
            } catch {
                await MainActor.run {
                    // Verbatim from the core — e.g. the egress-refused privacy
                    // text for a non-local endpoint. Never reworded here.
                    self.test.testFailed(message: error.localizedDescription)
                }
            }
        }
    }
}

// MARK: - Accounts tab

/// Settings > Accounts: list accounts, Add a new one, Re-authenticate (refresh
/// stored credentials), edit its Folders…, Duplicate one's
/// settings into a new account, or permanently Remove one.
/// Add, Re-authenticate and Duplicate all present `AccountSetupSheet` — this
/// view never re-implements its fields. Folders… presents
/// `AccountFoldersSheet` instead — same picker component, different sheet,
/// since it edits an already-added account rather than a pre-sync one.
private struct AccountsTab: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var prefs = Preferences.shared
    @State private var selection: String?
    @State private var sheetMode: SheetMode?
    @State private var pendingRemoval: Account?
    @State private var pendingNewIdentityAccountId: String?
    @State private var pendingEditIdentity: Identity?
    @State private var pendingReset: Account?
    /// Identity queued for removal, pending confirmation (mirrors `pendingRemoval`
    /// for accounts —: identity Remove is destructive and
    /// unconfirmed today).
    @State private var pendingIdentityRemoval: (accountId: String, identity: Identity)?
    @State private var diagnosticsByAccount: [String: LocalAccountDiagnostics] = [:]
    @State private var configsByAccount: [String: AccountConfig] = [:]

    private enum SheetMode: Identifiable {
        case add
        case reauthenticate(String)
        /// Add a NEW account, prefilled from this existing account's settings
        ///. Distinct from `.reauthenticate`, which reuses the
        /// same account id — this always creates a fresh one.
        case duplicate(String)
        /// Edit which folders (and, going forward, how far back) an
        /// ALREADY-ADDED account syncs — the S62 residual;
        /// distinct from `.reauthenticate`, which never shows this.
        case folders(String)
        var id: String {
            switch self {
            case .add: return "add"
            case .reauthenticate(let accountId): return "reauth-\(accountId)"
            case .duplicate(let accountId): return "duplicate-\(accountId)"
            case .folders(let accountId): return "folders-\(accountId)"
            }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if model.accounts.isEmpty {
                emptyState
            } else {
                List(selection: $selection) {
                    ForEach(model.accounts) { account in
                        AccountRow(
                            account: account,
                            writeState: model.writeState(for: account.id),
                            diagnostics: diagnosticsByAccount[account.id],
                            status: model.accountSyncStatuses[account.id],
                            config: configsByAccount[account.id],
                            isChangingWritePolicy: model.isChangingWritePolicy,
                            onWritePolicy: { model.requestWritePolicyChange(for: account) }
                        )
                        .tag(account.id)
                    }
                }
                .listStyle(.inset(alternatesRowBackgrounds: true))
                .frame(minHeight: SettingsLayout.accountListMinHeight)
                if let account = model.accounts.first(where: { $0.id == selection }) {
                    Divider()
                    // Scrolls inside a capped area so the signature editor and
                    // identities can never cover the account list (S71).
                    ScrollView {
                        VStack(alignment: .leading, spacing: 0) {
                            signatureEditor(for: account)
                            ReplyAttributionEditor(account: account, prefs: prefs)
                            // Same 10 pt leading inset as signatureEditor (RP04/RP11).
                            markdownDefaultToggle(for: account)
                                .padding(.horizontal, 10)
                            identitiesEditor(for: account)
                                .padding(.horizontal, 10)
                                .padding(.vertical, 4)
                        }
                    }
                    .frame(maxHeight: SettingsLayout.accountDetailMaxHeight)
                }
            }
            Divider()
            toolbar.padding(10)
            if !model.accounts.isEmpty {
                writePolicyFooter
                    .padding(.horizontal, 10)
                    .padding(.bottom, 10)
            }
        }
        .frame(minHeight: 260)
        .task { await loadDiagnostics() }
        .onAppear { loadConfigs() }
        // A Reconnect (or Add) may have changed the stored host/port; the
        // account list changes (reloadAccounts) whenever that happens.
        .onChange(of: model.accounts) { _ in loadConfigs() }
        // Diagnostics are read once on appear; a sync finishing later (a new or
        // duplicated account's first sync) must refresh them (RP06).
        .onChange(of: model.accountSyncStatuses) { _ in Task { await loadDiagnostics() } }
        // Land on the account "Open Accounts…" asked for (AppModel already
        // switched `settingsTab`; this consumes the one-shot selection).
        .onAppear { applyPendingSelection() }
        .onChange(of: model.settingsSelectedAccountId) { _ in applyPendingSelection() }
        .sheet(item: $sheetMode) { mode in
            switch mode {
            case .add:
                AccountSetupSheet(model: model)
            case .reauthenticate(let accountId):
                AccountSetupSheet(model: model, existingAccountId: accountId)
            case .duplicate(let accountId):
                AccountSetupSheet(model: model, duplicateFromAccountId: accountId)
            case .folders(let accountId):
                AccountFoldersSheet(model: model, accountId: accountId)
            }
        }
        .alert(
            "Remove Account?",
            isPresented: Binding(
                get: { pendingRemoval != nil },
                set: { if !$0 { pendingRemoval = nil } }
            ),
            presenting: pendingRemoval
        ) { account in
            Button("Remove", role: .destructive) {
                model.removeAccount(accountId: account.id)
                if selection == account.id { selection = nil }
                pendingRemoval = nil
            }
            Button("Cancel", role: .cancel) { pendingRemoval = nil }
        } message: { account in
            Text(String(localized: "This permanently deletes \(account.email)'s locally-synced mail, drafts and queued sends from this Mac, and removes its saved password from the Keychain. The mail server itself is not affected. This cannot be undone."))
        }
        .alert(
            "Reset and Re-sync This Account?",
            isPresented: Binding(
                get: { pendingReset != nil },
                set: { if !$0 { pendingReset = nil } }
            ),
            presenting: pendingReset
        ) { account in
            Button("Reset", role: .destructive) {
                let account = account
                pendingReset = nil
                model.resetAccount(accountId: account.id)
            }
            Button("Cancel", role: .cancel) { pendingReset = nil }
        } message: { account in
            Text(String(localized: "This wipes \(account.email)'s locally-synced mail, downloaded attachments, drafts, sync state and AI summaries from this Mac, then re-syncs from the server.\n\nKept: its saved password, host/port settings and signature. The mail server itself is not affected. This cannot be undone."))
        }
        .alert(
            "Still Sending",
            isPresented: Binding(
                get: { model.resetAccountBlocked != nil },
                set: { if !$0 { model.resetAccountBlocked = nil } }
            ),
            presenting: model.resetAccountBlocked
        ) { prompt in
            Button("Discard and Reset", role: .destructive) {
                model.resetAccountBlocked = nil
                model.resetAccount(accountId: prompt.accountId, force: true)
            }
            Button("Cancel", role: .cancel) { model.resetAccountBlocked = nil }
        } message: { prompt in
            Text(String(localized: "\(prompt.email) still has \(prompt.blockers.unsentOutboxDrafts) unsent outbox draft(s) and \(prompt.blockers.pendingOperations) pending change(s) not yet sent to the server. Resetting now discards them — any message still sending will NOT be delivered."))
        }
        .alert(
            "Remove Identity?",
            isPresented: Binding(
                get: { pendingIdentityRemoval != nil },
                set: { if !$0 { pendingIdentityRemoval = nil } }
            ),
            presenting: pendingIdentityRemoval
        ) { pending in
            Button("Remove", role: .destructive) {
                model.deleteIdentity(id: pending.identity.id, accountId: pending.accountId)
                pendingIdentityRemoval = nil
            }
            Button("Cancel", role: .cancel) { pendingIdentityRemoval = nil }
        } message: { pending in
            Text(String(localized: "This removes \(pending.identity.email) as a send-as identity for this account. This cannot be undone."))
        }
    }

    private func applyPendingSelection() {
        guard let target = model.settingsSelectedAccountId else { return }
        selection = target
        model.settingsSelectedAccountId = nil
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "person.crop.circle.badge.questionmark")
                .font(.system(size: 28))
                .foregroundStyle(.tertiary)
            Text("No accounts yet.")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    /// Opens the Signatures window (GUI-4). Signatures live in their own
    /// window so editing never covers this account list; see DECISIONS.md
    /// 2026-09-30 and `SignatureEditorView`.
    private func signatureEditor(for account: Account) -> some View {
        HStack {
            let count = prefs.signatureLibrary.items(for: account.id).count
            let defaultName = prefs.signatureLibrary.defaultItem(for: account.id)?.name ?? ""
            // "2 signatures · Default: Work": which one new
            // messages start with, without opening the window.
            Text(count == 0 ? String(localized: "No signature")
                 : String(localized: "\(count) signatures") + " · " + String(localized: "Default: \(defaultName)"))
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.tail)
            Spacer()
            Button("Signatures…") {
                SignatureEditorWindowController.show(prefs: prefs, model: model, accountId: account.id)
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }

    /// Per-account Markdown compose default toggle (S46), Settings > Accounts.
    private func markdownDefaultToggle(for account: Account) -> some View {
        Toggle(
            String(localized: "New messages from \(account.email) start as Markdown"),
            isOn: Binding(
                get: { prefs.markdownDefault(for: account.id) },
                set: { prefs.setMarkdownDefault($0, for: account.id) }
            )
        )
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }

    /// Identities/aliases editor for `account` (S47), Settings > Accounts.
    private func identitiesEditor(for account: Account) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Identities for \(account.email)")
                .font(.headline)
            ForEach(model.identities[account.id] ?? []) { identity in
                HStack {
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 6) {
                            Text(identity.displayName.map { "\($0) <\(identity.email)>" } ?? identity.email)
                            if identity.isDefault {
                                Text("default").font(.caption2).padding(.horizontal, 5)
                                    .background(Capsule().fill(Color.secondary.opacity(0.2)))
                            }
                        }
                        let extras = [identity.replyTo.map { "Reply-To: \($0)" }, identity.bcc.map { "Bcc: \($0)" }]
                            .compactMap { $0 }.joined(separator: " · ")
                        if !extras.isEmpty {
                            Text(extras).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    Button("Edit…") { pendingEditIdentity = identity }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                    Button("Remove", role: .destructive) {
                        pendingIdentityRemoval = (accountId: account.id, identity: identity)
                    }
                    .buttonStyle(.bordered)
                    .foregroundStyle(.red)
                    .controlSize(.small)
                }
            }
            Button("Add Identity…") { pendingNewIdentityAccountId = account.id }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
        .onAppear { model.reloadIdentities(account.id) }
        .sheet(isPresented: Binding(
            get: { pendingNewIdentityAccountId == account.id },
            set: { if !$0 { pendingNewIdentityAccountId = nil } }
        )) {
            IdentitySheet(accountId: account.id, existing: nil, model: model) { pendingNewIdentityAccountId = nil }
        }
        .sheet(item: Binding(
            get: { pendingEditIdentity.flatMap { $0.accountId == account.id ? $0 : nil } },
            set: { pendingEditIdentity = $0 }
        )) { identity in
            IdentitySheet(accountId: account.id, existing: identity, model: model) { pendingEditIdentity = nil }
        }
    }

    /// Six buttons when they fit, otherwise the compact bar with a More menu
    /// (the six-button row is wider than the 480 pt minimum window, worse in es/ca).
    private var toolbar: some View {
        let hasSelection = selection != nil
        return ViewThatFits(in: .horizontal) {
            AccountsActionBar(compact: false, hasSelection: hasSelection, onAction: accountAction)
                .fixedSize(horizontal: true, vertical: false)
            AccountsActionBar(compact: true, hasSelection: hasSelection, onAction: accountAction)
        }
    }

    private func accountAction(_ action: AccountsActionBar.Action) {
        switch action {
        case .add: sheetMode = .add
        case .reauthenticate: if let selection { sheetMode = .reauthenticate(selection) }
        case .folders: if let selection { sheetMode = .folders(selection) }
        case .duplicate: if let selection { sheetMode = .duplicate(selection) }
        case .reset: if let selection { pendingReset = model.accounts.first { $0.id == selection } }
        case .remove: pendingRemoval = model.accounts.first { $0.id == selection }
        }
    }

    /// WS1 (, DECISIONS.md 2026-10-05): leave evaluation mode for
    /// every read-only account in one confirmed step. Deliberately a separate
    /// line from the per-account controls, and it only ASKS: the dialog that
    /// lists the accounts (`WriteSafetyPresenter`) is where it is confirmed.
    private var writePolicyFooter: some View {
        let readOnly = model.accountsReadOnly.count
        return HStack(spacing: 8) {
            Image(systemName: readOnly == 0 ? "lock.open" : "lock")
                .foregroundStyle(.secondary)
            Text(readOnly == 0
                 ? String(localized: "Writes are enabled for every account.")
                 : String(localized: "\(readOnly) of \(model.accounts.count) accounts read-only (evaluation mode)."))
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
            Button(AccountWritePolicy.enableAllButton) {
                model.requestEnableWritesForAllAccounts()
            }
            .disabled(readOnly == 0 || model.isChangingWritePolicy)
            .help(AccountWritePolicy.enableAllHelp)
            #if DEBUG
            .uiProbe("enableWritesForAll", label: AccountWritePolicy.enableAllButton,
                     enabled: readOnly != 0 && !model.isChangingWritePolicy) {
                model.requestEnableWritesForAllAccounts()
            }
            #endif
        }
    }

    /// Keychain-backed config per account, for the IMAP/SMTP endpoint line
    ///. Same synchronous-decode idiom as
    /// `AccountSetupSheet.prefill` — small, per-account reads on the main
    /// actor, not the background `Task.detached` diagnostics need.
    private func loadConfigs() {
        configsByAccount = Dictionary(
            uniqueKeysWithValues: model.accounts.compactMap { account -> (String, AccountConfig)? in
                guard let json = model.storedConfig(for: account.id),
                      let config = try? AccountConfig.decode(fromJSON: json)
                else { return nil }
                return (account.id, config)
            }
        )
    }

    /// Best-effort: an unreachable/erroring core just leaves the tab without
    /// "last sync" detail rather than failing the whole Accounts tab over it.
    private func loadDiagnostics() async {
        let core = model.core
        let accounts = (try? await Task.detached { try core.localDiagnostics() }.value)?.accounts ?? []
        diagnosticsByAccount = Dictionary(uniqueKeysWithValues: accounts.map { ($0.accountId, $0) })
    }
}

/// The Accounts pane's action buttons. `compact` folds Duplicate, Reset and
/// Remove into a More menu (destructive roles kept) so the bar fits 460 pt.
struct AccountsActionBar: View {
    enum Action { case add, reauthenticate, folders, duplicate, reset, remove }
    let compact: Bool
    let hasSelection: Bool
    let onAction: (Action) -> Void

    var body: some View {
        HStack {
            Button("Add…") { onAction(.add) }
            Spacer()
            Button("Re-authenticate…") { onAction(.reauthenticate) }
                .disabled(!hasSelection)
            Button("Folders…") { onAction(.folders) }
                .disabled(!hasSelection)
                .help("Choose which folders this account syncs.")
            if compact {
                Menu("More") {
                    Button("Duplicate…") { onAction(.duplicate) }
                    Divider()
                    Button("Reset…", role: .destructive) { onAction(.reset) }
                    Button("Remove…", role: .destructive) { onAction(.remove) }
                }
                .fixedSize()
                .disabled(!hasSelection)
            } else {
                Button("Duplicate…") { onAction(.duplicate) }
                    .disabled(!hasSelection)

                Divider().frame(height: 16)

                // Destructive actions: same role: .destructive + red foreground
                // pattern used for "Delete Project" (ProjectDetailView) and
                // "Delete" (SidebarView) — visually distinct from the safe actions
                // above, even before their confirmation alert fires.
                Button("Reset…", role: .destructive) { onAction(.reset) }
                    .foregroundStyle(.red)
                    .disabled(!hasSelection)
                Button("Remove…", role: .destructive) { onAction(.remove) }
                    .foregroundStyle(.red)
                    .disabled(!hasSelection)
            }
        }
    }
}

/// One row: email + provider chip + write-policy badge, and a last-sync
/// summary line mirroring the wording used in the Diagnostics window.
private struct AccountRow: View {
    let account: Account
    let writeState: AccountWriteState
    let diagnostics: LocalAccountDiagnostics?
    /// The model's CURRENT sync status; a failure here beats the (load-once)
    /// diagnostics so a failed Save anyway never reads "Last synced".
    let status: AccountSyncStatus?
    /// Stored (Keychain) config, for the IMAP/SMTP endpoint line. `nil` when
    /// there is no readable config yet (e.g. mid-load).
    let config: AccountConfig?
    /// True while a policy change is in flight (the control disables itself).
    let isChangingWritePolicy: Bool
    /// Ask to change this account's write policy (WS1). It only asks: the
    /// confirmation dialog is where the change is made. The badge alone used
    /// to sit here looking clickable, and was not.
    let onWritePolicy: () -> Void

    private var writePolicyButtonTitle: String {
        writeState.isReadOnly
            ? String(localized: "Enable Writes…")
            : String(localized: "Return to Read-only…")
    }

    var body: some View {
        HStack(alignment: .center, spacing: 8) {
            summary
            Spacer(minLength: 8)
            Button(writePolicyButtonTitle, action: onWritePolicy)
                .controlSize(.small)
                .disabled(isChangingWritePolicy)
                .help(writeState.isReadOnly
                      ? AccountWritePolicy.badgeHelp(for: account.email)
                      : AccountWritePolicy.writesEnabledHelp)
                .accessibilityLabel(Text(verbatim: "\(account.email): \(writePolicyButtonTitle)"))
                #if DEBUG
                .uiProbe("writePolicy:\(account.email)", label: writePolicyButtonTitle,
                         enabled: !isChangingWritePolicy, action: onWritePolicy)
                #endif
        }
    }

    private var summary: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Text(account.email)
                Text(account.provider.uppercased())
                    .font(.caption2)
                    .fontWeight(.semibold)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Capsule().fill(Color.secondary.opacity(0.18)))
                    .foregroundStyle(.secondary)
                if writeState.isReadOnly {
                    Label(AccountWritePolicy.badgeLabel, systemImage: "lock")
                        .labelStyle(.titleAndIcon)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                } else {
                    Label(AccountWritePolicy.writesEnabledLabel, systemImage: "lock.open")
                        .labelStyle(.titleAndIcon)
                        .font(.caption2)
                        .foregroundStyle(Color.accentColor)
                }
            }
            if let config {
                Text("IMAP \(config.imapEndpoint) · SMTP \(config.effectiveSMTP.endpoint)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Text(lastSyncSummary)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
        #if DEBUG
        // UIT-1: expose the sync line to scenarios (SwiftUI text is not an NSView).
        .uiProbe("accountSyncLine", role: "staticText", label: lastSyncSummary, action: nil)
        #endif
    }

    private var lastSyncSummary: String {
        AccountRowSummary.text(status: status, diagnostics: diagnostics)
    }
}

/// "Add Identity" sheet (S47) — a bare email + optional display name,
/// client-validated with the same `AddressParser.isValidEmail` compose uses.
/// Add or edit an identity (S47, extended by MM-6 with Reply-To, an automatic
/// Bcc and the "default for new mail" flag). The email itself is immutable.
private struct IdentitySheet: View {
    let accountId: String
    let existing: Identity?
    @ObservedObject var model: AppModel
    var onDone: () -> Void

    @State private var email: String
    @State private var displayName: String
    @State private var replyTo: String
    @State private var bcc: String
    @State private var isDefault: Bool
    @State private var errorText: String?

    init(accountId: String, existing: Identity?, model: AppModel, onDone: @escaping () -> Void) {
        self.accountId = accountId
        self.existing = existing
        self.model = model
        self.onDone = onDone
        _email = State(initialValue: existing?.email ?? "")
        _displayName = State(initialValue: existing?.displayName ?? "")
        _replyTo = State(initialValue: existing?.replyTo ?? "")
        _bcc = State(initialValue: existing?.bcc ?? "")
        _isDefault = State(initialValue: existing?.isDefault ?? false)
    }

    private var canSave: Bool {
        AddressParser.isValidEmail(email)
            && IdentityAddressField.isAcceptable(replyTo)
            && IdentityAddressField.isAcceptable(bcc)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(existing == nil ? "Add Identity" : "Edit Identity").font(.headline)
            // LabeledContent keeps each label visible once the field is filled
            // (a TextField title in a plain stack is never drawn); the example
            // addresses stay as prompts and the accessibility label is the title.
            LabeledContent("Email") {
                TextField("Email", text: $email, prompt: Text("alias@example.com"))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .accessibilityLabel(Text("Email"))
                    .disabled(existing != nil)
            }
            LabeledContent("Display Name (optional)") {
                TextField("Display Name (optional)", text: $displayName)
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .accessibilityLabel(Text("Display Name (optional)"))
            }
            LabeledContent("Reply-To (optional)") {
                TextField("Reply-To (optional)", text: $replyTo, prompt: Text("replies@example.com"))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .accessibilityLabel(Text("Reply-To (optional)"))
            }
            LabeledContent("Always Bcc (optional)") {
                TextField("Always Bcc (optional)", text: $bcc, prompt: Text("archive@example.com"))
                    .textFieldStyle(.roundedBorder)
                    .labelsHidden()
                    .accessibilityLabel(Text("Always Bcc (optional)"))
            }
            Toggle("Use for new messages from this account", isOn: $isDefault)
            Text("Replies use the address the message was sent to, when it is one of your identities. Reply-To and Bcc apply to every message sent from this identity; the Bcc is never shown in the headers.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let errorText {
                Text(errorText).font(.caption).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel") { onDone() }
                Button(existing == nil ? "Add" : "Save") { save() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(!canSave)
            }
        }
        .padding(20)
        .frame(minWidth: 380)
    }

    private func save() {
        let name = displayName.trimmingCharacters(in: .whitespaces)
        if let existing {
            model.updateIdentity(
                existing, displayName: name, replyTo: replyTo, bcc: bcc, isDefault: isDefault
            ) { failure in
                if let failure { errorText = failure } else { onDone() }
            }
        } else {
            model.createIdentity(
                accountId: accountId, email: email, displayName: name.isEmpty ? nil : name,
                replyTo: replyTo.isEmpty ? nil : replyTo, bcc: bcc.isEmpty ? nil : bcc,
                isDefault: isDefault
            ) { failure in
                if let failure { errorText = failure } else { onDone() }
            }
        }
    }
}
