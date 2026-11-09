// Castellum - which blocks of the plain-text body are open.
//
// Pure view state, kept per message and never persisted (decision M2): folds reset to the
// `PlainTextStructure.startsFolded` rule each time a message is opened. Only the indices whose
// state DIFFERS from that default are stored.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

struct PlainFoldState: Equatable {
    /// Block indices the user flipped away from their default.
    private(set) var toggled: Set<Int> = []

    func isFolded(index: Int, block: PlainTextStructure.Block) -> Bool {
        PlainTextStructure.startsFolded(block) != toggled.contains(index)
    }

    mutating func toggle(index: Int) {
        if toggled.contains(index) { toggled.remove(index) } else { toggled.insert(index) }
    }

    /// Open every block that starts folded (the View > Expand All Quotes command).
    mutating func expandAll(_ blocks: [PlainTextStructure.Block]) {
        toggled = Set(blocks.enumerated().filter { PlainTextStructure.startsFolded($0.element) }.map(\.offset))
    }

    mutating func reset() { toggled = [] }

    /// How many blocks are folded right now.
    func foldedCount(_ blocks: [PlainTextStructure.Block]) -> Int {
        blocks.enumerated().filter { isFolded(index: $0.offset, block: $0.element) }.count
    }
}

/// The text selected in the reading pane, tied to the message it was selected in, so Reply can
/// quote just that part and never quotes a selection from another message (MM-17 slice 4).
struct ReadingSelection: Equatable {
    let messageId: String
    let text: String
}
