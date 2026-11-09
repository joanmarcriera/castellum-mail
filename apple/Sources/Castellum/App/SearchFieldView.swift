// Castellum - AppKit NSSearchField wrapper with a focus hook.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// A SwiftUI TextField in a toolbar can't be focused reliably from a keyboard
// shortcut on macOS 13, so search uses an NSSearchField that becomes first
// responder when it receives the `.castellumFocusSearch` notification (posted by
// the ⌘K menu command in CastellumApp).

import SwiftUI
import AppKit

extension Notification.Name {
    static let castellumFocusSearch = Notification.Name("castellum.focusSearch")
    static let castellumMarkRead = Notification.Name("castellum.markRead")
    static let castellumMarkUnread = Notification.Name("castellum.markUnread")
    static let castellumSnoozeLaterToday = Notification.Name("castellum.snooze.laterToday")
    static let castellumSnoozeTomorrow = Notification.Name("castellum.snooze.tomorrow")
    static let castellumSnoozeThisWeekend = Notification.Name("castellum.snooze.thisWeekend")
    static let castellumSnoozeNextWeek = Notification.Name("castellum.snooze.nextWeek")
    static let castellumOpenInWindow = Notification.Name("castellum.openInWindow")
    /// MM-12: message-list navigation and the go-to-mailbox jumps are ordinary
    /// notification commands, so a key sequence can run (or be rebound to) each.
    static let castellumNextMessage = Notification.Name("castellum.nextMessage")
    static let castellumPreviousMessage = Notification.Name("castellum.previousMessage")
    static let castellumOpenMessage = Notification.Name("castellum.openMessage")
    static let castellumGotoInbox = Notification.Name("castellum.goto.inbox")
    static let castellumGotoArchive = Notification.Name("castellum.goto.archive")
    static let castellumGotoSent = Notification.Name("castellum.goto.sent")
    static let castellumGotoTrash = Notification.Name("castellum.goto.trash")
    static let castellumGotoDrafts = Notification.Name("castellum.goto.drafts")
    static let castellumGotoJunk = Notification.Name("castellum.goto.junk")
    static let castellumFindSimilar = Notification.Name("castellum.findSimilar")
    static let castellumToggleRead = Notification.Name("castellum.toggleRead")
    static let castellumMarkAllRead = Notification.Name("castellum.markAllRead")
    static let castellumFitColumns = Notification.Name("castellum.fitColumns")
    static let castellumLayoutWidescreen = Notification.Name("castellum.layoutWidescreen")
    static let castellumHeaderCompact = Notification.Name("castellum.headerCompact")
    static let castellumHeaderExtended = Notification.Name("castellum.headerExtended")
    static let castellumHeaderAll = Notification.Name("castellum.headerAll")
    static let castellumLayoutClassic = Notification.Name("castellum.layoutClassic")
    static let castellumShowCorrespondence = Notification.Name("castellum.showCorrespondence")
    /// HP-2: View > Next Split / Previous Split.
    static let castellumNextSplit = Notification.Name("castellum.nextSplit")
    static let castellumPreviousSplit = Notification.Name("castellum.previousSplit")
    static let castellumArchive = Notification.Name("castellum.archive")
    static let castellumUndoRemoval = Notification.Name("castellum.undoRemoval")
    static let castellumMarkJunk = Notification.Name("castellum.markJunk")
    static let castellumMarkNotJunk = Notification.Name("castellum.markNotJunk")
    static let castellumMoveToFolder = Notification.Name("castellum.moveToFolder")
    static let castellumInsertSnippet = Notification.Name("castellum.insertSnippet")
    static let castellumToggleRemoteImages = Notification.Name("castellum.toggleRemoteImages")
    static let castellumExpandAllQuotes = Notification.Name("castellum.expandAllQuotes")
    static let castellumShowSnippets = Notification.Name("castellum.showSnippets")
    static let castellumDelete = Notification.Name("castellum.delete")
    static let castellumEmptyTrash = Notification.Name("castellum.emptyTrash")
    static let castellumSnooze = Notification.Name("castellum.snooze")
    static let castellumSendLater = Notification.Name("castellum.sendLater")
    static let castellumSendLaterInOneHour = Notification.Name("castellum.sendLater.inOneHour")
    static let castellumSendLaterThisEvening = Notification.Name("castellum.sendLater.thisEvening")
    static let castellumSendLaterTomorrowMorning = Notification.Name("castellum.sendLater.tomorrowMorning")
    static let castellumSendLaterMonday = Notification.Name("castellum.sendLater.monday")
    static let castellumUnsnooze = Notification.Name("castellum.unsnooze")
    static let castellumShowShortcuts = Notification.Name("castellum.showShortcuts")
    static let castellumToggleFlag = Notification.Name("castellum.toggleFlag")
    static let castellumTagMessage = Notification.Name("castellum.tagMessage")
    /// MM-8: toggle the tag in slot `n` (1...9) on the selection.
    static func castellumTagSlot(_ n: Int) -> Notification.Name {
        Notification.Name("castellum.tagSlot\(n)")
    }
    /// MM-8: `g l`, open the tag picker in go-to mode.
    static let castellumGotoTag = Notification.Name("castellum.gotoTag")
    static let castellumToggleMuteThread = Notification.Name("castellum.toggleMuteThread")
    static let castellumToggleVipSender = Notification.Name("castellum.toggleVipSender")
    static let castellumCompose = Notification.Name("castellum.compose")
    static let castellumReply = Notification.Name("castellum.reply")
    static let castellumReplyAll = Notification.Name("castellum.replyAll")
    static let castellumForward = Notification.Name("castellum.forward")
    static let castellumShowMainWindow = Notification.Name("castellum.showMainWindow")
    static let castellumReplyWithAi = Notification.Name("castellum.replyWithAi")
    static let castellumShowContacts = Notification.Name("castellum.showContacts")
    static let castellumShowProjects = Notification.Name("castellum.showProjects")
    static let castellumAssignProject = Notification.Name("castellum.assignProject")
    static let castellumSummarize = Notification.Name("castellum.summarize")
    static let castellumShowDiagnostics = Notification.Name("castellum.showDiagnostics")
    static let castellumShowOutbox = Notification.Name("castellum.showOutbox")
    static let castellumSendAllOutbox = Notification.Name("castellum.sendAllOutbox")
    static let castellumCheckForUpdates = Notification.Name("castellum.checkForUpdates")
    static let castellumShowStatus = Notification.Name("castellum.showStatus")
    static let castellumShowAiActivity = Notification.Name("castellum.showAiActivity")
    static let castellumViewSource = Notification.Name("castellum.viewSource")
    static let castellumExportEml = Notification.Name("castellum.exportEml")
    static let castellumImportEml = Notification.Name("castellum.importEml")
    static let castellumPrintMessage = Notification.Name("castellum.printMessage")
    static let castellumShowCommandPalette = Notification.Name("castellum.showCommandPalette")
}

