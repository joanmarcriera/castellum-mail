// Castellum - compose attachment state tests (S33).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Pure logic only: size warning, attachment builders, paste extraction,
// forward prefill and close-time persistence. Nothing here opens the real
// profile or the Keychain (no AppModel, no CastellumCore).

import AppKit
import XCTest
@testable import Castellum

private func meta(_ id: Int64, size: Int64, inline: Bool = false) -> DraftAttachmentMeta {
    DraftAttachmentMeta(
        id: id, ordinal: id, filename: "f\(id)", contentType: "application/octet-stream",
        sizeBytes: size, isInline: inline, contentId: inline ? "cid\(id)" : nil)
}

// MARK: - size warning

final class ComposeAttachmentSizeTests: XCTestCase {
    func testSizeWarningThresholdIs25MB() {
        XCTAssertEqual(ATTACHMENT_SIZE_WARNING_BYTES, 25 * 1_000_000)
    }

    func testExceedsIsFalseAtExactlyTheThreshold() {
        XCTAssertFalse(exceedsAttachmentSizeWarning(totalBytes: ATTACHMENT_SIZE_WARNING_BYTES))
        XCTAssertTrue(exceedsAttachmentSizeWarning(totalBytes: ATTACHMENT_SIZE_WARNING_BYTES + 1))
    }

    func testWarningTextAppearsOnlyOverTheLimit() {
        XCTAssertNil(attachmentSizeWarning(totalBytes: 1_000))
        let text = attachmentSizeWarning(totalBytes: 30_000_000)
        XCTAssertNotNil(text)
        XCTAssertTrue(text!.contains("Attachments total"))
    }

    func testTotalSumsAllAttachments() {
        let list = [meta(1, size: 10_000_000), meta(2, size: 20_000_000)]
        XCTAssertEqual(list.reduce(0) { $0 + $1.sizeBytes }, 30_000_000)
    }
}

// MARK: - builders

final class ComposeAttachmentBuilderTests: XCTestCase {
    private var dir: URL!

