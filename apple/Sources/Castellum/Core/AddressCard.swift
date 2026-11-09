// Castellum - what the address hover card in the reader header shows.
//
// Pure: built from the header name/address, the contact row the core keeps for that address
// (counts and dates come from `refreshContacts`, i.e. from mail already stored locally), and
// whether the address is a VIP. No network: a hover never fetches anything.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

struct AddressCardContent: Equatable {
    /// The name shown big: the user's override, else the header name, else the address.
    let title: String
    let address: String
    let isVip: Bool
    /// The user's own address (an account or identity): the exchange counts are hidden then.
    let isSelf: Bool
    let received: Int64
    let sent: Int64
    let firstSeen: Date?
    let lastSeen: Date?
    let notes: String?

    /// "12 received · 3 sent", or nil when there is nothing known (no contact row yet) or it is me.
    var exchangeSummary: String? {
        guard !isSelf, received > 0 || sent > 0 else { return nil }
        let receivedText = String(localized: "\(Int(received)) received")
        let sentText = String(localized: "\(Int(sent)) sent")
        return "\(receivedText) · \(sentText)"
    }

    /// `name` is the header's display name; `contact` the core's row for this address (nil when
    /// the address has no contact row yet); `myAddresses` lower-cased.
    static func make(
        name: String?, address: String, contact: Contact?, isVip: Bool, myAddresses: Set<String>
    ) -> AddressCardContent {
        let key = address.trimmingCharacters(in: .whitespaces).lowercased()
        let override = contact?.displayName?.trimmingCharacters(in: .whitespacesAndNewlines)
        let headerName = name?.trimmingCharacters(in: .whitespacesAndNewlines)
        let title = [override, headerName, contact?.derivedName?.trimmingCharacters(in: .whitespacesAndNewlines)]
            .compactMap { $0 }.first { !$0.isEmpty } ?? address
        let notes = contact?.notes.trimmingCharacters(in: .whitespacesAndNewlines)
        func date(_ epoch: Int64?) -> Date? { epoch.map { Date(timeIntervalSince1970: TimeInterval($0)) } }
        return AddressCardContent(
            title: title, address: address, isVip: isVip,
            isSelf: (contact?.isSelf ?? false) || myAddresses.contains(key),
            received: contact?.inboundCount ?? 0, sent: contact?.outboundCount ?? 0,
            firstSeen: date(contact?.firstSeenAt), lastSeen: date(contact?.lastSeenAt),
            notes: (notes?.isEmpty ?? true) ? nil : notes)
    }
}

/// Timing of the hover card, so the rules are testable: it opens after a short rest on the
/// address (not while the pointer sweeps across the header) and closes shortly after the pointer
/// has left both the address and the card.
enum AddressCardTiming {
    static let openDelay: TimeInterval = 0.6
    static let closeDelay: TimeInterval = 0.3
}
