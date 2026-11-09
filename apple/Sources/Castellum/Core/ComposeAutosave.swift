// Castellum - pure debounce/coalescing policy for server-draft autosave.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Separated from AppModel so the coalescing decision (should THIS edit
// trigger a fresh debounce window, given when the last one started?) is
// unit-testable without a Task/Timer or the Rust core. AppModel owns the
// actual `Task.sleep`-based scheduling (mirrors `refreshContactsDebounced`);
// this type owns only the "is it time yet" arithmetic.

import Foundation

/// Decides when a burst of compose edits should coalesce into one
/// server-draft save. Not thread-safe by itself - AppModel (main actor)
/// owns the single instance per open compose window.
struct ComposeAutosaveScheduler {
    /// How long to wait after the LAST edit before saving.
    let debounceInterval: TimeInterval

    /// `true` once `debounceInterval` has elapsed since `lastEditAt`,
    /// relative to `now`. Pure - no side effects, no clock of its own.
    func isDue(lastEditAt: Date, now: Date) -> Bool {
        now.timeIntervalSince(lastEditAt) >= debounceInterval
    }
}