    override func setUpWithError() throws {
        dir = TestScratch.root
            .appendingPathComponent("castellum-compose-attach-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: dir)
    }

    func testFileAttachmentCarriesNameTypeAndBase64Bytes() throws {
        let url = dir.appendingPathComponent("report.pdf")
        try Data("hello".utf8).write(to: url)
        let a = try ComposeAttachmentBuilder.make(fileURL: url)
        XCTAssertEqual(a.filename, "report.pdf")
        XCTAssertEqual(a.contentType, "application/pdf")
        XCTAssertFalse(a.isInline)
        XCTAssertNil(a.contentId)
        XCTAssertEqual(Data(base64Encoded: a.dataBase64), Data("hello".utf8))
    }

    func testUnknownExtensionFallsBackToOctetStream() throws {
        let url = dir.appendingPathComponent("blob.zzqx")
        try Data([1, 2, 3]).write(to: url)
        XCTAssertEqual(try ComposeAttachmentBuilder.make(fileURL: url).contentType,
                       "application/octet-stream")
    }

    func testMissingFileIsAnUnreadableError() {
        let url = dir.appendingPathComponent("nope.txt")
        XCTAssertThrowsError(try ComposeAttachmentBuilder.make(fileURL: url)) { error in
            XCTAssertEqual(error as? ComposeAttachmentError, .unreadable("nope.txt"))
        }
    }

    func testPastedImageIsAlwaysARegularAttachment() throws {
        let a = try ComposeAttachmentBuilder.make(
            pastedImage: Data([0x89, 0x50]), contentType: "image/png",
            now: Date(timeIntervalSince1970: 1_000))
        XCTAssertEqual(a.filename, "Pasted Image 1000.png")
        XCTAssertEqual(a.contentType, "image/png")
        XCTAssertFalse(a.isInline, "compose is plain text: no cid: reference is possible")
        XCTAssertNil(a.contentId)
    }

    func testErrorMessagesAreUserFacing() {
        XCTAssertEqual(ComposeAttachmentError.unreadable("a.txt").message, "Couldn't read a.txt.")
        XCTAssertTrue(ComposeAttachmentError.tooLarge("a.mov").message.contains("too large"))
    }

    func testNewDraftAttachmentEncodesSnakeCase() throws {
        let attachment = NewDraftAttachment(
            filename: "logo.png", contentType: "image/png",
            isInline: true, contentId: "cid1", dataBase64: "aGVsbG8=")
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = String(data: try encoder.encode(attachment), encoding: .utf8)!
        XCTAssertTrue(json.contains("\"content_type\""))
        XCTAssertTrue(json.contains("\"data_base64\""))
        XCTAssertTrue(json.contains("\"is_inline\""))
    }
}

// MARK: - FFI round trip (temp profile only)

final class DraftAttachmentFFITests: XCTestCase {
    func testAddListRemoveRoundTripOverTheCAbi() throws {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-draft-attach-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        let core = try CastellumCore(profilePath: dir)
        let draftId = try core.saveDraft(NewDraft(accountId: "acct", to: []))
        XCTAssertEqual(try core.listDraftAttachments(draftId: draftId), [])

        let added = try core.addDraftAttachment(
            draftId: draftId,
            attachment: NewDraftAttachment(
                filename: "a.txt", contentType: "text/plain",
                dataBase64: Data("hello".utf8).base64EncodedString()))
        XCTAssertEqual(added.count, 1)
        XCTAssertEqual(added[0].filename, "a.txt")
        XCTAssertEqual(added[0].sizeBytes, 5)
        XCTAssertFalse(added[0].isInline)

        let inline = try core.addDraftAttachment(
            draftId: draftId,
            attachment: NewDraftAttachment(
                filename: "logo.png", contentType: "image/png", isInline: true,
                contentId: "cid1", dataBase64: Data([1, 2, 3]).base64EncodedString()))
        XCTAssertEqual(inline.map(\.filename), ["a.txt", "logo.png"])
        XCTAssertEqual(inline[1].contentId, "cid1")
        XCTAssertTrue(inline[1].isInline)

        let remaining = try core.removeDraftAttachment(draftId: draftId, attachmentId: added[0].id)
        XCTAssertEqual(remaining.map(\.filename), ["logo.png"])
        XCTAssertThrowsError(try core.addDraftAttachment(
            draftId: draftId,
            attachment: NewDraftAttachment(filename: "x", contentType: "text/plain", dataBase64: "!!!")))
    }
}

// MARK: - paste

final class PasteboardAttachmentsTests: XCTestCase {
    /// A private, uniquely named pasteboard: never touches the user's clipboard.
    private func pasteboard() -> NSPasteboard {
        NSPasteboard(name: NSPasteboard.Name("castellum-test-\(UUID().uuidString)"))
    }

    func testFinderFilesBecomeFileItems() {
        let pb = pasteboard()
        defer { pb.releaseGlobally() }
        pb.clearContents()
        let url = URL(fileURLWithPath: "/tmp/castellum-paste-test.txt")
        pb.writeObjects([url as NSURL])
        XCTAssertEqual(PasteboardAttachments.items(from: pb), [.file(url)])
    }

    func testBareImageBecomesAnImageItem() {
        let pb = pasteboard()
        defer { pb.releaseGlobally() }
        pb.clearContents()
        pb.setData(Data([0x89, 0x50, 0x4e, 0x47]), forType: .png)
        XCTAssertEqual(PasteboardAttachments.items(from: pb),
                       [.image(Data([0x89, 0x50, 0x4e, 0x47]), contentType: "image/png")])
    }

    func testTextWithAnImageFlavourIsLeftToOrdinaryTextPaste() {
        let pb = pasteboard()
        defer { pb.releaseGlobally() }
        pb.clearContents()
        pb.setString("some copied text", forType: .string)
        pb.setData(Data([1, 2, 3]), forType: .png)
        XCTAssertTrue(PasteboardAttachments.items(from: pb).isEmpty)
    }

