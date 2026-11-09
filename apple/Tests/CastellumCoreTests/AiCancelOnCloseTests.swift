// Castellum - cancelling in-flight AI jobs when their view goes away.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Closing a reply window or navigating off a message destroys the only place an
// AI result could be shown. Before this, the job kept generating on the Ollama
// endpoint and its entry sat in `AiCoordinator.jobs` as a zombie until restart.

import XCTest
@testable import Castellum

@MainActor
final class AiCancelOnCloseTests: XCTestCase {
    /// `core` is held weakly by the coordinator, so the test must keep it
    /// alive — otherwise `cancel(jobId:)` bails at its `guard let core` and the
    /// optimistic status this asserts on is never applied.
    private var core: CastellumCore!

    override func setUpWithError() throws {
        core = try CastellumCore(
            profilePath: TestScratch.root
                .appendingPathComponent("castellum-ai-cancel-\(UUID().uuidString)"))
    }

    override func tearDown() { core = nil }

    private func coordinator() -> AiCoordinator {
        AiCoordinator(core: core, prefs: Preferences.shared)
    }

    private func job(_ id: Int64, _ owner: String, _ feature: String,
                     _ status: String) -> AiJobViewState {
        AiJobViewState(id: id, feature: feature, ownerId: owner, status: status,
                       text: "", errorCode: nil, isCached: false, lastEventId: 0)
    }

    func testCancelsOnlyLiveJobsForThatOwnerAndFeature() {
        let c = coordinator()
        c.jobs = [
            1: job(1, "msg-a", "draft_reply", "streaming"),   // cancel
            2: job(2, "msg-a", "draft_reply", "succeeded"),   // terminal
            3: job(3, "msg-b", "draft_reply", "running"),     // other message
            4: job(4, "msg-a", "summarize_message", "running"), // other feature
            5: job(5, "msg-a", "draft_reply", "queued"),      // cancel
        ]

        let cancelled = c.cancelActiveJobs(forOwnerId: "msg-a", feature: "draft_reply")

        XCTAssertEqual(cancelled, [1, 5])
        XCTAssertEqual(c.jobs[1]?.status, "cancelled")
        XCTAssertEqual(c.jobs[5]?.status, "cancelled")
        // A second window drafting against another message must not be hit.
        XCTAssertEqual(c.jobs[3]?.status, "running")
        XCTAssertEqual(c.jobs[4]?.status, "running")
        XCTAssertEqual(c.jobs[2]?.status, "succeeded")
    }

    /// Cached results live in the negative id space and were never a running
    /// job, so cancelling one would hand the core an id that does not exist.
    func testCachedEntriesAreNeverCancelled() {
        let c = coordinator()
        c.jobs = [-7: job(-7, "msg-a", "summarize_message", "running")]
        XCTAssertEqual(c.cancelActiveJobs(forOwnerId: "msg-a", feature: "summarize_message"), [])
        XCTAssertEqual(c.jobs[-7]?.status, "running")
    }

    /// The core only sets a flag when cancelling a running job — the row keeps
    /// its running/streaming status until the executor notices. A poll already
    /// in flight would write that back over the optimistic "cancelled",
    /// resurrecting a job the user was told was cancelled.
    func testACancelledJobStaysCancelledAndIsExcludedFromTheActiveSet() {
        let c = coordinator()
        c.jobs = [1: job(1, "msg-a", "summarize_message", "running")]
        c.cancel(jobId: 1)
        XCTAssertEqual(c.jobs[1]?.status, "cancelled")
        XCTAssertTrue(c.jobs[1]?.isTerminal ?? false,
                      "a cancelled job must be terminal, or the poll loop keeps it active")
    }

    /// A stale poll can include `started` after local cancellation. The reducer
    /// must not let that event resurrect the job before the core acknowledges
    /// the cancellation with a terminal status.
    func testStaleStartedEventCannotUndoRequestedCancellation() {
        var state = job(1, "msg-a", "summarize_message", "cancelled")
        AiEventReducer.apply(
            event: AiEvent(id: 1, kind: "started", payload: ""),
            to: &state
        )
        XCTAssertEqual(state.status, "running", "proves the stale event changes reducer state")

        state.status = AiJobViewState.resolvedPollStatus(
            polledStatus: "running",
            reducedStatus: state.status,
            cancellationRequested: true
        )
        XCTAssertEqual(state.status, "cancelled")
        XCTAssertEqual(
            AiJobViewState.resolvedPollStatus(
                polledStatus: "succeeded",
                reducedStatus: "succeeded",
                cancellationRequested: true
            ),
            "succeeded",
            "a real terminal result must win if completion beat cancellation"
        )
    }

    /// Cancelling a finished job would replace a summary the user can still
    /// read with "cancelled", and nothing repairs it — the poll loop skips
    /// terminal jobs. Reachable via close-on-send and via re-selecting a
    /// message whose summary already succeeded.
    func testCancellingATerminalJobLeavesItAlone() {
        let c = coordinator()
        c.jobs = [1: job(1, "msg-a", "summarize_message", "succeeded")]
        c.cancel(jobId: 1)
        XCTAssertEqual(c.jobs[1]?.status, "succeeded")
    }

    func testNothingToCancelIsHarmless() {
        let c = coordinator()
        XCTAssertEqual(c.cancelActiveJobs(forOwnerId: "msg-a", feature: "draft_reply"), [])
    }

    /// The instruction reaching the prompt builder must be trimmed, not merely
    /// tested for emptiness after trimming — that was the pre-fix behaviour and
    /// it put stray whitespace/newlines into the model's context.
    func testInstructionIsTrimmedBeforeUse() {
        XCTAssertEqual(AiDraftInstruction.normalised("  reply politely\n "), "reply politely")
        XCTAssertNil(AiDraftInstruction.normalised("   \n "))
    }

    /// #3381: a runner that throws (endpoint refused) must not leave queued jobs
    /// spinning; running jobs and cached entries are left alone.
    func testRunnerFailureFailsOnlyQueuedJobs() {
        let c = coordinator()
        c.jobs = [
            1: job(1, "msg-a", "summarize_message", "queued"),
            2: job(2, "msg-b", "summarize_message", "streaming"),
            3: job(3, "msg-c", "summarize_message", "succeeded"),
            -9: job(-9, "msg-d", "summarize_message", "queued"),
        ]
        let failed = c.failUnstartedJobs(errorCode: "endpoint_rejected")
        XCTAssertEqual(failed, [1])
        XCTAssertEqual(c.jobs[1]?.status, "failed")
        XCTAssertEqual(c.jobs[1]?.errorCode, "endpoint_rejected")
        XCTAssertTrue(c.jobs[1]?.isTerminal ?? false, "the poll loop can now drain")
        XCTAssertEqual(c.jobs[2]?.status, "streaming")
        XCTAssertEqual(c.jobs[3]?.status, "succeeded")
        XCTAssertEqual(c.jobs[-9]?.status, "queued")
    }
}
