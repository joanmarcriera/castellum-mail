// Castellum - user preferences (UserDefaults-backed).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Deploys to macOS 13, so we back @Published values with UserDefaults directly
// (rather than relying on @AppStorage in an ObservableObject). A shared singleton
// so both the Settings window and AppModel read the same live values.

import SwiftUI

/// When a message should be automatically marked read. The two behaviours Marc
/// asked for are mutually exclusive, so they're one mode rather than two toggles.
enum AutoMarkReadMode: String, CaseIterable, Identifiable {
    case never
    case onOpen
    case afterDelay

    var id: String { rawValue }
    var label: String {
        switch self {
        case .never: return String(localized: "Never")
        case .onOpen: return String(localized: "When opened")
        case .afterDelay: return String(localized: "After a delay")
        }
    }
}

/// How often Castellum checks the public GitHub releases endpoint for a
/// newer version. `never` disables checks entirely;
/// `atLaunch` checks on every launch with no additional gating; `daily`
/// (the default) checks at most once every 24 hours.
enum UpdateCheckSchedule: String, CaseIterable, Identifiable {
    case never
    case atLaunch
    case daily

    var id: String { rawValue }
    var label: String {
        switch self {
        case .never: return String(localized: "Never")
        case .atLaunch: return String(localized: "At launch")
        case .daily: return String(localized: "Daily")
        }
    }

    /// Settings caption for this schedule. Each value states its own network
    /// behaviour, so the privacy line never promises a daily check that Never
    /// or At launch doesn't make.
    var disclosure: String {
        switch self {
        case .never:
            return String(localized: "Never contacts github.com unless you choose Check for Updates…")
        case .atLaunch:
            return String(localized: "Checks github.com each time Castellum starts; sends no identifiers.")
        case .daily:
            return String(localized: "Checks github.com once a day; sends no identifiers.")
        }
    }
}

/// The undo-send grace period (S25): how long a queued outgoing message waits
/// before the sender is allowed to deliver it, and for how long the compose
/// banner offers Undo. Raw value IS the delay in seconds, so `.seconds` needs
/// no separate mapping; `.off` (0) disables the whole feature — Send behaves
/// exactly as before.
enum UndoSendDelay: Int, CaseIterable, Identifiable {
    case off = 0
    case five = 5
    case ten = 10
    case twenty = 20
    case thirty = 30

    var id: Int { rawValue }
    var seconds: Int { rawValue }
    var label: String {
        self == .off ? String(localized: "Off") : String(localized: "\(rawValue) seconds")
    }
}

/// Which new inbox mail raises a Notification Centre banner (S59).
enum NewMailNotifications: String, CaseIterable, Identifiable {
    case all
    case vipsOnly
    case off

    var id: String { rawValue }
    var label: String {
        switch self {
        case .all: return String(localized: "All new mail")
        case .vipsOnly: return String(localized: "VIPs only")
        case .off: return String(localized: "Off")
        }
    }
}

/// What the Dock icon's red badge counts (S59). Independent of the
/// notification mode: turning banners off does not clear the badge.
enum DockBadgeMode: String, CaseIterable, Identifiable {
    case inboxUnread
    case vipUnread
    /// MM-9: the unread count of one smart mailbox (`dockBadgeSmartMailboxId`).
    case smartMailbox
    case off

    var id: String { rawValue }
    var label: String {
        switch self {
        case .inboxUnread: return String(localized: "Inbox unread")
        case .vipUnread: return String(localized: "VIP unread")
        case .smartMailbox: return String(localized: "Smart mailbox")
        case .off: return String(localized: "Off")
        }
    }
}

@MainActor
final class Preferences: ObservableObject {
    static let shared = Preferences()

