// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 probes for SwiftUI-drawn controls (Debug builds only).
//
// Usage: `.uiProbe("sync", label: "Sync", enabled: canSync, keyEquivalent: "\r") { startSync() }`
// behind `#if DEBUG` at the call site. The UIT-1 spike found that on macOS 26 a
// bordered SwiftUI `Button` (the sheet footer's Cancel / Test Connection / Sync) is
// drawn by SwiftUI itself: there is NO NSButton, and the in-process
// accessibility tree is empty without an AX client, so an NSView walk cannot see
// it. A probe is a zero-cost NSView placed as the control's `.background`: it has
// the control's exact frame, so `UIViewTree` reports it as a node and
// `press` can call its action. Call sites must be `#if DEBUG` so Release has no symbol.
#if DEBUG
import AppKit
import SwiftUI

final class UIProbeView: NSView {
    var probeId = ""
    var role = "button"
    var label = ""
    var probeEnabled = true
    var keyEquivalent = ""
    var action: (() -> Void)?
    override func hitTest(_ point: NSPoint) -> NSView? { nil }   // never intercept real clicks
}

struct UIProbe: NSViewRepresentable {
    var id: String, role: String, label: String, enabled: Bool, keyEquivalent: String
    var action: (() -> Void)?

    func makeNSView(context: Context) -> UIProbeView { UIProbeView() }
    func updateNSView(_ v: UIProbeView, context: Context) {
        v.probeId = id; v.role = role; v.label = label; v.probeEnabled = enabled
        v.keyEquivalent = keyEquivalent; v.action = action
    }
}

extension View {
    func uiProbe(_ id: String, role: String = "button", label: String, enabled: Bool = true,
                 keyEquivalent: String = "", action: (() -> Void)? = nil) -> some View {
        background(UIProbe(id: id, role: role, label: label, enabled: enabled,
                           keyEquivalent: keyEquivalent, action: action))
    }
}
#endif
