// Castellum - AI coordinator: enqueue, run, poll, cancel, cache.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Mirrors the replayNow() guard+drain pattern from AppModel for the AI runner.
// The poll loop runs every ~200 ms while any tracked job is non-terminal,
// merging events through AiEventReducer (pure, testable).

import Foundation
import Combine
import SwiftUI

@MainActor
final class AiCoordinator: ObservableObject {
    /// Live state for every job this session has touched (keyed by job id;
    /// cached results use a negative synthetic key derived from the owner id).
    @Published var jobs: [Int64: AiJobViewState] = [:]
    /// Typed thread-extraction rows (S54), keyed by thread id. Populated by
    /// `loadThreadExtractions` once an `extract_thread_items` job succeeds
    /// (or a complete cached output is used) — the panel binds to this, not
    /// to `jobs[id].text` (which never holds readable content for this
    /// feature; see `extractThreadItems`'s doc comment).
    @Published var threadExtractions: [String: [ThreadExtractionItem]] = [:]

    private weak var core: CastellumCore?
    private let prefs: Preferences

    /// In-flight runner Task (nil when idle).
    private var runnerTask: Task<Void, Never>?
    /// Set when a run is requested while one is already in progress.
    private var runnerNeeded = false
    /// In-flight poll loop Task.
    private var pollTask: Task<Void, Never>?
    /// Consecutive poll failures per job (circuit breaker: a job whose polls
    /// keep failing is marked failed locally so the loop can drain).
    private var pollFailures: [Int64: Int] = [:]
    /// Consecutive poll failures after which a job is locally failed.
    private static let maxPollFailures = 10
    /// Jobs the user (or a view teardown) asked to cancel.
    ///
    /// Cancelling a running job only sets a flag in the core — the row keeps
    /// its `leased`/`running`/`streaming` status until the executor notices.
    /// A poll already in flight would therefore write that status back over the
    /// optimistic "cancelled", the job would re-enter the active set, and the
    /// pane would show "Summarizing…" for something the user was told had been
    /// cancelled. This set makes the local cancel stick.
    private var cancelRequested: Set<Int64> = []

    /// Snake_case encoder for JSON payloads sent down to the core.
    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    /// Background embeddings (F1): drains the `embed_message` backlog ingest enqueues, so Find
    /// Similar has vectors to search. Progress is shown in the AI Activity window.
    let embeddings: EmbeddingDrainer
    private var embeddingsRelay: AnyCancellable?

    init(core: CastellumCore, prefs: Preferences,
         embeddings: EmbeddingDrainer? = nil) {
        self.core = core
        self.prefs = prefs
        self.embeddings = embeddings ?? EmbeddingDrainer(
            prefs: prefs,
            runner: { [weak core] json in
                guard let core else { throw CancellationError() }
                return try core.runAiJobs(configJSON: json)
            })
        // Re-publish drainer changes so views observing the coordinator refresh too.
        embeddingsRelay = self.embeddings.objectWillChange
            .sink { [weak self] _ in self?.objectWillChange.send() }
    }

    // MARK: - Public API

    /// Summarize a message. Unless `forceRefresh`, checks the cache first; if a
    /// complete output exists it publishes instantly with `isCached = true`.
    /// Otherwise enqueues a job and starts the runner + poll loop.
    /// `forceRefresh: true` (Regenerate) skips the cache and always enqueues.
    func summarize(messageId: String, forceRefresh: Bool = false) {
        guard prefs.aiEnabled, !prefs.aiModel.isEmpty else { return }
        guard let core else { return }
        let feature = "summarize_message"
        let scope = "message"
        // Build the payloads here on the main actor before entering the task.
        guard let configJSON = buildRunConfigJSON(),
              let requestJSON = encodeJSON(EnqueueRequestPayload(
                  feature: feature, messageId: messageId))
        else { return }

        Task.detached { [weak self] in
            guard let self else { return }
            // Check cache first (blocking DB read, safe off-actor).
            let cached = try? core.getAiOutput(scope: scope, ownerId: messageId,
                                               feature: feature)
            if AiEventReducer.shouldUseCachedOutput(cached, forceRefresh: forceRefresh),
               let cached {
                await MainActor.run {
                    // Stable synthetic key in the negative id space; the state's
                    // id matches the key so clear(jobId:) works on cached entries.
                    let key = Self.cachedKey(forOwnerId: messageId)
                    self.jobs[key] = AiJobViewState(
                        id: key,
                        feature: feature,
                        ownerId: messageId,
                        status: "succeeded",
                        text: cached.text,
                        errorCode: nil,
                        isCached: true,
                        lastEventId: 0
                    )
                }
                return
            }
            // Not cached (or Regenerate) — enqueue a new job.
            do {
                let enqueued = try core.enqueueAiJob(requestJSON: requestJSON)
                await MainActor.run {
                    self.jobs[enqueued.jobId] = AiJobViewState(
                        id: enqueued.jobId,
                        feature: feature,
                        ownerId: messageId,
                        status: "queued",
                        text: "",
                        errorCode: nil,
                        isCached: false,
                        lastEventId: 0
                    )
                    self.kickRunner(configJSON: configJSON)
                    self.startPollingIfNeeded()
                }
            } catch {
                await MainActor.run {
                    let key: Int64 = Int64.min + abs(Int64(messageId.hashValue) % 1_000_000)
                    self.jobs[key] = AiJobViewState(
                        id: key, feature: feature, ownerId: messageId,
                        status: "failed", text: "",
                        errorCode: error.localizedDescription,
                        isCached: false, lastEventId: 0
                    )
                }
            }
        }
    }

