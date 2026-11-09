// Castellum - Codable DTOs mirroring the Rust core's JSON read APIs.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// JSON is snake_case (serde default); decode with `.convertFromSnakeCase`.

import Foundation

struct Account: Codable, Identifiable, Hashable {
    let id: String
    let email: String
    let displayName: String?
    let provider: String

    /// Sidebar/account label (S70): the display name when set, else the address.
    var sidebarLabel: String {
        let name = displayName?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return name.isEmpty ? email : name
    }

    /// `Name <addr>` for the compose From picker; bare address without a name.
    var fromHeader: String { AddressFormatter.header(name: displayName, email: email) }
}

struct Mailbox: Codable, Identifiable, Hashable {
    let id: String
    let accountId: String
    let path: String
    let name: String
    let role: String?
    let delimiter: String?
    let subscribed: Bool?
    let selectable: Bool?
    let syncByDefault: Bool?
}

struct Address: Codable, Hashable {
    let kind: String
    let name: String?
    let address: String
}

/// Incoming attachment metadata (no payload bytes — see AttachmentDownloadResult
/// for the on-demand download path).
struct AttachmentDescriptor: Codable, Identifiable, Hashable {
    var id: Int64 { ordinal }
    let ordinal: Int64
    let filename: String?
    let contentType: String?
    let sizeBytes: Int64?
    /// Dot-separated IMAP BODYSTRUCTURE section path. Empty for a
    /// pre-migration row that hasn't been re-hydrated.
    let section: String
    let contentId: String?
    let isInline: Bool
    /// Declared Content-Transfer-Encoding (informational for the UI: the core
    /// decodes the bytes itself before caching). Optional for decode
    /// resilience against older core payloads.
    let transferEncoding: String?
}

/// Decoded `castellum_attachment_cache_status`/`castellum_download_attachment`
/// result.
struct AttachmentDownloadResult: Codable, Hashable {
    /// "cached" | "not_cached" | "downloaded" | "failed".
    let status: String
    let localPath: String?
    let error: String?
}

/// Decoded `castellum_import_eml` result.
struct ImportEmlResult: Codable, Hashable {
    /// "queued" | "failed".
    let status: String
    let error: String?
}

/// Keyset paging cursor for `list_message_page`/`list_role_page` (S64):
/// opaque to Swift beyond round-tripping it back to the next call.
/// Encodes/decodes as `{"sent_at":…,"id":"…"}` via the shared snake_case
/// coder — matches `castellum_db::ListCursor`'s serde derive verbatim.
struct ListCursor: Codable, Equatable {
    let sentAt: Int64?
    let id: String
    /// Folded sort value of the last row for a From/Subject sort; absent for
    /// date order. Opaque to Swift: round-trip it, never build it.
    var key: String? = nil
}

/// What the message list is sorted by (#3043). Mirrors the core's `SortKey`
/// wire names, so `rawValue` is passed to `castellum_list_messages_page_sorted`.
enum MessageSortKey: String, Codable, CaseIterable, Sendable {
    case date, from, subject
    /// Grouping keys (slice 5): ascending puts unread / unflagged / attachment-less rows first.
    case unread, starred, attachments

    /// The `kColumns` identifier of the column whose header sorts by this key.
    var columnId: String {
        switch self {
        case .unread: return "unread"
        case .starred: return "star"
        case .attachments: return "attach"
        default: return rawValue
        }
    }

    init?(columnId: String) {
        guard let key = Self.allCases.first(where: { $0.columnId == columnId }) else { return nil }
        self = key
    }

    /// Grouping keys: a value of 0 or 1 per row, newest first inside each group.
    var isFlag: Bool { self == .unread || self == .starred || self == .attachments }
}

/// The list's sort: one column and a direction. `date` + `ascending == false`
/// is the original newest-first order. One global choice (spec decision M2),
/// persisted by `MessageSort.load`/`save`.
struct MessageSort: Equatable, Codable, Sendable {
    var key: MessageSortKey = .date
    var ascending = false

    static let `default` = MessageSort()

    /// The `kColumns` identifier of the column this sort is on.
    var columnId: String { key.columnId }

    /// `UserDefaults` key (JSON); a corrupt or unknown value falls back to the default.
    static let defaultsKey = "CastellumMessageList.sort"

    static func load(from defaults: UserDefaults) -> MessageSort {
        guard let data = defaults.data(forKey: defaultsKey),
              let sort = try? JSONDecoder().decode(MessageSort.self, from: data)
        else { return .default }
        return sort
    }

    func save(to defaults: UserDefaults) {
        if self == .default {
            defaults.removeObject(forKey: Self.defaultsKey)
        } else if let data = try? JSONEncoder().encode(self) {
            defaults.set(data, forKey: Self.defaultsKey)
        }
    }

    /// The direction a first click on `column` starts with: text columns A to Z,
    /// the date column newest first, Unread unread-first, Flag and Attachments
    /// flagged / with-attachment first.
    static func firstClick(on key: MessageSortKey) -> MessageSort {
        switch key {
        case .date, .starred, .attachments: return MessageSort(key: key, ascending: false)
        case .from, .subject, .unread: return MessageSort(key: key, ascending: true)
        }
    }

    /// The sort after clicking `key`'s header: the same column flips direction,
    /// another column starts at its natural direction.
    func toggled(on key: MessageSortKey) -> MessageSort {
        self.key == key ? MessageSort(key: key, ascending: !ascending) : Self.firstClick(on: key)
    }

    /// Short form for the UI harness and logs, e.g. "from:asc".
    var label: String { "\(key.rawValue):\(ascending ? "asc" : "desc")" }
}

/// One keyset page of message headers, as written by
/// `castellum_list_messages_page`/`castellum_list_messages_by_role_page`.
/// `next` is `nil` once the list is exhausted.
struct MessagePage: Decodable {
    let rows: [MessageHeader]
    let next: ListCursor?
}

struct MessageHeader: Codable, Identifiable, Hashable {
    let id: String
    let accountId: String
    let mailboxId: String
    let subject: String?
    let fromName: String?
    let fromAddress: String?
    let sentAt: Int64?
    let snippet: String?
    let hasAttachments: Bool
    let hasBody: Bool?
    /// Mutable so the UI can optimistically flip read state before replay.
    var isRead: Bool
    /// Mutable so the UI can optimistically flip starred/flagged state.
    var isStarred: Bool
    let threadCount: Int64
    /// Visible IMAP keywords (S42) — hidden system/protocol keywords are
    /// already filtered on the Rust side. Mutable for the tag picker's
    /// optimistic update. A header from an older core (missing the key)
    /// decodes to `[]`.
    var keywords: [String]
    /// Whether this message's conversation is muted (S42, local-only).
    /// Mutable for the mute command's optimistic update. Missing key
    /// decodes to `false`.
    var threadMuted: Bool
    /// Whether the sender is on the VIP list (S59). Missing key decodes to `false`.
    var fromVip: Bool
    /// DR1: recipients as display text for Drafts/Sent rows (`nil` elsewhere,
    /// where the list shows the sender). `""` = such a row with no recipient.
    var toDisplay: String?

    private enum CodingKeys: String, CodingKey {
        case id, accountId, mailboxId, subject, fromName, fromAddress, sentAt, snippet,
            hasAttachments, hasBody, isRead, isStarred, threadCount, keywords, threadMuted, fromVip, toDisplay
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        accountId = try c.decode(String.self, forKey: .accountId)
        mailboxId = try c.decode(String.self, forKey: .mailboxId)
        subject = try c.decodeIfPresent(String.self, forKey: .subject)
        fromName = try c.decodeIfPresent(String.self, forKey: .fromName)
        fromAddress = try c.decodeIfPresent(String.self, forKey: .fromAddress)
        sentAt = try c.decodeIfPresent(Int64.self, forKey: .sentAt)
        snippet = try c.decodeIfPresent(String.self, forKey: .snippet)
        hasAttachments = try c.decode(Bool.self, forKey: .hasAttachments)
        hasBody = try c.decodeIfPresent(Bool.self, forKey: .hasBody)
        isRead = try c.decode(Bool.self, forKey: .isRead)
        isStarred = try c.decode(Bool.self, forKey: .isStarred)
        threadCount = try c.decode(Int64.self, forKey: .threadCount)
        keywords = try c.decodeIfPresent([String].self, forKey: .keywords) ?? []
        threadMuted = try c.decodeIfPresent(Bool.self, forKey: .threadMuted) ?? false
        fromVip = try c.decodeIfPresent(Bool.self, forKey: .fromVip) ?? false
        toDisplay = try c.decodeIfPresent(String.self, forKey: .toDisplay)
    }

