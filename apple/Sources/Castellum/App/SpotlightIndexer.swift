// Castellum - CoreSpotlight indexing of message metadata (S58, opt-in).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Indexes ONLY the subject and sender name/address as searchable metadata —
// never text_body, sanitized_html, or attachment names/content. The one
// extraction point is `SpotlightMessageMetadata.init(header:)`, which reads
// from `MessageHeader` (never `MessageDetail`, which carries the body) and
// deliberately skips `MessageHeader.snippet` (a body preview living on that
// same struct). One Spotlight domain identifier per account (see
// `SpotlightDomain`) so turning the opt-in off, or removing an account, can
// scope-delete exactly that account's indexed items via
// `CSSearchableIndex.deleteSearchableItems(withDomainIdentifiers:)` and
// leave every other account's items untouched. This is a new category of
// on-device data exposure — subject/sender become visible to system-wide
// Spotlight and other apps' search, never leaving the Mac — hence OFF by
// default (see `Database::spotlight_indexing_enabled` in the Rust core) and
// documented in website/privacy.html.

import CoreSpotlight
import UniformTypeIdentifiers

// MARK: - Metadata extraction (the body-exclusion boundary)

/// The metadata extracted from one message for Spotlight indexing. This type
/// is the FIRST line of defence against body/HTML/attachment leakage: it has
/// no such field, so nothing but subject/sender can ever reach the indexer —
/// there is no field to forget to exclude.
struct SpotlightMessageMetadata: Equatable {
    let messageId: String
    let accountId: String
    let subject: String
    let senderName: String?
    let senderAddress: String

    /// Built from a `MessageHeader` — never a `MessageDetail` (which alone
    /// carries `textBody`/`sanitizedHtml`). Deliberately ignores
    /// `header.snippet` (a body preview) even though it sits on the same
    /// struct. `nil` when the header has no sender address to key on.
    init?(header: MessageHeader) {
        guard let fromAddress = header.fromAddress, !fromAddress.isEmpty else { return nil }
        messageId = header.id
        accountId = header.accountId
        subject = header.subject ?? ""
        senderName = header.fromName
        senderAddress = fromAddress
    }
}

// MARK: - Domain scoping

/// One Spotlight domain per account (S58): scopes
/// `deleteSearchableItems(withDomainIdentifiers:)` to exactly that account
/// when the opt-in turns off, or the account is removed, leaving every
/// other account's indexed items intact.
enum SpotlightDomain {
    static func identifier(accountId: String) -> String {
        "uk.co.riera.castellum.mail.\(accountId)"
    }
}

// MARK: - Attribute building (the audited field surface)

/// The CLOSED set of `CSSearchableItemAttributeSet` fields
/// `SpotlightAttributeBuilder` is allowed to write. Nothing outside this
/// enum may ever be set — `SpotlightAttributeBuilderTests` asserts
/// `fieldsWritten(for:)`'s keys are exactly `allCases`, so a future edit
/// that adds a field (e.g. `textContent`, a real body-length-capable
/// property on the real type) without extending this enum fails to compile,
/// not merely fails a runtime check.
enum SpotlightIndexedField: String, CaseIterable {
    case title
    case contentDescription
    case keywords
    case emailAddresses
}

enum SpotlightAttributeBuilder {
    /// The real attribute set indexed into Spotlight — `.emailMessage`
    /// content type, title/description/keywords/emailAddresses only.
    /// Deliberately never touches `.textContent` (the real, documented
    /// full-text-search field CoreSpotlight offers): a message's body must
    /// never be searchable metadata under this opt-in.
    static func attributeSet(for metadata: SpotlightMessageMetadata) -> CSSearchableItemAttributeSet {
        let set = CSSearchableItemAttributeSet(contentType: .emailMessage)
        let fields = derive(from: metadata)
        set.title = fields.title
        set.contentDescription = fields.contentDescription
        set.keywords = fields.keywords
        set.emailAddresses = fields.emailAddresses
        return set
    }

