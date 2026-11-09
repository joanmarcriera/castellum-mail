// Castellum - pure AI event reducer (streaming delta accumulator).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Designed as a testable pure struct: no ObservableObject, no FFI calls.
// AiCoordinator uses it to fold streamed events into AiJobViewState values.

import Foundation

/// Terminal job statuses — the executor will not produce further events.
private let terminalStatuses: Set<String> = ["succeeded", "failed", "cancelled"]

/// View-facing state for a single AI job. Owned by AiCoordinator.
struct AiJobViewState {
    let id: Int64
    let feature: String
    let ownerId: String
    var status: String
    var text: String
    var errorCode: String?
    /// `true` when the result came from the cache without running the job.
    var isCached: Bool
    /// Highest event id received for this job (used as `after_event_id` in the
    /// next poll call to request only new events).
    var lastEventId: Int64
    /// When this coordinator first learned of the job — the AI Activity
    /// window's "elapsed" column. Local wall-clock time, not
    /// from the core, so a cached (never-run) entry still gets a value.
    var createdAt: Date = Date()
    /// Wall milliseconds the model call took (generate or embed); nil until the
    /// job completes, and for cores that predate latency telemetry (M5).
    var durationMs: Int64?
    /// Output tokens per second; nil when the backend reported no token count.
    var tokensPerSecond: Double?

    var isTerminal: Bool { Self.isTerminalStatus(status) }

    /// Terminality of a status the coordinator has NOT yet stored — needed to
    /// decide whether a freshly polled status may overwrite local state.
    static func isTerminalStatus(_ status: String) -> Bool {
        terminalStatuses.contains(status)
    }

    /// Resolve the status after folding one poll response into local state.
    /// A cancellation request is optimistic because the core reports the old
    /// running state until the executor observes its cancellation flag. Keep
    /// the local state cancelled until a later poll confirms any terminal
    /// outcome, including a completion that won the race with cancellation.
    static func resolvedPollStatus(
        polledStatus: String,
        reducedStatus: String,
        cancellationRequested: Bool
    ) -> String {
        guard cancellationRequested, !isTerminalStatus(polledStatus) else {
            return reducedStatus
        }
        return "cancelled"
    }
}

/// Pure reducer: fold a batch of `AiEvent` values into an `AiJobViewState`.
///
/// Rules (mirroring the executor):
///   - `started`  → reset accumulated text (retry semantics) + update status
///   - `delta`    → append payload text to accumulated text
///   - `succeeded`, `failed`, `cancelled`, `error` → update status + error code
///
/// Call this for each event in poll-order; track `lastEventId` across calls.
struct AiEventReducer {
    /// Apply one event to `state` in place.
    static func apply(event: AiEvent, to state: inout AiJobViewState) {
        // Always advance the high-water mark.
        if event.id > state.lastEventId {
            state.lastEventId = event.id
        }

        switch event.kind {
        case "started":
            state.text = ""
            state.status = "running"

        case "delta":
            // The executor stores deltas as {"text":"..."} so arbitrary
            // whitespace and newlines survive the event boundary. Accept the
            // old raw-text shape as a compatibility fallback.
            state.text += decodedDeltaText(event.payload)

        case "succeeded":
            state.status = "succeeded"

        case "failed", "error":
            state.status = "failed"
            // error_code may be embedded as JSON {"error_code":"..."} or just a plain string.
            if let data = event.payload.data(using: .utf8),
               let obj = try? JSONDecoder().decode([String: String].self, from: data),
               let code = obj["error_code"] {
                state.errorCode = code
            } else if !event.payload.isEmpty {
                state.errorCode = event.payload
            }

        case "cancelled":
            state.status = "cancelled"

        case "completed":
            // Latency telemetry rides on the completed event as counts and
            // timings only. An older core sends "{}", which leaves both nil.
            if let data = event.payload.data(using: .utf8),
               let timing = try? JSONDecoder().decode(TimingPayload.self, from: data) {
                state.durationMs = timing.durationMs ?? state.durationMs
                state.tokensPerSecond = timing.tokensPerSecond ?? state.tokensPerSecond
            }

        default:
            break
        }
    }

    /// Apply a batch of events to `state`; events should be in ascending id order.
    static func applyBatch(events: [AiEvent], to state: inout AiJobViewState) {
        for event in events { apply(event: event, to: &state) }
    }

    private struct TimingPayload: Decodable {
        let durationMs: Int64?
        let tokensPerSecond: Double?

        enum CodingKeys: String, CodingKey {
            case durationMs = "duration_ms"
            case tokensPerSecond = "tokens_per_second"
        }
    }

    /// Fold the poll's job snapshot telemetry into `state` (never clears a
    /// value the snapshot does not carry).
    static func applyTiming(from job: AiJobInfo, to state: inout AiJobViewState) {
        if let ms = job.durationMs { state.durationMs = ms }
        if let tps = job.tokensPerSecond { state.tokensPerSecond = tps }
    }

    /// Short display label for the AI Activity row, e.g. "1.2 s" or
    /// "1.2 s · 18.4 tokens/s"; nil when no timing is known. The number
    /// formatting is locale-independent on purpose (one decimal, point).
    static func timingLabel(durationMs: Int64?, tokensPerSecond: Double?) -> String? {
        guard let durationMs else { return nil }
        let seconds = String(format: "%.1f s", Double(durationMs) / 1000.0)
        guard let tokensPerSecond else { return seconds }
        let rate = String(format: "%.1f", tokensPerSecond)
        return seconds + " · " + String(localized: "\(rate) tokens/s")
    }

    private struct DeltaPayload: Decodable {
        let text: String
    }

    private static func decodedDeltaText(_ payload: String) -> String {
        guard let data = payload.data(using: .utf8),
              let decoded = try? JSONDecoder().decode(DeltaPayload.self, from: data)
        else { return payload }
        return decoded.text
    }

    /// Pure decision: use a cached output instead of enqueueing a new job?
    /// Only a COMPLETE cached output qualifies, and `forceRefresh` (Regenerate)
    /// always bypasses the cache.
    static func shouldUseCachedOutput(_ cached: AiOutput?, forceRefresh: Bool) -> Bool {
        guard !forceRefresh, let cached, cached.complete else { return false }
        return true
    }

    /// Map an AI error code to a user-facing description.
    /// `modelName` is substituted into the `model_missing` message.
    /// Used by both AiSummarySection and AiDraftSection to keep copy consistent.
    static func errorMessage(code: String?, modelName: String) -> String {
        switch code {
        case "model_missing":
            return "Model '\(modelName)' not installed on the endpoint"
        case "endpoint_refused":
            return "Endpoint refused — only local network addresses are allowed"
        case "network":
            return "Network error — is Ollama running?"
        case "timeout":
            return "Request timed out"
        default:
            return code ?? "AI request failed"
        }
    }
}
