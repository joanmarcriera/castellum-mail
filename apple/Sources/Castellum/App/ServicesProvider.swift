// Castellum - Services menu provider: "New Email With Selection" (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Registered as `NSApp.servicesProvider` (AppDelegate) and declared in
// project.yml's `NSServices` Info.plist array. The selector signature below
// is fixed by AppKit's Services dispatch (Obj-C: `-(void)name:(NSPasteboard
// *)pboard userData:(NSString *)userData error:(NSString **)error`) — it is
// invoked by AppKit itself, never called directly from Swift.

import AppKit

@MainActor
final class ServicesProvider: NSObject {
    /// Set once `AppModel` exists (`AppDelegate.modelBecameAvailable`).
    weak var model: AppModel?

    @objc func newEmailWithSelection(
        _ pasteboard: NSPasteboard, userData: String,
        error: AutoreleasingUnsafeMutablePointer<NSString>
    ) {
        guard let model else {
            error.pointee = "Castellum isn't ready yet." as NSString
            return
        }
        guard let text = pasteboard.string(forType: .string),
              !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else {
            error.pointee = "No text was selected." as NSString
            return
        }
        MailtoOpener.open(MailtoPrefill(body: text), model: model)
    }
}
