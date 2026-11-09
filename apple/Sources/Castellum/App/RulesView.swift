// Castellum - Settings > Rules: ordered client-side rules.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The engine lives in Rust (castellum-core rules.rs): rules run after each
// sync in the order shown here, a "stop processing" rule consumes the
// messages it matched, and every action goes through the operation queue —
// so it is logged, replayable and held while an account is read-only.
// This file is the list (drag to reorder, enable switch, Run now) and the
// editor sheet (conditions from MM-1's FilterGroupEditor, actions, scope,
// dry-run preview).

import SwiftUI

struct RulesSettingsTab: View {
    @ObservedObject var model: AppModel
    @State private var selection: String?
    @State private var editing: RuleDraft?
    @State private var pendingDelete: SavedRule?
    @State private var runNotice: String?
    /// Run Now waits for an explicit OK that names how many messages it will touch.
    @State private var pendingRun: (rule: SavedRule, count: Int)?

    private func accountLabel(_ id: String?) -> String {
        guard let id else { return String(localized: "All accounts") }
        return model.accounts.first { $0.id == id }?.email ?? id
    }

    private func mailboxName(_ id: String) -> String? {
        model.mailboxes.values.flatMap { $0 }.first { $0.id == id }?.name
    }

    /// The rule's Move folder that no longer exists, only judged once that
    /// account's folder list has loaded (an empty cache proves nothing).
    private func missingFolder(_ rule: SavedRule) -> String? {
        guard let account = rule.accountId, let boxes = model.mailboxes[account], !boxes.isEmpty else { return nil }
        return RuleDraft(rule).missingMoveFolder(mailboxIds: boxes.map(\.id))
    }

    private var brokenRuleNames: [String] {
        model.rules.filter { missingFolder($0) != nil }.map(\.name)
    }

