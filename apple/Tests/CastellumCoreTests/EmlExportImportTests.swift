// Castellum - .eml filename/destination helper tests (S49).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class EmlExportImportTests: XCTestCase {
    func testSanitizedEmlFilenamePrefixesTheDateAndKeepsTheSubject() {
        let name = sanitizedEmlFilename(subject: "Quarterly report", sentAt: 1_735_689_600) // 2025-01-01T00:00:00Z
        XCTAssertTrue(name.hasSuffix("Quarterly report.eml"), "got \(name)")
        XCTAssertTrue(name.hasPrefix("2025-01-01") || name.hasPrefix("2024-12-31"), "got \(name)")
    }

    func testSanitizedEmlFilenameStripsPathSeparators() {
        let name = sanitizedEmlFilename(subject: "Re: Invoice/2026:final\\v2", sentAt: nil)
        XCTAssertFalse(name.contains("/"))
        XCTAssertFalse(name.contains(":"))
        XCTAssertFalse(name.contains("\\"))
        XCTAssertTrue(name.hasSuffix(".eml"))
    }

    func testSanitizedEmlFilenameFallsBackWhenSubjectIsEmptyOrNil() {
        XCTAssertEqual(sanitizedEmlFilename(subject: nil, sentAt: nil), "message.eml")
        XCTAssertEqual(sanitizedEmlFilename(subject: "   ", sentAt: nil), "message.eml")
    }

    func testSanitizedEmlFilenameNeverExceedsTheByteCap() {
        let longSubject = String(repeating: "a", count: 500)
        let name = sanitizedEmlFilename(subject: longSubject, sentAt: nil)
        XCTAssertLessThanOrEqual(name.utf8.count, 154, "150 cap + '.eml'")
    }

    func testUniqueDestinationAppendsANumberOnCollision() throws {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-eml-export-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let first = uniqueDestination(in: dir, filename: "message.eml")
        XCTAssertEqual(first.lastPathComponent, "message.eml")
        try Data().write(to: first)

        let second = uniqueDestination(in: dir, filename: "message.eml")
        XCTAssertEqual(second.lastPathComponent, "message 2.eml")
    }
}