    @Published var autoMarkReadMode: AutoMarkReadMode {
        didSet { defaults.set(autoMarkReadMode.rawValue, forKey: Keys.mode) }
    }
    /// Delay (seconds) before auto-marking read in `.afterDelay` mode.
    @Published var markReadDelaySeconds: Int {
        didSet { defaults.set(markReadDelaySeconds, forKey: Keys.delay) }
    }
    /// Load remote images automatically (tracking-pixel exposure). Default OFF;
    /// the reading pane offers per-message loading via the banner.
    @Published var alwaysLoadRemoteImages: Bool {
        didSet { defaults.set(alwaysLoadRemoteImages, forKey: Keys.remoteImages) }
    }
    /// Fold long quotes and signatures in the plain-text viewer (MM-17). Default ON; off = flat body.
    /// Changed with defaults write (no Settings row: the pane is at its height limit).
    @Published var foldQuotes: Bool {
        didSet { defaults.set(foldQuotes, forKey: Keys.foldQuotes) }
    }
    /// Re-wrap hard-wrapped prose in a reply quote (MM-17 decision M3). Default ON; hidden preference.
    @Published var quoteRewrapFixed: Bool {
        didSet { defaults.set(quoteRewrapFixed, forKey: Keys.quoteRewrapFixed) }
    }
    /// Whether the AI feature is active. Default OFF — AI is opt-in.
    @Published var aiEnabled: Bool {
        didSet { defaults.set(aiEnabled, forKey: Keys.aiEnabled) }
    }
    /// Whether to run bounded background body hydration after each sync.
    /// Default OFF: the pilot fetches headers/descriptors and hydrates a body
    /// only when the message is opened (body-on-open), so adding an account
    /// never triggers an unbounded background mailbox download. Turning this on
    /// enables the existing byte-budgeted background drain.
    @Published var backgroundBodyHydrationEnabled: Bool {
        didSet {
            defaults.set(backgroundBodyHydrationEnabled, forKey: Keys.backgroundBodyHydration)
        }
    }
    /// How often to check the public GitHub releases endpoint for updates.
    /// Default `.daily`; the request contains no Castellum installation
    /// identifier regardless of schedule.
    @Published var updateCheckSchedule: UpdateCheckSchedule {
        didSet {
            defaults.set(updateCheckSchedule.rawValue, forKey: Keys.updateCheckSchedule)
        }
    }
    /// List beside the message (widescreen, the original) or above it
    /// (classic). View menu + Settings > General (MM-4).
    @Published var viewerLayout: ViewerLayout {
        didSet { defaults.set(viewerLayout.rawValue, forKey: Keys.viewerLayout) }
    }
    /// How many of a message's headers the reader shows (MM-13): compact,
    /// extended (default) or all. View > Header + Settings > General.
    @Published var headerMode: MessageHeaderMode {
        didSet { defaults.set(headerMode.rawValue, forKey: Keys.headerMode) }
    }
    /// What a click on a downloaded attachment does (GUI-6); nil = ask on the
    /// next click and offer "Always do this". Changeable from the attachment
    /// strip's gear menu and Settings > General.
    @Published var attachmentDefaultAction: AttachmentDefaultAction? {
        didSet {
            if let action = attachmentDefaultAction {
                defaults.set(action.rawValue, forKey: Keys.attachmentDefaultAction)
            } else {
                defaults.removeObject(forKey: Keys.attachmentDefaultAction)
            }
        }
    }
    /// Default theme of the HTML alternative in Markdown mail (MM-5); a compose
    /// window can override it per message. Plain = the original unstyled output.
    @Published var markdownTheme: MarkdownThemeChoice {
        didSet { defaults.set(markdownTheme.rawValue, forKey: Keys.markdownTheme) }
    }
    /// Undo-send grace period (S25). Default 10 seconds, next to the other
    /// compose/mail options.
    @Published var undoSendDelay: UndoSendDelay {
        didSet { defaults.set(undoSendDelay.rawValue, forKey: Keys.undoSendDelay) }
    }
    /// MM-17 S2: send plain-text mail as RFC 3676 `format=flowed` (soft line
    /// breaks at 76 columns that flowed-aware clients re-flow). Default ON
    /// (decision M1); applies only to plain-text sends, never to Markdown mail.
    @Published var sendFlowed: Bool {
        didSet { defaults.set(sendFlowed, forKey: Keys.sendFlowed) }
    }
    /// HP5a: ask before sending a message that mentions an attachment
    /// ("attached", "adjunto", "adjunt"...) when nothing is attached. Default on.
    @Published var warnAboutForgottenAttachment: Bool {
        didSet { defaults.set(warnAboutForgottenAttachment, forKey: Keys.warnAboutForgottenAttachment) }
    }
    /// MM-14 (M1 default): look up server settings for an unknown domain in the
    /// Add-account sheet (DNS, the domain's own autoconfig, Mozilla ISPDB).
    /// Only the DOMAIN is ever sent. Off = no lookup call at all. Default on.
    @Published var autoconfigLookupEnabled: Bool {
        didSet { defaults.set(autoconfigLookupEnabled, forKey: Keys.autoconfigLookupEnabled) }
    }
    /// The start time of the most recent manual or automatic update attempt.
    @Published var lastUpdateCheck: Date? {
        didSet {
            if let lastUpdateCheck {
                defaults.set(lastUpdateCheck, forKey: Keys.lastUpdateCheck)
            } else {
                defaults.removeObject(forKey: Keys.lastUpdateCheck)
            }
        }
    }
    /// Base URL of the Ollama endpoint (local network only).
    @Published var aiEndpoint: String {
        didSet { defaults.set(aiEndpoint, forKey: Keys.aiEndpoint) }
    }
    /// Ollama model to use for all AI features. Empty string = not yet chosen.
    @Published var aiModel: String {
        didSet { defaults.set(aiModel, forKey: Keys.aiModel) }
    }
    /// Ollama embedding model for background indexing and Find Similar (F1).
    /// Empty = none chosen: nothing is embedded in the background. Kept apart
    /// from `aiModel` because a chat model cannot embed (Ollama answers 500).
    @Published var aiEmbeddingModel: String {
        didSet { defaults.set(aiEmbeddingModel, forKey: Keys.aiEmbeddingModel) }
    }
    /// One-line disclosure shown under the AI toggle when the first-launch
    /// probe turned AI on automatically. Deliberately NOT
    /// persisted — it explains what just happened this launch, not a
    /// standing fact restated on every later Settings visit.
    @Published var aiAutoEnableReason: String?
    /// The endpoint the first-launch probe found Ollama at, for the
    /// "Detected at …" caption under the Endpoint field. Also session-only.
    @Published var aiAutoDetectedEndpoint: String?
    /// Named plain-text signatures, several per account (S26, GUI-4), edited
    /// in the Signatures window. Presentation-side only: JSON in
    /// `UserDefaults` (not the Keychain, not synced, never sent to the Rust
    /// core) — see DECISIONS.md. Pre-GUI-4 profiles stored one text per
    /// account under `accountSignatures`; that is migrated on first load.
    @Published var signatureLibrary: SignatureLibrary {
        didSet {
            if let data = try? JSONEncoder().encode(signatureLibrary) {
                defaults.set(data, forKey: Keys.signatureLibrary)
            }
        }
    }
    /// Per-account reply attribution formats (MM-7). JSON in UserDefaults like
    /// `signatureLibrary`; the core never sees it.
    @Published var templateLibrary: TemplateLibrary {
        didSet {
            if let data = try? JSONEncoder().encode(templateLibrary) {
                defaults.set(data, forKey: Keys.templateLibrary)
            }
        }
    }
    /// Per-account Markdown compose default (S46), edited in Settings >
    /// Accounts: `true` = new compose windows for that account start with
    /// Markdown on. A single message can still flip `ComposeSession.
    /// markdownEnabled` for itself without changing this default. Keyed by
    /// account id; an account with no entry defaults to `false` (plain
    /// text) — every profile that predates S46 behaves exactly as before.
    @Published var markdownDefaults: [String: Bool] {
        didSet { defaults.set(markdownDefaults, forKey: Keys.markdownDefaults) }
    }

