// Castellum - macOS Contacts.framework source for address autocomplete (S47).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Reads (name, email) pairs from the user's macOS Contacts, once permission
// is granted, for AddressCompleter to rank alongside locally-seen mail
// correspondents (Contact, from CastellumCore.listContacts).
//
// Never nags: `.notDetermined` requests access exactly once (the caller
// decides WHEN — see AddressAutocompleteField's first-keystroke trigger);
// `.denied`/`.restricted` silently yields no suggestions from this source.
//
// Never blocks the typist: `CNContactStore.enumerateContacts` walks the whole
// address book synchronously, which on a large one is far longer than a
// keystroke, so it runs OFF the main actor and only the finished array hops
// back. The first few keystrokes then complete from local correspondents
// alone and Contacts joins in when the read lands — which is the right
// trade: an autocomplete that stutters is worse than one that warms up.
//
// Contacts data is never logged or exported (Global Constraints).

import Contacts

/// One (name, email) pair read from macOS Contacts.
struct SystemContactEntry: Equatable, Hashable, Sendable {
    var name: String?
    var email: String
}

@MainActor
final class SystemContactsSource {
    static let shared = SystemContactsSource()

    private(set) var entries: [SystemContactEntry] = []
    private var hasLoaded = false
    private var isLoading = false

    /// Current authorization state, read fresh every call (never cached —
    /// the user can change it in System Settings > Privacy at any time).
    var authorizationStatus: CNAuthorizationStatus {
        CNContactStore.authorizationStatus(for: .contacts)
    }

    /// Ensures permission has been asked for (via the OS's own one-shot
    /// `.notDetermined` prompt) and, once authorized, that `entries` is
    /// populated. Call this on the FIRST keystroke in a To/Cc/Bcc field,
    /// never at app launch. Safe to call repeatedly — a granted/denied
    /// decision is never re-prompted by macOS itself, and a load already in
    /// flight is not started twice.
    func prepareIfNeeded() {
        if TestHost.isUIScript { return }   // UIT-1: never raise a Contacts prompt
        switch authorizationStatus {
        case .notDetermined:
            // A fresh store per call: CNContactStore is not documented as
            // safe to share across threads, and it is cheap to make.
            CNContactStore().requestAccess(for: .contacts) { [weak self] granted, _ in
                guard granted else { return }
                Task { @MainActor in self?.startLoad() }
            }
        case .authorized:
            startLoad()
        case .denied, .restricted:
            break // silently: no suggestions from this source
        @unknown default:
            break
        }
    }

    /// Kicks off the off-main read once, then publishes the result on the
    /// main actor. A read failure (corrupt store, permission revoked
    /// mid-read) leaves `entries` at whatever it last successfully held and
    /// allows a later retry; a missing autocomplete source is never worth
    /// interrupting compose with an alert.
    private func startLoad() {
        guard !hasLoaded, !isLoading else { return }
        isLoading = true
        Task.detached(priority: .utility) {
            let loaded = Self.readAllEntries()
            await MainActor.run {
                Self.shared.isLoading = false
                guard let loaded else { return }
                Self.shared.entries = loaded
                Self.shared.hasLoaded = true
            }
        }
    }

    /// Reads every contact's name + email addresses. Runs off the main actor;
    /// returns nil if the store threw, so the caller can tell "no contacts"
    /// from "could not read".
    private nonisolated static func readAllEntries() -> [SystemContactEntry]? {
        let request = CNContactFetchRequest(keysToFetch: [
            CNContactGivenNameKey as CNKeyDescriptor,
            CNContactFamilyNameKey as CNKeyDescriptor,
            CNContactOrganizationNameKey as CNKeyDescriptor,
            CNContactEmailAddressesKey as CNKeyDescriptor,
        ])
        var loaded: [SystemContactEntry] = []
        do {
            try CNContactStore().enumerateContacts(with: request) { contact, _ in
                let personName = [contact.givenName, contact.familyName]
                    .filter { !$0.isEmpty }
                    .joined(separator: " ")
                let displayName = personName.isEmpty
                    ? (contact.organizationName.isEmpty ? nil : contact.organizationName)
                    : personName
                for labeled in contact.emailAddresses {
                    let email = String(labeled.value)
                    guard !email.isEmpty else { continue }
                    loaded.append(SystemContactEntry(name: displayName, email: email))
                }
            }
            return loaded
        } catch {
            return nil
        }
    }
}