    /// One `CSSearchableItem`, domain-scoped to `metadata.accountId` (see
    /// `SpotlightDomain`), keyed by message id within that domain — indexing
    /// the same message twice (incremental re-run, then a backfill) simply
    /// overwrites the same item.
    static func searchableItem(for metadata: SpotlightMessageMetadata) -> CSSearchableItem {
        CSSearchableItem(
            uniqueIdentifier: metadata.messageId,
            domainIdentifier: SpotlightDomain.identifier(accountId: metadata.accountId),
            attributeSet: attributeSet(for: metadata)
        )
    }

    /// Audit surface for `SpotlightAttributeBuilderTests`: every field this
    /// builder writes, as the EXACT value that ends up in the real attribute
    /// set — derived from the same `derive(from:)` call `attributeSet(for:)`
    /// uses, so the audited dict and the live object can never drift apart.
    static func fieldsWritten(for metadata: SpotlightMessageMetadata) -> [SpotlightIndexedField: String] {
        let fields = derive(from: metadata)
        return [
            .title: fields.title,
            .contentDescription: fields.contentDescription,
            .keywords: fields.keywords.joined(separator: ", "),
            .emailAddresses: fields.emailAddresses.joined(separator: ", "),
        ]
    }

    private struct DerivedFields {
        let title: String
        let contentDescription: String
        let keywords: [String]
        let emailAddresses: [String]
    }

    /// The single derivation every written field comes from — subject and
    /// sender only, nothing else of `metadata` exists to draw from.
    private static func derive(from metadata: SpotlightMessageMetadata) -> DerivedFields {
        let title = metadata.subject.isEmpty ? "(No subject)" : metadata.subject
        let senderLabel: String
        if let name = metadata.senderName, !name.isEmpty {
            senderLabel = "\(name) <\(metadata.senderAddress)>"
        } else {
            senderLabel = metadata.senderAddress
        }
        var keywords = [metadata.senderAddress]
        if let name = metadata.senderName, !name.isEmpty { keywords.append(name) }
        return DerivedFields(
            title: title,
            contentDescription: "From: \(senderLabel)",
            keywords: keywords,
            emailAddresses: [metadata.senderAddress]
        )
    }
}

// MARK: - Indexing seam (real CSSearchableIndex, or a test double)

/// Narrow protocol over the two `CSSearchableIndex` entry points this
/// feature needs, matching their real signatures exactly (a retroactive
/// conformance below, not a wrapper) — so tests can inject a recording
/// double instead of touching the real system index. `isAvailable` mirrors
/// `CSSearchableIndex.isIndexingAvailable()` (real system state — e.g. an
/// ad-hoc/unsigned bundle under `xcodebuild test` reports unavailable) but
/// is a per-instance protocol requirement rather than a hard-coded call to
/// the real static method, precisely so a test double can report `true` and
/// exercise the indexing/removal logic without needing a properly signed
/// bundle.
protocol SpotlightIndexing {
    var isAvailable: Bool { get }
    func indexSearchableItems(_ items: [CSSearchableItem], completionHandler: (@Sendable (Error?) -> Void)?)
    func deleteSearchableItems(
        withDomainIdentifiers identifiers: [String], completionHandler: (@Sendable (Error?) -> Void)?)
    func deleteSearchableItems(
        withIdentifiers identifiers: [String], completionHandler: (@Sendable (Error?) -> Void)?)
}

extension CSSearchableIndex: SpotlightIndexing {
    var isAvailable: Bool { CSSearchableIndex.isIndexingAvailable() }
}

/// Drives CoreSpotlight indexing for the S58 opt-in: incremental indexing on
/// ingest, a one-off backfill when the opt-in turns on, and domain-scoped
/// removal when it turns off or an account is removed. `@unchecked Sendable`
/// exactly like `CastellumCore` — its own state (`index`, `batchSize`) is
/// immutable, and every call is safe to make from an arbitrary thread.
final class SpotlightIndexer: @unchecked Sendable {
    private let index: SpotlightIndexing
    /// Messages per `indexSearchableItems` call. Bounded so one call's
    /// payload stays small and `Task.yield()` between batches gives sync/UI
    /// work sharing the cooperative thread pool a chance to run — indexing
    /// must never be perceived as blocking either.
    private let batchSize: Int

