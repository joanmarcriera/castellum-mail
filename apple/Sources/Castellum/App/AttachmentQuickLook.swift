// Castellum - QLPreviewPanel integration for the attachment strip (S32).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// AppKit's QLPreviewPanel is a single shared system panel with one
// data source/delegate at a time; a SwiftUI view has no natural place to
// host that, so this is a small NSViewRepresentable whose backing
// NSView inserts a responder into the chain and answers
// acceptsPreviewPanelControl/beginPreviewPanelControl/endPreviewPanelControl,
// the standard Apple-documented pattern for driving QLPreviewPanel from a
// custom UI.

import AppKit
import SwiftUI
import Quartz

/// One file to preview: a staged (quarantined, correctly named) copy plus the
/// title shown in the panel.
struct AttachmentQuickLookItem: Equatable {
    let url: URL
    let title: String
}

/// Mounted invisibly (zero size) inside `AttachmentStripView`; opens/closes
/// the shared `QLPreviewPanel` in response to `item` changing to/from
/// non-nil.
struct AttachmentQuickLookHost: NSViewRepresentable {
    let item: AttachmentQuickLookItem?
    let onDismiss: () -> Void

    func makeNSView(context: Context) -> AttachmentQuickLookView {
        AttachmentQuickLookView(onDismiss: onDismiss)
    }

    func updateNSView(_ nsView: AttachmentQuickLookView, context: Context) {
        nsView.onDismiss = onDismiss
        // SwiftUI re-runs this on every unrelated state change; only act when
        // the item actually changed, or the panel would be re-ordered front
        // (stealing focus) on every re-render while it is open.
        guard nsView.item != item else { return }
        nsView.item = item
        if item != nil {
            nsView.showPanel()
        } else {
            nsView.hidePanel()
        }
    }
}

final class AttachmentQuickLookView: NSView, QLPreviewPanelDataSource, QLPreviewPanelDelegate {
    /// Set by the host; `showPanel()`/`hidePanel()` follow it.
    var item: AttachmentQuickLookItem? {
        didSet { previewItem = item.map(AttachmentPreviewItem.init) }
    }
    var onDismiss: () -> Void
    private var previewItem: AttachmentPreviewItem?

    init(onDismiss: @escaping () -> Void) {
        self.onDismiss = onDismiss
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) not used") }

    override var acceptsFirstResponder: Bool { true }

    func showPanel() {
        guard let panel = QLPreviewPanel.shared() else { return }
        window?.makeFirstResponder(self)
        panel.dataSource = self
        panel.delegate = self
        panel.reloadData()
        panel.makeKeyAndOrderFront(nil)
    }

    func hidePanel() {
        guard QLPreviewPanel.sharedPreviewPanelExists(),
              let panel = QLPreviewPanel.shared(), panel.isVisible else { return }
        panel.orderOut(nil)
    }

    override func acceptsPreviewPanelControl(_ panel: QLPreviewPanel!) -> Bool { true }

    override func beginPreviewPanelControl(_ panel: QLPreviewPanel!) {
        panel.dataSource = self
        panel.delegate = self
    }

    override func endPreviewPanelControl(_ panel: QLPreviewPanel!) {
        // The user closed the panel: clear the host's state so the same
        // attachment can be previewed again.
        item = nil
        onDismiss()
    }

    func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        previewItem == nil ? 0 : 1
    }

    func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! {
        previewItem
    }
}

private final class AttachmentPreviewItem: NSObject, QLPreviewItem {
    let previewItemURL: URL?
    let previewItemTitle: String?

    init(_ item: AttachmentQuickLookItem) {
        self.previewItemURL = item.url
        self.previewItemTitle = item.title
    }
}
