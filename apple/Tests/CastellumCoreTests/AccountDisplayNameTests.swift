// Castellum - account display name in the sidebar and From.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Ada Lovelace Duocastella

import XCTest
@testable import Castellum

final class AccountDisplayNameTests: XCTestCase {
    private func account(_ name: String?) -> Account {
        Account(id: "a", email: "nuria@example.cat", displayName: name, provider: "imap")
    }

    // MARK: sidebar label

    func testSidebarShowsTheDisplayNameWhenSet() {
        XCTAssertEqual(account("Núria Pujol").sidebarLabel, "Núria Pujol")
    }

    func testSidebarFallsBackToTheAddressWhenNilEmptyOrBlank() {
        XCTAssertEqual(account(nil).sidebarLabel, "nuria@example.cat")
        XCTAssertEqual(account("").sidebarLabel, "nuria@example.cat")
        XCTAssertEqual(account("   ").sidebarLabel, "nuria@example.cat")
    }

    // MARK: From formatting

    func testFromIsBareAddressWithoutAName() {
        XCTAssertEqual(AddressFormatter.header(name: nil, email: "a@b.cat"), "a@b.cat")
        XCTAssertEqual(AddressFormatter.header(name: "  ", email: "a@b.cat"), "a@b.cat")
    }

    func testFromWithSimpleNameIsUnquoted() {
        XCTAssertEqual(AddressFormatter.header(name: "Ada Lovelace", email: "a@b.cat"),
                       "Ada Lovelace <a@b.cat>")
    }

    func testFromKeepsCatalanAccentsUnescaped() {
        XCTAssertEqual(AddressFormatter.header(name: "Núria Àngels Ràdio", email: "a@b.cat"),
                       "Núria Àngels Ràdio <a@b.cat>")
    }

    func testFromQuotesANameContainingAComma() {
        XCTAssertEqual(AddressFormatter.header(name: "Pujol, Núria", email: "a@b.cat"),
                       "\"Pujol, Núria\" <a@b.cat>")
    }

    func testFromEscapesQuotesAndBackslashes() {
        XCTAssertEqual(AddressFormatter.header(name: "Al \"Bo\" Ca", email: "a@b.cat"),
                       "\"Al \\\"Bo\\\" Ca\" <a@b.cat>")
        XCTAssertEqual(AddressFormatter.header(name: "A\\B", email: "a@b.cat"),
                       "\"A\\\\B\" <a@b.cat>")
    }

    func testAccountFromHeaderUsesTheNameWithAddressFallback() {
        XCTAssertEqual(account("Pujol, Núria").fromHeader, "\"Pujol, Núria\" <nuria@example.cat>")
        XCTAssertEqual(account(nil).fromHeader, "nuria@example.cat")
    }
}