    /// New-mail banners (S59). Default: all new inbox mail.
    @Published var newMailNotifications: NewMailNotifications {
        didSet { defaults.set(newMailNotifications.rawValue, forKey: Keys.newMailNotifications) }
    }
    /// Include the message snippet in a banner. Default on; notification
    /// content never leaves this Mac either way.
    @Published var notificationsShowPreview: Bool {
        didSet { defaults.set(notificationsShowPreview, forKey: Keys.notificationsShowPreview) }
    }
    @Published var notificationsPlaySound: Bool {
        didSet { defaults.set(notificationsPlaySound, forKey: Keys.notificationsPlaySound) }
    }
    /// Accounts that never raise a banner (empty = every account notifies).
    @Published var notificationsExcludedAccountIds: Set<String> {
        didSet {
            defaults.set(Array(notificationsExcludedAccountIds).sorted(), forKey: Keys.notificationsExcludedAccountIds)
        }
    }
    @Published var dockBadgeMode: DockBadgeMode {
        didSet { defaults.set(dockBadgeMode.rawValue, forKey: Keys.dockBadgeMode) }
    }
    /// The smart mailbox the Dock badge counts in `.smartMailbox` mode (MM-9).
    @Published var dockBadgeSmartMailboxId: String? {
        didSet { defaults.set(dockBadgeSmartMailboxId, forKey: Keys.dockBadgeSmartMailboxId) }
    }
    /// What the menu bar counter shows (MM-9). Independent of the Dock badge;
    /// `.off` (the default, T11) means no status item exists.
    @Published var menuBarCounterMode: DockBadgeMode {
        didSet { defaults.set(menuBarCounterMode.rawValue, forKey: Keys.menuBarCounterMode) }
    }
    @Published var menuBarSmartMailboxId: String? {
        didSet { defaults.set(menuBarSmartMailboxId, forKey: Keys.menuBarSmartMailboxId) }
    }
    /// Per-account notification settings by account id (MM-9, JSON). Slice 1 uses
    /// only `countsInCounters`. Rules of deleted accounts are inert, never pruned.
    @Published var notificationAccountRules: [String: AccountNotificationRule] {
        didSet {
            if let data = try? JSONEncoder().encode(notificationAccountRules) {
                defaults.set(data, forKey: Keys.notificationAccountRules)
            }
        }
    }

