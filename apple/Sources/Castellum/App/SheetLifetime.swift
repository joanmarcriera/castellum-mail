// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// SheetLifetime.swift
//
// Ties deferred work started by a sheet (e.g. AccountSetupSheet's "close 0.9 s
// after the sync succeeded") to THAT sheet's own lifetime.
//
// Why: SwiftUI's `@Environment(\.dismiss)` closes whatever the sheet's
// presentation binding currently shows, not a specific view instance. A
// completion closure captured by the first Add-account sheet and fired after
// "Continue in Background" would therefore dismiss the *second* sheet the
// user had meanwhile opened (, GUI-2). A sheet must only close
// on its own explicit Save/Cancel/success, never on a stale callback.
//
// Usage: create one per sheet (`@State private var lifetime = SheetLifetime()`),
// call `close()` from `.onDisappear`, and route every deferred dismissal
// through `dismissIfOpen(_:)`.

import Foundation

@MainActor
final class SheetLifetime {
    /// False once the owning sheet has disappeared; never flips back.
    private(set) var isOpen = true

    /// Mark the owning sheet as gone. Idempotent.
    func close() { isOpen = false }

    /// Run `dismiss` only if the owning sheet is still on screen.
    /// Returns whether it ran (for tests).
    @discardableResult
    func dismissIfOpen(_ dismiss: () -> Void) -> Bool {
        guard isOpen else { return false }
        dismiss()
        return true
    }
}
