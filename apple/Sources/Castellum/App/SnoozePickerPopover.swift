// Castellum - Snooze preset/custom picker (S43). Shown as a popover from the
// message list / reading pane's Snooze command.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct SnoozePickerPopover: View {
    @ObservedObject var model: AppModel
    let messageIds: [String]
    let onDismiss: () -> Void

    @State private var showCustomDatePicker = false
    @State private var customDate = Date().addingTimeInterval(3600)

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(SnoozePreset.standardCases, id: \.self) { preset in
                Button(preset.title) { apply(preset) }
                    .buttonStyle(.plain)
                    .padding(.vertical, 4)
            }
            Divider()
            if showCustomDatePicker {
                DatePicker("", selection: $customDate, in: Date()..., displayedComponents: [.date, .hourAndMinute])
                    .labelsHidden()
                Button("Snooze") { apply(.custom(customDate)) }
                    .buttonStyle(.borderedProminent)
            } else {
                Button("Custom…") { showCustomDatePicker = true }
                    .buttonStyle(.plain)
                    .padding(.vertical, 4)
            }
        }
        .padding(12)
        .frame(width: 200)
    }

    private func apply(_ preset: SnoozePreset) {
        let epoch = Int64(preset.dueDate().timeIntervalSince1970)
        model.snoozeSelected(until: epoch)
        onDismiss()
    }
}
