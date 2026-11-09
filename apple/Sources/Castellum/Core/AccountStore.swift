// Castellum - Keychain-backed credential store for IMAP accounts.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Persists per-account connection config (INCLUDING the password) in the macOS
// Keychain, and nowhere else. The Rust core stays credential-agnostic: it only
// ever receives config JSON per sync/replay call and never writes a secret to
// the SQLite DB or disk in plaintext. One generic-password item per account; the
// stored secret is the full connection-config JSON that sync/replay pass down.
// The blob shape is OAuth-ready — tokens replace the password field later.

import CryptoKit
import Foundation
import Security

struct AccountStore {
    /// Keychain service namespace. Overridable so tests use an isolated namespace.
    private let service: String
    /// The four SecItem* primitives, behind a protocol. See `KeychainBacking.swift`.
    private let keychain: KeychainBacking
    /// Per-process read cache, keyed by account id. See `ConfigCache`.
    private let cache = ConfigCache()

    /// The real profile's namespace. Accounts created by shipped builds live here.
    static let defaultService = "com.castellum.account"

    /// `keychain`'s default is the actual fix for: under
    /// XCTest OR the S64 GUI launch probe (`TestHost.usesInMemoryKeychain`)
    /// it routes to the process-wide in-memory backing, so no unit test —
    /// this one, `AccountStoreTests`, `AccountReconnectTests`, or any future
    /// test that just calls `AccountStore(service:)` or builds an `AppModel`
    /// with a custom profile — can reach a real `SecItem*` call and hang.
    /// Production (`TestHost.usesInMemoryKeychain == false`) is unaffected:
    /// it gets a fresh `RealKeychainBacking()`, a pure extraction of the
    /// calls this file made inline before. The one deliberate exception is
    /// `AccountStoreRealKeychainTests`, which passes `RealKeychainBacking()`
    /// explicitly to keep the real path covered.
    init(
        service: String = AccountStore.defaultService,
        keychain: KeychainBacking = TestHost.usesInMemoryKeychain
            ? InMemoryKeychainBacking.shared : RealKeychainBacking()
    ) {
        self.service = service
        self.keychain = keychain
    }

    /// The Keychain namespace this store reads and writes.
    var serviceName: String { service }

    /// Keychain namespace for a profile. The default profile (`nil`) keeps
    /// `defaultService`; any other profile directory (unit tests,
    /// `CASTELLUM_PROFILE_DIR` smokes) gets its own namespace derived from its
    /// path. Without this, a test's temp-profile AppModel wrote accounts into
    /// the real namespace, and the shipped app then tried to sync them at
    /// launch: one login-keychain password prompt per leftover item, each
    /// pointing at 127.0.0.1:3143 (2026-09-17, 0.3.5).
    static func service(forProfileDirectory dir: URL?) -> String {
        guard let dir else { return defaultService }
        let path = dir.standardizedFileURL.path
        let digest = SHA256.hash(data: Data(path.utf8))
        let hex = digest.prefix(8).map { String(format: "%02x", $0) }.joined()
        return "\(defaultService).profile.\(hex)"
    }

    private func baseQuery(accountID: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: accountID,
        ]
    }

    /// Store (or replace) the connection-config JSON for an account.
    func save(accountID: String, configJSON: String) throws {
        let data = Data(configJSON.utf8)
        // Update in place if present; otherwise add. Keep the accessibility
        // attribute on both paths: device-only, available only while unlocked.
        let update: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        let status = keychain.update(baseQuery(accountID: accountID) as CFDictionary,
                                     attributesToUpdate: update as CFDictionary)
        switch status {
        case errSecSuccess:
            cache.set(configJSON, for: accountID)
        case errSecItemNotFound:
            var add = baseQuery(accountID: accountID)
            add[kSecValueData as String] = data
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            let addStatus = keychain.add(add as CFDictionary)
            guard addStatus == errSecSuccess else { throw KeychainError.status(addStatus) }
            cache.set(configJSON, for: accountID)
        default:
            throw KeychainError.status(status)
        }
    }

    /// The stored config JSON for an account, or `nil` if none is stored.
    ///
    /// Reads the Keychain at most once per account per process: the item is
    /// read by sync, replay, status seeding and the compose From picker, and
    /// every read a binary the item's ACL does not trust yet triggers a
    /// "Castellum wants to use your confidential information" prompt — seven
    /// at launch on 0.3.2. The cache is the fix for the storm; the Keychain
    /// stays the source of truth (`save`/`delete` write through).
    func configJSON(for accountID: String) throws -> String? {
        if let cached = cache.value(for: accountID) { return cached }
        let json = try readConfigJSON(for: accountID)
        if let json { cache.set(json, for: accountID) }
        return json
    }

    private func readConfigJSON(for accountID: String) throws -> String? {
        var query = baseQuery(accountID: accountID)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        let status = keychain.copyMatching(query as CFDictionary, result: &item)
        switch status {
        case errSecItemNotFound:
            return nil
        case errSecSuccess:
            guard let data = item as? Data, let json = String(data: data, encoding: .utf8) else {
                throw KeychainError.decode
            }
            return json
        default:
            throw KeychainError.status(status)
        }
    }

    /// All account ids that have stored credentials (for launch-time re-sync).
    func allAccountIDs() throws -> [String] {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecReturnAttributes as String: true,
            kSecMatchLimit as String: kSecMatchLimitAll,
        ]
        var items: CFTypeRef?
        let status = keychain.copyMatching(query as CFDictionary, result: &items)
        switch status {
        case errSecItemNotFound:
            return []
        case errSecSuccess:
            guard let arr = items as? [[String: Any]] else { throw KeychainError.decode }
            return arr.compactMap { $0[kSecAttrAccount as String] as? String }
        default:
            throw KeychainError.status(status)
        }
    }

    /// Remove every item in this store's namespace. For tests and disposable
    /// profiles; never called on the default namespace by the app.
    func deleteAll() throws {
        for id in try allAccountIDs() {
            try delete(accountID: id)
        }
    }

    /// Remove an account's stored credentials. A missing item is not an error.
    func delete(accountID: String) throws {
        cache.remove(accountID)
        let status = keychain.delete(baseQuery(accountID: accountID) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainError.status(status)
        }
    }
}

/// Thread-safe in-memory copy of each account's config JSON. A reference type
/// so the value-type `AccountStore` copies all share it; guarded by a lock
/// because sync and replay read from detached tasks while the UI saves on
/// the main actor. Never persisted — it lives exactly as long as the process.
private final class ConfigCache: @unchecked Sendable {
    private var values: [String: String] = [:]
    private let lock = NSLock()

    func value(for accountID: String) -> String? {
        lock.lock(); defer { lock.unlock() }
        return values[accountID]
    }

    func set(_ json: String, for accountID: String) {
        lock.lock(); defer { lock.unlock() }
        values[accountID] = json
    }

    func remove(_ accountID: String) {
        lock.lock(); defer { lock.unlock() }
        values[accountID] = nil
    }
}

enum KeychainError: Error, CustomStringConvertible {
    case status(OSStatus)
    case decode

    var description: String {
        switch self {
        case .status(let s):
            let msg = (SecCopyErrorMessageString(s, nil) as String?) ?? "unknown error"
            return "Keychain error \(s): \(msg)"
        case .decode:
            return "Keychain returned data that was not valid UTF-8 config"
        }
    }
}
