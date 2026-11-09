// Castellum - the menu bar unread counter (MM-9, slice 1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// An optional NSStatusItem (an envelope plus the unread number) with a small
// menu. OFF by default (T11): while `Preferences.menuBarCounterMode == .off` no
// status item exists at all. Turning it off removes the item from NSStatusBar
// and drops every reference to it, so nothing lingers. The number comes from
// the same `CounterFeed` the Dock badge uses (the sidebar's counts), with its
// own scope, and it ignores Focus "Show Dock badge" (that is a Dock toggle).
//
// Under XCTest the port is a recording no-op: no status item is ever created.
// If the user removes the item by Cmd-dragging it out of the menu bar, the
// preference is set to off (the item is gone and the setting must say so).

import AppKit
import Combine
import Foundation

/// What the controller needs from a status item; the real one wraps NSStatusItem.
@MainActor
protocol StatusItemPort: AnyObject {
    /// The counter text next to the icon; "" = icon only.
    func setTitle(_ title: String)
    /// Create (true) or remove (false) the item. Idempotent.
    func setVisible(_ visible: Bool)
    func setMenu(_ menu: NSMenu)
    /// Called when the user removes the item from the menu bar themselves.
    var onUserRemoved: (() -> Void)? { get set }
}

/// Test and UI-script double: records, creates nothing.
@MainActor
final class RecordingStatusItemPort: StatusItemPort {
    private(set) var titles: [String] = []
    private(set) var visible = false
    private(set) var menu: NSMenu?
    var onUserRemoved: (() -> Void)?
    func setTitle(_ title: String) { titles.append(title) }
    func setVisible(_ visible: Bool) { self.visible = visible }
    func setMenu(_ menu: NSMenu) { self.menu = menu }
}

/// The real NSStatusItem. Created lazily on `setVisible(true)`, removed on false.
@MainActor
final class SystemStatusItemPort: StatusItemPort {
    static let autosaveName = "CastellumUnread"
    var onUserRemoved: (() -> Void)?
    private var item: NSStatusItem?
    private var visibilityObservation: NSKeyValueObservation?
    private var title = ""
    private var menu: NSMenu?

    func setTitle(_ title: String) {
        self.title = title
        applyTitle()
    }

    func setMenu(_ menu: NSMenu) {
        self.menu = menu
        item?.menu = menu
    }

    func setVisible(_ visible: Bool) {
        if visible {
            guard item == nil else { return }
            let created = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
            created.autosaveName = Self.autosaveName
            // A Cmd-drag removal is remembered under the autosave name; the user
            // switching the counter on again must bring the item back.
            created.isVisible = true
            created.button?.image = NSImage(
                systemSymbolName: "envelope", accessibilityDescription: String(localized: "Unread mail"))
            created.menu = menu
            item = created
            applyTitle()
            visibilityObservation = created.observe(\.isVisible, options: [.new]) { [weak self] _, change in
                guard change.newValue == false else { return }
                DispatchQueue.main.async { MainActor.assumeIsolated { self?.onUserRemoved?() } }
            }
        } else {
            visibilityObservation = nil
            if let item { NSStatusBar.system.removeStatusItem(item) }
            item = nil
        }
    }

    private func applyTitle() {
        guard let button = item?.button else { return }
        button.title = title.isEmpty ? "" : " " + title
        button.imagePosition = title.isEmpty ? .imageOnly : .imageLeading
    }

    deinit {
        // The controller removes the item first; this covers an abandoned port.
        MainActor.assumeIsolated { if let item { NSStatusBar.system.removeStatusItem(item) } }
    }
}

