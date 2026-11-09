// Castellum - IMAP security mode in the stored account config.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `imap_security` ("tls" | "starttls" | "none") is additive in the Keychain
// JSON: old configs without it keep their `use_tls` meaning, an unknown value
// fails closed to implicit TLS (same table as Rust's
// `TransportSecurity::resolve`), and the connection test request carries it.

import XCTest
@testable import Castellum

final class IMAPSecurityTests: XCTestCase {
    private func config(useTls: Bool, imapSecurity: String? = nil) -> AccountConfig {
        var c = AccountConfig(
            accountId: "a", email: "a@example.com", host: "imap.example.com", port: 143,
            username: "a@example.com", password: "pw", useTls: useTls, allowInvalidCerts: false,
            smtpHost: nil, smtpPort: nil, smtpUsername: nil, smtpPassword: nil, smtpUseTls: nil,
            displayName: nil)
        c.imapSecurity = imapSecurity
        return c
    }

    func testResolutionTableMatchesTheCore() {
        XCTAssertEqual(config(useTls: true).effectiveIMAPSecurity, .tls)
        XCTAssertEqual(config(useTls: false).effectiveIMAPSecurity, .none)
        XCTAssertEqual(config(useTls: false, imapSecurity: "starttls").effectiveIMAPSecurity, .starttls)
        XCTAssertEqual(config(useTls: true, imapSecurity: "none").effectiveIMAPSecurity, .none)
        XCTAssertEqual(config(useTls: false, imapSecurity: "bogus").effectiveIMAPSecurity, .tls, "fail closed")
    }

    func testAnOldConfigWithoutTheKeyDecodesAndEncodesUnchanged() throws {
        let old = #"{"account_id":"a","email":"a@x","host":"h","port":993,"username":"u","password":"p","use_tls":true}"#
        let c = try AccountConfig.decode(fromJSON: old)
        XCTAssertNil(c.imapSecurity)
        XCTAssertEqual(c.effectiveIMAPSecurity, .tls)
        XCTAssertFalse(try c.encodedJSON().contains("imap_security"), "no key is written for an old config")
    }

    func testTheModeIsStoredUnderTheSnakeCaseKey() throws {
        let json = try config(useTls: true, imapSecurity: "starttls").encodedJSON()
        XCTAssertTrue(json.contains(#""imap_security":"starttls""#), json)
        XCTAssertEqual(try AccountConfig.decode(fromJSON: json).effectiveIMAPSecurity, .starttls)
    }

    func testTheConnectionTestRequestCarriesTheMode() throws {
        let json = try config(useTls: true, imapSecurity: "starttls").connectionTestJSON()
        let obj = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
        XCTAssertEqual(obj["imap_security"] as? String, "starttls")
    }

    func testDuplicatingAnAccountCopiesItsMode() {
        XCTAssertEqual(config(useTls: true, imapSecurity: "starttls").duplicateFields().imapSecurity, .starttls)
        XCTAssertEqual(config(useTls: false).duplicateFields().imapSecurity, .none)
    }

    func testEachModeHasItsUsualPort() {
        XCTAssertEqual(IMAPSecurity.tls.defaultPort, 993)
        XCTAssertEqual(IMAPSecurity.starttls.defaultPort, 143)
        XCTAssertEqual(IMAPSecurity.none.defaultPort, 143)
    }
}
