// Castellum - raw source fetch + .eml import AppModel tests (S49).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Every AppModel here runs against a temp profile directory (never the real
// profile or the real Keychain namespace) — see AttachmentDTOTests for the
// same pattern this file follows.

import XCTest
@testable import Castellum

@MainActor
final class RawSourceAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-rawsource-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    func testFetchRawSourcePathFailsFastWithoutStoredCredentials() async {
        let model = makeModel()
        do {
            _ = try await model.fetchRawSourcePath(messageId: "nope:1", accountId: "no-such-account")
            XCTFail("expected an error")
        } catch {
            guard case CastellumError.message(let text) = error else {
                return XCTFail("expected CastellumError.message, got \(error)")
            }
            XCTAssertTrue(text.contains("credentials"))
        }
    }

    func testImportEmlResultDecodesBothStatuses() throws {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let queued = try decoder.decode(
            ImportEmlResult.self, from: Data(#"{"status":"queued","error":null}"#.utf8))
        XCTAssertEqual(queued.status, "queued")
        XCTAssertNil(queued.error)

        let failed = try decoder.decode(
            ImportEmlResult.self,
            from: Data(#"{"status":"failed","error":"doesn't look like a valid email"}"#.utf8))
        XCTAssertEqual(failed.status, "failed")
        XCTAssertEqual(failed.error, "doesn't look like a valid email")
    }

    func testImportEmlFilesWithNoUrlsIsANoOp() {
        let model = makeModel()
        model.importEmlFiles(urls: [], mailboxId: "acc1:INBOX", accountId: "acc1")
        XCTAssertNil(model.errorText, "no urls means no work and no error")
    }

    func testImportEmlFilesReportsAFailureThroughErrorText() async throws {
        let model = makeModel()
        let dir = TestScratch.root
            .appendingPathComponent("castellum-rawsource-import-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let garbage = dir.appendingPathComponent("garbage.eml")
        try Data(repeating: 0xFF, count: 64).write(to: garbage)

        // No account/mailbox exists in this fresh profile, so the core's
        // enqueue refuses — the failure path is what this test proves
        // reaches errorText, regardless of the exact refusal reason.
        model.importEmlFiles(urls: [garbage], mailboxId: "acc1:INBOX", accountId: "acc1")

        let deadline = Date().addingTimeInterval(5)
        while model.errorText == nil, Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        XCTAssertNotNil(model.errorText, "the failed import surfaces through errorText")
    }
}
