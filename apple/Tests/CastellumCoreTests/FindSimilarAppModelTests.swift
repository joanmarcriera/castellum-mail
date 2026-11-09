// Castellum - "Find similar" AppModel action tests (S53-UI).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Every AppModel here runs against a temp profile directory (never the real
// profile or the real Keychain namespace) — see RawSourceAppModelTests for
// the same pattern this file follows.
//
// DEVIATION from the step-level plan: the plan asked for these tests to use
// "a fake/injected CastellumCore call, the same pattern existing AppModel
// tests inject one for summarize/draft". No such seam exists anywhere in
// this codebase — CastellumCore is a concrete final class with no protocol,
// and grepping every existing AI-adjacent test (AiCancelOnCloseTests,
// AiEventReducerTests) turned up none that exercise summarize()/draftReply()
// through a fake core either; they test the pure reducer or manipulate
// AiCoordinator.jobs directly instead. So these tests run against the REAL
// core (RawSourceAppModelTests' own established pattern) and prove the
// AppModel-level state machine using the message-does-not-exist fast-fail
// path, which `embed_message_ad_hoc` resolves BEFORE any network call
// (`run::build_embed_text` returns `None` for an unknown message id) — a
// deterministic, network-free error, not a flaky timeout.

import XCTest
@testable import Castellum

@MainActor
final class FindSimilarAppModelTests: XCTestCase {
    private var models: [AppModel] = []
    private var originalAiEnabled = false
    private var originalAiModel = ""
    private var originalAiEmbeddingModel = ""
    private var originalAiEndpoint = ""

    override func setUp() {
        super.setUp()
        let prefs = Preferences.shared
        originalAiEnabled = prefs.aiEnabled
        originalAiModel = prefs.aiModel
        originalAiEmbeddingModel = prefs.aiEmbeddingModel
        originalAiEndpoint = prefs.aiEndpoint
        prefs.aiEnabled = true
        prefs.aiModel = "test-model"
        prefs.aiEmbeddingModel = "test-embed"
        prefs.aiEndpoint = "http://127.0.0.1:11434"
    }

    func testConnectionRefusedIsHumanReadable() {
        let raw = "network error: connection refused: http://127.0.0.1:1/api/embeddings: Connection Failed: Connect error: Connection refused (os error 61)"
        let text = AppModel.findSimilarMessage(for: raw, model: "m")
        XCTAssertTrue(text.contains("Settings"), text)
        XCTAssertFalse(text.contains("os error"), text)
        XCTAssertFalse(text.contains("/api/embeddings"), text)
        let unknown = AppModel.findSimilarMessage(for: "weird SENTINEL failure", model: "m")
        XCTAssertFalse(unknown.contains("SENTINEL"))
        XCTAssertTrue(AppModel.findSimilarMessage(for: "HTTP 500 boom", model: "m").contains("m"))
    }

