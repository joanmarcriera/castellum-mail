// Castellum - hands AppModel a way to open the Settings scene.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The deployment target is macOS 13, where the only way to show a SwiftUI
// `Settings` scene is the AppKit `showSettingsWindow:` selector. macOS 14
// added the `openSettings` environment action, which works from any context
// (an alert button in a secondary window included). This zero-size view reads
// that action where it exists and stores it on the model, so
// `AppModel.openAccountsSettings` can prefer it and fall back to the selector.

import SwiftUI

struct SettingsOpenerInstaller: View {
    @ObservedObject var model: AppModel

    var body: some View {
        if #available(macOS 14, *) {
            ModernSettingsOpenerInstaller(model: model)
        } else {
            EmptyView()
        }
    }
}

@available(macOS 14, *)
private struct ModernSettingsOpenerInstaller: View {
    @ObservedObject var model: AppModel
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear { model.settingsOpener = { openSettings() } }
    }
}
