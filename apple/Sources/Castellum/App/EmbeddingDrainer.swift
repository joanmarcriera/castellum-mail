// Castellum - background embeddings drainer (F1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Ingest enqueues one `embed_message` job per message body (core `sync.rs`), but until F1
// nothing in the app ever ran them: `AiCoordinator.kickRunner` only fires from a user gesture
// (summarise, draft, extract), so the vectors table stayed empty and Find Similar said
// "No similar messages found" however long it waited.
//
// This type is the missing driver. It runs the core's `embeddings`-scoped AI pass in small
// batches, off the main actor, and only while ALL of these hold:
//   * AI is enabled and an embedding model is chosen (`Preferences.aiEmbeddingModel`);
//   * the first message list has reached the UI (`firstListReachedUI()`), plus a short grace
//     period, so it never sits on the launch path;
//   * it has not been shut down (app quit) and has not stopped itself on a configuration error.
// Between batches it pauses, so sync and the UI are never starved of the DB lock (see
// project-docs/concurrency-first-sync-2026-09-29.md). When the endpoint is down it backs off
// exponentially (the core refunds the job's attempt so an outage never burns the backlog).
// Exactly one loop runs at a time. No mail text is ever read or logged here: the core reports
// counts and an error code only.

import Combine
import Foundation

/// What the AI Activity window and Settings > AI show about the embedding index.
struct EmbeddingProgress: Equatable {
    enum State: Equatable {
        /// AI off or no embedding model chosen, or shut down.
        case off
        /// Enabled, but the first message list has not reached the UI yet.
        case waitingForList
        /// A batch is being (or is about to be) processed.
        case running
        /// Backlog empty; checking again every `idlePoll`.
        case idle
        /// The endpoint did not answer; retrying after `seconds`.
        case backingOff(seconds: Int)
        /// A configuration error that retrying cannot fix (core error code); stopped until
        /// the endpoint or embedding model changes.
        case failed(code: String)
    }

    var state: State = .off
    /// Embedding jobs still to do.
    var queued = 0
    /// Vectors stored in total.
    var done = 0
    /// Embedding model the figures refer to (for the error text).
    var model = ""
}

@MainActor
final class EmbeddingDrainer: ObservableObject {
    struct Timing {
        /// Grace period after the first list is up, before the first batch.
        var startDelay: TimeInterval = 5
        /// Jobs per batch: small, so the executor lock is held for a second or two at most.
        var batchSize = 8
        /// Pause between batches while a backlog remains.
        var batchPause: TimeInterval = 2
        /// How often to look for new jobs once the backlog is empty.
        var idlePoll: TimeInterval = 30
        /// First retry delay after the endpoint failed; doubles up to `backoffMax`.
        var backoffBase: TimeInterval = 15
        var backoffMax: TimeInterval = 600
        static let production = Timing()
    }

    /// Runs one core pass for a config JSON (blocking; called off the main actor).
    typealias Runner = @Sendable (String) throws -> AiRunReport
    typealias Sleeper = @Sendable (TimeInterval) async -> Void

    @Published private(set) var progress = EmbeddingProgress()

    private let prefs: Preferences
    private let runner: Runner
    private let timing: Timing
    private let sleeper: Sleeper
    private var loop: Task<Void, Never>?
    private var firstListShown = false
    private var isShutDown = false
    private var cancellables = Set<AnyCancellable>()

    init(
        prefs: Preferences, runner: @escaping Runner, timing: Timing = .production,
        sleeper: @escaping Sleeper = { seconds in
            try? await Task.sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
        }
    ) {
        self.prefs = prefs
        self.runner = runner
        self.timing = timing
        self.sleeper = sleeper
        // Published values arrive on willSet; hop to the main queue so `reconcile` reads the
        // new ones. A change of enable flag, endpoint or embedding model restarts the loop
        // (which also clears a `failed` stop).
        Publishers.Merge3(
            prefs.$aiEnabled.dropFirst().map { _ in () },
            prefs.$aiEndpoint.dropFirst().map { _ in () },
            prefs.$aiEmbeddingModel.dropFirst().map { _ in () }
        )
        .receive(on: DispatchQueue.main)
        .sink { [weak self] in self?.restart() }
        .store(in: &cancellables)
    }