    func testPlainTextAloneAttachesNothing() {
        let pb = pasteboard()
        defer { pb.releaseGlobally() }
        pb.clearContents()
        pb.setString("hello", forType: .string)
        XCTAssertTrue(PasteboardAttachments.items(from: pb).isEmpty)
    }
}

// MARK: - forward prefill and close behaviour

@MainActor
final class ComposeForwardTests: XCTestCase {
    private func detail(attachmentCount: Int) -> MessageDetail {
        let descriptors: [AttachmentDescriptor]? = attachmentCount == 0 ? [] : (0..<attachmentCount).map {
            AttachmentDescriptor(
                ordinal: Int64($0), filename: "f\($0).pdf", contentType: "application/pdf",
                sizeBytes: 10, section: "\($0 + 2)", contentId: nil, isInline: false,
                transferEncoding: nil)
        }
        return MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb", subject: "Hello",
                fromName: "Alice", fromAddress: "alice@example.com", sentAt: 0, snippet: nil,
                hasAttachments: attachmentCount > 0, hasBody: true, isRead: true,
                isStarred: false, threadCount: 1),
            rfc822MessageId: "<orig@x.test>",
            addresses: [Address(kind: "from", name: "Alice", address: "alice@example.com")],
            textBody: "Original body.", sanitizedHtml: nil, hasRemoteContent: false,
            attachments: descriptors)
    }

    func testForwardPrefillHasNoRecipientsNoThreadingAndAFwdSubject() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 2), mode: .forward,
                                  selfEmail: "me@example.com")
        XCTAssertTrue(p.to.isEmpty)
        XCTAssertEqual(p.subject, "Fwd: Hello")
        XCTAssertTrue(p.isForward)
        XCTAssertEqual(p.originalDbId, "m1")
        XCTAssertEqual(p.forwardAttachmentCount, 2)
        XCTAssertNil(p.inReplyTo)
        XCTAssertNil(p.referencesHeader)
        XCTAssertTrue(p.body.contains("Forwarded message"))
        XCTAssertTrue(p.body.contains("From: Alice <alice@example.com>"))
        XCTAssertTrue(p.body.contains("Original body."))
    }

    func testForwardSubjectPrefixIsNotDoubled() {
        var d = detail(attachmentCount: 0)
        d = MessageDetail(
            header: MessageHeader(
                id: "m1", accountId: "acct", mailboxId: "mb", subject: "Fwd: Hello",
                fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                hasAttachments: false, hasBody: true, isRead: true, isStarred: false,
                threadCount: 1),
            rfc822MessageId: nil, addresses: [], textBody: nil, sanitizedHtml: nil,
            hasRemoteContent: false, attachments: [])
        XCTAssertEqual(ReplyPrefill.make(detail: d, mode: .forward, selfEmail: "me@x.test").subject,
                       "Fwd: Hello")
    }

    func testReplyPrefillIsNotAForward() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 1), mode: .reply,
                                  selfEmail: "me@example.com")
        XCTAssertFalse(p.isForward)
        XCTAssertEqual(p.forwardAttachmentCount, 0)
    }

    func testForwardSessionRemembersItsSourceButIsNotAnAiReply() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 1), mode: .forward,
                                  selfEmail: "me@example.com")
        let s = ComposeSession(accountId: "acct", prefill: p)
        XCTAssertEqual(s.forwardSourceMessageId, "m1")
        XCTAssertEqual(s.forwardSourceAttachmentCount, 1)
        XCTAssertNil(s.originalMessageDbId, "AI must not draft a reply to a forwarded message")
    }

    func testReplySessionHasNoForwardSource() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 1), mode: .reply,
                                  selfEmail: "me@example.com")
        let s = ComposeSession(accountId: "acct", prefill: p)
        XCTAssertNil(s.forwardSourceMessageId)
        XCTAssertEqual(s.originalMessageDbId, "m1")
    }

    func testAttachmentsCountAsContent() {
        let s = ComposeSession(accountId: "acct")
        XCTAssertFalse(s.hasContent)
        s.attachments = [meta(1, size: 5)]
        XCTAssertTrue(s.hasContent, "an attachment-only message can be sent")
    }

    func testUntouchedForwardWindowWithASavedDraftDeletesIt() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 1), mode: .forward,
                                  selfEmail: "me@example.com")
        let s = ComposeSession(accountId: "acct", prefill: p)
        XCTAssertEqual(s.closeAction, .none, "no draft was created yet")
        s.draftId = 42   // the early save that holds the copied attachments
        XCTAssertEqual(s.closeAction, .deleteDraft(42), "untouched forward leaves no orphan draft")
    }

    func testEditedForwardWindowSavesOnClose() {
        let p = ReplyPrefill.make(detail: detail(attachmentCount: 1), mode: .forward,
                                  selfEmail: "me@example.com")
        let s = ComposeSession(accountId: "acct", prefill: p)
        s.draftId = 42
        s.toText = "bob@example.com"
        XCTAssertEqual(s.closeAction, .save)
    }
}
