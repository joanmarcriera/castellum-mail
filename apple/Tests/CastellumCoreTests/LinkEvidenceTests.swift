// Castellum - "why linked" evidence presentation helpers (pinned).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class LinkEvidenceTests: XCTestCase {
    func testManualExplanation() {
        XCTAssertEqual(
            linkEvidenceExplanation(origin: "manual", provenance: nil),
            "Linked manually."
        )
    }

    func testDerivedWithMessageEvidenceExplains() {
        let text = linkEvidenceExplanation(
            origin: "derived", provenance: "message:acc:INBOX:9")
        XCTAssertTrue(
            text.contains("participated in a message"),
            "derived-with-evidence must explain the connection: \(text)"
        )
    }

    func testDerivedLegacyParticipantAndBareDerived() {
        XCTAssertEqual(
            linkEvidenceExplanation(origin: "derived", provenance: "participant"),
            "Auto-linked as a message participant."
        )
        XCTAssertEqual(
            linkEvidenceExplanation(origin: "derived", provenance: nil),
            "Auto-linked."
        )
    }

    func testNamedSourceExplanation() {
        XCTAssertEqual(
            linkEvidenceExplanation(origin: "linkedin-import", provenance: nil),
            "Imported from linkedin-import."
        )
    }

    func testEvidenceMessageIdParsing() {
        XCTAssertEqual(
            evidenceMessageId(provenance: "message:acc:INBOX:9"),
            "acc:INBOX:9",
            "message ids keep their internal colons"
        )
        XCTAssertNil(evidenceMessageId(provenance: "participant"))
        XCTAssertNil(evidenceMessageId(provenance: "message:"), "empty id is nil")
        XCTAssertNil(evidenceMessageId(provenance: nil))
    }

    // MARK: - the "no evidencing message" note

    /// The exact case migration 0017 created: legacy auto-links stamped
    /// `participant` because the evidencing message id was never recorded.
    /// The popover must SAY that rather than silently dropping the button.
    func testLegacyParticipantLinkExplainsTheMissingMessage() {
        let note = linkEvidenceMissingMessageNote(origin: "derived", provenance: "participant")
        XCTAssertNotNil(note)
        XCTAssertTrue(
            note!.contains("before Castellum recorded evidence"),
            "must say the reason predates the feature, not that there is none: \(note!)"
        )
    }

    func testDerivedWithNoProvenanceStillGetsAnHonestNote() {
        XCTAssertEqual(
            linkEvidenceMissingMessageNote(origin: "derived", provenance: nil),
            "No evidencing message was recorded for this link."
        )
    }

    /// No note where the button IS shown — the popover must never say both
    /// "open the evidencing message" and "no message was recorded".
    func testNoNoteWhenTheEvidencingMessageExists() {
        XCTAssertNil(linkEvidenceMissingMessageNote(
            origin: "derived", provenance: "message:acc:INBOX:9"))
    }

    /// The DTO spelling must work too. The popover passes the LinkRecord
    /// ("derived") today, but matching only that would make a future rewiring
    /// to `link.origin` ("auto") return nil for every row — the note would
    /// disappear silently and all the other tests here would stay green.
    func testTheLegacyAutoSpellingIsAcceptedToo() {
        XCTAssertEqual(
            linkEvidenceMissingMessageNote(origin: "auto", provenance: "participant"),
            linkEvidenceMissingMessageNote(origin: "derived", provenance: "participant")
        )
        XCTAssertNil(linkEvidenceMissingMessageNote(
            origin: "auto", provenance: "message:acc:INBOX:9"))
    }

    /// A manual link has no evidencing message by definition and never
    /// promised one, so the note would be noise.
    func testNoNoteForManualOrImportedLinks() {
        XCTAssertNil(linkEvidenceMissingMessageNote(origin: "manual", provenance: nil))
        XCTAssertNil(linkEvidenceMissingMessageNote(
            origin: "linkedin-import", provenance: nil))
    }

    func testDateTextIsAbsoluteAndLocaleStable() {
        let text = linkEvidenceDateText(
            epoch: 1_788_000_000, locale: Locale(identifier: "en_GB"))
        XCTAssertEqual(text, "29 Aug 2026")
    }
}
