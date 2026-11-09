// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

final class AccountSyncStatusTests: XCTestCase {
    private let target = AccountSyncTarget(configJSON: """
        {"account_id":"a1","email":"person@example.test","host":"127.0.0.1","port":3143,
         "username":"person@example.test","password":"secret","use_tls":false}
        """)!

    func testCoreErrorKindsMapToPresentationStates() {
        let expected: [(String, AccountSyncState)] = [
            ("auth", .authenticationFailed),
            ("tls", .tlsFailed),
            ("network", .unreachable),
            ("protocol", .protocolFailed),
            ("db", .databaseFailed),
            ("unsupported", .unsupported),
            ("internal", .internalFailed),
        ]

        for (kind, state) in expected {
            XCTAssertEqual(
                AccountSyncStatus.failure(
                    target: target,
                    errorKind: kind,
                    rawError: "provider detail"
                ).state,
                state,
                "core ErrorKind \(kind)"
            )
        }
    }

    func testNetworkWordingUsesStructuredKindAndEndpointNotRawText() {
        let first = AccountSyncStatus.failure(
            target: target,
            errorKind: "network",
            rawError: "connection refused"
        )
        let hostile = AccountSyncStatus.failure(
            target: target,
            errorKind: "network",
            rawError: "AUTHENTICATION FAILED SECRET-SENTINEL"
        )

        XCTAssertEqual(first.state, .unreachable)
        XCTAssertEqual(first.message, hostile.message)
        XCTAssertTrue(first.message.contains("127.0.0.1:3143"))
        XCTAssertFalse(hostile.message.contains("SECRET-SENTINEL"))
    }

    func testRawErrorIsRetainedOnlyAsDiagnosticDetail() {
        let status = AccountSyncStatus.failure(
            target: target,
            errorKind: "auth",
            rawError: "server diagnostic detail"
        )

        XCTAssertEqual(status.rawError, "server diagnostic detail")
        XCTAssertFalse(status.message.contains("server diagnostic detail"))
    }

    func testUnknownFutureKindGetsNeutralWording() {
        let status = AccountSyncStatus.failure(
            target: target,
            errorKind: "future-kind",
            rawError: "connection refused"
        )

        XCTAssertEqual(status.state, .internalFailed)
        XCTAssertEqual(status.message, "Castellum couldn’t sync person@example.test.")
    }

    // MARK: - Visible, copyable, retryable failures

    private let failureKinds = ["network", "auth", "tls", "protocol", "db", "unsupported", "internal"]

    func testCopyableTextNeverContainsRawServerTextOrPasswords() {
        for kind in failureKinds {
            let status = AccountSyncStatus.failure(
                target: target, errorKind: kind,
                rawError: "* BYE SERVER-IDENTIFIER-SENTINEL message body secret")
            XCTAssertTrue(status.copyableText.contains(status.message), kind)
            XCTAssertTrue(status.copyableText.contains(kind), "kind tag is included for support")
            XCTAssertFalse(status.copyableText.contains("SENTINEL"), kind)
            XCTAssertFalse(status.copyableText.contains("secret"), kind)
        }
    }

    /// F3: a first-ever connect has no diagnostics row, so the kind comes from
    /// the core's leading "<kind>: " token; unclassified text stays nil.
    func testErrorKindFromThrownReadsTheCoreKindToken() {
        XCTAssertEqual(AccountSyncStatus.errorKind(fromThrown: "auth: LOGIN failed. Invalid login"), "auth")
        XCTAssertEqual(AccountSyncStatus.errorKind(fromThrown: "network: Connection refused (os error 61)"), "network")
        XCTAssertEqual(AccountSyncStatus.errorKind(fromThrown: "tls: handshake"), "tls")
        XCTAssertNil(AccountSyncStatus.errorKind(fromThrown: "bad config json: expected value"))
        XCTAssertNil(AccountSyncStatus.errorKind(fromThrown: "no separator"))
        // End to end: an auth token yields the sign-in message, never "can't reach".
        let kind = AccountSyncStatus.errorKind(fromThrown: "auth: LOGIN failed")
        let status = AccountSyncStatus.failure(target: target, errorKind: kind, rawError: nil)
        XCTAssertEqual(status.state, .authenticationFailed)
    }

