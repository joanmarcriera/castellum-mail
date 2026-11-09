// Castellum - app delegate: mailto: URLs, the Services provider, and the
// Share Extension handoff (S57).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// SwiftUI's WindowGroup has no built-in hook for a custom URL scheme opened
// while the app is (or isn't yet) running — the traditional, most reliable
// way to catch it on macOS is the Apple Event `GetURL` handler, registered
// in `applicationWillFinishLaunching` so Launch Services never drops the
// event that cold-launched the app. `AppModel` doesn't exist yet at that
// point (it's a SwiftUI `@StateObject`), so an event that arrives before
// `modelBecameAvailable` is queued and flushed then.
//
// Two S57 cold-launch bugs, found and reproduced 2026-09-26, are fixed here:
//  1. Launched via a GetURL Apple Event (as opposed to a Finder/Dock click),
//     macOS neither activates the app nor orders the WindowGroup's window
//     front — SwiftUI still creates it (this file's `modelBecameAvailable`
//     runs from that very window's content view's `onAppear`), it just
//     stays inactive/behind until the user manually clicks the Dock icon.
//     `activateMainWindow` does explicitly what that click does — but ONLY
//     when a URL or Share handoff was actually waiting at that moment (a
//     queued `pendingURLs` entry or a pending `SharePendingCompose`), never
//     on an ordinary Finder/Dock launch or a background Sparkle relaunch,
//     either of which `NSApp.activate(ignoringOtherApps:)` would otherwise
//     yank to the front for no reason (PR #182 review).
//  2. `modelBecameAvailable` used to flush `pendingURLs` immediately, but
//     `AppModel.accounts` is only populated once `reloadAccounts()`'s
//     Task.detached FFI call returns — a queued mailto: could race that and
//     find `accounts` still empty, losing the compose window (Cmd-N and a
//     mailto: to an already-running instance never hit this, since by then
//     accounts are long since loaded). The flush now waits for
//     `AppModel.onAccountsLoaded` before draining the queue, and the same
//     guard covers `handleIncomingURL`/`applicationDidBecomeActive` for a
//     URL or Share Extension handoff that arrives in that same window.
//     `onAccountsLoaded` fires on a failed `reloadAccounts()` pass too (see
//     its call site in `AppModel.reloadAccounts`), so a `listAccounts()` FFI/
//     DB error can never strand a queued URL forever — the flush still runs
//     and falls through to the existing no-account/error alert.
//
// No network call anywhere in this file; nothing handled here is logged
// beyond what a normal compose draft already persists.

