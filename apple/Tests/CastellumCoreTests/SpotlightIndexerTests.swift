// Castellum - CoreSpotlight indexing tests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import CoreSpotlight
import XCTest
@testable import Castellum

/// Records every call instead of touching the real system Spotlight index,
/// which needs a properly signed bundle identifier and is unavailable under
/// the `CODE_SIGNING_ALLOWED=NO` build `xcodebuild test` uses.
final class MockSpotlightIndex: SpotlightIndexing, @unchecked Sendable {
    private let lock = NSLock()
    private var _indexedBatches: [[CSSearchableItem]] = []
    private var _deletedDomainBatches: [[String]] = []
    private var _deletedIdentifierBatches: [[String]] = []
    private var _sawMainThreadCall = false

    /// Always available — unlike the real `CSSearchableIndex`, which reports
    /// unavailable for an unsigned `xcodebuild test` bundle and would
    /// otherwise make this double silently drop every call.
    var isAvailable: Bool { true }

    var indexedBatches: [[CSSearchableItem]] {
        lock.lock(); defer { lock.unlock() }
        return _indexedBatches
    }
    var allIndexedItems: [CSSearchableItem] { indexedBatches.flatMap { $0 } }
    var deletedDomainBatches: [[String]] {
        lock.lock(); defer { lock.unlock() }
        return _deletedDomainBatches
    }
    var deletedIdentifierBatches: [[String]] {
        lock.lock(); defer { lock.unlock() }
        return _deletedIdentifierBatches
    }
    /// True if ANY call landed on the main thread — used to prove indexing
    /// never runs where it could block sync or the UI.
    var sawMainThreadCall: Bool {
        lock.lock(); defer { lock.unlock() }
        return _sawMainThreadCall
    }

    func indexSearchableItems(_ items: [CSSearchableItem], completionHandler: ((Error?) -> Void)?) {
        lock.lock()
        _indexedBatches.append(items)
        if Thread.isMainThread { _sawMainThreadCall = true }
        lock.unlock()
        completionHandler?(nil)
    }

    func deleteSearchableItems(
        withIdentifiers identifiers: [String], completionHandler: ((Error?) -> Void)?
    ) {
        lock.lock()
        _deletedIdentifierBatches.append(identifiers)
        if Thread.isMainThread { _sawMainThreadCall = true }
        lock.unlock()
        completionHandler?(nil)
    }

    func deleteSearchableItems(
        withDomainIdentifiers identifiers: [String], completionHandler: ((Error?) -> Void)?
    ) {
        lock.lock()
        _deletedDomainBatches.append(identifiers)
        if Thread.isMainThread { _sawMainThreadCall = true }
        lock.unlock()
        completionHandler?(nil)
    }
}