    /// The rule for one account (defaults when none is stored).
    func accountRule(_ accountId: String) -> AccountNotificationRule {
        notificationAccountRules[accountId] ?? AccountNotificationRule()
    }

    /// Include or leave out an account in the Dock and menu bar counters. Setting
    /// "include" on a default rule removes the entry rather than storing noise.
    func setCountsInCounters(_ include: Bool, accountId: String) {
        var rule = accountRule(accountId)
        rule.countsInCounters = include
        if rule == AccountNotificationRule() {
            notificationAccountRules.removeValue(forKey: accountId)
        } else {
            notificationAccountRules[accountId] = rule
        }
    }

    /// A smart mailbox was deleted: drop it from both counter choices. The mode stays
    /// `.smartMailbox`, which counts the inbox until the user picks another.
    func forgetSmartMailbox(_ id: String) {
        if dockBadgeSmartMailboxId == id { dockBadgeSmartMailboxId = nil }
        if menuBarSmartMailboxId == id { menuBarSmartMailboxId = nil }
    }

    var dockScope: CounterScope {
        CounterScope(mode: dockBadgeMode, smartMailboxId: dockBadgeSmartMailboxId)
    }
    var menuBarScope: CounterScope {
        CounterScope(mode: menuBarCounterMode, smartMailboxId: menuBarSmartMailboxId)
    }

    private let defaults: UserDefaults

    // MARK: Move-to recents (context menu slice 2)

    /// Most recent "Move to" destinations kept per account. A presentation
    /// convenience only: nothing here is durable mail state.
    static let maxRecentMoveDestinations = 5