    /// Cancel every still-running job for one owner+feature and return the ids
    /// cancelled (for tests and callers that want to log it).
    ///
    /// This is the view-teardown path: closing a reply window or navigating off
    /// a message leaves nothing to show the result, but the Ollama endpoint
    /// keeps generating and the entry sits in `jobs` as a zombie until restart.
    /// Cached results are skipped — they live in the negative id space and were
    /// never a running job, so there is nothing to cancel and
    /// `core.cancelAiJob` would be handed an id that does not exist.
    @discardableResult
    func cancelActiveJobs(forOwnerId ownerId: String, feature: String) -> [Int64] {
        let live = jobs.values
            .filter { $0.ownerId == ownerId && $0.feature == feature }
            .filter { $0.id > 0 && !$0.isTerminal }
            .map(\.id)
        for id in live { cancel(jobId: id) }
        return live.sorted()
    }

    /// Request cooperative cancellation of a tracked job.
    ///
    /// A terminal job is left alone: the core is a no-op for one, and
    /// overwriting the local state would replace a finished summary with
    /// "cancelled" in the reading pane — with nothing to repair it, since the
    /// poll loop skips terminal jobs.
    func cancel(jobId: Int64) {
        guard let core, let state = jobs[jobId], !state.isTerminal else { return }
        cancelRequested.insert(jobId)
        jobs[jobId]?.status = "cancelled"   // optimistic
        Task.detached {
            try? core.cancelAiJob(id: jobId)
        }
    }

    /// Enqueue a `draft_reply` AI job for the given original message.
    ///
    /// Unlike `summarize`, draft replies are never cache-served — each call
    /// always enqueues a fresh job so the user gets a new generation.
    /// `instruction` is an optional free-text hint (e.g. "accept and propose
    /// Thursday") forwarded to the prompt builder in the Rust core.
    func draftReply(messageId: String, instruction: String? = nil) {
        guard prefs.aiEnabled, !prefs.aiModel.isEmpty else { return }
        guard let core else { return }
        let feature = "draft_reply"
        guard let configJSON = buildRunConfigJSON(),
              let requestJSON = encodeJSON(EnqueueRequestPayload(
                  feature: feature, messageId: messageId, instruction: instruction))
        else { return }

        Task.detached { [weak self] in
            guard let self else { return }
            do {
                let enqueued = try core.enqueueAiJob(requestJSON: requestJSON)
                await MainActor.run {
                    self.jobs[enqueued.jobId] = AiJobViewState(
                        id: enqueued.jobId,
                        feature: feature,
                        ownerId: messageId,
                        status: "queued",
                        text: "",
                        errorCode: nil,
                        isCached: false,
                        lastEventId: 0
                    )
                    self.kickRunner(configJSON: configJSON)
                    self.startPollingIfNeeded()
                }
            } catch {
                await MainActor.run {
                    let key: Int64 = Int64.min + abs(Int64(messageId.hashValue) % 1_000_000)
                    self.jobs[key] = AiJobViewState(
                        id: key, feature: feature, ownerId: messageId,
                        status: "failed", text: "",
                        errorCode: error.localizedDescription,
                        isCached: false, lastEventId: 0
                    )
                }
            }
        }
    }