    override func tearDown() async throws {
        let prefs = Preferences.shared
        prefs.aiEnabled = originalAiEnabled
        prefs.aiModel = originalAiModel
        prefs.aiEmbeddingModel = originalAiEmbeddingModel
        prefs.aiEndpoint = originalAiEndpoint
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-findsimilar-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    private func anchorHeader(id: String = "acc1:INBOX:no-such-message") -> MessageHeader {
        MessageHeader(
            id: id, accountId: "acc1", mailboxId: "acc1:INBOX",
            subject: "Anchor subject", fromName: "Ada", fromAddress: "ada@x.test",
            sentAt: 1_700_000_000, snippet: nil, hasAttachments: false, hasBody: true,
            isRead: true, isStarred: false, threadCount: 1)
    }

    func testFindSimilarSetsAnchorAndInFlightImmediately() {
        let model = makeModel()
        let anchor = anchorHeader()

        // Synchronous, no `await` — the detached Task cannot have run any of
        // its `await MainActor.run` yet, so this is deterministic, not racy.
        model.findSimilar(anchor: anchor)

        XCTAssertEqual(model.findSimilarAnchor?.id, anchor.id)
        XCTAssertTrue(model.findSimilarInFlight)
        XCTAssertNil(model.findSimilarError)
        XCTAssertTrue(model.findSimilarResults.isEmpty)
    }

    func testFindSimilarForANonexistentMessageFailsFastAndClearsInFlight() async {
        let model = makeModel()
        let anchor = anchorHeader()
        model.findSimilar(anchor: anchor)
        XCTAssertTrue(model.findSimilarInFlight, "in-flight is set before the FFI call even starts")

        let deadline = Date().addingTimeInterval(5)
        while model.findSimilarInFlight, Date() < deadline {
            try? await Task.sleep(nanoseconds: 20_000_000)
        }

        XCTAssertFalse(
            model.findSimilarInFlight,
            "must never hang — a nonexistent message resolves before any network call")
        XCTAssertNotNil(model.findSimilarError)
        XCTAssertTrue(model.findSimilarResults.isEmpty)
        XCTAssertEqual(model.findSimilarAnchor?.id, anchor.id, "the sheet stays open to show the error")
    }

    func testFindSimilarWithoutAiConfiguredFailsImmediatelyWithoutEverSettingInFlight() {
        let model = makeModel()
        Preferences.shared.aiEmbeddingModel = "" // no embedding model: nothing can index or embed

        model.findSimilar(anchor: anchorHeader())

        XCTAssertFalse(
            model.findSimilarInFlight, "no FFI call is even attempted without a configured model")
        XCTAssertNotNil(model.findSimilarError)
        XCTAssertNotNil(model.findSimilarAnchor, "the sheet still opens, showing the inline error")
    }

    /// 2871: with only a chat model chosen, Find Similar used to embed with the chat model
    /// (Ollama HTTP 500) while nothing indexed mail. It must say what to do instead.
    func testFindSimilarWithOnlyAChatModelAsksForAnEmbeddingModel() {
        let model = makeModel()
        Preferences.shared.aiEmbeddingModel = ""
        Preferences.shared.aiModel = "llama3.1:8b"

        model.findSimilar(anchor: anchorHeader())

        XCTAssertFalse(model.findSimilarInFlight)
        XCTAssertEqual(
            model.findSimilarError,
            String(localized: "Find Similar needs an embedding model: choose one in Settings → AI"))
    }

    /// AI switched off: `findSimilarConfigJSON()` is nil, so the old "set an endpoint" text
    /// shows and no FFI call is attempted.
    func testFindSimilarWithAiDisabledShowsTheSetEndpointMessage() {
        let model = makeModel()
        Preferences.shared.aiEnabled = false

        model.findSimilar(anchor: anchorHeader())

        XCTAssertFalse(model.findSimilarInFlight)
        XCTAssertEqual(model.findSimilarError, "Set an Ollama endpoint and model in Settings → AI first.")
    }

    func testDismissFindSimilarClearsEveryPieceOfState() {
        let model = makeModel()
        Preferences.shared.aiEmbeddingModel = ""
        model.findSimilar(anchor: anchorHeader())
        XCTAssertNotNil(model.findSimilarAnchor)
        XCTAssertNotNil(model.findSimilarError)

        model.dismissFindSimilar()

        XCTAssertNil(model.findSimilarAnchor)
        XCTAssertNil(model.findSimilarError)
        XCTAssertFalse(model.findSimilarInFlight)
        XCTAssertTrue(model.findSimilarResults.isEmpty)
    }

    /// A second gesture on a DIFFERENT message while the first is still
    /// resolving must not let the first's late-arriving result/error
    /// clobber the second's state — the callback re-checks
    /// `findSimilarAnchor?.id` before writing (see `AppModel.findSimilar`).
    func testASecondGestureSupersedesAFirstStillInFlight() async {
        let model = makeModel()
        let first = anchorHeader(id: "acc1:INBOX:first")
        let second = anchorHeader(id: "acc1:INBOX:second")

        model.findSimilar(anchor: first)
        model.findSimilar(anchor: second)
        XCTAssertEqual(model.findSimilarAnchor?.id, second.id)

        let deadline = Date().addingTimeInterval(5)
        while model.findSimilarInFlight, Date() < deadline {
            try? await Task.sleep(nanoseconds: 20_000_000)
        }

        XCTAssertEqual(
            model.findSimilarAnchor?.id, second.id,
            "the first gesture's late completion must not overwrite the second's anchor")
    }
}
