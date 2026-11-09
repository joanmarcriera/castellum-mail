// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import Foundation

/// Per-key cancel-and-replace task slots where the replacement waits for its
/// cancelled predecessor to finish before starting. The hydration drain makes
/// blocking FFI/IMAP calls that are not cancellation-aware, so plain
/// cancel-and-restart could run two provider connections for one account at
/// once and fetch the same UIDs twice; awaiting the predecessor keeps at most
/// one in flight per key while the old loop exits at its next cancellation
/// check. Worst-case handoff delay: the cancelled drain may already be past
/// its cancellation check, so the replacement can wait out one full blocking
/// pass (plus any unelapsed between-pass sleep) before starting.
@MainActor
final class SerializedTaskSlots {
    private var tasks: [String: Task<Void, Never>] = [:]

    func replace(
        key: String,
        priority: TaskPriority = .background,
        operation: @escaping @Sendable () async -> Void
    ) {
        let previous = tasks[key]
        previous?.cancel()
        tasks[key] = Task.detached(priority: priority) {
            await previous?.value
            await operation()
        }
    }
}

/// Drive loop for one account's background body-hydration drain, split from
/// `AppModel` so its stop conditions are unit-testable without a core handle
/// or an IMAP connection.
enum BackgroundHydrationLoop {
    /// Upper bound on hydration passes per sync: 40 × 100 bodies covers a
    /// 4000-message backlog burst; anything larger resumes on the next sync.
    static let maxPassesPerSync = 40

    /// Upper bound on bytes fetched per sync across passes. The pass cap
    /// bounds message count, not transfer size — 4000 multi-megabyte bodies
    /// would otherwise be an unbounded background download on a slow or
    /// metered link. The remaining budget is threaded into every pass, where
    /// the Rust core selects candidates against it PRE-fetch from stored
    /// RFC822.SIZE metadata, so the ceiling holds up to server size honesty;
    /// bodies larger than the budget wait for explicit body-on-open
    /// hydration. Beyond this cap the backlog resumes on the next sync,
    /// exactly like the pass cap.
    static let maxBytesPerSync: Int64 = 64 << 20

    /// Why a drain stopped. Tests assert on this; callers behave identically
    /// for every case (the next sync reschedules the drain).
    enum Stop: Equatable {
        case cancelled, failedPass, drained, zeroProgress, byteBudget, passBudget
    }

    /// Run bounded passes until a stop condition. `runPass` is the blocking
    /// hydration FFI call, given the byte budget remaining for this sync
    /// (`nil` = provider/config failure; the next sync is the retry).
    /// `onProgress` fires after any pass that hydrated bodies. Stops on:
    /// cancellation, a failed pass, a drained backlog, a zero-progress pass
    /// (all-error), the per-sync byte budget, or the per-sync pass budget.
    @discardableResult
    static func drain(
        runPass: (Int64) async -> HydrationReport?,
        onProgress: () async -> Void,
        betweenPasses: () async -> Void
    ) async -> Stop {
        var bytesFetched: Int64 = 0
        for pass in 1...maxPassesPerSync {
            if Task.isCancelled { return .cancelled }
            guard let report = await runPass(maxBytesPerSync - bytesFetched) else {
                return .failedPass
            }
            if report.hydrated > 0 { await onProgress() }
            if report.remaining <= 0 { return .drained }
            if report.hydrated == 0 { return .zeroProgress }
            bytesFetched += report.bytesFetched
            if bytesFetched >= maxBytesPerSync { return .byteBudget }
            if pass < maxPassesPerSync { await betweenPasses() }
        }
        return .passBudget
    }
}

/// Builds the per-pass hydration config: the stored account config JSON with
/// the remaining per-sync byte budget inserted, so the Rust pass can select
/// candidates pre-fetch (`hydration_byte_budget` in `ImapSyncConfig`).
enum HydrationConfig {
    static func withByteBudget(_ configJSON: String, remainingBytes: Int64) -> String? {
        guard let data = configJSON.data(using: .utf8),
              var object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return nil }
        object["hydration_byte_budget"] = remainingBytes
        guard let out = try? JSONSerialization.data(withJSONObject: object) else { return nil }
        return String(data: out, encoding: .utf8)
    }
}
