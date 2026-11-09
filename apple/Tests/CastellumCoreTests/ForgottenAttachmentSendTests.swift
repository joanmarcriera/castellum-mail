// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// HP5a: the compose session's send gate (view-model level). `send` is a
/// counter closure, so nothing touches the core, the Keychain or a server.
@MainActor
final class ForgottenAttachmentSendTests: XCTestCase {
    private func makeSession(
        body: String = "Please see the attached file.",
        warn: Bool = true,
        attachments: [DraftAttachmentMeta] = []
    ) -> ComposeSession {
        let suite = "ForgottenAttachmentSendTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        let prefs = Preferences(defaults: defaults)
        prefs.warnAboutForgottenAttachment = warn
        let s = ComposeSession(accountId: "acct", preferences: prefs)
        s.toText = "ada@x.test"
        s.subject = "Hello"
        s.body = body
        s.attachments = attachments
        return s
    }

    private let file = DraftAttachmentMeta(
        id: 1, ordinal: 0, filename: "a.pdf", contentType: "application/pdf",
        sizeBytes: 10, isInline: false, contentId: nil)

    func testPreferenceDefaultsToOn() {
        let suite = "ForgottenAttachmentSendTests-default-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        XCTAssertTrue(Preferences(defaults: defaults).warnAboutForgottenAttachment)
    }

    func testPreferencePersists() {
        let suite = "ForgottenAttachmentSendTests-persist-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defaults.removePersistentDomain(forName: suite)
        Preferences(defaults: defaults).warnAboutForgottenAttachment = false
        XCTAssertFalse(Preferences(defaults: defaults).warnAboutForgottenAttachment)
    }

    func testAsksFirstThenSendsOnlyOnSendAnyway() {
        let s = makeSession()
        var sent = 0
        s.requestSend { sent += 1 }
        XCTAssertEqual(sent, 0, "must ask before sending")
        XCTAssertEqual(s.attachmentWarningWord, "attached")
        s.resolveAttachmentWarning(sendAnyway: true) { sent += 1 }
        XCTAssertEqual(sent, 1)
        XCTAssertNil(s.attachmentWarningWord)
    }

    func testCancelKeepsTheDraftAndDoesNotSend() {
        let s = makeSession()
        var sent = 0
        s.requestSend { sent += 1 }
        s.resolveAttachmentWarning(sendAnyway: false) { sent += 1 }
        XCTAssertEqual(sent, 0)
        XCTAssertNil(s.attachmentWarningWord)
        XCTAssertEqual(s.body, "Please see the attached file.")
        XCTAssertEqual(s.toText, "ada@x.test")
        XCTAssertFalse(s.suppressSaveOnClose)
    }

    func testPreferenceOffSendsDirectly() {
        let s = makeSession(warn: false)
        var sent = 0
        s.requestSend { sent += 1 }
        XCTAssertEqual(sent, 1)
        XCTAssertNil(s.attachmentWarningWord)
    }

    func testAttachmentPresentSendsDirectly() {
        let s = makeSession(attachments: [file])
        var sent = 0
        s.requestSend { sent += 1 }
        XCTAssertEqual(sent, 1)
        XCTAssertNil(s.attachmentWarningWord)
    }

    func testNoKeywordSendsDirectly() {
        let s = makeSession(body: "See you Thursday.")
        var sent = 0
        s.requestSend { sent += 1 }
        XCTAssertEqual(sent, 1)
    }

    func testInvalidMessageSkipsTheQuestionAndLetsSendReportTheProblem() {
        let s = makeSession()
        s.toText = ""
        var sent = 0
        s.requestSend { sent += 1 }
        XCTAssertEqual(sent, 1)
        XCTAssertNil(s.attachmentWarningWord)
    }
}
