// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// Requires local GreenMail seeded (scripts/local-imap). Skips if unreachable so
/// CI (no docker) stays green. Run locally after seeding.
final class SyncIntegrationTests: XCTestCase {
    func testSyncAndReadFromGreenMail() throws {
        let host = "127.0.0.1"
        let port: UInt16 = 3143

        // Skip if GreenMail isn't listening on the plaintext IMAP port.
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = inet_addr(host)
        let reachable = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
        try XCTSkipUnless(reachable, "GreenMail not running on \(host):\(port)")

        let dir = TestScratch.root
            .appendingPathComponent("castellum-swift-gm-\(UUID().uuidString)")
        let core = try CastellumCore(profilePath: dir)
        let config = """
        {"account_id":"gm","email":"test@local.test","host":"\(host)","port":\(port),
         "username":"test@local.test","password":"secret","use_tls":false,
         "allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
        let report = try core.syncAccount(configJSON: config)
        let total = report.mailboxes.first?.total ?? 0
        // Skip (don't fail) if GreenMail is up but not seeded — otherwise this
        // false-fails whenever the container is running without a seed.
        try XCTSkipIf(total == 0, "GreenMail reachable but INBOX empty — seed via scripts/local-imap")

        // List with a generous limit; the page is capped at `limit`, so compare
        // against min(total, limit) — asserting == total false-fails when the
        // seed has more than one page of messages.
        let limit: Int64 = 2000
        let msgs = try core.listMessages(accountId: "gm", mailboxId: "gm:INBOX", limit: limit, offset: 0)
        XCTAssertEqual(Int64(msgs.count), min(total, limit))
        XCTAssertGreaterThan(try core.listThreads(accountId: "gm", limit: 500, offset: 0).count, 0)

        if let first = msgs.first {
            XCTAssertNotNil(try core.getMessage(id: first.id))
        }
    }
}
