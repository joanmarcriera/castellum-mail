// Castellum - CommandPaletteAvailability: pure enabled/disabled predicate.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class CommandPaletteAvailabilityTests: XCTestCase {
    private let empty = PaletteContext(
        hasSelectedMessage: false, messageBodyAvailable: false, aiEnabled: false)
    private let selected = PaletteContext(
        hasSelectedMessage: true, messageBodyAvailable: true, aiEnabled: false)
    private let selectedWithAi = PaletteContext(
        hasSelectedMessage: true, messageBodyAvailable: true, aiEnabled: true)

    func testSelectionDependentCommandsAreDisabledWithNoSelection() {
        for id: CommandID in [.reply, .replyAll, .forward, .summarize, .toggleRead, .toggleFlag,
                               .archive, .delete, .assignProject] {
            XCTAssertFalse(CommandPaletteAvailability.isEnabled(id, context: empty), "\(id)")
        }
    }

    func testSelectionDependentCommandsAreEnabledWithASelection() {
        for id: CommandID in [.reply, .replyAll, .forward, .summarize, .toggleRead, .toggleFlag,
                               .archive, .delete, .assignProject] {
            XCTAssertTrue(CommandPaletteAvailability.isEnabled(id, context: selected), "\(id)")
        }
    }

    func testReplyWithAiNeedsBothASelectionAndAiEnabled() {
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.replyWithAi, context: empty))
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.replyWithAi, context: selected),
                       "a selection alone isn't enough — AI must be enabled too")
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.replyWithAi, context: selectedWithAi))
    }

    func testUngatedCommandsAreAlwaysEnabled() {
        for id: CommandID in [.compose, .focusSearch, .emptyTrash, .showOutbox, .fitColumns,
                               .showStatus, .showAiActivity, .showContacts, .showProjects,
                               .showShortcuts, .showDiagnostics, .checkForUpdates,
                               .markAllRead] {
            XCTAssertTrue(CommandPaletteAvailability.isEnabled(id, context: empty), "\(id)")
        }
    }

    func testSelectionReadersAreDisabledWithNoSelection() {
        for id: CommandID in [.tagMessage, .toggleMuteThread, .toggleVipSender, .snooze,
                               .unsnooze, .exportEml, .viewSource, .printMessage,
                               .showCorrespondence] {
            XCTAssertFalse(CommandPaletteAvailability.isEnabled(id, context: empty), "\(id)")
            XCTAssertTrue(CommandPaletteAvailability.isEnabled(id, context: selected), "\(id)")
        }
    }

    func testSendLaterNeedsAnOpenComposer() {
        var composing = selected
        composing.composerOpen = true
        for id: CommandID in [.sendLater, .sendLaterInOneHour, .sendLaterThisEvening,
                               .sendLaterTomorrowMorning, .sendLaterMonday] {
            XCTAssertFalse(CommandPaletteAvailability.isEnabled(id, context: selected), "\(id)")
            XCTAssertTrue(CommandPaletteAvailability.isEnabled(id, context: composing), "\(id)")
        }
    }
}
