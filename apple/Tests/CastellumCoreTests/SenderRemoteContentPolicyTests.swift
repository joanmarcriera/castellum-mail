// Castellum - sender remote-content policy identity tests (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class SenderRemoteContentPolicyTests: XCTestCase {
    func testPolicyIdentityIncludesAccount() {
        let a = SenderRemoteContentPolicy(accountId: "acc1", fromAddress: "news@example.com", allowed: true, updatedAt: "t")
        let b = SenderRemoteContentPolicy(accountId: "acc2", fromAddress: "news@example.com", allowed: true, updatedAt: "t")
        XCTAssertNotEqual(a.id, b.id, "the same sender on two accounts must be reviewable/revocable independently")
    }

    func testAllowlistIsIgnoredWhenAuthenticationExplicitlyFailed() {
        let failed = TrustSignals(spf: "pass", dkim: "none", dmarc: "fail", authservId: "mx.example")
        XCTAssertFalse(TrustBadgeKind.senderAllowlistApplies(failed), "a forged From must not inherit the allowlist")
        let spfFail = TrustSignals(spf: "fail", dkim: nil, dmarc: nil, authservId: nil)
        XCTAssertFalse(TrustBadgeKind.senderAllowlistApplies(spfFail))
    }

    func testAllowlistStillAppliesToPassedOrUnverifiedMail() {
        XCTAssertTrue(TrustBadgeKind.senderAllowlistApplies(nil))
        XCTAssertTrue(TrustBadgeKind.senderAllowlistApplies(TrustSignals(spf: "none", dkim: "none", dmarc: "none", authservId: nil)))
        XCTAssertTrue(TrustBadgeKind.senderAllowlistApplies(TrustSignals(spf: "pass", dkim: "pass", dmarc: "pass", authservId: "mx")))
    }
}
