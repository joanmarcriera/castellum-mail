// Castellum - Focus filter (S59): limit notifications and the Dock badge to
// chosen accounts, or to VIPs, while a Focus is on.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// An App Intents `SetFocusFilterIntent` in the main app target (macOS 13+).
// The system runs `perform()` in the app process, launching it in the
// background if needed, both when the Focus turns on and when it turns off (the
// parameters then hold their defaults, which reproduce `FocusFilterState.inactive`).
// The intent only writes a small state struct to UserDefaults; the notification
// policy and the Dock badge read it. It never opens the Rust core, so the
// account list the filter UI offers is a snapshot AppModel writes whenever
// `accounts` changes (`FocusAccountsSnapshot`).
// Set it up in: System Settings > Focus > (a Focus) > Add Filter > Castellum.

import AppIntents
import Foundation

/// What the active Focus allows. `allowedAccountIds == nil` means every account.
struct FocusFilterState: Codable, Equatable {
    var allowedAccountIds: Set<String>?
    var vipsOnly: Bool
    var showDockBadge: Bool

    /// No Focus filter in force: every account, all mail, badge shown.
    static let inactive = FocusFilterState(
        allowedAccountIds: nil, vipsOnly: false, showDockBadge: true)
}

extension Notification.Name {
    /// Posted (on the main thread's default centre) after the Focus filter
    /// state changes, so the Dock badge can recompute immediately.
    static let castellumFocusFilterChanged = Notification.Name("castellumFocusFilterChanged")
}

/// UserDefaults-backed persistence of `FocusFilterState`.
enum FocusFilterStore {
    static let key = "focusFilterState"

    static func load(defaults: UserDefaults = .standard) -> FocusFilterState {
        guard let data = defaults.data(forKey: key),
              let state = try? JSONDecoder().decode(FocusFilterState.self, from: data)
        else { return .inactive }
        return state
    }

    static func save(_ state: FocusFilterState, defaults: UserDefaults = .standard) {
        if let data = try? JSONEncoder().encode(state) {
            defaults.set(data, forKey: key)
        }
        NotificationCenter.default.post(name: .castellumFocusFilterChanged, object: nil)
    }
}

/// An account as the Focus filter UI lists it.
struct AccountEntity: AppEntity {
    static var typeDisplayRepresentation: TypeDisplayRepresentation = "Mail Account"
    static var defaultQuery = AccountEntityQuery()

    let id: String
    let label: String

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(label)")
    }
}

/// The account list, snapshotted to UserDefaults by AppModel so the intent
/// never has to open the Rust core from the system's intent context.
enum FocusAccountsSnapshot {
    static let key = "focusAccountsSnapshot"

    private struct Row: Codable { let id: String; let label: String }

    static func save(_ accounts: [(id: String, label: String)], defaults: UserDefaults = .standard) {
        let rows = accounts.map { Row(id: $0.id, label: $0.label) }
        if let data = try? JSONEncoder().encode(rows) { defaults.set(data, forKey: key) }
    }

    static func load(defaults: UserDefaults = .standard) -> [AccountEntity] {
        guard let data = defaults.data(forKey: key),
              let rows = try? JSONDecoder().decode([Row].self, from: data)
        else { return [] }
        return rows.map { AccountEntity(id: $0.id, label: $0.label) }
    }
}

struct AccountEntityQuery: EntityQuery {
    func entities(for identifiers: [String]) async throws -> [AccountEntity] {
        FocusAccountsSnapshot.load().filter { identifiers.contains($0.id) }
    }

    func suggestedEntities() async throws -> [AccountEntity] {
        FocusAccountsSnapshot.load()
    }
}

struct CastellumFocusFilter: SetFocusFilterIntent {
    static var title: LocalizedStringResource = "Set Castellum Filter"
    static var description: IntentDescription? =
        "Choose which accounts can notify you and badge the Dock while this Focus is on."

    @Parameter(title: "Accounts") var accounts: [AccountEntity]?
    @Parameter(title: "Only VIPs", default: false) var vipsOnly: Bool
    @Parameter(title: "Show Dock badge", default: true) var showDockBadge: Bool

    var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(
            title: "\(accounts.map { "\($0.count) account(s)" } ?? "All accounts")",
            subtitle: vipsOnly ? "VIPs only" : nil)
    }

    /// The complete state this intent's parameters describe. Split out so it
    /// is testable without the App Intents runtime.
    var state: FocusFilterState {
        FocusFilterState(
            allowedAccountIds: accounts.map { Set($0.map(\.id)) },
            vipsOnly: vipsOnly, showDockBadge: showDockBadge)
    }

    func perform() async throws -> some IntentResult {
        // Called on Focus ON and OFF (off = parameter defaults), so this always
        // writes the complete state.
        FocusFilterStore.save(state)
        return .result()
    }
}
