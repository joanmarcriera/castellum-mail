// Castellum - regression tests for (flicker after a refused
// write on a read-only account).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `mutateRemoving` (AppModel.swift) backs delete/archive/snooze: it used to
// remove the row from `messages` optimistically, then — once the async write
// came back refused (the account is read-only/evaluation mode) — restore it
// via a full reload. That remove-then-restore round trip is the flicker: the
// row vanished for a moment and then came back.
//
// The fix skips the optimistic removal entirely for an id whose account
// `writeState(for:)` already reports read-only, since the write is certain
// to be refused. These tests assert that skip SYNCHRONOUSLY, right after the
// mutating call returns and before its background `Task.detached` write has
// had any chance to run — exactly the window where the old code had already
// pulled the row out of `messages`.
//
// No account is ever added, so every account id here is unprobed and, per
// `AccountWritePolicy`'s fail-closed rule, reads as read-only by default —
// the same state a fresh evaluation-mode account has (see AccountWritePolicyUITests).

import XCTest
@testable import Castellum

@MainActor
final class RefusedWriteFlickerAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-flicker-\(UUID().uuidString)")
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

    private func header(id: String, accountId: String) -> MessageHeader {
        MessageHeader(
            id: id, accountId: accountId, mailboxId: "\(accountId):INBOX",
            subject: "Test", fromName: nil, fromAddress: nil, sentAt: nil,
            snippet: nil, hasAttachments: false, hasBody: nil,
            isRead: false, isStarred: false, threadCount: 1)
    }

    func testDeleteOnAKnownReadOnlyAccountNeverRemovesTheRowOptimistically() {
        let model = makeModel()
        XCTAssertTrue(model.writeState(for: "acct-ro").isReadOnly, "unprobed accounts fail closed")

        model.messages = [header(id: "msg-1", accountId: "acct-ro")]
        model.selectedMessageIds = ["msg-1"]

        model.deleteSelected()

        // Checked synchronously: the old code called `removeFromList` before
        // ever spawning the background write, so this is precisely the
        // instant the row used to disappear.
        XCTAssertTrue(
            model.messages.contains { $0.id == "msg-1" },
            "a delete on a known-read-only account must not optimistically remove the row " +
                "— the write is certain to be refused, so there is nothing to restore later")
    }

    func testArchiveOnAKnownReadOnlyAccountNeverRemovesTheRowOptimistically() {
        // archiveSelected shares `mutateRemoving` with deleteSelected — one
        // fix at that shared site covers both.
        let model = makeModel()
        model.messages = [header(id: "msg-2", accountId: "acct-ro")]
        model.selectedMessageIds = ["msg-2"]

        model.archiveSelected()

        XCTAssertTrue(
            model.messages.contains { $0.id == "msg-2" },
            "archive on a known-read-only account must not optimistically remove the row")
    }
}
