// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - inline (cid:) image rules (#3442): cid lookup, raster-only serving by magic
// bytes, and hiding referenced inline parts from the attachment strip.

import XCTest
@testable import Castellum

final class InlineImagePolicyTests: XCTestCase {
    private func part(_ ordinal: Int64, cid: String?, inline: Bool = true, name: String? = "logo.png",
                      type: String? = "image/png") -> AttachmentDescriptor {
        AttachmentDescriptor(
            ordinal: ordinal, filename: name, contentType: type, sizeBytes: 10, section: "2",
            contentId: cid, isInline: inline, transferEncoding: nil)
    }

    func testNormaliseStripsSchemeBracketsAndPercentEncoding() {
        XCTAssertEqual(InlineImagePolicy.normalise("cid:logo123"), "logo123")
        XCTAssertEqual(InlineImagePolicy.normalise("<logo123>"), "logo123")
        XCTAssertEqual(InlineImagePolicy.normalise("CID:a%40b.example"), "a@b.example")
        XCTAssertEqual(InlineImagePolicy.normalise("Logo"), "Logo", "case is preserved")
    }

    func testRequestResolvesToThePartWithThatContentId() {
        let parts = [part(1, cid: "<a@x>"), part(2, cid: "logo123")]
        XCTAssertEqual(InlineImagePolicy.part(forRequest: URL(string: "cid:logo123")!, in: parts)?.ordinal, 2)
        XCTAssertEqual(InlineImagePolicy.part(forRequest: URL(string: "cid:a@x")!, in: parts)?.ordinal, 1)
        XCTAssertNil(InlineImagePolicy.part(forRequest: URL(string: "cid:nope")!, in: parts))
        XCTAssertNil(InlineImagePolicy.part(forRequest: URL(string: "cid:LOGO123")!, in: parts), "case-sensitive")
    }

    func testOnlyRasterBytesAreServedWhateverThePartHeaderSays() {
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0])), "image/png")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: Data([0xFF, 0xD8, 0xFF, 0xE0])), "image/jpeg")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: Data("GIF89a....".utf8)), "image/gif")
        XCTAssertEqual(InlineImagePolicy.rasterMime(for: Data("RIFF\0\0\0\0WEBPVP8 ".utf8)), "image/webp")
        // SVG and HTML are refused even though a part header may call them images.
        XCTAssertNil(InlineImagePolicy.rasterMime(for: Data("<svg xmlns='http://www.w3.org/2000/svg'/>".utf8)))
        XCTAssertNil(InlineImagePolicy.rasterMime(for: Data("<html><script>x</script>".utf8)))
        XCTAssertNil(InlineImagePolicy.rasterMime(for: Data()))
    }

    func testReferencedInlinePartsAreHiddenUnreferencedAndNonInlineStay() {
        let html = #"<p>Hi</p><img src="cid:logo123"><img src='CID:other%40x'>"#
        XCTAssertTrue(InlineImagePolicy.isReferencedInline(part(1, cid: "logo123"), html: html))
        XCTAssertTrue(InlineImagePolicy.isReferencedInline(part(2, cid: "<other@x>"), html: html))
        XCTAssertFalse(InlineImagePolicy.isReferencedInline(part(3, cid: "unused"), html: html), "unreferenced stays listed")
        XCTAssertFalse(InlineImagePolicy.isReferencedInline(part(4, cid: "logo123", inline: false), html: html), "a real attachment stays")
        XCTAssertFalse(InlineImagePolicy.isReferencedInline(part(5, cid: "logo123"), html: nil), "no HTML body: nothing is hidden")
        XCTAssertFalse(InlineImagePolicy.isReferencedInline(part(6, cid: "logo"), html: html), "a prefix of another cid is not a match")
    }

    func testStripHidesReferencedInlineImagesOnly() {
        let header = MessageHeader(
            id: "m", accountId: "a", mailboxId: "a:INBOX", subject: nil, fromName: nil, fromAddress: nil,
            sentAt: nil, snippet: nil, hasAttachments: true, hasBody: true, isRead: true, isStarred: false,
            threadCount: 1)
        let detail = MessageDetail(
            header: header, rfc822MessageId: nil, addresses: [], textBody: nil,
            sanitizedHtml: #"<img src="cid:logo123">"#, hasRemoteContent: nil,
            attachments: [part(1, cid: "logo123"), part(2, cid: nil, inline: false, name: "report.pdf", type: "application/pdf")],
            trust: nil)
        XCTAssertEqual(detail.stripAttachments.map(\.filename), ["report.pdf"])
    }
}