import AppKit
// kInternetEventClass/kAEGetURL (ApplicationServices/HIServices) and
// keyDirectObject (CoreServices/AE) — NOT under the `Carbon` module on this
// SDK, despite older sample code importing `Carbon.Carbon` for them.
import ApplicationServices
import CoreServices
import CoreSpotlight
import SwiftUI

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private(set) var model: AppModel?
    private let servicesProvider = ServicesProvider()
    /// Shows every write-safety dialog (see `WriteSafetyPresenter`).
    private var writeSafetyPresenter: WriteSafetyPresenter?
    /// Menu routes that must outlive the main window (FP44).
    private var commandRouter: AppCommandRouter?
    /// Lets Window > Main Window reopen a closed main window (FP44).
    var openMainWindowAction: OpenWindowAction?
    private var pendingURLs: [String] = []
    /// A Spotlight result clicked before the model/accounts were ready (#3382).
    /// Only the latest is kept: two clicks before launch finished mean the
    /// user wants the second.
    private var pendingSpotlightMessageId: String?
    /// The last delivered id and when, to drop a double delivery (both the
    /// AppDelegate and the SwiftUI entry point can fire for one click).
    private var lastSpotlightDelivery: (id: String, at: Date)?
    /// Window within which the same id is treated as one click.
    static let spotlightDuplicateWindow: TimeInterval = 2

    /// Test seam, mirrors `openMailtoLink`: how a delivered Spotlight id opens.
    var openSpotlightResult: (String, AppModel) -> Void = { id, model in
        model.openSpotlightResult(messageId: id)
    }
    /// Test seam: the clock for the duplicate guard.
    var spotlightClock: () -> Date = Date.init

    /// Test seam: how a mailto: URL (queued or delivered immediately) opens
    /// a compose window. Production default is the real `MailtoOpener`;
    /// tests substitute a recording closure so no live `NSWindow` is ever
    /// created off a temp/no-account profile.
    var openMailtoLink: (String, AppModel) -> Void = MailtoOpener.openMailtoLink

    /// Test seam: how a Share Extension handoff opens a compose window —
    /// mirrors `openMailtoLink` for the same reason.
    var openSharePendingCompose: (MailtoPrefill, AppModel) -> Void = MailtoOpener.open

    /// Test seam: activates the app and orders its window(s) front (S57 bug
    /// #1 above). Overridden in tests so they never touch real `NSApp` state.
    var activateMainWindow: () -> Void = {
        NSApp.activate(ignoringOtherApps: true)
        for window in NSApp.windows where window.canBecomeKey {
            window.makeKeyAndOrderFront(nil)
        }
    }

    func applicationWillFinishLaunching(_ notification: Notification) {
        NSAppleEventManager.shared().setEventHandler(
            self, andSelector: #selector(handleGetURLEvent(_:withReplyEvent:)),
            forEventClass: AEEventClass(kInternetEventClass), andEventID: AEEventID(kAEGetURL))
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        Log.lifecycle.info("app: launched")
        NSApp.servicesProvider = servicesProvider
        #if DEBUG
        MainThreadHitchProbe.installIfEnabled()
        #endif
    }

    /// F1: stop the background embeddings loop at quit (no new batch starts after this).
    func applicationWillTerminate(_ notification: Notification) {
        model?.aiCoordinator.embeddings.shutdown()
    }

    /// A Share Extension handoff activates the main app rather than sending
    /// it a URL, so it's picked up here rather than via the GetURL handler
    /// below. Guarded on `accountsLoaded` for the same reason
    /// `handleIncomingURL` is (S57 bug #2): an activation racing the very
    /// first `reloadAccounts()` pass must not consume the handoff before
    /// `AppModel` is ready for it. `modelBecameAvailable`'s own
    /// `onAccountsLoaded` callback covers that case instead — whichever of
    /// the two runs first wins, `SharePendingCompose.consumePending()` is a
    /// single-consumption read, so the other is just a harmless no-op.
    func applicationDidBecomeActive(_ notification: Notification) {
        guard let model, model.accountsLoaded else { return }
        consumePendingShare(model: model)
    }

    /// Spotlight (CoreSpotlight) result clicked (#3382). Returns true once the
    /// activity was ours. SwiftUI's `.onContinueUserActivity` is also wired in
    /// `CastellumApp`; whichever fires first wins and the duplicate guard drops
    /// the other.
    func application(
        _ application: NSApplication, continue userActivity: NSUserActivity,
        restorationHandler: @escaping ([NSUserActivityRestoring]) -> Void
    ) -> Bool {
        guard let id = Self.spotlightMessageId(from: userActivity) else { return false }
        handleSpotlightContinuation(messageId: id)
        return true
    }

    /// Pure. The message id of a CoreSpotlight activity, or nil for any other
    /// activity or an id that fails validation. The id comes from another
    /// process, so it is untrusted (T4): non-empty, at most 1024 UTF-8 bytes,
    /// no NUL (`withCString` would silently truncate there). It is used only as
    /// a lookup key and never parsed.
    static func spotlightMessageId(from userActivity: NSUserActivity) -> String? {
        guard userActivity.activityType == CSSearchableItemActionType,
              let id = userActivity.userInfo?[CSSearchableItemActivityIdentifier] as? String
        else { return nil }
        return validSpotlightId(id)
    }

    static func validSpotlightId(_ id: String) -> String? {
        guard !id.isEmpty, id.utf8.count <= 1024, !id.utf8.contains(0) else { return nil }
        return id
    }

    /// The testable core: queue while the model or its accounts are not ready
    /// (a cold launch), else deliver now. Warm delivery fronts the window first.
    func handleSpotlightContinuation(messageId: String) {
        guard let id = Self.validSpotlightId(messageId) else { return }
        guard let model, model.accountsLoaded else {
            pendingSpotlightMessageId = id   // latest wins
            return
        }
        deliverSpotlight(id, model: model, activate: true)
    }

    private func deliverSpotlight(_ id: String, model: AppModel, activate: Bool) {
        let now = spotlightClock()
        if let last = lastSpotlightDelivery, last.id == id,
           now.timeIntervalSince(last.at) < Self.spotlightDuplicateWindow {
            return
        }
        lastSpotlightDelivery = (id, now)
        Log.lifecycle.info("spotlight: opening result \(id, privacy: .private)")
        if activate { activateMainWindow() }
        openSpotlightResult(id, model)
    }

    @objc private func handleGetURLEvent(
        _ event: NSAppleEventDescriptor, withReplyEvent: NSAppleEventDescriptor
    ) {
        guard let urlString = event.paramDescriptor(forKeyword: keyDirectObject)?.stringValue
        else { return }
        handleIncomingURL(urlString)
    }

    /// The testable core of `handleGetURLEvent` — split out so tests can
    /// drive it with plain strings instead of constructing
    /// `NSAppleEventDescriptor`s (AppKit/Apple Event plumbing that isn't
    /// worth re-testing here). Queues `urlString` whenever `model` isn't set
    /// yet OR its accounts haven't loaded yet (S57 bug #2); otherwise opens
    /// it immediately, exactly like every URL later flushed from the queue.
    func handleIncomingURL(_ urlString: String) {
        guard let model, model.accountsLoaded else {
            pendingURLs.append(urlString)
            return
        }
        openMailtoLink(urlString, model)
    }

    /// Called once from `RootView.onAppear` (CastellumApp), after which
    /// `AppModel` exists for the rest of the app's life. Activates/orders the
    /// main window front (S57 bug #1) ONLY when a `mailto:`/Share URL is
    /// actually waiting at this exact moment — i.e. this is a genuine cold
    /// launch driven by one of them, per the real-world ordering that made
    /// bug #2 possible in the first place: a `GetURL` Apple Event that
    /// cold-launches the app arrives (and gets queued into `pendingURLs`)
    /// before SwiftUI finishes presenting the window and calling this. An
    /// ordinary Finder/Dock launch, or a background Sparkle relaunch, has
    /// nothing queued here and must never have its focus stolen by
    /// `NSApp.activate(ignoringOtherApps:)`. Either way, this then waits for
    /// `AppModel.onAccountsLoaded` before flushing any `mailto:` link that
    /// raced launch (S57 bug #2) and checking for a Share Extension handoff
    /// already waiting.
    func modelBecameAvailable(_ model: AppModel) {
        self.model = model
        servicesProvider.model = model
        // WS1: one presenter for every write-safety dialog, on whichever
        // window the user acted in. Not under XCTest: unit tests answer the
        // prompt through the model and must never put a modal on screen.
        if writeSafetyPresenter == nil, !TestHost.isActive {
            writeSafetyPresenter = WriteSafetyPresenter(model: model)
        }
        // FP44: once per app life (RootView reappears with each main window).
        // Not under XCTest, for the same reason as the presenter above.
        if commandRouter == nil, !TestHost.isActive {
            commandRouter = AppCommandRouter(model: model, openWindow: { [weak self] in
                self?.openMainWindowAction
            })
        }
        if !pendingURLs.isEmpty || SharePendingCompose.isPending || pendingSpotlightMessageId != nil {
            activateMainWindow()
        }
        model.onAccountsLoaded { [weak self] in
            self?.flushPendingURLs(model: model)
        }
        #if DEBUG
        UIScriptRunner.startIfRequested(model: model)   // no-op unless CASTELLUM_UI_SCRIPT is set
        #endif
    }

    /// Drains `pendingURLs` — every URL queued by `handleIncomingURL` while
    /// accounts weren't loaded yet — in arrival order, then checks for a
    /// Share Extension handoff. Called exactly once, after
    /// `AppModel.onAccountsLoaded` fires, so it never runs twice for the
    /// same queue. Never drops a URL: with no account configured,
    /// `MailtoOpener.open`'s `composeAccountIdOrPromptUnavailable()` still
    /// surfaces the existing no-account alert (#138) instead of silently
    /// doing nothing.
    private func flushPendingURLs(model: AppModel) {
        let queued = pendingURLs
        pendingURLs.removeAll()
        queued.forEach { openMailtoLink($0, model) }
        consumePendingShare(model: model)
        if let id = pendingSpotlightMessageId {
            pendingSpotlightMessageId = nil
            // The window was already fronted in `modelBecameAvailable`.
            deliverSpotlight(id, model: model, activate: false)
        }
    }

    private func consumePendingShare(model: AppModel) {
        guard let pending = SharePendingCompose.consumePending() else { return }
        openSharePendingCompose(
            MailtoPrefill(subject: pending.subject, body: pending.body), model)
    }
}