    /// Test/preview convenience — the FFI path always goes through
    /// `init(from:)` above.
    init(
        id: String, accountId: String, mailboxId: String, subject: String?, fromName: String?,
        fromAddress: String?, sentAt: Int64?, snippet: String?, hasAttachments: Bool,
        hasBody: Bool?, isRead: Bool, isStarred: Bool, threadCount: Int64,
        keywords: [String] = [], threadMuted: Bool = false, fromVip: Bool = false,
        toDisplay: String? = nil
    ) {
        self.id = id
        self.accountId = accountId
        self.mailboxId = mailboxId
        self.subject = subject
        self.fromName = fromName
        self.fromAddress = fromAddress
        self.sentAt = sentAt
        self.snippet = snippet
        self.hasAttachments = hasAttachments
        self.hasBody = hasBody
        self.isRead = isRead
        self.isStarred = isStarred
        self.threadCount = threadCount
        self.keywords = keywords
        self.threadMuted = threadMuted
        self.fromVip = fromVip
        self.toDisplay = toDisplay
    }
}

/// A VIP sender (S59) — mirrors the Rust `VipSender`. `address` is the
/// lower-cased, trimmed key; it doubles as the stable `id`.
struct VipSender: Codable, Hashable, Identifiable {
    let address: String
    let displayName: String?
    let createdAt: Int64

    var id: String { address }
}

/// One new-mail notice drained from the core's `notification_outbox` (S59) —
/// mirrors the Rust `PendingNotification`.
struct PendingNotification: Decodable, Equatable {
    let outboxId: Int64
    let accountId: String
    let messageId: String
    let mailboxId: String
    let threadId: String?
    let fromName: String?
    let fromAddress: String?
    let subject: String?
    let snippet: String?
    let sentAt: Int64?
    let fromVip: Bool
}

/// A local, profile-wide tag definition (S42) — mirrors the Rust
/// `TagDefinition`. `id` is `keywordKey` so `ForEach`/`Identifiable` needs no
/// extra wrapping (it never changes for a given tag, unlike `displayName`).
struct TagDefinition: Codable, Hashable, Identifiable {
    var keyword: String
    var keywordKey: String
    var displayName: String
    var colour: String
    var sortOrder: Int64
    var id: String { keywordKey }
}

/// Result of `CastellumCore.setMessageTag` — whether the change reached the
/// server, and if not, why (never an error: a tag is an organisational aid
/// first — plan Assumption 5).
struct TagApplyOutcome: Decodable {
    let synced: Bool
    let reason: String?
}

/// One distinct visible IMAP keyword in use with a message count (S42) —
/// Settings' "In use on your servers" adopt list.
struct KeywordInUse: Codable, Hashable, Identifiable {
    let keyword: String
    let keywordKey: String
    let count: Int64
    var id: String { keywordKey }
}

/// Message counts for one defined tag (S42) — the sidebar/menu badge.
struct TagCount: Codable, Hashable, Identifiable {
    let keywordKey: String
    let total: Int64
    let unread: Int64
    /// Messages whose tag never reached a server (MM-8); 0 when the core
    /// predates the field.
    let localOnly: Int64
    var id: String { keywordKey }

    init(keywordKey: String, total: Int64, unread: Int64, localOnly: Int64 = 0) {
        self.keywordKey = keywordKey
        self.total = total
        self.unread = unread
        self.localOnly = localOnly
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        keywordKey = try c.decode(String.self, forKey: .keywordKey)
        total = try c.decode(Int64.self, forKey: .total)
        unread = try c.decode(Int64.self, forKey: .unread)
        localOnly = try c.decodeIfPresent(Int64.self, forKey: .localOnly) ?? 0
    }
}

/// One "Find similar" (S53-UI) result: a candidate message plus its cosine
/// similarity to the anchor's embedding, in `[-1.0, 1.0]` (in practice
/// `[0, 1]` for the embedding models this app uses). `Identifiable` via the
/// nested header's id, so the sheet's `ForEach` needs no extra wrapping.
struct SimilarMessageResult: Codable, Identifiable, Hashable {
    let header: MessageHeader
    let similarity: Double
    var id: String { header.id }
}

/// A snoozed message: the ordinary header (flattened, matching the Rust
/// `#[serde(flatten)]`) plus its due time. `.convertFromSnakeCase` decodes
/// `snoozed_until_epoch` the same way every other field is decoded, so no
/// custom `init(from:)` is needed even with the flatten.
struct SnoozedMessageHeader: Codable, Identifiable, Hashable {
    let header: MessageHeader
    let snoozedUntilEpoch: Int64
    var id: String { header.id }

    private enum CodingKeys: String, CodingKey {
        case snoozedUntilEpoch
    }

    init(from decoder: Decoder) throws {
        header = try MessageHeader(from: decoder)
        let container = try decoder.container(keyedBy: CodingKeys.self)
        snoozedUntilEpoch = try container.decode(Int64.self, forKey: .snoozedUntilEpoch)
    }
}

/// One message that just became due (`CastellumCore.unsnoozeDue`).
struct UnsnoozedMessage: Codable, Hashable {
    let messageId: String
    let accountId: String
}

struct MailboxCounts: Codable, Identifiable, Hashable {
    var id: String { mailboxId }
    let mailboxId: String
    let total: Int64
    let unread: Int64
}

/// Per-mailbox body-hydration progress (#3631 G5; mirrors Rust
/// `MailboxBodyProgress`). Hydration is budgeted, so `hydrated < total` is
/// normal on a fresh large account; search only covers hydrated bodies.
struct BodyHydrationProgress: Codable, Identifiable, Hashable {
    var id: String { mailboxId }
    let mailboxId: String
    let total: Int64
    let hydrated: Int64
}

/// Body-hydration coverage of a search scope (#3633 G13; mirrors Rust
/// `BodyHydrationScope`): `missing` messages have no local body yet, so a
/// body match in them cannot be found.
struct BodyHydrationScope: Codable, Hashable {
    let total: Int64
    let missing: Int64
}

/// Pure presentation for body-hydration progress (unit-tested; no core).
enum BodyHydrationPresentation {
    /// The one-line note under search results when some bodies in the searched
    /// scope are not downloaded yet, or `nil` (nothing missing / no search).
    static func searchNote(_ scope: BodyHydrationScope?) -> String? {
        guard let scope, scope.missing > 0 else { return nil }
        let missing = scope.missing
        return String(localized: "Bodies of \(missing) messages are not downloaded yet; matches in them may be missing")
    }

    /// The sidebar tooltip for a mailbox, or `nil` when there is nothing to
    /// say (no progress known, an empty mailbox, or every body downloaded).
    static func tooltip(_ progress: BodyHydrationProgress?) -> String? {
        guard let p = progress, p.total > 0, p.hydrated < p.total else { return nil }
        let hydrated = max(0, p.hydrated)
        return String(localized: "Message bodies downloaded: \(hydrated) of \(p.total)")
    }
}

/// A saved S39 search-grammar query pinned in the sidebar (mirrors Rust
/// SmartMailbox). Counts are cached and refreshed after sync/create/rename —
/// never per keystroke; `countsCapped` means `cachedTotal`/`cachedUnread`
/// are a lower bound (hit the recomputation cap), not an exact count.
struct SmartMailbox: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    /// S39 text query; `""` for a group mailbox.
    let query: String
    /// MM-1: nested all/any/none conditions; nil for a text-query mailbox.
    let group: FilterGroupDTO?
    /// MM-1: account / mailbox restriction (a group mailbox only).
    let scope: SmartMailboxScopeDTO?
    let sortOrder: Int64
    let symbol: String?
    let cachedTotal: Int64
    let cachedUnread: Int64
    let countsCapped: Bool
    let countsUpdatedAt: Int64
    let createdAt: Int64
    let updatedAt: Int64
}

/// HP-2: one Split Inbox tab with its exact counts (Rust `InboxSplitCount`).
/// Every Inbox message is in exactly one tab; `id == "other"` is the
/// remainder tab, always last.
struct InboxSplitCount: Codable, Identifiable, Hashable {
    static let otherId = "other"
    let id: String
    let name: String
    let total: Int64
    let unread: Int64
    var isOther: Bool { id == Self.otherId }
    /// Tab title: a split's own name, or the localised "Other".
    var displayName: String { isOther ? String(localized: "Other") : name }
}

/// What an unforced reset would refuse on.
struct ResetAccountBlockers: Codable, Hashable {
    let unsentOutboxDrafts: Int64
    let pendingOperations: Int64
    var isEmpty: Bool { unsentOutboxDrafts == 0 && pendingOperations == 0 }
}

