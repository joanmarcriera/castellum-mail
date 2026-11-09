// Castellum - CommandPaletteExecutor: dismiss synchronously, post async.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class CommandPaletteExecutorTests: XCTestCase {
    func testDismissRunsBeforePostAndPostCarriesTheSpecsNotification() {
        var events: [String] = []
        let posted = expectation(description: "notification posted")
        let executor = CommandPaletteExecutor(
            dismiss: { events.append("dismiss") },
            post: { name in
                events.append("post:\(name.rawValue)")
                posted.fulfill()
            })
        executor.execute(CommandRegistry.spec(for: .archive))
        XCTAssertEqual(events, ["dismiss"], "dismiss must happen synchronously, before post")
        wait(for: [posted], timeout: 1)
        XCTAssertEqual(events, ["dismiss", "post:castellum.archive"])
    }

    func testACommandWithNoNotificationDoesNothing() {
        var events: [String] = []
        let executor = CommandPaletteExecutor(
            dismiss: { events.append("dismiss") },
            post: { events.append("post:\($0.rawValue)") })
        // sendComposer has no notification (a SwiftUI shortcut in the compose window).
        executor.execute(CommandRegistry.spec(for: .sendComposer))
        XCTAssertEqual(events, [], "a command with no notification must not even dismiss")
    }
}
