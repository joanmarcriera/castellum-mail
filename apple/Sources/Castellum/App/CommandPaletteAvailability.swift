// Castellum - which registry commands the command palette can actually run
// right now (S45). Deliberately NOT AppModel itself: this is the minimal,
// pure slice of state a command's availability depends on, so it is
// unit-testable with no FFI, no Keychain, no profile directory.
// `AppModel.paletteContext` (App/AppModel.swift) is the only production
// builder of a `PaletteContext`.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

/// A disabled command is shown greyed out in the palette, not hidden — see
/// this plan's Assumption 6. Only commands the existing UI already gates
/// (RootView.swift toolbar: Reply/Reply All need a hydrated body, Archive/
/// Delete need a selection) plus the ones that share their "act on
/// selection" sheet group, the other selection/detail readers (source,
/// export, print, mute, VIP, snooze, tag, correspondence) and the compose-window
/// Send Later commands are covered; the rest are always enabled.
struct PaletteContext: Equatable {
    var hasSelectedMessage: Bool
    /// Mirrors `messageBodyActionsAvailable(model.detail)` (RootView.swift) —
    /// a selected message whose body hasn't hydrated yet cannot be replied to.
    var messageBodyAvailable: Bool
    var aiEnabled: Bool
    /// A compose window exists to receive Send Later (the only commands that
    /// act on a composer; the palette itself is hosted by the main window).
    var composerOpen: Bool = false
    /// The session undo stack holds an archive / delete / move to reverse.
    var removalUndoAvailable: Bool = true
    /// How many tags are defined: tag slot `n` can run only when `n <= tagCount` (MM-8).
    var tagCount: Int = 0
}

enum CommandPaletteAvailability {
    static func isEnabled(_ id: CommandID, context: PaletteContext) -> Bool {
        switch id {
        case .reply, .replyAll, .forward:
            return context.messageBodyAvailable
        case .replyWithAi:
            return context.messageBodyAvailable && context.aiEnabled
        case .summarize, .toggleRead, .markRead, .markUnread, .toggleFlag, .archive, .delete, .assignProject,
             .tagMessage, .toggleMuteThread, .toggleVipSender, .snooze, .unsnooze, .exportEml, .moveToFolder,
             .markJunk, .markNotJunk:
            return context.hasSelectedMessage
        // These read the loaded message (RootView's handlers bail on nil detail).
        case .viewSource, .printMessage, .showCorrespondence, .expandAllQuotes:
            return context.messageBodyAvailable
        case .undoRemoval:
            return context.removalUndoAvailable
        case .tagSlot1, .tagSlot2, .tagSlot3, .tagSlot4, .tagSlot5, .tagSlot6, .tagSlot7, .tagSlot8, .tagSlot9:
            return context.hasSelectedMessage && (id.tagSlotIndex ?? Int.max) <= context.tagCount
        case .sendLater, .sendLaterInOneHour, .sendLaterThisEvening,
             .sendLaterTomorrowMorning, .sendLaterMonday, .sendComposer, .insertSnippet:
            return context.composerOpen
        default:
            return true
        }
    }
}