    /// Enqueue a `draft_message` AI job for a blank compose window.
    ///
    /// Unlike `draftReply`, there is no original message id to key by — the
    /// caller supplies a stable per-window `sessionId`
    /// (`ComposeSession.composeSessionId`) instead. `instruction` is required
    /// (the "What should this email say?" sheet only enables Draft once it's
    /// non-empty); `subject`/`recipients`/`existingBody` give the prompt
    /// builder the live compose-window context. Each call always enqueues a
    /// fresh job — never cache-served, same as `draftReply`.
    func draftMessage(
        sessionId: String,
        instruction: String,
        subject: String,
        recipients: String,
        existingBody: String?
    ) {
        guard prefs.aiEnabled, !prefs.aiModel.isEmpty else { return }
        guard let core else { return }
        let feature = "draft_message"
        guard let configJSON = buildRunConfigJSON(),
              let requestJSON = encodeJSON(DraftMessageRequestPayload(
                  feature: feature, instruction: instruction, subject: subject,
                  recipients: recipients, existingBody: existingBody))
        else { return }

        Task.detached { [weak self] in
            guard let self else { return }
            do {
                let enqueued = try core.enqueueAiJob(requestJSON: requestJSON)
                await MainActor.run {
                    self.jobs[enqueued.jobId] = AiJobViewState(
                        id: enqueued.jobId,
                        feature: feature,
                        ownerId: sessionId,
                        status: "queued",
                        text: "",
                        errorCode: nil,
                        isCached: false,
                        lastEventId: 0
                    )
                    self.kickRunner(configJSON: configJSON)
                    self.startPollingIfNeeded()
                }
            } catch {
                await MainActor.run {
                    let key: Int64 = Int64.min + abs(Int64(sessionId.hashValue) % 1_000_000)
                    self.jobs[key] = AiJobViewState(
                        id: key, feature: feature, ownerId: sessionId,
                        status: "failed", text: "",
                        errorCode: error.localizedDescription,
                        isCached: false, lastEventId: 0
                    )
                }
            }
        }
    }

    /// Extract action items/dates/amounts for a thread (S54). Cache-first
    /// like `summarize`: a complete cached output means the typed rows are
    /// loaded straight away with no job to watch. Otherwise enqueues a job
    /// and starts the runner + poll loop, same as every other feature —
    /// `startPollingIfNeeded`'s loop calls `loadThreadExtractions` itself
    /// once the job's status turns "succeeded" (see below), so callers only
    /// need to watch `jobs`/`threadExtractions`, never poll manually.
    ///
    /// Unlike `summarize`'s `state.text`, this job's accumulated delta text
    /// is never shown: it is raw (and, mid-stream, incomplete) JSON, not
    /// prose — `ThreadExtractionsPanel` reads `threadExtractions`, never
    /// `jobs[id].text`, for this feature.
    func extractThreadItems(threadId: String, forceRefresh: Bool = false) {
        guard prefs.aiEnabled, !prefs.aiModel.isEmpty else { return }
        guard let core else { return }
        let feature = "extract_thread_items"
        let scope = "thread"
        guard let configJSON = buildRunConfigJSON(),
              let requestJSON = encodeJSON(ThreadEnqueueRequestPayload(
                  feature: feature, threadId: threadId))
        else { return }

        Task.detached { [weak self] in
            guard let self else { return }
            let cached = try? core.getAiOutput(scope: scope, ownerId: threadId, feature: feature)
            if AiEventReducer.shouldUseCachedOutput(cached, forceRefresh: forceRefresh),
               cached != nil {
                await MainActor.run {
                    let key = Self.cachedKey(forOwnerId: threadId)
                    self.jobs[key] = AiJobViewState(
                        id: key,
                        feature: feature,
                        ownerId: threadId,
                        status: "succeeded",
                        text: "",
                        errorCode: nil,
                        isCached: true,
                        lastEventId: 0
                    )
                    self.loadThreadExtractions(threadId: threadId)
                }
                return
            }
            do {
                let enqueued = try core.enqueueAiJob(requestJSON: requestJSON)
                await MainActor.run {
                    self.jobs[enqueued.jobId] = AiJobViewState(
                        id: enqueued.jobId,
                        feature: feature,
                        ownerId: threadId,
                        status: "queued",
                        text: "",
                        errorCode: nil,
                        isCached: false,
                        lastEventId: 0
                    )
                    self.kickRunner(configJSON: configJSON)
                    self.startPollingIfNeeded()
                }
            } catch {
                await MainActor.run {
                    let key: Int64 = Int64.min + abs(Int64(threadId.hashValue) % 1_000_000)
                    self.jobs[key] = AiJobViewState(
                        id: key, feature: feature, ownerId: threadId,
                        status: "failed", text: "",
                        errorCode: error.localizedDescription,
                        isCached: false, lastEventId: 0
                    )
                }
            }
        }
    }