    /// Recent destination mailbox ids for `accountId`, newest first.
    func recentMoveDestinations(accountId: String) -> [String] {
        let all = defaults.dictionary(forKey: Keys.recentMoveDestinations) as? [String: [String]]
        return all?[accountId] ?? []
    }

    /// Record `mailboxId` as the newest destination of `accountId` (deduplicated, bounded).
    func noteMoveDestination(_ mailboxId: String, accountId: String) {
        var all = (defaults.dictionary(forKey: Keys.recentMoveDestinations) as? [String: [String]]) ?? [:]
        all[accountId] = Self.noting(mailboxId, in: all[accountId] ?? [])
        defaults.set(all, forKey: Keys.recentMoveDestinations)
    }

    /// Forget one account's recents (account removed; tests).
    func clearRecentMoveDestinations(accountId: String) {
        var all = (defaults.dictionary(forKey: Keys.recentMoveDestinations) as? [String: [String]]) ?? [:]
        all[accountId] = nil
        defaults.set(all, forKey: Keys.recentMoveDestinations)
    }

    /// Pure list rule behind `noteMoveDestination`: newest first, no duplicates, at most 5.
    nonisolated static func noting(_ id: String, in list: [String]) -> [String] {
        Array(([id] + list.filter { $0 != id }).prefix(maxRecentMoveDestinations))
    }

