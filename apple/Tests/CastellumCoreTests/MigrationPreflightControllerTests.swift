// Castellum - launch-time migration state machine tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure logic tests against injected stubs — never the real profile, never
// the Keychain, never a real `NSWindow`. `showWindow`/`hideWindow` are
// plain closures here, not `MigratingWindowController`; the real AppKit
// window is exercised only by the manual GUI check (see the PR body).

import XCTest
@testable import Castellum

final class MigrationPreflightControllerTests: XCTestCase {
    private func tempDir() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-migration-preflight-\(UUID().uuidString)")
    }

    // MARK: - up to date: no window, openMigrating still runs on the caller's thread

    func testUpToDateNeverShowsTheWindowAndStillCallsOpenMigrating() {
        var openMigratingCalls = 0
        var shown = false

        let controller = MigrationPreflightController(
            checkPending: { _ in .upToDate },
            openMigrating: { _ in
                openMigratingCalls += 1
                return nil
            },
            showWindow: { _ in shown = true },
            hideWindow: { XCTFail("must never hide a window that was never shown") }
        )

        let result = controller.run(directory: tempDir())

        XCTAssertNil(result)
        XCTAssertEqual(openMigratingCalls, 1, "the real open must still run when nothing is pending")
        XCTAssertFalse(shown)
        XCTAssertEqual(controller.phase, .done)
    }

    /// An unclassified probe error (e.g. a plain I/O failure) falls through
    /// exactly like `.upToDate` — no window; the real open still decides.
    func testUnknownProbeResultFallsThroughLikeUpToDate() {
        var openMigratingCalls = 0
        var shown = false

        let controller = MigrationPreflightController(
            checkPending: { _ in .unknown },
            openMigrating: { _ in
                openMigratingCalls += 1
                return nil
            },
            showWindow: { _ in shown = true },
            hideWindow: {}
        )

        let result = controller.run(directory: tempDir())

        XCTAssertNil(result)
        XCTAssertEqual(openMigratingCalls, 1)
        XCTAssertFalse(shown)
        XCTAssertEqual(controller.phase, .done)
    }

    // MARK: - launch-blocking from the cheap probe: #169's refusal wins outright

    func testLaunchBlockingFromTheProbeNeverCallsTheRealOpenOrShowsAWindow() {
        var openMigratingCalls = 0
        var shown = false
        let expected = CastellumLaunchError.schemaTooNew(message: "found ahead of supported")

        let controller = MigrationPreflightController(
            checkPending: { _ in .launchBlocking(expected) },
            openMigrating: { _ in
                openMigratingCalls += 1
                return nil
            },
            showWindow: { _ in shown = true },
            hideWindow: { XCTFail("must never hide a window that was never shown") }
        )

        let result = controller.run(directory: tempDir())

        XCTAssertEqual(result, expected)
        XCTAssertEqual(
            openMigratingCalls, 0,
            "the expensive migrating open must never run once the profile is already refused"
        )
        XCTAssertFalse(shown)
        XCTAssertEqual(controller.phase, .failed(expected))
    }

    // MARK: - pending: window shown for the duration, then hidden, real result returned

    func testPendingMigrationShowsTheWindowRunsInTheBackgroundAndHidesOnSuccess() {
        var shownWithHeavy: Bool?
        var hidden = false
        var openMigratingCalls = 0

        let controller = MigrationPreflightController(
            checkPending: { _ in .pending(count: 1, heavy: true) },
            openMigrating: { _ in
                openMigratingCalls += 1
                return nil
            },
            showWindow: { heavy in shownWithHeavy = heavy },
            hideWindow: { hidden = true }
        )

        let result = controller.run(directory: tempDir())

        XCTAssertNil(result)
        XCTAssertEqual(openMigratingCalls, 1)
        XCTAssertEqual(shownWithHeavy, true, "must be shown, and told this migration is heavy")
        XCTAssertTrue(hidden, "must be hidden again once the migrating open finishes")
        XCTAssertEqual(controller.phase, .done)
    }

    /// A migration failure (or SchemaTooNew surfacing only once the real
    /// open runs) must still close the window before the caller shows the
    /// recovery alert — never leave it on screen underneath the alert.
    func testPendingMigrationHidesTheWindowEvenWhenTheRealOpenFails() {
        var hidden = false
        let failure = CastellumLaunchError.migrationFailed(message: "disk full")

        let controller = MigrationPreflightController(
            checkPending: { _ in .pending(count: 1, heavy: false) },
            openMigrating: { _ in failure },
            showWindow: { _ in },
            hideWindow: { hidden = true }
        )

        let result = controller.run(directory: tempDir())

        XCTAssertEqual(result, failure)
        XCTAssertTrue(hidden)
        XCTAssertEqual(controller.phase, .failed(failure))
    }

    /// The window is shown with `heavy: false` when the only pending
    /// migration isn't a heavy one — the copy is the same either way today,
    /// but the flag must still reach `showWindow` correctly.
    func testPendingMigrationPassesHeavyFalseThrough() {
        var shownWithHeavy: Bool?
        let controller = MigrationPreflightController(
            checkPending: { _ in .pending(count: 3, heavy: false) },
            openMigrating: { _ in nil },
            showWindow: { heavy in shownWithHeavy = heavy },
            hideWindow: {}
        )
        _ = controller.run(directory: tempDir())
        XCTAssertEqual(shownWithHeavy, false)
    }

    // MARK: - the event/modal-runner seam (`pumpEventsOnce`)

    /// The injected event pump — `CastellumApp.init` wires this to
    /// `MigratingWindowController.pumpModalEventsOnce` in production, so
    /// the Quit button's clicks actually reach the window (a plain
    /// `RunLoop.main.run(mode: .default, …)` pump never delivers NSEvents —
    /// this is exactly the bug review caught before merge) — must be used
    /// at least once while a migration is pending. The spy wraps the real
    /// default pump rather than replacing it, so the background dispatch's
    /// `DispatchQueue.main.async` hop still drains normally and this stays
    /// deterministic without needing a real `NSWindow`/modal session.
    func testPendingMigrationDrivesTheInjectedEventPumpUntilFinished() {
        var pumpCalls = 0
        let controller = MigrationPreflightController(
            checkPending: { _ in .pending(count: 1, heavy: false) },
            openMigrating: { _ in nil },
            showWindow: { _ in },
            hideWindow: {},
            pumpEventsOnce: {
                pumpCalls += 1
                RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.02))
            }
        )

        let result = controller.run(directory: tempDir())

        XCTAssertNil(result)
        XCTAssertGreaterThan(
            pumpCalls, 0,
            "the pending path must drive the injected event/modal-runner pump, not just wait")
    }

    /// The up-to-date path calls `openMigrating` synchronously on the
    /// caller's thread and never enters the wait loop at all — the event
    /// pump (real AppKit modal session in production) must never be
    /// touched, since there is no window to keep responsive.
    func testUpToDateNeverDrivesTheEventPump() {
        let controller = MigrationPreflightController(
            checkPending: { _ in .upToDate },
            openMigrating: { _ in nil },
            showWindow: { _ in },
            hideWindow: {},
            pumpEventsOnce: { XCTFail("must never pump events when nothing is pending") }
        )

        _ = controller.run(directory: tempDir())
    }

    // MARK: - phase starts at .checking before `run` is ever called

    func testPhaseStartsAtChecking() {
        let controller = MigrationPreflightController(
            checkPending: { _ in .upToDate },
            openMigrating: { _ in nil }
        )
        XCTAssertEqual(controller.phase, .checking)
    }
}