struct SearchFieldView: NSViewRepresentable {
    @Binding var text: String
    var onChangeApply: () -> Void   // called on Return and when cleared

    /// Shown as the field's tooltip on hover — the S39 search grammar, in
    /// British English, kept to one line. `static` (not computed) so
    /// SearchQuerySyntaxTests can assert its content directly without
    /// instantiating an `NSSearchField`.
    static let syntaxHint = String(
        localized: "Search — from: to: cc: subject: has:attachment before: after: in: is:read|unread|flagged, \"exact phrase\", -exclude"
    )

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSSearchField {
        let field = NSSearchField()
        field.placeholderString = String(localized: "Search")
        field.toolTip = Self.syntaxHint
        field.delegate = context.coordinator
        field.sendsWholeSearchString = true
        field.sendsSearchStringImmediately = false
        context.coordinator.field = field
        NotificationCenter.default.addObserver(
            context.coordinator,
            selector: #selector(Coordinator.focus),
            name: .castellumFocusSearch,
            object: nil
        )
        return field
    }

    func updateNSView(_ nsView: NSSearchField, context: Context) {
        if nsView.stringValue != text { nsView.stringValue = text }
    }

    static func dismantleNSView(_ nsView: NSSearchField, coordinator: Coordinator) {
        NotificationCenter.default.removeObserver(coordinator)
    }

    @MainActor
    final class Coordinator: NSObject, NSSearchFieldDelegate {
        let parent: SearchFieldView
        weak var field: NSSearchField?

        init(_ parent: SearchFieldView) { self.parent = parent }

        func controlTextDidChange(_ obj: Notification) {
            let value = field?.stringValue ?? ""
            parent.text = value
            if value.isEmpty { parent.onChangeApply() } // cleared → restore list
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            if selector == #selector(NSResponder.insertNewline(_:)) {
                parent.onChangeApply()
                return true
            }
            return false
        }

        @objc func focus() {
            guard let field else { return }
            field.window?.makeFirstResponder(field)
        }
    }
}
