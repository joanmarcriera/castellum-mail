// Castellum - recursive all/any/none condition editor.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A thin shell over `FilterGroupDTO` (Core/FilterGroupModel.swift), which
// owns every rule (operators per field, blank rows, limits). One group row:
// "Match [all|any|none] of the following" with + condition / + group / remove
// buttons, then its condition rows, then its nested groups, indented. Nested
// groups recurse; "add group" stops at `FilterGroupDTO.maxEditorDepth`.

import SwiftUI

struct FilterGroupEditor: View {
    @Binding var group: FilterGroupDTO
    var depth: Int = 1
    /// The fields the field picker offers. Defaults to every editable kind; the
    /// Rules sheet passes the list without `.header` until rules can use it. A
    /// row whose field is not in the list still shows it (`pickerOptions`).
    var fields: [FilterFieldKind] = FilterFieldKind.editable
    /// MM-1c: header-name rules and completion; nil until the core answered.
    var headerPolicy: FilterHeaderPolicy?
    /// nil for the root group (it cannot be removed).
    var onRemove: (() -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Text("Match")
                Picker("", selection: $group.op) {
                    ForEach(FilterBoolOp.allCases) { Text($0.label).tag($0) }
                }
                .labelsHidden()
                .fixedSize()
                Text("of the following")
                Spacer()
                Button {
                    group.conditions.append(.blank(field: .subject))
                } label: { Image(systemName: "plus") }
                .buttonStyle(.borderless)
                .help("Add a condition")
                Button {
                    group.groups.append(FilterGroupDTO(op: .or, conditions: [.blank(field: .subject)]))
                } label: { Image(systemName: "plus.rectangle.on.rectangle") }
                .buttonStyle(.borderless)
                .help("Add a nested group")
                .disabled(depth >= FilterGroupDTO.maxEditorDepth)
                if let onRemove {
                    Button(action: onRemove) { Image(systemName: "trash") }
                        .buttonStyle(.borderless)
                        .help("Remove this group")
                }
            }
            ForEach($group.conditions) { $condition in
                FilterConditionRow(condition: $condition, fields: fields, headerPolicy: headerPolicy) {
                    let id = condition.id
                    group.conditions.removeAll { $0.id == id }
                }
            }
            ForEach($group.groups) { $child in
                FilterGroupEditor(group: $child, depth: depth + 1, fields: fields, headerPolicy: headerPolicy) {
                    let id = child.id
                    group.groups.removeAll { $0.id == id }
                }
                .padding(.leading, 12)
                .padding(.vertical, 4)
                .overlay(alignment: .leading) {
                    Rectangle().fill(Color.secondary.opacity(0.3)).frame(width: 2)
                }
            }
        }
    }
}

