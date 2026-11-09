// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// GUI-2 regression: a stale dismissal from a sheet that has
/// already gone away must never close the sheet that replaced it.
@MainActor
final class SheetLifetimeTests: XCTestCase {
    func testDismissRunsWhileSheetIsOpen() {
        let lifetime = SheetLifetime()
        var dismissed = 0
        XCTAssertTrue(lifetime.dismissIfOpen { dismissed += 1 })
        XCTAssertEqual(dismissed, 1)
    }

    func testDismissIsSuppressedAfterSheetDisappears() {
        let lifetime = SheetLifetime()
        lifetime.close()
        var dismissed = 0
        XCTAssertFalse(lifetime.dismissIfOpen { dismissed += 1 })
        XCTAssertEqual(dismissed, 0)
    }

    /// The reported flow: sheet A sends its first sync to the background and
    /// closes; sheet B opens; A's sync completes and fires its deferred close.
    /// Only B's own lifetime may authorise closing B.
    func testBackgroundedSheetCannotDismissItsSuccessor() {
        let sheetA = SheetLifetime()
        sheetA.close()                       // "Continue in Background"
        let sheetB = SheetLifetime()         // user starts adding bob
        var closedB = 0
        sheetA.dismissIfOpen { closedB += 1 }  // A's late completion
        XCTAssertEqual(closedB, 0)
        XCTAssertTrue(sheetB.isOpen)
    }

    func testCloseIsIdempotent() {
        let lifetime = SheetLifetime()
        lifetime.close(); lifetime.close()
        XCTAssertFalse(lifetime.isOpen)
    }
}
