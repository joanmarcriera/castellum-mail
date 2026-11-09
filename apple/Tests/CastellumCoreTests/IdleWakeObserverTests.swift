// Castellum - IdleWakeObserver tests (S31): sleep/wake and stop semantics,
// driven by an injected NotificationCenter rather than a real system sleep.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class IdleWakeObserverTests: XCTestCase {
    func testWakeNotificationInvokesTheClosure() {
        let center = NotificationCenter()
        var callCount = 0
        let observer = IdleWakeObserver(center: center) { callCount += 1 }
        observer.start()
        // `start()` also arms a REAL `NWPathMonitor`, which fires its own
        // initial path callback (the current path, not a change) shortly
        // after starting — that is genuine, correct behaviour, not something
        // to disable just for this test. Let it settle before measuring the
        // wake notification's own effect, so this test isolates exactly what
        // its name says rather than racing the path monitor's startup fire.
        let settled = XCTestExpectation(description: "initial path callback settled")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { settled.fulfill() }
        wait(for: [settled], timeout: 2.0)
        let baseline = callCount

        center.post(name: NSWorkspace.didWakeNotification, object: nil)
        // NotificationCenter delivers synchronously on .main for a same-thread
        // post in this configuration; give one run-loop turn to be safe.
        let expectation = XCTestExpectation(description: "wake delivered")
        DispatchQueue.main.async { expectation.fulfill() }
        wait(for: [expectation], timeout: 1.0)
        XCTAssertEqual(callCount, baseline + 1)
        observer.stop()
    }

    func testHandleWakeCanBeCalledDirectly() {
        var callCount = 0
        let observer = IdleWakeObserver(center: NotificationCenter()) { callCount += 1 }
        observer.handleWake()
        observer.handleWake()
        XCTAssertEqual(callCount, 2)
    }

    func testStopRemovesTheObserverSoLaterPostsDoNothing() {
        let center = NotificationCenter()
        var callCount = 0
        let observer = IdleWakeObserver(center: center) { callCount += 1 }
        observer.start()
        observer.stop()
        center.post(name: NSWorkspace.didWakeNotification, object: nil)
        XCTAssertEqual(callCount, 0)
    }
}
