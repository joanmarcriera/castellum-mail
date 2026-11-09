// Castellum - link-hover reveal decision tests (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class LinkHoverPolicyTests: XCTestCase {
    func testNilHoverRevealsNothing() {
        XCTAssertNil(LinkHoverPolicy.reveal(for: nil))
    }

    func testEmptyHrefRevealsNothing() {
        let hover = LinkHoverInfo(href: "", text: "x", point: .zero)
        XCTAssertNil(LinkHoverPolicy.reveal(for: hover))
    }

    func testRealHrefProducesAnInspection() {
        let hover = LinkHoverInfo(href: "https://example.com", text: "example.com", point: .zero)
        let result = LinkHoverPolicy.reveal(for: hover)
        XCTAssertEqual(result?.1.displayHost, "example.com")
    }

    func testMismatchedTextIsFlagged() {
        let hover = LinkHoverInfo(href: "https://evil.example", text: "yourbank.com", point: .zero)
        let result = LinkHoverPolicy.reveal(for: hover)
        XCTAssertEqual(result?.1.textMismatch, true)
    }
}