/// Result of editing an existing account's folder selection:
/// how many locally known mailboxes were removed (see
/// `CastellumCore.applyMailboxSelection`).
struct MailboxSelectionResult: Codable, Hashable {
    let removed: Int64
}

/// A folder `applyMailboxSelection` would refuse to remove unforced, because
/// a non-terminal operation still references it ( follow-up
/// review) — see `CastellumCore.applyMailboxSelectionBlockers`.
struct MailboxSelectionBlocker: Codable, Hashable, Identifiable {
    var id: String { path }
    let path: String
    let pendingOperations: Int64
}

/// A queued/replayable provider mutation (write-path operation queue).
struct OperationRecord: Codable, Identifiable, Hashable {
    let id: Int64
    let accountId: String
    let kind: String
    let payloadJson: String
    let status: String
    let attempts: Int64
    let lastError: String?
    /// `updated_at` as Unix epoch seconds (from the core; optional for decode
    /// resilience against older payloads).
    let updatedAtEpoch: Int64?
    /// Durable earliest-send time (Unix epoch seconds), undo-send window
    /// (S25). `nil` for every op except a delayed `send`.
    let notBeforeEpoch: Int64?
    /// `true` for a `send` the user scheduled with Send Later (HP1);
    /// `notBeforeEpoch` is then the scheduled time. Optional so older payloads
    /// and test fixtures without the key still decode.
    var scheduled: Bool? = nil

    /// A scheduled send (any status).
    var isScheduledSend: Bool { kind == "send" && scheduled == true }
}

/// Outcome of replaying queued operations to the provider.
struct ReplayReport: Codable, Hashable {
    let accountId: String
    let applied: Int
    let failed: Int
}

struct MessageDetail: Codable, Hashable {
    let header: MessageHeader
    let rfc822MessageId: String?
    let addresses: [Address]
    let textBody: String?
    let sanitizedHtml: String?
    /// True when sanitizedHtml references remote (http/https) sources.
    /// Optional for decode resilience against older core payloads.
    let hasRemoteContent: Bool?
    /// Optional for decode resilience against older core payloads (empty
    /// array, not missing, once this build's core is in use).
    let attachments: [AttachmentDescriptor]?
    /// SPF/DKIM/DMARC trust signals (S48). Optional for decode resilience
    /// against older core payloads (a message synced before S48 has no
    /// `message_trust` row; the core already defaults that to all-"none",
    /// but an OLDER core binary won't emit the key at all).
    let trust: TrustSignals?
    /// RFC 2369 mailto: unsubscribe target, if any.
    let listUnsubscribeMailto: String?
    /// RFC 2369 https: unsubscribe target, if any.
    let listUnsubscribeUrl: String?
    /// True only when an https: target exists AND the message opts into
    /// RFC 8058 one-click.
    let listUnsubscribeOneClick: Bool
    /// The thread this message belongs to (S54), so the reading pane can key
    /// the extraction panel by thread rather than by this one message.
    /// Optional for decode resilience against older core payloads.
    let threadId: String?

    /// Explicit memberwise init with defaults for the 3 List-Unsubscribe
    /// fields (S44), the trust signals (S48) and threadId (S54), so existing
    /// call sites (tests, AppModel's synthetic detail builder) that have no
    /// opinion on them don't need updating.
    /// The message's calendar invite (S55), if it carries a `text/calendar`
    /// part. Optional for decode resilience against older core payloads.
    let calendarInvite: CalendarInvite?
    /// The message's own `References` chain, oldest first (#3443). Optional for decode
    /// resilience against older core payloads.
    let references: [String]?

    /// Explicit memberwise init with defaults for the 3 List-Unsubscribe
    /// fields (S44), the trust signals (S48) and the calendar invite (S55),
    /// so existing call sites (tests, AppModel's synthetic detail builder)
    /// that have no opinion on them don't need updating.
    ///
    /// NOTE: this is required to keep decoding correct. A stored `let`
    /// property declared with an inline default (`let x: T = v`) is
    /// EXCLUDED from the compiler-synthesized `Decodable.init(from:)` — the
    /// default is used unconditionally and the JSON key is never read, even
    /// when present. Giving the properties no inline default (defaults live
    /// only as parameters here) keeps them in the synthesized CodingKeys so
    /// `Codable` still decodes real payload values, while this initializer
    /// preserves memberwise-init ergonomics for callers that don't care.
    init(
        header: MessageHeader,
        rfc822MessageId: String?,
        addresses: [Address],
        textBody: String?,
        sanitizedHtml: String?,
        hasRemoteContent: Bool? = nil,
        attachments: [AttachmentDescriptor]? = nil,
        listUnsubscribeMailto: String? = nil,
        listUnsubscribeUrl: String? = nil,
        listUnsubscribeOneClick: Bool = false,
        trust: TrustSignals? = nil,
        threadId: String? = nil,
        calendarInvite: CalendarInvite? = nil,
        references: [String]? = nil
    ) {
        self.header = header
        self.rfc822MessageId = rfc822MessageId
        self.addresses = addresses
        self.textBody = textBody
        self.sanitizedHtml = sanitizedHtml
        self.hasRemoteContent = hasRemoteContent
        self.attachments = attachments
        self.listUnsubscribeMailto = listUnsubscribeMailto
        self.listUnsubscribeUrl = listUnsubscribeUrl
        self.listUnsubscribeOneClick = listUnsubscribeOneClick
        self.trust = trust
        self.threadId = threadId
        self.calendarInvite = calendarInvite
        self.references = references
    }
}

/// One attendee (or organizer-as-attendee) from a parsed `.ics` `ATTENDEE`
/// property (S55). Every field but `email` is best-effort.
struct CalendarInviteAttendee: Codable, Hashable {
    let email: String
    let name: String?
    /// `CHAIR` | `REQ-PARTICIPANT` | `OPT-PARTICIPANT` | `NON-PARTICIPANT`, verbatim.
    let role: String?
    /// `NEEDS-ACTION` | `ACCEPTED` | `DECLINED` | `TENTATIVE` | `DELEGATED`, verbatim.
    let partStat: String?
}

/// A `DTSTART`/`DTEND` value kept as the original iCalendar wire form — see
/// the core's `castellum_models::CalendarInviteDateTime` doc comment for why
/// timezone resolution happens here (Foundation), not in Rust.
struct CalendarInviteDateTime: Codable, Hashable {
    /// e.g. `20260925T140000Z`, `20260925T140000` or (all-day) `20260925`.
    let raw: String
    let tzid: String?
    let isUtc: Bool
    let isDateOnly: Bool

    /// Resolves `raw` to a `Date`, or `nil` if it doesn't match the expected
    /// iCalendar `DATE`/`DATE-TIME` shape (defensive — the core already
    /// validates this, but a Swift-side decode should never crash on a
    /// surprise value). A floating (no `tzid`, not UTC) value is resolved in
    /// the CURRENT calendar's time zone, matching what most producers mean
    /// by "local time" when they omit both.
    func resolvedDate(calendar: Calendar = .current) -> Date? {
        var components = DateComponents()
        let digits = raw.prefix(8)
        guard digits.count == 8 else { return nil }
        let y = digits.prefix(4)
        let m = digits.dropFirst(4).prefix(2)
        let d = digits.dropFirst(6).prefix(2)
        guard let year = Int(y), let month = Int(m), let day = Int(d) else { return nil }
        components.year = year
        components.month = month
        components.day = day
        if !isDateOnly {
            let timePart = raw.dropFirst(9) // past the "T"
            guard timePart.count >= 6 else { return nil }
            let h = timePart.prefix(2)
            let mi = timePart.dropFirst(2).prefix(2)
            let s = timePart.dropFirst(4).prefix(2)
            guard let hour = Int(h), let minute = Int(mi), let second = Int(s) else { return nil }
            components.hour = hour
            components.minute = minute
            components.second = second
        }
        var cal = calendar
        if isUtc {
            cal.timeZone = TimeZone(identifier: "UTC") ?? cal.timeZone
        } else if let tzid, let tz = WindowsTimeZones.timeZone(forTZID: tzid) {
            cal.timeZone = tz
        }
        return cal.date(from: components)
    }

    /// True when a `TZID` is given but is neither an IANA identifier nor a known Windows zone
    /// name. `resolvedDate` then falls back to the Mac's own zone, so the UI must say the time
    /// may be off rather than present it as certain (#3446).
    var hasUnrecognisedTimeZone: Bool {
        guard !isUtc, !isDateOnly, let tzid, !tzid.isEmpty else { return false }
        return WindowsTimeZones.timeZone(forTZID: tzid) == nil
    }
}

