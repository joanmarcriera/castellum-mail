// Castellum - background embeddings drainer (F1).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Find Similar stayed empty because nothing ran the `embed_message` queue. These tests pin the
// driver's contract with a fake runner (no core, no network, no real defaults): it starts only
// when AI is on, an embedding model is set and the first list is up; runs one batch at a time;
// stops when AI is turned off or at quit; backs off while the endpoint is down; and stops on a
// configuration error (a chat model chosen as the embedding model) until the preference changes.

import XCTest
@testable import Castellum

/// Thread-safe recorder shared between the main-actor test and the detached runner.
private final class Recorder: @unchecked Sendable {
    private let lock = NSLock()
    private var _configs: [String] = []
    private var _delays: [TimeInterval] = []
    private var _inFlight = 0
    private var _maxInFlight = 0
    private var _reports: [AiRunReport] = []

    var calls: Int { lock.withLock { _configs.count } }
    var configs: [String] { lock.withLock { _configs } }
    var delays: [TimeInterval] { lock.withLock { _delays } }
    var maxInFlight: Int { lock.withLock { _maxInFlight } }

    /// Reports are served in order; the last one repeats.
    func script(_ reports: [AiRunReport]) { lock.withLock { _reports = reports } }

    func run(_ config: String, hold: TimeInterval) -> AiRunReport {
        let report: AiRunReport = lock.withLock {
            _configs.append(config)
            _inFlight += 1
            _maxInFlight = max(_maxInFlight, _inFlight)
            return _reports.count > 1 ? _reports.removeFirst() : _reports[0]
        }
        if hold > 0 { Thread.sleep(forTimeInterval: hold) }
        lock.withLock { _inFlight -= 1 }
        return report
    }

    func recordDelay(_ d: TimeInterval) { lock.withLock { _delays.append(d) } }
}

private func report(queued: Int = 0, embedded: Int = 0, down: Bool = false, code: String? = nil)
    -> AiRunReport
{
    AiRunReport(ran: 1, succeeded: 1, failed: 0, cancelled: 0, endpointDown: down,
                errorCode: code, queuedEmbeddings: queued, embedded: embedded)
}

@MainActor
final class EmbeddingDrainerTests: XCTestCase {
    private var recorder: Recorder!
    private var prefs: Preferences!

    override func setUp() {
        recorder = Recorder()
        let suite = "castellum-embed-drainer-\(UUID().uuidString)"
        prefs = Preferences(defaults: UserDefaults(suiteName: suite)!)
        prefs.aiEnabled = true
        prefs.aiEndpoint = "http://127.0.0.1:11434"
        prefs.aiModel = "chat-model"
        prefs.aiEmbeddingModel = "embed-model"
    }

    private func makeDrainer(hold: TimeInterval = 0) -> EmbeddingDrainer {
        let rec = recorder!
        return EmbeddingDrainer(
            prefs: prefs,
            runner: { rec.run($0, hold: hold) },
            timing: .init(startDelay: 0.01, batchSize: 8, batchPause: 0.01, idlePoll: 0.05,
                          backoffBase: 15, backoffMax: 60),
            // Never actually waits the backoff: records it, yields briefly so the loop cannot spin hot.
            sleeper: { seconds in
                rec.recordDelay(seconds)
                try? await Task.sleep(nanoseconds: 2_000_000)
            })
    }

    private func waitUntil(_ what: String, timeout: TimeInterval = 5, _ cond: () -> Bool) async {
        let deadline = Date().addingTimeInterval(timeout)
        while !cond() {
            if Date() > deadline { return XCTFail("timed out waiting for \(what)") }
            try? await Task.sleep(nanoseconds: 5_000_000)
        }
    }

    func testDoesNotStartBeforeTheFirstListIsOnScreen() async {
        recorder.script([report()])
        let d = makeDrainer()
        d.reconcile()
        try? await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertEqual(recorder.calls, 0)
        XCTAssertEqual(d.progress.state, .waitingForList)
    }

    func testDoesNotStartWhenAiIsOffOrNoEmbeddingModelIsChosen() async {
        recorder.script([report()])
        let d = makeDrainer()
        prefs.aiEmbeddingModel = ""
        d.firstListReachedUI()
        try? await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertEqual(recorder.calls, 0, "no embedding model chosen")
        XCTAssertEqual(d.progress.state, .off)

        prefs.aiEmbeddingModel = "embed-model"
        prefs.aiEnabled = false
        try? await Task.sleep(nanoseconds: 100_000_000)
        XCTAssertEqual(recorder.calls, 0, "AI disabled")
    }