    /// Fetch a thread's typed extraction rows and publish them. Safe to call
    /// any number of times (e.g. Retry) — always replaces the published set
    /// with the DB's current rows for that thread.
    func loadThreadExtractions(threadId: String) {
        guard let core else { return }
        Task.detached { [weak self] in
            guard let self else { return }
            let items = (try? core.getThreadExtractions(threadId: threadId)) ?? []
            await MainActor.run {
                self.threadExtractions[threadId] = items
            }
        }
    }

    /// Discard state for a job (e.g. to allow Regenerate to re-enqueue).
    func clear(jobId: Int64) {
        jobs.removeValue(forKey: jobId)
        pollFailures.removeValue(forKey: jobId)
    }

    /// Find the most recent job state for a given message id (highest job id —
    /// live jobs have positive ids and always beat cached negative-key entries).
    func jobState(forMessageId messageId: String, feature: String) -> AiJobViewState? {
        jobs.values
            .filter { $0.ownerId == messageId && $0.feature == feature }
            .max(by: { $0.id < $1.id })
    }

    /// Same lookup, named for thread-scoped features (`ownerId` is generic —
    /// this is `jobState(forMessageId:feature:)` in every way except the
    /// name a thread-keyed call site reads more clearly with).
    func jobState(forThreadId threadId: String, feature: String) -> AiJobViewState? {
        jobState(forMessageId: threadId, feature: feature)
    }

    // MARK: - Runner (mirrors replayNow drain pattern)

    /// Kick off the Ollama runner if not already running.
    func kickRunner(configJSON: String) {
        guard runnerTask == nil else { runnerNeeded = true; return }
        guard let core else { return }
        runnerTask = Task.detached { [weak self] in
            var runnerError: Error?
            do { _ = try core.runAiJobs(configJSON: configJSON) } catch { runnerError = error }
            let thrown = runnerError
            await MainActor.run {
                guard let self else { return }
                self.runnerTask = nil
                // The runner throws before touching any job when the endpoint is
                // refused (egress guard, bad URL, unresolvable host). Without this
                // the jobs stayed `queued` and the pane spun forever (#3381).
                if let thrown {
                    self.failUnstartedJobs(errorCode: "endpoint_rejected: \(thrown.localizedDescription)")
                    self.runnerNeeded = false
                    return
                }
                if self.runnerNeeded {
                    self.runnerNeeded = false
                    self.kickRunner(configJSON: configJSON)
                }
            }
        }
    }

    /// Fail every job that is still `queued` (the runner never started it), and cancel it
    /// in the core so it cannot fire later against some other endpoint. Returns the ids.
    @discardableResult
    func failUnstartedJobs(errorCode: String) -> [Int64] {
        let ids = jobs.values.filter { $0.id > 0 && $0.status == "queued" }.map(\.id).sorted()
        for id in ids {
            jobs[id]?.status = "failed"
            jobs[id]?.errorCode = errorCode
            cancelRequested.insert(id)
        }
        if let core, !ids.isEmpty {
            Task.detached { for id in ids { try? core.cancelAiJob(id: id) } }
        }
        return ids
    }

    // MARK: - Poll loop