private struct FilterConditionRow: View {
    @Binding var condition: FilterConditionDTO
    let fields: [FilterFieldKind]
    let headerPolicy: FilterHeaderPolicy?
    let onRemove: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            row
            if condition.field == .header, !suggestions.isEmpty {
                HStack(spacing: 4) {
                    ForEach(suggestions, id: \.self) { name in
                        Button(name) { condition.headerName = name }
                            .buttonStyle(.link)
                            .font(.caption)
                    }
                }
                .padding(.leading, 4)
            }
            if condition.field == .header, let note = headerNote {
                Text(note).font(.caption).foregroundStyle(.secondary).padding(.leading, 4)
            }
        }
    }

    /// One caption under a Header row when the typed name cannot work.
    private var headerNote: String? {
        guard let policy = headerPolicy, let name = condition.headerName,
              !name.trimmingCharacters(in: .whitespaces).isEmpty else { return nil }
        switch policy.status(of: name) {
        case .storable: return nil
        case .builtIn(let kind):
            return String(localized: "Use the \(kind.label) field instead")
        case .notStored: return String(localized: "Castellum does not keep this header")
        case .invalid: return String(localized: "Not a valid header name")
        }
    }

    /// Names seen in this profile that start with what was typed (completion).
    private var suggestions: [String] {
        headerPolicy?.completions(for: condition.headerName ?? "") ?? []
    }

    private var headerNameField: some View {
        TextField("", text: Binding(
            get: { condition.headerName ?? "" },
            set: { condition.headerName = $0 }),
            prompt: Text(verbatim: "List-Id"))
            .textFieldStyle(.roundedBorder)
            .frame(width: 150)
            .accessibilityIdentifier("headerName")
    }

    private var row: some View {
        HStack(spacing: 6) {
            Picker("", selection: Binding(
                get: { condition.field },
                set: { condition.changeField(to: $0) })
            ) {
                ForEach(FilterFieldKind.pickerOptions(offered: fields, current: condition.field)) {
                    Text($0.label).tag($0)
                }
            }
            .labelsHidden()
            .fixedSize()
            if condition.field == .header { headerNameField }
            Picker("", selection: Binding(
                get: { condition.op },
                set: { condition.changeOp(to: $0) })
            ) {
                ForEach(condition.field.operators) { Text(condition.field.label(for: $0)).tag($0) }
            }
            .labelsHidden()
            .fixedSize()
            valueEditor
            Spacer(minLength: 0)
            Button(action: onRemove) { Image(systemName: "minus.circle") }
                .buttonStyle(.borderless)
                .help("Remove this condition")
        }
    }

    @ViewBuilder
    private var valueEditor: some View {
        switch condition.field.valueKind(for: condition.op) {
        case .text:
            TextField("value", text: Binding(
                get: { if case .text(let s) = condition.value { return s } else { return "" } },
                set: { condition.value = .text($0) }))
                .textFieldStyle(.roundedBorder)
        case .bool:
            Picker("", selection: Binding(
                get: { if case .bool(let b) = condition.value { return b } else { return true } },
                set: { condition.value = .bool($0) })
            ) {
                Text(condition.field.boolLabel(true)).tag(true)
                Text(condition.field.boolLabel(false)).tag(false)
            }
            .labelsHidden()
            .fixedSize()
        case .days:
            NumberField(
                initial: { if case .number(let n) = condition.value { return String(n) } else { return "7" } }(),
                placeholder: "days", width: 70
            ) { text in
                if let n = Int64(text.trimmingCharacters(in: .whitespaces)) {
                    condition.value = .number(max(0, min(n, 36_500)))
                }
            }
        case .absoluteDate:
            DatePicker("", selection: Binding(
                get: {
                    if case .number(let n) = condition.value { return Date(timeIntervalSince1970: TimeInterval(n)) }
                    return Date()
                },
                // A date-only picker keeps the old time of day; store the
                // day's start so the operators cover whole days (#3455).
                set: { condition.value = .number(
                    FilterConditionDTO.startOfDay(Int64($0.timeIntervalSince1970))) }),
                displayedComponents: .date)
                .labelsHidden()
        case .size:
            HStack(spacing: 4) {
                NumberField(
                    initial: {
                        if case .number(let n) = condition.value { return String(Double(n) / 1_000_000) }
                        return "1"
                    }(),
                    placeholder: "MB", width: 70
                ) { text in
                    if let mb = Double(text.trimmingCharacters(in: .whitespaces)), mb >= 0, mb < 1_000_000 {
                        condition.value = .number(Int64(mb * 1_000_000))
                    }
                }
                Text("MB")
            }
        case .list:
            Text("(set via scope)").foregroundStyle(.secondary)
        }
    }
}

/// Multi-select menu used for the scope line (accounts / mailboxes).
struct ScopeMenu: View {
    let title: String
    let options: [(id: String, label: String)]
    @Binding var selection: [String]

    var body: some View {
        Menu {
            ForEach(options, id: \.id) { option in
                Toggle(option.label, isOn: Binding(
                    get: { selection.contains(option.id) },
                    set: { on in
                        if on { if !selection.contains(option.id) { selection.append(option.id) } }
                        else { selection.removeAll { $0 == option.id } }
                    }))
            }
            if !selection.isEmpty {
                Divider()
                Button("Clear") { selection = [] }
            }
        } label: {
            Text(selection.isEmpty ? title + ": all" : title + ": \(selection.count)")
        }
        .fixedSize()
    }
}

/// A text field that commits every keystroke that parses (a `TextField(value:
/// format:)` only commits on Return or focus loss, so Save could store a stale
/// number). Unparseable text leaves the last good value in place.
private struct NumberField: View {
    let placeholder: String
    let width: CGFloat
    let onText: (String) -> Void
    @State private var text: String

    init(initial: String, placeholder: String, width: CGFloat, onText: @escaping (String) -> Void) {
        self.placeholder = placeholder
        self.width = width
        self.onText = onText
        _text = State(initialValue: initial)
    }

    var body: some View {
        TextField(placeholder, text: $text)
            .textFieldStyle(.roundedBorder)
            .frame(width: width)
            .onChange(of: text) { onText($0) }
    }
}
