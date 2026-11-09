// Castellum - Tests for S44's mailto: URI parsing (pure, no FFI).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class UnsubscribeMailtoTests: XCTestCase {
    func testParsesAddressAndSubject() {
        let (address, subject) = UnsubscribeMailto.parse("mailto:unsub@example.com?subject=unsubscribe")
        XCTAssertEqual(address, "unsub@example.com")
        XCTAssertEqual(subject, "unsubscribe")
    }

    func testParsesAddressWithNoSubject() {
        let (address, subject) = UnsubscribeMailto.parse("mailto:unsub@example.com")
        XCTAssertEqual(address, "unsub@example.com")
        XCTAssertNil(subject)
    }

    func testMalformedUriNeverCrashes() {
        // Just proving the call completes without crashing; the exact
        // fallback value for unparseable input is not load-bearing.
        _ = UnsubscribeMailto.parse("not a uri at all")
    }

    /// #3050: LISTSERV-style lists read the command from body=.
    func testBodyParameterIsParsedAndPercentDecoded() {
        let m = UnsubscribeMailto.parseMessage("mailto:list@example.com?subject=unsub&body=UNSUBSCRIBE%20all")
        XCTAssertEqual(m.address, "list@example.com")
        XCTAssertEqual(m.subject, "unsub")
        XCTAssertEqual(m.body, "UNSUBSCRIBE all")
    }

    func testMissingOrEmptyBodyIsNil() {
        XCTAssertNil(UnsubscribeMailto.parseMessage("mailto:a@example.com").body)
        XCTAssertNil(UnsubscribeMailto.parseMessage("mailto:a@example.com?body=").body)
    }

    func testOversizedBodyIsCapped() {
        let long = String(repeating: "a", count: 5000)
        let m = UnsubscribeMailto.parseMessage("mailto:a@example.com?body=\(long)")
        XCTAssertEqual(m.body?.count, UnsubscribeMailto.maxBodyLength)
    }

    func testSubjectIsCappedAndControlCharactersStripped() {
        let long = String(repeating: "s", count: 5000)
        let m = UnsubscribeMailto.parseMessage("mailto:a@example.com?subject=\(long)")
        XCTAssertEqual(m.subject?.count, UnsubscribeMailto.maxSubjectLength)
        let c = UnsubscribeMailto.parseMessage("mailto:a@example.com?subject=a%0D%0ABcc:%20x%00%E2%80%AEz")
        XCTAssertEqual(c.subject, "aBcc: xz")
        XCTAssertNil(UnsubscribeMailto.parseMessage("mailto:a@example.com?subject=%0A%0D").subject)
    }
}
