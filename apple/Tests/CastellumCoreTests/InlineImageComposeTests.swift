// Castellum - inline images in Markdown mail, compose side (MM-5b, slice 2):
// the pure reference rules, the core round trip over the C ABI, and the compose session's
// paste / drop / remove behaviour against a real profile.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

/// A real 2x2 PNG (so a preview or a send could render it), built once.
enum TestPNG {
    static let data: Data = {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 2, pixelsHigh: 2, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        return rep.representation(using: .png, properties: [:])!
    }()
}

final class ComposeInlineImagesPureTests: XCTestCase {
    private let cid = "img-0123456789ab@castellum"

    func testRemovesOnlyTheNamedReference() {
        let body = "Hello ![shot](cid:\(cid)) and ![other](cid:img-ffffffffffff@castellum) end"
        XCTAssertEqual(
            ComposeInlineImages.removingReferences(to: cid, from: body),
            "Hello  and ![other](cid:img-ffffffffffff@castellum) end")
    }

    func testAReferenceAloneOnItsLineLeavesNoBlankLine() {
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: cid, from: "a\n![x](cid:\(cid))\nb"), "a\nb")
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: cid, from: "![x](cid:\(cid))"), "")
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: cid, from: "a\n\n![x](cid:\(cid))"), "a\n\n")
    }

    func testEveryOccurrenceGoesAndTheContentIdIsMatchedExactly() {
        let body = "![a](cid:\(cid)) mid ![b](cid:\(cid))"
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: cid, from: body), " mid ")
        // A prefix of another id must not match.
        XCTAssertEqual(
            ComposeInlineImages.removingReferences(to: "img-0123456789ab@castellum", from: "![a](cid:img-0123456789abc@castellum)"),
            "![a](cid:img-0123456789abc@castellum)")
    }

    func testRegexMetacharactersInAnIdAreLiteral() {
        let odd = "img-a.b+c@castellum"
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: odd, from: "x ![i](cid:\(odd)) y"), "x  y")
        XCTAssertEqual(ComposeInlineImages.removingReferences(to: odd, from: "x ![i](cid:img-aXbbc@castellum) y"),
                       "x ![i](cid:img-aXbbc@castellum) y", ". is not a wildcard")
    }

    func testReferencesDetection() {
        XCTAssertTrue(ComposeInlineImages.references(cid, in: "see ![x](cid:\(cid))"))
        XCTAssertFalse(ComposeInlineImages.references(cid, in: "see ![x](https://x.test/a.png)"))
    }

    func testLooksLikeImageByExtension() {
        for name in ["a.png", "a.JPG", "a.jpeg", "a.gif", "a.webp", "a.heic", "a.tiff"] {
            XCTAssertTrue(ComposeInlineImages.looksLikeImage(URL(fileURLWithPath: "/tmp/\(name)")), name)
        }
        for name in ["a.pdf", "a.txt", "a.zip", "a"] {
            XCTAssertFalse(ComposeInlineImages.looksLikeImage(URL(fileURLWithPath: "/tmp/\(name)")), name)
        }
    }

    func testFallbackNotesNameTheFileAndTheReason() {
        XCTAssertTrue(ComposeInlineImages.fallbackNote(filename: "big.png", reason: "too_large: x").contains("10 MB"))
        XCTAssertTrue(ComposeInlineImages.fallbackNote(filename: "a.heic", reason: "not_an_image: x").contains("a.heic"))
    }
}

final class InlineImageTranscodeTests: XCTestCase {
    private func tiff() -> Data {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 3, pixelsHigh: 3, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        return rep.representation(using: .tiff, properties: [:])!
    }

