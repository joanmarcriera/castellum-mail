// Castellum - the "Upgrading your mail database…" launch window.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Shown by `MigrationPreflightController` (wired from `CastellumApp.init`)
// for the duration of a pending migration, so the app never appears to
// freeze with no window while the migrating open runs (#178's heavy 0038
// rebuild can take ~8s plus a ~1.6s backup on a 229k-message profile).
//
// Built directly with AppKit, never SwiftUI/@StateObject: see
// DECISIONS.md's 2026-09-25 entry (#169) for why touching AttributeGraph
// from `CastellumApp.init`'s call stack SIGABRTs — the same reason
// `AppModel.presentLaunchErrorAndQuit` reaches for a plain `NSAlert`
// instead of a SwiftUI one. This window is always closed and released
// before `AppModel` (and its @StateObject) is ever constructed.
//
// Non-dismissable via window chrome (no close/minimize/zoom buttons, no
// Cmd-W, no Escape handler). The explicit "Quit" button IS clickable while
// the window is up: `show()` opens an AppKit modal session
// (`NSApp.beginModalSession(for:)`) for this window, and
// `MigrationPreflightController` drives it (`pumpModalEventsOnce`,
// `NSApp.runModalSession(_:)`) instead of a plain `RunLoop.main.run(mode:
// .default, …)` pump — a bare `.default`-mode pump never delivers NSEvents
// (a first, shipped version of this window had a Quit button that silently
// did nothing, caught by review before merge). This is the exact mechanism
// `NSAlert.runModal()` uses under the hood, and #169 already proved that's
// safe to run from `CastellumApp.init`, strictly BEFORE SwiftUI's `.run()`
// starts. Cmd-Q is NOT expected to work here regardless: there is no menu
// bar yet (`.commands` is only evaluated once `.run()` reaches `body`), so
// even with real event delivery there's no "Quit" menu item's key
// equivalent to catch it — the button is the one deliberate, tested way
// out. It calls `exit(0)` directly rather than routing through
// `NSApplication.terminate` (which needs the app/menu machinery `.run()`
// normally provides). That is safe to do here specifically:
// `castellum_db::Database::run_migration_set` applies every migration in
// its OWN transaction (a plain `Drop` rolls it back — see
// `Database::lock_conn`'s doc comment), so a quit mid-migration leaves the
// schema exactly at the last migration that fully committed, never a
// half-applied one; and SQLite's file lock is a per-process OS resource
// the kernel releases the instant the process exits, so the next launch
// opens the (unchanged or one-migration-further) profile normally.
// Matches the copy's own promise ("if you quit, nothing is lost").

import AppKit

final class MigratingWindowController: NSObject {
    private var window: NSWindow?
    private var modalSession: NSApplication.ModalSession?

    /// Build and show the window, and begin an AppKit modal session for it
    /// — see this file's header comment for why that (not a plain
    /// `RunLoop` pump) is what makes the Quit button clickable. Safe to
    /// call more than once (a second call is a no-op while one is already
    /// showing).
    func show() {
        guard window == nil else { return }

        NSApplication.shared.setActivationPolicy(.regular)
        NSApplication.shared.activate(ignoringOtherApps: true)

        let title = String(localized: "Upgrading your mail database…")
        let detail = String(
            localized:
                "This can take a minute for large mailboxes. Your mail is safe; if you quit, nothing is lost."
        )

        let contentSize = NSSize(width: 360, height: 220)
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: contentSize),
            styleMask: [.titled, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        window.isReleasedWhenClosed = false
        window.title = title
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        // No close/minimize/zoom — non-dismissable via window chrome; the
        // explicit Quit button built below is the one deliberate way out.
        window.standardWindowButton(.closeButton)?.isHidden = true
        window.standardWindowButton(.miniaturizeButton)?.isHidden = true
        window.standardWindowButton(.zoomButton)?.isHidden = true
        window.isMovableByWindowBackground = true
        // A normal level: this is an ordinary launch-time window, not a
        // panel that should float above every other app on the machine.
        window.level = .normal
        window.contentView = buildContent(size: contentSize, title: title, detail: detail)
        window.center()
        window.makeKeyAndOrderFront(nil)

        // No control on this window ever becomes first responder, so
        // VoiceOver has nothing to read just by the window appearing —
        // announce both sentences explicitly.
        NSAccessibility.post(
            element: window,
            notification: .announcementRequested,
            userInfo: [
                .announcement: "\(title) \(detail)",
                .priority: NSAccessibilityPriorityLevel.high.rawValue,
            ]
        )

        self.window = window
        // The modal session is what makes NSEvents (the Quit button's
        // click) actually reach this window — see the header comment.
        // Begun AFTER `makeKeyAndOrderFront`, mirroring `NSAlert.runModal`'s
        // own ordering (show, then enter the modal loop).
        modalSession = NSApplication.shared.beginModalSession(for: window)
    }