    init(index: SpotlightIndexing = CSSearchableIndex.default(), batchSize: Int = 50) {
        self.index = index
        self.batchSize = batchSize
    }

    /// Index `metadatas` in bounded batches, yielding between them. Call
    /// from a background `Task`/`Task.detached` — never from @MainActor code
    /// (see `AppModel.setSpotlightIndexingEnabled` / `sync(config:)`).
    /// Errors are deliberately swallowed: Spotlight indexing is a
    /// best-effort on-device convenience and must never surface a
    /// user-facing failure or abort a sync/backfill in progress.
    func index(_ metadatas: [SpotlightMessageMetadata]) async {
        guard index.isAvailable, !metadatas.isEmpty else { return }
        for chunk in metadatas.chunked(into: batchSize) {
            let items = chunk.map(SpotlightAttributeBuilder.searchableItem(for:))
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                index.indexSearchableItems(items) { _ in continuation.resume() }
            }
            await Task.yield()
        }
    }

    /// Domain-scoped removal for one account (toggle-off, or account
    /// removal): deletes ONLY `SpotlightDomain.identifier(accountId:)`,
    /// never the app-wide index, so every other account's items survive.
    func removeAll(forAccount accountId: String) async {
        guard index.isAvailable else { return }
        let domain = SpotlightDomain.identifier(accountId: accountId)
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            index.deleteSearchableItems(withDomainIdentifiers: [domain]) { _ in continuation.resume() }
        }
    }

    /// Delete individual items by message id (a Spotlight result that opened to
    /// nothing, #3382). Not domain-scoped: ids are unique across accounts.
    /// Best effort and a no-op when indexing is unavailable.
    func remove(ids: [String]) async {
        guard index.isAvailable, !ids.isEmpty else { return }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            index.deleteSearchableItems(withIdentifiers: ids) { _ in continuation.resume() }
        }
    }

    /// One-off backfill for every already-synced message across `accounts`,
    /// paginated per mailbox (`pageSize` at a time) so a large mailbox is
    /// never loaded wholesale into memory. Runs entirely off the main
    /// actor — it takes `core` as a parameter rather than reading
    /// `AppModel.core` so the caller can dispatch it from `Task.detached`
    /// without ever hopping onto @MainActor.
    func backfillAll(core: CastellumCore, accounts: [Account], pageSize: Int64 = 200) async {
        guard index.isAvailable else { return }
        for account in accounts {
            guard let mailboxes = try? core.listMailboxes(accountId: account.id) else { continue }
            for mailbox in mailboxes {
                var offset: Int64 = 0
                while true {
                    guard
                        let page = try? core.listMessages(
                            accountId: account.id, mailboxId: mailbox.id, limit: pageSize, offset: offset),
                        !page.isEmpty
                    else { break }
                    await index(page.compactMap(SpotlightMessageMetadata.init(header:)))
                    if page.count < Int(pageSize) { break }
                    offset += pageSize
                    await Task.yield()
                }
            }
        }
    }

    /// Incremental indexing on ingest (S58): after a sync, index the newest
    /// headers of every mailbox the sync actually fetched something into.
    /// Re-indexing an already-indexed message is a harmless overwrite (same
    /// `uniqueIdentifier`) — simpler and safer than diffing exactly which
    /// ids are new against what CoreSpotlight already holds.
    func indexRecentSync(core: CastellumCore, report: SyncReport) async {
        guard index.isAvailable else { return }
        for mailboxReport in report.mailboxes where mailboxReport.fetched > 0 {
            let mailboxId = "\(report.accountId):\(mailboxReport.path)"
            guard
                let headers = try? core.listMessages(
                    accountId: report.accountId, mailboxId: mailboxId,
                    limit: mailboxReport.fetched, offset: 0)
            else { continue }
            await index(headers.compactMap(SpotlightMessageMetadata.init(header:)))
        }
    }
}

extension Array {
    /// Splits into consecutive slices of at most `size` elements each —
    /// used to bound one `indexSearchableItems` call's payload.
    func chunked(into size: Int) -> [[Element]] {
        guard size > 0 else { return [self] }
        return stride(from: 0, to: count, by: size).map {
            Array(self[$0..<Swift.min($0 + size, count)])
        }
    }
}