private func header(
    id: String = "m1", accountId: String = "acc1", subject: String? = "Q3 numbers",
    fromName: String? = "Alice", fromAddress: String? = "alice@example.com",
    snippet: String? = nil
) -> MessageHeader {
    MessageHeader(
        id: id, accountId: accountId, mailboxId: "\(accountId):INBOX", subject: subject,
        fromName: fromName, fromAddress: fromAddress, sentAt: nil, snippet: snippet,
        hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
}

// MARK: - Metadata extraction (the body-exclusion boundary)

final class SpotlightMessageMetadataTests: XCTestCase {
    func testExtractsOnlySubjectAndSenderFromTheHeader() {
        let meta = SpotlightMessageMetadata(header: header())
        XCTAssertEqual(meta?.messageId, "m1")
        XCTAssertEqual(meta?.accountId, "acc1")
        XCTAssertEqual(meta?.subject, "Q3 numbers")
        XCTAssertEqual(meta?.senderName, "Alice")
        XCTAssertEqual(meta?.senderAddress, "alice@example.com")
    }

    func testNilWithNoSenderAddress() {
        XCTAssertNil(SpotlightMessageMetadata(header: header(fromAddress: nil)))
        XCTAssertNil(SpotlightMessageMetadata(header: header(fromAddress: "")))
    }

    /// `MessageHeader.snippet` is a body PREVIEW living on the same struct as
    /// subject/sender — the extraction point must never read it, even though
    /// it's right there. This is the test that would catch a future edit
    /// that "helpfully" folds `snippet` into the description/keywords.
    func testBodyPreviewSnippetNeverReachesTheExtractedMetadataOrAnyIndexedField() {
        let bodyLikeText = "CONFIDENTIAL: the Q3 numbers show a 40% shortfall against plan, see attached full breakdown for the board meeting on Thursday"
        let h = header(subject: "Re: numbers", snippet: bodyLikeText)
        guard let meta = SpotlightMessageMetadata(header: h) else {
            return XCTFail("expected metadata")
        }
        let written = SpotlightAttributeBuilder.fieldsWritten(for: meta)
        for (field, value) in written {
            XCTAssertFalse(
                value.contains(bodyLikeText),
                "\(field.rawValue) must never carry the body-preview snippet")
        }
    }
}

// MARK: - Domain scoping

final class SpotlightDomainTests: XCTestCase {
    func testIdentifierIsPerAccount() {
        let a = SpotlightDomain.identifier(accountId: "acc1")
        let b = SpotlightDomain.identifier(accountId: "acc2")
        XCTAssertNotEqual(a, b)
        XCTAssertTrue(a.contains("acc1"))
    }
}

// MARK: - Attribute building: the closed, audited field set

final class SpotlightAttributeBuilderTests: XCTestCase {
    private let metadata = SpotlightMessageMetadata(header: header())!

    func testWrittenFieldsAreExactlyTheClosedAllowlist() {
        let written = SpotlightAttributeBuilder.fieldsWritten(for: metadata)
        XCTAssertEqual(Set(written.keys), Set(SpotlightIndexedField.allCases),
                        "the builder must write EXACTLY the closed metadata-only field set — no more, no less")
    }

    func testFieldValuesAreDerivedOnlyFromSubjectAndSender() {
        let written = SpotlightAttributeBuilder.fieldsWritten(for: metadata)
        XCTAssertEqual(written[.title], "Q3 numbers")
        XCTAssertEqual(written[.contentDescription], "From: Alice <alice@example.com>")
        XCTAssertTrue(written[.keywords]?.contains("alice@example.com") == true)
        XCTAssertTrue(written[.keywords]?.contains("Alice") == true)
        XCTAssertEqual(written[.emailAddresses], "alice@example.com")
    }

    func testMissingSubjectFallsBackToAPlaceholderNeverAnEmptyOrNilTitle() {
        let meta = SpotlightMessageMetadata(header: header(subject: nil))!
        XCTAssertEqual(SpotlightAttributeBuilder.fieldsWritten(for: meta)[.title], "(No subject)")
    }

    /// The audited dict is derived from the SAME internal call the real
    /// `CSSearchableItemAttributeSet` is built from — cross-check a couple
    /// of the real object's fields directly too, and assert the real,
    /// documented full-text/body-capable property (`textContent`) is never
    /// touched: this test would fail if a future edit started setting it.
    func testRealAttributeSetMatchesTheAuditAndNeverSetsTextContent() {
        let set = SpotlightAttributeBuilder.attributeSet(for: metadata)
        XCTAssertEqual(set.title, "Q3 numbers")
        XCTAssertEqual(set.contentDescription, "From: Alice <alice@example.com>")
        XCTAssertEqual(set.emailAddresses, ["alice@example.com"])
        XCTAssertNil(set.textContent, "textContent is CoreSpotlight's full-text/body field — must stay untouched")
    }

    func testSearchableItemIsKeyedByMessageIdWithinTheAccountDomain() {
        let item = SpotlightAttributeBuilder.searchableItem(for: metadata)
        XCTAssertEqual(item.uniqueIdentifier, "m1")
        XCTAssertEqual(item.domainIdentifier, SpotlightDomain.identifier(accountId: "acc1"))
    }
}

// MARK: - SpotlightIndexer: batching, off-main-thread, domain-scoped delete

final class SpotlightIndexerTests: XCTestCase {
    private func metadatas(_ n: Int, accountId: String = "acc1") -> [SpotlightMessageMetadata] {
        (0..<n).map { i in
            SpotlightMessageMetadata(header: header(id: "m\(i)", accountId: accountId))!
        }
    }

    func testBatchesIndexingCallsBySize() async {
        let mock = MockSpotlightIndex()
        let indexer = SpotlightIndexer(index: mock, batchSize: 2)
        await indexer.index(metadatas(5))
        XCTAssertEqual(mock.indexedBatches.map(\.count), [2, 2, 1])
        XCTAssertEqual(mock.allIndexedItems.count, 5)
    }

    func testEmptyInputIndexesNothing() async {
        let mock = MockSpotlightIndex()
        await SpotlightIndexer(index: mock).index([])
        XCTAssertTrue(mock.indexedBatches.isEmpty)
    }

    /// Indexing must never be perceptible as blocking sync/UI — called from
    /// a background task, no call should land on the main thread.
    func testIndexingNeverRunsOnTheMainThread() async {
        let mock = MockSpotlightIndex()
        let indexer = SpotlightIndexer(index: mock, batchSize: 3)
        let items = metadatas(12)
        await Task.detached { await indexer.index(items) }.value
        XCTAssertFalse(mock.sawMainThreadCall)
    }

    /// Domain-scoped removal must request deletion of EXACTLY the target
    /// account's domain — never a wider/global delete, so other accounts'
    /// indexed items are provably untouched.
    func testRemoveAllIsScopedToExactlyOneAccountDomain() async {
        let mock = MockSpotlightIndex()
        let indexer = SpotlightIndexer(index: mock)
        await indexer.removeAll(forAccount: "acc1")
        XCTAssertEqual(mock.deletedDomainBatches, [[SpotlightDomain.identifier(accountId: "acc1")]])
    }
}

// MARK: - AppModel wiring (toggle default, no-account safety, account removal)

@MainActor
final class SpotlightAppModelWiringTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel(index: SpotlightIndexing) -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-spotlight-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir, spotlightIndex: index)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 10,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    func testDefaultsOffUntilReloaded() async throws {
        let model = makeModel(index: MockSpotlightIndex())
        XCTAssertFalse(model.spotlightIndexingEnabled)
        model.reloadSpotlightIndexingEnabled()
        try await Task.sleep(nanoseconds: 200_000_000) // let the detached read land
        XCTAssertFalse(model.spotlightIndexingEnabled, "a fresh profile has never opted in")
    }

    /// Flipping the toggle with no accounts yet must persist cleanly and
    /// never crash the backfill/removal loops — both simply iterate zero
    /// accounts. Also proves the setting survives via the same `core` the
    /// rest of the app reads.
    func testTogglingWithNoAccountsPersistsAndDoesNotCrash() async throws {
        let mock = MockSpotlightIndex()
        let model = makeModel(index: mock)

        model.setSpotlightIndexingEnabled(true)
        XCTAssertTrue(model.spotlightIndexingEnabled, "UI reflects the choice immediately")
        try await waitUntil({ (try? model.core.spotlightIndexingEnabled()) == true },
                             "the FFI write never landed")

        model.setSpotlightIndexingEnabled(false)
        try await waitUntil({ (try? model.core.spotlightIndexingEnabled()) == false },
                             "turning it back off never persisted")
        XCTAssertTrue(mock.indexedBatches.isEmpty, "no accounts existed — nothing to backfill")
    }
}

