// Castellum - the per-account forward header format (MM-7, slice 4).
//
// Lives in the Snippets window, not in Settings > Accounts: that pane is at its
// layout-test height limit. A custom format replaces only the From/Date/Subject lines
// under the fixed "Forwarded message" separator; blank keeps the built-in block.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct ForwardHeaderEditor: View {
    @ObservedObject var model: AppModel
    @ObservedObject var prefs: Preferences
    @State private var accountId: String?
    @State private var showHelp = false

    init(model: AppModel, prefs: Preferences) {
        self.model = model
        self.prefs = prefs
        _accountId = State(initialValue: model.accounts.first?.id)
    }

    private var account: Account? { model.accounts.first { $0.id == accountId } }

    /// The built-in block, shown as the field's placeholder.
    static let builtInExample = "From: ${sender.name} <${sender.email}>\nDate: ${date}\nSubject: ${subject}"

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                Text("Forward header").font(.caption).foregroundStyle(.secondary)
                Picker("Account", selection: Binding(get: { accountId }, set: { accountId = $0 })) {
                    ForEach(model.accounts) { Text($0.email).tag(String?.some($0.id)) }
                }
                .labelsHidden()
                .fixedSize()
                .disabled(model.accounts.isEmpty)
                Spacer()
                Button { showHelp.toggle() } label: { Image(systemName: "questionmark.circle") }
                    .buttonStyle(.plain)
                    .popover(isPresented: $showHelp) { TemplatePlaceholderHelp() }
                    .accessibilityLabel(String(localized: "Placeholder help"))
            }
            if let account {
                let current = prefs.templateLibrary.forwardHeaderFormat(for: account.id) ?? ""
                TextEditor(text: Binding(
                    get: { current },
                    set: { prefs.templateLibrary.setForwardHeader($0, for: account.id) }))
                    .font(.callout)
                    .frame(height: 54)
                    .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.secondary.opacity(0.25)))
                    .overlay(alignment: .topLeading) {
                        if current.isEmpty {
                            Text(Self.builtInExample).font(.callout).foregroundStyle(.tertiary)
                                .padding(.horizontal, 5).padding(.vertical, 8).allowsHitTesting(false)
                        }
                    }
                if !current.isEmpty {
                    let rendered = TemplateFormatter.render(
                        current, context: ReplyAttributionEditor.sampleContext(for: account), mode: .multiLine).text
                    Text(rendered.isEmpty ? String(localized: "(no header lines)") : rendered)
                        .font(.caption).foregroundStyle(.secondary).lineLimit(4)
                    ForEach(Array(TemplateFormatter.problems(in: current, mode: .multiLine).enumerated()), id: \.offset) { _, p in
                        Text("\(p.message): \(p.source)").font(.caption).foregroundStyle(.red)
                    }
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}