/// A meeting invite parsed from one message's `text/calendar` part (S55).
/// `ourResponse`/`respondedAt` are Castellum's own memory of what the user
/// already did — the calendar event itself lives in macOS Calendar via
/// EventKit, not here. See the core's `castellum_models::CalendarInvite`.
struct CalendarInvite: Codable, Hashable {
    let uid: String
    let sequence: Int64
    /// `REQUEST` | `CANCEL` | `REPLY` | `PUBLISH` | ..., upper-cased.
    let method: String
    let summary: String?
    let dtstart: CalendarInviteDateTime?
    let dtend: CalendarInviteDateTime?
    let organizerEmail: String?
    let organizerName: String?
    let attendees: [CalendarInviteAttendee]
    /// `nil` until the user acts; then `"accepted"` | `"declined"` | `"tentative"`.
    let ourResponse: String?
    let respondedAt: Int64?
    /// LOCATION / DESCRIPTION text of the event (#3041); nil when absent.
    var location: String? = nil
    var description: String? = nil
    /// RECURRENCE-ID: the one occurrence of a recurring meeting this is about; nil = the series.
    var recurrenceId: CalendarInviteDateTime? = nil
    /// `"updated"` | `"cancelled"` when a later invite for the same meeting is in the account,
    /// derived by the core on read (#3446).
    var superseded: String? = nil
    /// Advisory: a later invite from the same organizer exists but its sender is not
    /// authenticated as that organizer, so it may be forged. Shown as a note only.
    var possiblySuperseded: String? = nil

    /// Whether Add to Calendar makes sense: not for a cancellation, a reply attachment, or an
    /// invitation a newer one has replaced.
    var offersAddToCalendar: Bool {
        guard superseded == nil else { return false }
        return ["REQUEST", "PUBLISH"].contains(method.uppercased())
    }
}

/// Identifies a message detail by its underlying message id — lets
/// `MessageDetail` drive a SwiftUI `.sheet(item:)` (S44's
/// `UnsubscribeConfirmSheet`) without a synthetic wrapper type.
extension MessageDetail: Identifiable {
    var id: String { header.id }

    /// Whether the reading pane should show an "Unsubscribe" affordance for
    /// this message (S44 / S66): true when EITHER a mailto: or an https:
    /// List-Unsubscribe target was found. Extracted so S66's tests can
    /// assert against the exact predicate `ReadingPaneView` renders from,
    /// rather than a reimplementation of it.
    var hasUnsubscribeAffordance: Bool {
        // Must match what `UnsubscribeConfirmSheet` can act on: a mailto:, or
        // an https target that is flagged RFC 8058 one-click. An https-only
        // list without the one-click flag has no action, so no link (#3046).
        // The same validated plan the sheet acts on (#3114): a target it would
        // refuse (user-info, look-alike host, extra recipients) is not offered.
        UnsubscribePlan.make(for: self).isActionable
    }
}


/// SPF/DKIM/DMARC verification result for one mechanism (S48).
struct TrustSignals: Codable, Hashable {
    /// Raw core values: "pass" | "fail" | "none". Kept as plain strings (not
    /// a Swift enum) so an unrecognised future value decodes rather than
    /// throwing — TrustBadgeView.badge(for:) maps them, defaulting anything
    /// unrecognised to "not verified".
    let spf: String?
    let dkim: String?
    let dmarc: String?
    let authservId: String?
}

/// Link-hover analysis (S48): real destination host, text/href mismatch,
/// punycode/IDN caution.
struct LinkInspection: Codable, Hashable {
    let href: String
    let scheme: String?
    let displayHost: String?
    let unicodeHost: String?
    let idnCaution: Bool
    let textMismatch: Bool
}

/// A remembered "always load remote content from this sender" decision (S48).
struct SenderRemoteContentPolicy: Codable, Hashable, Identifiable {
    let accountId: String
    let fromAddress: String
    let allowed: Bool
    let updatedAt: String
    var id: String { "\(accountId)|\(fromAddress)" }
}

/// Body-dependent actions must wait for descriptor-only messages to hydrate.
/// A missing `hasBody` is treated as ready for compatibility with older cores.
func messageBodyActionsAvailable(_ detail: MessageDetail?) -> Bool {
    guard let detail else { return false }
    return detail.header.hasBody != false
}

/// One composer address entry (`{name?, email}`) inside a draft's to/cc/bcc.
struct DraftAddress: Codable, Hashable {
    var name: String?
    var email: String
}

/// One outgoing attachment's metadata (no bytes). Mirrors the Rust
/// `DraftAttachmentMeta`.
struct DraftAttachmentMeta: Codable, Identifiable, Hashable {
    let id: Int64
    let ordinal: Int64
    let filename: String
    let contentType: String
    let sizeBytes: Int64
    let isInline: Bool
    let contentId: String?
}

/// A new outgoing attachment to add to a draft. `dataBase64` is the file's
/// raw bytes, base64-encoded: the FFI carries every payload as a JSON
/// C-string (one wire contract for every call).
struct NewDraftAttachment: Codable {
    var filename: String
    var contentType: String
    var isInline: Bool = false
    var contentId: String?
    var dataBase64: String
}

/// One image to embed inline in a draft (MM-5b). The core decides the type from the bytes and
/// generates the Content-ID; `alt` defaults to the file name without its extension.
struct NewInlineImage: Codable {
    var filename: String
    var alt: String?
    var dataBase64: String
}

/// Result of `castellum_embed_draft_image`: the stored inline row, the Markdown reference to put
/// in the body, and the draft's refreshed attachment list.
struct EmbeddedImage: Codable {
    let attachment: DraftAttachmentMeta
    let markdown: String
    let attachments: [DraftAttachmentMeta]
}

/// A compose draft to create or update, as sent to the core. `id` is `nil` to
/// create a new draft, non-nil to update an existing one (only while its status
/// is still `draft`). Mirrors the Rust `NewDraft`.
struct NewDraft: Codable, Hashable {
    var id: Int64?
    var accountId: String
    var to: [DraftAddress]
    var cc: [DraftAddress] = []
    var bcc: [DraftAddress] = []
    var subject: String = ""
    var bodyText: String = ""
    /// Sanitized, already-rendered HTML alternative for `bodyText` when
    /// Markdown is on for this message (S46); `nil` = plain text only.
    var bodyHtml: String?
    /// The identity/alias this message sends from, if the From picker
    /// chose one other than the account's own address (S47). `nil` = the
    /// account's own address, today's behaviour, unchanged.
    var fromEmail: String?
    var fromName: String?
    /// RFC822 Message-ID this draft replies to, if any.
    var inReplyTo: String?
    /// Full References header value for the reply chain, if any.
    var referencesHeader: String?
}

/// A stored compose draft. `status` walks draft -> outbox (queued) -> sent.
struct Draft: Codable, Identifiable, Hashable {
    let id: Int64
    let accountId: String
    let to: [DraftAddress]
    let cc: [DraftAddress]
    let bcc: [DraftAddress]
    let subject: String
    let bodyText: String
    /// See `NewDraft.bodyHtml`.
    let bodyHtml: String?
    /// See `NewDraft.fromEmail`/`fromName`.
    let fromEmail: String?
    let fromName: String?
    let inReplyTo: String?
    let referencesHeader: String?
    /// "draft" | "outbox" | "sent".
    let status: String
    /// `updated_at` as Unix epoch seconds (list ordering / UI display).
    let updatedAtEpoch: Int64
    /// S36: server-mirror bookkeeping. `nil` = never saved to the server
    /// Drafts mailbox (or the mirror was cleaned up on send/discard).
    let serverMailboxPath: String?
    let serverUid: Int64?
    let serverUidValidity: Int64?
    let serverMessageId: String?
    let serverSavedAtEpoch: Int64?
}

/// Outcome of a `send_pending` run (SMTP delivery of queued drafts).
struct SendReport: Codable, Hashable {
    let accountId: String
    /// Drafts delivered successfully this run.
    let sent: Int
    /// Send ops that failed (left as `failed`, with the error recorded).
    let failed: Int
    /// Sends NOT attempted because the account is read-only (evaluation
    /// mode). Absent in older reports, so it decodes as 0.
    let blocked: Int

    init(accountId: String, sent: Int, failed: Int, blocked: Int = 0) {
        self.accountId = accountId
        self.sent = sent
        self.failed = failed
        self.blocked = blocked
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        accountId = try c.decode(String.self, forKey: .accountId)
        sent = try c.decode(Int.self, forKey: .sent)
        failed = try c.decode(Int.self, forKey: .failed)
        blocked = try c.decodeIfPresent(Int.self, forKey: .blocked) ?? 0
    }
}