/// A real account synced against GreenMail with indexing already ON: proves
/// the incremental on-ingest path actually reaches the (mock) Spotlight
/// index with the right subject/sender/domain — not just that the plumbing
/// compiles. Skips — not fails — when GreenMail is unreachable.
@MainActor
final class SpotlightGreenMailAppModelTests: XCTestCase {
    private static let host = "127.0.0.1"
    private static let port: UInt16 = 3143
    private static let smtpPort: UInt16 = 3025

    private var models: [AppModel] = []

    private func makeModel(index: SpotlightIndexing) -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-spotlight-e2e-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir, spotlightIndex: index)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func greenMailReachable() -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Self.port.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.host)
        return withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

    private func config(accountId: String, email: String) -> String {
        """
        {"account_id":"\(accountId)","email":"\(email)","host":"\(Self.host)",
         "port":\(Self.port),"username":"\(email)","password":"\(email)",
         "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
    }

    private struct SMTPError: Error { let reason: String }

    /// Copied verbatim (see `SnoozeAppModelTests`) — GreenMail auto-creates
    /// an account only on SMTP delivery (CLAUDE.md gotcha 4), and this seeds
    /// the one message the test asserts got indexed.
    private func provision(to: String, subject: String) throws {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Self.smtpPort.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.host)
        let connected = withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
        guard connected else { throw SMTPError(reason: "connect GreenMail SMTP") }

        var pending: [UInt8] = []
        func readLine() throws -> String {
            while true {
                if let idx = pending.firstIndex(of: 0x0A) {
                    let line = String(decoding: pending[...idx], as: UTF8.self)
                    pending.removeSubrange(...idx)
                    return line
                }
                var chunk = [UInt8](repeating: 0, count: 512)
                let n = recv(sock, &chunk, chunk.count, 0)
                guard n > 0 else { throw SMTPError(reason: "SMTP connection closed early") }
                pending.append(contentsOf: chunk[0..<n])
            }
        }
        func expect(_ code: String) throws {
            while true {
                let line = try readLine()
                guard line.hasPrefix(code) else { throw SMTPError(reason: "expected \(code), got: \(line)") }
                let bytes = Array(line.utf8)
                if bytes.count > 3, bytes[3] == 0x20 { break }
            }
        }
        func send(_ s: String) throws {
            let bytes = Array(s.utf8)
            let sent = bytes.withUnsafeBufferPointer { write(sock, $0.baseAddress, $0.count) }
            guard sent == bytes.count else { throw SMTPError(reason: "short SMTP write") }
        }

        try expect("220")
        try send("HELO castellum-spotlight-e2e\r\n")
        try expect("250")
        try send("MAIL FROM:<bootstrap@local.test>\r\n")
        try expect("250")
        try send("RCPT TO:<\(to)>\r\n")
        try expect("250")
        try send("DATA\r\n")
        try expect("354")
        try send("From: bootstrap@local.test\r\nSubject: \(subject)\r\n\r\nbootstrap for \(to)\r\n\r\n.\r\n")
        try expect("250")
        try send("QUIT\r\n")
        try expect("221")
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 45,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    func testIncrementalIndexingOnFirstSyncReachesTheMockWithTheRightMetadata() async throws {
        try XCTSkipUnless(greenMailReachable(), "GreenMail not running on \(Self.host):\(Self.port)")
        let mock = MockSpotlightIndex()
        let model = makeModel(index: mock)
        let accountId = "spotlight-e2e-\(UUID().uuidString)"
        let email = "e2e-\(accountId)@local.test"
        let subject = "castellum spotlight e2e \(accountId)"

        try provision(to: email, subject: subject)
        model.setSpotlightIndexingEnabled(true)
        try await waitUntil({ (try? model.core.spotlightIndexingEnabled()) == true },
                             "the opt-in never persisted before sync")

        var addResult: AccountSyncStatus?
        model.addAccount(
            accountID: accountId, configJSON: config(accountId: accountId, email: email),
            completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "account never finished syncing")

        try await waitUntil({ !mock.allIndexedItems.isEmpty }, "the synced message was never indexed")
        let item = try XCTUnwrap(mock.allIndexedItems.first { $0.attributeSet.title == subject })
        XCTAssertEqual(item.domainIdentifier, SpotlightDomain.identifier(accountId: accountId))
        XCTAssertEqual(item.attributeSet.emailAddresses, ["bootstrap@local.test"])
        XCTAssertFalse(mock.sawMainThreadCall, "indexing must never run on the main thread")

        // Turning the opt-in off must scope-delete exactly this account's domain.
        model.setSpotlightIndexingEnabled(false)
        try await waitUntil({
            mock.deletedDomainBatches.contains([SpotlightDomain.identifier(accountId: accountId)])
        }, "toggle-off never removed the account's indexed items")
    }
}

// MARK: - Array chunking helper

final class ArrayChunkedTests: XCTestCase {
    func testChunksIntoBoundedSlices() {
        XCTAssertEqual([1, 2, 3, 4, 5].chunked(into: 2), [[1, 2], [3, 4], [5]])
        XCTAssertEqual([Int]().chunked(into: 2), [])
        XCTAssertEqual([1, 2, 3].chunked(into: 10), [[1, 2, 3]])
    }
}
