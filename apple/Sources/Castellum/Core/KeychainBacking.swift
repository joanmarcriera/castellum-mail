// Castellum - the four Security-framework primitives AccountStore depends on,
// extracted behind a protocol so XCTest never has to call the real Keychain.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
//: `AccountDuplicateSaveTests.tearDown()` hung inside
// `SecItemDelete` — no visible auth prompt, just a genuine block (ACL/lock
// contention or trust re-evaluation) against the real login keychain. The
// 2026-09-17 namespacing fix (`AccountStore.service(forProfileDirectory:)`)
// isolates *which* items a test touches but not *whether* the real Keychain
// is touched at all, so the hang risk stayed. This protocol is the actual
// fix: `AccountStore` never calls `SecItem*` directly any more, so under
// XCTest it is architecturally impossible for it to reach the real Keychain
// (see the default-parameter wiring in `AccountStore.init`).

import Foundation
import Security

/// Query/attribute dictionaries are exactly what `AccountStore.baseQuery`
/// already builds — this is a pure seam, not a redesign of the Keychain API.
protocol KeychainBacking {
    func add(_ attributes: CFDictionary) -> OSStatus
    func update(_ query: CFDictionary, attributesToUpdate: CFDictionary) -> OSStatus
    func copyMatching(_ query: CFDictionary, result: inout CFTypeRef?) -> OSStatus
    func delete(_ query: CFDictionary) -> OSStatus
}

/// Thin wrapper over the real `SecItem*` calls. Behaviourally identical to
/// what `AccountStore` called inline before this refactor — production code
/// path, unchanged.
struct RealKeychainBacking: KeychainBacking {
    func add(_ attributes: CFDictionary) -> OSStatus {
        SecItemAdd(attributes, nil)
    }

    func update(_ query: CFDictionary, attributesToUpdate: CFDictionary) -> OSStatus {
        SecItemUpdate(query, attributesToUpdate)
    }

    func copyMatching(_ query: CFDictionary, result: inout CFTypeRef?) -> OSStatus {
        SecItemCopyMatching(query, &result)
    }

    func delete(_ query: CFDictionary) -> OSStatus {
        SecItemDelete(query)
    }
}

/// In-memory reproduction of just the Keychain semantics `AccountStore`
/// depends on: existence keyed by `(service, account)`, and the
/// errSecSuccess/errSecItemNotFound/errSecDuplicateItem status contract
/// `AccountStore.save` branches on to decide add-vs-update. NOT a general
/// SecItem* stub — only the query/result shapes `AccountStore`'s own code
/// builds (`kSecAttrService`/`kSecAttrAccount` scoped lookups, single-item
/// `kSecReturnData`, and the `allAccountIDs()` all-items-in-service listing).
///
/// One process-wide instance (`.shared`), not per-`AccountStore`: several
/// tests deliberately build two `AccountStore`s against the same `service`
/// string and expect them to observe each other's writes (e.g.
/// `AccountStoreTests.testCacheFollowsKeychain`) — exactly what two
/// `AccountStore`s pointed at the real Keychain do today, since the real
/// Keychain is itself a single process/system-wide resource keyed by
/// service. A fresh instance per `AccountStore` would break that.
final class InMemoryKeychainBacking: KeychainBacking, @unchecked Sendable {
    static let shared = InMemoryKeychainBacking()

    private var itemsByService: [String: [String: Data]] = [:]
    private let lock = NSLock()

    private static func string(_ dict: CFDictionary, _ key: CFString) -> String? {
        (dict as NSDictionary)[key as String] as? String
    }

    private static func data(_ dict: CFDictionary, _ key: CFString) -> Data? {
        (dict as NSDictionary)[key as String] as? Data
    }

    private static func bool(_ dict: CFDictionary, _ key: CFString) -> Bool {
        (dict as NSDictionary)[key as String] as? Bool ?? false
    }

    func add(_ attributes: CFDictionary) -> OSStatus {
        guard let service = Self.string(attributes, kSecAttrService),
              let account = Self.string(attributes, kSecAttrAccount),
              let data = Self.data(attributes, kSecValueData)
        else { return errSecParam }
        lock.lock(); defer { lock.unlock() }
        if itemsByService[service]?[account] != nil { return errSecDuplicateItem }
        itemsByService[service, default: [:]][account] = data
        return errSecSuccess
    }

    func update(_ query: CFDictionary, attributesToUpdate: CFDictionary) -> OSStatus {
        guard let service = Self.string(query, kSecAttrService),
              let account = Self.string(query, kSecAttrAccount)
        else { return errSecParam }
        lock.lock(); defer { lock.unlock() }
        guard itemsByService[service]?[account] != nil else { return errSecItemNotFound }
        guard let data = Self.data(attributesToUpdate, kSecValueData) else { return errSecParam }
        itemsByService[service]?[account] = data
        return errSecSuccess
    }

    func copyMatching(_ query: CFDictionary, result: inout CFTypeRef?) -> OSStatus {
        guard let service = Self.string(query, kSecAttrService) else { return errSecParam }
        lock.lock(); defer { lock.unlock() }
        let bucket = itemsByService[service] ?? [:]
        if let account = Self.string(query, kSecAttrAccount) {
            // `configJSON(for:)`-style single-item lookup.
            guard let data = bucket[account] else { return errSecItemNotFound }
            if Self.bool(query, kSecReturnData) { result = data as CFTypeRef }
            return errSecSuccess
        }
        // `allAccountIDs()`-style: every item in this service.
        guard !bucket.isEmpty else { return errSecItemNotFound }
        if Self.bool(query, kSecReturnAttributes) {
            let attrs: [[String: Any]] = bucket.keys.map { [kSecAttrAccount as String: $0] }
            result = attrs as CFTypeRef
        }
        return errSecSuccess
    }

    func delete(_ query: CFDictionary) -> OSStatus {
        guard let service = Self.string(query, kSecAttrService),
              let account = Self.string(query, kSecAttrAccount)
        else { return errSecParam }
        lock.lock(); defer { lock.unlock() }
        guard itemsByService[service]?[account] != nil else { return errSecItemNotFound }
        itemsByService[service]?[account] = nil
        return errSecSuccess
    }
}