    /// The first message list is on screen: embedding may begin (after the grace period).
    func firstListReachedUI() {
        guard !firstListShown else { return }
        firstListShown = true
        reconcile()
    }

    /// App quit: stop for good.
    func shutdown() {
        isShutDown = true
        stop()
    }

    /// Start or stop the loop to match the preferences. Idempotent: never starts a second loop.
    func reconcile() {
        guard wanted else {
            stop()
            return
        }
        guard loop == nil else { return }
        progress.model = prefs.aiEmbeddingModel
        progress.state = .running
        loop = Task { [weak self] in await self?.run() }
    }

    private var wanted: Bool {
        !isShutDown && firstListShown && prefs.aiEnabled && !prefs.aiEmbeddingModel.isEmpty
    }

    private func restart() {
        stop()
        reconcile()
    }

    private func stop() {
        loop?.cancel()
        loop = nil
        let waiting = prefs.aiEnabled && !prefs.aiEmbeddingModel.isEmpty && !isShutDown && !firstListShown
        progress.state = waiting ? .waitingForList : .off
    }

    /// JSON for `castellum_run_ai_jobs` restricted to one embeddings batch.
    func configJSON() -> String? {
        struct Payload: Encodable {
            let endpoint: String
            let model: String
            let embeddingModel: String
            let scope = "embeddings"
            let maxJobs: Int
            let requestTimeoutSecs = 60
        }
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let embedding = prefs.aiEmbeddingModel
        guard !embedding.isEmpty,
            let data = try? encoder.encode(
                Payload(
                    endpoint: prefs.aiEndpoint,
                    model: prefs.aiModel.isEmpty ? embedding : prefs.aiModel,
                    embeddingModel: embedding, maxJobs: timing.batchSize))
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    private func run() async {
        await sleeper(timing.startDelay)
        var failures = 0
        while !Task.isCancelled {
            guard let config = configJSON() else { break }
            let runner = self.runner
            let report: AiRunReport?
            do {
                report = try await Task.detached(priority: .utility) { try runner(config) }.value
            } catch {
                report = nil
            }
            if Task.isCancelled { return }

            var delay = timing.batchPause
            if let report {
                progress.queued = report.queuedEmbeddings
                progress.done = report.embedded
                if let code = report.errorCode, !report.endpointDown {
                    // The model or endpoint config is wrong: retrying cannot help. Stop; a
                    // preference change restarts the loop.
                    progress.state = .failed(code: code)
                    loop = nil
                    return
                }
                if report.endpointDown {
                    failures += 1
                    delay = backoff(failures)
                    progress.state = .backingOff(seconds: Int(delay))
                } else {
                    failures = 0
                    if report.queuedEmbeddings > 0 {
                        progress.state = .running
                    } else {
                        progress.state = .idle
                        delay = timing.idlePoll
                    }
                }
            } else {
                // The FFI call itself threw (e.g. the endpoint was refused by the egress
                // guard): treat as an outage and back off rather than spin.
                failures += 1
                delay = backoff(failures)
                progress.state = .backingOff(seconds: Int(delay))
            }
            await sleeper(delay)
        }
    }

    /// 15 s, 30 s, 60 s ... capped at `backoffMax`.
    private func backoff(_ failures: Int) -> TimeInterval {
        min(timing.backoffMax, timing.backoffBase * pow(2, Double(max(0, failures - 1))))
    }

    /// Readable one-line status for an error state, or nil when there is nothing to complain
    /// about. Localised (en, es, ca); carries the model name, never mail content.
    static func errorText(for progress: EmbeddingProgress) -> String? {
        switch progress.state {
        case .failed(let code):
            switch code {
            case "embed_unsupported":
                return String(
                    format: String(localized: "%@ cannot produce embeddings; choose an embedding model"),
                    progress.model)
            case "model_missing":
                return String(
                    format: String(localized: "Embedding model %@ is not installed at the endpoint; choose another"),
                    progress.model)
            default:
                return String(
                    format: String(localized: "Embeddings stopped (%@); check the endpoint and embedding model"),
                    code)
            }
        case .backingOff:
            return String(localized: "Ollama is not answering; embeddings will retry automatically")
        default:
            return nil
        }
    }
}