/// One message waiting in the Outbox (OB1), from `castellum_outbox_sends`:
/// a draft in `outbox` status and the state of its send. Carries a failure
/// KIND only (`errorKind`), never the server's own text.
struct OutboxSend: Codable, Hashable, Identifiable {
    let draftId: Int64
    let accountId: String
    /// `nil` = stranded: the draft is in the Outbox with no live send.
    let operationId: Int64?
    let subject: String
    let recipients: [String]
    /// queued | running | failed | failed_permanent | held |
    /// needs_confirmation | stranded
    let status: String
    let attempts: Int64
    /// auth | tls | network | protocol | unsupported | internal
    let errorKind: String?
    /// Whether Castellum will try again by itself.
    let automaticRetry: Bool
    let nextAttemptEpoch: Int64?
    let scheduled: Bool
    let updatedAtEpoch: Int64

    var id: Int64 { draftId }
}

struct ThreadSummary: Codable, Identifiable, Hashable {
    let id: String
    let accountId: String
    let subject: String?
    let lastMessageAt: Int64?
    let messageCount: Int64
    let unreadCount: Int64
}

struct SyncMailboxReport: Codable, Hashable {
    let path: String
    let fetched: Int64
    let descriptorsFetched: Int64?
    let bodiesFetched: Int64?
    let bytesFetched: Int64?
    let fetchedUidMin: Int64?
    let fetchedUidMax: Int64?
    let total: Int64
}

struct SyncReport: Codable, Hashable {
    let accountId: String
    let mailboxes: [SyncMailboxReport]
}

/// One leg (IMAP or SMTP) of an account-setup connection test (S62).
/// `errorKind` is the Rust `redact::ErrorKind` tag; `summary` its fixed,
/// server-text-free phrase. Mirrors Rust `connection_test::LegResult`.
struct ConnectionLegResult: Codable, Hashable {
    let ok: Bool
    let errorKind: String?
    let summary: String?
}

/// A server folder offered by the setup sheet's folder picker. Mirrors Rust
/// `connection_test::MailboxChoice`; `syncByDefault` is exactly what the
/// first sync would cover with no explicit mailbox filter.
struct ConnectionTestMailbox: Codable, Hashable, Identifiable {
    var id: String { path }
    let path: String
    let name: String
    let role: String?
    let selectable: Bool
    let syncByDefault: Bool
}

/// Result of `castellum_test_connection` (S62). Mirrors Rust
/// `connection_test::ConnectionTestReport`.
struct ConnectionTestReport: Codable, Hashable {
    let imap: ConnectionLegResult
    /// Nil when the request carried no SMTP settings.
    let smtp: ConnectionLegResult?
    /// Empty unless the IMAP leg succeeded.
    let mailboxes: [ConnectionTestMailbox]
}

/// One server endpoint found by account auto-detect (MM-14). Mirrors Rust
/// `castellum_core::autoconfig::Endpoint`; `tls` is "implicit" or "starttls".
struct AutoconfigEndpoint: Codable, Hashable {
    let host: String
    let port: Int
    let tls: String
}

/// How a username is written for the found server: the whole address or only
/// the part before the @. The wire values are Rust's `UsernameForm`.
enum UsernameForm: String, Codable, Hashable {
    case email
    case localPart = "local_part"
}

/// Result of `castellum_autoconfig_lookup` (MM-14). Mirrors Rust
/// `castellum_core::autoconfig::AutoconfigReport`. `status` is one of
/// found | preset | unsupported | not_found | offline; `source` is
/// srv | mx | domain_autoconfig | well_known | ispdb.
struct AutoconfigReport: Codable, Hashable {
    let status: String
    let source: String?
    /// status = preset: gmail | fastmail | icloud | yahoo (see `ProviderPreset.byId`).
    let presetId: String?
    /// status = unsupported: microsoft | oauth_only | no_tls_imap.
    let unsupported: String?
    let displayName: String?
    let imap: AutoconfigEndpoint?
    let smtp: AutoconfigEndpoint?
    let username: UsernameForm?
    /// True when a DNS-sourced server is not under the looked-up domain: DNS is
    /// unauthenticated, so such a host is never pre-filled (MM-14 security).
    let hostOutsideDomain: Bool?

    init(status: String, source: String? = nil, presetId: String? = nil, unsupported: String? = nil,
         displayName: String? = nil, imap: AutoconfigEndpoint? = nil, smtp: AutoconfigEndpoint? = nil,
         username: UsernameForm? = nil, hostOutsideDomain: Bool? = nil) {
        self.hostOutsideDomain = hostOutsideDomain
        self.status = status; self.source = source; self.presetId = presetId
        self.unsupported = unsupported; self.displayName = displayName
        self.imap = imap; self.smtp = smtp; self.username = username
    }
}

/// The mailbox currently being fetched in a `SyncProgress` snapshot.
struct SyncMailboxProgress: Codable, Hashable {
    let path: String
    let fetched: Int64
    let total: Int64
}

/// A best-effort, in-process snapshot of an account's current (or most
/// recently finished) sync pass — polled alongside a
/// long-running sync rather than pushed, so the UI can show real numbers
/// ("Inbox: 1,204 / 12,830") instead of an indeterminate spinner during a
/// large first sync. Mirrors Rust `castellum_core::sync_progress::SyncProgress`.
struct SyncProgress: Codable, Hashable {
    let accountId: String
    let mailboxesCompleted: Int
    let mailboxesTotal: Int
    let currentMailbox: SyncMailboxProgress?
    let aggregateFetched: Int64
    let aggregateTotal: Int64
    let done: Bool
}

/// Per-mailbox outcome of one bounded background body-hydration pass.
struct MailboxHydrationReport: Codable, Hashable {
    let path: String
    let requested: Int
    let hydrated: Int
    let bytesFetched: Int64
    let durationMs: UInt64
    /// Missing bodies left in this mailbox after the pass.
    let remaining: Int64
    /// Select/UIDVALIDITY/body-fetch failure for this mailbox, if any. Later
    /// mailboxes in the same pass still hydrate.
    let error: String?
}

/// Result of one bounded background body-hydration pass. Re-invoke while
/// `hydrated > 0 && remaining > 0` to drain the backlog gradually.
struct HydrationReport: Codable, Hashable {
    let accountId: String
    let mailboxes: [MailboxHydrationReport]
    let requested: Int
    let hydrated: Int
    let bytesFetched: Int64
    /// Backlog left across all mailboxes after the pass; 0 = drained.
    let remaining: Int64
    let elapsedMs: UInt64
}

/// The redacted support bundle (mirrors Rust `SupportBundle`). Every field is
/// a constant, a count, a timestamp, or a value re-validated against fixed
/// sets by the core — never credentials, message content, folder names, or
/// raw error text.
struct SupportBundle: Codable, Hashable {
    let bundleSchema: Int
    let generatedAt: String
    let schemaVersion: Int64
    let coreVersion: String
    let integrity: IntegrityReport
    let accounts: [AccountDiagnostics]
}

/// Read-only `PRAGMA quick_check` result (mirrors Rust `IntegrityReport`).
struct IntegrityReport: Codable, Hashable {
    let ok: Bool
    let issueCount: Int
    let firstIssue: String?
}

/// Last-run sync KPI snapshot (mirrors Rust `LastSyncKpiSnapshot` in
/// native/crates/castellum-core/src/sync_kpi.rs, exposed via
/// `castellum_last_sync_kpi`). Same discipline as `SupportBundle`: only
/// counts and timings, never message content, mailbox names or server
/// identifiers. `accountId` is the app's own local account id — in the
/// common case (see `AccountSetupSheet.derivedAccountId`) that is the
/// account's own email, so this struct is intentionally NOT included in the
/// redacted "Gather Diagnostics…" bundle verbatim; only the numeric fields
/// are surfaced there (see `DiagnosticsViewModel.kpiSummaryText`).
///
/// `CastellumCore.lastSyncKpi()` decodes the Rust side's `"null"` sentinel
/// (written when no sync has completed yet, NOT a failure) straight into a
/// Swift `nil` by asking `decodeJSON` for `LastSyncKpi?` rather than
/// `LastSyncKpi` — `Optional<T>` is itself `Decodable` whenever `T` is, so
/// this needs no bespoke "success but null" handling on top of the existing
/// out-param + status-code call convention.
struct LastSyncKpi: Codable, Hashable {
    let available: Bool
    let accountId: String
    /// Unix epoch seconds when this snapshot was written.
    let timestampUnix: Int64
    /// Total new messages imported by the sync this snapshot describes.
    let messageCount: Int64
    let wallMs: Double
    /// Keyed by phase name (`"imap.fetch_descriptors"`, `"db.upsert_descriptor"`,
    /// `"core.parse_body"`, …) — only phases actually exercised by that sync
    /// are present. Prefix convention: `imap.*` = network round-trips,
    /// `db.*` = SQLite writes, `core.*` = in-process parsing/rules.
    let phases: [String: SyncPhaseKpi]