/// Builds the item's menu lazily each time it opens, so it never goes stale.
@MainActor
final class MenuBarMenuProvider: NSObject, NSMenuDelegate {
    private weak var model: AppModel?
    init(model: AppModel) { self.model = model }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        guard let model else { return }
        menu.addItem(item(String(localized: "Open Castellum"), #selector(openMain)))
        menu.addItem(item(String(localized: "New Message"), #selector(compose)))
        let unread = UnifiedInboxBadge.unreadByAccount(role: "inbox", mailboxes: model.mailboxes, counts: model.counts)
        if !model.accounts.isEmpty { menu.addItem(.separator()) }
        for account in model.accounts {
            let n = unread[account.id] ?? 0
            let line = item("\(account.sidebarLabel)  \(n)", #selector(openInbox(_:)))
            line.representedObject = account.id
            menu.addItem(line)
        }
        menu.addItem(.separator())
        menu.addItem(item(String(localized: "Settings…"), #selector(openSettings)))
    }

    private func item(_ title: String, _ action: Selector) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: action, keyEquivalent: "")
        i.target = self
        return i
    }

    @objc private func openMain() {
        NotificationCenter.default.post(name: .castellumShowMainWindow, object: nil)
    }
    @objc private func compose() {
        NotificationCenter.default.post(name: .castellumCompose, object: nil)
    }
    @objc private func openInbox(_ sender: NSMenuItem) {
        openMain()
        guard let model, let id = sender.representedObject as? String,
              let inbox = model.mailboxes[id]?.first(where: { $0.role == "inbox" }) else { return }
        model.select(mailbox: inbox)
    }
    @objc private func openSettings() { model?.openSettingsTab(.notifications) }
}

@MainActor
final class MenuBarCounterController {
    private let preferences: Preferences
    private let port: StatusItemPort
    private let menuProvider: MenuBarMenuProvider
    private var feed: CounterFeed?
    private var scopeObserver: AnyCancellable?
    /// What the item shows now: "off" (no item), "" (icon only) or the number. UIT-1 reads this.
    private(set) var currentTitle = "off"

    init(
        model: AppModel,
        preferences: Preferences? = nil,
        focusState: @escaping () -> FocusFilterState = { FocusFilterStore.load() },
        debounceMilliseconds: Int = 300,
        port: StatusItemPort? = nil
    ) {
        let prefs = preferences ?? Preferences.shared
        self.preferences = prefs
        // Never create a real status item under XCTest.
        self.port = port ?? (TestHost.isActive ? RecordingStatusItemPort() : SystemStatusItemPort())
        menuProvider = MenuBarMenuProvider(model: model)
        let menu = NSMenu()
        menu.delegate = menuProvider
        self.port.setMenu(menu)
        self.port.onUserRemoved = { [weak self] in
            // The user took the item out of the menu bar: the setting follows.
            guard let self, self.preferences.menuBarCounterMode != .off else { return }
            self.preferences.menuBarCounterMode = .off
            self.recompute()
        }
        feed = CounterFeed(
            model: model, preferences: prefs, focusState: focusState,
            debounceMilliseconds: debounceMilliseconds, honoursDockFlag: false,
            scope: { prefs.menuBarScope },
            scopePrefs: [
                prefs.$menuBarCounterMode.map { _ in () }.eraseToAnyPublisher(),
                prefs.$menuBarSmartMailboxId.map { _ in () }.eraseToAnyPublisher(),
            ],
            deliver: { [weak self] label in self?.apply(label) })
    }

    func recompute() { feed?.recompute() }

    /// Remove the item and stop observing. Idempotent; also what deinit does.
    func stop() {
        feed = nil
        port.onUserRemoved = nil
        port.setVisible(false)
        currentTitle = "off"
    }

    deinit { MainActor.assumeIsolated { port.setVisible(false) } }

    private func apply(_ label: String?) {
        if preferences.menuBarScope == .off {
            guard currentTitle != "off" else { return }
            port.setVisible(false)
            currentTitle = "off"
            return
        }
        let title = label ?? ""
        port.setVisible(true)
        guard title != currentTitle else { return }
        currentTitle = title
        port.setTitle(title)
    }
}
