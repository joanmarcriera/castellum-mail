// Castellum - tracker report for blocked remote content.
// The analysis itself is Rust (unit-tested there); these cover the FFI
// round trip and the reader's wording.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class TrackerReportTests: XCTestCase {
    func testFFIReportsPixelsAndNamedServices() throws {
        let html = """
        <p>Hi</p>
        <img src="https://mailchi.list-manage.com/track/open.php?u=1" width="1" height="1">
        <img src="https://cdn.shop.example/hero.jpg" width="600" height="300">
        """
        let report = try CastellumCore.remoteContentReport(sanitizedHtml: html)
        XCTAssertEqual(report.remoteCount, 2)
        XCTAssertEqual(report.trackerCount, 1)
        XCTAssertEqual(report.pixelCount, 1)
        XCTAssertEqual(report.services.map(\.name), ["Mailchimp"])
        XCTAssertEqual(Set(report.hosts.map(\.host)), ["mailchi.list-manage.com", "cdn.shop.example"])
    }

    func testFFIOnMailWithNoRemoteContentIsEmpty() throws {
        let report = try CastellumCore.remoteContentReport(sanitizedHtml: "<p>plain</p>")
        XCTAssertEqual(report, .none)
    }

    func testLabelNamesTrackersAndAtMostThreeServices() {
        let report = RemoteContentReport(
            remoteCount: 9, trackerCount: 5, pixelCount: 2,
            services: ["A", "B", "C", "D"].map { .init(name: $0, count: 1) }, hosts: [])
        let label = TrackerReportPresentation.label(for: report)
        XCTAssertTrue(label.contains("5"), label)
        XCTAssertTrue(label.contains("A, B, C"), label)
        XCTAssertFalse(label.contains("D"), label)
        XCTAssertTrue(label.contains("…"), label)
    }

    func testSingularAndNoTrackerWording() {
        let one = RemoteContentReport(
            remoteCount: 1, trackerCount: 1, pixelCount: 1, services: [], hosts: [])
        XCTAssertTrue(TrackerReportPresentation.label(for: one).contains("1"))
        let none = RemoteContentReport(
            remoteCount: 3, trackerCount: 0, pixelCount: 0, services: [], hosts: [])
        XCTAssertEqual(
            TrackerReportPresentation.label(for: none), String(localized: "Remote images blocked"))
        XCTAssertEqual(
            TrackerReportPresentation.label(for: nil), String(localized: "Remote images blocked"))
    }

    func testDetailListsHosts() {
        let report = RemoteContentReport(
            remoteCount: 2, trackerCount: 0, pixelCount: 0, services: [],
            hosts: [.init(host: "a.example", count: 2)])
        XCTAssertEqual(TrackerReportPresentation.detail(for: report), "a.example × 2")
        XCTAssertNil(TrackerReportPresentation.detail(for: .none))
    }

    func testPathologicalInputReturnsQuicklyAndIsMarkedTruncated() throws {
        let hostile = "<div style=\"" + String(repeating: "url(", count: 250_000) + "\">x</div>"
        let start = Date()
        let report = try CastellumCore.remoteContentReport(sanitizedHtml: hostile)
        XCTAssertLessThan(Date().timeIntervalSince(start), 1.0)
        XCTAssertTrue(report.truncated)
        XCTAssertEqual(report.remoteCount, 0)
    }
}
