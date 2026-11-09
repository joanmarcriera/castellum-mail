// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 sheet hook (Debug builds only).
//
// Usage: `view.uiScriptHook("addAccount") { params in showSetup = true }`. Sheets in
// Castellum are view-local `@State`, so nothing outside the view can present
// them. The runner posts `castellum.uiScript.present` with `target` and
// `params`, and the matching hook flips the view's own state, so SwiftUI's real
// presentation path is what gets exercised. Call sites are wrapped in `#if DEBUG`.
#if DEBUG
import SwiftUI

extension Notification.Name {
    static let uiScriptPresent = Notification.Name("castellum.uiScript.present")
}

extension View {
    func uiScriptHook(_ id: String, perform: @escaping ([String: String]) -> Void) -> some View {
        onReceive(NotificationCenter.default.publisher(for: .uiScriptPresent)) { note in
            guard (note.userInfo?["target"] as? String) == id else { return }
            perform((note.userInfo?["params"] as? [String: String]) ?? [:])
        }
    }
}
#endif
