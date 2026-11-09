// Castellum - the per-account reply-attribution editor and its placeholder help
//. Lives in Settings > Accounts under the signature row.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// The popover that lists every placeholder (from `TemplateFormatter.placeholders`,
/// the same table `project-docs/template-placeholders.md` is checked against).
struct TemplatePlaceholderHelp: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 6) {
                Text("Placeholders").font(.headline)
                ForEach(TemplateFormatter.placeholders, id: \.name) { doc in
                    VStack(alignment: .leading, spacing: 1) {
                        Text("${\(doc.name)}").font(.system(.caption, design: .monospaced)).bold()
                        Text(doc.meaning).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Text("Month and day names follow your Mac's language. In a Catalan or Spanish line on an English Mac, use a numeric date such as ${date:d/M/yyyy}. Write $${ for a literal ${.")
                    .font(.caption2).foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(12)
        }
        .frame(width: 340, height: 360)
    }
}

/// "Reply attribution" row for one account: a format field with a live preview,
/// problems listed in red, and a help popover. Blank = the built-in line.
struct ReplyAttributionEditor: View {
    let account: Account
    @ObservedObject var prefs: Preferences
    @State private var showHelp = false

    /// Fixed sample so the preview never depends on real mail.
    static func sampleContext(for account: Account) -> TemplateContext {
        TemplateContext(
            senderName: "Ada Lovelace", senderEmail: "ada@example.org",
            sentAt: 1_792_143_000, subject: "Lunch?",
            recipientName: "Grace Hopper", recipientEmail: "grace@example.org",
            myName: account.displayName, myEmail: account.email,
            now: Date(timeIntervalSince1970: 1_792_143_000),
            locale: .current, timeZone: .current)
    }

    private var format: Binding<String> {
        Binding(
            get: { prefs.templateLibrary.attribution(for: account.id) ?? "" },
            set: { prefs.templateLibrary.setAttribution($0, for: account.id) })
    }

    var body: some View {
        let current = prefs.templateLibrary.attribution(for: account.id) ?? ""
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text("Reply attribution").font(.caption).foregroundStyle(.secondary)
                Button { showHelp.toggle() } label: { Image(systemName: "questionmark.circle") }
                    .buttonStyle(.plain)
                    .popover(isPresented: $showHelp) { TemplatePlaceholderHelp() }
                    .accessibilityLabel(String(localized: "Placeholder help"))
            }
            TextField("On ${date}, ${sender} wrote:", text: format)
                .textFieldStyle(.roundedBorder)
                .font(.callout)
            if !current.isEmpty {
                let rendered = TemplateFormatter.render(
                    current, context: Self.sampleContext(for: account), mode: .singleLine).text
                Text(rendered.isEmpty ? String(localized: "(no attribution line)") : rendered)
                    .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                ForEach(Array(TemplateFormatter.problems(in: current, mode: .singleLine).enumerated()), id: \.offset) { _, p in
                    Text("\(p.message): \(p.source)").font(.caption).foregroundStyle(.red)
                }
            }
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }
}