    func testTiffIsReEncodedAsPNGWithANewExtension() throws {
        let data = tiff()
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: data), "image/tiff")
        let out = ComposeInlineImages.preparedForEmbedding(data, filename: "scan.tiff")
        XCTAssertEqual(out.filename, "scan.png")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: out.data), "image/png")
    }

    func testBmpIsReEncodedAsPNG() throws {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 2, pixelsHigh: 2, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        let bmp = try XCTUnwrap(rep.representation(using: .bmp, properties: [:]))
        let out = ComposeInlineImages.preparedForEmbedding(bmp, filename: "a.bmp")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: out.data), "image/png")
        XCTAssertEqual(out.filename, "a.png")
    }

    func testHeicIsReEncodedAsPNGWhenTheMachineCanWriteOne() throws {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 8, pixelsHigh: 8, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        let out = NSMutableData()
        guard let dest = CGImageDestinationCreateWithData(out, "public.heic" as CFString, 1, nil),
              let cg = rep.cgImage else { throw XCTSkip("no HEIC encoder on this machine") }
        CGImageDestinationAddImage(dest, cg, nil)
        guard CGImageDestinationFinalize(dest) else { throw XCTSkip("HEIC encode failed here") }
        let heic = out as Data
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: heic), "image/heic")
        let prepared = ComposeInlineImages.preparedForEmbedding(heic, filename: "IMG_0001.HEIC")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: prepared.data), "image/png")
        XCTAssertEqual(prepared.filename, "IMG_0001.png")
    }

    func testPNGJPEGAndUnknownBytesPassThroughUntouched() {
        let jpeg = Data([0xFF, 0xD8, 0xFF, 0xE0, 0, 0])
        for (data, name) in [(TestPNG.data, "a.png"), (jpeg, "b.jpg"), (Data("not an image".utf8), "c.heic")] {
            let out = ComposeInlineImages.preparedForEmbedding(data, filename: name)
            XCTAssertEqual(out.data, data)
            XCTAssertEqual(out.filename, name)
        }
    }

    func testAnUndecodableHeicHeaderIsLeftForTheCoreToRefuse() {
        // Looks like HEIC by its ftyp box but is not decodable.
        var junk = Data([0, 0, 0, 24]); junk.append(Data("ftypheic".utf8)); junk.append(Data(count: 16))
        let out = ComposeInlineImages.preparedForEmbedding(junk, filename: "x.heic")
        XCTAssertEqual(out.data, junk)
        XCTAssertEqual(out.filename, "x.heic")
    }

    func testNotesForARefusedInsert() {
        XCTAssertTrue(ComposeInlineImages.cannotInsertNote(filename: "a.pdf", reason: "not_an_image: x").contains("a.pdf"))
        XCTAssertTrue(ComposeInlineImages.cannotInsertNote(filename: "b.png", reason: "too_large: x").contains("10 MB"))
    }
}

