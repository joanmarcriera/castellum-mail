// Castellum - window-level key-sequence dispatch (, MM-12).
//
// MailMate-style bare mail keys (e/#/u/f/c/r/R/p/? , j/k, the digits and the
// `g …` chords) work anywhere in the MAIN window, not only while the message
// list is first responder. A local NSEvent key-down monitor — scoped to its own
// window — turns each key into a `KeyStroke`, and `KeySequenceRouter` matches
// the strokes typed so far against the `KeySequenceTable` built from the
// effective bindings (built-in `bareKey`/`gChord` shorthand or the user's
// `keys`), then posts the command's notification exactly like a menu chord.
// RootView's notification handlers route it through CommandDispatcher, so
// registry-marked destructive commands (archive, delete) stay behind the
// text-editing focus guard.
//
// Precedence (no double-fire): local monitors run BEFORE the event reaches
// any responder. A consumed key returns nil and is never delivered, so the
// message list's keyDown never sees it; the list has no local bare keys at all
// (j/k are ordinary commands now). Compose, Contacts, sheets and popovers are
// separate windows and are never intercepted.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

/// Matches key strokes against the live `KeySequenceTable`. Pure decision logic
/// with injectable seams; the NSEvent monitor below is its only production caller.
///
/// Rules, in order: ⌘ held or an unrepresentable key passes through and drops
/// any pending prefix; a focused text field/view passes through and drops it
/// (typing is never a command); Esc with something pending is consumed and
/// cancels; otherwise the stroke is appended and looked up: a command posts once
/// and resets, a prefix arms a timeout and is consumed, and a key that matches
/// nothing is passed through when nothing was pending but swallowed (and the
/// sequence reset) when a prefix was pending, so `g x` never leaks `x` (K8).
@MainActor
final class KeySequenceRouter {
    enum Outcome: Equatable {
        /// The key ran a command or advanced a sequence; swallow the event.
        case consumed
        /// Not ours; deliver the event normally (typing, navigation, menus).
        case passthrough
    }

    private let tableProvider: @MainActor () -> KeySequenceTable
    /// Read at every key press (not snapshotted), so Reload Key Bindings takes
    /// effect without a relaunch (audit #3454, MM-12 F5).
    private let specsProvider: @MainActor () -> [CommandSpec]
    private let isTextEditing: @MainActor () -> Bool
    private let post: @MainActor (Notification.Name) -> Void
    /// Whether a command can act right now (MM-8: a digit whose tag slot is
    /// empty, or with nothing selected, is passed through to the responder
    /// chain instead of being swallowed).
    private let isEnabled: @MainActor (CommandSpec) -> Bool
    private let timeout: TimeInterval

    /// Strokes typed so far toward a longer sequence.
    private var pending: [KeyStroke] = []
    /// The table the pending strokes were matched against; a reload (new
    /// table) drops them rather than completing them against new bindings.
    private var pendingTableId: UUID?
    private var expiry: DispatchWorkItem?
    /// An unfinished sequence clears itself after this long, so a bare key
    /// pressed much later can never be misread as the next stroke of it.
    static let sequenceTimeout: TimeInterval = 2

    init(
        table: @escaping @MainActor () -> KeySequenceTable,
        specs: @escaping @MainActor () -> [CommandSpec],
        isTextEditing: @escaping @MainActor () -> Bool = { CommandFocusPolicy.textEditingOwnsFocus },
        isEnabled: @escaping @MainActor (CommandSpec) -> Bool = { _ in true },
        post: @escaping @MainActor (Notification.Name) -> Void = {
            NotificationCenter.default.post(name: $0, object: nil)
        },
        timeout: TimeInterval = KeySequenceRouter.sequenceTimeout
    ) {
        self.tableProvider = table
        self.specsProvider = specs
        self.isTextEditing = isTextEditing
        self.isEnabled = isEnabled
        self.post = post
        self.timeout = timeout
    }

    /// Convenience for a fixed spec table (tests, default bindings).
    convenience init(
        specs: [CommandSpec] = CommandRegistry.all,
        isTextEditing: @escaping @MainActor () -> Bool = { CommandFocusPolicy.textEditingOwnsFocus },
        isEnabled: @escaping @MainActor (CommandSpec) -> Bool = { _ in true },
        post: @escaping @MainActor (Notification.Name) -> Void = {
            NotificationCenter.default.post(name: $0, object: nil)
        },
        timeout: TimeInterval = KeySequenceRouter.sequenceTimeout
    ) {
        let table = KeySequenceTable(specs: specs)
        self.init(table: { table }, specs: { specs }, isTextEditing: isTextEditing,
                  isEnabled: isEnabled, post: post, timeout: timeout)
    }

    /// Strokes waiting for the rest of a sequence, as shown to the user ("G");
    /// nil when nothing is pending.
    var pendingDisplay: String? {
        KeySequence(strokes: pending)?.display
    }

