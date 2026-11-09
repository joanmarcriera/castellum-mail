// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 deterministic quiescence waiter (Debug builds only).
//
// Usage: `await UIIdle.wait(model:timeoutMs:ignoreSync:)`. "Idle" means the same
// fingerprint in 3 consecutive samples 100 ms apart, where the fingerprint
// covers every visible window (number, frame, attached sheet, class+frame+hidden
// of every subview) plus the model's syncing/replaying flags. Each sample first
// forces layout and display, and each `Task.sleep` yields the main run loop so
// pending `DispatchQueue.main.async` work drains. Fixed sleeps are never used.
// On timeout the failure names the windows whose fingerprint was still changing.
#if DEBUG
import AppKit

@MainActor
enum UIIdle {
    static func fingerprints(model: AppModel, ignoreSync: Bool) -> [String: Int] {
        var out: [String: Int] = [:]
        for w in UIWindows.candidates() {
            w.layoutIfNeeded()
            w.displayIfNeeded()
            var h = Hasher()
            h.combine(w.windowNumber)
            h.combine(NSStringFromRect(w.frame))
            h.combine(w.attachedSheet?.windowNumber ?? 0)
            if let root = w.contentView { hash(root, into: &h) }
            out["\(w.windowNumber):\(w.title)"] = h.finalize()
        }
        var h = Hasher()
        if !ignoreSync { h.combine(model.isSyncing); h.combine(model.isReplaying) }
        out["model"] = h.finalize()
        return out
    }

    private static func hash(_ v: NSView, into h: inout Hasher) {
        h.combine(String(describing: type(of: v)))
        h.combine(NSStringFromRect(v.frame))
        h.combine(v.isHidden)
        for s in v.subviews { hash(s, into: &h) }
    }

    /// Returns nil when idle, or a description of what kept changing.
    static func wait(model: AppModel, timeoutMs: Int = 10_000, ignoreSync: Bool = false) async -> String? {
        let deadline = Date().addingTimeInterval(Double(timeoutMs) / 1000)
        var last = fingerprints(model: model, ignoreSync: ignoreSync)
        var stable = 0
        var changing: Set<String> = []
        while Date() < deadline {
            try? await Task.sleep(nanoseconds: 100_000_000)
            let now = fingerprints(model: model, ignoreSync: ignoreSync)
            if now == last {
                stable += 1
                if stable >= 3 { return nil }
            } else {
                stable = 0
                changing = Set(now.filter { last[$0.key] != $0.value }.keys).union(last.keys.filter { now[$0] == nil })
            }
            last = now
        }
        return "UI never settled within \(timeoutMs) ms; still changing: \(changing.sorted().joined(separator: ", "))"
    }
}
#endif