final class InlineImageCoreRoundTripTests: XCTestCase {
    private func makeCore() throws -> (CastellumCore, Int64, URL) {
        let dir = TestScratch.root.appendingPathComponent("castellum-inline-img-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let draftId = try core.saveDraft(NewDraft(accountId: "acct", to: []))
        return (core, draftId, dir)
    }

    func testEmbedRenderAndReadBackOverTheCAbi() throws {
        let (core, draftId, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let result = try core.embedDraftImage(
            draftId: draftId, image: NewInlineImage(filename: "Screen Shot.png", alt: nil, dataBase64: TestPNG.data.base64EncodedString()))
        XCTAssertTrue(result.attachment.isInline)
        let cid = try XCTUnwrap(result.attachment.contentId)
        XCTAssertTrue(cid.hasPrefix("img-") && cid.hasSuffix("@castellum"))
        XCTAssertEqual(result.markdown, "![Screen Shot](cid:\(cid))")
        XCTAssertEqual(result.attachments.count, 1)

        let md = "Hi \(result.markdown)"
        let withDraft = core.renderMarkdown(md, theme: .plain, draftId: draftId)
        XCTAssertTrue(withDraft.contains("<img src=\"cid:\(cid)\""), withDraft)
        let stateless = core.renderMarkdown(md, theme: .plain)
        XCTAssertFalse(stateless.contains("<img"), "the stateless render still drops every image")
        XCTAssertEqual(core.renderMarkdown(md, theme: .plain, draftId: nil), stateless, "no draft id = the stateless render")

        let image = try XCTUnwrap(core.draftInlineImage(draftId: draftId, contentId: cid))
        XCTAssertEqual(image.mimeType, "image/png")
        XCTAssertEqual(image.data, TestPNG.data)
        XCTAssertNil(core.draftInlineImage(draftId: draftId, contentId: "img-000000000000@castellum"))
    }

    func testTheSameBytesTwiceGiveOneAttachment() throws {
        let (core, draftId, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let image = NewInlineImage(filename: "a.png", alt: nil, dataBase64: TestPNG.data.base64EncodedString())
        _ = try core.embedDraftImage(draftId: draftId, image: image)
        let second = try core.embedDraftImage(draftId: draftId, image: image)
        XCTAssertEqual(second.attachments.count, 1)
    }

    func testASetInlineRoundTripOverTheCAbi() throws {
        let (core, draftId, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let list = try core.addDraftAttachment(
            draftId: draftId,
            attachment: NewDraftAttachment(
                filename: "photo.png", contentType: "application/octet-stream", isInline: false, contentId: nil,
                dataBase64: TestPNG.data.base64EncodedString()))
        let id = list[0].id
        let made = try core.setDraftAttachmentInline(draftId: draftId, attachmentId: id, inline: true)
        XCTAssertTrue(made.attachments[0].isInline)
        let cid = try XCTUnwrap(made.attachments[0].contentId)
        XCTAssertEqual(made.markdown, "![photo](cid:\(cid))")
        XCTAssertEqual(made.attachments[0].contentType, "image/png")
        let back = try core.setDraftAttachmentInline(draftId: draftId, attachmentId: id, inline: false)
        XCTAssertFalse(back.attachments[0].isInline)
        XCTAssertNil(back.markdown)
        XCTAssertNil(back.attachments[0].contentId)
        XCTAssertThrowsError(try core.setDraftAttachmentInline(draftId: draftId, attachmentId: 9_999, inline: true))
    }

    func testARefusalCarriesTheCoreReasonAndWritesNothing() throws {
        let (core, draftId, dir) = try makeCore()
        defer { try? FileManager.default.removeItem(at: dir) }
        let svg = NewInlineImage(filename: "x.png", alt: nil, dataBase64: Data("<svg onload=alert(1)></svg>".utf8).base64EncodedString())
        XCTAssertThrowsError(try core.embedDraftImage(draftId: draftId, image: svg)) { error in
            XCTAssertTrue("\(error)".contains("not_an_image"), "\(error)")
        }
        XCTAssertTrue(try core.listDraftAttachments(draftId: draftId).isEmpty)
    }
}

@MainActor
final class InlineImageComposeSessionTests: XCTestCase {
    private var models: [AppModel] = []
    private var dirs: [URL] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root.appendingPathComponent("castellum-inline-session-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        dirs.append(dir)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        for dir in dirs { try? FileManager.default.removeItem(at: dir) }
        dirs = []
        try await super.tearDown()
    }

    private func waitUntil(_ cond: () -> Bool, timeout: TimeInterval = 8, _ what: String) async {
        let end = Date().addingTimeInterval(timeout)
        while !cond() && Date() < end { try? await Task.sleep(nanoseconds: 20_000_000) }
        if !cond() { XCTFail("timed out: \(what)") }
    }

    private func session(markdown: Bool) -> ComposeSession {
        let s = ComposeSession(accountId: "acct")
        s.markdownEnabled = markdown
        return s
    }

    func testPastedImageEmbedsInlineWhenMarkdownIsOn() async {
        let model = makeModel()
        let s = session(markdown: true)
        s.body = "Look: "
        s.attachPastedImage(data: TestPNG.data, contentType: "image/png", model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        XCTAssertTrue(s.attachments[0].isInline)
        XCTAssertNotNil(s.attachments[0].contentId)
        XCTAssertTrue(s.body.hasPrefix("Look: ![Pasted Image"), s.body)
        XCTAssertNil(s.attachmentError)
        XCTAssertTrue(s.hasUserEdits)
    }

    func testPastedImageStillAttachesWhenMarkdownIsOff() async {
        let model = makeModel()
        let s = session(markdown: false)
        s.attachPastedImage(data: TestPNG.data, contentType: "image/png", model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached")
        XCTAssertFalse(s.attachments[0].isInline)
        XCTAssertEqual(s.body, "", "nothing is written into a plain-text body")
    }

    func testNonImageFileAttachesNormallyWithMarkdownOn() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        let url = TestScratch.root.appendingPathComponent("inline-test-\(UUID().uuidString).pdf")
        try Data("%PDF-1.4".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        s.attach(fileURLs: [url], model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached")
        XCTAssertFalse(s.attachments[0].isInline)
        XCTAssertEqual(s.body, "")
    }

    func testAnImageFileEmbedsWithMarkdownOn() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        let url = TestScratch.root.appendingPathComponent("inline-photo-\(UUID().uuidString).png")
        try TestPNG.data.write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        s.attach(fileURLs: [url], model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        XCTAssertTrue(s.attachments[0].isInline)
        XCTAssertTrue(s.body.contains("![inline-photo-"), s.body)
    }

    func testAnImageTheCoreCannotEmbedFallsBackToAFileWithANote() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        let url = TestScratch.root.appendingPathComponent("not-really-\(UUID().uuidString).png")
        try Data("<svg onload=alert(1)></svg>".utf8).write(to: url)   // an SVG body under a .png name
        defer { try? FileManager.default.removeItem(at: url) }
        s.attach(fileURLs: [url], model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached as a file")
        XCTAssertFalse(s.attachments[0].isInline)
        XCTAssertEqual(s.body, "", "no reference for a file that was not embedded")
        XCTAssertTrue(s.attachmentError?.contains("attached as a file") == true, s.attachmentError ?? "nil")
    }

    func testOversizeImageFallsBackToARegularAttachmentWithMessage() async {
        let model = makeModel()
        let s = session(markdown: true)
        var big = TestPNG.data
        big.append(Data(count: ComposeInlineImages.maxBytes))   // a PNG signature, then over 10 MB
        s.embedImage(data: big, filename: "huge.png", model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached as a file")
        XCTAssertFalse(s.attachments[0].isInline)
        XCTAssertTrue(s.attachmentError?.contains("10 MB") == true, s.attachmentError ?? "nil")
        XCTAssertEqual(s.body, "")
    }

    func testRemovingTheInlineAttachmentRemovesItsReference() async {
        let model = makeModel()
        let s = session(markdown: true)
        s.body = "Intro\n\nOutro"
        s.embedImage(data: TestPNG.data, filename: "one.png", model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        let id = s.attachments[0].id
        s.removeAttachment(id: id, model: model)
        await waitUntil({ s.attachments.isEmpty }, "removed")
        XCTAssertFalse(s.body.contains("cid:"), s.body)
        XCTAssertTrue(s.body.contains("Intro") && s.body.contains("Outro"))
    }

    func testInsertInBodyTurnsAnAttachmentInline() async throws {
        let model = makeModel()
        let s = session(markdown: false)   // attach as a plain file first (Markdown off)
        s.body = "Hello"
        s.attachPastedImage(data: TestPNG.data, contentType: "image/png", model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached")
        XCTAssertFalse(s.attachments[0].isInline)
        s.markdownEnabled = true
        s.insertAttachmentInBody(id: s.attachments[0].id, model: model)
        await waitUntil({ s.attachments.first?.isInline == true && s.body.contains("](cid:img-") }, "inlined")
        XCTAssertTrue(s.body.hasPrefix("Hello"), s.body)
        XCTAssertNil(s.attachmentError)
    }

    func testInsertInBodyOnANonImageKeepsItRegularWithANote() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        let url = TestScratch.root.appendingPathComponent("doc-\(UUID().uuidString).pdf")
        try Data("%PDF-1.4".utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        s.attach(fileURLs: [url], model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached")
        s.insertAttachmentInBody(id: s.attachments[0].id, model: model)
        await waitUntil({ s.attachmentError != nil }, "refusal note")
        XCTAssertFalse(s.attachments[0].isInline)
        XCTAssertEqual(s.body, "")
    }

    func testKeepAsAttachmentOnlyRemovesTheReference() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        s.body = "Top\n\nBottom"
        s.embedImage(data: TestPNG.data, filename: "one.png", model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        s.keepAttachmentOnly(id: s.attachments[0].id, model: model)
        await waitUntil({ s.attachments.first?.isInline == false }, "kept as attachment")
        XCTAssertEqual(s.attachments.count, 1, "the file is still attached")
        XCTAssertFalse(s.body.contains("cid:"), s.body)
        XCTAssertTrue(s.body.contains("Top") && s.body.contains("Bottom"))
    }

    func testATiffFileIsTranscodedBeforeEmbedding() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 4, pixelsHigh: 4, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        let url = TestScratch.root.appendingPathComponent("scan-\(UUID().uuidString).tiff")
        try rep.representation(using: .tiff, properties: [:])!.write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        s.attach(fileURLs: [url], model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        XCTAssertTrue(s.attachments[0].isInline)
        XCTAssertEqual(s.attachments[0].contentType, "image/png", "re-encoded as PNG")
        XCTAssertTrue(s.attachments[0].filename.hasSuffix(".png"))
    }

    func testRemovingARegularAttachmentLeavesTheBodyAlone() async {
        let model = makeModel()
        let s = session(markdown: false)
        s.body = "text with ![x](cid:img-000000000000@castellum) kept"
        s.attachPastedImage(data: TestPNG.data, contentType: "image/png", model: model)
        await waitUntil({ s.attachments.count == 1 }, "attached")
        let before = s.body
        s.removeAttachment(id: s.attachments[0].id, model: model)
        await waitUntil({ s.attachments.isEmpty }, "removed")
        XCTAssertEqual(s.body, before)
    }

    func testDraftAwareRenderShowsTheImageThroughTheSessionRenderer() async {
        let model = makeModel()
        let s = session(markdown: true)
        s.embedImage(data: TestPNG.data, filename: "p.png", model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") }, "embedded")
        // The renderer was built BEFORE the draft existed in real use; here it is built after, and
        // reads draftId when it runs either way.
        let html = s.markdownRenderer(core: model.core)(s.body) ?? ""
        XCTAssertTrue(html.contains("<img src=\"cid:img-"), html)
        let stateless = model.core.renderMarkdown(s.body, theme: s.markdownTheme)
        XCTAssertFalse(stateless.contains("<img"))
    }

    func testThePreviewSourceServesTheStoredBytes() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        XCTAssertNil(s.previewImageSource(core: model.core), "no draft, no images, no source")
        s.embedImage(data: TestPNG.data, filename: "p.png", model: model)
        await waitUntil({ s.attachments.count == 1 }, "embedded")
        let source = try XCTUnwrap(s.previewImageSource(core: model.core))
        let part = try XCTUnwrap(InlineImagePolicy.part(
            forRequest: URL(string: "cid:\(s.attachments[0].contentId!)")!, in: source.attachments))
        XCTAssertEqual(source.load(part.ordinal), TestPNG.data)
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: try XCTUnwrap(source.load(part.ordinal))), "image/png")
    }

    func testAReopenedDraftKeepsItsImageAndItsMarkdown() async throws {
        let model = makeModel()
        let s = session(markdown: true)
        s.markdownTheme = .clean
        s.embedImage(data: TestPNG.data, filename: "p.png", model: model)
        await waitUntil({ s.attachments.count == 1 && s.body.contains("](cid:img-") && s.draftId != nil }, "embedded")
        // Save what the window would autosave, then reopen it.
        let saved = s.buildDraftForSave(renderMarkdown: s.markdownRenderer(core: model.core))
        let id = try model.core.saveDraft(saved)
        let reopened = try XCTUnwrap(model.core.listDrafts(accountId: "acct").first { $0.id == id })
        let t = ComposeSession(draft: reopened)
        XCTAssertTrue(t.markdownEnabled)
        XCTAssertEqual(t.markdownTheme, .clean)
        XCTAssertTrue(t.body.contains("](cid:img-"))
        t.loadAttachments(model: model)
        await waitUntil({ t.attachments.count == 1 }, "reopened attachments")
        XCTAssertTrue(t.attachments[0].isInline)
        XCTAssertTrue((t.markdownRenderer(core: model.core)(t.body) ?? "").contains("<img src=\"cid:img-"))
    }
}