    /// Decide what a key-down does. `stroke` is nil for a key that cannot be a
    /// stroke (dead key, function key); `hasCommand` is true when ⌘ is held
    /// (menu key equivalents own those). `isRepeat` is the event's
    /// auto-repeat flag: holding a prefix key must neither re-arm nor fire.
    func handle(stroke: KeyStroke?, hasCommand: Bool, isRepeat: Bool = false) -> Outcome {
        guard !hasCommand, let stroke else {
            cancelPending()
            return .passthrough
        }
        if isTextEditing() {
            cancelPending() // a half-typed sequence never fires after editing
            return .passthrough
        }
        let table = tableProvider()
        if let pendingTableId, pendingTableId != table.id { cancelPending() }
        let wasPending = !pending.isEmpty
        if wasPending, stroke.isEscape {
            cancelPending()
            return .consumed
        }
        // A held prefix key repeats: the press already armed it.
        if isRepeat, wasPending { return .consumed }
        switch table.match(pending + [stroke]) {
        case .command(let id):
            cancelPending()
            guard let spec = specsProvider().first(where: { $0.id == id }),
                  let notification = spec.notification, isEnabled(spec)
            else { return wasPending ? .consumed : .passthrough }
            post(notification)
            return .consumed
        case .prefix:
            if isRepeat { return .consumed }
            pending.append(stroke)
            pendingTableId = table.id
            armExpiry()
            return .consumed
        case .none:
            cancelPending()
            return wasPending ? .consumed : .passthrough
        }
    }

    /// Abandon a half-typed sequence: the next key is interpreted fresh.
    /// Called when the window resigns key or a sheet attaches, when text
    /// editing intervenes, on reload, and by the expiry timer.
    func cancelPending() {
        pending = []
        pendingTableId = nil
        expiry?.cancel()
        expiry = nil
    }

    /// (Re)schedule the pending sequence's expiry.
    private func armExpiry() {
        expiry?.cancel()
        let work = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated { self?.cancelPending() }
        }
        expiry = work
        DispatchQueue.main.asyncAfter(deadline: .now() + timeout, execute: work)
    }
}

/// Installs the key-sequence router for the window that hosts it (RootView
/// puts one in the main window's background). Handles ONLY events targeted at
/// its own window, so composer/contacts windows, sheets and popovers keep their
/// normal key handling.
struct BareKeyMonitor: NSViewRepresentable {
    let model: AppModel

    func makeNSView(context: Context) -> BareKeyMonitorView {
        let view = BareKeyMonitorView()
        view.router = BareKeyMonitorView.makeRouter(model: model)
        return view
    }

    func updateNSView(_ nsView: BareKeyMonitorView, context: Context) {}
}

/// Invisible NSView whose lifecycle owns a local key-down NSEvent monitor
/// plus observers that abandon a half-typed sequence when the window loses key
/// status or a sheet attaches (press `g`, click to another window, come back —
/// the next key must be read fresh, not as the second stroke).
@MainActor
final class BareKeyMonitorView: NSView {
    var router: KeySequenceRouter?
    private var monitor: Any?
    private var resignKeyObserver: NSObjectProtocol?

    /// The production router: reads the live table and specs from the model's
    /// `KeyBindingsStore` on every key, so a reload needs no relaunch.
    static func makeRouter(model: AppModel) -> KeySequenceRouter {
        KeySequenceRouter(
            table: { [weak model] in model?.keyBindings.table ?? KeySequenceTable(specs: CommandRegistry.all) },
            specs: { [weak model] in model?.keyBindings.effectiveSpecs ?? CommandRegistry.all },
            isEnabled: { [weak model] spec in model?.isBareKeyCommandAvailable(spec.id) ?? true })
    }

    /// The monitor closure's body: what one key-down does. Returns nil when the
    /// key was consumed, else the event to deliver. `internal` so the UIT-1
    /// harness can drive it with synthetic events.
    func deliver(_ event: NSEvent) -> NSEvent? {
        guard let router else { return event }
        let outcome = router.handle(
            stroke: KeyStroke(event: event),
            hasCommand: event.modifierFlags.contains(.command),
            isRepeat: event.isARepeat)
        return outcome == .consumed ? nil : event
    }

    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        removeMonitor()
        guard newWindow != nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self, let window = self.window else { return event }
            guard event.window === window else {
                // A sheet on our window takes the keys: drop any half-typed prefix.
                if let sheet = window.attachedSheet, event.window === sheet { self.router?.cancelPending() }
                return event
            }
            return self.deliver(event)
        }
        resignKeyObserver = NotificationCenter.default.addObserver(
            forName: NSWindow.didResignKeyNotification, object: newWindow, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.router?.cancelPending() }
        }
    }

    private func removeMonitor() {
        if let monitor {
            NSEvent.removeMonitor(monitor)
            self.monitor = nil
        }
        if let resignKeyObserver {
            NotificationCenter.default.removeObserver(resignKeyObserver)
            self.resignKeyObserver = nil
        }
    }
}