    /// `timestampUnix` as a `Date`, for display.
    var timestamp: Date { Date(timeIntervalSince1970: TimeInterval(timestampUnix)) }

    /// Messages synced per second, or `nil` when `wallMs` is non-positive
    /// (an effectively-instant or zero-message sync) — avoids a divide-by-zero.
    var messagesPerSecond: Double? {
        guard wallMs > 0 else { return nil }
        return Double(messageCount) / (wallMs / 1000.0)
    }

    /// Sum of every phase's `totalMs` whose name has the given prefix
    /// (`"imap."` for network round-trip time, `"db."` for SQLite write
    /// time). Used by the Diagnostics "Performance" section to roll many
    /// fine-grained phases up into the few numbers worth glancing at.
    func totalMs(forPhasePrefix prefix: String) -> Double {
        phases.filter { $0.key.hasPrefix(prefix) }.reduce(0) { $0 + $1.value.totalMs }
    }
}

/// One phase's timing distribution (mirrors Rust `PhaseKpi`).
struct SyncPhaseKpi: Codable, Hashable {
    let count: Int
    let totalMs: Double
    let p50Ms: Double
    let p95Ms: Double
    let maxMs: Double
}

struct AccountDiagnostics: Codable, Hashable {
    /// Per-bundle ordinal label ("account-1"); never the id or email.
    let label: String
    let provider: String
    let capabilities: [String]
    let lastAttemptAt: String?
    let lastSuccessAt: String?
    let lastErrorKind: String?
    let lastError: String?
    let lastErrorPhase: String?
    let consecutiveFailures: Int64
    let operations: OperationSummary
    let mailboxes: [MailboxDiagnostics]
}

struct OperationSummary: Codable, Hashable {
    let queued: Int64
    let running: Int64
    let succeeded: Int64
    let failed: Int64
    let failedPermanent: Int64
    let cancelled: Int64
    let other: Int64
}

struct MailboxDiagnostics: Codable, Hashable {
    /// Role name for well-known roles, else "folder-N"; never the path.
    let label: String
    let role: String?
    let lastAttemptAt: String?
    let lastSuccessAt: String?
    let lastErrorKind: String?
    let lastError: String?
    let lastErrorPhase: String?
    let consecutiveFailures: Int64
    let totalMessages: Int64
    let messagesWithBody: Int64
    let messagesWithoutBody: Int64
    let hasSyncCursor: Bool
}

// MARK: - Local diagnostics (never export; identity is real here)

/// Per-mailbox local diagnostic row: real path + redacted error text.
/// Mirrors Rust `LocalMailboxDiagnostics`. LOCAL-ONLY — never export.
struct LocalMailboxDiagnostics: Codable, Hashable {
    let mailboxId: String
    /// The real mailbox path — shown in the Diagnostics window only.
    let path: String
    let role: String?
    let lastAttemptAt: String?
    let lastSuccessAt: String?
    /// Re-validated against known kinds; "redacted" if unknown.
    let lastErrorKind: String?
    /// Re-validated against known summaries; "redacted" if unknown.
    let lastError: String?
    /// Re-validated against known phases; "redacted" if unknown.
    let lastErrorPhase: String?
    let consecutiveFailures: Int64
    let totalMessages: Int64
    let messagesWithBody: Int64
    let messagesWithoutBody: Int64
    let hasSyncCursor: Bool
}

/// Per-account local diagnostic row: real email + redacted error text.
/// Mirrors Rust `LocalAccountDiagnostics`. LOCAL-ONLY — never export.
struct LocalAccountDiagnostics: Codable, Hashable {
    let accountId: String
    /// The real account email — shown in the Diagnostics window only.
    let email: String
    let provider: String
    let lastAttemptAt: String?
    let lastSuccessAt: String?
    let lastErrorKind: String?
    let lastError: String?
    let lastErrorPhase: String?
    let consecutiveFailures: Int64
    let mailboxes: [LocalMailboxDiagnostics]
}

/// The full local-only diagnostics payload. LOCAL-ONLY — never export.
struct LocalDiagnostics: Codable, Hashable {
    let accounts: [LocalAccountDiagnostics]
}

/// A person aggregated from message addresses (mirrors Rust `ContactRecord`).
/// Derived fields are recomputed by refresh; displayName/notes/isHidden/
/// isUserCreated are user-owned and preserved.
struct Contact: Codable, Identifiable, Hashable {
    let id: Int64
    /// Normalized (lowercase, trimmed) email; unique.
    let email: String
    /// Best-pick name from mail headers.
    let derivedName: String?
    /// User override; never touched by refresh.
    let displayName: String?
    let notes: String
    let isHidden: Bool
    let isSelf: Bool
    let isUserCreated: Bool
    let firstSeenAt: Int64?
    let lastSeenAt: Int64?
    let inboundCount: Int64
    let outboundCount: Int64

    /// What the UI shows: the user's override, else the header-derived name,
    /// else the address itself.
    var bestName: String { displayName ?? derivedName ?? email }
}

/// A recipient of mail we sent or queued (from the sent/outbox drafts) — the
/// autocomplete source that is valid before the Sent copy syncs back.
struct SentRecipient: Codable, Hashable {
    let email: String
    let name: String?
    let lastSentAt: Int64
    let sentCount: Int64
}

/// An extra From address (S47) a compose window's From picker can offer
/// alongside the account's own address, which is never itself a row here.
struct Identity: Codable, Identifiable, Hashable {
    let id: Int64
    let accountId: String
    let email: String
    let displayName: String?
    /// MM-6: Reply-To header added to mail sent from this identity.
    let replyTo: String?
    /// MM-6: an address silently added to the envelope of every message sent
    /// from this identity (never shown in headers).
    let bcc: String?
    /// MM-6: the account's default identity for new mail (at most one).
    let isDefault: Bool

    init(id: Int64, accountId: String, email: String, displayName: String?,
         replyTo: String? = nil, bcc: String? = nil, isDefault: Bool = false) {
        self.id = id
        self.accountId = accountId
        self.email = email
        self.displayName = displayName
        self.replyTo = replyTo
        self.bcc = bcc
        self.isDefault = isDefault
    }
}

/// Full contact detail — all `Contact` fields plus enrichment rows (slice 8d).
/// Returned by `castellum_get_contact`; the enrichment array is absent from the
/// list path (`castellum_list_contacts`) so that stays light.
struct ContactDetail: Codable, Identifiable, Hashable {
    let id: Int64
    let email: String
    let derivedName: String?
    let displayName: String?
    let notes: String
    let isHidden: Bool
    let isSelf: Bool
    let isUserCreated: Bool
    let firstSeenAt: Int64?
    let lastSeenAt: Int64?
    let inboundCount: Int64
    let outboundCount: Int64
    /// Enrichment fields for this contact (sorted by source, field).
    let enrichment: [EnrichmentField]

    var bestName: String { displayName ?? derivedName ?? email }

    /// Convert to the lightweight `Contact` DTO used by the list view.
    var asContact: Contact {
        Contact(
            id: id, email: email, derivedName: derivedName,
            displayName: displayName, notes: notes, isHidden: isHidden,
            isSelf: isSelf, isUserCreated: isUserCreated,
            firstSeenAt: firstSeenAt, lastSeenAt: lastSeenAt,
            inboundCount: inboundCount, outboundCount: outboundCount
        )
    }
}

/// One enrichment field row (mirrors the Rust `EnrichmentFieldOut` shape).
struct EnrichmentField: Codable, Identifiable, Hashable {
    let id: Int64
    let source: String
    let field: String
    let value: String
    let provenance: String?
    let fetchedAt: Int64
}

/// A PREVIEW enrichment row (mirrors Rust `PreviewFieldOut`): the shape
/// returned by `enrich` with save:false. Deliberately distinct from
/// `EnrichmentField` — previews have no DB id/source/fetchedAt yet.
/// Contract guarded by the Rust FFI test
/// `enrichment_preview_and_save_shapes_over_the_c_abi`.
struct PreviewEnrichmentField: Codable, Hashable {
    let field: String
    let value: String
    let provenance: String
}

/// An enrichment adapter registered on the Rust side (slice 8d).
struct EnrichmentSourceInfo: Codable, Identifiable, Hashable {
    let id: String
    let displayName: String
    /// "PasteImport" | "Http"
    let kind: String
    /// Short label to show above the input field in the UI.
    let inputLabel: String
}

/// Outcome of a contacts refresh pass.
struct AggregateReport: Codable, Hashable {
    let contactsUpserted: Int64
    let edgesRebuilt: Int64
    let elapsedMs: UInt64
}

