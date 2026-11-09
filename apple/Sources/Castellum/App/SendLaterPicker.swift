// Castellum - the Send Later picker (HP1): presets plus a custom date and time.
// Used by the compose window's "Send Later" control and by the Outbox
// "Reschedule" action, so both offer exactly the same choices.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct SendLaterPicker: View {
    /// Called with the chosen instant (always in the future).
    let onPick: (Date) -> Void
    let onCancel: () -> Void

    @State private var customDate = Date().addingTimeInterval(3600)
    @State private var showingCustom = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(SendLaterPreset.standardCases, id: \.self) { preset in
                let date = preset.sendDate()
                Button {
                    onPick(date)
                } label: {
                    HStack {
                        Text(preset.title)
                        Spacer(minLength: 16)
                        Text(sendLaterDisplay(Int64(date.timeIntervalSince1970)))
                            .foregroundStyle(.secondary)
                            .font(.caption)
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .padding(.vertical, 3)
                .accessibilityIdentifier("sendLater.preset.\(index(of: preset))")
            }
            Divider()
            if showingCustom {
                DatePicker(
                    "Date and time", selection: $customDate, in: Date().addingTimeInterval(60)...,
                    displayedComponents: [.date, .hourAndMinute]
                )
                .datePickerStyle(.compact)
                .accessibilityIdentifier("sendLater.customDate")
                HStack {
                    Button("Cancel", action: onCancel)
                    Spacer()
                    Button("Schedule") { onPick(customDate) }
                        .keyboardShortcut(.defaultAction)
                        .disabled(customDate <= Date())
                        .accessibilityIdentifier("sendLater.schedule")
                }
            } else {
                Button(SendLaterPreset.custom(customDate).title) { showingCustom = true }
                    .buttonStyle(.plain)
                    .padding(.vertical, 3)
                    .accessibilityIdentifier("sendLater.custom")
            }
        }
        .padding(12)
        .frame(width: 300)
    }

    private func index(of preset: SendLaterPreset) -> Int {
        SendLaterPreset.standardCases.firstIndex(of: preset) ?? 0
    }
}
