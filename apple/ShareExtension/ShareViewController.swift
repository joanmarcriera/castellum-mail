// Castellum Share Extension - accepts shared text/URLs/files from any Mac
// app's Share Sheet and hands them to the main app (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// No UI of its own (the design bar is "no intermediate dialog"): gather the
// shared items, write one SharePendingCompose descriptor into the App Group
// container, activate the main app, and dismiss. Never touches the network —
// this process is sandboxed and has no Rust FFI, IMAP, or SMTP access at
// all; see SharePendingCompose.swift for why the App Group handoff is the
// chosen shape.

import Cocoa
import UniformTypeIdentifiers

final class ShareViewController: NSViewController {
    override func loadView() {
        view = NSView(frame: NSRect(x: 0, y: 0, width: 1, height: 1))
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        gatherSharedContent()
    }

    private func gatherSharedContent() {
        guard let items = extensionContext?.inputItems as? [NSExtensionItem], !items.isEmpty else {
            finish()
            return
        }

        var subject = ""
        // Slots are reserved in enumeration order here (main) and filled on main only (#3381).
        var slots = ShareContentSlots()
        let group = DispatchGroup()

        for item in items {
            if let title = item.attributedTitle?.string, !title.isEmpty, subject.isEmpty {
                subject = title
            }
            if let content = item.attributedContentText?.string, !content.isEmpty {
                let slot = slots.reserve()
                slots.fill(slot, with: content)
            }
            for provider in item.attachments ?? [] {
                // public.file-url conforms to public.url, so test the more specific type
                // first or a Finder file would land as a full file:///Users/... path.
                if provider.hasItemConformingToTypeIdentifier(UTType.fileURL.identifier) {
                    let slot = slots.reserve()
                    group.enter()
                    provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier) { data, _ in
                        // v1: shared files are noted by name in the body
                        // rather than attached (the extension cannot reach
                        // the Rust attachment/FFI path — see the S57
                        // Deviations note). Dropping a file directly onto a
                        // compose window's own attachment well is still the
                        // full-fidelity path for attachments.
                        let text = (data as? URL).map { "[Shared file: \($0.lastPathComponent)]" }
                        DispatchQueue.main.async { slots.fill(slot, with: text); group.leave() }
                    }
                } else if provider.hasItemConformingToTypeIdentifier(UTType.url.identifier) {
                    let slot = slots.reserve()
                    group.enter()
                    provider.loadItem(forTypeIdentifier: UTType.url.identifier) { data, _ in
                        let text = (data as? URL)?.absoluteString
                        DispatchQueue.main.async { slots.fill(slot, with: text); group.leave() }
                    }
                } else if provider.hasItemConformingToTypeIdentifier(UTType.plainText.identifier) {
                    let slot = slots.reserve()
                    group.enter()
                    provider.loadItem(forTypeIdentifier: UTType.plainText.identifier) { data, _ in
                        let text = data as? String
                        DispatchQueue.main.async { slots.fill(slot, with: text); group.leave() }
                    }
                }
            }
        }

        group.notify(queue: .main) { [weak self] in
            let pending = SharePendingCompose(subject: subject, body: slots.body)
            pending.write()
            self?.activateMainAppAndFinish()
        }
    }

    /// Activates Castellum so it can pick up the descriptor just written
    /// (`AppDelegate.applicationDidBecomeActive` -> `SharePendingCompose.
    /// consumePending()`), then completes the extension request so the
    /// Share Sheet dismisses immediately — no intermediate dialog.
    private func activateMainAppAndFinish() {
        if let appURL = Self.containingAppURL() {
            let configuration = NSWorkspace.OpenConfiguration()
            configuration.activates = true
            NSWorkspace.shared.openApplication(at: appURL, configuration: configuration) { [weak self] _, _ in
                self?.finish()
            }
        } else {
            finish()
        }
    }

    /// The app this extension is embedded in:
    /// `X.app/Contents/PlugIns/Castellum Share.appex` -> `X.app`. Not a
    /// bundle-id lookup: Debug builds carry `uk.co.riera.castellum.dev`, and
    /// Launch Services can know dozens of stale DerivedData copies of either
    /// id — the embedding app is the only one guaranteed to share our group.
    private static func containingAppURL() -> URL? {
        let app = Bundle.main.bundleURL
            .deletingLastPathComponent()  // PlugIns
            .deletingLastPathComponent()  // Contents
            .deletingLastPathComponent()  // X.app
        return app.pathExtension == "app" ? app : nil
    }

    private func finish() {
        extensionContext?.completeRequest(returningItems: nil)
    }
}