/// Outcome of the one-off repair of subjects and display names that an
/// earlier build stored RFC 2047-encoded (stage X1). All counts are zero when
/// `alreadyDone`: the repair completed on a previous launch.
struct EnvelopeTextRepairReport: Codable, Hashable {
    let alreadyDone: Bool
    let batches: Int64
    let scanned: Int64
    let messagesRepaired: Int64
    let subjectsRepaired: Int64
    let namesRepaired: Int64
}

/// A co-occurrence edge between two contacts (canonical contactA < contactB).
struct EdgeRecord: Codable, Hashable {
    let contactA: Int64
    let contactB: Int64
    let weight: Double
    /// Shared messages only (thread-only co-occurrence adds weight, not count).
    let messageCount: Int64
    let lastSeenAt: Int64?
}

// MARK: - Projects

/// A project card as returned by list_projects (mirrors Rust ProjectCard).
struct ProjectCard: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    let notes: String
    /// "active" | "waiting" | "someday" | "done"
    let status: String
    let color: String?
    let position: Int64
    let archived: Bool
    let createdAt: Int64
    let updatedAt: Int64
    let lastActivityAt: Int64
    let emailCount: Int64
    let contactCount: Int64
}

/// A full project row (mirrors Rust Project).
struct Project: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    let notes: String
    /// "active" | "waiting" | "someday" | "done"
    let status: String
    let color: String?
    let position: Int64
    let archived: Bool
    let createdAt: Int64
    let updatedAt: Int64
    let lastActivityAt: Int64
}

/// A contact linked to a project (mirrors Rust ProjectContactLink).
struct ProjectContactLink: Codable, Hashable {
    let contactId: Int64
    let email: String
    let derivedName: String?
    let displayName: String?
    /// "auto" = inferred from email addresses; "manual" = user-confirmed.
    let origin: String
    let linkedAt: Int64
}

/// A user-owned work item (mirrors Rust WorkItem, migrations 0018 + 0045).
/// Never created automatically from mail; relationships to
/// messages/threads/contacts go through the shared link model. The four
/// trailing optionals are absent in pre-0045 JSON and null when unset.
struct WorkItem: Codable, Identifiable, Hashable {
    let id: String
    /// nil = an unassigned item (belongs to no project).
    let projectId: String?
    let title: String
    let notes: String
    /// One of: needs_reply, waiting, delegated, blocked, done, someday.
    let state: String
    let position: Int64
    let createdAt: Int64
    let updatedAt: Int64
    /// Due date, epoch seconds.
    var dueAt: Int64? = nil
    /// Epoch seconds at which the item entered `waiting` (core-managed).
    var waitingSince: Int64? = nil
    /// Epoch seconds at which the item entered `done` (core-managed).
    var completedAt: Int64? = nil
    /// Contact the item was delegated to (state `delegated`).
    var delegatedToContactId: Int64? = nil
}

/// The proposal inside a `Suggestion` (mirrors the Rust payload JSON). Absent
/// keys decode to nil: a `state_change` has no title, a plain item no state.
struct SuggestionPayload: Codable, Hashable {
    /// Proposed work-item title (suggestion text, not mail content).
    var title: String? = nil
    /// Proposed due date, epoch seconds.
    var dueAt: Int64? = nil
    /// `state_change` only: the state the target item would move to.
    var toState: String? = nil
}

/// A suggestion you accept (mirrors Rust Suggestion, migration 0046, AW2).
/// Castellum PROPOSES; nothing becomes a work item until the user accepts.
/// Evidence is ids only (message, thread, extraction row, RFC822 id).
struct Suggestion: Codable, Identifiable, Hashable {
    let id: String
    /// One of: work_item, waiting_for_reply, state_change.
    let kind: String
    /// One of: proposed, accepted, dismissed.
    let status: String
    let payload: SuggestionPayload
    var messageId: String? = nil
    var threadId: String? = nil
    var extractionId: Int64? = nil
    var rfc822MessageId: String? = nil
    /// `state_change`: the work item that would move.
    var targetWorkItemId: String? = nil
    /// Set once accepted: the work item this suggestion became.
    var createdWorkItemId: String? = nil
    /// One of: heuristic, ai_extraction.
    let origin: String
    var confidence: Double? = nil
    let dedupeKey: String
    let createdAt: Int64
    var decidedAt: Int64? = nil
}

/// Result of accepting a suggestion: the accepted row and the work item it
/// created (or, for a state change, moved).
struct SuggestionAccepted: Codable, Hashable {
    let suggestion: Suggestion
    var workItem: WorkItem? = nil
}

/// `castellum_count_suggestions` result: suggestions awaiting a decision.
struct SuggestionCount: Codable, Hashable {
    let proposed: Int64
}

/// The work-item filter a saved view stores (mirrors Rust WorkItemFilter,
/// AW1 + AW3). Every key is optional and absent when unset. `due` holds
/// relative windows ("today", "this_week", "overdue"), OR-ed together;
/// `waitingOnMe` is the needs_reply shorthand, `waitingOnOthers` the
/// waiting-or-delegated one.
struct SavedViewFilter: Codable, Hashable {
    var projectId: String? = nil
    var states: [String]? = nil
    var dueBefore: Int64? = nil
    var dueAfter: Int64? = nil
    var overdue: Bool? = nil
    var contactId: Int64? = nil
    var linkedMessageId: String? = nil
    var linkedThreadId: String? = nil
    var unassigned: Bool? = nil
    var due: [String]? = nil
    var waitingOnMe: Bool? = nil
    var waitingOnOthers: Bool? = nil
}

/// A saved Active Work view (mirrors Rust SavedView, migration 0047). The
/// four built-ins (ids "builtin:*") can be hidden and reordered but not
/// renamed or deleted.
struct SavedView: Codable, Identifiable, Hashable {
    let id: String
    let name: String
    let filter: SavedViewFilter
    let position: Int64
    let builtIn: Bool
    let hidden: Bool
    let createdAt: Int64
    let updatedAt: Int64
}

/// One visible saved view's item count (mirrors Rust SavedViewCount).
struct SavedViewCount: Codable, Identifiable, Hashable {
    let id: String
    let count: Int64
}

/// An inbox thread with no project or work-item link (mirrors Rust
/// UnassignedThread). Ids and counts only; rows come from the message list.
struct UnassignedThread: Codable, Identifiable, Hashable {
    let threadId: String
    /// Messages of the thread that sit in an inbox.
    let messageCount: Int64
    /// Newest message time in the inbox, epoch seconds.
    let lastMessageAt: Int64?
    var id: String { threadId }
}

/// One page of unassigned mail (mirrors Rust UnassignedMailPage).
struct UnassignedMailPage: Codable, Hashable {
    let threads: [UnassignedThread]
    /// Unassigned inbox threads in all, independent of paging.
    let total: Int64
    let limit: Int64
    let offset: Int64
}

/// One row of the shared evidence-backed link model (mirrors Rust
/// LinkRecord, migration 0017). `provenance` answers "why is this
/// connected?" with entity ids only (e.g. "message:acc:INBOX:9"); `note` is
/// user-owned and survives derived refreshes.
struct LinkRecord: Codable, Identifiable, Hashable {
    let id: Int64
    let srcKind: String
    let srcId: String
    let dstKind: String
    let dstId: String
    let relation: String
    /// "manual", "derived", or a named source (e.g. "linkedin-import").
    let origin: String
    let provenance: String?
    let confidence: Double?
    let note: String?
    let createdAt: Int64
    let updatedAt: Int64
}

/// A project's full detail: the project row + linked emails + linked contacts.
struct ProjectDetail: Codable, Hashable {
    let project: Project
    let emails: [MessageHeader]
    let contacts: [ProjectContactLink]
}

/// A lightweight tag returned by projects_for_message (reverse lookup).
struct ProjectTag: Codable, Hashable {
    let projectId: String
    let name: String
    let color: String?
}

// MARK: - People graph (slice 8c)

/// A node in the contact co-occurrence graph (mirrors Rust GraphNode).
/// `messageCount` = inbound + outbound (used for node sizing).
struct GraphNode: Codable, Identifiable, Hashable {
    let id: Int64
    /// display_name ?? derived_name ?? email — the same bestName priority order.
    let label: String
    let messageCount: Int64
}

/// A directed edge in the co-occurrence graph (canonical a < b).
struct GraphEdge: Codable, Hashable {
    let a: Int64
    let b: Int64
    let weight: Double
}

/// The full graph payload returned by the core.
struct ContactGraph: Codable, Hashable {
    let nodes: [GraphNode]
    let edges: [GraphEdge]
}

