// Castellum - Confirmation sheet for List-Unsubscribe (S44). Always shows
// the EXACT destination before anything leaves the machine; never follows a
// link or sends automatically.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct UnsubscribeConfirmSheet: View {
    let detail: MessageDetail
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Unsubscribe from this list?").font(.headline)
            switch UnsubscribePlan.make(for: detail) {
            case .oneClick(let url, let host):
                Text("Castellum will send a single request to \(host):")
                Text(url).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                Text("No other content leaves your device.").font(.caption).foregroundStyle(.secondary)
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Unsubscribe") {
                        model.confirmUnsubscribeOneClick()
                        dismiss()
                    }
                    .buttonStyle(.borderedProminent)
                }
            case .mailto(let address, let subject, let body):
                Text("Castellum will create and send a short email to:")
                Text(address).font(.system(.body, design: .monospaced)).textSelection(.enabled)
                // Everything that will be sent is shown, never sent unseen.
                Text("Subject: \(subject)").font(.caption).lineLimit(3).textSelection(.enabled)
                if !body.isEmpty {
                    // Bounded height so Cancel / Unsubscribe always stay reachable.
                    ScrollView {
                        Text("Body: \(body)").font(.caption).textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .frame(maxHeight: 80)
                }
                HStack {
                    Spacer()
                    Button("Cancel", role: .cancel) { dismiss() }
                    Button("Unsubscribe") {
                        model.confirmUnsubscribeByMailto()
                        dismiss()
                    }
                    .buttonStyle(.borderedProminent)
                }
            case .refused(let reason):
                Text(reason)
                Button("OK") { dismiss() }
            }
        }
        .padding(20)
        .frame(width: 380)
    }
}
