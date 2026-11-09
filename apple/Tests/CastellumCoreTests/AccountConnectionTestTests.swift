// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S62 "Test Connection": request building, report decoding, presentation,
/// and (when GreenMail is running) the real FFI round trip — including that a
/// test never creates an account.
final class AccountConnectionTestTests: XCTestCase {
    private func config(
        host: String = "imap.example.com", port: Int = 993,
        smtpHost: String? = nil, smtpPort: Int? = nil, smtpPassword: String? = nil
    ) -> AccountConfig {
        AccountConfig(
            accountId: "a@example.com", email: "a@example.com", host: host, port: port,
            username: "a@example.com", password: "pw \"quoted\"", useTls: true,
            allowInvalidCerts: nil, smtpHost: smtpHost, smtpPort: smtpPort,
            smtpUsername: nil, smtpPassword: smtpPassword, smtpUseTls: nil, displayName: nil)
    }

    private func object(_ json: String) throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])
    }

    func testRequestAppliesTheSameSMTPDefaultsSendingUses() throws {
        let dict = try object(config().connectionTestJSON())
        XCTAssertEqual(dict["host"] as? String, "imap.example.com")
        XCTAssertEqual(dict["port"] as? Int, 993)
        XCTAssertEqual(dict["password"] as? String, "pw \"quoted\"", "escaped, not mangled")
        let smtp = try XCTUnwrap(dict["smtp"] as? [String: Any])
        XCTAssertEqual(smtp["host"] as? String, "smtp.example.com")
        XCTAssertEqual(smtp["port"] as? Int, 587)
        XCTAssertEqual(smtp["username"] as? String, "a@example.com")
        XCTAssertEqual(smtp["password"] as? String, "pw \"quoted\"")
        XCTAssertEqual(smtp["use_tls"] as? Bool, true)
    }

    func testRequestUsesExplicitSMTPOverrides() throws {
        let smtp = try XCTUnwrap(try object(
            config(smtpHost: "mail.other.net", smtpPort: 465, smtpPassword: "smtp-pw").connectionTestJSON()
        )["smtp"] as? [String: Any])
        XCTAssertEqual(smtp["host"] as? String, "mail.other.net")
        XCTAssertEqual(smtp["port"] as? Int, 465)
        XCTAssertEqual(smtp["password"] as? String, "smtp-pw")
    }

    func testReportDecodesTheRustShape() throws {
        let json = """
        {"imap":{"ok":true,"error_kind":null,"summary":null},
         "smtp":{"ok":false,"error_kind":"auth","summary":"authentication failed"},
         "mailboxes":[{"path":"INBOX","name":"INBOX","role":"inbox","selectable":true,"sync_by_default":true}]}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let report = try decoder.decode(ConnectionTestReport.self, from: Data(json.utf8))
        XCTAssertTrue(report.imap.ok)
        XCTAssertEqual(report.smtp?.errorKind, "auth")
        XCTAssertEqual(report.mailboxes.first?.syncByDefault, true)
        XCTAssertFalse(ConnectionTestPresentation.allPassed(report))
    }

    private let ok = ConnectionLegResult(ok: true, errorKind: nil, summary: nil)
    private func failed(_ kind: String?) -> ConnectionLegResult {
        ConnectionLegResult(ok: false, errorKind: kind, summary: "x")
    }

    func testSuccessLinesCountOnlySelectableFolders() {
        let report = ConnectionTestReport(imap: ok, smtp: ok, mailboxes: [
            ConnectionTestMailbox(path: "INBOX", name: "INBOX", role: "inbox", selectable: true, syncByDefault: true),
            ConnectionTestMailbox(path: "[Gmail]", name: "[Gmail]", role: nil, selectable: false, syncByDefault: false),
        ])
        let lines = ConnectionTestPresentation.lines(for: report, config: config())
        XCTAssertEqual(lines.count, 2)
        XCTAssertTrue(lines.allSatisfy(\.ok))
        XCTAssertTrue(lines[0].text.contains("1 folder found"), lines[0].text)
        XCTAssertTrue(lines[1].text.contains("smtp.example.com:587"), lines[1].text)
        XCTAssertTrue(ConnectionTestPresentation.allPassed(report))
    }

    func testAuthFailureNamesTheLegAndSuggestsAnAppPassword() {
        let line = ConnectionTestPresentation.line(
            for: failed("auth"), protocolName: "IMAP", endpoint: "imap.gmail.com:993", successText: "")
        XCTAssertFalse(line.ok)
        XCTAssertTrue(line.text.contains("IMAP sign-in"))
        XCTAssertTrue(line.text.contains("app-specific password"))
    }

    func testEachKindGetsDistinctActionableWording() {
        let kinds = ["auth", "network", "tls", "protocol", "unsupported"]
        let texts = kinds.map {
            ConnectionTestPresentation.line(for: failed($0), protocolName: "SMTP",
                                            endpoint: "h:1", successText: "").text
        }
        XCTAssertEqual(Set(texts).count, kinds.count)
        XCTAssertTrue(texts[1].contains("check the server name and port"))
    }

    func testUnknownKindFailsClosedToNeutralWording() {
        let line = ConnectionTestPresentation.line(
            for: failed("some-future-kind"), protocolName: "IMAP", endpoint: "h:1", successText: "")
        XCTAssertEqual(line.text, "IMAP test against h:1 failed.")
    }

    // MARK: - GreenMail round trip (skips when GreenMail isn't running)

    private func greenMailReachable() -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = UInt16(3143).bigEndian
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        return withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

    private func greenMailConfig(password: String) -> AccountConfig {
        AccountConfig(
            accountId: "test@local.test", email: "test@local.test", host: "127.0.0.1", port: 3143,
            username: "test@local.test", password: password, useTls: false, allowInvalidCerts: false,
            smtpHost: "127.0.0.1", smtpPort: 3025, smtpUsername: nil, smtpPassword: nil,
            smtpUseTls: false, displayName: nil)
    }

    func testRealConnectionTestPassesAndSavesNothing() throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on 127.0.0.1:3143")
        let dir = TestScratch.root
            .appendingPathComponent("castellum-conntest-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let report = try core.testConnection(configJSON: greenMailConfig(password: "secret").connectionTestJSON())
        XCTAssertTrue(report.imap.ok)
        XCTAssertEqual(report.smtp?.ok, true)
        XCTAssertTrue(report.mailboxes.contains { $0.path == "INBOX" && $0.syncByDefault })
        XCTAssertEqual(try core.listAccounts().count, 0, "a connection test must not create an account")
    }

    func testRealWrongPasswordIsALegibleAuthFailure() throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on 127.0.0.1:3143")
        let dir = TestScratch.root
            .appendingPathComponent("castellum-conntest-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let config = greenMailConfig(password: "wrong")
        let report = try core.testConnection(configJSON: config.connectionTestJSON())
        XCTAssertFalse(report.imap.ok)
        XCTAssertEqual(report.imap.errorKind, "auth")
        let first = ConnectionTestPresentation.lines(for: report, config: config)[0]
        XCTAssertFalse(first.ok)
        XCTAssertTrue(first.text.contains("check the username and password"), first.text)
    }
}
