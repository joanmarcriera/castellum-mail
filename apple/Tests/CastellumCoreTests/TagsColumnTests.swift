// Castellum - message-list Tags column cell (S42).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import XCTest
@testable import Castellum

final class TagsColumnTests: XCTestCase {
    private let tags = [
        TagDefinition(keyword: "Work", keywordKey: "work", displayName: "Work", colour: "blue", sortOrder: 0),
        TagDefinition(keyword: "Later", keywordKey: "later", displayName: "Later", colour: "orange", sortOrder: 1),
    ]

    private func header(keywords: [String] = [], threadMuted: Bool = false) -> MessageHeader {
        MessageHeader(
            id: "m1", accountId: "acct", mailboxId: "mb", subject: "Hello", fromName: "Alice",
            fromAddress: "alice@example.com", sentAt: 0, snippet: nil, hasAttachments: false,
            hasBody: true, isRead: true, isStarred: false, threadCount: 1,
            keywords: keywords, threadMuted: threadMuted)
    }

    func testColourFallsBackToGreyForAnUndefinedKeyword() {
        XCTAssertEqual(TagsCell.colour(forKeyword: "Ghost", tags: tags), .grey)
    }

    func testColourMatchesCaseInsensitively() {
        XCTAssertEqual(TagsCell.colour(forKeyword: "WORK", tags: tags), .blue)
    }

    func testNamedFallsBackToGreyForAnUnknownStoredValue() {
        XCTAssertEqual(TagColour.named("neon-pink"), .grey)
    }

    func testAccessibilityLabelListsTagsThenMuted() {
        let h = header(keywords: ["Work", "Later"], threadMuted: true)
        XCTAssertEqual(TagsCell.accessibilityLabel(for: h, tags: tags), "Tags: Work, Later. Muted.")
    }

    func testAccessibilityLabelOmitsAbsentClauses() {
        XCTAssertEqual(TagsCell.accessibilityLabel(for: header(), tags: tags), "")
        XCTAssertEqual(TagsCell.accessibilityLabel(for: header(threadMuted: true), tags: tags), "Muted.")
        XCTAssertEqual(
            TagsCell.accessibilityLabel(for: header(keywords: ["Work"]), tags: tags), "Tags: Work.")
    }

    /// Max 4 dots then "+n" — the attributed string's plain-text content
    /// (not just its accessibility label) must reflect the overflow.
    func testAttributedStringCapsDotsAtFourAndShowsOverflowCount() {
        let h = header(keywords: ["a", "b", "c", "d", "e", "f"])
        let s = TagsCell.attributedString(for: h, tags: []).string
        XCTAssertEqual(s, "●●●●+2")
    }

    func testAttributedStringAppendsAMuteGlyphAfterTheDots() {
        let h = header(keywords: ["Work"], threadMuted: true)
        let s = TagsCell.attributedString(for: h, tags: tags)
        // The mute glyph is an NSTextAttachment, which renders as the
        // attachment-character placeholder — length is dot + space + glyph.
        XCTAssertEqual(s.length, 3)
    }

    // MARK: - the wiring (a pure-helper test cannot catch this)

    func testTheTagsColumnIsActuallyInTheMessageList() {
        let matches = kColumns.filter { $0.id == "tags" }
        XCTAssertEqual(matches.count, 1, "exactly one tags column")
        XCTAssertEqual(matches[0].title, "Tags")
    }

    // MARK: - #2245: dots lead the subject while the Tags column is hidden

    private let font = NSFont.systemFont(ofSize: 13)

    func testSubjectLeadsWithTheDotsWhenTheTagsColumnIsHidden() {
        let h = header(keywords: ["Work"])
        let s = SubjectCell.attributedString(subject: "Hello", header: h, tags: tags, font: font, tagsColumnVisible: false)
        XCTAssertEqual(s?.string, "\u{25CF} Hello")
        let colour = s?.attribute(.foregroundColor, at: 0, effectiveRange: nil) as? NSColor
        XCTAssertEqual(colour, TagColour.blue.nsColor, "the dot keeps the tag's colour")
    }

    func testSubjectStaysPlainWhenTheTagsColumnShowsTheDots() {
        let h = header(keywords: ["Work"], threadMuted: true)
        XCTAssertNil(SubjectCell.attributedString(subject: "Hello", header: h, tags: tags, font: font, tagsColumnVisible: true))
    }

    func testSubjectStaysPlainWithNothingToShow() {
        XCTAssertNil(SubjectCell.attributedString(subject: "Hello", header: header(), tags: tags, font: font, tagsColumnVisible: false))
    }

    func testMutedOnlyStillShowsTheGlyphBeforeTheSubject() {
        let s = SubjectCell.attributedString(
            subject: "Hello", header: header(threadMuted: true), tags: tags, font: font, tagsColumnVisible: false)
        XCTAssertNotNil(s?.attribute(.attachment, at: 0, effectiveRange: nil), "mute glyph first")
        XCTAssertTrue(s?.string.hasSuffix(" Hello") ?? false)
    }

    func testSubjectAccessibilityLabelNamesTheTagsOnlyWhenItShowsThem() {
        let h = header(keywords: ["Work"])
        XCTAssertEqual(SubjectCell.accessibilityLabel(header: h, tags: tags, tagsColumnVisible: false), "Subject. Tags: Work.")
        XCTAssertEqual(SubjectCell.accessibilityLabel(header: h, tags: tags, tagsColumnVisible: true), "Subject")
        XCTAssertEqual(SubjectCell.accessibilityLabel(header: header(), tags: tags, tagsColumnVisible: false), "Subject")
    }
}