    /// Close and release the window (and its modal session, if one is
    /// active). Safe to call even if `show()` was never called (the
    /// up-to-date, no-window path never calls this).
    func hide() {
        if let session = modalSession {
            NSApplication.shared.endModalSession(session)
            modalSession = nil
        }
        window?.orderOut(nil)
        window = nil
    }

    /// Run one cycle of this window's modal session — the real event pump
    /// `MigrationPreflightController` drives instead of a plain `RunLoop`
    /// pump, so the Quit button (and everything else about this window)
    /// gets real NSEvents. A no-op if no session is active (there always
    /// is one while `run(directory:)`'s pending branch is looping — see
    /// `CastellumApp.init`'s wiring — so this only matters for safety, not
    /// normal operation).
    func pumpModalEventsOnce() {
        guard let session = modalSession else { return }
        _ = NSApplication.shared.runModalSession(session)
    }

    /// The Quit button's action. Calls `exit(0)` directly rather than
    /// `NSApplication.shared.terminate(nil)` — this window exists strictly
    /// BEFORE `.run()`, so the normal termination machinery (delegate
    /// callbacks, the app's run loop) isn't running yet to act on it. See
    /// this file's header comment for why quitting here, mid-migration, is
    /// safe: each migration step is its own transaction that rolls back
    /// cleanly, and the SQLite file lock is released by the OS the instant
    /// this process exits.
    @objc private func quitTapped(_ sender: Any?) {
        exit(0)
    }

    private func buildContent(size: NSSize, title: String, detail: String) -> NSView {
        let icon = NSImageView(image: NSApplication.shared.applicationIconImage ?? NSImage())
        icon.imageScaling = .scaleProportionallyUpOrDown
        icon.translatesAutoresizingMaskIntoConstraints = false
        icon.setAccessibilityElement(false)
        icon.widthAnchor.constraint(equalToConstant: 48).isActive = true
        icon.heightAnchor.constraint(equalToConstant: 48).isActive = true

        let titleLabel = NSTextField(labelWithString: title)
        titleLabel.font = .systemFont(ofSize: 13, weight: .semibold)
        titleLabel.alignment = .center
        titleLabel.translatesAutoresizingMaskIntoConstraints = false

        let detailLabel = NSTextField(wrappingLabelWithString: detail)
        detailLabel.font = .systemFont(ofSize: 11)
        detailLabel.textColor = .secondaryLabelColor
        detailLabel.alignment = .center
        detailLabel.translatesAutoresizingMaskIntoConstraints = false
        detailLabel.preferredMaxLayoutWidth = size.width - 48

        let spinner = NSProgressIndicator()
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isIndeterminate = true
        spinner.translatesAutoresizingMaskIntoConstraints = false
        spinner.startAnimation(nil)
        spinner.setAccessibilityElement(false)

        // Icon + both sentences as ONE combined accessibility element, so a
        // VoiceOver user who tabs into this (non-first-responder) window
        // gets the full text as a single stop, in addition to the
        // announcement posted in `show()`. Kept as its own sub-stack
        // (rather than collapsing the whole window's content) specifically
        // so the Quit button below stays a normal, separately-focusable
        // accessible control rather than being swallowed into this group.
        let textStack = NSStackView(views: [icon, titleLabel, detailLabel])
        textStack.orientation = .vertical
        textStack.alignment = .centerX
        textStack.spacing = 12
        textStack.translatesAutoresizingMaskIntoConstraints = false
        textStack.setAccessibilityElement(true)
        textStack.setAccessibilityRole(.group)
        textStack.setAccessibilityLabel("\(title) \(detail)")

        // The one deliberate way out of this non-dismissable window — see
        // this file's header comment for why `quitTapped` is safe to call
        // mid-migration. `#selector` requires an `NSObject` target, hence
        // this class's `NSObject` conformance.
        let quitButton = NSButton(
            title: String(localized: "Quit"), target: self, action: #selector(quitTapped))
        quitButton.bezelStyle = .rounded
        quitButton.translatesAutoresizingMaskIntoConstraints = false

        let stack = NSStackView(views: [textStack, spinner, quitButton])
        stack.orientation = .vertical
        stack.alignment = .centerX
        stack.spacing = 16
        stack.edgeInsets = NSEdgeInsets(top: 24, left: 24, bottom: 24, right: 24)
        stack.translatesAutoresizingMaskIntoConstraints = false

        let content = NSView(frame: NSRect(origin: .zero, size: size))
        content.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: content.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: content.trailingAnchor),
            stack.topAnchor.constraint(equalTo: content.topAnchor),
            stack.bottomAnchor.constraint(equalTo: content.bottomAnchor),
        ])
        return content
    }
}