    func testStartsOnceEnabledAndPublishesQueuedAndDoneCounts() async {
        recorder.script([report(queued: 20, embedded: 8), report(queued: 12, embedded: 16),
                         report(queued: 0, embedded: 28)])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("backlog drained") { d.progress.state == .idle }
        XCTAssertEqual(d.progress.queued, 0)
        XCTAssertEqual(d.progress.done, 28)
        XCTAssertGreaterThanOrEqual(recorder.calls, 3)
        // One small embeddings-scoped batch per call; no mail text in the config, only refs.
        let cfg = recorder.configs[0]
        XCTAssertTrue(cfg.contains("\"scope\":\"embeddings\""), cfg)
        XCTAssertTrue(cfg.contains("\"max_jobs\":8"), cfg)
        XCTAssertTrue(cfg.contains("\"embedding_model\":\"embed-model\""), cfg)
    }

    func testStopsWhenAiIsTurnedOff() async {
        recorder.script([report(queued: 5, embedded: 1)])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("first batch") { self.recorder.calls >= 2 }
        prefs.aiEnabled = false
        await waitUntil("stopped") { d.progress.state == .off }
        try? await Task.sleep(nanoseconds: 100_000_000)   // let any in-flight batch land
        let settled = recorder.calls
        try? await Task.sleep(nanoseconds: 150_000_000)
        XCTAssertEqual(recorder.calls, settled, "no batches after AI was turned off")
    }

    func testShutdownAtQuitStopsForGood() async {
        recorder.script([report(queued: 5, embedded: 1)])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("running") { self.recorder.calls >= 1 }
        d.shutdown()
        try? await Task.sleep(nanoseconds: 100_000_000)
        let settled = recorder.calls
        d.firstListReachedUI()
        d.reconcile()
        try? await Task.sleep(nanoseconds: 150_000_000)
        XCTAssertEqual(recorder.calls, settled)
        XCTAssertEqual(d.progress.state, .off)
    }

    func testOnlyOneDrainRunsAtATime() async {
        recorder.script([report(queued: 5, embedded: 1)])
        let d = makeDrainer(hold: 0.03)
        for _ in 0..<5 { d.firstListReachedUI(); d.reconcile() }
        // Preference churn restarts the loop; a slow batch from the old loop may still be in
        // flight, but the new loop must wait its start delay, and reconcile never doubles up.
        await waitUntil("a few batches") { self.recorder.calls >= 3 }
        d.reconcile()
        d.reconcile()
        XCTAssertEqual(recorder.maxInFlight, 1)
        d.shutdown()
    }

    func testBacksOffExponentiallyWhileTheEndpointIsDownThenRecovers() async {
        recorder.script([report(queued: 3, down: true, code: "network"),
                         report(queued: 3, down: true, code: "network"),
                         report(queued: 3, down: true, code: "network"),
                         report(queued: 3, down: true, code: "network"),
                         report(queued: 0, embedded: 3)])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("recovered") { d.progress.state == .idle }
        // startDelay, then 15, 30, 60, 60 (capped at backoffMax), then the idle poll.
        XCTAssertEqual(Array(recorder.delays.prefix(5)), [0.01, 15, 30, 60, 60])
        XCTAssertEqual(d.progress.done, 3)
        d.shutdown()
    }

    func testBackoffStateIsReportedWithAReadableMessage() async {
        recorder.script([report(queued: 3, down: true, code: "network")])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("backing off") {
            if case .backingOff = d.progress.state { return true } else { return false }
        }
        XCTAssertEqual(EmbeddingDrainer.errorText(for: d.progress),
                       "Ollama is not answering; embeddings will retry automatically")
        d.shutdown()
    }

    func testChatModelAsEmbeddingModelStopsWithAReadableErrorUntilChanged() async {
        prefs.aiEmbeddingModel = "qwen-chat"
        recorder.script([report(queued: 4, code: "embed_unsupported")])
        let d = makeDrainer()
        d.firstListReachedUI()
        await waitUntil("failed state") { d.progress.state == .failed(code: "embed_unsupported") }
        XCTAssertEqual(EmbeddingDrainer.errorText(for: d.progress),
                       "qwen-chat cannot produce embeddings; choose an embedding model")
        let settled = recorder.calls
        try? await Task.sleep(nanoseconds: 150_000_000)
        XCTAssertEqual(recorder.calls, settled, "a config error is not retried in a loop")

        // Choosing a real embedding model restarts it.
        recorder.script([report(queued: 0, embedded: 4)])
        prefs.aiEmbeddingModel = "embed-model"
        await waitUntil("restarted and drained") { d.progress.state == .idle }
        XCTAssertEqual(d.progress.done, 4)
        d.shutdown()
    }
}
