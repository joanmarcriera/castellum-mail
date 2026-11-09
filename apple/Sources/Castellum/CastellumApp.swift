// Castellum - native macOS app entry point.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import CoreSpotlight
import SwiftUI

@main
struct CastellumApp: App {
    /// S64 manual GUI launch probe (`scripts/perf/launch-probe.sh`): stamped
    /// as the first statement of `init()` below — the earliest Swift code
    /// in the process reaches. `MessageListView.Coordinator.apply` reads it
    /// to print `PERF gui_first_list measured=<ms>ms` the first time the
    /// probe's unified inbox renders non-empty. Dyld and the AppKit/
    /// Objective-C runtime start-up before `init()` are NOT included, so
    /// this is a floor on the true cold-launch time, not the whole of it.
    /// Meaningless outside the probe (`TestHost.isPerfProbe`), which is the
    /// only reader.
    static var launchStart = ContinuousClock.now

    // Handles mailto: links, the Services provider, and the Share Extension
    // handoff (S57) — see AppDelegate.swift.
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var prefs = Preferences.shared
    // Owned here (not inside RootView) so the Settings scene can share it:
    // the Accounts tab reads model.accounts and AppModel.openAccountsSettings
    // (the missing-credentials alert's "Open Accounts…" button, the tracker
    // #2152) drives which tab/account it opens to. WindowGroup and Settings
    // are separate SwiftUI scenes with no shared environment by default, so
    // this single instance is what bridges them.
    // Under XCTest the app is only a host process: never open the real
    // profile (see TestHost).
    @StateObject private var model: AppModel

    // A custom `init`, rather than `model`'s default-value form this had
    // before, so the SchemaTooNew/MigrationFailed launch-refusal preflight
    // — and, since, the migrating-window
    // preflight below — run strictly BEFORE SwiftUI's runtime starts.
    //
    // Why here and not inside `AppModel.init`: `AppModel` is normally built
    // as a `@StateObject`'s default value, which SwiftUI evaluates as part
    // of its OWN machinery (AttributeGraph) even before `body` is read.
    // Calling `NSAlert.runModal()` — a nested run loop — from in there
    // reenters AttributeGraph mid-transaction and aborts (`SIGABRT` in
    // `AttributeGraph`/`SwiftUICore`/`AppKit`, empirically confirmed while
    // building the 2615 change's own GUI check — not a hypothetical). The
    // same reasoning rules out a SwiftUI-hosted migrating window:
    // `MigrationPreflightController`/`MigratingWindowController` are plain
    // AppKit classes for exactly this reason. Plain `init()` on an
    // `App`-conforming struct, by contrast, runs before `.run()` starts
    // that runtime at all, so a blocking `NSAlert` or a manually-pumped
    // `NSWindow` here is safe. If the preflight passes (or hits some OTHER,
    // non-recoverable error), `AppModel`'s own `init` reopens the same
    // profile normally — idempotent, so this costs nothing beyond one
    // extra migration check — and still `fatalError`s on a genuine
    // failure, unchanged.
    init() {
        // Must stay the first statement — see `launchStart`'s doc comment.
        Self.launchStart = ContinuousClock.now
        #if DEBUG
        // UIT-1: refuse an unsafe UI-script launch before ANY profile/defaults access.
        UIScriptRunner.refuseUnsafeLaunchEarly()
        #endif
        let dir = AppModel.resolveProfileDirectory(override: TestHost.profileDirectory)
        //: a cheap pending-migration probe decides whether to
        // show the "Upgrading your mail database…" window before running
        // the same migrating open `preflightProfileOpen` always ran here —
        // see `MigrationPreflightController`'s header for why this has to
        // stay a plain class wired from THIS `init`, not `AppModel`'s.
        let migrationWindow = MigratingWindowController()
        let preflight = MigrationPreflightController(
            checkPending: AppModel.checkPendingMigrations(at:),
            openMigrating: AppModel.preflightProfileOpen(at:),
            dispatchBackground: { work in
                DispatchQueue.global(qos: .userInitiated).async {
                    // DEV/TEST-ONLY, compiled out of Release entirely — see
                    // `TestHost.fakeMigrationDelaySeconds`'s doc comment.
                    // Deliberately applied HERE (the background dispatch
                    // used only on the migrating/pending path) and never
                    // wrapped around `openMigrating` itself, so the
                    // up-to-date path (which calls `openMigrating` directly,
                    // synchronously, with no window) never pays for it.
                    if let seconds = TestHost.fakeMigrationDelaySeconds {
                        Thread.sleep(forTimeInterval: seconds)
                    }
                    work()
                }
            },
            showWindow: { _ in migrationWindow.show() },
            hideWindow: { migrationWindow.hide() },
            // A plain `RunLoop.main.run(mode: .default, …)` pump (the
            // default) never delivers NSEvents, so a click on the window's
            // Quit button would silently do nothing — caught in review
            // before merge. `pumpModalEventsOnce` drives the SAME modal
            // session `show()` began, the mechanism `NSAlert.runModal()`
            // (#169) already uses safely pre-`.run()`; see
            // `MigratingWindowController`'s header comment.
            pumpEventsOnce: { migrationWindow.pumpModalEventsOnce() }
        )
        if let launchError = preflight.run(directory: dir) {
            AppModel.presentLaunchErrorAndQuit(launchError)
        }
        _model = StateObject(wrappedValue: AppModel(profileDirectory: TestHost.profileDirectory))
    }

    var body: some Scene {
        // The id lets Window > Main Window reopen it (AppCommandRouter, FP44).
        WindowGroup("Castellum", id: AppCommandRouter.mainWindowId) {
            RootView(model: model)
                .background(SettingsOpenerInstaller(model: model))
                .onAppear { appDelegate.modelBecameAvailable(model) }
                // #3382: a clicked Spotlight result. Also handled in AppDelegate
                // (`application(_:continue:)`); the delegate's duplicate guard
                // drops whichever arrives second.
                .onContinueUserActivity(CSSearchableItemActionType) { activity in
                    if let id = AppDelegate.spotlightMessageId(from: activity) {
                        appDelegate.handleSpotlightContinuation(messageId: id)
                    }
                }
                .background(AppCommandRouter.Installer(store: { [appDelegate] in
                    appDelegate.openMainWindowAction = $0
                }))
        }
        // AA1: a first-launch window tall enough to host the Add-account sheet
        // (a sheet cannot exceed its parent window) on a 768 pt display.
        .defaultSize(width: 1200, height: 720)
        .commands {
            // Menus are generated from CommandRegistry via the app's
            // KeyBindingsStore — add or change commands in CommandRegistry
            // THERE, never here (CommandRegistryTests pins the structure,
            // chords, and conflict-freedom; KeyBindingsStoreTests pins the
            // override layer this reads through).
            RegistryCommands(
                specs: model.keyBindings.effectiveSpecs,
                // Mailbox-wide actions are off while Correspondence hides the mailbox.
                disabled: model.correspondence != nil ? [.markAllRead, .emptyTrash, .importEml]
                    : (model.searchText.isEmpty ? [] : [.markAllRead]),
                checked: MenuChecks.checked(layout: prefs.viewerLayout, headerMode: prefs.headerMode,
                                            loadsRemoteImages: prefs.alwaysLoadRemoteImages))
        }

        Settings {
            SettingsView(prefs: prefs, model: model)
        }
        // Let the user resize the Settings window (min size from the content)
        // instead of the scene pinning it to the content's ideal size (GUI-3).
        .windowResizability(.contentMinSize)
    }
}