    private var readOnlyAccountsWithRules: [String] {
        let scoped = Set(model.rules.filter(\.enabled).compactMap(\.accountId))
        let ids = model.rules.contains { $0.enabled && $0.accountId == nil }
            ? Set(model.accounts.map(\.id)) : scoped
        return ids.filter { model.writeState(for: $0) == .readOnly }
            .compactMap { id in model.accounts.first { $0.id == id }?.email }
            .sorted()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Rules run in this order after each sync. A rule marked “stop” takes the messages it matched away from the rules below it.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if !readOnlyAccountsWithRules.isEmpty {
                Label("Read-only accounts (\(readOnlyAccountsWithRules.joined(separator: ", "))): rules cannot change mail there until writes are enabled for the account.",
                      systemImage: "lock")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            if !brokenRuleNames.isEmpty {
                Label("A Move folder no longer exists, so these rules cannot file mail: \(brokenRuleNames.joined(separator: ", ")). Edit the rule and choose another folder.",
                      systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            List(selection: $selection) {
                ForEach(model.rules) { rule in
                    RuleRow(rule: rule, scope: accountLabel(rule.accountId), broken: missingFolder(rule) != nil,
                            mailboxName: { id in
                                (rule.accountId.flatMap { model.mailboxes[$0] } ?? model.mailboxes.values.flatMap { $0 })
                                    .first { $0.id == id }?.name
                            },
                            onToggle: { model.setRule(rule, enabled: $0) })
                        .tag(rule.id)
                        .contextMenu {
                            Button("Edit…") { editing = RuleDraft(rule) }
                            Button("Run Now") { runNow(rule) }
                                .disabled(model.ruleRunProgress != nil)
                            if model.ruleContinuation?.ruleId == rule.id {
                                Button("Continue") { continueRun(rule) }
                                    .disabled(model.ruleRunProgress != nil)
                            }
                            Button("Delete…", role: .destructive) { pendingDelete = rule }
                        }
                }
                .onMove { model.moveRules(fromOffsets: $0, toOffset: $1) }
            }
            .listStyle(.inset(alternatesRowBackgrounds: true))
            .frame(minHeight: 160)
            .overlay {
                if model.rules.isEmpty {
                    Text("No rules yet. Add one to sort, flag or file mail automatically.")
                        .foregroundStyle(.secondary)
                }
            }
            if let progress = model.ruleRunProgress {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Running “\(progress.ruleName)”: \(progress.processed) messages checked, \(progress.queued) actions queued…")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Cancel") { model.cancelRuleRun() }
                        .controlSize(.small)
                }
            } else if let runNotice {
                Text(runNotice).font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                Button("Add…") { editing = RuleDraft() }
                Button("Edit…") {
                    if let rule = model.rules.first(where: { $0.id == selection }) { editing = RuleDraft(rule) }
                }
                .disabled(selection == nil)
                Button("Run Now") {
                    if let rule = model.rules.first(where: { $0.id == selection }) { runNow(rule) }
                }
                .disabled(selection == nil || model.ruleRunProgress != nil)
                .help("Apply the selected rule to everything it currently matches")
                if let rule = model.rules.first(where: { $0.id == selection }),
                   model.ruleContinuation?.ruleId == rule.id {
                    Button("Continue") { continueRun(rule) }
                        .disabled(model.ruleRunProgress != nil)
                        .help("Carry on from where the last run stopped. Run Now starts again from the first message.")
                }
                Spacer()
                Button("Delete…", role: .destructive) {
                    pendingDelete = model.rules.first { $0.id == selection }
                }
                .disabled(selection == nil)
            }
        }
        .padding(16)
        .onAppear { model.reloadRules() }
        .sheet(item: $editing) { draft in
            RuleEditorSheet(model: model, initial: draft)
        }
        .alert("Run “\(pendingRun?.rule.name ?? "")” now?", isPresented: Binding(
            get: { pendingRun != nil }, set: { if !$0 { pendingRun = nil } })
        ) {
            Button("Run") {
                if let run = pendingRun { confirmedRun(run.rule) }
                pendingRun = nil
            }
            Button("Cancel", role: .cancel) { pendingRun = nil }
        } message: {
            if let run = pendingRun {
                Text(Self.runConfirmation(
                    actions: RuleDraft(run.rule).actionsInRunOrder.map { $0.label(mailboxName: mailboxName) }
                        .joined(separator: ", ").lowercased(),
                    count: run.count))
            }
        }
        .alert("Delete “\(pendingDelete?.name ?? "")”?", isPresented: Binding(
            get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } })
        ) {
            Button("Delete", role: .destructive) {
                if let rule = pendingDelete { model.deleteRule(rule.id) }
                pendingDelete = nil
            }
            Button("Cancel", role: .cancel) { pendingDelete = nil }
        } message: {
            Text("The rule stops running. Mail it already changed stays as it is.")
        }
    }

    /// Dry-run first, then ask: Run Now can archive or trash everything a rule
    /// matches, so it never fires from one click without a count.
    private func runNow(_ rule: SavedRule) {
        runNotice = nil
        model.previewRuleMatchCount(rule) { result in
            switch result {
            case .success(0): runNotice = "“\(rule.name)” matches no current messages."
            case .success(let count): pendingRun = (rule, count)
            case .failure(let error): runNotice = "Could not check “\(rule.name)”: \(error)"
            }
        }
    }

    /// Continue: resume the rule's unfinished run from the stored cursor
    /// (the count was confirmed when the run began).
    private func continueRun(_ rule: SavedRule) {
        runNotice = nil
        model.runRuleNow(rule, continuing: true) { runNotice = "“\(rule.name)”: \($0)" }
    }

    /// The Run Now confirmation text. Above the per-run cap it says "N+"
    /// is not what runs: only the first `RuleRunNow.maxMessages` are
    /// processed, and running it again continues.
    static func runConfirmation(actions: String, count: Int) -> String {
        let cap = RuleRunNow.maxMessages
        let scope = count > cap
            ? "the first \(cap.formatted()) of the \(count.formatted()) current messages it matches (\(cap.formatted())+ in one run; "
                + String(localized: "choose Continue afterwards for the rest") + ")"
            : String(localized: "\(count) current messages")
        return "This will \(actions) for \(scope). Mail in Trash, Junk, Drafts and Sent is never touched."
    }

    private func confirmedRun(_ rule: SavedRule) {
        model.runRuleNow(rule) { runNotice = "“\(rule.name)”: \($0)" }
    }
}

private struct RuleRow: View {
    let rule: SavedRule
    let scope: String
    var broken = false
    let mailboxName: (String) -> String?
    let onToggle: (Bool) -> Void

