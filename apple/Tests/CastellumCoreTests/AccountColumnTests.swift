// Castellum - message-list source-account column.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class AccountColumnTests: XCTestCase {
    private let accounts = [
        Account(id: "acc1", email: "user@example.com", displayName: nil, provider: "imap"),
        Account(id: "acc2", email: "marc@work.example", displayName: nil, provider: "imap"),
    ]

    func testLabelIsTheAccountEmail() {
        XCTAssertEqual(AccountColumn.label(forAccountId: "acc2", in: accounts), "marc@work.example")
    }

    /// Never blank for an unknown id: the account list loads asynchronously,
    /// and an empty cell would read as "this mail has no account" rather than
    /// "the accounts have not arrived yet".
    func testUnknownAccountFallsBackToTheIdRatherThanBlank() {
        XCTAssertEqual(AccountColumn.label(forAccountId: "ghost", in: accounts), "ghost")
        XCTAssertEqual(AccountColumn.label(forAccountId: "acc1", in: []), "acc1")
    }

    /// The colour must survive a relaunch and any reordering of `accounts`.
    /// Swift's `hashValue` is seeded per-process and would reshuffle every
    /// account's colour on every launch, which is why this uses FNV-1a.
    func testTintIsStableForAnIdAndInRange() {
        let first = AccountColumn.tintIndex(forAccountId: "acc1")
        XCTAssertEqual(first, AccountColumn.tintIndex(forAccountId: "acc1"))
        for id in ["acc1", "acc2", "a", "", "user@example.com"] {
            let i = AccountColumn.tintIndex(forAccountId: id)
            XCTAssertTrue((0..<AccountColumn.tints.count).contains(i), "\(id) -> \(i)")
        }
    }

    /// Pins the HASH, not the index. Pinning `hash % tints.count` would fail
    /// when a seventh tint is added — a legitimate change that repaints
    /// nothing — while letting a change to the mixing constants through if it
    /// happened to land on the same residues.
    func testHashIsPinnedToKnownVectors() {
        XCTAssertEqual(AccountColumn.hash64("acc1"), 0x0754_9383_F51F_9511)
        XCTAssertEqual(AccountColumn.hash64("acc2"), 0x0754_9083_F51F_8FF8)
    }

    // MARK: - the wiring (a pure-helper test cannot catch these)

    /// Delete the `("account", …)` entry from `kColumns` and every helper test
    /// above still passes while the feature is entirely gone from the UI.
    func testTheAccountColumnIsActuallyInTheMessageList() {
        let matches = kColumns.filter { $0.id == AccountColumn.columnId }
        XCTAssertEqual(matches.count, 1, "exactly one account column")
        XCTAssertEqual(matches[0].title, "Account", "it is a named, visible column")
        XCTAssertLessThanOrEqual(matches[0].width, 120, "quiet by default, not a wide column")
        XCTAssertGreaterThan(matches[0].max, matches[0].width)
    }

    /// The show/hide header menu is built from `kColumns` excluding the two
    /// icon columns, so the account column must be hideable — Marc asked to
    /// keep list density, and a column he cannot turn off is not that.
    func testTheAccountColumnIsHideableFromTheHeaderMenu() {
        let menuIds = kColumns.filter { $0.id != "unread" && $0.id != "star" }.map(\.id)
        XCTAssertTrue(menuIds.contains(AccountColumn.columnId))
    }

    /// Colour is for telling accounts apart. With one account there is nothing
    /// to tell apart, so the column stays as quiet as Preview and Received.
    func testSingleAccountIsNotTinted() {
        XCTAssertEqual(AccountColumn.colour(forAccountId: "acc1", accountCount: 1),
                       NSColor.secondaryLabelColor)
        XCTAssertNotEqual(AccountColumn.colour(forAccountId: "acc1", accountCount: 2),
                          NSColor.secondaryLabelColor)
    }
}