    private enum Keys {
        static let recentMoveDestinations = "recentMoveDestinations"
        static let mode = "autoMarkReadMode"
        static let delay = "markReadDelaySeconds"
        static let remoteImages = "alwaysLoadRemoteImages"
        static let foldQuotes = "foldQuotes"
        static let quoteRewrapFixed = "quoteRewrapFixed"
        static let aiEnabled = "aiEnabled"
        static let aiEndpoint = "aiEndpoint"
        static let aiModel = "aiModel"
        static let aiEmbeddingModel = "aiEmbeddingModel"
        static let backgroundBodyHydration = "backgroundBodyHydrationEnabled"
        static let updateCheckSchedule = "updateCheckSchedule"
        static let undoSendDelay = "undoSendDelay"
        static let sendFlowed = "sendFlowed"
        static let warnAboutForgottenAttachment = "warnAboutForgottenAttachment"
        static let autoconfigLookupEnabled = "autoconfigLookupEnabled"
        static let viewerLayout = "viewerLayout"
        static let headerMode = "messageHeaderMode"
        static let attachmentDefaultAction = "attachmentDefaultAction"
        static let markdownTheme = "markdownTheme"
        /// Legacy (pre-GUI-4) one-text-per-account dictionary; read once to migrate.
        static let signatures = "accountSignatures"
        static let signatureLibrary = "signatureLibrary"
        static let templateLibrary = "templateLibrary"
        static let markdownDefaults = "accountMarkdownDefaults"
        static let newMailNotifications = "newMailNotifications"
        static let notificationsShowPreview = "notificationsShowPreview"
        static let notificationsPlaySound = "notificationsPlaySound"
        static let notificationsExcludedAccountIds = "notificationsExcludedAccountIds"
        static let dockBadgeMode = "dockBadgeMode"
        static let dockBadgeSmartMailboxId = "dockBadgeSmartMailboxId"
        static let menuBarCounterMode = "menuBarCounterMode"
        static let menuBarSmartMailboxId = "menuBarSmartMailboxId"
        static let notificationAccountRules = "notificationAccountRules"
        /// Pre-#2179 Bool preference key, read once at init for migration only.
        static let legacyCheckForUpdatesAutomatically = "checkForUpdatesAutomatically"
        static let lastUpdateCheck = "lastUpdateCheck"
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        autoMarkReadMode = AutoMarkReadMode(rawValue: defaults.string(forKey: Keys.mode) ?? "")
            ?? .onOpen
        markReadDelaySeconds = (defaults.object(forKey: Keys.delay) as? Int) ?? 2
        alwaysLoadRemoteImages = defaults.bool(forKey: Keys.remoteImages)
        foldQuotes = (defaults.object(forKey: Keys.foldQuotes) as? Bool) ?? true
        quoteRewrapFixed = (defaults.object(forKey: Keys.quoteRewrapFixed) as? Bool) ?? true
        aiEnabled = defaults.bool(forKey: Keys.aiEnabled)
        aiEndpoint = defaults.string(forKey: Keys.aiEndpoint) ?? "http://127.0.0.1:11434"
        aiModel = defaults.string(forKey: Keys.aiModel) ?? ""
        aiEmbeddingModel = defaults.string(forKey: Keys.aiEmbeddingModel) ?? ""
        // Default OFF: body-on-open only until the user opts into background bodies.
        backgroundBodyHydrationEnabled = defaults.bool(forKey: Keys.backgroundBodyHydration)
        // Migrate the pre-#2179 Bool preference: true → .daily, false → .never,
        // unset → .daily. The new enum key wins once it exists.
        if let rawSchedule = defaults.string(forKey: Keys.updateCheckSchedule),
            let schedule = UpdateCheckSchedule(rawValue: rawSchedule)
        {
            updateCheckSchedule = schedule
        } else if defaults.object(forKey: Keys.legacyCheckForUpdatesAutomatically) != nil {
            updateCheckSchedule =
                defaults.bool(forKey: Keys.legacyCheckForUpdatesAutomatically) ? .daily : .never
        } else {
            updateCheckSchedule = .daily
        }
        lastUpdateCheck = defaults.object(forKey: Keys.lastUpdateCheck) as? Date
        viewerLayout = defaults.string(forKey: Keys.viewerLayout)
            .flatMap(ViewerLayout.init(rawValue:)) ?? ViewerLayout.defaultLayout
        headerMode = defaults.string(forKey: Keys.headerMode)
            .flatMap(MessageHeaderMode.init(rawValue:)) ?? MessageHeaderMode.defaultMode
        attachmentDefaultAction = defaults.string(forKey: Keys.attachmentDefaultAction)
            .flatMap(AttachmentDefaultAction.init(rawValue:))
        markdownTheme = defaults.string(forKey: Keys.markdownTheme)
            .flatMap(MarkdownThemeChoice.init(rawValue:)) ?? MarkdownThemeChoice.defaultChoice
        undoSendDelay = UndoSendDelay(
            rawValue: (defaults.object(forKey: Keys.undoSendDelay) as? Int) ?? UndoSendDelay.ten.rawValue
        ) ?? .ten
        sendFlowed = (defaults.object(forKey: Keys.sendFlowed) as? Bool) ?? true
        warnAboutForgottenAttachment =
            (defaults.object(forKey: Keys.warnAboutForgottenAttachment) as? Bool) ?? true
        autoconfigLookupEnabled =
            (defaults.object(forKey: Keys.autoconfigLookupEnabled) as? Bool) ?? true
        // Migrate from the legacy dictionary ONLY when the library key is
        // absent. A present-but-undecodable blob (e.g. written by a newer
        // build) must never fall back to the stale legacy data — the next
        // save would silently erase every later edit — so keep a copy of the
        // raw bytes and start empty rather than resurrect old text.
        if let data = defaults.data(forKey: Keys.signatureLibrary) {
            if let library = try? JSONDecoder().decode(SignatureLibrary.self, from: data) {
                signatureLibrary = library
            } else {
                defaults.set(data, forKey: Keys.signatureLibrary + ".undecodable-backup")
                Log.ui.error("preferences: signatureLibrary blob undecodable; backed up, starting empty")
                signatureLibrary = SignatureLibrary()
            }
        } else {
            let legacy = (defaults.dictionary(forKey: Keys.signatures) as? [String: String]) ?? [:]
            signatureLibrary = SignatureLibrary(migratingLegacy: legacy)
        }
        if let data = defaults.data(forKey: Keys.templateLibrary) {
            if let library = try? JSONDecoder().decode(TemplateLibrary.self, from: data) {
                templateLibrary = library
            } else {
                defaults.set(data, forKey: Keys.templateLibrary + ".undecodable-backup")
                Log.ui.error("preferences: templateLibrary blob undecodable; backed up, starting empty")
                templateLibrary = TemplateLibrary()
            }
        } else {
            templateLibrary = TemplateLibrary()
        }
        markdownDefaults = (defaults.dictionary(forKey: Keys.markdownDefaults) as? [String: Bool]) ?? [:]
        newMailNotifications = NewMailNotifications(
            rawValue: defaults.string(forKey: Keys.newMailNotifications) ?? "") ?? .all
        notificationsShowPreview =
            (defaults.object(forKey: Keys.notificationsShowPreview) as? Bool) ?? true
        notificationsPlaySound =
            (defaults.object(forKey: Keys.notificationsPlaySound) as? Bool) ?? true
        notificationsExcludedAccountIds = Set(
            (defaults.array(forKey: Keys.notificationsExcludedAccountIds) as? [String]) ?? [])
        dockBadgeMode = DockBadgeMode(rawValue: defaults.string(forKey: Keys.dockBadgeMode) ?? "")
            ?? .inboxUnread
        dockBadgeSmartMailboxId = defaults.string(forKey: Keys.dockBadgeSmartMailboxId)
        menuBarCounterMode = DockBadgeMode(rawValue: defaults.string(forKey: Keys.menuBarCounterMode) ?? "")
            ?? .off
        menuBarSmartMailboxId = defaults.string(forKey: Keys.menuBarSmartMailboxId)
        // An undecodable blob (e.g. written by a newer build) reads as defaults and is
        // not overwritten until the user edits a rule (property observers do not run in init).
        notificationAccountRules = defaults.data(forKey: Keys.notificationAccountRules)
            .flatMap { try? JSONDecoder().decode([String: AccountNotificationRule].self, from: $0) } ?? [:]
    }