    /// FP30: the body-fetch error keeps the core's kind but never the raw text.
    func testBodyFetchErrorIsReadableAndNeverRaw() {
        let net = AccountSyncStatus.bodyFetchMessage(
            target: target, thrown: "network error: Connection refused (os error 61)")
        XCTAssertFalse(net.contains("os error"))
        XCTAssertFalse(net.contains("network error:"))
        XCTAssertTrue(net.contains("127.0.0.1:3143"), net)

        let auth = AccountSyncStatus.bodyFetchMessage(target: target, thrown: "auth error: LOGIN failed SENTINEL")
        XCTAssertFalse(auth.contains("SENTINEL"))
        XCTAssertTrue(auth.contains("person@example.test"), auth)

        for thrown in ["something odd: SENTINEL", "no separator SENTINEL", "unsupported error: SENTINEL"] {
            let neutral = AccountSyncStatus.bodyFetchMessage(target: target, thrown: thrown)
            XCTAssertFalse(neutral.contains("SENTINEL"), thrown)
            XCTAssertEqual(neutral, "Couldn’t download this message.")
        }
        XCTAssertEqual(AccountSyncStatus.bodyFetchMessage(target: nil, thrown: "network error: x"),
                       "Couldn’t download this message.")
        XCTAssertEqual(AccountSyncStatus.errorKind(fromThrown: "network error: x"), "network")
    }

    func testOnlyFailuresOfferARetry() {
        XCTAssertTrue(AccountSyncStatus.failure(target: target, errorKind: "network", rawError: nil).offersRetry)
        XCTAssertFalse(AccountSyncStatus.ok(target).offersRetry)
        XCTAssertFalse(AccountSyncStatus.syncing(target).offersRetry)
    }

    /// Every failure message has a Catalan and a Spanish translation (the
    /// English source strings are the catalogue keys, `%@` for the interpolation).
    func testFailureMessagesAreLocalisedInCatalanAndSpanish() throws {
        let keys = [
            "Can’t reach %@ — the connection failed.",
            "Sign-in failed for %@ — check the username or password.",
            "Secure connection to %@ failed — check the TLS settings.",
            "%@ returned an unexpected IMAP response.",
            "Castellum couldn’t update local mail for %@.",
            "%@ doesn’t support a required IMAP capability.",
            "Castellum couldn’t sync %@.",
            "Retry sync",
            "Copy details",
        ]
        let bundle = Bundle(for: AppModel.self)
        for lang in ["ca", "es"] {
            let path = try XCTUnwrap(bundle.path(forResource: lang, ofType: "lproj"), lang)
            let lproj = try XCTUnwrap(Bundle(path: path))
            for key in keys {
                let value = lproj.localizedString(forKey: key, value: "«missing»", table: nil)
                XCTAssertNotEqual(value, "«missing»", "\(lang) lacks: \(key)")
                XCTAssertNotEqual(value, key, "\(lang) is untranslated: \(key)")
            }
        }
    }

    // MARK: - SyncProgressPresentation

    func testProgressLabelShowsCurrentMailboxAgainstItsKnownTotal() {
        let progress = SyncProgress(
            accountId: "a1",
            mailboxesCompleted: 1,
            mailboxesTotal: 6,
            currentMailbox: SyncMailboxProgress(path: "INBOX", fetched: 1204, total: 12830),
            aggregateFetched: 1204,
            aggregateTotal: 12830,
            done: false
        )

        XCTAssertEqual(
            SyncProgressPresentation.label(for: progress),
            "INBOX: 1,204 / 12,830 · 1 of 6 folders"
        )
    }

    func testProgressLabelOmitsFolderCountForASingleMailbox() {
        let progress = SyncProgress(
            accountId: "a1",
            mailboxesCompleted: 0,
            mailboxesTotal: 1,
            currentMailbox: SyncMailboxProgress(path: "INBOX", fetched: 5, total: 20),
            aggregateFetched: 5,
            aggregateTotal: 20,
            done: false
        )

        XCTAssertEqual(SyncProgressPresentation.label(for: progress), "INBOX: 5 / 20")
    }

    func testProgressLabelFallsBackToAggregateBetweenMailboxes() {
        // No `currentMailbox` (between mailboxes) but earlier mailboxes made
        // progress — show the running aggregate rather than nothing.
        let progress = SyncProgress(
            accountId: "a1",
            mailboxesCompleted: 2,
            mailboxesTotal: 6,
            currentMailbox: nil,
            aggregateFetched: 3410,
            aggregateTotal: 5000,
            done: false
        )

        XCTAssertEqual(
            SyncProgressPresentation.label(for: progress),
            "3,410 messages so far · 2 of 6 folders"
        )
    }

    func testProgressLabelShowsStartingBeforeAnyMailboxHasATotal() {
        // Right after `begin()`, before the first SELECT reports EXISTS.
        let progress = SyncProgress(
            accountId: "a1",
            mailboxesCompleted: 0,
            mailboxesTotal: 3,
            currentMailbox: nil,
            aggregateFetched: 0,
            aggregateTotal: 0,
            done: false
        )

        XCTAssertEqual(
            SyncProgressPresentation.label(for: progress),
            "Starting… · 0 of 3 folders"
        )
    }
}