    private func startPollingIfNeeded() {
        guard pollTask == nil else { return }
        pollTask = Task { [weak self] in
            // Bind self strongly once; holds the coordinator alive for the loop.
            guard let self else { return }
            while true {
                let active = self.jobs.values.filter { !$0.isTerminal }
                if active.isEmpty { break }
                try? await Task.sleep(nanoseconds: 200_000_000)   // 200 ms
                if Task.isCancelled { break }
                guard let core = self.core else { break }
                // Poll each non-terminal job off the main actor.
                for state in active {
                    // try? on a throwing call that returns Optional gives T??.
                    // ?? nil flattens to T? so guard let result gives T.
                    let pollOpt = try? await Task.detached(priority: .utility) {
                        try core.pollAiJob(id: state.id, afterEventId: state.lastEventId)
                    }.value
                    guard let result = pollOpt ?? nil else {
                        // Circuit breaker: FFI errors and unknown-job nulls both
                        // count; after maxPollFailures consecutive misses the job
                        // is failed locally so the loop can drain instead of
                        // spinning at 200 ms forever.
                        let failures = (self.pollFailures[state.id] ?? 0) + 1
                        self.pollFailures[state.id] = failures
                        if failures >= Self.maxPollFailures {
                            if var current = self.jobs[state.id] {
                                current.status = "failed"
                                current.errorCode = "poll_timeout"
                                self.jobs[state.id] = current
                            }
                            self.pollFailures.removeValue(forKey: state.id)
                        }
                        continue
                    }
                    self.pollFailures.removeValue(forKey: state.id)
                    guard var current = self.jobs[state.id] else { continue }
                    // A cancel we requested must not be undone by a poll that
                    // was already in flight: the core keeps the row at
                    // running/streaming until its executor notices the flag, so
                    // writing that status back would resurrect the job.
                    let cancelledLocally = self.cancelRequested.contains(state.id)
                    current.status = result.job.status
                    if AiJobViewState.isTerminalStatus(result.job.status) {
                        self.cancelRequested.remove(state.id)
                    }
                    if let code = result.job.errorCode { current.errorCode = code }
                    AiEventReducer.applyBatch(events: result.events, to: &current)
                    AiEventReducer.applyTiming(from: result.job, to: &current)
                    // `started` in this same stale poll can also set the reducer
                    // back to running, so resolve cancellation after events.
                    current.status = AiJobViewState.resolvedPollStatus(
                        polledStatus: result.job.status,
                        reducedStatus: current.status,
                        cancellationRequested: cancelledLocally
                    )
                    self.jobs[state.id] = current
                    // Thread extraction (S54) has no streamed-text UI: once
                    // the job succeeds, load the typed rows it just wrote so
                    // the panel fills in — this is the "no click needed"
                    // half of the design bar, the poll loop's own transition
                    // is what triggers the load, not a view appearing.
                    if current.feature == "extract_thread_items", current.status == "succeeded" {
                        self.loadThreadExtractions(threadId: current.ownerId)
                    }
                }
            }
            self.pollTask = nil
        }
    }

    // MARK: - Helpers

    /// Stable negative key for cache-published results (no live job id exists).
    private static func cachedKey(forOwnerId ownerId: String) -> Int64 {
        -abs(Int64(ownerId.hashValue) % (Int64.max - 1)) - 1
    }

    /// JSON body for `enqueue_ai_job` (snake_case; refs only, never content).
    /// `instruction` is optional — nil is serialised as `null`, which the Rust
    /// side accepts via `#[serde(default)] Option<String>`.
    private struct EnqueueRequestPayload: Encodable {
        let feature: String
        let messageId: String
        var instruction: String? = nil
    }

    /// JSON body for `enqueue_ai_job` for thread-scoped features
    /// (`extract_thread_items`; `summarize_thread` will want this shape too
    /// once it grows a caller).
    private struct ThreadEnqueueRequestPayload: Encodable {
        let feature: String
        let threadId: String
    }

    /// JSON body for `enqueue_ai_job` for `draft_message` — a blank compose
    /// window has no message id to key on, so the request instead carries the
    /// live compose-window state the prompt builder needs.
    private struct DraftMessageRequestPayload: Encodable {
        let feature: String
        let instruction: String
        let subject: String
        let recipients: String
        var existingBody: String? = nil
    }

    /// JSON config for `run_ai_jobs` (snake_case).
    private struct RunConfigPayload: Encodable {
        let endpoint: String
        let model: String
        let numCtx: UInt32
        let maxOutputTokens: UInt32
        /// Interactive pass: leaves the embeddings backlog to `EmbeddingDrainer`, so a
        /// summarise click can never end up embedding 20k messages under the executor lock.
        let scope = "chat"
    }

    private func buildRunConfigJSON() -> String? {
        encodeJSON(RunConfigPayload(endpoint: prefs.aiEndpoint,
                                    model: prefs.aiModel,
                                    numCtx: 8192,
                                    maxOutputTokens: 1024))
    }

    private func encodeJSON<T: Encodable>(_ value: T) -> String? {
        guard let data = try? Self.encoder.encode(value) else { return nil }
        return String(data: data, encoding: .utf8)
    }
}
