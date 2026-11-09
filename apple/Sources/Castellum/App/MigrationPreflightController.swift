// Castellum - the launch-time migration state machine.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `CastellumApp.init` used to call `AppModel.preflightProfileOpen(at:)`
// directly and block: on a profile with a pending migration (#178's heavy
// 0038 rebuild can take ~8s plus a ~1.6s backup on a 229k-message profile),
// the app appeared to freeze with no window at all. This type decides
// whether to show the "Upgrading your mail database…" window BEFORE
// running that same migrating open, and drives it through to completion —
// pure decision logic, injectable end to end, so it's unit-testable
// without a real profile, Keychain, or AppKit window (see
// `MigrationPreflightControllerTests`). `CastellumApp.init` wires the
// production closures (the real FFI probe/open calls and a real
// `MigratingWindowController`) and calls `run(directory:)` once, getting
// back exactly the `CastellumLaunchError?` `preflightProfileOpen` used to
// return directly.
//
// Deliberately NOT a SwiftUI/@StateObject type — see DECISIONS.md's
// 2026-09-25 (#169) entry for why touching AttributeGraph from
// `CastellumApp.init`'s call stack would SIGABRT. This stays a plain class,
// same as `MigratingWindowController`.

import Foundation

final class MigrationPreflightController {
    /// What the cheap, read-only pending-migration probe found.
    enum PendingCheck: Equatable {
        case upToDate
        case pending(count: Int, heavy: Bool)
        /// The probe itself hit a fully classified launch-blocking refusal
        /// (in practice always `.schemaTooNew` — this probe never runs a
        /// migration, so it can't hit `.migrationFailed`) — decided WITHOUT
        /// ever calling the real, expensive migrating open. The existing
        /// #169 refusal path still wins: no window, no migrating open
        /// attempted.
        case launchBlocking(CastellumLaunchError)
        /// Anything else (a plain I/O error, e.g.): fall through exactly
        /// like `AppModel.preflightProfileOpen`'s existing `nil` case used
        /// to for every non-classified failure — no window; call the real
        /// open and let ITS result (or `AppModel`'s `fatalError`) decide.
        case unknown
    }

    /// Mirrors the goal's launch-state machine: pending -> migrating ->
    /// done / failed / too-new. `checking`/`upToDate` cover the "no window"
    /// path explicitly rather than folding it into `.done` so a test can
    /// assert a window was never shown for it.
    enum Phase: Equatable {
        case checking
        case upToDate
        case migrating(heavy: Bool)
        case done
        case failed(CastellumLaunchError)
    }

    private let checkPending: (URL) -> PendingCheck
    private let openMigrating: (URL) -> CastellumLaunchError?
    private let dispatchBackground: (@escaping () -> Void) -> Void
    private let showWindow: (Bool) -> Void
    private let hideWindow: () -> Void
    private let pumpEventsOnce: () -> Void

    private(set) var phase: Phase = .checking

    /// - checkPending/openMigrating: the two FFI-backed decisions (default
    ///   to the real Rust core / `AppModel.preflightProfileOpen`).
    /// - dispatchBackground: where `openMigrating` actually runs when a
    ///   window is shown (default: a background queue) — kept injectable
    ///   so a test can run it synchronously and assert ordering without
    ///   real threading.
    /// - showWindow/hideWindow: the real window's `show()`/`hide()` by
    ///   default; a no-op unless overridden.
    /// - pumpEventsOnce: how the calling (main) thread waits for the
    ///   background open to finish while still servicing the window's
    ///   event loop. THE MODAL RUNNER SEAM: the default here is a plain
    ///   `RunLoop.main.run(mode: .default, …)` pump, which is fine for
    ///   tests (and was this app's first, wrong, production wiring — a
    ///   `.default`-mode pump never delivers NSEvents, so a real window's
    ///   button could never actually be clicked). `CastellumApp.init`
    ///   overrides this with `MigratingWindowController.pumpModalEventsOnce`
    ///   (an `NSApp.beginModalSession`/`runModalSession` loop — the same
    ///   mechanism `NSAlert.runModal()` uses), which DOES deliver real
    ///   events to the window `showWindow` opened. A test overrides this to
    ///   something that doesn't touch `RunLoop.main` (or any real AppKit
    ///   session) at all, and can assert on how many times it was called —
    ///   see `MigrationPreflightControllerTests`.
    init(
        checkPending: @escaping (URL) -> PendingCheck,
        openMigrating: @escaping (URL) -> CastellumLaunchError?,
        dispatchBackground: @escaping (@escaping () -> Void) -> Void = { work in
            DispatchQueue.global(qos: .userInitiated).async(execute: work)
        },
        showWindow: @escaping (Bool) -> Void = { _ in },
        hideWindow: @escaping () -> Void = {},
        pumpEventsOnce: @escaping () -> Void = {
            RunLoop.main.run(mode: .default, before: Date().addingTimeInterval(0.02))
        }
    ) {
        self.checkPending = checkPending
        self.openMigrating = openMigrating
        self.dispatchBackground = dispatchBackground
        self.showWindow = showWindow
        self.hideWindow = hideWindow
        self.pumpEventsOnce = pumpEventsOnce
    }

    /// Runs to completion before returning — from the caller's point of
    /// view this is a drop-in replacement for calling
    /// `AppModel.preflightProfileOpen(at:)` directly. When nothing is
    /// pending (or the probe couldn't tell), `openMigrating` runs
    /// synchronously on the CALLING thread, exactly as it always did —
    /// zero added latency, no window, no background dispatch. When a
    /// migration is pending, the window is shown first, `openMigrating`
    /// runs on `dispatchBackground`'s queue, and the calling thread pumps
    /// events (`pumpEventsOnce` — the modal runner in production) until
    /// that finishes, so the window keeps animating AND stays clickable
    /// (its Quit button) instead of the process looking hung.
    @discardableResult
    func run(directory: URL) -> CastellumLaunchError? {
        switch checkPending(directory) {
        case .launchBlocking(let error):
            phase = .failed(error)
            return error

        case .upToDate, .unknown:
            phase = .upToDate
            let result = openMigrating(directory)
            phase = result.map(Phase.failed) ?? .done
            return result

        case .pending(_, let heavy):
            phase = .migrating(heavy: heavy)
            showWindow(heavy)

            var finished = false
            var result: CastellumLaunchError?
            dispatchBackground { [openMigrating] in
                let r = openMigrating(directory)
                // Hop back so `finished`/`result` are only ever written on
                // the thread `pumpEventsOnce` services (the caller's,
                // normally main) — the event pump is what drains this.
                DispatchQueue.main.async {
                    result = r
                    finished = true
                }
            }
            while !finished {
                pumpEventsOnce()
            }

            hideWindow()
            phase = result.map(Phase.failed) ?? .done
            return result
        }
    }
}
