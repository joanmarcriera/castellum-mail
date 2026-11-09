// Castellum - pure tests for the session undo stack.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class RemovalUndoStackTests: XCTestCase {
    private func entry(
        _ verb: RemovalVerb = .archive, ops: [Int64] = [1], accounts: Set<String> = ["a"]
    ) -> UndoableRemoval {
        UndoableRemoval(verb: verb, operationIds: ops, count: ops.count, accountIds: accounts)
    }

    func testStackCapIsTwentyAndEvictsTheOldest() {
        var stack = RemovalUndoStack()
        var all: [UndoableRemoval] = []
        for i in 0..<25 {
            let e = entry(ops: [Int64(i)])
            all.append(e)
            let evicted = stack.push(e)
            XCTAssertEqual(evicted.count, i >= 20 ? 1 : 0)
        }
        XCTAssertEqual(stack.depth, 20)
        XCTAssertEqual(stack.entries.first?.id, all[5].id, "the five oldest were evicted")
        XCTAssertEqual(stack.top?.id, all[24].id)
    }

    func testRemoveByIdIsOneShot() {
        var stack = RemovalUndoStack()
        let e = entry()
        stack.push(e)
        XCTAssertEqual(stack.remove(id: e.id)?.id, e.id)
        XCTAssertNil(stack.remove(id: e.id), "a second remove (Cmd-Z racing z) is a no-op")
        XCTAssertTrue(stack.isEmpty)
    }

    func testDroppingAnAccountPrunesOnlyItsEntries() {
        var stack = RemovalUndoStack()
        let a = entry(accounts: ["a"])
        let b = entry(accounts: ["b"])
        let both = entry(accounts: ["a", "b"])
        [a, b, both].forEach { stack.push($0) }
        let dropped = stack.dropEntries(forAccount: "a")
        XCTAssertEqual(Set(dropped.map(\.id)), [a.id, both.id])
        XCTAssertEqual(stack.entries.map(\.id), [b.id])
    }

    func testActionNamesAreTheEditMenuVerbs() {
        XCTAssertFalse(RemovalVerb.archive.actionName.isEmpty)
        XCTAssertNotEqual(RemovalVerb.archive.actionName, RemovalVerb.delete.actionName)
        XCTAssertNotEqual(RemovalVerb.delete.actionName, RemovalVerb.move.actionName)
    }

    func testBareZIsBoundToUndoRemoval() {
        let spec = CommandRegistry.spec(for: .undoRemoval)
        XCTAssertEqual(spec.bareKey, "z")
        XCTAssertNil(spec.menuChord, "Cmd-Z belongs to the system Edit > Undo item")
        XCTAssertEqual(spec.notification, .castellumUndoRemoval)
    }

    func testUndoCommandIsDisabledInThePaletteWhenNothingToUndo() {
        let ctx = PaletteContext(
            hasSelectedMessage: false, messageBodyAvailable: false, aiEnabled: false,
            composerOpen: false, removalUndoAvailable: false)
        XCTAssertFalse(CommandPaletteAvailability.isEnabled(.undoRemoval, context: ctx))
        var enabled = ctx
        enabled.removalUndoAvailable = true
        XCTAssertTrue(CommandPaletteAvailability.isEnabled(.undoRemoval, context: enabled))
    }
}