// MARK: - AI job queue (slice 7b)

/// Result of enqueueing an AI job.
struct AiEnqueueResult: Codable, Hashable {
    let jobId: Int64
}

/// Snapshot of an AI job as returned by poll.
struct AiJobInfo: Codable, Hashable {
    let id: Int64
    let status: String
    let feature: String
    let attempts: Int64
    let errorCode: String?
    /// Latency telemetry (M5, additive): wall milliseconds of the generate or
    /// embed call, and tokens/s where the backend reported token counts. All
    /// absent/null until the job completes and for jobs from older cores, so
    /// they stay optional and old JSON keeps decoding.
    var durationMs: Int64? = nil
    var tokensIn: Int64? = nil
    var tokensOut: Int64? = nil
    var tokensPerSecond: Double? = nil
}

/// One streaming event within an AI job (delta text, lifecycle markers, etc.).
struct AiEvent: Codable, Hashable {
    let id: Int64
    let kind: String
    /// Raw payload text (delta chunk, or JSON-encoded metadata for error events).
    let payload: String
}

/// Poll result: current job snapshot + any new events since the last call.
/// The core returns JSON `null` for an unknown job id; decode as `AiPollResult?`.
struct AiPollResult: Codable, Hashable {
    let job: AiJobInfo
    let events: [AiEvent]
}

/// One Ollama model available at the configured endpoint.
struct AiModelInfo: Codable, Hashable, Identifiable {
    var id: String { name }
    let name: String
    let sizeBytes: Int64?
}

/// Cached AI output (stored in the DB by the executor after each run).
struct AiOutput: Codable, Hashable {
    let text: String
    /// `true` = fully generated; `false` = partial (cancelled or interrupted).
    let complete: Bool
    let model: String?
    let tokensIn: Int64?
    let tokensOut: Int64?
    let jobId: Int64?
}

/// Summary of an `run_ai_jobs` pass.
struct AiRunReport: Codable, Hashable {
    let ran: Int
    let succeeded: Int
    let failed: Int
    let cancelled: Int
    /// Embeddings pass stopped because the endpoint did not answer (F1).
    let endpointDown: Bool
    /// `network`, `timeout`, `model_missing`, `endpoint_refused` or `embed_unsupported`.
    let errorCode: String?
    /// Embeddings scope: jobs still to do / vectors stored in total.
    let queuedEmbeddings: Int
    let embedded: Int

    init(ran: Int, succeeded: Int, failed: Int, cancelled: Int, endpointDown: Bool = false,
         errorCode: String? = nil, queuedEmbeddings: Int = 0, embedded: Int = 0) {
        self.ran = ran; self.succeeded = succeeded; self.failed = failed; self.cancelled = cancelled
        self.endpointDown = endpointDown; self.errorCode = errorCode
        self.queuedEmbeddings = queuedEmbeddings; self.embedded = embedded
    }

    /// The F1 fields are absent from older payloads; default them rather than fail the decode.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        self.init(
            ran: try c.decode(Int.self, forKey: .ran),
            succeeded: try c.decode(Int.self, forKey: .succeeded),
            failed: try c.decode(Int.self, forKey: .failed),
            cancelled: try c.decode(Int.self, forKey: .cancelled),
            endpointDown: try c.decodeIfPresent(Bool.self, forKey: .endpointDown) ?? false,
            errorCode: try c.decodeIfPresent(String.self, forKey: .errorCode),
            queuedEmbeddings: try c.decodeIfPresent(Int.self, forKey: .queuedEmbeddings) ?? 0,
            embedded: try c.decodeIfPresent(Int.self, forKey: .embedded) ?? 0)
    }
}

// MARK: - Thread extraction (S54)

/// One schema-validated action item / date / amount extracted from a
/// thread. Already validated server-side (`defense::validate_output`'s
/// `Json` schema, `extraction::parse_and_validate`) before this row ever
/// existed — the panel renders it directly, never re-parses or re-validates.
struct ThreadExtractionItem: Codable, Hashable, Identifiable {
    let id: Int64
    let threadId: String
    let messageId: String?
    /// "action_item" | "date" | "amount" — see `ExtractionKind` for the
    /// typed view the panel actually switches on.
    let kind: String
    let text: String
    let structuredValue: StructuredValue?
    let createdAt: String

    var extractionKind: ExtractionKind { ExtractionKind(rawValue: kind) ?? .unknown }

    /// `structured_value`'s two possible shapes. Both fields are optional
    /// because the JSON on the wire only ever populates the pair matching
    /// `kind` (or neither, for `action_item`) — decoding never fails just
    /// because the other pair is absent.
    struct StructuredValue: Codable, Hashable {
        let date: String?
        let amount: Double?
        let currency: String?
    }
}

/// Swift-side view of `ThreadExtractionItem.kind`. `.unknown` is the never-
/// crash fallback for a kind string this build doesn't recognise (an older
/// panel reading a newer core's row) — rendered as a plain chip with no icon
/// rather than force-unwrapped or skipped.
enum ExtractionKind: String {
    case actionItem = "action_item"
    case date
    case amount
    case unknown
}

// MARK: - Undoable moves (archive / delete / move-to-folder)

/// Where `CastellumCore.moveMessages` sends a batch.
enum MoveTarget: Equatable, Sendable {
    case archive
    case trash
    /// A specific mailbox id (must belong to the messages' account).
    case mailbox(id: String)
}

/// Wire form of a `castellum_move_messages` request.
struct MoveRequest: Encodable {
    let messageIds: [String]
    let target: String
    let mailboxId: String?

    init(ids: [String], target: MoveTarget) {
        messageIds = ids
        switch target {
        case .archive: self.target = "archive"; mailboxId = nil
        case .trash: self.target = "trash"; mailboxId = nil
        case .mailbox(let id): self.target = "mailbox"; mailboxId = id
        }
    }
}

/// One message the core declined to move.
struct MoveRefusal: Decodable, Equatable, Sendable {
    let messageId: String
    let reason: String
    /// True when the account is read-only (evaluation mode).
    let evaluationMode: Bool
}

/// Result of `castellum_move_messages`. `operationIds` are opaque undo
/// handles for `castellum_undo_moves`, one per message actually moved.
struct MoveBatchResult: Decodable, Equatable, Sendable {
    let operationIds: [Int64]
    let refused: [MoveRefusal]
}

/// One op `castellum_undo_moves` could not undo.
struct UndoRefusal: Decodable, Equatable, Sendable {
    let operationId: Int64
    let reason: String
    let evaluationMode: Bool
}

/// Result of `castellum_undo_moves`.
struct UndoMovesResult: Decodable, Equatable, Sendable {
    let queued: Int
    let accountIds: [String]
    let sourceMailboxIds: [String]
    let notUndoable: [UndoRefusal]
}

// MARK: - Remote content / tracker report (#2812)

/// What a message's remote references look like, from
/// `castellum_remote_content_report`. A "tracker" is a 1x1/0x0 image or one
/// served by a known mail-tracking service.
struct RemoteContentReport: Decodable, Equatable, Sendable {
    struct Service: Decodable, Equatable, Sendable {
        let name: String
        let count: Int
    }
    struct Host: Decodable, Equatable, Sendable {
        let host: String
        let count: Int
    }
    let remoteCount: Int
    let trackerCount: Int
    let pixelCount: Int
    let services: [Service]
    let hosts: [Host]
    /// The scan hit the core's input or reference cap; the counts are a lower bound.
    var truncated: Bool = false

    static let none = RemoteContentReport(
        remoteCount: 0, trackerCount: 0, pixelCount: 0, services: [], hosts: [])
}

/// The one-line wording of the blocked-content chip in the reader.
enum TrackerReportPresentation {
    /// "3 trackers blocked (Mailchimp, HubSpot)" when any tracker was found,
    /// else the plain "Remote images blocked". At most three service names.
    static func label(for report: RemoteContentReport?) -> String {
        guard let report, report.trackerCount > 0 else {
            return String(localized: "Remote images blocked")
        }
        let headline = report.trackerCount == 1
            ? String(localized: "1 tracker blocked")
            : String(localized: "\(report.trackerCount) trackers blocked")
        let names = report.services.prefix(3).map(\.name)
        guard !names.isEmpty else { return headline }
        let more = report.services.count > 3 ? "…" : ""
        return "\(headline) (\(names.joined(separator: ", "))\(more))"
    }

    /// Tooltip: every host the message tried to reach, with counts.
    static func detail(for report: RemoteContentReport?) -> String? {
        guard let report, !report.hosts.isEmpty else { return nil }
        return report.hosts.map { "\($0.host) × \($0.count)" }.joined(separator: "\n")
    }
}
