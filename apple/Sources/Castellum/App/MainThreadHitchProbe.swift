// Castellum - DEBUG-only main-thread hitch probe.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
//'s other two items were a perf gate (`read_perf_gate.rs`/the
// Swift `LaunchPathPerfTests`, see project-docs/perf/read-path-budgets.md)
// and a real fix (the DB-side incremental thread rebuild in this same
// change). This is the third: a cheap way to SEE a main-thread stall while
// poking around the app interactively, without attaching Instruments or
// `sample`-ing the process by hand — a runloop observer that times how long
// the main run loop spends outside `.beforeWaiting` (i.e. actually doing
// work: SwiftUI updates, `Coordinator.apply`, anything synchronous on the
// main thread including a stray blocking FFI call) and logs whenever that
// exceeds a threshold.
//
// DEV-ONLY, same posture as `CASTELLUM_PERF_PROBE`/`TestHost.isPerfProbe`
// (see that type's doc comment): compiled out of Release entirely
// (`#if DEBUG`), and even in a Debug build it does nothing unless
// `CASTELLUM_HITCH_PROBE=1` is set AND this is not an XCTest host run —
// never enabled for a real user, and never adds a runloop observer to a
// test host's run loop. Logs to `Log.perf` at `.notice` (persisted, so a
// hitch during manual testing survives to `log show`/Console.app without
// needing the Console window open at the time).

#if DEBUG
import Foundation
import os

@MainActor
enum MainThreadHitchProbe {
    /// Below this, ordinary per-frame SwiftUI/AppKit work is expected and
    /// not worth a log line — 100ms is roughly 6 dropped 60Hz frames, well
    /// past "feels fine" (see the GUI perf budgets in
    /// project-docs/perf/read-path-budgets.md, whose main-thread ceilings
    /// are all under 45ms).
    static let hitchThreshold: TimeInterval = 0.1

    private static var observer: CFRunLoopObserver?
    private static var workStartedAt: ContinuousClock.Instant?

    /// Installs the observer on the main run loop, if enabled. Safe to call
    /// more than once (a second call is a no-op) and safe to call from a
    /// build where `CASTELLUM_HITCH_PROBE` is unset (does nothing).
    static func installIfEnabled() {
        guard observer == nil else { return }
        guard !TestHost.isActive else { return }
        guard ProcessInfo.processInfo.environment["CASTELLUM_HITCH_PROBE"] == "1" else { return }

        // `.exit` marks the end of whatever activity (timers, sources,
        // blocks) the run loop just finished dispatching; `.beforeWaiting`
        // marks it about to go idle. The gap between one iteration's
        // `.entry`/first `.exit` and the next `.beforeWaiting` is "how long
        // this pass of the loop kept the main thread busy" -- so this
        // stamps a start on `.afterWaiting` (the loop just woke up to do
        // something) and measures on `.beforeWaiting` (about to go back to
        // sleep), which brackets exactly the "doing work" span once per
        // iteration.
        let mask = CFRunLoopActivity.afterWaiting.rawValue | CFRunLoopActivity.beforeWaiting.rawValue
        let obs = CFRunLoopObserverCreateWithHandler(kCFAllocatorDefault, mask, true, 0) { _, activity in
            MainActor.assumeIsolated {
                switch activity {
                case .afterWaiting:
                    workStartedAt = .now
                case .beforeWaiting:
                    guard let started = workStartedAt else { return }
                    workStartedAt = nil
                    let elapsed = started.duration(to: .now)
                    let seconds = Double(elapsed.components.seconds)
                        + Double(elapsed.components.attoseconds) / 1e18
                    if seconds >= hitchThreshold {
                        Log.perf.notice(
                            "hitch: main thread busy for \(seconds * 1000, format: .fixed(precision: 1), privacy: .public)ms"
                        )
                    }
                default:
                    break
                }
            }
        }
        CFRunLoopAddObserver(CFRunLoopGetMain(), obs, .commonModes)
        observer = obs
        Log.perf.notice("hitch probe: installed (CASTELLUM_HITCH_PROBE=1, threshold=\(hitchThreshold * 1000, format: .fixed(precision: 0), privacy: .public)ms)")
    }
}
#endif
