// Castellum - per-account write policy (evaluation mode) FFI wrappers.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AccountWritePolicyTests: XCTestCase {
    private func tempProfile() -> URL {
        TestScratch.root
            .appendingPathComponent("castellum-write-policy-\(UUID().uuidString)")
    }

    func testFailClosedAndPlumbing() throws {
        let core = try CastellumCore(profilePath: tempProfile())

        // Fail-closed: a new/unknown account is read-only (evaluation mode).
        // Real accounts are created by sync; the full toggle round-trip is
        // covered by the Rust db test and the GreenMail e2e.
        XCTAssertFalse(try core.accountWriteEnabled(accountId: "acc"))

        // Enabling writes on a nonexistent account surfaces as a thrown error,
        // never a silent no-op.
        XCTAssertThrowsError(try core.setAccountWriteEnabled(accountId: "acc", enabled: true))
    }

    /// Both read-flag entry points are linked and reachable over the C ABI,
    /// and both reject an unknown message id.
    ///
    /// This is a LINKAGE test, and only that: on an empty profile both calls
    /// throw for the same reason ("unknown message id"), so it would still
    /// pass if `markReadAuto` were rewired to `castellum_mark_read`. What
    /// actually distinguishes them — explicit refused in evaluation mode,
    /// auto local-only there but still enqueuing when writes are enabled — is
    /// pinned in Rust by `evaluation_mode_guards_the_enqueue_layer` and
    /// `auto_mark_read_still_enqueues_when_writes_are_enabled`, which can
    /// build the account and message rows this side cannot.
    func testBothReadFlagEntryPointsAreLinked() throws {
        let core = try CastellumCore(profilePath: tempProfile())
        XCTAssertThrowsError(try core.markRead(messageId: "nope", isRead: true))
        XCTAssertThrowsError(try core.markReadAuto(messageId: "nope", isRead: true))
    }
}
