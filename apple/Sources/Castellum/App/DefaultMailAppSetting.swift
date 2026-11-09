// Castellum - "Make Castellum my default mail app" (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Deliberate only, per design: becoming the default mail handler must never
// be automatic. This button is the one place it can happen, and macOS's own
// `setDefaultApplication` presents a system confirmation before anything
// actually changes — so the user is asked twice, never once silently.

import AppKit
import SwiftUI

struct DefaultMailAppSection: View {
    @State private var resultText: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Button("Make Castellum My Default Mail App…") { setDefault() }
            if let resultText {
                Text(resultText)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text("macOS asks you to confirm before anything changes. Switch back any time from System Settings.")
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
    }

    private func setDefault() {
        NSWorkspace.shared.setDefaultApplication(
            at: Bundle.main.bundleURL, toOpenURLsWithScheme: "mailto"
        ) { error in
            DispatchQueue.main.async {
                if let error {
                    resultText = "Couldn't set the default: \(error.localizedDescription)"
                } else {
                    resultText = "Castellum is now the default for mailto: links."
                }
            }
        }
    }
}
