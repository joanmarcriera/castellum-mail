// Castellum - detects when the app is only the host process for XCTest.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `xcodebuild test` launches the full app to host the unit-test bundle. Before
// this guard, that launch opened the developer's REAL profile, synced every
// real Keychain account, and ran the update check and the Ollama probe. On
// 2026-09-17 a test run synced the real profile's accounts, and the junk that
// test runs had left in the real Keychain namespace made the shipped 0.3.5
// prompt for the keychain password over and over. Under test, the app now
// opens a throwaway profile (which also gets its own Keychain namespace) and
// skips launch-time work entirely.

import Foundation

enum TestHost {
    /// True when this process is hosting XCTest.
    static let isActive: Bool = isTestHost(environment: ProcessInfo.processInfo.environment)

    /// DEV/TEST-ONLY — never set in a shipped build or a real user's
    /// environment. True under the S64 manual GUI launch probe
    /// (`scripts/perf/launch-probe.sh`, `CASTELLUM_PERF_PROBE=1`): a real,
    /// non-XCTest launch that still must never touch the developer's real
    /// Keychain namespace — same reasoning as `isActive`'s doc comment, just
    /// a different trigger. Every reader of this flag (`usesInMemoryKeychain`
    /// below, `AppModel.launch`'s no-network/no-sync branch,
    /// `MessageListView.Coordinator.emitFirstListPerfSignalIfNeeded`'s
    /// terminate-after-first-render) changes behaviour a real user must never
    /// see: the app silently runs against an in-memory Keychain (accounts
    /// don't persist across launches) and, in `AppModel`, skips IMAP sync
    /// and the periodic resync entirely.
    static let isPerfProbe: Bool = ProcessInfo.processInfo.environment["CASTELLUM_PERF_PROBE"] == "1"

    /// Whether `AccountStore` should default to the
    /// in-memory Keychain backing. True for BOTH `isActive` and
    /// `isPerfProbe` — the probe launches the real app binary outside
    /// XCTest, so `isActive` alone would miss it.
    static let usesInMemoryKeychain: Bool = isActive || isPerfProbe || isUIScript

    /// DEV/TEST-ONLY (UIT-1) — path of a UI-script scenario JSON
    /// (`CASTELLUM_UI_SCRIPT`), set only by `scripts/ui-check.sh`. Compiled
    /// out of Release entirely: a shipped build has no such symbol and can
    /// never honour the variable. See `App/UIHarness/UIScriptRunner.swift`.
    #if DEBUG
    static let uiScriptPath: String? = {
        let v = ProcessInfo.processInfo.environment["CASTELLUM_UI_SCRIPT"] ?? ""
        return v.isEmpty ? nil : v
    }()
    #else
    static let uiScriptPath: String? = nil
    #endif

    /// True when the app was launched to run a UI-script scenario. Like
    /// `isPerfProbe` it forces the in-memory Keychain (a scenario must never
    /// leave items in the real Keychain) and, via `suppressesLaunchSideEffects`,
    /// skips network/prompt side effects.
    static var isUIScript: Bool { uiScriptPath != nil }

    /// No update check, Ollama probe, or Contacts/EventKit prompt: true under
    /// XCTest and under a UI script.
    static var suppressesLaunchSideEffects: Bool { isActive || isUIScript }

    /// Pure form of `isActive`, for tests. Xcode sets these variables in the
    /// host process it launches for a hosted unit-test bundle.
    static func isTestHost(environment: [String: String]) -> Bool {
        ["XCTestConfigurationFilePath", "XCTestBundlePath", "XCTestSessionIdentifier"]
            .contains { !(environment[$0] ?? "").isEmpty }
    }

    /// Profile for the app's own `AppModel`: a fresh temp directory under
    /// test, `nil` (the real profile) otherwise.
    static let profileDirectory: URL? = {
        guard isActive else { return nil }
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-testhost-\(UUID().uuidString)", isDirectory: true)
        // One directory per test-host run used to be left in $TMPDIR for ever;
        // remove it when the host exits. Only this exact directory.
        testHostProfilePathForExit = dir.path
        atexit { if let p = testHostProfilePathForExit { try? FileManager.default.removeItem(atPath: p) } }
        return dir
    }()

    /// DEV/TEST-ONLY — never set outside a manual GUI check, and compiled
    /// out of Release entirely (`#if DEBUG`) so a Release build can never
    /// honour this env var even by accident.'s "Upgrading your
    /// mail database…" window only stays on screen for as long as the real
    /// migration takes, which is milliseconds against an otherwise-empty or
    /// single-migration temp profile — too fast to reliably screenshot.
    /// `CastellumApp.init` pads ONLY the migrating-window background dispatch
    /// (never the plain, no-window `preflightProfileOpen` call on the
    /// up-to-date path) with this delay via `Thread.sleep(forTimeInterval:)`,
    /// WITHOUT faking whether a migration is pending — that part is always
    /// the real `castellum_pending_migration_count` probe against whatever
    /// profile is actually on disk. Stored already converted to seconds (a
    /// `Double`, not milliseconds as a fixed-width integer) specifically so
    /// no arithmetic here can overflow the way `UInt32(ms) * 1000` would for
    /// a delay above ~4295 s when handed to `usleep`.
    #if DEBUG
    static let fakeMigrationDelaySeconds: Double? =
        ProcessInfo.processInfo.environment["CASTELLUM_FAKE_MIGRATION_DELAY_MS"]
        .flatMap(Double.init)
        .map { $0 / 1000 }
    #else
    static let fakeMigrationDelaySeconds: Double? = nil
    #endif
}

/// Read by the C exit hook above, which cannot capture context.
nonisolated(unsafe) private var testHostProfilePathForExit: String?
