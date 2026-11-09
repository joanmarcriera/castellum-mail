// Castellum - a standalone message window for a descriptor-only message (#3441).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Double-clicking a message whose body was never downloaded used to open a dead
// "Message body not downloaded" window and mark the message read. The window now
// owns its hydration state: it shows a spinner or a Retry, and the message is only
// marked read once a body is available.

import XCTest
@testable import Castellum

@MainActor
final class StandaloneMessageStateTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let model = AppModel(
            profileDirectory: TestScratch.root
                .appendingPathComponent("castellum-standalone-\(UUID().uuidString)"))
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func detail(hasBody: Bool?, isRead: Bool = false) -> MessageDetail {
        MessageDetail(
            header: MessageHeader(
                id: "acct:INBOX:1", accountId: "acct", mailboxId: "acct:INBOX",
                subject: "Hi", fromName: nil, fromAddress: "a@x.test", sentAt: nil, snippet: nil,
                hasAttachments: false, hasBody: hasBody, isRead: isRead, isStarred: false,
                threadCount: 1),
            rfc822MessageId: nil, addresses: [], textBody: hasBody == true ? "hello" : nil,
            sanitizedHtml: nil, hasRemoteContent: nil, attachments: nil, trust: nil)
    }

    func testMissingCredentialsShowARetryAndNeverMarkRead() {
        let model = makeModel()
        let header = detail(hasBody: false).header
        model.messages = [header]
        let state = model.beginStandaloneMessage(
            detail: detail(hasBody: false), markReadAfterBody: header)
        XCTAssertFalse(state.isHydrating)
        XCTAssertNotNil(state.error, "the window says why there is no body")
        XCTAssertNotNil(state.retry, "and offers Retry instead of a dead placeholder")
        XCTAssertFalse(model.messages[0].isRead, "a failed open is not a read")
    }

    func testAMessageWithABodyOpensWithoutHydrating() {
        let model = makeModel()
        let state = model.beginStandaloneMessage(detail: detail(hasBody: true), markReadAfterBody: nil)
        XCTAssertFalse(state.isHydrating)
        XCTAssertNil(state.error)
        XCTAssertNil(state.retry)
        XCTAssertEqual(state.detail.textBody, "hello")
    }
}
