// Castellum - forwards macOS sleep/wake and network-path-change signals to a
// single closure AppModel uses to nudge every account's IDLE thread. Kept
// thin and injectable (the notification center is a parameter, not always
// NSWorkspace.shared's) so the sleep/wake half is unit-testable without a
// real system sleep.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import Foundation
import Network

final class IdleWakeObserver {
    private var wakeObserver: NSObjectProtocol?
    private let center: NotificationCenter
    private var pathMonitor: NWPathMonitor?
    private let onWake: () -> Void

    /// `center` defaults to `NSWorkspace.shared.notificationCenter` in
    /// production; tests inject a plain `NotificationCenter()` and post
    /// `NSWorkspace.didWakeNotification` on it directly.
    init(center: NotificationCenter, onWake: @escaping () -> Void) {
        self.center = center
        self.onWake = onWake
    }

    /// Start observing. Network-path monitoring always uses the real system
    /// monitor (there is no practical way to inject one) — call this only
    /// from production code; tests exercise `handleWake()` directly instead.
    func start() {
        wakeObserver = center.addObserver(
            forName: NSWorkspace.didWakeNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.handleWake()
        }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] _ in
            self?.handleWake()
        }
        monitor.start(queue: DispatchQueue(label: "castellum.idle-wake.network"))
        pathMonitor = monitor
    }

    func stop() {
        if let wakeObserver {
            center.removeObserver(wakeObserver)
        }
        wakeObserver = nil
        pathMonitor?.cancel()
        pathMonitor = nil
    }

    /// Exposed for tests: the same call the wake notification/path update
    /// handlers make.
    func handleWake() {
        onWake()
    }

    deinit { stop() }
}