    var body: some View {
        HStack(spacing: 8) {
            Toggle("", isOn: Binding(get: { rule.enabled }, set: onToggle))
                .labelsHidden()
                .help(rule.enabled ? "Enabled" : "Disabled")
            if broken {
                Image(systemName: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
                    .help("This rule's Move folder no longer exists, so it cannot file mail.")
            }
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 6) {
                    Text(rule.name).foregroundStyle(rule.enabled ? .primary : .secondary)
                    if rule.stopProcessing {
                        Text("stop").font(.caption2).padding(.horizontal, 5)
                            .background(Capsule().fill(Color.secondary.opacity(0.2)))
                    }
                    if rule.trigger == .manual {
                        Text("manual").font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Text("\(scope) → " + RuleDraft(rule).actionsInRunOrder
                        .map { $0.label(mailboxName: mailboxName) }.joined(separator: ", "))
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
        }
    }
}

struct RuleEditorSheet: View {
    @ObservedObject var model: AppModel
    @State private var draft: RuleDraft
    @Environment(\.dismiss) private var dismiss
    @State private var preview: PreviewState = .idle
    @State private var saving = false
    @FocusState private var nameFocused: Bool

    enum PreviewState { case idle, running, done(RuleConditionsPreviewDTO), failed(String) }

    init(model: AppModel, initial: RuleDraft) {
        self.model = model
        _draft = State(initialValue: initial)
    }

    private var mailboxOptions: [Mailbox] {
        guard let account = draft.accountId else { return [] }
        return (model.mailboxes[account] ?? []).sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }

    private func mailboxName(_ id: String) -> String? {
        let scoped = draft.accountId.flatMap { model.mailboxes[$0] } ?? model.mailboxes.values.flatMap { $0 }
        return scoped.first { $0.id == id }?.name
    }

    /// Height of the form content, so the scroll area fits it when it can.
    @State private var contentHeight: CGFloat = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(draft.id == nil ? "New Rule" : "Edit Rule").font(.headline)
            ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    VStack(alignment: .leading, spacing: 2) {
                        TextField("Name (required)", text: $draft.name)
                            .textFieldStyle(.roundedBorder)
                            .focused($nameFocused)
                            .overlay(RoundedRectangle(cornerRadius: 5)
                                .stroke(Color.red, lineWidth: draft.nameIsMissing ? 1 : 0))
                            .accessibilityIdentifier("ruleName")
                        if draft.nameIsMissing {
                            Text("A rule needs a name before it can be saved.")
                                .font(.caption).foregroundStyle(.red)
                        }
                    }
                    HStack {
                        Picker("Apply to", selection: $draft.accountId) {
                            Text("All accounts").tag(nil as String?)
                            ForEach(model.accounts) { Text($0.email).tag($0.id as String?) }
                        }
                        .onChange(of: draft.accountId) { _ in
                            // A Move folder belongs to one account: drop the ones the new scope cannot use.
                            let foreign = Set(draft.moveFoldersOfOtherAccounts)
                            if draft.accountId != nil, !foreign.isEmpty {
                                draft.actions.removeAll { if case .move(let id) = $0 { return foreign.contains(id) } else { return false } }
                            }
                        }
                        Picker("When", selection: $draft.trigger) {
                            ForEach(RuleTriggerDTO.allCases) { Text($0.label).tag($0) }
                        }
                    }
                    GroupBox("If") {
                        // MM-1c: Header is not offered for new rule rows until slice 3. The core already
                        // defers an on-sync Header rule until the message's headers are captured (body
                        // hydrated), but without slice 3's header fetch that can be much later than the
                        // user expects. A rule that already holds a Header row still shows and edits it.
                        FilterGroupEditor(group: $draft.group, fields: FilterFieldKind.editable.filter { $0 != .header })
                            .padding(6)
                    }
                    GroupBox("Then") { actionsEditor.padding(6) }
                    Toggle("Stop processing more rules for messages this rule matches", isOn: $draft.stopProcessing)
                        #if DEBUG
                        .uiProbe("stopProcessing", role: "checkbox", label: "Stop processing")
                        #endif
                    Toggle("Enabled", isOn: $draft.enabled)
                        #if DEBUG
                        .uiProbe("ruleEnabled", role: "checkbox", label: "Enabled")
                        #endif
                    previewSection
                }
                .padding(.trailing, 4)
                .background(GeometryReader { proxy in
                    Color.clear.preference(key: RuleEditorHeightKey.self, value: proxy.size.height)
                })
            }
            .onPreferenceChange(RuleEditorHeightKey.self) { contentHeight = $0 }
            .frame(height: min(max(contentHeight, 260), 460))
            if let problem = draft.validationProblem {
                // Says why Save is greyed out (#3146), not just a faint hint.
                Label(problem, systemImage: "info.circle")
                    .font(.caption).foregroundStyle(.orange)
            }
            if let error = model.ruleSaveError {
                Text(error).font(.caption).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                Button("Save") { save() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!draft.isValid || saving)
                    .help(draft.validationProblem ?? "")
            }
        }
        .padding(20)
        .frame(width: 640)
        .onAppear { if draft.nameIsMissing { nameFocused = true } }
        .onDisappear { model.ruleSaveError = nil }
    }

    @ViewBuilder
    private var actionsEditor: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let missing = draft.missingMoveFolder(mailboxIds: mailboxOptions.map(\.id)), !mailboxOptions.isEmpty {
                Label("A Move folder no longer exists: remove that action and choose another folder.",
                      systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.orange)
                    .help(missing)
            }
            ForEach(Array(draft.actions.enumerated()), id: \.offset) { index, action in
                HStack {
                    Text(action.label(mailboxName: mailboxName))
                    Spacer()
                    Button { draft.actions.remove(at: index) } label: { Image(systemName: "minus.circle") }
                        .buttonStyle(.borderless)
                        .help("Remove this action")
                }
            }
            Menu("Add Action") {
                Button("Mark as read") { add(.markRead) }
                Button("Mark as unread") { add(.markUnread) }
                Button("Star") { add(.addStar) }
                Button("Remove star") { add(.removeStar) }
                if !model.tags.isEmpty {
                    Menu("Add tag…") {
                        ForEach(model.tags) { tag in
                            Button(tag.displayName) { add(.addTag(keyword: tag.keyword)) }
                        }
                    }
                    Menu("Remove tag…") {
                        ForEach(model.tags) { tag in
                            Button(tag.displayName) { add(.removeTag(keyword: tag.keyword)) }
                        }
                    }
                }
                Divider()
                Button("Archive") { add(.archive) }
                Button("Move to Trash") { add(.trash) }
                if draft.canOfferMove {
                    Menu("Move to…") {
                        ForEach(mailboxOptions) { mailbox in
                            Button(mailbox.syncByDefault == false
                                   ? mailbox.name + " " + String(localized: "— not synced") : mailbox.name) {
                                add(.move(mailboxId: mailbox.id))
                            }
                        }
                    }
                }
            }
            .fixedSize()
            .frame(maxWidth: .infinity, alignment: .leading)
            if !draft.canOfferMove {
                Text("Choose a single account under Apply to for “Move to…”.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Text("Actions that move the message run last, one per match. Each action is queued like a manual one, so it is logged, retried, and held while an account is read-only.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder
    private var previewSection: some View {
        HStack {
            Button("Preview Matches") { runPreview() }
                .disabled(!draft.group.isValidForSave)
                #if DEBUG
                .uiProbe("previewMatches", label: "Preview Matches")
                #endif
            switch preview {
            case .idle: EmptyView()
            case .running: ProgressView().controlSize(.small)
            case .done(let preview):
                Text(preview.total == 1
                 ? String(localized: "1 current message matches (rules never touch Trash, Junk, Drafts or Sent)")
                 : String(localized: "\(preview.total.formatted()) current messages match (rules never touch Trash, Junk, Drafts or Sent)"))
                    .font(.caption).foregroundStyle(.secondary)
            case .failed(let message):
                Text(message).font(.caption).foregroundStyle(.red)
            }
        }
        if case .done(let result) = preview, !result.matches.isEmpty {
            VStack(alignment: .leading, spacing: 2) {
                ForEach(result.matches.prefix(8)) { header in
                    Text("\(header.fromName ?? header.fromAddress ?? "?") — \(header.subject ?? "(no subject)")")
                        .font(.caption).lineLimit(1)
                }
                if result.total > 8 {
                    Text("… and \((result.total - 8).formatted()) more").font(.caption).foregroundStyle(.secondary)
                }
            }
            Text("A dry run: nothing has been changed or queued.").font(.caption2).foregroundStyle(.secondary)
        }
    }

    private func add(_ action: RuleActionDTO) {
        guard !draft.actions.contains(action) else { return }
        draft.actions.append(action)
    }

    private func runPreview() {
        preview = .running
        model.previewRuleConditions(draft.group, accountId: draft.accountId) { result in
            switch result {
            case .success(let result): preview = .done(result)
            case .failure(let error): preview = .failed("Could not preview: \(error)")
            }
        }
    }

    private func save() {
        saving = true
        model.saveRule(draft) { ok in
            saving = false
            if ok { dismiss() }
        }
    }
}

/// Carries the rule editor's content height up to its scroll view.
private struct RuleEditorHeightKey: PreferenceKey {
    static var defaultValue: CGFloat = 0
    static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}