    /// The default signature text for `accountId`, or `""` when none is set.
    func signature(for accountId: String) -> String {
        signatureLibrary.defaultText(for: accountId)
    }

    /// Sets the text of `accountId`'s default signature, creating one when the
    /// account has none. An empty (or whitespace-only) string deletes the
    /// default signature rather than storing a blank one.
    func setSignature(_ text: String, for accountId: String) {
        let blank = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        if let current = signatureLibrary.defaultItem(for: accountId) {
            if blank {
                signatureLibrary.delete(id: current.id)
            } else {
                signatureLibrary.setText(id: current.id, text)
            }
        } else if !blank {
            signatureLibrary.add(name: "Signature", accountId: accountId, text: text)
        }
    }

    /// Whether `accountId` defaults new compose windows to Markdown on.
    func markdownDefault(for accountId: String) -> Bool {
        markdownDefaults[accountId] ?? false
    }

    /// Forgets everything stored per account in preferences: signatures (and the legacy
    /// plain-text signature dictionary) and the Markdown default. Called when the account
    /// is removed, so re-adding the same address does not resurrect them (#3439).
    func forgetAccount(_ accountId: String) {
        signatureLibrary.removeAll(forAccount: accountId)
        markdownDefaults.removeValue(forKey: accountId)
        if var legacy = defaults.dictionary(forKey: Keys.signatures) as? [String: String],
           legacy.removeValue(forKey: accountId) != nil {
            defaults.set(legacy, forKey: Keys.signatures)
        }
    }

    /// Stores the per-account Markdown default.
    func setMarkdownDefault(_ enabled: Bool, for accountId: String) {
        if enabled {
            markdownDefaults[accountId] = true
        } else {
            markdownDefaults.removeValue(forKey: accountId)
        }
    }

    /// True once `aiEnabled` has been explicitly written at least once —
    /// distinguishes "never decided" (fresh install, eligible for the
    /// first-launch Ollama probe) from "explicitly off" (`bool(forKey:)`
    /// alone can't tell these apart). The probe always writes `aiEnabled`
    /// after it runs, win or lose, so it fires at most once per install.
    var hasStoredAiEnabledPreference: Bool {
        defaults.object(forKey: Keys.aiEnabled) != nil
    }
}
