// Castellum - observable app state for the read shell.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Owns the CastellumCore, the loaded data, and the current selection. All FFI
// work runs off the main actor (it's blocking) and results hop back to @MainActor.

import AppKit
import Combine
import CoreSpotlight
import SwiftUI

/// S64 infinite scroll: whether the last `AppModel.messages` mutation
/// replaced the array (mailbox switch, search, sync-triggered reload) or
/// appended a keyset page at the tail (scrolled near the end). Drives
/// `MessageListView.Coordinator.apply`'s `reloadData` vs `insertRows` choice.
enum MessagesChange: Equatable {
    case replaced
    case appended(Range<Int>)
}

/// Which of the two paging FFI calls `loadMoreMessagesIfNeeded` should use —
/// captured synchronously (as plain Sendable strings, matching
/// `loadSmartMailboxMessages`'s existing `let id = sm.id` convention rather
/// than carrying a whole model struct across the `Task.detached` hop) before
/// the fetch, so it never has to force-unwrap `selectedMailbox`/
/// `selectedUnifiedRole`.
private enum PagingScope {
    case mailbox(accountId: String, mailboxId: String)
    case unifiedRole(String)
}

/// S35: the specific counts a reset refused on.
struct ResetAccountBlockedPrompt: Identifiable, Equatable {
    var id: String { accountId }
    let accountId: String
    let email: String
    let blockers: ResetAccountBlockers
}

/// S36: where a compose draft's server mirror stands, for `ComposeView`'s status caption.
enum ServerDraftSaveState: Equatable {
    case idle
    case saving
    case saved(Date)
}

/// What `AppModel.revealMessage` did (Spotlight continuation, notifications).
enum MessageRevealOutcome: Equatable { case shown, notFound }

@MainActor
final class AppModel: ObservableObject {
    @Published var accounts: [Account] = []
    /// True once `accounts` reflects the first completed `reloadAccounts()`
    /// pass — whether or not any account exists. Never resets to `false`.
    /// `AppDelegate` uses this (via `onAccountsLoaded`) to hold queued
    /// mailto:/Share-Extension URLs so the first one after a cold launch can
    /// never race `accounts` still being its default empty value (S57
    /// cold-launch bug, found 2026-09-26: `modelBecameAvailable` used to
    /// flush immediately, before this ever became true).
    @Published private(set) var accountsLoaded = false
    /// Callbacks queued by `onAccountsLoaded` while `accountsLoaded` is still
    /// `false`; drained exactly once, in `reloadAccounts()`'s first apply.
    private var accountsLoadedCallbacks: [() -> Void] = []
    @Published var mailboxes: [String: [Mailbox]] = [:]   // accountId -> mailboxes
    @Published var counts: [String: MailboxCounts] = [:]  // mailboxId -> counts
    /// mailboxId -> body-hydration progress (#3631 G5); sidebar tooltip only.
    @Published var hydrationProgress: [String: BodyHydrationProgress] = [:]
    /// Body coverage of the active search's scope (#3633 G13); nil when no search is running.
    @Published var searchBodyCoverage: BodyHydrationScope?
    /// S64: bumped on every `messages` mutation, replace or append. An `Int`
    /// (not the array itself) so `MessageListView.updateNSView`'s reload
    /// guard is an O(1) integer compare instead of an O(n) array `!=` — at
    /// 10k+ loaded rows the array comparison alone cost milliseconds per
    /// SwiftUI render.
    @Published private(set) var messagesGeneration = 0
    /// Whether the LAST `messages` mutation replaced the array wholesale or
    /// appended a page at the end (S64 infinite scroll) — read by
    /// `MessageListView.Coordinator.apply` to choose `insertRows` (append,
    /// preserves scroll/selection) over `reloadData` (replace). Defaults to
    /// `.replaced` on every assignment EXCEPT the one
    /// `loadMoreMessagesIfNeeded` append, which sets `pendingAppend` first —
    /// see `messages`'s `willSet` below.
    private(set) var lastMessagesChange: MessagesChange = .replaced
    /// Guards the `willSet` below: true for the one assignment (an append)
    /// that must NOT have `lastMessagesChange` reset back to `.replaced`.
    private var pendingAppend = false
    @Published var messages: [MessageHeader] = [] {
        willSet { if !pendingAppend { lastMessagesChange = .replaced } }
        didSet {
            messagesGeneration += 1; pendingAppend = false
            // M6: the first list reaching the UI releases the launch contacts
            // refresh (a no-op unless `bootstrap()` armed it).
            startLaunchContactsRefreshIfArmed()
            // F1: same release point for background embeddings — they never run on the
            // launch path, only once the first list is on screen (and after a grace period).
            aiCoordinator.embeddings.firstListReachedUI()
        }
    }
    /// S64 keyset paging: the cursor for the NEXT page of `messages`, or
    /// `nil` once the mailbox/unified list is exhausted. Only meaningful for
    /// a concrete mailbox or unified-role selection (paging is not wired for
    /// search, smart mailboxes or Snoozed).
    private var nextCursor: ListCursor?
    /// The list's sort (#3043): one global choice, persisted. Read the EFFECTIVE
    /// sort (`effectiveSort`) when querying; this is what the user last clicked.
    @Published private(set) var messageSort: MessageSort = MessageSort.load(from: .standard)
    /// Where the sort choice is stored; a testing seam (tests inject a throwaway suite).
    var messageSortDefaults: UserDefaults = .standard
    /// True while a `loadMoreMessagesIfNeeded` page fetch is in flight UNDER
    /// THE CURRENT `listToken`, so a fast scroll can't fire the same page
    /// request twice. Only two things clear it: that page's own completion
    /// (token still current) or `invalidateInFlightPage()` (token rotated) —
    /// a stale page's completion returns early on the token mismatch, so it
    /// can never clear it itself.
    private var isLoadingMore = false
    /// Rotated by `invalidateInFlightPage()` on every list switch and every
    /// sync reload; a page reply captured against a stale token (the list
    /// changed under it while it was in flight) is dropped instead of
    /// appending onto the WRONG list.
    private(set) var listToken = UUID()
    var hasMoreMessages: Bool { nextCursor != nil || tagListHasMore }
    /// Rows loaded per keyset page (S64) — the FFI's own paging default;
    /// deliberately the same 500 the pre-paging `select(...)` calls always
    /// asked for, so first-page behaviour is unchanged.
    static let listPageSize: Int64 = 500
    @Published var selectedMailbox: Mailbox? {
        didSet { if selectedMailbox != nil { correspondence = nil } }
    }
    /// The active unified (cross-account) sidebar group's mailbox role (e.g.
    /// "inbox"), or `nil` when a concrete `Mailbox` is selected instead.
    /// Mutually exclusive with `selectedMailbox`: `select(mailbox:)` and
    /// `select(unifiedRole:)` each clear the other.
    @Published var selectedUnifiedRole: String? {
        didSet { if selectedUnifiedRole != nil { correspondence = nil } }
    }
    /// Whether the message list is currently a unified (cross-account) list.
    var isUnifiedList: Bool { selectedUnifiedRole != nil }
    /// Set by `runSearch`: the active search spans every account (scope "All accounts", or a
    /// search typed inside a cross-account list).
    var activeSearchIsCrossAccount = false
    /// Rows may come from several accounts (All Inboxes, a smart mailbox, Snoozed, Correspondence
    /// or a cross-account search), so the list shows the Account column (#3459).
    var isCrossAccountList: Bool {
        selectedUnifiedRole != nil || selectedSmartMailboxId != nil || selectedIsSnoozed
            || selectedTagKey != nil
            || correspondence != nil || (!searchText.isEmpty && activeSearchIsCrossAccount)
    }
    /// Whether the "Snoozed" sidebar group is the active list. Mutually
    /// exclusive with `selectedMailbox`/`selectedUnifiedRole`: `select(mailbox:)`,
    /// `select(unifiedRole:)` and `select(snoozed:)` each clear the others.
    @Published var selectedIsSnoozed = false {
        didSet { if selectedIsSnoozed { correspondence = nil } }
    }
    /// MM-4: when non-nil the message list shows all mail to/from one person
    /// across every account and mailbox (View > Show Correspondence). Any
    /// sidebar navigation clears it (see the `didSet`s on the selection
    /// properties), and a sync refresh re-runs it instead of reverting the list.
    @Published var correspondence: CorrespondenceContext? {
        // However Correspondence ends (close button, sidebar navigation, typing
        // a search), the "came from Snoozed" memory ends with it.
        didSet { if correspondence == nil { correspondenceCameFromSnoozed = false } }
    }
    /// Whether the list Correspondence replaced was the Snoozed view (its flag
    /// is cleared while Correspondence is open, and restored on close).
    private var correspondenceCameFromSnoozed = false
    @Published var correspondenceStats = CorrespondenceStats()
    /// The currently-snoozed messages, soonest due first — backs the
    /// Snoozed sidebar list AND its badge count. Refreshed after every
    /// snooze/unsnooze action and after the due-check sweep.
    @Published var snoozedMessages: [SnoozedMessageHeader] = []
    @Published var selectedMessage: MessageHeader?
    /// All selected message ids (multi-selection) for bulk actions; `selectedMessage`
    /// is the primary (reading-pane) selection.
    @Published var selectedMessageIds: [String] = []
    @Published var detail: MessageDetail? {
        didSet {
            // Fold state and the selection belong to ONE message (MM-17): never carried to the next.
            if oldValue?.header.id != detail?.header.id {
                plainFold.reset()
                readingSelection = nil
            }
        }
    }
    /// Which quote/signature blocks of the plain-text body the user opened (MM-17; per message, not persisted).
    @Published var plainFold = PlainFoldState()
    /// The text selected in the reading pane, for Reply to quote just that (MM-17 slice 4).
    @Published var readingSelection: ReadingSelection?
    /// Sender addresses (lower-cased) with a remembered "always load remote
    /// content" decision for the currently active account, loaded lazily and
    /// cached for the session (S48). `nil` value = explicitly checked, no policy.
    @Published private(set) var senderRemoteContentCache: [String: Bool] = [:]
    /// Every remembered sender policy across every account, for Settings'
    /// review list (S48). Reloaded each time Settings' General tab appears.
    @Published var senderRemoteContentPolicies: [SenderRemoteContentPolicy] = []
    @Published var isHydratingSelectedBody = false
    @Published var selectedBodyHydrationError: String?
    @Published var searchText: String = "" {
        // #3043 T10: a search opens in relevance order; a header click overrides it, and
        // changing the query puts relevance back.
        didSet { if searchText != oldValue { searchSortOverridden = false } }
    }
    /// True once the user clicked a header while a search is showing (see `searchText`).
    @Published private(set) var searchSortOverridden = false
    /// Where a typed search looks (MM-2). Persisted; defaults to the current
    /// account, which is what search did before the scope menu existed.
    @Published var searchScope: SearchScope = SearchScope(
        rawValue: UserDefaults.standard.string(forKey: "searchScope") ?? "") ?? .account {
        didSet {
            UserDefaults.standard.set(searchScope.rawValue, forKey: "searchScope")
            if !searchText.isEmpty { runSearch() }
        }
    }
    @Published var errorText: String?
    /// Archive / Delete / Move actions that can still be undone this session
    /// (newest last, capped at `RemovalUndoStack.capacity`). Each entry is also
    /// registered with the window's `UndoManager` (Edit > Undo Archive, Cmd-Z);
    /// the bare `z` key pops the top. See `RemovalUndo.swift`.
    @Published private(set) var removalUndoStack = RemovalUndoStack()
    /// Where undo actions are registered. A testing seam: the default is the
    /// main window's manager, which is what drives the Edit > Undo menu item.
    var undoManagerProvider: () -> UndoManager? = { NSApp.mainWindow?.undoManager }
    /// The main window, captured by RootView. `NSApp.mainWindow` is nil while the
    /// app is not active (and under the UIT-1 harness), which silently dropped the
    /// registration; this keeps Edit > Undo and Cmd-Z working in both.
    weak var mainWindowForUndo: NSWindow?
    private var removalUndoManager: UndoManager? { undoManagerProvider() ?? mainWindowForUndo?.undoManager }
    /// A permanent delete (Delete pressed inside Trash) awaiting the user's
    /// confirmation; non-nil presents the dialog. See `PermanentDelete.swift`.
    @Published var pendingPermanentDelete: PermanentDeleteRequest?
    /// The message currently showing the unsubscribe confirmation sheet, or
    /// nil. Set from the reading pane's "Unsubscribe" link; cleared by the
    /// sheet's Cancel or by a successful/failed confirm.
    @Published var unsubscribeCandidate: MessageDetail?
    @Published var unsubscribeStatusText: String?
    /// Set when a reset was refused because the account still has an unsent
    /// outbox draft or a non-terminal queued operation (S35). The Accounts
    /// tab presents a SECOND confirmation naming the exact counts; confirming
    /// it calls `resetAccount(accountId:force: true)`.
    @Published var resetAccountBlocked: ResetAccountBlockedPrompt?
    /// Presentation state for background IMAP sync. The durable source is the
    /// core's account_sync_state row; raw errors remain local-only for Diagnostics.
    @Published private(set) var accountSyncStatuses: [String: AccountSyncStatus] = [:]
    /// A best-effort snapshot of each currently-syncing account's progress
    /// — mailbox/message counts, polled from Rust alongside
    /// the blocking sync call rather than pushed. Entries are removed once
    /// that account's `performSync` pass completes; absence just means
    /// "nothing in flight (or numbers not available yet)", never an error.
    @Published private(set) var accountSyncProgress: [String: SyncProgress] = [:]
    /// When each account's last successful sync pass finished (this session,
    /// or seeded from the durable diagnostics at launch). Feeds the sidebar's
    /// "Checked 8 s ago" line (#2832); a UI-only record, never a network input.
    @Published private(set) var accountLastChecked: [String: Date] = [:]
    /// At most one non-blocking background-sync notice is visible at a time.
    @Published var syncBanner: AccountSyncStatus?
    @Published var isSyncing = false
    /// Undelivered write-path operations across all accounts (outbox indicator).
    @Published var pendingCount = 0
    /// OB1: messages waiting in the Outbox (queued, failed or stranded sends)
    /// across all accounts — the sidebar's Outbox count.
    @Published var outboxMessageCount = 0
    /// Bumped on EVERY queue recomputation (`refreshPendingCount`), even when
    /// both counts are unchanged (a queued send turning failed keeps them
    /// equal). The Outbox window reloads from this signal (#3036).
    @Published private(set) var outboxRevision = 0
    /// OB1: a Send All started from the menu or the Outbox is running.
    @Published var isSendingOutbox = false
    /// OB1: one SMTP pass per account at a time (drains, Send Now, Send All).
    let smtpGate = AccountSerialGate()
    /// OB1: attempts queued sends once per account at launch and whenever the
    /// network comes back (the core's backoff decides what is due).
    private(set) lazy var outboxAutoSender = OutboxAutoSender { [weak self] id in
        self?.drainSendPending(accountId: id)
    }
    private var reachability: NetworkReachabilityMonitor?
    /// Local compose drafts per account (accountId -> drafts, any status).
    @Published var drafts: [String: [Draft]] = [:]
    /// A background replay pass is in flight.
    @Published var isReplaying = false
    /// S25 undo-send: the one non-modal "Message sent · Undo" banner, if a
    /// delayed send is currently within its undo window.
    @Published var undoSendBanner = UndoSendBannerState()
    /// #3022 S2: the "Archived 3 messages · Undo" banner (never shown over the undo-send one).
    @Published var removalUndoBanner = RemovalUndoBannerState()
    private var removalBannerTickTask: Task<Void, Never>?
    /// S25 undo-send: a short non-modal notice ("too late to undo") shown
    /// when Undo loses the race — the sender already claimed the op. Never a
    /// sheet/alert, same discipline as `undoSendBanner`; the view that
    /// presents it is responsible for auto-dismissing after a few seconds.
    @Published var undoTooLateNotice: String?
    /// FP29: set when a send fails after the undo window ("Not sent — kept in
    /// Outbox. <reason>"); shown until dismissed, see `SendFailedNoticeView`.
    @Published var sendFailedNotice: String?
    /// Contacts for the Contacts window (most recent first).
    @Published var contacts: [Contact] = []
    /// Contacts window filters (owned here so reloads keep them applied).
    @Published var contactsQuery: String = ""
    @Published var contactsIncludeHidden = false
    /// All projects (non-archived by default; refreshed on demand).
    @Published var projects: [ProjectCard] = []
    /// Saved searches pinned in the sidebar (S41), sidebar order.
    @Published var smartMailboxes: [SmartMailbox] = [] {
        didSet { inboxSplitDefinitionsChanged(from: oldValue) }
    }
    /// HP-2: the Split Inbox tabs (splits, then Other) with counts for the
    /// Inbox on screen; empty when no split is defined. See AppModel+InboxSplits.
    @Published var inboxSplitTabs: [InboxSplitCount] = []
    /// HP-2: the selected tab's id (a split id or "other"). Kept across
    /// Inbox visits so coming back to the Inbox returns to the same tab.
    @Published var selectedInboxSplitId: String?
    /// The split query failed (counts or rows): the Inbox shows the plain list instead and
    /// `isInboxSplitActive` is false until the next mailbox selection resets it (#3441).
    @Published var inboxSplitFailed = false
    /// The selected smart mailbox's id, or `nil` when a concrete `Mailbox`
    /// or the unified "All Inboxes" group is selected instead. Mutually
    /// exclusive with `selectedMailbox`/`selectedUnifiedRole` — every
    /// `select(...)` entry point clears the other two.
    @Published var selectedSmartMailboxId: String? {
        didSet { if selectedSmartMailboxId != nil { correspondence = nil } }
    }
    /// MM-8: the selected sidebar tag's `keywordKey`, or `nil`. Mutually
    /// exclusive with `selectedMailbox`/`selectedUnifiedRole`/
    /// `selectedSmartMailboxId`/`selectedIsSnoozed`: every `select(...)` entry
    /// point clears it, and `select(tag:)` clears the others.
    @Published var selectedTagKey: String? {
        didSet { if selectedTagKey != nil { correspondence = nil } }
    }
    /// Per-tag message counts (total, unread, local-only) behind the sidebar
    /// Tags badges, keyed by `keywordKey`. Recomputed by the core on demand,
    /// never cached there, so there is nothing to drift (MM-8).
    @Published private(set) var tagCounts: [String: TagCount] = [:]
    /// Keyset paging for the tag list (core cursor = the last loaded row):
    /// whether a full page suggests there is more.
    private var tagListHasMore = false
    /// Non-modal inline notice when the currently-selected smart mailbox
    /// couldn't run (deleted elsewhere, or a genuine DB error) — shown by
    /// `SmartMailboxNoticeBanner`, never a crash or a silently stale list.
    @Published var smartMailboxNotice: String?
    /// Tag definitions (S42), in display order.
    @Published var tags: [TagDefinition] = []
    /// Every distinct visible keyword in use with a message count (S42) —
    /// Settings' "In use on your servers" adopt list. Loaded on demand
    /// (`reloadKeywordsInUse()`), not at bootstrap: only Settings → Tags
    /// needs it.
    @Published var keywordsInUse: [KeywordInUse] = []
    /// Transient notice when a tag/mute change couldn't reach the server
    /// (evaluation mode, or the mailbox refused the keyword) — shown by the
    /// same banner style as `AccountWritePolicy` refusals. `nil` = no notice.
    @Published var lastTagLocalOnlyNotice: String?
    /// Transient inline-form error for the smart mailbox editor sheet —
    /// distinct from `errorText`'s top-level banner so the sheet shows this
    /// next to its fields instead of stacking a second alert.
    @Published var smartMailboxSaveError: String?
    /// MM-1c: the header-name policy and coverage the smart-mailbox editor
    /// shows. Loaded when the editor opens (`refreshHeaderPolicy`); nil until then.
    @Published var headerPolicy: FilterHeaderPolicy?
    @Published var headerCoverage: HeaderCoverageDTO?
    /// Tags for the currently-displayed message (shown in the reading pane).
    @Published var currentProjectTags: [ProjectTag] = []
    @Published var selectedMessageIdForAi: String?
    /// "Find similar" (S53-UI). The anchor message the gesture was invoked
    /// on drives sheet presentation (`.sheet(item:)`, mirroring
    /// `unsubscribeCandidate` below) — non-nil for the whole lifetime of the
    /// sheet, set the instant the gesture fires (before the FFI round trip
    /// even starts) and cleared on dismiss. `findSimilarResults`/
    /// `findSimilarError` populate once `findSimilarInFlight` finishes;
    /// none of these ever touch `searchText` or any other search state —
    /// per DECISION-S53-ui, semantic results never appear on the typing path.
    @Published var findSimilarAnchor: MessageHeader?
    @Published var findSimilarInFlight = false
    @Published var findSimilarResults: [SimilarMessageResult] = []
    @Published var findSimilarError: String?
    /// Per-account write policy as probed from the core (accountId -> enabled).
    /// An ABSENT entry means "not probed yet" and resolves to read-only — see
    /// `AccountWritePolicy.state`, which fails closed exactly like the core.
    @Published private(set) var accountWriteEnabled: [String: Bool] = [:]
    /// The one write-safety modal awaiting the user (refusal notice or an
    /// enable/disable confirmation). A single slot presented by
    /// `WriteSafetyPresenter` on the window the user acted in, never by a
    /// SwiftUI view — see `WriteSafetyPrompt`.
    @Published var writeSafetyPrompt: WriteSafetyPrompt?
    /// Repeats the action behind the pending `.blocked` prompt once writes are
    /// enabled; see `registerWriteSafetyRetry`. Never outlives its prompt.
    private var writeSafetyRetry: (@MainActor () -> Void)?
    /// True while an enable/disable round-trip is in flight (the control
    /// disables itself so the policy can't be double-flipped).
    @Published private(set) var isChangingWritePolicy = false
    /// Set by `composeAccountIdOrPromptUnavailable()` when compose can't open
    /// — either nothing is configured, or an account was just added and its
    /// first sync (the only thing that populates `accounts`, see
    /// `reloadAccounts()`) hasn't finished yet. Presented as a two-button
    /// alert from `RootView` instead of the old OK-only dead end that had
    /// Marc clicking through it repeatedly mid-first-sync of a large mailbox.
    @Published var composeUnavailablePrompt: ComposeUnavailablePrompt?
    /// Which Settings tab should be frontmost. Driven by `openAccountsSettings`
    /// (the credentials-error alert's "Open Accounts…" button) so Settings
    /// opens straight to the Accounts tab instead of wherever it last was.
    @Published var settingsTab: SettingsTab = .general
    /// The account the Accounts tab should highlight when it next appears —
    /// set together with `settingsTab` by `openAccountsSettings`.
    @Published var settingsSelectedAccountId: String?
    /// Bumped on every explicit policy flip. A probe that started BEFORE the
    /// flip must not land after it and overwrite the newer, authoritative
    /// value — `reloadAccounts` re-probes after every sync, so that race is
    /// routine, and losing it would leave the badge claiming the opposite of
    /// the truth until the next write.
    private var writePolicyGeneration: UInt64 = 0
    /// CoreSpotlight indexing of message metadata, opt-in (S58, the tracker
    /// #2260). Persisted profile-wide via `core.spotlightIndexingEnabled`
    /// (`app_meta`) — deliberately NOT `Preferences`/`UserDefaults`, which is
    /// per-machine: this must travel with the profile. Reloaded once at
    /// `bootstrap()`; `setSpotlightIndexingEnabled` is the only mutator.
    /// Fail-closed default `false` — see the Rust core's own default.
    @Published private(set) var spotlightIndexingEnabled = false

    let core: CastellumCore
    /// Keychain credentials, namespaced by profile (see
    /// `AccountStore.service(forProfileDirectory:)`).
    let accountStore: AccountStore
    let aiCoordinator: AiCoordinator
    /// Drives S58 CoreSpotlight indexing. Test seam: `init(spotlightIndex:)`
    /// substitutes a recording double for the real `CSSearchableIndex`
    /// (unavailable, signing-wise, under `xcodebuild test`).
    let spotlightIndexer: SpotlightIndexer
    /// The resolved profile directory (real, `CASTELLUM_PROFILE_DIR`-overridden,
    /// or a test's temp dir) — see `init`. `KeyBindingsStore` reads
    /// keybindings.json from here.
    let profileDirectory: URL
    /// S45: the effective (defaults + valid user overrides) command table.
    /// A plain `let`, not `@Published` — reloading is an explicit user
    /// action (Settings > General > "Reload Key Bindings"), not
    /// live-reactive; see Assumption 9.
    let keyBindings: KeyBindingsStore
    /// Debounce handle so a burst of write actions coalesces into one replay pass.
    private var replayDebounce: Task<Void, Never>?
    /// Set when a replay is requested while one is already running (drain again).
    private var needsReplay = false
    /// Pending "mark read after a delay" timer for the current selection.
    private(set) var autoMarkTimer: Task<Void, Never>?
    /// Current body-on-open hydration task for descriptor-only messages.
    private var bodyHydrationTask: Task<Void, Never>?
    /// M3 perf signpost slots (PerfSignpost.swift): one open interval each.
    private var mailboxSwitchSpan: PerfSpan?
    private var messageOpenSpan: PerfSpan?
    /// Monotonic token so cancelled/retried hydration results cannot apply late.
    private var bodyHydrationGeneration: UInt64 = 0
    /// S32: one incoming attachment's local download state, keyed by
    /// `attachmentKey(messageId:ordinal:)`.
    enum AttachmentDownloadState: Equatable {
        case idle
        case checking
        case downloading
        case ready(localPath: String)
        case failed(String)
    }
    /// S32: per-attachment download state (see `AttachmentStripView`).
    @Published var attachmentDownloads: [String: AttachmentDownloadState] = [:]
    private var attachmentDownloadTasks: [String: Task<Void, Never>] = [:]
    /// Bumped per key on every new request so a stale completion (the user
    /// re-selected the message, or retried) never overwrites a newer result
    /// — same generation-guard shape as `bodyHydrationGeneration`.
    private var attachmentDownloadGenerations: [String: UInt64] = [:]
    /// Debounce handle so a burst of syncs coalesces into one contacts refresh.
    private var contactsDebounce: Task<Void, Never>?
    /// M6: where the once-per-launch contacts refresh is. `bootstrap()` arms
    /// it; the first list delivered (or the fallback timer) starts it; any
    /// refresh that starts first (Contacts window opened early) consumes it.
    private enum LaunchContactsRefresh { case idle, armed, started }
    private var launchContactsRefresh: LaunchContactsRefresh = .idle
    private var launchContactsFallback: Task<Void, Never>?
    /// True while a contacts refresh is in flight: never two at once.
    private var contactsRefreshRunning = false
    /// A trigger arrived while one was running: rerun once when it finishes
    /// (new mail may have landed since it read the messages).
    private var contactsRefreshDirty = false
    /// The refresh itself. Injectable so tests can spy on ordering and
    /// concurrency without a real profile; production is the core call.
    var contactsRefreshWork: @Sendable (CastellumCore) throws -> Void = { _ = try $0.refreshContacts() }
    /// X1: the one-off repair of subjects and names stored RFC 2047-encoded by
    /// an earlier build; returns how many messages it rewrote. Runs once per
    /// launch just before the launch contacts refresh (so the contacts are
    /// derived from decoded names), never on the launch path. Injectable for
    /// the same reason as `contactsRefreshWork`.
    var envelopeRepairWork: @Sendable (CastellumCore) throws -> Int64 = {
        try $0.repairEncodedEnvelopeText().messagesRepaired
    }
    /// Safety net for launches that never deliver a list (no accounts, no
    /// selection): start the refresh anyway after this long.
    var launchContactsFallbackNanos: UInt64 = 5_000_000_000
    /// S36: one debounce handle per open compose draft, so a burst of edits
    /// to draft A never cancels a pending save for draft B.
    private var serverDraftSaveDebounce: [Int64: Task<Void, Never>] = [:]
    /// S36: pure "has it been long enough since the last edit" policy,
    /// unit-tested in isolation (`ComposeAutosaveTests`); this instance only
    /// supplies the interval, the actual Task/sleep scheduling lives here.
    private let composeAutosave = ComposeAutosaveScheduler(debounceInterval: 2.0)
    /// S36: per-draft server-mirror save status, for `ComposeView`'s small
    /// "Saving to server… / Saved to server" caption near Send.
    @Published var serverDraftSaveState: [Int64: ServerDraftSaveState] = [:]
    /// One background body-hydration drain loop per account (see
    /// `scheduleBackgroundHydration`); a new sync replaces the running drain,
    /// and the replacement waits for the old one's in-flight blocking pass.
    private let backgroundHydration = SerializedTaskSlots()
    /// Ref-count of in-flight syncs so concurrent per-account syncs (reSyncAll)
    /// don't clear the spinner while others are still running.
    private var syncingCount = 0
    /// Latest sync generation per account. A slower earlier request must not
    /// overwrite the status or refresh work from a newer request.
    private var syncGenerations: [String: UInt64] = [:]
    /// The generation whose config `addAccount` most recently saved to the
    /// Keychain, per account (early, once the attempt is past login — see
    /// `inFlightSyncs` — or on completion). `performSync`'s `onSuccess`/
    /// `onFailure` deliberately always run, even for a superseded generation
    /// — but `onSuccess` closes over ITS OWN
    /// `configJSON`, so two SUCCESSFUL attempts completing out of order (a
    /// retry finishing first, the original finishing later) would otherwise
    /// let the stale, later-completing original silently overwrite the
    /// newer, already-saved retry config. This is a monotonic guard, not an
    /// `isLatest` gate: a later completion must never regress an earlier
    /// save, but an EARLIER completion (the normal, in-order case, and the
    /// #2495 case where the retry fails and only the original ever
    /// succeeds) must still save normally. The SAME generation may save
    /// twice (early, then on completion) — same config, idempotent. Cleared
    /// wherever `syncGenerations[accountId]` is cleared.
    private var lastSavedGeneration: [String: UInt64] = [:]
    /// `performSync` calls currently running per account, of ANY generation
    /// (a superseded attempt keeps running — nothing cancels it). The
    /// progress poller uses it to know that a live (`done == false`) Rust
    /// progress snapshot can only be its own attempt's: `sync_progress` is
    /// one snapshot per account id, so while two attempts overlap it cannot
    /// say which one got past login.
    private var inFlightSyncs: [String: Int] = [:]
    /// Accounts the user removed. Inserted synchronously when Remove is clicked and kept until
    /// the core row, the Keychain entry and any in-flight sync's leftovers are all gone. While an
    /// id is in here no sync, periodic tick, IDLE start or credential save may touch it: an
    /// in-flight pass cannot be cancelled, and a failed Keychain delete used to let the periodic
    /// sync restore the account with all its mail (#3439). Persisted in the profile so a failed
    /// Keychain delete is retried at the next launch.
    private var removedAccountIdsCache: Set<String>?
    var removedAccountIds: Set<String> {
        get {
            if let cached = removedAccountIdsCache { return cached }
            let loaded = Self.readRemovalTombstones(in: profileDirectory)
            removedAccountIdsCache = loaded
            return loaded
        }
        set {
            removedAccountIdsCache = newValue
            Self.writeRemovalTombstones(newValue, in: profileDirectory)
        }
    }
    private static func tombstoneURL(in dir: URL) -> URL { dir.appendingPathComponent("removed-accounts.json") }
    private static func readRemovalTombstones(in dir: URL) -> Set<String> {
        guard let data = try? Data(contentsOf: tombstoneURL(in: dir)),
              let ids = try? JSONDecoder().decode([String].self, from: data) else { return [] }
        return Set(ids)
    }
    private static func writeRemovalTombstones(_ ids: Set<String>, in dir: URL) {
        let url = tombstoneURL(in: dir)
        if ids.isEmpty { try? FileManager.default.removeItem(at: url); return }
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        if let data = try? JSONEncoder().encode(ids.sorted()) { try? data.write(to: url, options: .atomic) }
    }
    /// `ids` minus the accounts being removed (the Keychain still lists those until it is gone).
    func excludingRemoved(_ ids: [String]) -> [String] {
        let removed = removedAccountIds
        return removed.isEmpty ? ids : ids.filter { !removed.contains($0) }
    }
    /// `performSync` passes started per account over this model's lifetime
    /// (S72). Read by `SyncCoalescingTests` to prove a request arriving
    /// while a sync is in flight never starts a second, stacked pass.
    private(set) var syncPassesStarted: [String: Int] = [:]
    /// S72 no-stacking rule: the config of ONE re-sync requested (through
    /// `sync(config:)` -- the periodic loop, launch `reSyncAll`, replay,
    /// Retry, reset) while that account already had a sync in flight. The
    /// latest request wins; it starts once the in-flight pass has finished
    /// (`runQueuedResyncIfIdle`). Before this, each request started another
    /// full pass on top of the running one (Marc's 2026-09-26 sample: eight
    /// passes for four accounts, a 50 s first sync taking 12 minutes).
    private var queuedResync: [String: String] = [:]
    /// One progress-polling loop per currently-syncing account (the tracker
    /// #2494), publishing into `accountSyncProgress`; cancelled and removed
    /// when that account's sync completes or is superseded by a newer one.
    private var syncProgressPollTasks: [String: Task<Void, Never>] = [:]
    /// The periodic background re-sync loop; cancelled in `deinit`.
    private var periodicSyncTask: Task<Void, Never>?
    /// Forwards sleep/wake and network-path-change signals into a nudge of
    /// every account's IDLE thread (S31); stopped in `deinit`.
    private var idleWakeObserver: IdleWakeObserver?
    /// S43 snooze due-check sweep loop; cancelled in `deinit`.
    private var snoozeSweepTask: Task<Void, Never>?
    /// S59: drains the core's notification outbox and posts banners; nil
    /// until `bootstrap()` (so a bare `AppModel` in a test never starts one).
    private(set) var notificationCoordinator: NotificationCoordinator?
    /// S59: the Dock icon's unread badge; nil until `bootstrap()`.
    private(set) var dockBadgeController: DockBadgeController?
    /// The optional menu bar counter (MM-9); nil until `bootstrap()`.
    private(set) var menuBarCounterController: MenuBarCounterController?
    /// S59: every VIP sender (drives the sidebar row and the Settings list).
    @Published private(set) var vips: [VipSender] = []
    /// S59: unread inbox messages from VIPs, across accounts (sidebar badge).
    @Published private(set) var vipUnreadCount: Int64 = 0
    /// HP1 Send Later sweep loop; cancelled in `deinit`.
    private var sendLaterSweepTask: Task<Void, Never>?
    /// How often the snooze sweep checks for due messages while the app is
    /// running. A launch-time check (in `bootstrap()`, via `startSnoozeSweep`
    /// running its first iteration immediately) covers "due while the app
    /// was closed"; this interval only covers "due while the app stays open".
    private static let snoozeSweepInterval: UInt64 = 30_000_000_000 // 30s
    /// S25 undo-send: the 1-second ticker driving `undoSendBanner`'s countdown.
    /// Replaced (not queued) by the next delayed send — only one banner is
    /// ever visible, so only one ticker needs to run.
    private var undoSendTickTask: Task<Void, Never>?
    /// S25 undo-send: one scheduled drain task per outstanding delayed send,
    /// keyed by draft id. Queuing a second delayed send while an earlier one
    /// is still in its window must NOT cancel the earlier one's drain — only
    /// the banner (which shows one send at a time) gets replaced; the earlier
    /// send keeps counting down in the background and still delivers when its
    /// own window ends. See `showUndoSendBanner`. Entries are removed once
    /// their drain fires or the send is undone.
    private var undoSendDrainTasks: [Int64: Task<Void, Never>] = [:]
    /// S25 crash-safety hold: account ids already reconciled this launch.
    /// `reconcile_undo_sends` must run exactly ONCE per account — repeating
    /// it mid-session would immediately hold a legitimate in-session undo
    /// window whose heartbeat simply hasn't ticked yet (see repo.rs).
    private var reconciledUndoSendAccountIds: Set<String> = []
    /// Testing seam (S25): overrides the action `undoSend` takes to reopen a
    /// draft after an in-time undo. `nil` (production default) means the
    /// real `openDraftForEditing`; tests substitute a recording closure to
    /// assert the ROUTING decision (reopen vs. too-late notice) without a
    /// live compose window — see `AppModelUndoRoutingTests`.
    var onUndoReopenDraft: ((Draft) -> Void)?
    /// Per-account timestamp of the last PERIODIC sync attempt, used only to
    /// decide the next one (`PeriodicSyncScheduler.isDue`). A manual sync —
    /// Retry, add-account, launch, replay-triggered re-sync — deliberately
    /// does NOT touch this: it must never suppress or delay the next
    /// scheduled check.
    private var periodicLastAttempt: [String: Date] = [:]
    /// Injected clock for periodic-resync scheduling decisions — mirrors
    /// `UpdateChecker`'s `now` pattern so the loop is unit-testable.
    private let clock: () -> Date
    /// How often the periodic loop wakes to check which accounts are due.
    /// Deliberately much shorter than either scheduling interval — `isDue`
    /// compares wall-clock time via `clock()`, not tick count, so a short
    /// poll just makes the loop responsive without affecting correctness.
    private static let periodicResyncPollInterval: UInt64 = 60_000_000_000 // 60s

    private func beginSync() { syncingCount += 1; isSyncing = true }
    private func endSync() { syncingCount = max(0, syncingCount - 1); isSyncing = syncingCount > 0 }

    /// `profileDirectory` is a test seam: pass a temp dir to keep unit tests off
    /// the real profile. nil = env override / production default. `clock` is a
    /// test seam for the periodic re-sync scheduler. `spotlightIndex` is a test seam for S58:
    /// pass a `SpotlightIndexing` double to keep tests off the real
    /// `CSSearchableIndex`.
    init(
        profileDirectory: URL? = nil, clock: @escaping () -> Date = Date.init,
        spotlightIndex: SpotlightIndexing = CSSearchableIndex.default()
    ) {
        self.clock = clock
        spotlightIndexer = SpotlightIndexer(index: spotlightIndex)
        // CASTELLUM_PROFILE_DIR points the app at a disposable profile for
        // agent-driven GUI smokes and manual testing; unset = the real one.
        // (`resolveProfileDirectory` above resolves the same "dir" value for
        // `CastellumApp.init`'s preflight; kept as an explicit chain here,
        // unchanged from before this change, because this branch ALSO
        // decides `customProfile` — Keychain namespacing — which
        // `resolveProfileDirectory` alone doesn't carry enough information
        // to reconstruct correctly.)
        let dir: URL
        // nil = the real profile; anything else gets its own Keychain namespace.
        let customProfile: URL?
        if let profileDirectory {
            dir = profileDirectory
            customProfile = profileDirectory
        } else {
            // S68: env override, else CastellumNativeDev (Debug) / real (Release).
            let resolved = ProfileLocation.current()
            dir = resolved.directory
            customProfile = resolved.isCustom ? dir : nil
        }
        accountStore = AccountStore(
            service: AccountStore.service(forProfileDirectory: customProfile))
        do {
            core = try CastellumCore(profilePath: dir)
            Log.lifecycle.info("app: profile opened")
        } catch {
            // The two launch-blocking-but-recoverable refusals
            // (SchemaTooNew/MigrationFailed — see `CastellumLaunchError`)
            // are meant to be caught by `CastellumApp.init`'s preflight
            // BEFORE `AppModel` is ever constructed — see its doc comment
            // for why: `AppModel` is normally built as a `@StateObject`
            // default value, which runs inside SwiftUI's own machinery, and
            // `NSAlert.runModal()` (a nested run loop) from in there reenters
            // AttributeGraph mid-transaction and aborts (confirmed empirically
            // via this change's own GUI check — not a hypothetical). If we
            // land here anyway (the preflight raced with something, or a
            // caller constructed `AppModel` directly without going through
            // `CastellumApp`), there is no safe way to show that alert from
            // this call stack, so it's still a `fatalError`, unchanged from
            // before this change.
            Log.lifecycle.fault("app: failed to open profile: \(error, privacy: .private)")
            fatalError("Failed to open Castellum profile at \(dir.path): \(error)")
        }
        aiCoordinator = AiCoordinator(core: core, prefs: Preferences.shared)
        // `self.` is required here: the init parameter `profileDirectory`
        // (URL?) shadows the stored property of the same name (URL) within
        // this scope.
        self.profileDirectory = dir
        keyBindings = KeyBindingsStore(directory: dir)
    }

    /// Resolve the profile directory the app will open, the SAME way this
    /// `init` does below: an explicit override (tests), else
    /// `CASTELLUM_PROFILE_DIR`, else `ProfileLocation` (Debug = CastellumNativeDev,
    /// Release = the real default under Application Support). Exposed statically so `CastellumApp.init`'s preflight (see
    /// `preflightProfileOpen`) resolves the identical path — the two must
    /// never disagree about which profile is "the" profile.
    static func resolveProfileDirectory(override profileDirectory: URL?) -> URL {
        if let profileDirectory { return profileDirectory }
        return ProfileLocation.current().directory
    }

    /// Try opening (and migrating) the profile at `dir`, purely to classify
    /// a launch-blocking refusal BEFORE `AppModel` itself — and so SwiftUI's
    /// runtime — exists. See `CastellumApp.init` for why this has to happen
    /// there and not inside this type's own `init`. `nil` means either the
    /// open succeeded, or it failed for some other, non-recoverable reason —
    /// both fall through to the normal `init` below, which reopens the same
    /// (now up to date, if it succeeded here) profile — cheap and
    /// idempotent — and still `fatalError`s on a genuine failure.
    ///
    /// `nonisolated`: `MigrationPreflightController` calls
    /// this off the main thread — on a background queue, while the
    /// migrating window's run loop pump keeps the main thread free — so it
    /// must not be `@MainActor`-isolated like the rest of `AppModel`. The
    /// body only ever touches `CastellumCore`/`CastellumLaunchError`, never
    /// any actor-isolated instance or static state, so opting out here is
    /// sound.
    nonisolated static func preflightProfileOpen(at dir: URL) -> CastellumLaunchError? {
        do {
            _ = try CastellumCore(profilePath: dir)
            return nil
        } catch {
            return CastellumLaunchError.classify(error)
        }
    }

    /// Cheap, read-only check ahead of `preflightProfileOpen(at:)` (the tracker
    /// 2635): does this profile have a pending migration, WITHOUT ever
    /// opening it for real? Feeds `MigrationPreflightController`, which
    /// decides whether to show the "Upgrading your mail database…" window
    /// before calling the real (expensive) `preflightProfileOpen` above.
    ///
    /// `.pending` only when `appliedVersion > 0` — a profile that doesn't
    /// exist yet (or was never migrated) also has `count > 0` (every
    /// migration this build knows is technically "pending" against an
    /// empty schema), but there's nothing to protect and
    /// `preflightProfileOpen` creating+migrating it from empty is fast
    /// enough to need no window. That also covers the XCTest host and the
    /// S64 perf probe for free: both always point at a fresh temp profile
    /// (`TestHost.profileDirectory`), so this always reads `.upToDate` for
    /// them and the window never appears under test.
    static func checkPendingMigrations(at dir: URL) -> MigrationPreflightController.PendingCheck {
        do {
            let info = try CastellumCore.pendingMigrationCount(profilePath: dir)
            return (info.appliedVersion > 0 && info.count > 0)
                ? .pending(count: info.count, heavy: info.hasHeavy)
                : .upToDate
        } catch {
            if let launchError = CastellumLaunchError.classify(error) {
                return .launchBlocking(launchError)
            }
            return .unknown
        }
    }

    /// Show a blocking, native alert for a launch-time profile-open refusal
    /// and quit. Called from `CastellumApp.init`, strictly BEFORE SwiftUI's
    /// runtime (`.run()`) starts — this app has no `LaunchError`/
    /// `ProfileOpenError` window today (see `CastellumLaunchError`'s
    /// header), so `NSAlert` is used directly, the same tool
    /// `AttachmentStripView` already reaches for outside a SwiftUI view
    /// hierarchy. MUST NOT be called from inside `AppModel.init` (a
    /// `@StateObject` default value, evaluated inside SwiftUI's own
    /// machinery) — `NSAlert.runModal()` there reenters AttributeGraph
    /// mid-transaction and aborts; confirmed empirically by this change's
    /// own GUI check, not a hypothetical.
    static func presentLaunchErrorAndQuit(_ error: CastellumLaunchError) -> Never {
        NSApplication.shared.setActivationPolicy(.regular)
        NSApplication.shared.activate(ignoringOtherApps: true)
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = error.title
        alert.informativeText = error.message
        alert.addButton(withTitle: String(localized: "Quit"))
        _ = alert.runModal()
        exit(1)
    }

    deinit {
        periodicSyncTask?.cancel()
        idleWakeObserver?.stop()
        snoozeSweepTask?.cancel()
        sendLaterSweepTask?.cancel()
        undoSendTickTask?.cancel()
        for task in undoSendDrainTasks.values { task.cancel() }
        for task in syncProgressPollTasks.values { task.cancel() }
    }

    /// Run blocking FFI `work` off-actor, then `apply` the result on the main actor.
    /// On failure `errorText` is set and `apply` is NOT run. `always`, if given,
    /// runs on the main actor after EITHER outcome, success or failure — for a
    /// caller (like `reloadAccounts`) that needs a completion signal regardless
    /// of whether the call actually produced data. Background provider sync
    /// deliberately uses `performSync` instead because it must stay non-modal.
    private func load<T: Sendable>(
        _ work: @escaping @Sendable (CastellumCore) throws -> T,
        apply: @escaping @MainActor (T) -> Void,
        always: (@MainActor () -> Void)? = nil
    ) {
        let core = self.core
        Task.detached {
            do {
                let result = try work(core)
                await MainActor.run { apply(result) }
            } catch {
                // This is the generic FFI-error catch every non-sync core
                // call funnels through. Before this line the error was only
                // ever shown in the modal `errorText` alert — if the user
                // dismissed it without a screenshot, it was gone. Now it's
                // retrievable afterwards too.
                Log.ui.error("core call failed: \(error, privacy: .private)")
                await MainActor.run {
                    self.errorText = String(describing: error)
                }
            }
            if let always {
                await MainActor.run { always() }
            }
        }
    }

    func bootstrap() {
        Log.lifecycle.info("app: bootstrap started")
        // Populates `accounts`, then — inside that same apply closure, once
        // the ids are actually known — reconciles undo-sends (S25
        // crash-safety hold) before draining the outbox. `accounts` is `[]`
        // here synchronously, so the reconcile CANNOT be a separate call
        // against this (still-empty) array; see `reconcileUndoSendsThenReplay`.
        reloadAccounts()
        reloadProjects()   // powers the always-visible Active Work sidebar entry
        reloadSmartMailboxes()
        reloadRules()
        reloadTags()
        // Contacts are derived from mail already stored in the local database.
        // M6: NOT refreshed here any more. Everything that reads contacts
        // works from the aggregates the previous refresh persisted; the
        // refresh runs once the first list has been delivered (see
        // `armLaunchContactsRefresh`), so it no longer contends with the
        // first list for the database lock.
        armLaunchContactsRefresh()
        // Populate the Snoozed badge/list immediately at launch (S43): the due
        // sweep below only reloads when something actually became due, so
        // without this the badge would read 0 until the first snooze/unsnooze
        // action or sidebar click, even with rows already snoozed from a
        // previous session.
        reloadSnoozedMessages()
        reloadSpotlightIndexingEnabled()
        // S64 GUI launch probe (`scripts/perf/launch-probe.sh`): a real launch
        // against a real (cloned) profile, timing ONLY the local read path to
        // the first rendered list — no network, so no IMAP sync, periodic
        // resync or Ollama probe. `select(unifiedRole:)` still runs (it's
        // exactly the read path being measured) and is a local DB call.
        // `TestHost.isPerfProbe` is DEV/TEST-ONLY (see its doc comment) —
        // this branch must never be taken in a shipped build.
        guard !TestHost.isPerfProbe else {
            select(unifiedRole: "inbox")
            startSnoozeSweep()
            return
        }
        onAccountsLoaded { [weak self] in self?.selectDefaultMailboxIfNothingSelected() }
        retryPendingAccountRemovals()
        reSyncAll()
        startPeriodicResync()
        startIdleForStoredAccounts()
        startIdleWakeObserver()
        startSnoozeSweep()
        startNotifications()
        startSendLaterSweep()
        startOutboxReachability()
        // UI-script runs (UIT-1) must not probe a local Ollama or write prefs.
        // (Also skipped under XCTest now that tests drive `bootstrap()`.)
        if !TestHost.suppressesLaunchSideEffects { probeOllamaOnFirstLaunchIfNeeded() }
    }

    /// Builds the pure `PaletteContext` the command palette's availability
    /// predicate needs, from live state. The only production caller of
    /// `PaletteContext.init` — see `CommandPaletteAvailability.swift`.
    var paletteContext: PaletteContext {
        PaletteContext(
            hasSelectedMessage: selectedMessage != nil,
            messageBodyAvailable: messageBodyActionsAvailable(detail),
            aiEnabled: Preferences.shared.aiEnabled,
            composerOpen: ComposeWindowController.hasOpenWindow,
            removalUndoAvailable: !removalUndoStack.isEmpty,
            tagCount: tags.count)
    }

    /// Whether a bare-key command can act right now. Only the MM-8 tag digits
    /// are gated: a digit with no tag in its slot, or with nothing selected, is
    /// passed through (the key does nothing and is not swallowed).
    func isBareKeyCommandAvailable(_ id: CommandID) -> Bool {
        guard let n = id.tagSlotIndex else { return true }
        return n <= tags.count && !currentSelectionIds().isEmpty
    }

    /// Digit key `n` (1...9): toggle the nth tag in Settings order on the
    /// selection with the same add-if-any-lacks rule as the picker. No-op for
    /// an empty slot.
    func toggleTagSlot(_ n: Int) {
        guard n >= 1, n <= tags.count else { return }
        toggleTag(tags[n - 1].keyword)
    }

    /// True while the `g l` go-to-tag picker is shown (RootView's popover binds to it).
    @Published var goToTagPickerShown = false

    /// Summary of the current selection for the row context menu
    /// (`MessageContextMenu.entries`). O(selection) over in-memory headers.
    func messageMenuContext() -> MessageMenuContext {
        let ids = Set(currentSelectionIds())
        let headers = messages.filter { ids.contains($0.id) }
        guard !headers.isEmpty else { return .empty }
        let readOnlyIds = Set(accounts.filter { writeState(for: $0.id).isReadOnly }.map(\.id))
        let targetAccounts = Set(headers.map(\.accountId))
        let readOnly = accounts.filter { targetAccounts.contains($0.id) && readOnlyIds.contains($0.id) }
        let roles = Set(headers.compactMap { h in
            mailboxes[h.accountId]?.first { $0.id == h.mailboxId }?.role
        })
        let primary = selectedMessage.flatMap { m in headers.first { $0.id == m.id } } ?? headers[0]
        let vipKey = primary.fromAddress?.trimmingCharacters(in: .whitespaces).lowercased() ?? ""
        return MessageMenuContext(
            selectionCount: headers.count,
            anyUnread: headers.contains { !$0.isRead },
            anyRead: headers.contains { $0.isRead },
            anyFlagged: headers.contains { $0.isStarred },
            allFlagged: headers.allSatisfy { $0.isStarred },
            // `detail` loads asynchronously after a right-click selects a row,
            // so a header that already has its body counts as available.
            bodyAvailable: messageBodyActionsAvailable(detail) || primary.hasBody == true,
            aiEnabled: Preferences.shared.aiEnabled,
            readOnlyAccounts: readOnly.map { .init(id: $0.id, email: $0.email) },
            allTargetsReadOnly: targetAccounts.isSubset(of: readOnlyIds),
            sourceRoles: roles,
            isSnoozedList: selectedIsSnoozed,
            threadMuted: primary.threadMuted,
            senderIsVip: !vipKey.isEmpty && vips.contains { $0.address == vipKey },
            aiEmbeddingModelSet: !Preferences.shared.aiEmbeddingModel.isEmpty,
            moveDestinations: moveMenuDestinations,
            recentMoveIds: moveMenuAccountId.map { Preferences.shared.recentMoveDestinations(accountId: $0) } ?? [],
            targetMailboxIds: Set(headers.map(\.mailboxId)),
            multiAccount: targetAccounts.count > 1,
            junkMailboxAvailable: targetAccounts.contains { acct in
                (mailboxes[acct] ?? []).contains { $0.role == "junk" }
            })
    }

    /// The account a "Move to" menu would file into: the selection's single
    /// account, or nil for no/multi-account selections (mailbox ids are per account).
    private var moveMenuAccountId: String? {
        guard case .ok(let accountId) = MoveToFolderPlan.eligibility(
            ids: currentSelectionIds(), rows: messages) else { return nil }
        return accountId
    }

    /// Folders offered by "Move to ▸": selectable, minus Drafts/Sent/Trash/Junk
    /// (Trash and Junk have their own verbs and guards).
    private var moveMenuDestinations: [MessageMenuContext.MoveDestination] {
        guard let accountId = moveMenuAccountId else { return [] }
        return MoveToFolderPlan.destinations(accountId: accountId, mailboxes: mailboxes)
            .filter { !MessageContextMenu.protectedMoveRoles.contains($0.role ?? "") }
            .map { .init(id: $0.id, name: $0.name) }
    }

    // MARK: - First-launch Ollama auto-enable

    /// On a fresh install (no stored `aiEnabled` preference at all), probe
    /// the standard local Ollama endpoint; if it answers, turn AI on for the
    /// user pointed at it. Never touches anything once a preference has been
    /// stored — including by this method itself, which always writes one.
    func probeOllamaOnFirstLaunchIfNeeded() {
        let prefs = Preferences.shared
        guard AiAutoEnableProbe.shouldProbe(
            hasStoredAiEnabledPreference: prefs.hasStoredAiEnabledPreference
        ) else { return }
        let endpoint = AiAutoEnableProbe.probeEndpoint
        let timeout = AiAutoEnableProbe.probeTimeoutSeconds
        Task.detached {
            let succeeded = await Self.probeOllamaTags(endpoint: endpoint, timeout: timeout)
            let decision = AiAutoEnableProbe.decide(endpoint: endpoint, probeSucceeded: succeeded)
            await MainActor.run {
                switch decision {
                case .enable(let endpoint, let reason, let detected):
                    prefs.aiEndpoint = endpoint
                    prefs.aiAutoEnableReason = reason
                    prefs.aiAutoDetectedEndpoint = detected
                    prefs.aiEnabled = true
                case .doNotEnable:
                    // Explicitly (re-)write `false` so `hasStoredAiEnabledPreference`
                    // becomes true and this probe never runs again this install,
                    // even though the value itself hasn't changed.
                    prefs.aiEnabled = false
                }
            }
        }
    }

    /// Off-actor GET `<endpoint>/api/tags` with a short timeout. Answers only
    /// with success/failure — the response body is irrelevant to the caller,
    /// which only needs to know whether something is listening.
    private static func probeOllamaTags(endpoint: String, timeout: TimeInterval) async -> Bool {
        guard let base = URL(string: endpoint) else { return false }
        let url = base.appendingPathComponent("api/tags")
        var request = URLRequest(url: url)
        request.timeoutInterval = timeout
        let config = URLSessionConfiguration.ephemeral
        config.timeoutIntervalForRequest = timeout
        config.timeoutIntervalForResource = timeout
        let session = URLSession(configuration: config)
        defer { session.finishTasksAndInvalidate() }
        do {
            let (_, response) = try await session.data(for: request)
            guard let http = response as? HTTPURLResponse else { return false }
            return (200..<300).contains(http.statusCode)
        } catch {
            return false
        }
    }

    // MARK: - background replay (MailMate/Superhuman style: no manual button)

    /// Coalesce a burst of write actions into a single replay pass ~1.5s later.
    func scheduleReplay() {
        replayDebounce?.cancel()
        replayDebounce = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            if Task.isCancelled { return }
            self?.replayNow()
        }
    }

    /// Replay queued operations for every account with stored credentials.
    /// Off-actor (blocking FFI); serialized via `isReplaying`. Offline-safe: a
    /// failed pass leaves ops queued/failed to drain on the next trigger.
    func replayNow() {
        // Already running: remember to drain again so ops enqueued mid-pass aren't
        // stranded until the next sync.
        guard !isReplaying else { needsReplay = true; return }
        isReplaying = true
        let core = self.core
        let store = accountStore
        let ids = accounts.map(\.id)
        Task.detached {
            var applied: [String] = []
            for id in ids {
                // Local-empty preflight: after Cancel All (or an already-drained
                // queue) there is nothing runnable, so skip credential lookup and
                // network work entirely. A concurrent enqueue schedules its own
                // replay, and the core's claim-before-apply guard remains the
                // correctness authority for anything that slips through.
                guard let pending = try? core.pendingOperations(accountId: id),
                      !pending.isEmpty
                else { continue }
                if let cfg = try? store.configJSON(for: id),
                   let report = try? core.replayOperations(configJSON: cfg),
                   report.applied > 0 {
                    applied.append(id)
                }
            }
            await MainActor.run {
                self.isReplaying = false
                self.refreshPendingCount()
                // Ops that applied (move/delete/expunge) changed server-side state;
                // re-sync those accounts so the destination/Trash reflect it locally.
                self.removalUndoBanner.replayApplied(accountIds: applied)
                for id in applied {
                    if let cfg = self.storedConfig(for: id) { self.sync(config: cfg) }
                }
                if self.needsReplay {
                    self.needsReplay = false
                    self.replayNow()
                }
            }
        }
    }

    /// Recompute the outbox badge from the core's undelivered-op count.
    func refreshPendingCount() {
        let core = self.core
        let ids = accounts.map(\.id)
        Task.detached {
            var total = 0
            var messages = 0
            for id in ids {
                total += (try? core.pendingOperations(accountId: id).count) ?? 0
                messages += OutboxSendPresentation.count((try? core.outboxSends(accountId: id)) ?? [])
            }
            await MainActor.run {
                self.pendingCount = total
                self.outboxMessageCount = messages
                self.outboxRevision &+= 1
            }
        }
    }

    /// Re-sync every account that has stored credentials, using the Keychain —
    /// no password prompt. Runs in the background at launch.
    func reSyncAll() {
        let store = accountStore
        Task.detached {
            let ids = (try? store.allAccountIDs()) ?? []
            for id in await MainActor.run(body: { self.excludingRemoved(ids) }) {
                if let cfg = try? store.configJSON(for: id) {
                    await MainActor.run { self.sync(config: cfg) }
                }
            }
        }
    }

    /// View > Expand All Quotes: open every folded quote and signature of the plain-text message on screen.
    func expandAllQuotes() {
        guard let text = detail?.textBody else { return }
        plainFold.expandAll(PlainTextStructure.parse(text))
    }

    /// The hover card for one header address (MM-13, #2703): the contact row for it (counts and
    /// dates already stored locally) plus the VIP state. Off-main, local only, never throws.
    func addressCardContent(address: String, name: String?) async -> AddressCardContent {
        let core = self.core
        let key = address.trimmingCharacters(in: .whitespaces).lowercased()
        let contact = await Task.detached { () -> Contact? in
            (try? core.listContacts(query: key, includeHidden: true, limit: 20))?.first { $0.email == key }
        }.value
        return AddressCardContent.make(
            name: name, address: address, contact: contact,
            isVip: vips.contains { $0.address == key }, myAddresses: myAddresses)
    }

    /// Where the web view gets this message's inline (cid:) images: the attachment cache, and a
    /// download over the account's own connection when a part is not cached yet. The Keychain is
    /// read lazily on the loading queue, only if a download is actually needed (#3442).
    func inlineImageSource(for detail: MessageDetail) -> InlineImageSource? {
        let parts = (detail.attachments ?? []).filter { $0.isInline && $0.contentId != nil }
        guard !parts.isEmpty else { return nil }
        let core = self.core
        let store = accountStore
        let messageId = detail.header.id
        let accountId = detail.header.accountId
        return InlineImageSource(attachments: parts) { ordinal in
            func bytes(_ r: AttachmentDownloadResult?) -> Data? {
                guard let r, r.status == "cached" || r.status == "downloaded", let path = r.localPath
                else { return nil }
                return try? Data(contentsOf: URL(fileURLWithPath: path))
            }
            if let data = bytes(try? core.attachmentCacheStatus(messageId: messageId, ordinal: ordinal)) {
                return data
            }
            guard let config = try? store.configJSON(for: accountId) else { return nil }
            return bytes(try? core.downloadAttachment(
                configJSON: config, messageId: messageId, ordinal: ordinal))
        }
    }

    /// Config JSON for an account from the Keychain (used by sync/replay).
    func storedConfig(for accountID: String) -> String? {
        try? accountStore.configJSON(for: accountID)
    }

    /// The account's stored config reshaped into the core's `SendConfig`
    /// (`account_id` + `from_email` + a nested `smtp` object). Every caller of
    /// `castellum_send_pending` must go through this: the stored shape is flat
    /// and has no `from_email`, so passing it straight through is a silent
    /// deserialisation failure, not a send.
    func sendConfigJSON(for accountID: String) -> String? {
        guard let stored = storedConfig(for: accountID),
              let config = try? AccountConfig.decode(fromJSON: stored) else { return nil }
        return try? config.sendConfigJSON(plainTextFlowed: Preferences.shared.sendFlowed)
    }

    /// The SMTP host:port an account currently sends through, for a failed
    /// send's inline caption. `nil` when there is no stored
    /// config to read (e.g. the account was just removed).
    func smtpEndpoint(for accountID: String) -> String? {
        guard let json = storedConfig(for: accountID),
              let config = try? AccountConfig.decode(fromJSON: json) else { return nil }
        return config.effectiveSMTP.endpoint
    }

    // MARK: - periodic background re-sync

    /// Starts (or restarts) the periodic loop: about once a minute, every
    /// account with stored credentials is checked against
    /// `PeriodicSyncScheduler.isDue` and re-synced once its interval has
    /// elapsed — 5 minutes normally, 15 minutes while its last sync failed.
    /// An account already mid-sync is always skipped, so a slow sync is
    /// never queued twice. This is what keeps a launch failure from staying
    /// orange forever when the user does nothing else. Idempotent (cancels
    /// any prior loop first); cancelled in `deinit`.
    func startPeriodicResync() {
        periodicSyncTask?.cancel()
        periodicSyncTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: Self.periodicResyncPollInterval)
                if Task.isCancelled { return }
                await self?.runDuePeriodicSyncs()
            }
        }
    }

    /// Nudge every account's IDLE thread on sleep/wake and network-path
    /// change (S31), so a connection that would otherwise sit idle through a
    /// dropped/renegotiated network catches up immediately rather than
    /// waiting for its next scheduled re-issue or the periodic safety net.
    /// Idempotent (stops any prior observer first); stopped in `deinit`.
    func startIdleWakeObserver() {
        idleWakeObserver?.stop()
        let observer = IdleWakeObserver(center: NSWorkspace.shared.notificationCenter) { [weak self] in
            guard let self else { return }
            Task.detached {
                let store = self.accountStore
                let ids = (try? store.allAccountIDs()) ?? []
                for id in await MainActor.run(body: { self.excludingRemoved(ids) }) {
                    try? self.core.nudgeIdle(accountId: id)
                }
            }
        }
        observer.start()
        idleWakeObserver = observer
    }

    /// Crash-safe snooze due-check (S43): runs once immediately (covers
    /// "due while the app was closed" — SQLite, not an in-memory timer, is
    /// the source of truth) and then every `snoozeSweepInterval`, mirroring
    /// `startPeriodicResync`'s Task-loop shape rather than a new mechanism.
    func startSnoozeSweep() {
        snoozeSweepTask?.cancel()
        snoozeSweepTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                await self.runSnoozeSweep()
                try? await Task.sleep(nanoseconds: Self.snoozeSweepInterval)
            }
        }
    }

    private func runSnoozeSweep() async {
        let core = self.core
        let now = Int64(Date().timeIntervalSince1970)
        guard let due = try? await Task.detached { try core.unsnoozeDue(nowEpoch: now) }.value,
              !due.isEmpty
        else { return }
        let accounts = Set(due.map(\.accountId))
        // FP14: the Snoozed badge reads `snoozedMessages`, so reload it even
        // when another list is on screen.
        self.reloadSnoozedMessages()
        if !self.selectedIsSnoozed {
            self.reloadCurrentMessages()
        }
        for a in accounts { self.reloadCounts(a) }
    }

    // MARK: - Notifications, VIPs and the Dock badge (S59)

    /// Start the outbox drain loop and the Dock badge. Called from
    /// `bootstrap()` only, so a bare `AppModel` under XCTest never starts
    /// either (and both are TestHost-guarded anyway).
    private func startNotifications() {
        reloadVips()
        let coordinator = NotificationCoordinator(model: self)
        notificationCoordinator = coordinator
        coordinator.start()
        let badge = DockBadgeController(model: self)
        dockBadgeController = badge
        badge.recompute()
        let menuBar = MenuBarCounterController(model: self)
        menuBarCounterController = menuBar
        menuBar.recompute()
    }

    /// Reload the VIP list and the VIP unread count.
    func reloadVips() {
        load({ core in (try core.listVips(), try core.vipUnreadCount(accountIds: nil)) }) { pair in
            self.vips = pair.0
            self.vipUnreadCount = pair.1
        }
    }

    /// Add or remove a VIP by address (Settings), then refresh lists so
    /// `fromVip` on visible rows updates.
    func setVip(address: String, displayName: String? = nil, vip: Bool) {
        load({ try $0.setVip(address: address, displayName: displayName, vip: vip) }) { _ in
            self.reloadVips()
            self.reloadCurrentMessages()
        }
    }

    /// The `v` command: toggle VIP for the primary selection's sender.
    func toggleVipForSelectedSender() {
        guard let m = selectedMessage, let address = m.fromAddress, !address.isEmpty else { return }
        // Decide from the VIP list, not `m.fromVip`: a reload keeps the
        // selection's stale header copy, so its flag can lag the truth.
        let key = address.trimmingCharacters(in: .whitespaces).lowercased()
        let isVip = vips.contains { $0.address == key }
        setVip(address: address, displayName: m.fromName, vip: !isVip)
    }

    /// True while the list shows the VIPs view (the unified inbox filtered by
    /// `is:vip` through the ordinary search path — no second filter path).
    var isVipViewSelected: Bool {
        selectedUnifiedRole == "inbox" && searchText == "is:vip"
    }

    /// Default selection (FP02): when the window has nothing selected at all
    /// (fresh launch, or the first account just added) show the unified inbox
    /// instead of an empty list. Never steals an existing selection, so a
    /// second account add or a periodic reload leaves the user where they are.
    func selectDefaultMailboxIfNothingSelected() {
        guard selectedMailbox == nil, selectedUnifiedRole == nil,
              selectedSmartMailboxId == nil, !selectedIsSnoozed, selectedTagKey == nil,
              !isVipViewSelected, searchText.isEmpty, !accounts.isEmpty else { return }
        select(unifiedRole: "inbox")
    }

    /// Sidebar "VIPs" row: All Inboxes, filtered by `is:vip`.
    func selectVips() {
        select(unifiedRole: "inbox")
        searchText = "is:vip"
        runSearch()
    }

    /// Open a message by id (a clicked notification or Spotlight result): select
    /// its mailbox, show it, and bring the window forward. `completion` runs on
    /// the main actor with what happened (`.notFound` when the message is no
    /// longer in the local database); a thrown core error keeps the generic
    /// `errorText` behaviour and does not call it.
    func revealMessage(id: String, completion: (@MainActor (MessageRevealOutcome) -> Void)? = nil) {
        load({ core -> (MessageDetail?, [Mailbox]) in
            guard let detail = try core.getMessage(id: id) else { return (nil, []) }
            // On a cold launch `mailboxes` may not hold this account yet; the
            // fetched list is only used to find the mailbox to select.
            let fetched = (try? core.listMailboxes(accountId: detail.header.accountId)) ?? []
            return (detail, fetched)
        }, apply: { detail, fetched in
            // Never steal focus from whoever is running the test suite.
            defer { if !TestHost.isActive { NSApp.activate(ignoringOtherApps: true) } }
            guard let detail else {
                completion?(.notFound)
                return
            }
            let header = detail.header
            let known = self.mailboxes[header.accountId] ?? []
            let mailbox = known.first(where: { $0.id == header.mailboxId })
                ?? fetched.first(where: { $0.id == header.mailboxId })
            if let mailbox { self.select(mailbox: mailbox) }
            self.showDetail(for: header)
            self.smartMailboxNotice = nil
            completion?(.shown)
        })
    }

    /// The Spotlight entry point (#3382): reveal the indexed message; if it is
    /// gone (moved, archived, deleted since it was indexed) say so in the
    /// existing notice banner and drop that one dead item from the index.
    func openSpotlightResult(messageId: String) {
        revealMessage(id: messageId) { [weak self] outcome in
            guard let self, outcome == .notFound else { return }
            self.smartMailboxNotice = String(
                localized: "That message is no longer in this mailbox. It may have been moved or deleted.")
            // Not gated on the opt-in flag: removing a dead item is always right.
            let indexer = self.spotlightIndexer
            Task.detached { await indexer.remove(ids: [messageId]) }
        }
    }

    /// "Mark as Read" on a banner: the automatic path (always local, a `\Seen`
    /// STORE only when writes are enabled), so it never raises the
    /// evaluation-mode refusal that an explicit toggle does.
    func markReadAuto(messageId: String, accountId: String) {
        load({ try $0.markReadAuto(messageId: messageId, isRead: true) }) { _ in
            self.reloadCounts(accountId)
            self.reloadCurrentMessages()
        }
    }

    private func runDuePeriodicSyncs() async {
        let store = accountStore
        let now = clock()
        let allIds = excludingRemoved((try? await Task.detached { try store.allAccountIDs() }.value) ?? [])
        for id in allIds {
            let state = PeriodicSyncScheduler.AccountState(
                lastAttempt: periodicLastAttempt[id],
                isUnreachable: accountSyncStatuses[id]?.state.isFailure ?? false,
                isSyncing: accountSyncStatuses[id]?.state == .syncing || inFlightSyncs[id] != nil,
                idleActive: (try? core.isIdleRunning(accountId: id)) ?? false
            )
            guard PeriodicSyncScheduler.isDue(state, now: now) else { continue }
            periodicLastAttempt[id] = now
            let cfg: String? = await Task.detached { try? store.configJSON(for: id) }.value
            if let cfg {
                sync(config: cfg)
            }
        }
    }

    /// The tracker report for a message's blocked remote content (#2812),
    /// memoised per message so re-rendering the reader never re-parses a large
    /// HTML body. The analysis is pure and offline.
    ///
    /// Never blocks the main thread: a cache miss returns nil (the banner shows
    /// its plain wording) and starts the analysis on a background task, which
    /// publishes the result back on the main actor so the banner updates.
    func remoteContentReport(for detail: MessageDetail) -> RemoteContentReport? {
        guard detail.hasRemoteContent ?? false, let html = detail.sanitizedHtml, !html.isEmpty else {
            return nil
        }
        let key = "\(detail.header.id)#\(html.utf8.count)"
        if let cached = remoteReportCache[key] { return cached }
        guard !remoteReportsInFlight.contains(key) else { return nil }
        remoteReportsInFlight.insert(key)
        Task.detached(priority: .userInitiated) {
            let report = try? CastellumCore.remoteContentReport(sanitizedHtml: html)
            await MainActor.run {
                self.remoteReportsInFlight.remove(key)
                guard let report else { return }
                if self.remoteReportCache.count >= 64 { self.remoteReportCache.removeAll() }
                self.remoteReportCache[key] = report
            }
        }
        return nil
    }
    @Published private var remoteReportCache: [String: RemoteContentReport] = [:]
    private var remoteReportsInFlight: Set<String> = []

    /// The sidebar status line for `accountId` at `now` (#2832). Reads only
    /// in-memory state, plus the core's in-memory IDLE-health flag: no network.
    func syncStatusLine(for accountId: String, now: Date) -> SyncStatusLine {
        SyncStatusLineBuilder.make(
            status: accountSyncStatuses[accountId],
            progress: accountSyncProgress[accountId],
            idleHealthy: (try? core.isIdleRunning(accountId: accountId)) ?? false,
            lastChecked: accountLastChecked[accountId],
            now: now)
    }

    /// Immediately re-sync one account, bypassing the periodic schedule —
    /// used by the sidebar status dot and the Status dashboard's Retry
    /// button. Does not touch `periodicLastAttempt`, so it never delays the
    /// next scheduled check.
    func retrySync(accountId: String) {
        guard let cfg = storedConfig(for: accountId) else {
            errorText = "Stored credentials are unavailable for this account."
            return
        }
        sync(config: cfg)
    }

    /// Start an account's supervised IDLE connection off-actor. Never surfaces
    /// an error to the user: IDLE is a freshness OPTIMISATION, not a
    /// requirement — an account that fails to start it still syncs via
    /// PeriodicSyncScheduler exactly as it always has.
    private func startIdleIfPossible(accountId: String, configJSON: String) {
        if let override = idleStartOverride {
            override(accountId, configJSON)
            return
        }
        // Under XCTest only an override may start IDLE: a test must never open
        // a connection to whatever host its config names.
        guard !TestHost.isActive else { return }
        let core = self.core
        Task.detached { try? core.startIdle(configJSON: configJSON) }
    }

    /// (Re)start IDLE and wait until the old thread is gone, for a caller that
    /// is already off the main actor and must have IDLE settled before it goes
    /// on (`updateFolderSelection`). `nonisolated` so `core.startIdle`'s join
    /// never runs on the main actor.
    private nonisolated func restartIdleOffActor(accountId: String, configJSON: String) async {
        if let override = await MainActor.run(body: { self.idleStartOverride }) {
            override(accountId, configJSON)
        } else if !TestHost.isActive {
            // Under XCTest only an override may start IDLE: a test must never
            // open a connection to whatever host its config names.
            try? core.startIdle(configJSON: configJSON)
        }
    }

    /// Test seam: observes every IDLE (re)start instead of opening a real
    /// connection (`startIdle` against a loopback server that never speaks
    /// IDLE proves nothing about WHICH config was used).
    var idleStartOverride: ((_ accountId: String, _ configJSON: String) -> Void)?

    /// Start every already-stored account's supervised IDLE connection at
    /// launch. Mirrors `startPeriodicResync()`'s call site in `bootstrap()` so
    /// both freshness mechanisms come up together. Off-actor, like every other
    /// `accountStore` access in this file — `allAccountIDs`/`configJSON` are
    /// Keychain-backed and must not block the main actor.
    private func startIdleForStoredAccounts() {
        let core = self.core
        let store = accountStore
        Task.detached {
            let ids = (try? store.allAccountIDs()) ?? []
            for id in await MainActor.run(body: { self.excludingRemoved(ids) }) {
                if let cfg = try? store.configJSON(for: id) {
                    Task.detached { try? core.startIdle(configJSON: cfg) }
                }
            }
        }
    }

    /// Add an account: sync once, and on success persist its credentials so
    /// future syncs/replays run without re-entering the password. `completion`
    /// (optional) reports the final `AccountSyncStatus` — ok or classified
    /// failure — for AccountSetupSheet's inline Add/Reconnect result (the tracker
    /// #2175); it does not duplicate `performSync`, only observes its result.
    func addAccount(
        accountID: String, configJSON: String,
        completion: (@MainActor (AccountSyncStatus) -> Void)? = nil
    ) {
        Log.accounts.info("account: add requested for \(accountID, privacy: .private)")
        // The user is explicitly adding this id again: a pending removal no longer applies.
        if removedAccountIds.contains(accountID) { removedAccountIds.remove(accountID) }
        // Monotonic guard (PR #158): `onSuccess` always runs, even for a
        // generation a newer attempt has since superseded (that's the #2495
        // fix), and both saves below store THIS attempt's captured
        // `configJSON` — a stale attempt must not regress the Keychain back
        // to an older config a newer generation already saved. The same
        // generation saving twice (early, then on completion) is fine: same
        // config. The normal in-order case (including #2495's "retry fails,
        // original's late success still saves") is unaffected.
        // Returns whether THIS generation's config is now the stored one.
        let persistConfig: @MainActor (UInt64) -> Bool = { generation in
            // A removal that landed while this attempt was running wins: never write the
            // credentials of a removed account back (#3439).
            guard !self.removedAccountIds.contains(accountID) else { return false }
            let lastSaved = self.lastSavedGeneration[accountID] ?? 0
            guard generation >= lastSaved else {
                Log.accounts.info("account: skipping stale credential save for \(accountID, privacy: .private) (generation \(generation, privacy: .public) superseded by an already-saved \(lastSaved, privacy: .public))")
                return false
            }
            do {
                try self.accountStore.save(accountID: accountID, configJSON: configJSON)
                self.lastSavedGeneration[accountID] = generation
                Log.accounts.info("account: credentials saved for \(accountID, privacy: .private)")
                return true
            } catch {
                Log.accounts.error("account: credential save failed for \(accountID, privacy: .private): \(error, privacy: .private)")
                self.errorText = "The account's credentials were not saved: \(error)"
                return false
            }
        }
        performSync(config: configJSON, onLoggedIn: { generation in
            // Save as soon as the server has ACCEPTED this config, not only
            // once the whole first sync returns (PR #158 GUI check, 33k-
            // message GreenMail adds): by now the core has created the
            // account row and the poller lists the account, and a large
            // first sync can run for minutes — or never return, if the app
            // quits. Saving only in `onSuccess` left the listed account with
            // no stored config for all that time: Re-authenticate showed an
            // empty form, and `reSyncAll`/the periodic scheduler (both walk
            // `accountStore.allAccountIDs()`) could never resume it, so an
            // interrupted first sync was orphaned for good. A wrong password
            // never gets here (no live progress snapshot without a login).
            _ = persistConfig(generation)
        }, onSuccess: { report, generation in
            let saved = persistConfig(generation)
            // Harmless regardless of generation: a plain reload/refresh of
            // durable state that's already idempotent (and, per the #2495
            // fix above, already ran unconditionally on every completed
            // sync before this change too).
            self.reloadAccounts(then: { self.selectDefaultMailboxIfNothingSelected() })
            self.refreshContactsDebounced()
            // First sync imports descriptors; start draining bodies right away.
            self.scheduleBackgroundHydration(accountId: accountID)
            // IDLE follows the config that is NOW stored, never a stale one
            // (S31b review, finding c). `onSuccess` also runs for attempts a
            // newer one superseded, and its `configJSON` is this attempt's
            // own: a superseded attempt finishing late must not (re)start
            // IDLE on it — only the latest generation, and only if its save
            // actually took, so IDLE never runs on credentials the Keychain
            // does not hold. (`core.startIdle` restarts a running thread, so
            // a stale start would also kill the good one.)
            if saved, self.syncGenerations[accountID] == generation {
                self.startIdleIfPossible(accountId: accountID, configJSON: configJSON)
            }
            // S58: the FIRST sync is also "messages arriving" — index it
            // incrementally too, exactly like every later `sync(config:)`.
            if self.spotlightIndexingEnabled {
                let core = self.core
                let indexer = self.spotlightIndexer
                Task.detached { await indexer.indexRecentSync(core: core, report: report) }
            }
            // A panic-guarded partial failure still lands here (performSync's
            // FFI call didn't throw) but already reported itself via
            // `onFailure` below — only report success once, when the status
            // that dashboard/sidebar/sheet all read actually agrees.
            if let status = self.accountSyncStatuses[accountID], status.state == .ok {
                completion?(status)
            }
        }, onFailure: { status in
            completion?(status)
        })
    }

    /// Account-setup "Test Connection" (S62): runs the blocking IMAP + SMTP
    /// preflight on a detached task so the sheet never blocks the main
    /// thread on network I/O, and touches NO model state — nothing is saved,
    /// no sync status or spinner changes. The caller owns staleness (a newer
    /// test or a dismissed sheet simply ignores an older result).
    nonisolated func testConnection(configJSON: String) async -> Result<ConnectionTestReport, Error> {
        let core = self.core
        return await Task.detached {
            Result { try core.testConnection(configJSON: configJSON) }
        }.value
    }

    /// Test seam: overrides the probe function
    /// `addAccountWithPreflight` runs — nil (production default) means the
    /// real `testConnection` round trip. Tests substitute a fake so the
    /// probe-gating DECISION can be exercised deterministically, with no
    /// GreenMail and no real Keychain — same override-closure shape as
    /// `onUndoReopenDraft` above.
    var connectionProbeOverride: ((String) async -> Result<ConnectionTestReport, Error>)?

    /// MM-14 test/UIT-1 seam, mirroring `connectionProbeOverride`: when set,
    /// `autoconfigLookup(domain:)` returns this instead of calling the core, so
    /// tests and UIT-1 scenarios never touch DNS or the network. Takes only the
    /// domain, exactly like the real call.
    var autoconfigLookupOverride: ((String) async -> AutoconfigReport)?

    /// Look up server settings for a mail DOMAIN (MM-14) off the main actor.
    /// Returns nil when the user turned lookups off in Settings (no call is
    /// made at all) or the call itself failed (the form then behaves as it does
    /// without a lookup). The core bounds the call by its own 5 s deadline; a
    /// caller that has since moved on simply ignores the late result.
    func autoconfigLookup(domain: String) async -> AutoconfigReport? {
        guard Preferences.shared.autoconfigLookupEnabled else { return nil }
        if let override = autoconfigLookupOverride { return await override(domain) }
        // Under XCTest and UIT-1 scenarios the network is never touched: an
        // un-faked lookup answers "not found" (fixtures opt in via the override).
        if TestHost.suppressesLaunchSideEffects { return AutoconfigReport(status: "not_found") }
        return await Task.detached(priority: .userInitiated) {
            try? CastellumCore.autoconfigLookup(domain: domain)
        }.value
    }

    ///: preflight IMAP + SMTP before a NEW or re-authenticated
    /// account's first sync — fail fast on a bad password/host rather than
    /// discovering it only after a long first sync. Runs the exact probe the
    /// "Test Connection" button uses (`testConnection`, i.e.
    /// `castellum_test_connection` — no duplicated connect logic, per the
    /// S62/#159 design this reuses rather than re-implements).
    ///
    /// `priorTest`, when given, is the caller's last Test Connection result
    /// plus the exact `AccountConfig` it tested — normally
    /// `AccountSetupSheet.testPhase`'s `.done` case. The probe is skipped
    /// ONLY when `priorTest.tested.connectionSettings` (host/port/user/
    /// password/TLS/SMTP — deliberately NOT the S62 sync-scope choices) is
    /// byte-identical to `connectionConfig.connectionSettings` AND it passed
    /// both legs: don't probe twice, but a changed field (or a prior
    /// failure) always re-probes.
    ///
    /// `addAccount` — and with it credential persistence and the sync
    /// itself — runs ONLY once a passing probe (fresh or prior) clears it;
    /// this function never touches `addAccount`'s own body, only decides
    /// whether to call it. On a failing leg, `onProbeComplete` gets the
    /// fresh report (the same shape `ConnectionTestPresentation` renders for
    /// the manual button) and `completion` is never invoked — nothing is
    /// saved, nothing syncs. `onProbeRequestFailed` covers the probe
    /// REQUEST itself failing (e.g. a JSON-encoding error), distinct from a
    /// leg reporting failure. `onProbePassed` fires exactly once, right
    /// before `addAccount` is called (skip path or fresh-pass path alike) —
    /// the caller's only synchronous-or-not signal that the real sync is
    /// about to start, e.g. to flip a "connecting" UI state at the right
    /// moment.
    ///
    /// `isStillWanted` (found missing in review of PR #163 — Cancel during
    /// the probe used to still save the account) is re-checked IMMEDIATELY
    /// before `addAccount` is called, on both the skip path and the
    /// fresh-probe-pass path: a probe that resolves as passing AFTER the
    /// caller has moved on (Cancel, or the sheet disappearing) must never
    /// persist credentials or start a sync nobody asked for any more — a
    /// manual Test Connection never persists either, so anything else would
    /// be inconsistent. Defaults to "always still wanted" so callers that
    /// don't offer a cancel affordance are unaffected.
    func addAccountWithPreflight(
        accountID: String, connectionConfig: AccountConfig, fullConfigJSON: String,
        priorTest: (report: ConnectionTestReport, tested: AccountConfig)? = nil,
        isStillWanted: (@MainActor () -> Bool)? = nil,
        onProbeComplete: (@MainActor (ConnectionTestReport) -> Void)? = nil,
        onProbeRequestFailed: (@MainActor (Error) -> Void)? = nil,
        onProbePassed: (@MainActor () -> Void)? = nil,
        completion: (@MainActor (AccountSyncStatus) -> Void)? = nil
    ) {
        let stillWanted = isStillWanted ?? { true }
        if let priorTest,
           priorTest.tested.connectionSettings == connectionConfig.connectionSettings,
           ConnectionTestPresentation.allPassed(priorTest.report) {
            guard stillWanted() else {
                Log.accounts.info("account: preflight skip-path cancelled before persisting for \(accountID, privacy: .private)")
                return
            }
            Log.accounts.info("account: preflight skipped (identical prior pass) for \(accountID, privacy: .private)")
            onProbePassed?()
            addAccount(accountID: accountID, configJSON: fullConfigJSON, completion: completion)
            return
        }
        guard let probeJSON = try? connectionConfig.connectionTestJSON() else {
            onProbeRequestFailed?(CastellumError.internalError)
            return
        }
        Log.accounts.info("account: preflight probing before first sync for \(accountID, privacy: .private)")
        Task { @MainActor in
            let result: Result<ConnectionTestReport, Error>
            if let override = self.connectionProbeOverride {
                result = await override(probeJSON)
            } else {
                result = await self.testConnection(configJSON: probeJSON)
            }
            switch result {
            case .success(let report):
                onProbeComplete?(report)
                guard ConnectionTestPresentation.allPassed(report) else {
                    Log.accounts.info("account: preflight failed for \(accountID, privacy: .private) — sync not started")
                    return
                }
                guard stillWanted() else {
                    Log.accounts.info("account: preflight passed but was cancelled before persisting for \(accountID, privacy: .private)")
                    return
                }
                onProbePassed?()
                self.addAccount(accountID: accountID, configJSON: fullConfigJSON, completion: completion)
            case .failure(let error):
                Log.accounts.error("account: preflight request failed for \(accountID, privacy: .private): \(error, privacy: .private)")
                onProbeRequestFailed?(error)
            }
        }
    }

    /// Persist an account's config without a successful sync — the sheet's
    /// "Save anyway" action: the user can fix the password
    /// later (Re-authenticate) or wait for the periodic re-sync to retry.
    /// Deliberately does NOT touch `accountSyncStatuses`, so the account
    /// stays showing whatever the last sync attempt reported (failed).
    ///
    /// Must create the account's row in the Rust-owned durable store (via
    /// `saveAccountWithoutSync(configJSON:)`), not just the Keychain entry:
    /// `reloadAccounts()` below reads accounts from `core.listAccounts()`,
    /// which is populated only by a successful sync — a connect/login
    /// failure never reaches that code path. Without this call the Keychain
    /// held valid credentials but the account never appeared anywhere in the
    /// UI, which is the bug this fixes. Order matters: the core row is
    /// created first so a Keychain-save failure never leaves an orphaned
    /// core-only account the UI can't attach credentials to.
    ///
    /// Returns true only when the account was fully persisted (core row AND
    /// Keychain). Callers that close a sheet must dismiss on true only, so a
    /// failure never discards what the user typed.
    @discardableResult
    func saveAccountWithoutSync(accountID: String, configJSON: String) -> Bool {
        do {
            try core.saveAccountWithoutSync(configJSON: configJSON)
        } catch {
            Log.accounts.error("account: save-without-sync failed for \(accountID, privacy: .private): \(error, privacy: .private)")
            errorText = "Could not save the account: \(error)"
            return false
        }
        do {
            try accountStore.save(accountID: accountID, configJSON: configJSON)
        } catch {
            Log.accounts.error("account: credential save failed for \(accountID, privacy: .private): \(error, privacy: .private)")
            errorText = "Account was saved but credentials could not be stored: \(error)"
            return false
        }
        Log.accounts.info("account: saved without sync for \(accountID, privacy: .private)")
        reloadAccounts()
        // A re-authenticate changes the stored password: the live IDLE connection
        // is still authenticated with the old one and would otherwise keep running
        // (or keep failing) until relaunch. core.startIdle stops the old thread
        // first, so this is a replace, not a second connection (#3439).
        startIdleIfPossible(accountId: accountID, configJSON: configJSON)
        return true
    }

    /// Takes a just-removed account's rows off the screen (FP04). Covers the
    /// account's own mailbox AND the cross-account lists (All Inboxes, smart
    /// mailboxes, Snoozed, search results), which used to keep showing its mail.
    func dropRemovedAccountFromList(_ accountId: String) {
        if selectedMailbox?.accountId == accountId {
            selectedMailbox = nil
            messages = []
            clearSelection()
        }
        if selectedMessage?.accountId == accountId { clearSelection() }
        messages.removeAll { $0.accountId == accountId }
        invalidateInFlightPage()
        if !searchText.isEmpty {
            runSearch()
        } else if isUnifiedList || selectedSmartMailboxId != nil || selectedIsSnoozed
            || selectedTagKey != nil || correspondence != nil {
            reloadCurrentMessages()
        }
    }

    /// The banner to show once `syncBanner`'s account recovers or is removed:
    /// another still-failing account, in sidebar order, or nil (FP04).
    func fallbackSyncBanner() -> AccountSyncStatus? {
        Self.fallbackSyncBanner(accounts: accounts, statuses: accountSyncStatuses)
    }

    /// Pure core of `fallbackSyncBanner()` (unit-tested).
    nonisolated static func fallbackSyncBanner(
        accounts: [Account], statuses: [String: AccountSyncStatus]
    ) -> AccountSyncStatus? {
        for a in accounts {
            if let st = statuses[a.id], st.state.isFailure { return st }
        }
        return nil
    }

    /// Permanently remove an account: deletes its local mail record through
    /// the core (mailboxes, messages, drafts, queued operations — see
    /// `castellum_remove_account`) AND its Keychain-stored credentials. This
    /// is the Accounts tab's Remove action; callers must confirm with the
    /// user — naming what gets deleted — before calling this. Irreversible.
    func removeAccount(accountId: String) {
        // Synchronously, before anything async: from here on no sync, periodic tick, IDLE start
        // or credential save may touch this account (#3439).
        removedAccountIds.insert(accountId)
        for dropped in removalUndoStack.dropEntries(forAccount: accountId) {
            removalUndoManager?.removeAllActions(withTarget: dropped.token)
        }
        queuedResync[accountId] = nil
        periodicLastAttempt[accountId] = nil
        finishSyncProgressPolling(accountId: accountId)
        let core = self.core
        let store = accountStore
        let indexer = spotlightIndexer
        Task.detached {
            try? core.stopIdle(accountId: accountId)
            do {
                try core.removeAccount(accountId: accountId)
            } catch {
                await MainActor.run {
                    self.removedAccountIds.remove(accountId)   // the removal did not happen
                    self.errorText = "Could not remove the account: \(error)"
                }
                return
            }
            // S58: domain-scoped Spotlight removal — unconditional (not
            // gated on the opt-in flag) because it's a harmless no-op
            // against a never-populated domain, and it means indexed items
            // can never outlive the account even across an opt-in that was
            // toggled off and back on between syncs.
            await indexer.removeAll(forAccount: accountId)
            // The core row is already gone at this point; a Keychain failure
            // here is the lesser problem, but still worth surfacing rather
            // than swallowing silently.
            var keychainError: String?
            do {
                try store.delete(accountID: accountId)
            } catch {
                keychainError = String(describing: error)
            }
            await MainActor.run {
                // Done unless a sync pass is still running (it sweeps when it ends) or the
                // Keychain entry could not be deleted (retried at launch).
                if keychainError == nil, self.inFlightSyncs[accountId] == nil {
                    self.removedAccountIds.remove(accountId)
                }
                self.dropRemovedAccountFromList(accountId)
                self.mailboxes[accountId] = nil
                self.drafts[accountId] = nil
                self.accountWriteEnabled[accountId] = nil
                self.forgetRemovedAccountPreferences(accountId)
                self.accountSyncStatuses[accountId] = nil
                self.syncGenerations[accountId] = nil
                self.queuedResync[accountId] = nil
                self.lastSavedGeneration[accountId] = nil
                if self.syncBanner?.accountId == accountId {
                    self.syncBanner = self.fallbackSyncBanner()
                }
                if self.settingsSelectedAccountId == accountId {
                    self.settingsSelectedAccountId = nil
                }
                self.reloadAccounts()
                self.refreshPendingCount()
                if let keychainError {
                    self.errorText =
                        "Account removed, but its Keychain entry could not be deleted: \(keychainError)"
                }
            }
        }
    }

    /// Per-account state that outlives the core row: identity aliases (which feed
    /// `myAddresses`), signatures, the legacy signature dictionary and the Markdown default.
    /// Removes the one account's entries only (#3439).
    func forgetRemovedAccountPreferences(_ accountId: String, prefs: Preferences = .shared) {
        identities[accountId] = nil
        prefs.forgetAccount(accountId)
    }

    // MARK: - account reset

    /// Read what an unforced reset would refuse on, for the confirmation
    /// dialog's wording. Best-effort: a failed read just means the caller
    /// shows the generic confirmation without specific counts.
    func checkResetAccountBlockers(accountId: String, completion: @escaping @MainActor (ResetAccountBlockers?) -> Void) {
        let core = self.core
        Task.detached {
            let blockers = try? core.resetAccountBlockers(accountId: accountId)
            await MainActor.run { completion(blockers) }
        }
    }

    /// Wipe `accountId`'s local mail and trigger a fresh sync, keeping its
    /// stored credentials, write policy, signature untouched.
    /// Callers must confirm with the user first — naming what is kept and
    /// what is dropped — exactly like `removeAccount`. On an unforced refusal
    /// (an unsent outbox draft or a queued operation), sets
    /// `resetAccountBlocked` with the exact counts instead of a raw error, so
    /// the UI can offer a second, specific confirmation and retry with
    /// `force: true`.
    func resetAccount(accountId: String, force: Bool = false) {
        guard let cfg = storedConfig(for: accountId) else {
            errorText = "Stored credentials are unavailable for this account."
            return
        }
        let core = self.core
        Task.detached {
            do {
                try core.resetAccount(accountId: accountId, force: force)
            } catch {
                await MainActor.run {
                    if AccountResetPolicy.isResetBlockedRefusal(error),
                       let email = self.accounts.first(where: { $0.id == accountId })?.email {
                        self.checkResetAccountBlockers(accountId: accountId) { blockers in
                            self.resetAccountBlocked = ResetAccountBlockedPrompt(
                                accountId: accountId, email: email,
                                blockers: blockers ?? ResetAccountBlockers(unsentOutboxDrafts: 0, pendingOperations: 0))
                        }
                    } else {
                        self.errorText = "Could not reset the account: \(error)"
                    }
                }
                return
            }
            await MainActor.run {
                // Also clears the account's rows from smart-mailbox, Snoozed, search and
                // Correspondence lists, not just the unified list (#3459).
                self.dropRemovedAccountFromList(accountId)
                self.drafts[accountId] = []
                self.reloadMailboxes(accountId)
                self.reloadCounts(accountId)
                self.refreshPendingCount()
                self.resetAccountBlocked = nil
                // Fresh sync from the server, over the SAME stored config —
                // progress shows through the existing sync status dashboard
                // (`sync(config:)` -> `performSync` sets `.syncing`, exactly
                // like `retrySync`).
                self.sync(config: cfg)
            }
        }
    }

    // MARK: - folder-selection edit for an existing account

    /// Outcome of `updateFolderSelection`, precise enough for the sheet to
    /// react correctly ( follow-up review) rather than a bare
    /// `Bool`: a refusal because pending operations still touch a folder
    /// being removed carries their per-folder counts, so the sheet can offer
    /// "sync now and retry" / "discard and remove" instead of a raw error.
    enum FolderSelectionUpdateResult {
        case success
        case blocked([MailboxSelectionBlocker])
        case failure(String)
    }

    /// Settings > Accounts > "Folders…": persist an edited config
    /// (`mailboxes`/`initialSyncSince` only — see `SyncScope.applyFolderEdit`/
    /// `applyHistoryOnlyEdit`) then purge every mailbox no longer selected
    /// (`keepPaths`, resolved concrete paths — see
    /// `SyncScope.resolvedKeepPaths`). Never runs a sync itself — deliberately
    /// the "Save anyway" path, not `addAccount`'s.
    ///
    /// Ordering fix ( follow-up review): the Keychain config is
    /// now written FIRST, synchronously, before anything else. Every sync or
    /// replay of this account reads `mailboxes`/`initialSyncSince` from the
    /// Keychain-stored config (`storedConfig(for:)`), never from the core's
    /// `accounts` row (`saveAccountWithoutSync` below only ever upserts
    /// id/email/provider) — so writing the Keychain LAST, as this used to,
    /// left a window where an in-flight sync could read the OLD filter and
    /// re-import the very folder this call is about to remove. If the
    /// Keychain write fails, nothing else runs (no core change, no purge) —
    /// a clean state to retry from. Callers should also disable Save while
    /// `accountSyncStatuses[accountId]?.state == .syncing` (the sheet does);
    /// this is checked here too as a defensive backstop against a
    /// last-instant race between that UI state and the tap.
    ///
    /// `force` threads through to `applyMailboxSelection`: unforced, a
    /// non-terminal operation still touching a folder about to be removed
    /// refuses the WHOLE call (`.blocked`, with per-folder counts fetched via
    /// `applyMailboxSelectionBlockers`) rather than silently cancelling it —
    /// removing a folder leaves server UIDs valid, unlike a UIDVALIDITY
    /// purge, so a queued move/flag/etc. is still live user intent. Retry
    /// with `force: true` only after explicit user confirmation.
    func updateFolderSelection(
        accountId: String, configJSON: String, keepPaths: [String], force: Bool = false,
        completion: (@MainActor (FolderSelectionUpdateResult) -> Void)? = nil
    ) {
        if accountSyncStatuses[accountId]?.state == .syncing {
            let message = "This account is syncing — try again once it finishes."
            completion?(.failure(message))
            return
        }
        let core = self.core
        Task.detached {
            // Blocked-removal PRE-check (PR #174 review): unforced, nothing may
            // be persisted if a pending operation still touches a folder about
            // to be removed — otherwise Cancel / "Sync Now and Try Again"
            // leaves the new filter saved with nothing purged. Fails open
            // (`try?`): the authoritative check is still `applyMailboxSelection`
            // below, which restores the prior config if it refuses.
            if !force,
               let blockers = try? core.applyMailboxSelectionBlockers(accountId: accountId, keepPaths: keepPaths),
               !blockers.isEmpty {
                await MainActor.run { completion?(.blocked(blockers)) }
                return
            }
            // Keychain write happens BEFORE the purge (see the ordering note
            // above). Remember the prior config so a late refusal can undo it.
            let priorConfig: String?? = await MainActor.run { () -> String?? in
                let prior = self.storedConfig(for: accountId)
                do {
                    try self.accountStore.save(accountID: accountId, configJSON: configJSON)
                } catch {
                    Log.accounts.error("account: credential save failed for \(accountId, privacy: .private): \(error, privacy: .private)")
                    completion?(.failure("Could not save the account settings: \(error)"))
                    return nil
                }
                return .some(prior)
            }
            guard let priorConfig else { return }
            // IDLE follows the synced folders (S31b review, finding e): it
            // watches INBOX only, and INBOX can be unticked here. Restarting
            // it with the NEW config retires it for an account whose folders
            // now exclude INBOX (the core starts no thread then) — and does it
            // BEFORE the purge below, so a wake cannot re-import the INBOX the
            // purge just removed. Off the main actor on purpose: `startIdle`
            // joins the old thread. Undone below if the purge is refused.
            await self.restartIdleOffActor(accountId: accountId, configJSON: configJSON)
            do {
                try core.saveAccountWithoutSync(configJSON: configJSON)
            } catch {
                await MainActor.run {
                    completion?(.failure("Folders were saved, but the account row could not be updated: \(error)"))
                }
                return
            }
            let removedCount: Int
            do {
                removedCount = try core.applyMailboxSelection(accountId: accountId, keepPaths: keepPaths, force: force)
            } catch {
                if MailboxSelectionPolicy.isRemovalBlockedRefusal(error) {
                    let blockers = (try? core.applyMailboxSelectionBlockers(accountId: accountId, keepPaths: keepPaths)) ?? []
                    await MainActor.run {
                        // An op was queued after the pre-check: undo the
                        // Keychain write so nothing is half-applied.
                        if let prior = priorConfig {
                            try? self.accountStore.save(accountID: accountId, configJSON: prior)
                            // ...and IDLE back on the config that is stored again.
                            self.startIdleIfPossible(accountId: accountId, configJSON: prior)
                        } else {
                            try? self.accountStore.delete(accountID: accountId)
                        }
                        completion?(.blocked(blockers))
                    }
                } else {
                    await MainActor.run {
                        completion?(.failure("Could not update the folder selection: \(error)"))
                    }
                }
                return
            }
            await MainActor.run {
                self.reloadAccounts()
                if removedCount > 0 {
                    // Checked BEFORE reloadMailboxes below: that reload is
                    // itself async (`load` -> `Task.detached`), so
                    // `self.mailboxes[accountId]` would still read the
                    // PRE-edit list at this point — comparing against
                    // `keepPaths` directly (what we just told the core to
                    // keep) avoids that race entirely.
                    if let selected = self.selectedMailbox, selected.accountId == accountId,
                       !keepPaths.contains(where: { $0.caseInsensitiveCompare(selected.path) == .orderedSame }) {
                        self.selectedMailbox = nil
                        self.messages = []
                        self.clearSelection()
                    }
                    // Mirror resetAccount: a unified list may still show
                    // messages from purged folders, and a selected message
                    // whose mailbox is no longer kept must not linger.
                    // (`self.mailboxes` still holds the PRE-edit list here.)
                    if let msg = self.selectedMessage, msg.accountId == accountId,
                       let box = self.mailboxes[accountId]?.first(where: { $0.id == msg.mailboxId }),
                       !keepPaths.contains(where: { $0.caseInsensitiveCompare(box.path) == .orderedSame }) {
                        self.clearSelection()
                    }
                    if self.isUnifiedList { self.reloadCurrentMessages() }
                    self.reloadMailboxes(accountId)
                    self.reloadCounts(accountId)
                    self.refreshPendingCount()
                }
                Log.accounts.info("account: folder selection updated for \(accountId, privacy: .private), removed \(removedCount)")
                completion?(.success)
            }
            // Belt-and-braces ( follow-up review): repair the
            // narrow race a sync of THIS account starting between the
            // syncing-check above and the purge just above landing.
            // `await` hops onto the main actor to call this MainActor-
            // isolated method from this detached, non-isolated task.
            await self.reconcileMailboxSelectionAfterInFlightSync(accountId: accountId, fallbackKeepPaths: keepPaths)
        }
    }

    /// If a sync of `accountId` was (or became) in-flight during
    /// `updateFolderSelection`, wait for it to finish, then re-apply the
    /// selection once more — a no-op (`apply_mailbox_selection` is
    /// idempotent) unless that sync re-imported a folder the user just
    /// removed. Reads the CURRENT stored config's `mailboxes` (not the
    /// closure-captured `keepPaths`) when it's an explicit list, so a second
    /// edit made while this was waiting is never clobbered; falls back to
    /// `fallbackKeepPaths` only while the stored config still says "defaults"
    /// (resolving that live would need a fresh server probe, which this
    /// background repair deliberately does not do).
    ///
    /// UNFORCED ( second follow-up review — the original
    /// `force: true` here was itself a bug): this repair runs with no user
    /// in front of it, so it must never cancel anything on its own say-so.
    /// The exact scenario that forced this fix: a role folder like Trash
    /// stays in `to_remove` on every call (unticking it never removes its
    /// row — only ADDING it back to `keepPaths` would), so if the user
    /// deletes messages (queues moves INTO Trash) while THIS is waiting for
    /// a racing sync, a forced re-apply would have silently cancelled those
    /// deletes under the unrelated label "the user removed Trash". Calling
    /// unforced instead means: if anything queued during the wait still
    /// blocks, this leaves that folder's row/mail exactly as it was rather
    /// than force-discarding an action the user never got to confirm
    /// losing. The user's own next Save retries normally, with the usual
    /// blocked-changes prompt — this background repair only ever tidies up
    /// the cases where nothing is blocking.
    ///
    /// Not `private`: a test seam, so `FolderSelectionAppModelTests` can
    /// call it directly rather than racing the real 500ms poll loop against
    /// a live sync's timing.
    func reconcileMailboxSelectionAfterInFlightSync(accountId: String, fallbackKeepPaths: [String]) {
        let core = self.core
        Task.detached { [weak self] in
            guard let self else { return }
            var waited = false
            while await MainActor.run(body: { self.accountSyncStatuses[accountId]?.state == .syncing }) {
                waited = true
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
            guard waited else { return }
            let keepPaths: [String] = await MainActor.run {
                if let stored = self.storedConfig(for: accountId),
                   let config = try? AccountConfig.decode(fromJSON: stored),
                   let explicit = config.mailboxes {
                    return explicit
                }
                return fallbackKeepPaths
            }
            do {
                _ = try core.applyMailboxSelection(accountId: accountId, keepPaths: keepPaths, force: false)
            } catch {
                // A blocked refusal is the expected/fine outcome when the
                // racing sync (or an S40 rule firing at its end) queued a
                // new op the user hasn't seen yet — never force this
                // unattended repair; see the doc comment above.
                Log.accounts.debug("account: post-sync folder reconciliation skipped for \(accountId, privacy: .private): \(error, privacy: .private)")
                return
            }
            await MainActor.run {
                self.reloadMailboxes(accountId)
                self.reloadCounts(accountId)
            }
        }
    }

    /// Opens (or fronts) the Settings window on the Accounts tab with
    /// `accountId` highlighted. Used by the "Open Accounts…" affordance on
    /// the missing-credentials send alert. The deployment target (macOS 13)
    /// predates the `openSettings` environment action (macOS 14+), so this
    /// uses the classic AppKit selector every SwiftUI `Settings` scene responds to.
    func openAccountsSettings(accountId: String?) {
        settingsSelectedAccountId = accountId
        openSettingsTab(.accounts)
    }

    /// Opens (or fronts) the Settings window on `tab`. Used directly by the
    /// Status dashboard's AI row; `openAccountsSettings`
    /// layers the accounts-tab highlight on top of this.
    func openSettingsTab(_ tab: SettingsTab) {
        settingsTab = tab
        // One run-loop turn later: the caller is an alert button whose panel
        // is still key while its action runs, and an action sent down the
        // responder chain at that moment is swallowed — the "Open Accounts…
        // does nothing" Marc saw on 0.3.2 ( follow-up).
        DispatchQueue.main.async { [self] in
            if let settingsOpener {
                settingsOpener()
            } else {
                NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
            }
        }
    }

    /// The SwiftUI `openSettings` environment action, installed by
    /// `SettingsOpenerInstaller` on macOS 14+ where it is the supported way to
    /// show the Settings scene. Nil on macOS 13, which keeps the AppKit selector.
    var settingsOpener: (() -> Void)?

    func reloadAccounts(then afterApply: (@MainActor () -> Void)? = nil) {
        load({ try $0.listAccounts() }, apply: { accts in
            self.accounts = accts
            // S59: the Focus filter UI lists accounts from this snapshot.
            if !TestHost.isActive {
                FocusAccountsSnapshot.save(accts.map { ($0.id, $0.email) })
            }
            for a in accts {
                self.reloadMailboxes(a.id)
                self.reloadCounts(a.id)
                self.reloadHydrationProgress(a.id)
                self.reloadDrafts(a.id)
                self.reloadIdentities(a.id) // "me" for Correspondence counts
            }
            // Accounts are now populated → safe to drain any queued ops, but
            // only AFTER the S25 crash-safety reconcile below has run for
            // each of them.
            self.refreshWriteEnabled()
            self.refreshPendingCount()
            self.reconcileUndoSendsThenReplay(accountIds: accts.map(\.id))
            self.seedAccountSyncStatuses()
            afterApply?()
        }, always: {
            // Fires on a `listAccounts()` FFI/DB failure too (`errorText` is
            // already set by `load` above in that case) — a queued mailto:/
            // Share-Extension URL must never be stranded forever just
            // because this particular pass failed to read accounts; the
            // flush it unblocks falls through to the existing no-account/
            // first-sync alert instead (S57, PR #182 review).
            self.markAccountsLoaded()
        })
    }

    /// Runs `action` once `accounts` reflects the first completed
    /// `reloadAccounts()` pass — immediately, if that has already happened.
    /// See `accountsLoaded`'s doc comment for why `AppDelegate` needs this
    /// rather than checking `accounts.isEmpty` (which is also true for a
    /// genuinely account-less profile).
    func onAccountsLoaded(_ action: @escaping () -> Void) {
        if accountsLoaded {
            action()
        } else {
            accountsLoadedCallbacks.append(action)
        }
    }

    /// Flips `accountsLoaded` and drains any callbacks queued via
    /// `onAccountsLoaded` — exactly once, on the first `reloadAccounts()`
    /// pass to complete; a no-op on every later reload.
    private func markAccountsLoaded() {
        guard !accountsLoaded else { return }
        accountsLoaded = true
        let callbacks = accountsLoadedCallbacks
        accountsLoadedCallbacks.removeAll()
        callbacks.forEach { $0() }
    }

    /// Seed sidebar status from Rust-owned durable diagnostics. Live failures
    /// already held in memory win so their raw local-only detail remains
    /// available to Diagnostics for this process lifetime.
    private func seedAccountSyncStatuses() {
        let core = self.core
        let store = accountStore
        Task.detached {
            guard let diagnostics = try? core.localDiagnostics() else { return }
            var seeded: [String: AccountSyncStatus] = [:]
            var seededChecked: [String: Date] = [:]
            for account in diagnostics.accounts {
                guard let json = try? store.configJSON(for: account.accountId),
                      let target = AccountSyncTarget(configJSON: json)
                else { continue }
                if account.consecutiveFailures > 0 {
                    seeded[account.accountId] = .failure(
                        target: target,
                        errorKind: account.lastErrorKind,
                        rawError: nil
                    )
                } else if account.lastSuccessAt != nil {
                    seeded[account.accountId] = .ok(target)
                }
                if let when = SyncStatusLineBuilder.parseCoreTimestamp(account.lastSuccessAt) {
                    seededChecked[account.accountId] = when
                }
            }
            let statuses = seeded
            let checked = seededChecked
            await MainActor.run {
                for (accountId, status) in statuses where self.accountSyncStatuses[accountId] == nil {
                    self.accountSyncStatuses[accountId] = status
                }
                for (accountId, when) in checked where self.accountLastChecked[accountId] == nil {
                    self.accountLastChecked[accountId] = when
                }
            }
        }
    }

    func dismissSyncBanner() {
        syncBanner = nil
    }

    func reloadMailboxes(_ accountId: String) {
        load({ try $0.listMailboxes(accountId: accountId) }) { self.mailboxes[accountId] = $0 }
    }

    /// Refresh the per-mailbox "bodies downloaded" counts (#3631 G5). Not part
    /// of `reloadCounts`: that runs on every read/unread change, while hydration
    /// only moves after a sync or a hydration pass.
    func reloadHydrationProgress(_ accountId: String) {
        load({ try $0.bodyHydrationProgress(accountId: accountId) }) { list in
            for p in list { self.hydrationProgress[p.mailboxId] = p }
        }
    }

    func reloadCounts(_ accountId: String) {
        load({ try $0.mailboxCounts(accountId: accountId) }) { list in
            for c in list { self.counts[c.mailboxId] = c }
        }
        refreshInboxSplitCounts() // HP-2: read/unread changes move the tab badges too
    }

    // MARK: - rules (MM-3)

    /// Rules only (smart mailboxes share the table), in run order.
    @Published var rules: [SavedRule] = []
    @Published var ruleSaveError: String?

    func reloadRules() {
        load({ try $0.listFilters(accountId: nil).filter { $0.kind == "rule" } }) { self.rules = $0 }
    }

    /// Create or update a rule from the editor's draft. Sets `ruleSaveError`
    /// (shown in the sheet) instead of the generic error banner on failure.
    func saveRule(_ draft: RuleDraft, completion: @escaping @MainActor (Bool) -> Void = { _ in }) {
        ruleSaveError = nil
        guard draft.isValid,
              let data = try? CastellumCore.encoder.encode(RuleUpsertRequest(draft)),
              let json = String(data: data, encoding: .utf8) else {
            completion(false)
            return
        }
        let core = self.core
        Task.detached {
            do {
                _ = try core.upsertFilter(json: json)
                await MainActor.run {
                    self.reloadRules()
                    completion(true)
                }
            } catch {
                await MainActor.run {
                    self.ruleSaveError = "Could not save this rule: \(error)"
                    completion(false)
                }
            }
        }
    }

    func deleteRule(_ id: String) {
        let core = self.core
        Task.detached {
            do {
                try core.deleteFilter(id: id)
                await MainActor.run { self.reloadRules() }
            } catch {
                await MainActor.run { self.errorText = "Could not delete this rule: \(error)" }
            }
        }
    }

    /// Enable/disable without opening the editor. Saves the stored rule with
    /// only `enabled` changed; a failure is shown (and the list reloaded so the
    /// switch snaps back to the truth), never swallowed.
    func setRule(_ rule: SavedRule, enabled: Bool) {
        guard let data = try? CastellumCore.encoder.encode(RuleUpsertRequest(rule: rule, enabled: enabled)),
              let json = String(data: data, encoding: .utf8) else { return }
        let core = self.core
        Task.detached {
            do {
                _ = try core.upsertFilter(json: json)
                await MainActor.run { self.reloadRules() }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not \(enabled ? "enable" : "disable") this rule: \(error)"
                    self.reloadRules()
                }
            }
        }
    }

    /// Drag-reorder: optimistic, reloads from the core on failure.
    func moveRules(fromOffsets: IndexSet, toOffset: Int) {
        rules.move(fromOffsets: fromOffsets, toOffset: toOffset)
        let ids = rules.map(\.id)
        guard let data = try? JSONEncoder().encode(ids), let json = String(data: data, encoding: .utf8) else { return }
        let core = self.core
        Task.detached {
            do {
                try core.reorderFilters(orderedIdsJSON: json)
            } catch {
                await MainActor.run {
                    self.errorText = "Could not reorder rules: \(error)"
                    self.reloadRules()
                }
            }
        }
    }

    /// Dry run of the editor's unsaved conditions against current mail, with
    /// the engine's own candidate rules (Trash, Junk, Drafts and Sent left
    /// out): the first 200 matches and the exact total. Nothing is enqueued.
    func previewRuleConditions(
        _ group: FilterGroupDTO, accountId: String?,
        completion: @escaping @MainActor (Result<RuleConditionsPreviewDTO, Error>) -> Void
    ) {
        guard let data = try? CastellumCore.encoder.encode(group.pruningEmptyGroups()),
              let json = String(data: data, encoding: .utf8) else { return }
        let core = self.core
        Task.detached {
            let result = Result { try core.previewRuleConditions(groupJSON: json, accountId: accountId, limit: 200) }
            await MainActor.run { completion(result) }
        }
    }

    /// Exactly how many current messages the rule would act on (a dry run,
    /// counted in the core with Trash, Junk, Drafts and Sent left out), for
    /// the Run Now confirmation.
    func previewRuleMatchCount(_ rule: SavedRule, completion: @escaping @MainActor (Result<Int, Error>) -> Void) {
        let core = self.core
        Task.detached {
            let result = Result { try core.previewRule(id: rule.id, accountId: rule.accountId, limit: 1).total }
            await MainActor.run { completion(result) }
        }
    }

    /// The Run Now in progress, if any (Settings > Rules shows it).
    @Published private(set) var ruleRunProgress: RuleRunProgress?
    private var ruleRunTask: Task<Void, Never>?
    /// Where the last Run Now stopped before the end of its match set (run
    /// cap reached or cancelled). Settings > Rules offers Continue, which
    /// resumes from this cursor; a fresh Run Now ignores and replaces it.
    @Published private(set) var ruleContinuation: RuleContinuation?

    /// "Run now": queue the rule's actions for what it currently matches, in
    /// bounded batches off the main actor (one short blocking core call per
    /// batch), at most `RuleRunNow.maxMessages` messages per run. Progress is
    /// published after each batch and `cancelRuleRun()` stops it between
    /// batches: what was already queued stays queued (and is reported).
    func runRuleNow(_ rule: SavedRule, continuing: Bool = false,
                    completion: @escaping @MainActor (String) -> Void) {
        guard ruleRunTask == nil else {
            completion(String(localized: "Another rule is still running."))
            return
        }
        // Continue resumes from the stored cursor of THIS rule; a fresh Run
        // Now (or Continue for another rule) starts from the first message.
        let resume = continuing && ruleContinuation?.ruleId == rule.id ? ruleContinuation?.after : nil
        ruleContinuation = nil
        let core = self.core
        ruleRunProgress = RuleRunProgress(ruleName: rule.name, target: RuleRunNow.maxMessages)
        ruleRunTask = Task.detached(priority: .userInitiated) {
            var after: String? = resume
            var remaining: Int?
            var processed = 0
            var queued = 0
            var firstError: String?
            var more = false
            var failure: Error?
            while processed < RuleRunNow.maxMessages, !Task.isCancelled {
                let limit = min(RuleRunNow.batchSize, Int64(RuleRunNow.maxMessages - processed))
                do {
                    let report = try core.runRuleNowBatch(id: rule.id, accountId: rule.accountId,
                                                          after: after, limit: limit)
                    processed += report.matchedCount
                    queued += report.actionsEnqueued
                    if firstError == nil { firstError = report.error }
                    after = report.nextAfter
                    remaining = report.remaining
                    more = report.nextAfter != nil
                    let (p, q) = (processed, queued)
                    await MainActor.run { self.ruleRunProgress?.processed = p; self.ruleRunProgress?.queued = q }
                    if !more { break }
                } catch {
                    failure = error
                    break
                }
            }
            let cancelled = Task.isCancelled
            let message: String
            if let failure {
                message = "Could not finish this rule: \(failure). \(processed) matched, \(queued) actions queued before it stopped."
            } else if cancelled {
                message = more
                    ? String(localized: "Stopped: \(processed) matched, \(queued) actions queued before you cancelled. Choose Continue to carry on.")
                    : String(localized: "Stopped: \(processed) matched, \(queued) actions queued before you cancelled.")
            } else if more {
                message = String(localized: "\(processed) matched, \(queued) actions queued. \(RuleRunNow.remainingText(remaining)) still match: choose Continue to carry on.")
            } else {
                message = String(localized: "\(processed) matched, \(queued) actions queued.")
            }
            let text = firstError.map { "\(message) Some messages could not be changed: \($0)" } ?? message
            let resumeFrom = failure == nil && more ? after : nil
            let queuedTotal = queued
            await MainActor.run {
                self.ruleRunTask = nil
                self.ruleRunProgress = nil
                self.ruleContinuation = resumeFrom.map {
                    RuleContinuation(ruleId: rule.id, after: $0, remaining: remaining)
                }
                self.refreshPendingCount()
                // FP19: queued actions must go to the server now, not at the
                // next sync (replayNow's isReplaying guard keeps this safe).
                if queuedTotal > 0 { self.scheduleReplay() }
                completion(text)
            }
        }
    }

    /// Stop the Run Now in progress after its current batch.
    func cancelRuleRun() {
        ruleRunTask?.cancel()
    }

    func reloadSmartMailboxes() {
        load({ try $0.listSmartMailboxes() }) { self.smartMailboxes = $0 }
    }

    /// Reload tag definitions (S42) — from `bootstrap()` and after each
    /// sync's `onSuccess` (server keywords may arrive with no local
    /// definition yet, which Settings' adopt list needs to reflect).
    func reloadTags() {
        load({ try $0.listTags() }) { self.tags = $0 }
        reloadTagCounts()
    }

    /// Reload the sidebar Tags badges (MM-8). Cheap (one grouped query); call
    /// after anything that can move a tag's total or unread count.
    func reloadTagCounts() {
        load({ try $0.tagCounts() }) { list in
            self.tagCounts = Dictionary(list.map { ($0.keywordKey, $0) }, uniquingKeysWith: { a, _ in a })
        }
    }

    /// Reload the "in use on your servers" adopt list — call when Settings
    /// → Tags appears; not needed anywhere else.
    func reloadKeywordsInUse() {
        load({ try $0.listKeywordsInUse() }) { self.keywordsInUse = $0 }
    }

    /// Recompute every smart mailbox's cached counts. Call after a sync
    /// pass and after a local read/unread mutation (`reloadCountsAfterMutation`)
    /// — never on a keystroke or on every selection change.
    func refreshSmartMailboxCounts() {
        smartMailboxCountRefreshes += 1
        load({ try $0.refreshSmartMailboxCounts() }) { self.smartMailboxes = $0 }
    }

    /// MM-1c: (re)load the header policy and coverage for the editor. Local-only.
    func refreshHeaderPolicy() {
        load({ try $0.filterHeaderPolicy() }) { self.headerPolicy = $0 }
        load({ try $0.messageHeaderCoverage() }) { self.headerCoverage = $0 }
    }

    /// How many times `refreshSmartMailboxCounts` was asked for (tests).
    private(set) var smartMailboxCountRefreshes = 0

    /// After a local flag, tag or mute change: the per-account counts do not
    /// move, but a smart mailbox matching on it does, so its sidebar badge
    /// would stay stale until the next sync (#3455).
    func refreshSmartMailboxCountsIfAny() {
        if !smartMailboxes.isEmpty { refreshSmartMailboxCounts() }
    }

    /// Encodable upsert payload; `id == nil` creates, `id != nil` renames/
    /// re-queries. `symbol` stays unset in this stage's UI (no picker yet)
    /// — present here only so the shape matches the core's JSON contract.
    struct SmartMailboxUpsertRequest: Encodable {
        var id: String?
        let name: String
        let query: String
        var symbol: String?
        /// MM-1: nested conditions (makes it a group mailbox) and scope.
        var group: FilterGroupDTO?
        var scope: SmartMailboxScopeDTO?
    }

    nonisolated static func encodeSmartMailboxUpsert(_ req: SmartMailboxUpsertRequest) -> String? {
        guard let data = try? CastellumCore.encoder.encode(req) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    /// Create (`id == nil`) or rename/re-query (`id != nil`) a smart
    /// mailbox. On a name collision (`smart_mailboxes.name` is `UNIQUE`)
    /// sets `smartMailboxSaveError` to a friendly message instead of the
    /// generic top-level `errorText` banner.
    func upsertSmartMailbox(
        id: String?, name: String, query: String,
        group: FilterGroupDTO? = nil, scope: SmartMailboxScopeDTO? = nil,
        completion: @escaping @MainActor (SmartMailbox?) -> Void = { _ in }
    ) {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let trimmedQuery = query.trimmingCharacters(in: .whitespacesAndNewlines)
        smartMailboxSaveError = nil
        // A group mailbox has no text query; a text mailbox needs one.
        let saveable = group.map { $0.isValidForSave(policy: headerPolicy) } ?? !trimmedQuery.isEmpty
        guard !trimmedName.isEmpty, saveable,
              let json = Self.encodeSmartMailboxUpsert(
                  SmartMailboxUpsertRequest(
                      id: id, name: trimmedName, query: group == nil ? trimmedQuery : "",
                      group: group?.pruningEmptyGroups(), scope: group == nil ? nil : scope))
        else {
            completion(nil)
            return
        }
        let core = self.core
        Task.detached {
            do {
                let saved = try core.upsertSmartMailbox(json: json)
                await MainActor.run {
                    self.reloadSmartMailboxes()
                    completion(saved)
                }
            } catch {
                await MainActor.run {
                    let description = String(describing: error)
                    self.smartMailboxSaveError = description.contains("UNIQUE constraint failed")
                        ? "A saved search named \u{201c}\(trimmedName)\u{201d} already exists."
                        : "Could not save this search: \(description)"
                    completion(nil)
                }
            }
        }
    }

    /// True when the selected plain mailbox is one the core does not sync by
    /// default and holds nothing locally, so an empty list is a hint, not a
    /// fact (FP15). Uses the default policy only: the stored filter lives in
    /// the Keychain and is not read from a view body.
    var selectedMailboxIsNotSynced: Bool {
        guard let mb = selectedMailbox else { return false }
        return !SyncScope.isSynced(mailbox: mb, accountFilter: nil) && (counts[mb.id]?.total ?? 0) == 0
    }

    func deleteSmartMailbox(_ id: String) {
        let core = self.core
        Task.detached {
            do {
                try core.deleteSmartMailbox(id: id)
                await MainActor.run {
                    Preferences.shared.forgetSmartMailbox(id)   // MM-9: a counter must not keep a dead id
                    if self.selectedSmartMailboxId == id {
                        // FP15: fall back to a real list instead of selecting nothing.
                        self.selectedSmartMailboxId = nil
                        self.smartMailboxNotice = nil
                        self.messages = []
                        if self.accounts.count > 1 {
                            self.select(unifiedRole: "inbox")
                        } else {
                            self.selectMailbox(role: "inbox")
                        }
                    }
                    self.reloadSmartMailboxes()
                }
            } catch {
                await MainActor.run { self.errorText = "Could not delete this smart mailbox: \(error)" }
            }
        }
    }

    /// Drag-reorder from the sidebar's `.onMove`. Applies the new order
    /// optimistically (no flash back to the old order while the FFI call is
    /// in flight); reverts by reloading from the core on failure.
    func moveSmartMailboxes(fromOffsets: IndexSet, toOffset: Int) {
        smartMailboxes.move(fromOffsets: fromOffsets, toOffset: toOffset)
        let ids = smartMailboxes.map(\.id)
        guard let data = try? JSONEncoder().encode(ids), let json = String(data: data, encoding: .utf8) else { return }
        let core = self.core
        Task.detached {
            do {
                try core.reorderSmartMailboxes(orderedIdsJSON: json)
                await MainActor.run { self.reloadSmartMailboxes() }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not save the new order: \(error)"
                    self.reloadSmartMailboxes() // reload the stored order
                }
            }
        }
    }

    func select(mailbox: Mailbox) {
        searchText = "" // FP14: a stale query must not outlive the list it filtered
        selectedMailbox = mailbox
        selectedUnifiedRole = nil
        selectedSmartMailboxId = nil
        smartMailboxNotice = nil
        selectedIsSnoozed = false
        clearTagSelection()
        // Switching mailbox IS navigating away from the open message, so it
        // goes through the one function that tears the selection down —
        // including the AI-summary cancel and `selectedMessageIdForAi`, both of
        // which this path used to skip, leaking a running job on every g-chord.
        clearSelection()
        invalidateInFlightPage()
        nextCursor = nil
        inboxSplitFailed = false
        if showInboxSplitIfDefined() { return } // HP-2: the Inbox opens on its split tab
        loadPlainFirstPage()
    }

    /// The plain (non-split) first page of the selected mailbox or unified role.
    /// Shared by the selection functions and the Inbox-split failure fallback.
    func loadPlainFirstPage() {
        let token = listToken
        let sort = effectiveSort
        if let mailbox = selectedMailbox {
            let (acct, mbox) = (mailbox.accountId, mailbox.id)
            PerfSignpost.begin(.mailboxSwitch, replacing: &mailboxSwitchSpan)
            load({ try $0.listMessagesPage(accountId: acct, mailboxId: mbox, sort: sort, after: nil, limit: AppModel.listPageSize) }) {
                // A reply that lost a race with Correspondence (or any later list
                // switch) must not overwrite the newer list.
                guard token == self.listToken else { return }
                self.messages = $0.rows
                self.nextCursor = $0.next
                PerfSignpost.end(&self.mailboxSwitchSpan, count: $0.rows.count)
            } always: {
                // The load threw: close the interval now, not at the next
                // switch. (Stale loads leave the newer span alone.)
                if token == self.listToken { PerfSignpost.end(&self.mailboxSwitchSpan, count: -1) }
            }
        } else if let role = selectedUnifiedRole {
            PerfSignpost.begin(.mailboxSwitch, replacing: &mailboxSwitchSpan)
            load({ try $0.listMessagesPage(byRole: role, sort: sort, after: nil, limit: AppModel.listPageSize) }) {
                guard token == self.listToken else { return }
                self.messages = $0.rows
                self.nextCursor = $0.next
                PerfSignpost.end(&self.mailboxSwitchSpan, count: $0.rows.count)
            } always: {
                if token == self.listToken { PerfSignpost.end(&self.mailboxSwitchSpan, count: -1) }
            }
        }
    }

    /// Select the unified (cross-account) group for `role` (S34) — today
    /// only the sidebar's "All Inboxes" row (`role == "inbox"`); the core
    /// query is generic over any mailbox role.
    func select(unifiedRole role: String) {
        searchText = "" // FP14 (selectVips sets "is:vip" after this call)
        selectedMailbox = nil
        selectedSmartMailboxId = nil
        smartMailboxNotice = nil
        selectedUnifiedRole = role
        selectedIsSnoozed = false
        clearTagSelection()
        clearSelection()
        invalidateInFlightPage()
        nextCursor = nil
        inboxSplitFailed = false
        if showInboxSplitIfDefined() { return } // HP-2: All Inboxes is split too
        loadPlainFirstPage()
    }

    /// Fetch the next keyset page when the table is scrolled within
    /// `loadMoreThreshold` rows of the end (S64 infinite scroll). A no-op
    /// for search, smart-mailbox and Snoozed lists (none of them page) —
    /// `nextCursor` is simply never set for those, so `hasMoreMessages` is
    /// already `false`.
    private static let loadMoreThreshold = 100

    /// Drops any in-flight `loadMoreMessagesIfNeeded` page (its captured
    /// token no longer matches) AND releases the `isLoadingMore` latch it
    /// held. The two must move together: the dropped page's completion
    /// returns on the token mismatch before it reaches `isLoadingMore =
    /// false`, so rotating the token alone left the latch set forever and
    /// infinite scroll dead for the list (S64 review round 3).
    func invalidateInFlightPage() {
        listToken = UUID()
        isLoadingMore = false
    }

    func loadMoreMessagesIfNeeded(lastVisibleRow: Int) {
        // Mirrors `reloadCurrentMessages()`'s own `searchText.isEmpty` guard:
        // search results (mailbox- or cross-account) never page, but
        // `selectedMailbox`/`selectedUnifiedRole` stay set while searching
        // within them, so a stale `nextCursor` from before the search would
        // otherwise still pass the guard below.
        if let key = selectedTagKey {
            loadMoreTagMessagesIfNeeded(keyword: key, lastVisibleRow: lastVisibleRow)
            return
        }
        guard searchText.isEmpty,
              hasMoreMessages, !isLoadingMore,
              lastVisibleRow >= messages.count - Self.loadMoreThreshold,
              let cursor = nextCursor
        else { return }
        let scope: PagingScope
        if let mailbox = selectedMailbox { scope = .mailbox(accountId: mailbox.accountId, mailboxId: mailbox.id) }
        else if let role = selectedUnifiedRole { scope = .unifiedRole(role) }
        else { return }
        isLoadingMore = true
        let token = listToken
        let core = self.core
        let sort = effectiveSort
        Task.detached {
            do {
                let page: MessagePage
                switch scope {
                case .mailbox(let accountId, let mailboxId):
                    page = try core.listMessagesPage(
                        accountId: accountId, mailboxId: mailboxId, sort: sort, after: cursor,
                        limit: AppModel.listPageSize)
                case .unifiedRole(let role):
                    page = try core.listMessagesPage(
                        byRole: role, sort: sort, after: cursor, limit: AppModel.listPageSize)
                }
                await MainActor.run {
                    guard self.listToken == token else { return } // selection changed mid-flight; drop it
                    self.isLoadingMore = false
                    guard !page.rows.isEmpty else { self.nextCursor = nil; return }
                    // The cursor holds the sort key the last row had WHEN IT WAS
                    // FETCHED. Under the Unread or Star sort, a loaded row whose
                    // flag changes since (mark read, star) now sorts past that
                    // cursor and comes back in this page: never list it twice.
                    // It stays where it is; the next reload re-sorts it.
                    let shown = Set(self.messages.map(\.id))
                    let fresh = page.rows.filter { !shown.contains($0.id) }
                    let old = self.messages.count
                    if !fresh.isEmpty {
                        self.lastMessagesChange = .appended(old..<(old + fresh.count))
                        self.pendingAppend = true
                        self.messages.append(contentsOf: fresh)
                    }
                    self.nextCursor = page.next
                }
            } catch {
                Log.ui.error("loadMoreMessagesIfNeeded failed: \(error, privacy: .private)")
                await MainActor.run {
                    guard self.listToken == token else { return }
                    self.isLoadingMore = false
                }
            }
        }
    }

    /// Keyset paging for the tag list (MM-8): same trigger, latch and stale
    /// guard as the mailbox path, with the last loaded row as the cursor.
    private func loadMoreTagMessagesIfNeeded(keyword: String, lastVisibleRow: Int) {
        guard searchText.isEmpty, correspondence == nil,
              tagListHasMore, !isLoadingMore,
              lastVisibleRow >= messages.count - Self.loadMoreThreshold
        else { return }
        isLoadingMore = true
        let token = listToken
        guard let last = messages.last else { return }
        let core = self.core
        Task.detached {
            do {
                let rows = try core.messagesForTag(
                    keyword: keyword, limit: AppModel.listPageSize, after: last)
                await MainActor.run {
                    guard self.listToken == token, self.selectedTagKey == keyword else { return }
                    self.isLoadingMore = false
                    guard !rows.isEmpty else { self.tagListHasMore = false; return }
                    // Belt and braces: a row already shown (a reload raced this page) is never listed twice.
                    let shown = Set(self.messages.map(\.id))
                    let fresh = rows.filter { !shown.contains($0.id) }
                    let old = self.messages.count
                    if !fresh.isEmpty {
                        self.lastMessagesChange = .appended(old..<(old + fresh.count))
                        self.pendingAppend = true
                        self.messages.append(contentsOf: fresh)
                    }
                    self.tagListHasMore = Int64(rows.count) >= AppModel.listPageSize
                }
            } catch {
                Log.ui.error("tag page failed: \(error, privacy: .private)")
                await MainActor.run {
                    guard self.listToken == token else { return }
                    self.isLoadingMore = false
                }
            }
        }
    }

    /// Fetches one smart mailbox's messages without disturbing the reading-
    /// pane selection — the shared body behind both `select(smartMailbox:)`
    /// (which does clear selection, like `select(mailbox:)`) and
    /// `reloadCurrentMessages()`'s background-refresh path (which must not,
    /// mirroring the existing unified/mailbox split in that function).
    /// Guards every reply against a since-changed selection (the user
    /// clicked something else while this was in flight) — the same
    /// discipline the sync-status seeding code already uses.
    private func loadSmartMailboxMessages(_ sm: SmartMailbox) {
        let core = self.core
        let id = sm.id
        let name = sm.name
        let token = listToken // any later select/search/reload rotates it (#3441)
        Task.detached {
            do {
                let rows = try core.messagesForSmartMailbox(id: id, limit: 500)
                await MainActor.run {
                    guard self.selectedSmartMailboxId == id, token == self.listToken else { return }
                    if let rows {
                        self.messages = self.clientSorted(rows)
                        self.smartMailboxNotice = nil
                    } else {
                        self.messages = []
                        self.smartMailboxNotice = "\u{201c}\(name)\u{201d} no longer exists \u{2014} it may have been deleted."
                    }
                }
            } catch {
                await MainActor.run {
                    guard self.selectedSmartMailboxId == id, token == self.listToken else { return }
                    self.messages = []
                    self.smartMailboxNotice = "\u{201c}\(name)\u{201d} couldn\u{2019}t run its saved search. Edit it and try again."
                }
            }
        }
    }

    /// Select the "Snoozed" sidebar group (S43).
    func select(snoozed: Void = ()) {
        searchText = "" // FP14
        selectedMailbox = nil
        selectedUnifiedRole = nil
        selectedSmartMailboxId = nil
        smartMailboxNotice = nil
        selectedIsSnoozed = true
        clearTagSelection()
        clearSelection()
        invalidateInFlightPage()
        nextCursor = nil // Snoozed never pages (S64)
        reloadSnoozedMessages()
    }

    /// Reload the Snoozed list from the core — used by `select(snoozed:)`
    /// and after any mutation that could change it (snooze/unsnooze, and the
    /// due-check sweep). Also refreshes `messages` (soonest-due first) when
    /// the Snoozed row is the active selection, so `MessageListView` — which
    /// only ever renders `messages` — actually shows the snoozed rows rather
    /// than whatever mailbox was on screen before.
    func reloadSnoozedMessages() {
        load({ try $0.listSnoozedMessages(limit: 500, offset: 0) }) { list in
            self.snoozedMessages = list
            // Only the plain Snoozed view shows these rows: a search typed inside
            // Snoozed owns `messages` (#3441).
            if self.selectedIsSnoozed && self.searchText.isEmpty {
                self.messages = self.clientSorted(list.map(\.header))
            }
        }
    }

    /// Select a smart mailbox (S41): runs its saved query cross-account
    /// through the S39 path, mutually exclusive with a concrete mailbox or
    /// the unified "All Inboxes" group.
    func select(smartMailbox: SmartMailbox) {
        searchText = "" // FP14
        selectedMailbox = nil
        selectedUnifiedRole = nil
        selectedIsSnoozed = false
        selectedSmartMailboxId = smartMailbox.id
        smartMailboxNotice = nil
        clearTagSelection()
        clearSelection()
        invalidateInFlightPage()
        nextCursor = nil // smart mailboxes never page (S64)
        loadSmartMailboxMessages(smartMailbox)
    }

    /// Drop the tag selection and its paging state (every other `select(...)`).
    private func clearTagSelection() {
        selectedTagKey = nil
        tagListHasMore = false
    }

    /// Select a sidebar tag (MM-8): every message carrying it across all
    /// accounts and folders except Trash and Junk, newest first. Mutually
    /// exclusive with the other list selections. Pages by keyset through
    /// `loadMoreMessagesIfNeeded`.
    func select(tag: TagDefinition) {
        searchText = ""
        selectedMailbox = nil
        selectedUnifiedRole = nil
        selectedSmartMailboxId = nil
        smartMailboxNotice = nil
        selectedIsSnoozed = false
        selectedTagKey = tag.keywordKey
        clearSelection()
        invalidateInFlightPage()
        nextCursor = nil
        tagListHasMore = false
        inboxSplitFailed = false
        loadTagList(keyword: tag.keywordKey, limit: AppModel.listPageSize)
    }

    /// Fetch `limit` rows of the selected tag's list from the top and replace
    /// `messages` (initial load, and a refresh after a sync that keeps as many
    /// rows as are loaded). A reply for a since-changed selection is dropped.
    private func loadTagList(keyword: String, limit: Int64) {
        let token = listToken
        load({ try $0.messagesForTag(keyword: keyword, limit: limit) }) { rows in
            guard self.selectedTagKey == keyword, token == self.listToken else { return }
            // Invalidate again on landing, as the mailbox reload does: a page
            // requested DURING this load carried the old last row as its
            // cursor, and if new tagged mail pushed rows past `limit` that
            // page no longer continues these rows (appending it would skip them).
            self.invalidateInFlightPage()
            self.messages = rows
            self.tagListHasMore = Int64(rows.count) >= limit
        }
    }

    /// Jump to the mailbox with the given special-use role of the selected message's account,
    /// else the selected mailbox's, else the first account's
    /// (keyboard `g`-chords: g i / g a / g s / g t / g d / g j).
    func selectMailbox(role: String) {
        // The selected message's account first: in a unified list `selectedMailbox` is nil and
        // the first account is the wrong target for a message of the second (#3459).
        let acct = selectedMessage?.accountId ?? selectedMailbox?.accountId ?? accounts.first?.id
        guard let acct, let mbs = mailboxes[acct],
              let mb = mbs.first(where: { $0.role == role }) else { return }
        select(mailbox: mb)
    }

    /// Refresh the visible message list from the DB without disturbing selection
    /// (the list view restores selection by id). Used after a reconciling sync.
    ///
    /// Invalidates in-flight pages twice (S64). First here, at dispatch: a
    /// `loadMoreMessagesIfNeeded` page already in flight captured
    /// `listToken` before dispatching and checks it before appending, so its
    /// append becomes a no-op instead of landing on top of the array this
    /// call is about to replace — and its `isLoadingMore` latch is released
    /// with it, or infinite scroll would stay dead. Then again when the
    /// reloaded rows land: a page requested DURING the reload still carried
    /// the pre-reload cursor, and if the reload brought in newer mail its
    /// rows no longer continue the reloaded list (appending it would skip the
    /// rows the new mail pushed past `limit`).
    func reloadCurrentMessages() {
        guard searchText.isEmpty else { return }
        if let context = correspondence {
            loadCorrespondence(context) // a sync must not snap the list back to a mailbox
            return
        }
        invalidateInFlightPage()
        // A slow reload must not replace a list the user has since switched to:
        // every select(...)/runSearch/newer reload rotates `listToken` (#3441).
        let token = listToken
        if let id = selectedSmartMailboxId, let sm = smartMailboxes.first(where: { $0.id == id }) {
            loadSmartMailboxMessages(sm) // preserves selection — see its doc comment
            return
        }
        if selectedIsSnoozed {
            reloadSnoozedMessages()
            return
        }
        if let key = selectedTagKey {
            // Keep as many rows as are loaded (at least one page), like the keyset path.
            loadTagList(keyword: key, limit: min(max(AppModel.listPageSize, Int64(messages.count)), 10_000))
            return
        }
        if isInboxSplitActive {
            reloadInboxSplit() // HP-2: refresh the open tab and every count
            return
        }
        // A sync completing while the user is deep in an infinite-scrolled
        // list must not snap them back to a 500-row top page: reload as many
        // rows as are currently loaded (at least one page), capped so an
        // enormous already-loaded list can't reload unboundedly. This is one
        // keyset call from the top in the CURRENT sort (#3043 T7); its `next`
        // cursor continues the paging, so no cursor is ever built by hand.
        let limit = min(max(AppModel.listPageSize, Int64(messages.count)), 10_000)
        let sort = effectiveSort
        if let role = selectedUnifiedRole {
            load({ try $0.listMessagesPage(byRole: role, sort: sort, after: nil, limit: limit) }) { page in
                guard token == self.listToken else { return }
                self.invalidateInFlightPage()
                self.messages = page.rows
                self.nextCursor = page.next
            }
            return
        }
        guard let mb = selectedMailbox else { return }
        let (acct, mbox) = (mb.accountId, mb.id)
        load({ try $0.listMessagesPage(accountId: acct, mailboxId: mbox, sort: sort, after: nil, limit: limit) }) { page in
            guard token == self.listToken else { return }
            self.invalidateInFlightPage()
            self.messages = page.rows
            self.nextCursor = page.next
        }
    }

    // MARK: - list sort (#3043)

    /// Columns whose header sorts the current list. From is not sortable in
    /// Sent and Drafts, where the cell shows the recipients (attached after the query).
    var sortableColumnIds: Set<String> {
        // The tag list pages in date order; a column sort would only reorder one page.
        if selectedTagKey != nil && correspondence == nil { return [] }
        var ids: Set<String> = ["date", "from", "subject", "unread", "star", "attach"]
        let role = selectedMailbox?.role ?? selectedUnifiedRole
        if role == "sent" || role == "drafts" { ids.remove("from") }
        return ids
    }

    /// The sort actually applied: the stored choice, unless its column cannot
    /// sort this list (From inside Sent), in which case the default order.
    var effectiveSort: MessageSort {
        sortableColumnIds.contains(messageSort.columnId) ? messageSort : .default
    }

    /// Header click: the same column flips direction, another starts at its natural direction.
    func toggleSort(column id: String) {
        guard let key = MessageSortKey(columnId: id), sortableColumnIds.contains(id) else { return }
        setMessageSort(effectiveSort.toggled(on: key))
    }

    /// Change the sort, persist it, and reload the current list from its first
    /// page in the new order. The selection stays (the list restores it by id).
    func setMessageSort(_ sort: MessageSort) {
        // A header click inside an un-overridden search always counts, even when it
        // names the stored sort: the search showed relevance order, not that sort.
        let overridesSearch = !searchText.isEmpty && !searchSortOverridden
        guard sort != messageSort || overridesSearch else { return }
        if overridesSearch { searchSortOverridden = true }
        messageSort = sort
        sort.save(to: messageSortDefaults)
        // A page in flight carries a cursor of the OLD order; drop it, and never
        // replay the old cursor under the new sort.
        invalidateInFlightPage()
        nextCursor = nil
        resortCurrentList()
    }

    /// Re-run the current list in the current sort (paged lists go back to page one;
    /// the fully loaded ones re-run their loader, which sorts client-side).
    private func resortCurrentList() {
        if !searchText.isEmpty { runSearch(); return }
        if correspondence != nil || selectedSmartMailboxId != nil || selectedIsSnoozed
            || selectedTagKey != nil || isInboxSplitActive {
            reloadCurrentMessages()
            return
        }
        loadPlainFirstPage()
    }

    /// The sort the table header should show, or nil when the list is in its own natural
    /// order: search results (relevance) until a header is clicked, and Snoozed (soonest
    /// due first) while the sort is still the default.
    var displaySort: MessageSort? {
        if !searchText.isEmpty { return searchSortOverridden ? effectiveSort : nil }
        if selectedIsSnoozed && correspondence == nil && effectiveSort == .default { return nil }
        if selectedTagKey != nil && correspondence == nil { return nil }
        return effectiveSort
    }

    /// Fully loaded lists (search, smart mailboxes, Snoozed, Correspondence, split tabs)
    /// sort here, with the core's rules; a list in its natural order is left alone.
    func clientSorted(_ rows: [MessageHeader]) -> [MessageHeader] {
        guard let sort = displaySort else { return rows }
        return MessageSortClient.sorted(rows, by: sort)
    }

    /// Single selection: show the reading pane + apply the auto-mark-read policy.
    func select(message: MessageHeader) {
        showDetail(for: message)
        scheduleAutoMarkRead(for: message)
    }

    /// Load a message into the reading pane WITHOUT auto-marking it read (used for
    /// the primary of a multi-selection — range-selecting isn't "opening" one).
    func showDetail(for message: MessageHeader) {
        cancelBodyHydration()
        cancelAiSummary(forPreviousSelectionOtherThan: message.id)
        selectedMessage = message
        selectedMessageIdForAi = message.id
        let id = message.id
        detail = nil
        currentProjectTags = []
        PerfSignpost.begin(.messageOpen, replacing: &messageOpenSpan)
        load({ try $0.getMessage(id: id) }) { detail in
            guard self.selectedMessage?.id == id else { return }
            self.detail = detail
            if let detail, detail.header.hasBody == false {
                self.hydrateSelectedBody(messageId: id, accountId: detail.header.accountId)
            } else {
                // Body already local: handed to the reading pane.
                PerfSignpost.end(&self.messageOpenSpan)
            }
        } always: {
            // getMessage threw (detail never arrived): close the interval.
            if self.selectedMessage?.id == id, self.detail == nil {
                PerfSignpost.end(&self.messageOpenSpan, count: -1)
            }
        }
        // Reload project tags so the reading pane can show which projects this message belongs to.
        let core = self.core
        Task.detached {
            let tags = (try? core.projectsForMessage(messageId: id)) ?? []
            await MainActor.run {
                guard self.selectedMessage?.id == id else { return }
                self.currentProjectTags = tags
            }
        }
    }

    func retryBodyHydrationForSelection() {
        guard let detail else { return }
        hydrateSelectedBody(messageId: detail.header.id, accountId: detail.header.accountId)
    }

    private func cancelBodyHydration() {
        bodyHydrationTask?.cancel()
        bodyHydrationTask = nil
        bodyHydrationGeneration &+= 1
        isHydratingSelectedBody = false
        selectedBodyHydrationError = nil
    }

    private func hydrateSelectedBody(messageId: String, accountId: String) {
        bodyHydrationTask?.cancel()
        bodyHydrationGeneration &+= 1
        let generation = bodyHydrationGeneration
        selectedBodyHydrationError = nil
        guard let configJSON = storedConfig(for: accountId) else {
            selectedBodyHydrationError = "Stored credentials are unavailable for this account."
            PerfSignpost.end(&messageOpenSpan, count: -1)
            return
        }
        isHydratingSelectedBody = true
        let core = self.core
        bodyHydrationTask = Task.detached {
            do {
                let hydrated = try core.hydrateMessageBody(configJSON: configJSON, messageId: messageId)
                await MainActor.run {
                    guard self.selectedMessage?.id == messageId,
                          self.bodyHydrationGeneration == generation else { return }
                    self.isHydratingSelectedBody = false
                    self.bodyHydrationTask = nil
                    guard let hydrated else {
                        self.selectedBodyHydrationError = "Message is no longer available locally."
                        PerfSignpost.end(&self.messageOpenSpan, count: -1)
                        return
                    }
                    let refreshed = self.detailWithCurrentSelectionState(hydrated)
                    self.detail = refreshed
                    PerfSignpost.end(&self.messageOpenSpan)
                    self.applyHydratedHeader(refreshed.header)
                }
            } catch {
                await MainActor.run {
                    guard self.selectedMessage?.id == messageId,
                          self.bodyHydrationGeneration == generation else { return }
                    self.isHydratingSelectedBody = false
                    self.bodyHydrationTask = nil
                    // Never the raw thrown text: it carries OS errors and server wording.
                    let thrown = String(describing: error)
                    self.selectedBodyHydrationError = AccountSyncStatus.bodyFetchMessage(
                        target: AccountSyncTarget(configJSON: configJSON), thrown: thrown)
                    PerfSignpost.end(&self.messageOpenSpan, count: -1)
                }
            }
        }
    }

    func detailWithCurrentSelectionState(_ hydrated: MessageDetail) -> MessageDetail {
        guard let selected = selectedMessage, selected.id == hydrated.header.id else {
            return hydrated
        }
        var header = hydrated.header
        header.isRead = selected.isRead
        header.isStarred = selected.isStarred
        return MessageDetail(
            header: header,
            rfc822MessageId: hydrated.rfc822MessageId,
            addresses: hydrated.addresses,
            textBody: hydrated.textBody,
            sanitizedHtml: hydrated.sanitizedHtml,
            hasRemoteContent: hydrated.hasRemoteContent,
            attachments: hydrated.attachments,
            listUnsubscribeMailto: hydrated.listUnsubscribeMailto,
            listUnsubscribeUrl: hydrated.listUnsubscribeUrl,
            listUnsubscribeOneClick: hydrated.listUnsubscribeOneClick,
            trust: hydrated.trust,
            threadId: hydrated.threadId,
            calendarInvite: hydrated.calendarInvite,
            references: hydrated.references
        )
    }

    // MARK: - Remote-content sender policy (S48)

    /// Look up (and cache) whether `fromAddress` is pre-approved for remote
    /// content on `accountId`. Never throws into the UI — a lookup failure
    /// behaves exactly like "no policy" (the per-message prompt still shows).
    func remoteContentAllowed(accountId: String, fromAddress: String?) -> Bool {
        guard let fromAddress else { return false }
        let key = "\(accountId)|\(fromAddress.lowercased())"
        if let cached = senderRemoteContentCache[key] { return cached }
        let allowed = (try? core.senderRemoteContentAllowed(accountId: accountId, fromAddress: fromAddress)) ?? nil
        let resolved = allowed ?? false
        senderRemoteContentCache[key] = resolved
        return resolved
    }

    /// "Always allow images from this sender" — the banner's remember action.
    func allowRemoteContentAlways(accountId: String, fromAddress: String) {
        do {
            try core.setSenderRemoteContentAllowed(accountId: accountId, fromAddress: fromAddress, allowed: true)
            senderRemoteContentCache["\(accountId)|\(fromAddress.lowercased())"] = true
        } catch {
            // Best-effort: the per-message "Load" button still worked for
            // this viewing, only the REMEMBER half failed silently — no
            // alert, matching the app's low-ceremony treatment of
            // preference-write failures elsewhere (e.g. signatures).
        }
    }

    /// Every remembered sender policy across every account, for Settings'
    /// review list. Reloaded each time Settings' General tab appears.
    ///
    /// S72 main-thread audit: this ran the FFI call ON the main actor from
    /// the General tab's `onAppear`, so while a first sync or a contacts
    /// refresh held the DB connection, switching Settings tabs froze the
    /// whole app until the lock came free (Marc, 2026-09-26: tab clicks did
    /// nothing, then replayed). Off-actor now, like every `load` call.
    func reloadSenderRemoteContentPolicies() {
        let core = self.core
        Task.detached {
            let policies = (try? core.listSenderRemoteContentPolicies()) ?? []
            await MainActor.run { self.senderRemoteContentPolicies = policies }
        }
    }

    func revokeSenderRemoteContent(_ policy: SenderRemoteContentPolicy) {
        let core = self.core
        Task.detached {
            try? core.revokeSenderRemoteContent(accountId: policy.accountId, fromAddress: policy.fromAddress)
            await MainActor.run {
                self.senderRemoteContentCache.removeValue(
                    forKey: "\(policy.accountId)|\(policy.fromAddress.lowercased())")
                self.reloadSenderRemoteContentPolicies()
            }
        }
    }

    // MARK: - CoreSpotlight indexing

    /// Reads the persisted opt-in from `app_meta` — call once at
    /// `bootstrap()`. A read failure leaves the fail-closed `false` default
    /// in place (never assume ON).
    func reloadSpotlightIndexingEnabled() {
        load({ try $0.spotlightIndexingEnabled() }) { self.spotlightIndexingEnabled = $0 }
    }

    /// Flip the CoreSpotlight opt-in: persists via
    /// `core.setSpotlightIndexingEnabled` (app_meta — travels with the
    /// profile, not `Preferences`), then off the main actor either runs the
    /// one-off backfill (turning on) or domain-scoped removal for every
    /// account (turning off) — see `SpotlightIndexer`. The FFI write and
    /// both side effects run in the SAME detached task, so a rapid
    /// on/off/on toggle still applies in the order the user made it rather
    /// than racing across separate tasks.
    func setSpotlightIndexingEnabled(_ enabled: Bool) {
        spotlightIndexingEnabled = enabled
        let core = self.core
        let indexer = spotlightIndexer
        let accountIds = accounts.map(\.id)
        let accountsSnapshot = accounts
        Task.detached {
            do {
                try core.setSpotlightIndexingEnabled(enabled)
            } catch {
                await MainActor.run {
                    self.errorText = "Could not save the Spotlight indexing setting: \(error)"
                }
                return
            }
            if enabled {
                await indexer.backfillAll(core: core, accounts: accountsSnapshot)
            } else {
                for accountId in accountIds {
                    await indexer.removeAll(forAccount: accountId)
                }
            }
        }
    }

    private func applyHydratedHeader(_ header: MessageHeader) {
        if let idx = messages.firstIndex(where: { $0.id == header.id }) {
            messages[idx] = header
        }
        if selectedMessage?.id == header.id {
            selectedMessage = header
        }
    }

    // MARK: - incoming attachments (S32)

    func attachmentKey(messageId: String, ordinal: Int64) -> String {
        "\(messageId)#\(ordinal)"
    }

    /// Local-only poll: refresh `attachmentDownloads[key]` from the on-disk
    /// cache state. Call on message selection for every attachment in the
    /// header's list, and again right after a download completes.
    func refreshAttachmentCacheStatus(messageId: String, ordinal: Int64) {
        let key = attachmentKey(messageId: messageId, ordinal: ordinal)
        // Never clobber an in-flight download with a stale "checking" poll.
        if case .downloading = attachmentDownloads[key] { return }
        attachmentDownloads[key] = .checking
        let core = self.core
        Task.detached {
            let result = try? core.attachmentCacheStatus(messageId: messageId, ordinal: ordinal)
            await MainActor.run {
                if case .downloading = self.attachmentDownloads[key] { return }
                guard let result else {
                    self.attachmentDownloads[key] = .idle
                    return
                }
                self.applyAttachmentResult(result, key: key)
            }
        }
    }

    /// Request a download. A no-op if one is already in flight for this key.
    func downloadAttachment(messageId: String, ordinal: Int64, accountId: String) {
        let key = attachmentKey(messageId: messageId, ordinal: ordinal)
        if case .downloading = attachmentDownloads[key] { return }
        guard let configJSON = storedConfig(for: accountId) else {
            attachmentDownloads[key] = .failed("Stored credentials are unavailable for this account.")
            return
        }
        attachmentDownloadGenerations[key, default: 0] &+= 1
        let generation = attachmentDownloadGenerations[key] ?? 0
        attachmentDownloads[key] = .downloading
        let core = self.core
        attachmentDownloadTasks[key] = Task.detached {
            do {
                let result = try core.downloadAttachment(
                    configJSON: configJSON, messageId: messageId, ordinal: ordinal)
                await MainActor.run {
                    guard self.attachmentDownloadGenerations[key] == generation else { return }
                    self.applyAttachmentResult(result, key: key)
                    self.attachmentDownloadTasks[key] = nil
                }
            } catch {
                await MainActor.run {
                    guard self.attachmentDownloadGenerations[key] == generation else { return }
                    self.attachmentDownloads[key] = .failed(String(describing: error))
                    self.attachmentDownloadTasks[key] = nil
                }
            }
        }
    }

    /// Best-effort cancel: marks the request stale via the generation guard
    /// so its eventual result is discarded, same as `cancelBodyHydration()`.
    /// The underlying blocking FFI call keeps running to completion on its
    /// detached thread — there is no mid-flight IMAP abort in this codebase
    /// today (see `hydrateSelectedBody`'s identical limitation).
    func cancelAttachmentDownload(messageId: String, ordinal: Int64) {
        let key = attachmentKey(messageId: messageId, ordinal: ordinal)
        attachmentDownloadGenerations[key, default: 0] &+= 1
        attachmentDownloadTasks[key]?.cancel()
        attachmentDownloadTasks[key] = nil
        if case .downloading = attachmentDownloads[key] {
            attachmentDownloads[key] = .idle
        }
    }

    private func applyAttachmentResult(_ result: AttachmentDownloadResult, key: String) {
        switch result.status {
        case "cached", "downloaded":
            if let path = result.localPath {
                attachmentDownloads[key] = .ready(localPath: path)
            } else {
                attachmentDownloads[key] = .failed("No local path returned.")
            }
        case "not_cached":
            attachmentDownloads[key] = .idle
        default:
            attachmentDownloads[key] = .failed(result.error ?? "Download failed.")
        }
    }

    // MARK: - calendar invites (S55)

    /// Per-message state of an Accept/Decline/Tentative action in flight,
    /// keyed by message id. Mirrors `attachmentDownloads`'s per-key shape.
    enum CalendarInviteResponseState: Equatable {
        case sending
        case failed(String)
        /// The reply would go to an ORGANIZER address that matches neither
        /// From nor Reply-To (#3380): the card asks the user to confirm the
        /// named recipient before anything is queued.
        case needsRecipientConfirmation(response: String, address: String)
    }

    @Published var calendarInviteResponses: [String: CalendarInviteResponseState] = [:]

    /// Record `response` for `messageId`'s calendar invite and queue +
    /// immediately drain an iTIP REPLY to its organizer through the normal
    /// outbox (`respondToCalendarInviteJson` already queued it; this just
    /// triggers the same drain a compose Send does). A no-op if a response
    /// for this message is already in flight. `response` is `"accepted"`,
    /// `"declined"` or `"tentative"` — `CalendarInviteCardView` is the only
    /// caller and only ever passes one of those. `confirmedRecipient` is the
    /// address the confirmation prompt named (nil on the first tap); the core
    /// sends only if that is still exactly where the reply would go, and
    /// otherwise answers with a fresh mismatch so the card asks again about
    /// the current address (#3472).
    func respondToCalendarInvite(
        messageId: String, accountId: String, response: String, confirmedRecipient: String? = nil
    ) {
        if case .sending = calendarInviteResponses[messageId] { return }
        calendarInviteResponses[messageId] = .sending
        let core = self.core
        Task.detached {
            do {
                _ = try core.respondToCalendarInvite(
                    messageId: messageId, response: response, confirmedRecipient: confirmedRecipient)
                let refreshed = try? core.getMessage(id: messageId)
                await MainActor.run {
                    self.calendarInviteResponses[messageId] = nil
                    if let refreshed, self.selectedMessage?.id == messageId {
                        self.detail = refreshed
                    }
                    self.drainSendPending(accountId: accountId, announceFailure: false)
                }
            } catch {
                await MainActor.run {
                    if let address = CalendarInviteRecipientPolicy.mismatchAddress(error) {
                        self.calendarInviteResponses[messageId] =
                            .needsRecipientConfirmation(response: response, address: address)
                    } else {
                        self.calendarInviteResponses[messageId] = .failed(String(describing: error))
                    }
                }
            }
        }
    }

    // MARK: - List-Unsubscribe (S44)

    /// Perform the RFC 8058 one-click POST for the currently-shown
    /// unsubscribe candidate. Only ever called from the confirmation sheet's
    /// "Unsubscribe" button — never automatically.
    func confirmUnsubscribeOneClick() {
        // The sheet only offers this button for an `UnsubscribePlan.oneClick`;
        // the core independently refuses anything that is not a one-click https
        // target, so the defensive paths stay covered there.
        guard let candidate = unsubscribeCandidate else { return }
        unsubscribeCandidate = nil
        let core = self.core
        Task.detached {
            do {
                let result = try core.unsubscribeOneClick(messageId: candidate.header.id)
                await MainActor.run {
                    self.unsubscribeStatusText = result.status == "sent"
                        ? "Unsubscribe request sent." : "This message has no one-click unsubscribe link."
                }
            } catch {
                await MainActor.run { self.errorText = String(describing: error) }
            }
        }
    }

    /// Create and send a short unsubscribe email to the candidate's mailto:
    /// target, through the ordinary compose/send pipeline (`deliverDraft`:
    /// crash-safe, write-safety routed). Delivery is attempted NOW and the
    /// success text is only set once SMTP accepted it (#3049: it used to
    /// queue with delay 0 and never drain, yet claim "sent"). A refused or
    /// failed attempt deletes the draft it created (#3047: no stray drafts)
    /// and, for an evaluation-mode refusal, offers Enable Writes with a retry
    /// of this same action. The mailto's `body=` is carried (#3050). Only
    /// ever called from the confirmation sheet.
    func confirmUnsubscribeByMailto() {
        guard let candidate = unsubscribeCandidate,
              case .mailto(let address, let subject, let body) = UnsubscribePlan.make(for: candidate),
              let accountId = accounts.first(where: { $0.id == candidate.header.accountId })?.id
        else { return }
        unsubscribeCandidate = nil
        let draft = NewDraft(
            accountId: accountId,
            to: [DraftAddress(name: nil, email: address)],
            subject: subject,
            bodyText: body)
        let core = self.core
        Task.detached {
            let draftId: Int64
            do {
                draftId = try core.saveDraft(draft)
            } catch {
                await MainActor.run { self.errorText = String(describing: error) }
                return
            }
            await MainActor.run {
                self.deliverDraft(id: draftId, accountId: accountId) { outcome in
                    switch outcome {
                    case .sent:
                        self.unsubscribeStatusText = "Unsubscribe email sent."
                    case .queued, .scheduled, .leftInOutbox:
                        // Durably queued but not delivered: say so, never "sent".
                        var why = ""
                        if case .leftInOutbox(let reason) = outcome { why = " (\(reason))" }
                        self.errorText = "Unsubscribe email is waiting in the Outbox\(why)."
                    case .failed(let reason):
                        try? core.deleteDraft(id: draftId)
                        self.reloadDrafts(accountId)
                        if self.writeSafetyPrompt != nil {
                            // Evaluation mode: deliverDraft already raised the
                            // Enable Writes dialog; re-run this action after.
                            self.registerWriteSafetyRetry(accountId: accountId) {
                                self.unsubscribeCandidate = candidate
                                self.confirmUnsubscribeByMailto()
                            }
                        } else {
                            self.errorText = "Unsubscribe email not sent: \(reason)"
                        }
                    case .missingCredentials:
                        try? core.deleteDraft(id: draftId)
                        self.reloadDrafts(accountId)
                        self.errorText = "Unsubscribe email not sent: no stored credentials for this account."
                    }
                }
            }
        }
    }

    // MARK: - Raw source + .eml import/export (S49)

    /// The header block of the message's raw source IF it is already cached on
    /// this Mac (local filesystem only, never the network). For the "All
    /// Headers" mode of the reader (MM-13).
    func cachedRawHeaderBlock(messageId: String) -> String? {
        guard let status = try? core.rawSourceCacheStatus(messageId: messageId),
              status.status == "cached", let path = status.localPath else { return nil }
        return Self.headerBlock(atPath: path)
    }

    /// Fetch the raw source (network round trip unless cached; the user asked
    /// for it with "Load full headers") and return its header block.
    func loadRawHeaderBlock(messageId: String, accountId: String) async -> String? {
        guard let path = try? await fetchRawSourcePath(messageId: messageId, accountId: accountId)
        else { return nil }
        return Self.headerBlock(atPath: path)
    }

    /// Only the first 256 KB of the file is read: headers are at the top.
    nonisolated private static func headerBlock(atPath path: String) -> String? {
        guard let handle = FileHandle(forReadingAtPath: path) else { return nil }
        defer { try? handle.close() }
        guard let data = try? handle.read(upToCount: 256 * 1024) else { return nil }
        let block = RawHeaderBlock.extract(from: String(decoding: data, as: UTF8.self))
        return block.isEmpty ? nil : block
    }

    /// Fetch the message's full raw RFC822 source (cached if a prior view
    /// already fetched it) and return its local path. Feeds both "Message
    /// Source" and `.eml` export — no persisted raw source exists
    /// otherwise, so every call may be a real network round trip.
    func fetchRawSourcePath(messageId: String, accountId: String) async throws -> String {
        guard let configJSON = storedConfig(for: accountId) else {
            throw CastellumError.message("Stored credentials are unavailable for this account.")
        }
        let core = self.core
        let result = try await Task.detached {
            try core.fetchRawSource(configJSON: configJSON, messageId: messageId)
        }.value
        guard result.status == "cached" || result.status == "downloaded",
              let path = result.localPath else {
            throw CastellumError.message(result.error ?? "Couldn't fetch the message source.")
        }
        return path
    }

    /// Import local `.eml` files into `mailboxId` on `accountId` via the
    /// durable operation queue (no network from this call — replay APPENDs
    /// them). Any per-file failure is collected and surfaced once via
    /// `errorText`; a successful import kicks the existing replay debounce
    /// so it reaches the server promptly.
    func importEmlFiles(urls: [URL], mailboxId: String, accountId: String) {
        guard !urls.isEmpty else { return }
        let core = self.core
        Task {
            var failures: [String] = []
            var anyQueued = false
            for url in urls {
                let path = url.path
                do {
                    let result = try await Task.detached {
                        try core.importEml(accountId: accountId, mailboxId: mailboxId, filePath: path)
                    }.value
                    if result.status == "queued" {
                        anyQueued = true
                    } else {
                        failures.append("\(url.lastPathComponent): \(result.error ?? "unknown error")")
                    }
                } catch {
                    failures.append("\(url.lastPathComponent): \(String(describing: error))")
                }
            }
            if !failures.isEmpty {
                self.errorText = "Couldn't import:\n" + failures.joined(separator: "\n")
            }
            if anyQueued {
                self.scheduleReplay()
            }
        }
    }

    func summarizeSelectedMessage() {
        guard messageBodyActionsAvailable(detail),
              let id = selectedMessageIdForAi,
              detail?.header.id == id else { return }
        aiCoordinator.summarize(messageId: id)
    }

    /// "Find similar" (S53-UI): embeds `anchor` (ad-hoc, reusing its stored
    /// embedding when one exists) then finds its nearest neighbours by
    /// cosine similarity — opens the results sheet immediately (spinner
    /// state) and never touches `searchText` or any live-search state, per
    /// DECISION-S53-ui. Deliberately takes the anchor's full `MessageHeader`
    /// (available at both call sites — the reading pane's `detail.header`
    /// and a message-list row) rather than a bare id: it sets
    /// `findSimilarAnchor` for the sheet's "Similar to: <subject>" header
    /// without a second lookup that could fail if the row has since
    /// scrolled out of `messages`.
    ///
    /// Never blocks the main thread: the FFI call — which may take longer
    /// than an ordinary search (a network round trip to Ollama on the
    /// ad-hoc-embed path) — runs off the main actor via `Task.detached`,
    /// mirroring every other AI-adjacent FFI call site in this file.
    func findSimilar(anchor: MessageHeader) {
        findSimilarAnchor = anchor
        findSimilarResults = []
        findSimilarError = nil

        // The chat model cannot embed (Ollama answers HTTP 500), and nothing indexes mail
        // without an embedding model, so say so up front instead of trying the chat model.
        let prefs = Preferences.shared
        if prefs.aiEnabled, prefs.aiEmbeddingModel.isEmpty {
            findSimilarError = String(localized: "Find Similar needs an embedding model: choose one in Settings → AI")
            findSimilarInFlight = false
            return
        }
        guard let configJSON = findSimilarConfigJSON() else {
            findSimilarError = "Set an Ollama endpoint and model in Settings → AI first."
            findSimilarInFlight = false
            return
        }
        findSimilarInFlight = true
        let core = self.core
        let messageId = anchor.id
        Task.detached { [weak self] in
            do {
                let results = try core.findSimilarMessages(
                    configJSON: configJSON, messageId: messageId, accountId: nil, limit: 20)
                await MainActor.run {
                    guard let self, self.findSimilarAnchor?.id == messageId else { return }
                    self.findSimilarResults = results
                    self.findSimilarInFlight = false
                }
            } catch {
                await MainActor.run {
                    guard let self, self.findSimilarAnchor?.id == messageId else { return }
                    let p = Preferences.shared
                    self.findSimilarError = Self.findSimilarMessage(
                        for: String(describing: error),
                        model: p.aiEmbeddingModel)
                    self.findSimilarInFlight = false
                }
            }
        }
    }

    /// Readable Find Similar failure text. Never the raw transport error (URLs,
    /// OS error numbers); every branch points at Settings → AI where it helps.
    nonisolated static func findSimilarMessage(for raw: String, model: String) -> String {
        let text = raw.lowercased()
        if text.contains("http 500") {
            // Ollama answers 500 for a model that cannot embed (a chat model).
            return String(format: String(localized: "%@ cannot produce embeddings; choose an embedding model"), model)
        }
        if text.contains("http 404") || text.contains("not found") {
            return String(format: String(localized: "Embedding model %@ is not installed at the endpoint; choose another"), model)
        }
        if text.contains("network error") || text.contains("connection refused") || text.contains("timed out") {
            return String(localized: "Ollama is not answering. Check the endpoint in Settings → AI.")
        }
        return String(localized: "Find Similar couldn’t finish. Check the AI settings in Settings → AI.")
    }

    /// Dismiss the Find Similar sheet (clears every piece of its state, so
    /// reopening it on a new anchor starts from a clean slate).
    func dismissFindSimilar() {
        findSimilarAnchor = nil
        findSimilarResults = []
        findSimilarError = nil
        findSimilarInFlight = false
    }

    /// The AI run config JSON `castellum_find_similar_messages` needs
    /// (same shape `AiCoordinator.buildRunConfigJSON` builds for
    /// `castellum_run_ai_jobs` — endpoint + model; `nil` when AI isn't
    /// configured, matching `AiCoordinator.summarize`'s own guard).
    private func findSimilarConfigJSON() -> String? {
        let prefs = Preferences.shared
        guard prefs.aiEnabled, !prefs.aiEmbeddingModel.isEmpty else { return nil }
        struct ConfigPayload: Encodable {
            let endpoint: String
            let model: String
            /// F1: the chosen embedding model (never empty here; see `findSimilar`).
            let embeddingModel: String
        }
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        guard let data = try? encoder.encode(
            ConfigPayload(endpoint: prefs.aiEndpoint,
                          model: prefs.aiModel.isEmpty ? prefs.aiEmbeddingModel : prefs.aiModel,
                          embeddingModel: prefs.aiEmbeddingModel))
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    func clearSelection() {
        autoMarkTimer?.cancel()
        cancelBodyHydration()
        cancelAiSummary(forPreviousSelectionOtherThan: nil)
        selectedMessage = nil
        selectedMessageIds = []
        detail = nil
        currentProjectTags = []
        selectedMessageIdForAi = nil
    }

    /// Navigating off a message cancels its in-flight summary, mirroring the
    /// body-hydration cancel above it. The reading pane is the only place that
    /// summary could appear, so once it is gone the job is generating into
    /// nothing while holding an Ollama slot — and its entry would linger in
    /// `AiCoordinator.jobs` until restart.
    ///
    /// A no-op when the selection is not actually changing (a reload re-selects
    /// the same message), so scrolling back to a message still finds its
    /// summary running.
    private func cancelAiSummary(forPreviousSelectionOtherThan newId: String?) {
        guard let previous = selectedMessageIdForAi, previous != newId else { return }
        aiCoordinator.cancelActiveJobs(forOwnerId: previous, feature: "summarize_message")
    }

    // MARK: - mark read/unread actions

    /// Apply the configured auto-mark-read policy when a message is opened.
    private func scheduleAutoMarkRead(for message: MessageHeader) {
        autoMarkTimer?.cancel()
        guard !message.isRead else { return }
        switch Preferences.shared.autoMarkReadMode {
        case .never:
            break
        case .onOpen:
            markRead(message: message, isRead: true, automatic: true)
        case .afterDelay:
            let seconds = UInt64(max(1, Preferences.shared.markReadDelaySeconds))
            autoMarkTimer = Task { [weak self] in
                try? await Task.sleep(nanoseconds: seconds * 1_000_000_000)
                guard !Task.isCancelled, let self else { return }
                // Only if this message is still selected AND still unread (an
                // explicit toggle in the meantime must win).
                if self.selectedMessage?.id == message.id, self.selectedMessage?.isRead == false {
                    self.markRead(message: message, isRead: true, automatic: true)
                }
            }
        }
    }

    /// Optimistically flip one message's read state, then persist + replay.
    /// `automatic` marks the auto-mark-on-open side effect, which stays local
    /// (and is never refused) while the account is read-only.
    func markRead(message: MessageHeader, isRead: Bool, automatic: Bool = false) {
        setReadState(ids: [message.id], isRead: isRead, automatic: automatic)
    }

    /// Set read state on many messages at once (⇧⌘U over a multi-selection).
    /// Only messages whose state actually changes are touched (no redundant ops).
    ///
    /// Defaults to the EXPLICIT path, deliberately: an explicit toggle flips a
    /// server \Seen and is refused on a read-only account like every other
    /// write, so it must revert the optimistic flip and raise the read-only
    /// notice rather than appearing to succeed and being reverted by the next
    /// sync. Only the auto-mark-on-open callers pass `automatic: true`.
    func setReadState(ids: [String], isRead: Bool, automatic: Bool = false) {
        // Any explicit mark cancels a pending auto-mark timer so it can't later
        // clobber the choice with a stale captured state.
        autoMarkTimer?.cancel()
        let changing = ids.filter { id in
            (messages.first { $0.id == id }?.isRead ?? !isRead) != isRead
                || (selectedMessage?.id == id && selectedMessage?.isRead != isRead)
        }
        guard !changing.isEmpty else { return }
        let accountForId = Dictionary(
            changing.compactMap { id -> (String, String)? in
                guard let m = messages.first(where: { $0.id == id }) else { return nil }
                return (id, m.accountId)
            },
            uniquingKeysWith: { first, _ in first })
        let accts = Set(accountForId.values)
        // Snapshot what was ACTUALLY there before the optimistic flip.
        // `changing` admits an id when EITHER the list row or `selectedMessage`
        // disagrees with the target, and the two can genuinely differ: a sync
        // replaces `messages` from the DB while `selectedMessage` keeps its
        // stale copy (the reload re-selects with `isReloading` set, which
        // suppresses the selection callback). Reverting to `!isRead` would then
        // write a state neither slot ever held — the read-only account would
        // end up LOOKING changed by a refusal, which is the exact invariant
        // this revert exists to protect.
        let priorInList = Dictionary(
            changing.compactMap { id -> (String, Bool)? in
                guard let m = messages.first(where: { $0.id == id }) else { return nil }
                return (id, m.isRead)
            },
            uniquingKeysWith: { first, _ in first })
        let priorSelected: (id: String, isRead: Bool)? = selectedMessage.map { ($0.id, $0.isRead) }
        for id in changing { applyReadState(messageId: id, isRead: isRead) }
        let core = self.core
        Task.detached {
            var failedIds: [String] = []
            var firstError: String?
            var firstWasEvaluationMode = false
            for id in changing {
                do {
                    if automatic {
                        try core.markReadAuto(messageId: id, isRead: isRead)
                    } else {
                        try core.markRead(messageId: id, isRead: isRead)
                    }
                } catch {
                    failedIds.append(id)
                    if firstError == nil {
                        firstError = String(describing: error)
                        firstWasEvaluationMode = AccountWritePolicy.isEvaluationModeRefusal(error)
                    }
                }
            }
            let (reverting, reason, isEvaluation) = (failedIds, firstError, firstWasEvaluationMode)
            let failedAccount = failedIds.first.flatMap { accountForId[$0] }
            await MainActor.run {
                // Nothing was applied for these, so restore exactly what each
                // slot held before — not `!isRead`, which the two slots need
                // not agree on (see the snapshot above).
                for id in reverting {
                    if let was = priorInList[id],
                       let idx = self.messages.firstIndex(where: { $0.id == id }) {
                        self.messages[idx].isRead = was
                    }
                    if let prior = priorSelected, prior.id == id {
                        self.selectedMessage?.isRead = prior.isRead
                    }
                }
                if let reason {
                    // The account that actually refused, not merely the first
                    // in the selection — `messages` is single-account today,
                    // but a unified inbox would make those differ.
                    self.reportWriteFailure(
                        verb: isRead ? "Mark as read" : "Mark as unread", accountId: failedAccount,
                        isEvaluationMode: isEvaluation, reason: reason)
                }
                self.reloadCountsAfterMutation(accounts: accts)
                self.refreshPendingCount()
                self.scheduleReplay()
            }
        }
    }

    /// Toggle read/unread on the current selection (⇧⌘U). Flips the whole
    /// multi-selection to the opposite of the primary's current state.
    func toggleReadUnread() {
        let ids = currentSelectionIds()
        guard !ids.isEmpty else { return }
        let anchorRead = selectedMessage?.isRead
            ?? messages.first { $0.id == ids[0] }?.isRead
            ?? false
        setReadState(ids: ids, isRead: !anchorRead)
    }

    /// Explicit Mark as Read / Mark as Unread on the whole selection (row menu).
    func setReadStateOnSelection(isRead: Bool) {
        let ids = currentSelectionIds()
        guard !ids.isEmpty else { return }
        setReadState(ids: ids, isRead: isRead)
    }

    /// Toggle starred/flagged on the current selection (flips all to the opposite
    /// of the primary's current state).
    func toggleFlagSelected() {
        let ids = currentSelectionIds()
        guard !ids.isEmpty else { return }
        let anchorStarred = selectedMessage?.isStarred
            ?? messages.first { $0.id == ids[0] }?.isStarred
            ?? false
        setStarred(ids: ids, starred: !anchorStarred)
    }

    /// Set starred state on many messages; only changed ones are touched.
    func setStarred(ids: [String], starred: Bool) {
        let changing = ids.filter { id in
            (messages.first { $0.id == id }?.isStarred ?? !starred) != starred
                || (selectedMessage?.id == id && selectedMessage?.isStarred != starred)
        }
        guard !changing.isEmpty else { return }
        // Each id's own account: a mixed-account selection must blame the account that
        // actually refused, not the first row's (#3459).
        let accountForId = Dictionary(
            changing.compactMap { id -> (String, String)? in
                guard let m = messages.first(where: { $0.id == id }) else { return nil }
                return (id, m.accountId)
            },
            uniquingKeysWith: { first, _ in first })
        // Each slot's OWN prior state: after a sync reload the list row can already
        // be starred while the (stale) selected header is not, so a failed write
        // must put THAT back, not blindly !starred (#3441), as setReadState does.
        var priorInList: [String: Bool] = [:]
        for id in changing {
            if let row = messages.first(where: { $0.id == id }) { priorInList[id] = row.isStarred }
        }
        let priorSelected = selectedMessage.map { (id: $0.id, starred: $0.isStarred) }
        let priorList = priorInList
        for id in changing { applyStarState(messageId: id, starred: starred) }
        let core = self.core
        Task.detached {
            var failedIds: [String] = []
            var firstError: String?
            var firstWasEvaluationMode = false
            for id in changing {
                do { try core.setFlagged(messageId: id, flagged: starred) } catch {
                    failedIds.append(id)
                    if firstError == nil {
                        firstError = String(describing: error)
                        firstWasEvaluationMode = AccountWritePolicy.isEvaluationModeRefusal(error)
                    }
                }
            }
            let (reverting, reason, isEvaluation) = (failedIds, firstError, firstWasEvaluationMode)
            let failedAccount = failedIds.first.flatMap { accountForId[$0] }
            await MainActor.run {
                // Nothing reached the server for these, so undo the optimistic
                // flip: a read-only account must never LOOK as if it changed.
                for id in reverting {
                    if let idx = self.messages.firstIndex(where: { $0.id == id }),
                       let was = priorList[id] {
                        self.messages[idx].isStarred = was
                    }
                    if let prior = priorSelected, prior.id == id, self.selectedMessage?.id == id {
                        self.selectedMessage?.isStarred = prior.starred
                    }
                }
                if let reason {
                    self.reportWriteFailure(
                        verb: starred ? "Flag" : "Unflag", accountId: failedAccount,
                        isEvaluationMode: isEvaluation, reason: reason)
                }
                self.refreshSmartMailboxCountsIfAny()
                self.refreshPendingCount()
                self.scheduleReplay()
            }
        }
    }

    private func applyStarState(messageId: String, starred: Bool) {
        if let idx = messages.firstIndex(where: { $0.id == messageId }) {
            messages[idx].isStarred = starred
        }
        if selectedMessage?.id == messageId { selectedMessage?.isStarred = starred }
    }

    // MARK: - tags and mute (S42)

    /// Add or remove `keyword` on the current selection. Apple Mail's
    /// flag-toggle rule: add if ANY selected message lacks it, else remove
    /// from all of them. Case-insensitive match (IMAP keywords are
    /// case-insensitive on the wire).
    func toggleTag(_ keyword: String) {
        let ids = currentSelectionIds()
        guard !ids.isEmpty else { return }
        let add = ids.contains { id in
            !(messages.first { $0.id == id }?.keywords
                .contains { $0.caseInsensitiveCompare(keyword) == .orderedSame } ?? false)
        }
        // Remember each message's own prior state: a failed write must restore
        // THAT, not blindly invert the toggle (a mixed selection has messages
        // that already carried the tag and were never changed by the core).
        var hadTag: [String: Bool] = [:]
        for id in ids {
            hadTag[id] = messages.first { $0.id == id }?.keywords
                .contains { $0.caseInsensitiveCompare(keyword) == .orderedSame } ?? false
        }
        for id in ids { applyTagState(messageId: id, keyword: keyword, add: add) }
        let priorState = hadTag
        let account = messages.first { ids.contains($0.id) }?.accountId
        let core = self.core
        Task.detached {
            var failedIds: [String] = []
            var firstError: String?
            var firstWasEvaluationMode = false
            var localOnlyReason: String?
            for id in ids {
                // Only write ids whose state actually changes: re-adding a tag a
                // message already has overwrites its stored spelling/origin and
                // queues a redundant keyword op (#3457).
                if priorState[id] == add { continue }
                do {
                    let outcome = try core.setMessageTag(messageId: id, keyword: keyword, add: add)
                    if !outcome.synced, localOnlyReason == nil {
                        localOnlyReason = outcome.reason
                    }
                } catch {
                    failedIds.append(id)
                    if firstError == nil {
                        firstError = String(describing: error)
                        firstWasEvaluationMode = AccountWritePolicy.isEvaluationModeRefusal(error)
                    }
                }
            }
            let (reverting, reason, isEvaluation, notice) =
                (failedIds, firstError, firstWasEvaluationMode, localOnlyReason)
            await MainActor.run {
                // Nothing reached the server for these — undo the optimistic flip.
                for id in reverting {
                    self.applyTagState(messageId: id, keyword: keyword, add: priorState[id] ?? !add)
                }
                if let reason {
                    self.reportWriteFailure(
                        verb: add ? "Tag" : "Untag", accountId: account,
                        isEvaluationMode: isEvaluation, reason: reason)
                    self.lastTagLocalOnlyNotice = nil
                } else if let notice {
                    // The wording depends on direction: removing a server tag
                    // in evaluation mode only hides it here, and the next sync
                    // brings it back (FP43).
                    if notice == "evaluation_mode" && !add {
                        self.lastTagLocalOnlyNotice = String(localized: "Tag removed on this Mac only: this account is in evaluation mode, so the server still has it and it will return at the next sync.")
                    } else {
                        self.lastTagLocalOnlyNotice = notice == "evaluation_mode"
                            ? "Tag kept on this Mac only: this account is in evaluation mode."
                            : "Tag kept on this Mac only: this mailbox doesn't allow new tags."
                    }
                } else {
                    self.lastTagLocalOnlyNotice = nil
                }
                self.refreshSmartMailboxCountsIfAny()
                self.reloadTagCounts()
                // Inside the tag's own list, an untagged message leaves it.
                if let open = self.selectedTagKey,
                   open.caseInsensitiveCompare(keyword) == .orderedSame {
                    self.reloadCurrentMessages()
                }
                self.refreshPendingCount()
                self.scheduleReplay()
            }
        }
    }

    private func applyTagState(messageId: String, keyword: String, add: Bool) {
        func applied(_ keywords: [String]) -> [String] {
            var keywords = keywords
            if add {
                if !keywords.contains(where: { $0.caseInsensitiveCompare(keyword) == .orderedSame }) {
                    keywords.append(keyword)
                }
            } else {
                keywords.removeAll { $0.caseInsensitiveCompare(keyword) == .orderedSame }
            }
            return keywords
        }
        if let idx = messages.firstIndex(where: { $0.id == messageId }) {
            messages[idx].keywords = applied(messages[idx].keywords)
        }
        if selectedMessage?.id == messageId {
            selectedMessage?.keywords = applied(selectedMessage?.keywords ?? [])
        }
    }

    /// Create a tag definition from a display name (next unused palette
    /// colour, round-robin) and reload `tags`. Returns the created row, or
    /// `nil` on failure (`errorText` is set). `keyword` is empty to derive one
    /// from the display name; pass the exact server spelling when adopting a
    /// keyword already in use, so it is kept verbatim (re-deriving would
    /// rewrite "Follow/up" to "Follow_up").
    @discardableResult
    func createTag(displayName: String, colour: String, keyword: String = "") -> TagDefinition? {
        do {
            let created = try core.upsertTag(
                TagDefinition(keyword: keyword, keywordKey: "", displayName: displayName, colour: colour, sortOrder: Int64(tags.count)))
            tags.append(created)
            reloadTagCounts()
            return created
        } catch {
            errorText = String(describing: error)
            return nil
        }
    }

    /// Rename/recolour/reorder an existing tag definition in place.
    func updateTag(_ tag: TagDefinition) {
        do {
            let saved = try core.upsertTag(tag)
            if let idx = tags.firstIndex(where: { $0.keywordKey == saved.keywordKey }) {
                tags[idx] = saved
            }
        } catch {
            errorText = String(describing: error)
        }
    }

    /// Delete a tag definition (the definition only — see
    /// `CastellumCore.deleteTag`'s doc comment).
    func deleteTag(_ tag: TagDefinition) {
        do {
            try core.deleteTag(keyword: tag.keywordKey)
            tags.removeAll { $0.keywordKey == tag.keywordKey }
            tagCounts[tag.keywordKey] = nil
            if selectedTagKey == tag.keywordKey {
                // The list the user was looking at no longer exists: back to the default.
                clearTagSelection()
                messages = []
                clearSelection()
                selectDefaultMailboxIfNothingSelected()
            }
        } catch {
            errorText = String(describing: error)
        }
    }

    /// Mute/unmute the current selection's (primary message's) conversation.
    /// Local only — never refused, so there is no evaluation-mode notice
    /// here. `MessageHeader` carries no `thread_id` (S42 keeps mute keyed
    /// by Message-ID on the Rust side, never exposed over the ABI), so the
    /// optimistic update touches only this row/every row already matching
    /// this id in `messages`; the rest of the conversation picks up
    /// `thread_muted` on the next list reload (mirrors how a mute made
    /// elsewhere already only becomes visible on reload).
    func toggleMuteSelectedThread() {
        guard let id = selectedMessage?.id ?? currentSelectionIds().first else { return }
        let anchorMuted = selectedMessage?.threadMuted
            ?? messages.first { $0.id == id }?.threadMuted
            ?? false
        let muted = !anchorMuted
        if let idx = messages.firstIndex(where: { $0.id == id }) {
            messages[idx].threadMuted = muted
        }
        if selectedMessage?.id == id { selectedMessage?.threadMuted = muted }
        let core = self.core
        Task.detached {
            do {
                try core.setThreadMuted(messageId: id, muted: muted)
                await MainActor.run { self.refreshSmartMailboxCountsIfAny() }
            } catch {
                await MainActor.run {
                    if let idx = self.messages.firstIndex(where: { $0.id == id }) {
                        self.messages[idx].threadMuted = !muted
                    }
                    if self.selectedMessage?.id == id { self.selectedMessage?.threadMuted = !muted }
                    self.errorText = String(describing: error)
                }
            }
        }
    }

    /// Ids to act on: the multi-selection if any, else the primary selection.
    func currentSelectionIds() -> [String] {
        if !selectedMessageIds.isEmpty { return selectedMessageIds }
        if let s = selectedMessage { return [s.id] }
        return []
    }

    /// Mark every message in the current mailbox read (one bulk op).
    func markAllReadInCurrentMailbox() {
        // Not in Correspondence: `selectedMailbox` is hidden context there, and
        // the core would mark the WHOLE hidden mailbox while the UI flipped only
        // the visible rows.
        // FP18: while a search is active `messages` holds results, but the core
        // would mark the WHOLE mailbox read. The menu item is disabled then too.
        guard searchText.isEmpty else { return }
        // A split tab shows only part of the Inbox, but the core would mark all of it read
        // (and a tab loads at most 500 rows, so marking "its" ids would be wrong too) (#3459).
        if isInboxSplitStripVisible {
            errorText = String(localized: "Mark All as Read is not available in an Inbox tab. Select the messages to mark instead.")
            return
        }
        guard let mb = actionMailbox else {
            // All Inboxes / a smart mailbox has no single mailbox to act on:
            // say so instead of a dead command (Correspondence stays silent).
            if correspondence == nil, isUnifiedList || selectedSmartMailboxId != nil {
                errorText = String(localized: "Mark All as Read needs a single mailbox. Select a mailbox first.")
            }
            return
        }
        // Remember what we optimistically flipped so a refusal can undo exactly
        // those rows (a read-only account must not look as if it changed).
        let flipped = messages.filter { !$0.isRead }.map(\.id)
        for i in messages.indices where !messages[i].isRead { messages[i].isRead = true }
        if selectedMessage?.isRead == false { selectedMessage?.isRead = true }
        let core = self.core
        let (acct, mbox) = (mb.accountId, mb.id)
        Task.detached {
            var firstError: String?
            var isEvaluation = false
            do { try core.markAllRead(accountId: acct, mailboxId: mbox) } catch {
                firstError = String(describing: error)
                isEvaluation = AccountWritePolicy.isEvaluationModeRefusal(error)
            }
            let (reason, wasEvaluation) = (firstError, isEvaluation)
            await MainActor.run {
                if let reason {
                    for id in flipped { self.applyReadState(messageId: id, isRead: false) }
                    self.reportWriteFailure(
                        verb: "Mark All Read", accountId: acct,
                        isEvaluationMode: wasEvaluation, reason: reason)
                }
                self.reloadCountsAfterMutation(accounts: [acct])
                self.refreshPendingCount()
                self.scheduleReplay()
            }
        }
    }

    /// After a LOCAL mutation (read/unread, mark all): refresh the per-account
    /// counts and, when smart mailboxes exist, their cached counts too, so the
    /// sidebar badge and the list agree without waiting for the next sync (FP18).
    func reloadCountsAfterMutation(accounts accts: some Sequence<String>) {
        for a in accts { reloadCounts(a) }
        if !smartMailboxes.isEmpty { refreshSmartMailboxCounts() }
        if !tags.isEmpty { reloadTagCounts() }
    }

    /// Open a message in its own window (double-click).
    func openStandaloneWindow(for message: MessageHeader) {
        let id = message.id
        let title = message.subject ?? "(no subject)"
        load({ try $0.getMessage(id: id) }) { detail in
            guard let detail else { return }
            let state = self.beginStandaloneMessage(detail: detail, markReadAfterBody: message)
            StandaloneMessageWindow.show(state: state, title: title)
        }
    }

    /// State for a standalone message window. A descriptor-only message (`hasBody == false`)
    /// fetches its body here, independently of the main pane's selection, and the window
    /// updates when it lands (or offers Retry). The deliberate open counts as reading it
    /// (unless auto-mark is off) but only once a body is actually available, never on a
    /// failed fetch (#3441). Still the automatic path: the user asked to READ the message.
    func beginStandaloneMessage(
        detail: MessageDetail, markReadAfterBody header: MessageHeader?
    ) -> StandaloneMessageState {
        let state = StandaloneMessageState(detail: detail)
        state.inlineImages = { [weak self] in self?.inlineImageSource(for: $0) }
        let markRead: @MainActor () -> Void = { [weak self, weak state] in
            guard let self, let header, state?.isClosed != true, !header.isRead,
                  Preferences.shared.autoMarkReadMode != .never else { return }
            self.markRead(message: header, isRead: true, automatic: true)
        }
        if detail.header.hasBody == false {
            hydrateStandaloneBody(state, onBody: markRead)
        } else {
            markRead()
        }
        return state
    }

    private func hydrateStandaloneBody(
        _ state: StandaloneMessageState, onBody: @escaping @MainActor () -> Void
    ) {
        let header = state.detail.header
        let retry: () -> Void = { [weak self, weak state] in
            guard let self, let state else { return }
            self.hydrateStandaloneBody(state, onBody: onBody)
        }
        guard let configJSON = storedConfig(for: header.accountId) else {
            state.isHydrating = false
            state.error = "Stored credentials are unavailable for this account."
            state.retry = retry
            return
        }
        state.isHydrating = true
        state.error = nil
        state.retry = nil
        let core = self.core
        let messageId = header.id
        Task.detached { [weak state] in
            do {
                let hydrated = try core.hydrateMessageBody(configJSON: configJSON, messageId: messageId)
                await MainActor.run {
                    guard let state, !state.isClosed else { return }
                    state.isHydrating = false
                    guard let hydrated else {
                        state.error = "Message is no longer available locally."
                        return
                    }
                    state.detail = hydrated
                    self.applyHydratedHeader(hydrated.header)
                    onBody()
                }
            } catch {
                await MainActor.run {
                    guard let state, !state.isClosed else { return }
                    state.isHydrating = false
                    // Never the raw thrown text: it carries OS errors and server wording.
                    state.error = AccountSyncStatus.bodyFetchMessage(
                        target: AccountSyncTarget(configJSON: configJSON),
                        thrown: String(describing: error))
                    state.retry = retry
                }
            }
        }
    }

    // MARK: - per-account write policy (evaluation mode)

    /// This account's write state, failing closed while unprobed.
    func writeState(for accountId: String) -> AccountWriteState {
        AccountWritePolicy.state(for: accountId, in: accountWriteEnabled)
    }

    /// Re-probe every account's write policy. The core is the only source of
    /// truth for this — the app never caches a value it did not read back.
    func refreshWriteEnabled() {
        let ids = accounts.map(\.id)
        let core = self.core
        let generation = writePolicyGeneration
        Task.detached {
            var probed: [String: Bool] = [:]
            for id in ids {
                // A failed probe is read-only: fail closed, never "assume yes".
                probed[id] = (try? core.accountWriteEnabled(accountId: id)) ?? false
            }
            let result = probed
            await MainActor.run {
                // Superseded by an explicit flip while we were probing: drop it
                // rather than resurrect the pre-flip value.
                guard generation == self.writePolicyGeneration else { return }
                // Merge per key rather than replacing the dictionary: `ids` is a
                // snapshot, and an account added since must not be erased.
                for (id, enabled) in result { self.accountWriteEnabled[id] = enabled }
            }
        }
    }

    /// Flip an account's write policy. Only ever called after the user confirms
    /// the dialog — leaving evaluation mode must be an explicit human act.
    /// The new value is READ BACK from the core rather than assumed, so a
    /// refused or partial write can never leave the indicator lying.
    func setWriteEnabled(accountId: String, enabled: Bool) {
        setWriteEnabled(accountIds: [accountId], enabled: enabled)
    }

    /// Flip the write policy of exactly `accountIds` (one account, or the
    /// snapshot an all-accounts confirmation listed). `completion` receives
    /// true only when every account READ BACK from the core in the requested
    /// state; it is not called when another change is already in flight.
    func setWriteEnabled(
        accountIds: [String], enabled: Bool,
        completion: (@MainActor (Bool) -> Void)? = nil
    ) {
        guard !isChangingWritePolicy, !accountIds.isEmpty else { return }
        isChangingWritePolicy = true
        writePolicyGeneration &+= 1
        writeSafetyPrompt = nil
        let core = self.core
        Task.detached {
            var failure: String?
            var readBack: [String: Bool] = [:]
            for accountId in accountIds {
                do {
                    try core.setAccountWriteEnabled(accountId: accountId, enabled: enabled)
                } catch {
                    if failure == nil { failure = String(describing: error) }
                }
                // Fail closed: an unreadable policy is read-only.
                readBack[accountId] = (try? core.accountWriteEnabled(accountId: accountId)) ?? false
            }
            let actual = readBack
            let reported = failure
            await MainActor.run {
                self.writePolicyGeneration &+= 1
                for (accountId, value) in actual { self.accountWriteEnabled[accountId] = value }
                self.isChangingWritePolicy = false
                if let reported {
                    self.errorText = "Could not change the write policy: \(reported)"
                }
                if actual.values.contains(true) {
                    // Writes just became live — drain anything queued while the
                    // account was read-only.
                    self.replayNow()
                }
                completion?(reported == nil && actual.values.allSatisfy { $0 == enabled })
            }
        }
    }

    /// Ask for confirmation before flipping an account's write policy. The
    /// sidebar badge and the Settings → Accounts row call this instead of
    /// presenting a dialog themselves.
    func requestWritePolicyChange(for account: Account) {
        guard !isChangingWritePolicy else { return }
        writeSafetyRetry = nil
        writeSafetyPrompt = writeState(for: account.id).isReadOnly
            ? .confirmEnable(accountId: account.id, email: account.email)
            : .confirmDisable(accountId: account.id, email: account.email)
    }

    /// Accounts still in evaluation mode (unprobed ones included: fail closed).
    var accountsReadOnly: [Account] {
        accounts.filter { writeState(for: $0.id).isReadOnly }
    }

    /// Ask to leave evaluation mode for every account that is still read-only
    /// (Settings → Accounts; WS1, DECISIONS.md 2026-10-05). The confirmation
    /// names a SNAPSHOT of those accounts and only they are flipped: nothing
    /// is stored, so an account added later starts read-only like any other.
    func requestEnableWritesForAllAccounts() {
        guard !isChangingWritePolicy else { return }
        let targets = accountsReadOnly
        guard !targets.isEmpty else { return }
        writeSafetyRetry = nil
        writeSafetyPrompt = .confirmEnableAll(
            accounts: targets.map { WriteSafetyAccountRef(id: $0.id, email: $0.email) })
    }

    /// Report a failed write. An evaluation-mode refusal is the safety policy
    /// working as designed, not a fault, so it gets its own dialog carrying the
    /// fix (enable writes) instead of a dead-end error string.
    func reportWriteFailure(
        verb: String, accountId: String?, isEvaluationMode: Bool, reason: String
    ) {
        guard isEvaluationMode, let accountId,
              let email = accounts.first(where: { $0.id == accountId })?.email
        else {
            errorText = "\(verb) failed: \(reason)"
            return
        }
        // The core just authoritatively told us this account is read-only.
        accountWriteEnabled[accountId] = false
        writeSafetyRetry = nil
        writeSafetyPrompt = .blocked(accountId: accountId, email: email, verb: verb)
    }

    /// Offer a way to repeat the action behind the pending refusal once writes
    /// are enabled (a compose window re-runs its Send). Ignored unless the
    /// pending prompt is a refusal for `accountId`, so a late registration can
    /// never attach to some other dialog.
    func registerWriteSafetyRetry(accountId: String, _ retry: @escaping @MainActor () -> Void) {
        guard case .blocked(let pending, _, _)? = writeSafetyPrompt, pending == accountId else { return }
        writeSafetyRetry = retry
    }

    /// True when the pending refusal will be retried automatically.
    var writeSafetyWillRetry: Bool { writeSafetyRetry != nil }

    /// Carry out the user's answer to the pending write-safety dialog. This is
    /// the ONLY path out of evaluation mode: `.enable` arrives here solely from
    /// a button the user clicked in that dialog (`WriteSafetyPresenter`).
    ///
    /// After enabling from a refusal, the refused action is retried when its
    /// window registered a retry; otherwise the user is told to repeat it.
    func resolveWriteSafetyPrompt(_ action: WriteSafetyAlertSpec.Action) {
        let prompt = writeSafetyPrompt
        let retry = writeSafetyRetry
        writeSafetyRetry = nil
        writeSafetyPrompt = nil
        switch action {
        case .dismiss:
            break
        case .disable(let accountId):
            setWriteEnabled(accountIds: [accountId], enabled: false)
        case .enable(let accountIds):
            setWriteEnabled(accountIds: accountIds, enabled: true) { [weak self] allEnabled in
                guard let self, allEnabled,
                      case .blocked(let accountId, let email, let verb)? = prompt else { return }
                if let retry {
                    retry()
                } else {
                    self.writeSafetyPrompt = .retryNotice(
                        accountId: accountId, email: email, verb: verb)
                }
            }
        }
    }

    // MARK: - destructive actions (archive / delete / empty trash)

    /// Archive the selection (move to the account's Archive mailbox).
    func archiveSelected() {
        moveRemoving(ids: Self.idsNotAlreadyArchived(
            currentSelectionIds(), rows: messages, mailboxes: mailboxes,
            fallbackRole: selectedMailbox?.role), target: .archive, verb: .archive)
    }

    /// The ids whose mailbox is not already an Archive. The core treats archiving such a message
    /// as a no-op and keeps the row, so the optimistic removal must not drop it either (#3459).
    /// The role comes from each row's OWN mailbox (search and smart-mailbox lists mix roles).
    nonisolated static func idsNotAlreadyArchived(
        _ ids: [String], rows: [MessageHeader], mailboxes: [String: [Mailbox]], fallbackRole: String?
    ) -> [String] {
        let byId = Dictionary(rows.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        return ids.filter { id in
            guard let row = byId[id] else { return true }
            let role = (mailboxes[row.accountId] ?? []).first { $0.id == row.mailboxId }?.role ?? fallbackRole
            return role != "archive"
        }
    }

    /// Move the selection to `mailboxId` (Move to Folder, #3022 S4). Same
    /// undoable batch path as Archive and Delete. Does nothing for a selection
    /// that spans accounts, or whose messages are all in `mailboxId` already.
    func moveSelected(toMailboxId mailboxId: String) {
        let ids = currentSelectionIds()
        guard case .ok(let accountId) = MoveToFolderPlan.eligibility(ids: ids, rows: messages),
              (mailboxes[accountId] ?? []).contains(where: { $0.id == mailboxId })
        else { return }
        moveRemoving(
            ids: MoveToFolderPlan.idsToMove(ids: ids, rows: messages, destinationId: mailboxId),
            target: .mailbox(id: mailboxId), verb: .move,
            // Recents change only once the core actually queued a move (never on refusal).
            onMoved: { Preferences.shared.noteMoveDestination(mailboxId, accountId: accountId) })
    }

    /// Mark as Junk (`isJunk`) or Not Junk for the selection: a move to the
    /// account's Junk-role mailbox, or from Junk back to the Inbox, through the
    /// same undoable, write-guarded batch as Archive / Move to Folder. Castellum
    /// never creates a Junk folder and writes no `$Junk` keywords (the existing
    /// code does not model them): an account without one is reported, nothing queued.
    func setJunkOnSelection(_ isJunk: Bool) {
        let ids = Set(currentSelectionIds())
        let plan = Self.junkPlan(
            isJunk: isJunk, rows: messages.filter { ids.contains($0.id) }, mailboxes: mailboxes)
        if let accountId = plan.accountsWithoutJunk.first {
            reportWriteFailure(
                verb: JunkVerbName.name(isJunk), accountId: accountId, isEvaluationMode: false,
                reason: String(localized: "This account has no Junk folder."))
        }
        if plan.batches.isEmpty {
            // Palette / menu-bar entry points are enabled on any selection: say why nothing happened.
            if plan.accountsWithoutJunk.isEmpty {
                errorText = isJunk
                    ? String(localized: "Mark as Junk: the selected messages are already in Junk.")
                    : String(localized: "Not Junk: none of the selected messages are in Junk.")
            }
            return
        }
        // ONE removal for the whole selection (one undo entry / banner, one list restore),
        // even when it spans accounts and therefore several destination mailboxes.
        moveRemoving(
            batches: plan.batches.map { ($0.ids, .mailbox(id: $0.destinationId)) },
            verb: isJunk ? .junk : .notJunk)
    }

    /// What `setJunkOnSelection` will move, per account (pure, unit-tested).
    struct JunkPlan: Equatable {
        struct Batch: Equatable { let ids: [String]; let destinationId: String }
        var batches: [Batch] = []
        var accountsWithoutJunk: [String] = []
    }

    nonisolated static func junkPlan(
        isJunk: Bool, rows: [MessageHeader], mailboxes: [String: [Mailbox]]
    ) -> JunkPlan {
        var plan = JunkPlan()
        var order: [String] = []
        var byAccount: [String: [MessageHeader]] = [:]
        for row in rows {
            if byAccount[row.accountId] == nil { order.append(row.accountId) }
            byAccount[row.accountId, default: []].append(row)
        }
        for account in order {
            let boxes = mailboxes[account] ?? []
            let junkId = boxes.first { $0.role == "junk" }?.id
            let destination = isJunk ? junkId : boxes.first { $0.role == "inbox" }?.id
            guard let destination else {
                if isJunk { plan.accountsWithoutJunk.append(account) }
                continue
            }
            // Decided by each row's own mailbox ROLE (the menu does the same), not by one junk id.
            let inJunk: (MessageHeader) -> Bool = { row in
                boxes.first { $0.id == row.mailboxId }?.role == "junk"
            }
            // Mark as Junk skips what is already in Junk; Not Junk only takes what is.
            let ids = (byAccount[account] ?? [])
                .filter { isJunk ? !inJunk($0) : inJunk($0) }.map(\.id)
            if !ids.isEmpty { plan.batches.append(.init(ids: ids, destinationId: destination)) }
        }
        return plan
    }

    /// Delete the selection. Outside Trash: move to the account's Trash
    /// mailbox (undoable). Inside Trash: DESTRUCTIVE, so this only raises
    /// `pendingPermanentDelete` for the confirmation dialog; nothing is
    /// queued until `confirmPermanentDelete()`.
    func deleteSelected() {
        if selectionDeleteDisposition() == .confirmPermanent {
            let ids = currentSelectionIds()
            guard !ids.isEmpty else { return }
            pendingPermanentDelete = PermanentDeleteRequest(ids: ids)
            return
        }
        moveRemoving(ids: currentSelectionIds(), target: .trash, verb: .delete)
    }

    /// The user confirmed the "cannot be undone" dialog: queue a permanent
    /// delete per message through the core's operation queue. Unlike archive/
    /// delete this is NOT optimistic — rows stay in the list until the server
    /// has expunged them (the core drops the local row on confirmation and the
    /// post-replay re-sync refreshes the list), so a failed expunge leaves the
    /// message visible alongside the error.
    func confirmPermanentDelete() {
        guard let request = pendingPermanentDelete else { return }
        pendingPermanentDelete = nil
        let accountForId = Dictionary(
            messages.compactMap { m in request.ids.contains(m.id) ? (m.id, m.accountId) : nil },
            uniquingKeysWith: { first, _ in first })
        let core = self.core
        Task.detached {
            var firstError: String?
            var firstWasEvaluationMode = false
            var firstFailedAccount: String?
            for id in request.ids {
                do { try core.deleteMessagePermanently(messageId: id) } catch {
                    if firstError == nil {
                        firstError = String(describing: error)
                        firstWasEvaluationMode = AccountWritePolicy.isEvaluationModeRefusal(error)
                        firstFailedAccount = accountForId[id]
                    }
                }
            }
            let (reason, isEvaluation, failedAccount) =
                (firstError, firstWasEvaluationMode, firstFailedAccount)
            await MainActor.run {
                if let reason {
                    self.reportWriteFailure(
                        verb: String(localized: "Delete permanently"), accountId: failedAccount,
                        isEvaluationMode: isEvaluation, reason: reason)
                }
                self.refreshPendingCount()
                self.scheduleReplay()
            }
        }
    }

    /// The user dismissed the confirmation: nothing was queued.
    func cancelPermanentDelete() {
        pendingPermanentDelete = nil
    }

    /// Snooze the current selection until `untilEpoch` (Unix epoch seconds).
    /// Optimistically removed from whichever list is showing (mailbox or
    /// unified); restored on a refused write, exactly like archive/delete.
    func snoozeSelected(until untilEpoch: Int64) {
        // `then:` rather than a call after `mutateRemoving` returns: the write
        // is detached, so an immediate reload races it and the Snoozed badge
        // can under-count the message just snoozed until the next sweep tick.
        mutateRemoving(ids: currentSelectionIds(), verb: "Snooze", then: { [weak self] in
            self?.reloadSnoozedMessages()
        }) { core, id in
            try core.snoozeMessage(messageId: id, untilEpoch: untilEpoch)
        }
    }

    /// Snooze the current selection until a preset's due date (row menu).
    func snoozeSelected(preset: SnoozePreset) {
        snoozeSelected(until: Int64(preset.dueDate().timeIntervalSince1970))
    }

    /// Un-snooze the current selection now ("back to inbox"), from the
    /// Snoozed list. Removes the row from `snoozedMessages` (and, since the
    /// Snoozed list is the only view that can select these ids, from the
    /// on-screen `messages` too) immediately, restoring both on a refused
    /// write (mirrors `mutateRemoving`).
    func unsnoozeSelected() {
        let ids = currentSelectionIds()
        guard !ids.isEmpty else { return }
        snoozedMessages.removeAll { ids.contains($0.id) }
        if selectedIsSnoozed {
            messages.removeAll { ids.contains($0.id) }
        }
        let core = self.core
        Task.detached {
            var firstError: String?
            for id in ids {
                do { _ = try core.unsnoozeMessage(id: id) } catch {
                    if firstError == nil { firstError = String(describing: error) }
                }
            }
            await MainActor.run {
                if let firstError {
                    self.errorText = firstError
                    self.reloadSnoozedMessages()
                }
                self.reloadCurrentMessages()
            }
        }
    }

    /// Permanently empty the current account's Trash (destructive; confirmed in UI).
    func emptyTrash() {
        // Never guess an account: in a unified list (no selected mailbox) there is
        // no single Trash to empty, and falling back to accounts.first would
        // silently erase an unrelated account's Trash.
        guard let acct = emptyTrashAccountId else {
            // Always say why nothing happened (FP22): a silent no-op after
            // the user asked for an irreversible action reads as a hang.
            errorText = isUnifiedList
                ? "Empty Trash needs a single account. Select a mailbox first."
                : String(localized: "There is no Trash to empty. Select a mailbox first.")
            return
        }
        let core = self.core
        Task.detached {
            do { try core.emptyTrash(accountId: acct) } catch {
                let isEvaluation = AccountWritePolicy.isEvaluationModeRefusal(error)
                let reason = String(describing: error)
                await MainActor.run {
                    self.reportWriteFailure(
                        verb: "Empty Trash", accountId: acct,
                        isEvaluationMode: isEvaluation, reason: reason)
                }
                return
            }
            await MainActor.run {
                self.reloadCounts(acct)
                self.refreshPendingCount()
                self.scheduleReplay()
                self.restoreCurrentList()
            }
        }
    }

    /// The account whose Trash "Empty Trash" targets: the selected mailbox's
    /// account only. Nil in a unified list (or with nothing selected).
    /// Nil while the list is a Correspondence view too: the selected mailbox is
    /// hidden context there, and Empty Trash must never act on something the
    /// user cannot see.
    var emptyTrashAccountId: String? { actionMailbox?.accountId }

    /// Title of the permanent-erase confirmation: names the account whose
    /// Trash will be emptied (FP22), so an irreversible erase never has an
    /// unstated target. Generic only if the account cannot be resolved.
    var emptyTrashConfirmationTitle: String {
        if let id = emptyTrashAccountId,
           let email = accounts.first(where: { $0.id == id })?.email {
            return String(localized: "Permanently erase all messages in the Trash of \(email)? This can’t be undone.")
        }
        return String(localized: "Permanently erase all messages in Trash? This can’t be undone.")
    }

    /// The mailbox that mailbox-wide actions (Mark All Read, Empty Trash, .eml
    /// import) may act on: the selected mailbox, but never while the list is a
    /// Correspondence view, where `selectedMailbox` is hidden context the user
    /// is not looking at.
    var actionMailbox: Mailbox? { correspondence == nil ? selectedMailbox : nil }

    /// Permanent-delete confirmation vs move-to-Trash for the CURRENT selection.
    /// Decided from each selected row's OWN mailbox role (never the sidebar
    /// mailbox, which is not what a search, unified or Correspondence list
    /// shows): the confirmation only when every selected row sits in a Trash
    /// mailbox. A row whose mailbox cannot be resolved falls back to the
    /// selected mailbox's role. The core independently refuses a permanent
    /// delete outside Trash, so this is UI gating, not the safety net.
    private func selectionDeleteDisposition() -> DeleteDisposition {
        let ids = Set(currentSelectionIds())
        let rows = messages.filter { ids.contains($0.id) }
        let roles: [String?] = rows.map { row in
            (mailboxes[row.accountId] ?? []).first { $0.id == row.mailboxId }?.role
                ?? selectedMailbox?.role
        }
        if roles.isEmpty { return DeleteDisposition.forMailboxRole(correspondence == nil ? selectedMailbox?.role : nil) }
        return DeleteDisposition.forRowMailboxRoles(roles)
    }

    /// Reload whichever list is showing (mailbox, unified, or Snoozed) so
    /// optimistically removed rows reappear after a refused write.
    private func restoreCurrentList() {
        if let context = correspondence {
            loadCorrespondence(context)
        } else if selectedIsSnoozed {
            reloadSnoozedMessages()
        } else if let id = selectedSmartMailboxId, let sm = smartMailboxes.first(where: { $0.id == id }) {
            loadSmartMailboxMessages(sm) // a refused archive/delete must bring the rows back (#3441)
        } else if isUnifiedList || selectedTagKey != nil {
            reloadCurrentMessages()
        } else if let mb = selectedMailbox {
            select(mailbox: mb)
        }
    }

    // MARK: - undoable removals (archive / delete / move)

    /// Archive, trash or move `ids` with ONE core call, removing the rows
    /// optimistically, and make the action undoable. Same optimistic-removal
    /// and read-only-skip rules as `mutateRemoving`: an id on
    /// an account already known to be read-only is not removed up front. On
    /// success the operation ids the core returns become an
    /// `UndoableRemoval` registered with the window's `UndoManager`.
    private func moveRemoving(
        ids: [String], target: MoveTarget, verb: RemovalVerb, onMoved: (@MainActor () -> Void)? = nil
    ) {
        moveRemoving(batches: [(ids, target)], verb: verb, onMoved: onMoved)
    }

    /// One removal made of several (ids, target) batches run in order in one Task: the
    /// outcomes merge into a single result, so the list restore, the refusal notice and
    /// the undo entry (all operation ids) happen exactly once. A batch the core rejects
    /// outright turns into per-message refusals.
    private func moveRemoving(
        batches: [(ids: [String], target: MoveTarget)], verb: RemovalVerb,
        onMoved: (@MainActor () -> Void)? = nil
    ) {
        let ids = batches.flatMap(\.ids)
        guard !ids.isEmpty else { return }
        let accountForId = Dictionary(
            messages.compactMap { m in ids.contains(m.id) ? (m.id, m.accountId) : nil },
            uniquingKeysWith: { first, _ in first })
        let knownReadOnlyIds = Set(ids.filter {
            accountForId[$0].map { writeState(for: $0).isReadOnly } ?? false
        })
        let removedUpFront = Set(ids.filter { !knownReadOnlyIds.contains($0) })
        for id in removedUpFront { removeFromList(id) }
        let core = self.core
        Task.detached {
            var operationIds: [Int64] = []
            var refused: [MoveRefusal] = []
            for batch in batches where !batch.ids.isEmpty {
                do {
                    let r = try core.moveMessages(ids: batch.ids, target: batch.target)
                    operationIds += r.operationIds
                    refused += r.refused
                } catch {
                    let eval = AccountWritePolicy.isEvaluationModeRefusal(error)
                    refused += batch.ids.map {
                        MoveRefusal(messageId: $0, reason: String(describing: error), evaluationMode: eval)
                    }
                }
            }
            let outcome: Result<MoveBatchResult, Error> =
                .success(MoveBatchResult(operationIds: operationIds, refused: refused))
            await MainActor.run {
                self.finishMoveRemoving(
                    outcome, ids: ids, accountForId: accountForId,
                    removedUpFront: removedUpFront, verb: verb, onMoved: onMoved)
            }
        }
    }

    /// Main-actor tail of `moveRemoving`: report refusals, restore the list if
    /// a row that was removed up front was refused, record the undo entry.
    private func finishMoveRemoving(
        _ outcome: Result<MoveBatchResult, Error>, ids: [String],
        accountForId: [String: String], removedUpFront: Set<String>, verb: RemovalVerb,
        onMoved: (@MainActor () -> Void)? = nil
    ) {
        let accounts = Set(accountForId.values)
        switch outcome {
        case .failure(let error):
            reportWriteFailure(
                verb: verb.actionName, accountId: accountForId[ids[0]],
                isEvaluationMode: AccountWritePolicy.isEvaluationModeRefusal(error),
                reason: String(describing: error))
            restoreCurrentList()
        case .success(let result):
            let refusedIds = Set(result.refused.map(\.messageId))
            if let first = result.refused.first {
                reportWriteFailure(
                    verb: verb.actionName, accountId: accountForId[first.messageId],
                    isEvaluationMode: first.evaluationMode, reason: first.reason)
                if !refusedIds.isDisjoint(with: removedUpFront) { restoreCurrentList() }
            }
            // A known-read-only row whose write unexpectedly went through (stale
            // cache): it was never removed up front, so remove it now.
            for id in ids where !refusedIds.contains(id) && !removedUpFront.contains(id) {
                removeFromList(id)
            }
            if !result.operationIds.isEmpty {
                onMoved?()
                let moved = ids.filter { !refusedIds.contains($0) }
                recordRemovalUndo(UndoableRemoval(
                    verb: verb, operationIds: result.operationIds, count: moved.count,
                    accountIds: Set(moved.compactMap { accountForId[$0] })))
            }
        }
        for a in accounts { reloadCounts(a) }
        refreshPendingCount()
        scheduleReplay()
    }

    /// Push `entry` on the session stack and register it with the window's
    /// `UndoManager` so Edit > Undo <verb> and Cmd-Z reach `undoRemoval`.
    private func recordRemovalUndo(_ entry: UndoableRemoval) {
        let undoManager = removalUndoManager
        for evicted in removalUndoStack.push(entry) {
            undoManager?.removeAllActions(withTarget: evicted.token)
        }
        undoManager?.registerUndo(withTarget: entry.token) { [weak self] _ in
            MainActor.assumeIsolated { self?.undoRemoval(id: entry.id) }
        }
        undoManager?.setActionName(entry.verb.actionName)
        removalUndoBanner.offer(
            entryId: entry.id, verb: entry.verb, count: entry.count, accountIds: entry.accountIds)
        startRemovalBannerTicker()
    }

    /// One-second ticker for `removalUndoBanner`; stops itself once the banner hides.
    private func startRemovalBannerTicker() {
        removalBannerTickTask?.cancel()
        removalBannerTickTask = Task { [weak self] in
            while let self, !Task.isCancelled, self.removalUndoBanner.isVisible {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                if Task.isCancelled { return }
                self.removalUndoBanner.tick()
            }
        }
    }

    /// Banner Undo button: same path as Cmd-Z and `z`.
    func undoRemovalFromBanner() {
        guard let shown = removalUndoBanner.shown else { return }
        undoRemoval(id: shown.entryId)
    }

    /// Undo the most recent archive / delete / move (the bare `z` key and the
    /// "Undo Archive or Delete" command). No-op when nothing is undoable.
    func undoLastRemoval() {
        guard let top = removalUndoStack.top else { return }
        undoRemoval(id: top.id)
    }

    /// Undo one stack entry: ask the core to queue the inverse moves, then
    /// replay now so the message comes back after one server round trip (it
    /// returns as a NEW local row once the post-replay sync sees it). Safe to
    /// call twice for one entry (Cmd-Z racing the `z` key): the second is a
    /// no-op because the entry is gone.
    func undoRemoval(id: UUID) {
        guard let entry = removalUndoStack.remove(id: id) else { return }
        removalUndoManager?.removeAllActions(withTarget: entry.token)
        removalUndoBanner.undoRequested(entryId: entry.id, destinationName: nil)
        let core = self.core
        let singleAccount = entry.accountIds.count == 1 ? entry.accountIds.first : nil
        Task.detached {
            let outcome = Result { try core.undoMoves(operationIds: entry.operationIds) }
            await MainActor.run {
                switch outcome {
                case .failure(let error):
                    self.removalUndoBanner.clear(entryId: entry.id)
                    self.reportWriteFailure(
                        verb: "Undo", accountId: singleAccount,
                        isEvaluationMode: AccountWritePolicy.isEvaluationModeRefusal(error),
                        reason: String(describing: error))
                case .success(let result):
                    if let refusal = result.notUndoable.first {
                        self.reportWriteFailure(
                            verb: "Undo", accountId: singleAccount,
                            isEvaluationMode: refusal.evaluationMode, reason: refusal.reason)
                    }
                    guard result.queued > 0 else {
                        self.removalUndoBanner.clear(entryId: entry.id)
                        return
                    }
                    self.nameReturningDestination(entry: entry, mailboxIds: result.sourceMailboxIds)
                    self.restoreCurrentList() // a Trash/Archive copy synced early leaves at once
                    for a in result.accountIds { self.reloadCounts(a) }
                    self.refreshPendingCount()
                    self.replayNow()
                }
            }
        }
    }

    /// Fill in the folder name of "Returning to Inbox…" once the core says where
    /// the mail goes back to (one folder -> its name, several -> generic).
    private func nameReturningDestination(entry: UndoableRemoval, mailboxIds: [String]) {
        let all = mailboxes.values.flatMap { $0 }
        let names = Set(mailboxIds.compactMap { id in all.first { $0.id == id }?.name })
        removalUndoBanner.undoRequested(
            entryId: entry.id, destinationName: names.count == 1 ? names.first : nil)
    }

    /// Optimistically remove the given messages from the list, run `op` per id,
    /// and if any fail, restore the mailbox view (they reappear) with the error.
    ///
    ///: an id whose account `writeState(for:)` already reports
    /// `.readOnly` is skipped by the optimistic removal below — the write is
    /// certain to be refused (evaluation mode), so removing the row just to
    /// restore it a moment later on the refusal produced a visible flash. Not
    /// removing it in the first place needs no restore at all. This still
    /// calls `op` for those ids (the core, not this cache, is the safety
    /// boundary — see `AccountWritePolicy`), so a stale cache self-corrects:
    /// an unexpected success removes the row then, and an expected refusal
    /// leaves the untouched row exactly where it was.
    private func mutateRemoving(
        ids: [String], verb: String,
        then: (@MainActor () -> Void)? = nil,
        _ op: @escaping @Sendable (CastellumCore, String) throws -> Void
    ) {
        guard !ids.isEmpty else { return }
        let accountForId = Dictionary(
            messages.compactMap { m in ids.contains(m.id) ? (m.id, m.accountId) : nil },
            uniquingKeysWith: { first, _ in first })
        let accts = Set(accountForId.values)
        let knownReadOnlyIds = Set(ids.filter {
            accountForId[$0].map { writeState(for: $0).isReadOnly } ?? false
        })
        for id in ids where !knownReadOnlyIds.contains(id) { removeFromList(id) }
        let core = self.core
        Task.detached {
            var firstError: String?
            var firstWasEvaluationMode = false
            var firstFailedAccount: String?
            var removedIdFailed = false
            var readOnlyIdSucceeded: [String] = []
            for id in ids {
                do {
                    try op(core, id)
                    if knownReadOnlyIds.contains(id) { readOnlyIdSucceeded.append(id) }
                } catch {
                    if !knownReadOnlyIds.contains(id) { removedIdFailed = true }
                    if firstError == nil {
                        firstError = String(describing: error)
                        firstWasEvaluationMode = AccountWritePolicy.isEvaluationModeRefusal(error)
                        firstFailedAccount = accountForId[id]
                    }
                }
            }
            let (reason, isEvaluation, failedAccount) =
                (firstError, firstWasEvaluationMode, firstFailedAccount)
            let restoreNeeded = removedIdFailed
            await MainActor.run {
                // A known-read-only account whose write unexpectedly went
                // through (stale cache, e.g. writes were just re-enabled
                // elsewhere): the row was never optimistically removed, so
                // remove it now that the server change is confirmed.
                for id in readOnlyIdSucceeded { self.removeFromList(id) }
                if let reason {
                    self.reportWriteFailure(
                        verb: verb, accountId: failedAccount,
                        isEvaluationMode: isEvaluation, reason: reason)
                    // Only restore when something was actually removed
                    // optimistically and then failed — a known-read-only id
                    // was never touched, so there is nothing to flicker back.
                    if restoreNeeded { self.restoreCurrentList() } // restore (mailbox or unified)
                }
                // Archive, delete, move, snooze: smart mailbox badges too (#3455).
                self.reloadCountsAfterMutation(accounts: accts)
                self.refreshPendingCount()
                self.scheduleReplay()
                then?()
            }
        }
    }

    private func removeFromList(_ id: String) {
        messages.removeAll { $0.id == id }
        selectedMessageIds.removeAll { $0 == id }
        if selectedMessage?.id == id {
            selectedMessage = nil
            detail = nil
        }
    }

    /// Flip read state in the loaded list + selection without a round-trip.
    private func applyReadState(messageId: String, isRead: Bool) {
        if let idx = messages.firstIndex(where: { $0.id == messageId }) {
            messages[idx].isRead = isRead
        }
        if selectedMessage?.id == messageId { selectedMessage?.isRead = isRead }
    }

    // MARK: - compose / send

    /// The account a new compose window should default to: the selected
    /// message's account (right in a unified view, where `selectedMailbox`
    /// is nil; harmless in a single-mailbox view since every message there
    /// already shares that mailbox's account), else the selected mailbox's
    /// account, else the first account.
    var composeDefaultAccountId: String? {
        selectedMessage?.accountId ?? selectedMailbox?.accountId ?? accounts.first?.id
    }

    /// `composeDefaultAccountId`, but when it's nil this also decides *why*
    /// and sets `composeUnavailablePrompt` accordingly, so the caller can
    /// bail out with `guard let ... else { return }` exactly like before,
    /// while the user gets an actionable alert instead of a dead end.
    ///
    /// `accounts` is populated only once an account's FIRST sync succeeds
    /// (`reloadAccounts()` reads `core.listAccounts()`, which is empty until
    /// then — see `saveAccountWithoutSync`'s doc comment). `accountSyncStatuses`
    /// is set to `.syncing` synchronously the moment a sync starts, well
    /// before that — so `accounts.isEmpty` with a `.syncing` entry present
    /// means an account genuinely was just added and is still completing its
    /// first sync, not that nothing was ever configured. Confirmed this is
    /// what Marc hit while pilot-testing a first sync of a large real Gmail
    /// mailbox: he had already added the account, so the old "Add an account
    /// before composing." message was actively misleading.
    func composeAccountIdOrPromptUnavailable() -> String? {
        if let accountId = composeDefaultAccountId { return accountId }
        if let syncingAccountId = accountSyncStatuses.first(where: { $0.value.state == .syncing })?.key {
            composeUnavailablePrompt = .firstSyncInProgress(accountId: syncingAccountId)
        } else {
            composeUnavailablePrompt = .noAccount
        }
        return nil
    }

    /// The email address "we" send from on an account (used to drop self from
    /// Reply-All recipients).
    func selfEmail(for accountId: String) -> String {
        accounts.first { $0.id == accountId }?.email ?? accountId
    }

    /// The account's other addresses (identities/aliases), dropped from Reply
    /// and Reply-All recipients along with `selfEmail` (audit #3443). Empty
    /// until the account's identities have been loaded.
    func selfAliases(for accountId: String) -> [String] {
        (identities[accountId] ?? []).map(\.email)
    }

    func reloadDrafts(_ accountId: String) {
        load({ try $0.listDrafts(accountId: accountId) }) { self.drafts[accountId] = $0 }
    }

    /// Identities per account, loaded on demand for the compose From/"Send
    /// As" picker (S47). Keyed by account id; an account never fetched has
    /// no key at all (distinct from "fetched, has none": `[]`).
    @Published var identities: [String: [Identity]] = [:]

    func reloadIdentities(_ accountId: String) {
        load({ try $0.listIdentities(accountId: accountId) }) { self.identities[accountId] = $0 }
    }

    /// Locally-seen correspondents for address autocomplete (S47) — one
    /// broad snapshot (`AddressCompleter` filters/ranks it client-side per
    /// keystroke), not re-queried on every keypress.
    @Published var autocompleteContacts: [Contact] = []
    /// Recipients of mail we sent/queued — known the moment a message goes
    /// out, ahead of the contacts refresh that needs the Sent copy to sync.
    @Published var autocompleteSentRecipients: [SentRecipient] = []

    func reloadAutocompleteContacts() {
        load({ try $0.listContacts(includeHidden: false, limit: 500) }) { self.autocompleteContacts = $0 }
        load({ try $0.listSentRecipients(limit: 500) }) { self.autocompleteSentRecipients = $0 }
    }

    /// Create an identity/alias for `accountId` (Settings > Accounts, S47;
    /// Reply-To / Bcc / default added by MM-6).
    func createIdentity(
        accountId: String, email: String, displayName: String?,
        replyTo: String? = nil, bcc: String? = nil, isDefault: Bool = false,
        completion: @escaping @MainActor (String?) -> Void = { _ in }
    ) {
        let core = self.core
        Task.detached {
            do {
                _ = try core.createIdentity(
                    accountId: accountId, email: email, displayName: displayName,
                    replyTo: replyTo, bcc: bcc, isDefault: isDefault)
                await MainActor.run {
                    self.reloadIdentities(accountId)
                    completion(nil)
                }
            } catch {
                // The sheet shows the reason inline; no global alert on top of it.
                await MainActor.run {
                    completion(Self.identityFailureReason(error, fallback: String(localized: "Could not add that identity — check the email address.")))
                }
            }
        }
    }

    /// Change an identity's details (MM-6). Strings are sent as they are:
    /// empty clears, so the caller passes every field it edits.
    func updateIdentity(
        _ identity: Identity, displayName: String, replyTo: String, bcc: String, isDefault: Bool,
        completion: @escaping @MainActor (String?) -> Void = { _ in }
    ) {
        let core = self.core
        Task.detached {
            do {
                try core.updateIdentity(
                    id: identity.id, displayName: displayName, replyTo: replyTo, bcc: bcc,
                    isDefault: isDefault)
                await MainActor.run {
                    self.reloadIdentities(identity.accountId)
                    completion(nil)
                }
            } catch {
                await MainActor.run {
                    completion(Self.identityFailureReason(error, fallback: String(localized: "Could not save — check the addresses.")))
                }
            }
        }
    }

    /// The core's own reason when it gave one, otherwise `fallback`.
    nonisolated static func identityFailureReason(_ error: Error, fallback: String) -> String {
        if case CastellumError.message(let reason) = error, !reason.isEmpty { return reason }
        return fallback
    }

    /// Delete an identity/alias (Settings > Accounts, S47). A draft that
    /// already chose it keeps its own denormalised From.
    func deleteIdentity(id: Int64, accountId: String) {
        let core = self.core
        Task.detached {
            try? core.deleteIdentity(id: id)
            await MainActor.run { self.reloadIdentities(accountId) }
        }
    }

    /// Save a compose draft (insert or update). On success reports the draft id;
    /// on failure sets `errorText` and reports nil.
    func saveDraft(_ draft: NewDraft, completion: @escaping @MainActor (Int64?) -> Void = { _ in }) {
        let core = self.core
        Task.detached {
            do {
                let id = try core.saveDraft(draft)
                await MainActor.run {
                    self.reloadDrafts(draft.accountId)
                    completion(id)
                }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not save draft: \(error)"
                    completion(nil)
                }
            }
        }
    }

    /// Delete a draft (only while its status is still `draft`).
    func deleteDraft(id: Int64, accountId: String) {
        let core = self.core
        Task.detached {
            try? core.deleteDraft(id: id)
            await MainActor.run { self.reloadDrafts(accountId) }
        }
    }

    /// Outcome of queue-and-deliver for one draft.
    enum ComposeSendOutcome {
        /// Delivered over SMTP.
        case sent
        /// Queued durably with an undo-send window (S25): not yet delivered,
        /// the "Message sent · Undo" banner is now showing. The compose
        /// window closes exactly as it would for `.sent` — from the person's
        /// perspective the message is gone unless they choose Undo.
        case queued(draftId: Int64, delaySeconds: Int)
        /// Queued durably but SMTP delivery failed; the draft stays in the
        /// outbox with the error recorded.
        case leftInOutbox(String)
        /// Scheduled with Send Later (HP1): held in the durable Outbox until
        /// `sendAt`. The compose window closes, like `.queued`.
        case scheduled(draftId: Int64, sendAt: Date)
        /// Could not even queue the draft (it is still editable).
        case failed(String)
        /// No stored credentials for this account — the Keychain entry is
        /// missing or malformed (e.g. the account was removed, or predates
        /// SMTP settings). Distinct from `.failed` because the fix is a
        /// specific navigation, not just an error string: ComposeView
        /// presents this as an alert with an "Open Accounts…" button
        /// instead of the inline caption.
        case missingCredentials(accountId: String)
    }

    /// Queue an already-saved draft and immediately run a delivery pass with
    /// the account's stored SMTP settings (Keychain, with IMAP-derived defaults).
    func deliverDraft(id: Int64, accountId: String,
                      completion: @escaping @MainActor (ComposeSendOutcome) -> Void) {
        guard let sendConfig = sendConfigJSON(for: accountId) else {
            completion(.missingCredentials(accountId: accountId))
            return
        }
        let core = self.core
        Task.detached {
            do {
                try core.sendDraft(id: id)
            } catch {
                // Queuing a draft for SMTP is refused on a read-only account,
                // so this is a write refusal like Archive or Flag and must go
                // through the same policy path — a bare error string here left
                // Send as the one action with no "Enable Writes" affordance.
                let isEvaluation = AccountWritePolicy.isEvaluationModeRefusal(error)
                let reason = String(describing: error)
                await MainActor.run {
                    if isEvaluation {
                        self.reportWriteFailure(
                            verb: "Send", accountId: accountId,
                            isEvaluationMode: true, reason: reason)
                        let email = self.accounts.first { $0.id == accountId }?.email ?? accountId
                        completion(.failed(
                            AccountWritePolicy.blockedNoticeMessage(verb: "Send", email: email)))
                    } else {
                        completion(.failed(reason))
                    }
                }
                return
            }
            var outcome = ComposeSendOutcome.leftInOutbox("SMTP delivery failed.")
            do {
                _ = try core.sendPending(configJSON: sendConfig)
                let status = try core.listDrafts(accountId: accountId).first { $0.id == id }?.status
                if status == "sent" {
                    outcome = .sent
                } else {
                    // Surface the recorded per-op error when we can find it.
                    let ops = (try? core.pendingOperations(accountId: accountId)) ?? []
                    // `failed_permanent` too: an auth or SMTP-AUTH-disabled
                    // refusal (MS1) is terminal on its first attempt, and its
                    // reason is what Compose must explain.
                    let reason = ops.last {
                        $0.kind == "send" && ($0.status == "failed" || $0.status == "failed_permanent")
                    }?.lastError
                    outcome = .leftInOutbox(reason ?? "SMTP delivery failed — will retry later.")
                }
            } catch {
                outcome = .leftInOutbox(String(describing: error))
            }
            let final = outcome
            await MainActor.run {
                self.reloadDrafts(accountId)
                self.refreshPendingCount()
                // A successful SMTP submit enqueues a durable `sent_copy` op
                // (APPEND to the Sent mailbox); nudge the replayer so the copy
                // uploads promptly instead of waiting for the next sync.
                if case .sent = final { self.scheduleReplay() }
                completion(final)
            }
        }
    }

    // MARK: - undo send (S25)

    /// Queue an already-saved draft with the undo-send window drawn from
    /// `Preferences.shared.undoSendDelay`. `.off` (0) is exactly `deliverDraft`
    /// — no behaviour change. Otherwise: queues with the delay, shows the
    /// "Message sent · Undo" banner immediately (the compose window closes
    /// just like a real send), and schedules the actual SMTP drain for when
    /// the window ends.
    func deliverDraftWithUndo(
        id: Int64, accountId: String,
        completion: @escaping @MainActor (ComposeSendOutcome) -> Void
    ) {
        let delay = Preferences.shared.undoSendDelay.seconds
        guard delay > 0 else {
            deliverDraft(id: id, accountId: accountId, completion: completion)
            return
        }
        guard storedConfig(for: accountId) != nil else {
            completion(.missingCredentials(accountId: accountId))
            return
        }
        let core = self.core
        Task.detached {
            do {
                try core.sendDraft(id: id, delaySeconds: Int64(delay))
            } catch {
                // Same write-safety routing as the immediate path: queuing a
                // delayed send is refused in evaluation mode exactly like an
                // immediate one.
                let isEvaluation = AccountWritePolicy.isEvaluationModeRefusal(error)
                let reason = String(describing: error)
                await MainActor.run {
                    if isEvaluation {
                        self.reportWriteFailure(
                            verb: "Send", accountId: accountId,
                            isEvaluationMode: true, reason: reason)
                        let email = self.accounts.first { $0.id == accountId }?.email ?? accountId
                        completion(.failed(
                            AccountWritePolicy.blockedNoticeMessage(verb: "Send", email: email)))
                    } else {
                        completion(.failed(reason))
                    }
                }
                return
            }
            await MainActor.run {
                self.reloadDrafts(accountId)
                self.refreshPendingCount()
                self.showUndoSendBanner(draftId: id, accountId: accountId, delaySeconds: delay)
                completion(.queued(draftId: id, delaySeconds: delay))
            }
        }
    }

    /// Shows the banner and starts its countdown ticker, plus this send's own
    /// scheduled drain. Replaces any earlier BANNER (only one is ever shown),
    /// but deliberately does NOT touch an earlier send's drain task — that
    /// one keeps running and still delivers when ITS window ends, exactly as
    /// `UndoSendBannerState`'s doc comment promises. Each drain task is
    /// tracked by draft id in `undoSendDrainTasks` so it can be found again
    /// (and only it) from Undo or from `deinit`.
    private func showUndoSendBanner(draftId: Int64, accountId: String, delaySeconds: Int) {
        undoSendBanner.send(draftId: draftId, accountId: accountId, delaySeconds: delaySeconds)
        undoSendTickTask?.cancel()
        undoSendTickTask = Task { [weak self] in
            while let self, !Task.isCancelled, self.undoSendBanner.pending?.draftId == draftId {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                if Task.isCancelled { return }
                self.undoSendBanner.tick()
            }
        }
        undoSendDrainTasks[draftId] = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delaySeconds) * 1_000_000_000 + 300_000_000)
            if Task.isCancelled { return }
            self?.undoSendDrainTasks[draftId] = nil
            self?.drainSendPending(accountId: accountId)
        }
    }

    /// Undo the send currently shown in the banner (the banner's Undo button,
    /// or ⌘Z while it's visible). Only ITS OWN drain task is cancelled — an
    /// earlier, still-counting-down send (already off the banner) is
    /// untouched, see `showUndoSendBanner`.
    func undoBannerSend() {
        guard let pending = undoSendBanner.pending else { return }
        undoSendBanner.dismiss()
        undoSendTickTask?.cancel()
        undoSend(draftId: pending.draftId, accountId: pending.accountId)
    }

    /// What `undoSend` does once the core reports whether it undid the send
    /// in time: reopen the draft for editing, or — the sender already
    /// claimed it — a short notice instead of silently doing nothing. Pure
    /// so the ROUTING decision itself has a fast unit test independent of
    /// the FFI/DB (`AppModelUndoRoutingTests`); `undoSend` is the only caller.
    enum UndoSendResult: Equatable {
        case reopenDraft(draftId: Int64)
        case tooLate
    }

    static func undoSendResult(inTime: Bool, draftId: Int64) -> UndoSendResult {
        inTime ? .reopenDraft(draftId: draftId) : .tooLate
    }

    /// Undo a not-yet-submitted queued send by draft id (used by the banner's
    /// Undo/⌘Z). Cancels that draft's own scheduled drain task, if any is
    /// still tracked (it may not be — e.g. a `held` op reconciled from a
    /// previous launch never had one in this process).
    ///
    /// The spec is: Undo reopens the draft, unsent, in a compose window — NOT
    /// merely reloading the drafts list and leaving the person to go find it.
    /// If the undo lost the race (the sender already claimed the op — too
    /// late), nothing is reopened; a short non-modal notice explains why
    /// instead (`undoTooLateNotice`, never a blocking alert).
    func undoSend(draftId: Int64, accountId: String) {
        undoSendDrainTasks[draftId]?.cancel()
        undoSendDrainTasks[draftId] = nil
        let core = self.core
        Task.detached {
            let inTime = (try? core.undoSend(draftId: draftId)) ?? false
            await MainActor.run {
                self.refreshPendingCount()
                switch AppModel.undoSendResult(inTime: inTime, draftId: draftId) {
                case .reopenDraft(let id):
                    self.reloadDrafts(accountId)
                    self.reopenDraftForEditing(accountId: accountId, draftId: id)
                case .tooLate:
                    self.undoTooLateNotice = String(localized: "Too late to undo — the message was already sent.")
                }
            }
        }
    }

    // MARK: - Send Later (HP1)

    /// How often the Send Later sweep looks for a scheduled send that has come
    /// due. A poll (not one timer per message) so it survives sleep, clock
    /// changes and relaunch with no extra state.
    static let sendLaterSweepInterval: UInt64 = 15_000_000_000 // 15s

    /// Queue an already-saved draft for delivery at `sendAt` (HP1). Durable:
    /// the message waits in the Outbox through quit and relaunch, is listed
    /// with its time and can be cancelled back to Drafts. Same write-safety
    /// routing as the undo-send path: refused in evaluation mode.
    func scheduleDraft(
        id: Int64, accountId: String, sendAt: Date,
        completion: @escaping @MainActor (ComposeSendOutcome) -> Void
    ) {
        guard storedConfig(for: accountId) != nil else {
            completion(.missingCredentials(accountId: accountId))
            return
        }
        let core = self.core
        let epoch = Int64(sendAt.timeIntervalSince1970)
        Task.detached {
            do {
                try core.scheduleSend(draftId: id, sendAt: epoch)
            } catch {
                let isEvaluation = AccountWritePolicy.isEvaluationModeRefusal(error)
                let reason = String(describing: error)
                await MainActor.run {
                    if isEvaluation {
                        self.reportWriteFailure(
                            verb: "Send", accountId: accountId,
                            isEvaluationMode: true, reason: reason)
                        let email = self.accounts.first { $0.id == accountId }?.email ?? accountId
                        completion(.failed(
                            AccountWritePolicy.blockedNoticeMessage(verb: "Send", email: email)))
                    } else {
                        completion(.failed(reason))
                    }
                }
                return
            }
            await MainActor.run {
                self.reloadDrafts(accountId)
                self.refreshPendingCount()
                completion(.scheduled(draftId: id, sendAt: sendAt))
            }
        }
    }

    /// Outbox "Reschedule": move a waiting (or needs-confirmation) scheduled
    /// send to `sendAt`. Throws when the time is not in the future.
    func rescheduleSend(accountId: String, draftId: Int64, sendAt: Date) async throws {
        let core = self.core
        let epoch = Int64(sendAt.timeIntervalSince1970)
        _ = try await Task.detached { try core.rescheduleSend(draftId: draftId, sendAt: epoch) }.value
        refreshPendingCount()
    }

    /// Outbox "Cancel" / "Back to Drafts": the scheduled message returns,
    /// intact, to Drafts. A benign no-op when the sender already claimed it.
    func sendBackToDrafts(accountId: String, draftId: Int64) async throws {
        let core = self.core
        _ = try await Task.detached { try core.cancelScheduledSend(draftId: draftId) }.value
        reloadDrafts(accountId)
        refreshPendingCount()
    }

    /// Start the periodic Send Later sweep: every `sendLaterSweepInterval`,
    /// drain any account that has a scheduled send whose time has come. The
    /// core decides what actually goes out (never before `send_at`; a send
    /// more than ten minutes late is parked as needs-confirmation instead).
    func startSendLaterSweep() {
        sendLaterSweepTask?.cancel()
        sendLaterSweepTask = Task { [weak self] in
            while let self, !Task.isCancelled {
                await self.runSendLaterSweep()
                try? await Task.sleep(nanoseconds: Self.sendLaterSweepInterval)
            }
        }
    }

    /// Accounts the Send Later sweep should drain: those with a scheduled send
    /// that is `queued` and whose time has come, OR one whose earlier attempt
    /// `failed` (non-permanent) — previously a failed first attempt was never
    /// retried until relaunch or a network flap (#3453). The core still decides
    /// whether a failed send's backoff has elapsed (`due_for_attempt`), so a
    /// premature drain is a cheap no-op. `failed_permanent` is never swept.
    nonisolated static func sendLaterSweepIsDue(_ ops: [OperationRecord], now: Int64) -> Bool {
        ops.contains {
            guard $0.isScheduledSend else { return false }
            switch $0.status {
            case "queued": return ($0.notBeforeEpoch ?? Int64.max) <= now
            case "failed": return true
            default: return false
            }
        }
    }

    private func runSendLaterSweep() async {
        let core = self.core
        let ids = accounts.map(\.id)
        let now = Int64(Date().timeIntervalSince1970)
        let due: [String] = await Task.detached {
            ids.filter { id in
                let ops = (try? core.pendingOperations(accountId: id)) ?? []
                return AppModel.sendLaterSweepIsDue(ops, now: now)
            }
        }.value
        for id in due { drainSendPending(accountId: id, announceFailure: false) }
    }

    /// Reopen a draft (now back in `draft` status, e.g. after `undoSend`) in
    /// a compose window — used by the Outbox "Edit" action, and by
    /// `undoSend` above. Fetches the current row rather than trusting a
    /// stale in-memory one, and is a silent no-op if it's gone (e.g. deleted
    /// from another window in the interim).
    func reopenDraftForEditing(accountId: String, draftId: Int64) {
        guard let draft = (try? core.listDrafts(accountId: accountId))?
            .first(where: { $0.id == draftId })
        else { return }
        (onUndoReopenDraft ?? openDraftForEditing)(draft)
    }

    /// Open `draft` in a compose window. The real action behind
    /// `reopenDraftForEditing` (and callable directly wherever a `Draft` is
    /// already in hand).
    func openDraftForEditing(_ draft: Draft) {
        ComposeWindowController.show(model: self, session: ComposeSession(draft: draft))
    }

    /// DR1: opening a row of the message list (double-click, Return). When
    /// the row is a message in a Drafts-role mailbox it is opened in the
    /// composer for editing and this returns true; any other row returns
    /// false and the caller opens it as a message as before. The core keys
    /// the draft by the LOCAL message id (`header.id`), reusing the draft
    /// row (attachments included) when this server copy was saved by us.
    @discardableResult
    func openDraft(from header: MessageHeader) -> Bool {
        let role = (mailboxes[header.accountId] ?? []).first { $0.id == header.mailboxId }?.role
            ?? (selectedMailbox?.id == header.mailboxId ? selectedMailbox?.role : nil)
        guard role == "drafts" else { return false }
        openServerDraftForEditing(accountId: header.accountId, messageId: header.id)
        return true
    }

    /// S36: hydrate (or reuse) a local editable draft from a message already
    /// synced into a Drafts-role mailbox — e.g. composed from another
    /// client, or synced before this build's server-drafts support existed
    /// — then open it for editing. Silent no-op on failure, matching
    /// `reopenDraftForEditing`.
    func openServerDraftForEditing(accountId: String, messageId: String) {
        let core = self.core
        Task.detached {
            guard let draftId = try? core.openServerDraft(messageId: messageId) else { return }
            await MainActor.run { self.reopenDraftForEditing(accountId: accountId, draftId: draftId) }
        }
    }

    /// Run one SMTP drain pass for `accountId` off-actor, using its stored
    /// credentials. Used both by the delayed-send scheduler (once the undo
    /// window ends) and by the Outbox "Send now" action. A missing config
    /// (e.g. the account was removed) is a silent no-op, like every other
    /// background drain in this file.
    ///
    /// `announceFailure` (default true) publishes `sendFailedNotice` when the
    /// pass fails; the due-scheduled and calendar-reply drains pass false.
    func drainSendPending(accountId: String, announceFailure: Bool = true) {
        // The core's send_pending takes a SendConfig (from_email + a nested
        // smtp object), NOT the flat AccountConfig the Keychain stores —
        // handing it the stored JSON made every delayed drain fail to
        // deserialise, and `try?` swallowed it, so an undo-send window that
        // simply ran out never delivered (and Outbox > Send Now did nothing).
        guard let cfg = sendConfigJSON(for: accountId) else { return }
        let core = self.core
        Task { @MainActor in
            // OB1: wait for any other SMTP pass on this account to finish.
            await self.smtpGate.acquire(accountId)
            let outcome = await Task.detached { () -> (report: SendReport?, thrown: String?, lastError: String?) in
                var report: SendReport?
                var thrown: String?
                do { report = try core.sendPending(configJSON: cfg) } catch { thrown = String(describing: error) }
                var lastError: String?
                if announceFailure, (report?.failed ?? 0) > 0 {
                    let ops = (try? core.pendingOperations(accountId: accountId)) ?? []
                    lastError = ops.last {
                        $0.kind == "send" && ($0.status == "failed" || $0.status == "failed_permanent")
                    }?.lastError
                }
                return (report, thrown, lastError)
            }.value
            self.smtpGate.release(accountId)
            self.reloadDrafts(accountId)
            self.refreshPendingCount()
            if (outcome.report?.sent ?? 0) > 0 { self.scheduleReplay() }
            if announceFailure, let notice = SendFailureNotice.message(
                failed: outcome.report?.failed ?? 0, thrown: outcome.thrown, lastError: outcome.lastError) {
                self.sendFailedNotice = notice
            }
        }
    }

    // MARK: - Outbox: Send Now / Send All (OB1)

    /// Why an Outbox send could not even start.
    struct OutboxSendError: LocalizedError {
        let errorDescription: String?
    }

    /// Outbox "Send Now": attempt exactly this message now, whatever its
    /// state (a permanent failure included). A read-only account is refused
    /// by the core and raises WS1's write-safety dialog, whose Enable Writes
    /// repeats this Send Now.
    @discardableResult
    func sendOutboxMessageNow(accountId: String, draftId: Int64) async throws -> SendReport {
        guard let cfg = sendConfigJSON(for: accountId) else {
            throw OutboxSendError(errorDescription: String(
                localized: "No saved sign-in for this account — add its password in Settings › Accounts."))
        }
        let core = self.core
        await smtpGate.acquire(accountId)
        let result = await Task.detached {
            Result { try core.sendDraftNow(configJSON: cfg, draftId: draftId) }
        }.value
        smtpGate.release(accountId)
        reloadDrafts(accountId)
        refreshPendingCount()
        let report = try result.get()
        if report.sent > 0 { scheduleReplay() }
        if report.blocked > 0 {
            raiseOutboxWriteRefusal(accountId: accountId) { [weak self] in
                Task { try? await self?.sendOutboxMessageNow(accountId: accountId, draftId: draftId) }
            }
        }
        return report
    }

    /// Outbox "Send All" for one account: every failed send gets a fresh
    /// attempt (permanent failures included), then one delivery pass.
    @discardableResult
    func sendAllOutbox(accountId: String) async throws -> SendReport {
        guard let cfg = sendConfigJSON(for: accountId) else {
            throw OutboxSendError(errorDescription: String(
                localized: "No saved sign-in for this account — add its password in Settings › Accounts."))
        }
        let core = self.core
        await smtpGate.acquire(accountId)
        let result = await Task.detached {
            Result { () throws -> SendReport in
                try core.requeueFailedSends(accountId: accountId)
                return try core.sendPending(configJSON: cfg)
            }
        }.value
        smtpGate.release(accountId)
        reloadDrafts(accountId)
        refreshPendingCount()
        let report = try result.get()
        if report.sent > 0 { scheduleReplay() }
        if report.blocked > 0 {
            raiseOutboxWriteRefusal(accountId: accountId) { [weak self] in
                Task { try? await self?.sendAllOutbox(accountId: accountId) }
            }
        }
        return report
    }

    /// Send All across every account with messages waiting (the Message menu
    /// command and its shortcut). Returns the reports, one per account tried.
    @discardableResult
    func sendAllOutboxEverywhere() async -> [Result<SendReport, Error>] {
        guard !isSendingOutbox else { return [] }
        isSendingOutbox = true
        defer { isSendingOutbox = false }
        let core = self.core
        let ids = accounts.map(\.id)
        let withMessages = await Task.detached {
            ids.filter { id in
                OutboxSendPresentation.count((try? core.outboxSends(accountId: id)) ?? []) > 0
            }
        }.value
        var results: [Result<SendReport, Error>] = []
        for id in withMessages {
            do { results.append(.success(try await sendAllOutbox(accountId: id))) } catch {
                results.append(.failure(error))
            }
        }
        return results
    }

    /// WS1 owns the dialog: report the refusal and let its Enable Writes
    /// repeat the Outbox action.
    private func raiseOutboxWriteRefusal(accountId: String, retry: @escaping @MainActor () -> Void) {
        reportWriteFailure(
            verb: String(localized: "Send"), accountId: accountId,
            isEvaluationMode: true, reason: "")
        registerWriteSafetyRetry(accountId: accountId, retry)
    }

    /// Start watching the network so queued sends go out when it returns.
    /// Never under XCTest or a UI-script run (no surprise SMTP traffic).
    private func startOutboxReachability() {
        guard !TestHost.isActive, !TestHost.isUIScript, reachability == nil else { return }
        let monitor = NetworkReachabilityMonitor()
        reachability = monitor
        monitor.start { [weak self] online in
            self?.outboxAutoSender.networkChanged(isOnline: online)
        }
    }

    /// Crash-safety pass (S25 — see DECISIONS.md): reconciles every account
    /// id in `accountIds` NOT already reconciled this launch — holding any
    /// delayed send whose undo window cannot be proven to have stayed open —
    /// then runs the queued-op drain (`replayNow`). Must be called from
    /// inside `reloadAccounts`'s apply closure (never against a separately
    /// captured `accounts` snapshot): at `bootstrap()` time `accounts` is
    /// still `[]`, so a call made before this closure runs would reconcile
    /// zero accounts and the hold would silently never happen.
    ///
    /// Reconcile-once-per-account is enforced by marking `accountIds`
    /// reconciled SYNCHRONOUSLY here, on the main actor, before the
    /// `Task.detached` that actually runs it — so a second, concurrent
    /// `reloadAccounts` (e.g. triggered by a sync that finished while the
    /// first reconcile was still in flight) sees those ids already claimed
    /// and only awaits the drain, never repeating the reconcile itself
    /// (which `reconcile_undo_sends` in repo.rs requires: repeating it
    /// mid-session would immediately hold a legitimate in-session undo
    /// window whose heartbeat simply hasn't ticked yet).
    private func reconcileUndoSendsThenReplay(accountIds: [String]) {
        let toReconcile = accountIds.filter { !reconciledUndoSendAccountIds.contains($0) }
        guard !toReconcile.isEmpty else {
            replayNow()
            outboxAutoSender.accountsLoaded(accountIds)
            return
        }
        reconciledUndoSendAccountIds.formUnion(toReconcile)
        let core = self.core
        Task.detached {
            for id in toReconcile { _ = try? core.reconcileUndoSends(accountId: id) }
            await MainActor.run {
                self.refreshPendingCount()
                self.replayNow()
                // OB1: queued sends are attempted at launch (once per account).
                self.outboxAutoSender.accountsLoaded(accountIds)
            }
        }
    }

    // MARK: - server drafts (S36)

    /// Coalesce a burst of compose edits into one server-draft save ~2s
    /// after the last edit (mirrors `refreshContactsDebounced`). Call this
    /// on every compose text change; cancels and reschedules on each call,
    /// so only the LAST edit in a burst actually reaches the network. Keyed
    /// per draft id so concurrent compose windows never cancel each other.
    /// `session` is captured weakly and re-read only when the debounce
    /// actually fires, so a fast burst of edits within the window is never
    /// lost to a stale snapshot taken when the timer was (re)started —
    /// `draftId` itself is stable once assigned (the caller must have
    /// already created the local row, e.g. via `ComposeSession.ensureDraftSaved`).
    func scheduleServerDraftSave(session: ComposeSession, draftId: Int64) {
        serverDraftSaveDebounce[draftId]?.cancel()
        serverDraftSaveState[draftId] = .idle
        let interval = composeAutosave.debounceInterval
        serverDraftSaveDebounce[draftId] = Task { [weak self, weak session] in
            try? await Task.sleep(nanoseconds: UInt64(interval * 1_000_000_000))
            if Task.isCancelled { return }
            guard let self, let session else { return }
            // Fired: nothing is pending any more (`hasPendingServerDraftSave`).
            self.serverDraftSaveDebounce[draftId] = nil
            self.saveServerDraftNow(session: session, draftId: draftId)
        }
    }

    /// True while a debounced server-draft save is scheduled but has not
    /// fired: the composer holds edits the server copy has not seen.
    func hasPendingServerDraftSave(draftId: Int64) -> Bool {
        serverDraftSaveDebounce[draftId] != nil
    }

    /// Runs a pending debounced server-draft save now instead of dropping
    /// it (window close, #3444): the local row and the server mirror both
    /// get the composer's current content. A no-op when nothing is pending.
    func flushServerDraftSave(session: ComposeSession, draftId: Int64) {
        guard let pending = serverDraftSaveDebounce[draftId] else { return }
        pending.cancel()
        serverDraftSaveDebounce[draftId] = nil
        saveServerDraftNow(session: session, draftId: draftId)
    }

    /// Drops a pending debounced save. Only for a draft that is being sent
    /// or deleted: a pending save holds edits the server copy has not seen,
    /// so a window that keeps its draft must `flushServerDraftSave` instead
    /// (#3444). An op the debounce already queued is durable either way.
    func cancelServerDraftSave(draftId: Int64) {
        serverDraftSaveDebounce[draftId]?.cancel()
        serverDraftSaveDebounce[draftId] = nil
        serverDraftSaveState[draftId] = nil
    }

    /// Persist the composer's CURRENT content locally (an edit made after
    /// the debounce was scheduled but before it fired must not be lost),
    /// then mirror it to the server. Every blocking FFI call stays off the
    /// main actor, matching `saveDraft`/`replayNow`'s existing shape.
    private func saveServerDraftNow(session: ComposeSession, draftId: Int64) {
        serverDraftSaveState[draftId] = .saving
        let core = self.core
        let accountId = session.accountId
        let latest = session.buildDraftForSave(renderMarkdown: session.markdownRenderer(core: core))
        let fromEmail = session.fromIdentity?.email ?? selfEmail(for: accountId)
        let fromName = session.fromIdentity?.name
        Task.detached {
            _ = try? core.saveDraft(latest)
            let queued = (try? core.saveDraftToServer(id: draftId, fromEmail: fromEmail, fromName: fromName)) ?? false
            await MainActor.run {
                self.reloadDrafts(accountId)
                self.serverDraftSaveState[draftId] = queued ? .saved(Date()) : .idle
                self.scheduleReplay()
            }
        }
    }

    // MARK: - contacts

    /// Coalesce a burst of syncs into one contacts refresh ~2s later (mirrors
    /// the replay debounce idiom).
    func refreshContactsDebounced() {
        contactsDebounce?.cancel()
        contactsDebounce = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            if Task.isCancelled { return }
            self?.refreshContactsNow()
        }
    }

    /// Arm the once-per-launch refresh: it starts when the first list reaches
    /// the UI, or after `launchContactsFallbackNanos` if none ever does.
    func armLaunchContactsRefresh() {
        guard launchContactsRefresh == .idle else { return }
        launchContactsRefresh = .armed
        let delay = launchContactsFallbackNanos
        launchContactsFallback = Task { [weak self] in
            try? await Task.sleep(nanoseconds: delay)
            if Task.isCancelled { return }
            self?.startLaunchContactsRefreshIfArmed()
        }
    }

    /// Start the launch refresh if armed and not yet started: exactly once.
    func startLaunchContactsRefreshIfArmed() {
        guard launchContactsRefresh == .armed else { return }
        // X1: the encoded-text repair goes first, off the main actor, so the
        // refresh that follows derives contact names from decoded text. It is
        // a single cheap read once the profile has been repaired.
        launchContactsRefresh = .started
        launchContactsFallback?.cancel()
        launchContactsFallback = nil
        let core = self.core
        let repair = envelopeRepairWork
        Task.detached {
            var repaired: Int64 = 0
            do {
                repaired = try repair(core)
            } catch {
                // Background maintenance: log, never a modal alert. The
                // repair is resumable, so the next launch tries again.
                Log.ui.error("encoded-text repair failed: \(error, privacy: .private)")
            }
            let changed = repaired > 0
            await MainActor.run {
                // Rows on screen were loaded before the repair: show the
                // decoded subjects now rather than at the next sync.
                if changed { self.reloadCurrentMessages() }
                self.refreshContactsNow()
            }
        }
    }

    /// Recompute the aggregate in the core (off the main actor), then reload
    /// the visible list. Never re-entrant: a call while one is running only
    /// marks a single follow-up run. Any refresh consumes the launch one.
    func refreshContactsNow() {
        launchContactsRefresh = .started
        launchContactsFallback?.cancel()
        launchContactsFallback = nil
        guard !contactsRefreshRunning else { contactsRefreshDirty = true; return }
        contactsRefreshRunning = true
        let work = contactsRefreshWork
        load({ try work($0) }, apply: { _ in self.reloadContacts() }, always: {
            self.contactsRefreshRunning = false
            if self.contactsRefreshDirty {
                self.contactsRefreshDirty = false
                self.refreshContactsNow()
            }
        })
    }

    /// Reload the contact list with the current query/hidden filters.
    func reloadContacts() {
        let query = contactsQuery.isEmpty ? nil : contactsQuery
        let includeHidden = contactsIncludeHidden
        load({ try $0.listContacts(query: query, includeHidden: includeHidden) }) {
            self.contacts = $0
        }
    }

    /// Save a contact's user-owned fields, then refresh the list/detail.
    func updateContact(id: Int64, displayName: String?, notes: String, isHidden: Bool,
                       completion: @escaping @MainActor (Bool) -> Void = { _ in }) {
        let core = self.core
        Task.detached {
            do {
                try core.updateContact(id: id, displayName: displayName,
                                       notes: notes, isHidden: isHidden)
                await MainActor.run {
                    self.reloadContacts()
                    completion(true)
                }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not save contact: \(error)"
                    completion(false)
                }
            }
        }
    }

    /// Open (or re-front) the Contacts window with fresh data.
    func openContacts() {
        // Recompute rather than only reading the aggregate table. This covers
        // profiles created before contact aggregation existed and launches
        // where the provider is currently unavailable but cached mail remains.
        refreshContactsNow()
        ContactsWindowController.show(model: self)
    }

    /// Load the full contact detail (including enrichment) for `id`.
    func loadContactDetail(id: Int64, completion: @escaping @MainActor (ContactDetail?) -> Void) {
        load({ try $0.getContact(id: id) }) { completion($0) }
    }

    // MARK: - enrichment

    /// All registered enrichment adapters. On failure surfaces the error via
    /// `errorText` and reports an empty list (the sheet then hides the picker).
    func loadEnrichmentSources(
        completion: @escaping @MainActor ([EnrichmentSourceInfo]) -> Void
    ) {
        let core = self.core
        Task.detached {
            do {
                let sources = try core.enrichmentSources()
                await MainActor.run { completion(sources) }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not load enrichment sources: \(error)"
                    completion([])
                }
            }
        }
    }

    /// Preview: parse `input` through `sourceId` without writing to the DB.
    /// The completion receives the parsed rows or the error (shown inline
    /// in the sheet, not via the global `errorText`).
    func previewEnrichment(
        contactId: Int64, sourceId: String, input: String,
        completion: @escaping @MainActor (Result<[PreviewEnrichmentField], Error>) -> Void
    ) {
        let core = self.core
        Task.detached {
            let result = Result { try core.previewEnrichment(
                contactId: contactId, sourceId: sourceId, input: input) }
            await MainActor.run { completion(result) }
        }
    }

    /// Save: parse `input` through `sourceId` and persist the fields.
    func saveEnrichment(
        contactId: Int64, sourceId: String, input: String,
        completion: @escaping @MainActor (Result<[EnrichmentField], Error>) -> Void
    ) {
        let core = self.core
        Task.detached {
            let result = Result { try core.saveEnrichment(
                contactId: contactId, sourceId: sourceId, input: input) }
            await MainActor.run { completion(result) }
        }
    }

    /// Delete a single enrichment field row by id, then invoke `completion`.
    func deleteEnrichment(id: Int64, completion: @escaping @MainActor () -> Void = {}) {
        let core = self.core
        Task.detached {
            do {
                try core.deleteEnrichment(id: id)
                await MainActor.run { completion() }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not delete enrichment field: \(error)"
                    completion()
                }
            }
        }
    }

    /// Quick-add a single free-form field to the 'manual' source directly
    /// (bypasses the heuristic parser; used by the key-value quick-add row).
    func addEnrichmentField(
        contactId: Int64, field: String, value: String,
        completion: @escaping @MainActor () -> Void = {}
    ) {
        let core = self.core
        Task.detached {
            do {
                try core.rawAddEnrichment(
                    contactId: contactId, source: "manual",
                    field: field, value: value)
                await MainActor.run { completion() }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not add enrichment field: \(error)"
                    completion()
                }
            }
        }
    }

    /// Show all mail between me and `address` (View > Show Correspondence).
    /// Three cross-account searches (from / to / cc) merged: no new core API,
    /// and it stays consistent with the search grammar.
    func showCorrespondence(address: String, name: String?) {
        let context = CorrespondenceContext(address: address.lowercased(), name: name)
        searchText = ""
        // Snoozed refreshes write `messages` whenever its flag is set; step out
        // of it while Correspondence owns the list, and remember to come back.
        if selectedIsSnoozed {
            correspondenceCameFromSnoozed = true
            selectedIsSnoozed = false
        }
        correspondence = context
        invalidateInFlightPage()
        nextCursor = nil
        loadCorrespondence(context)
    }

    func closeCorrespondence() {
        guard correspondence != nil else { return }
        let cameFromSnoozed = correspondenceCameFromSnoozed // the didSet below clears it
        correspondence = nil
        correspondenceStats = CorrespondenceStats()
        if cameFromSnoozed {
            select(snoozed: ())
        } else if let id = selectedSmartMailboxId, let sm = smartMailboxes.first(where: { $0.id == id }) {
            loadSmartMailboxMessages(sm)
        } else if let key = selectedTagKey, let tag = tags.first(where: { $0.keywordKey == key }) {
            select(tag: tag)
        } else if let role = selectedUnifiedRole { select(unifiedRole: role) }
        else if let mb = selectedMailbox { select(mailbox: mb) }
        else { messages = [] }
    }

    /// Every address that is "me": account addresses plus all loaded identity
    /// addresses (MM-6), lower-cased. Drives sent/received counts and which
    /// party "Show Correspondence" picks.
    var myAddresses: Set<String> {
        Correspondence.myAddresses(
            accountEmails: accounts.map(\.email),
            identityEmails: identities.values.flatMap { $0.map(\.email) })
    }

    private func loadCorrespondence(_ context: CorrespondenceContext) {
        let queries = Correspondence.queries(for: context.address)
        let mine = myAddresses
        let cap = Correspondence.perQueryLimit
        let token = listToken
        load({ core in
            let lists = try queries.map { try core.searchAcrossAccounts(query: $0, limit: cap) }
            return Correspondence.mergeCapped(lists, cap: cap)
        }) { merged, capped in
            // Ignore a result that arrived after the user navigated away or
            // switched lists.
            guard self.correspondence == context, token == self.listToken else { return }
            self.messages = self.clientSorted(merged)
            var stats = Correspondence.stats(merged, myAddresses: mine)
            stats.isCapped = capped
            self.correspondenceStats = stats
        }
    }

    func runSearch() {
        let query = searchText
        // Typing a search leaves Correspondence (it is a different list mode);
        // an empty field while in Correspondence just refreshes it.
        if let context = correspondence {
            if query.isEmpty {
                loadCorrespondence(context)
                return
            }
            correspondence = nil
            correspondenceStats = CorrespondenceStats()
        }
        guard !query.isEmpty else {
            searchBodyCoverage = nil
            if let id = selectedSmartMailboxId, let sm = smartMailboxes.first(where: { $0.id == id }) {
                loadSmartMailboxMessages(sm)
            } else if selectedIsSnoozed { reloadSnoozedMessages() }
            else if let key = selectedTagKey, let tag = tags.first(where: { $0.keywordKey == key }) { select(tag: tag) }
            else if let role = selectedUnifiedRole { select(unifiedRole: role) }
            else if let mb = selectedMailbox { select(mailbox: mb) }
            return
        }
        // A smart mailbox is inherently cross-account (its saved queries have
        // no account-scoping operator) — typing into the search field while
        // one is selected searches cross-account too, exactly like the
        // unified "All Inboxes" case. Otherwise the scope menu decides (MM-2).
        nextCursor = nil // search results never page (S64) — see loadMoreMessagesIfNeeded's own guard too
        guard let plan = SearchQueryBuilder.plan(
            scope: searchScope, text: query, selectedMailbox: selectedMailbox,
            firstAccountId: accounts.first?.id,
            crossAccountSelection: selectedUnifiedRole != nil || selectedSmartMailboxId != nil
                || selectedIsSnoozed || selectedTagKey != nil)
        else { return }
        activeSearchIsCrossAccount = plan.accountId == nil
        // #3633: how many bodies in this scope are not downloaded yet (one cheap
        // indexed COUNT, off the main actor); the note shows under the results.
        let (scopeAccount, scopeMailbox, scopeQuery) = (plan.accountId, plan.mailboxId, plan.query)
        load({
            try $0.bodyHydrationScope(accountId: scopeAccount, mailboxId: scopeMailbox, query: scopeQuery)
        }) { coverage in
            if self.searchText == query { self.searchBodyCoverage = coverage }
        }
        // FP14: a late result must not overwrite a list the user has since
        // switched to (every select(...) rotates `listToken`).
        invalidateInFlightPage()
        let token = listToken
        if let acct = plan.accountId {
            load({ try $0.search(accountId: acct, query: plan.query, limit: 500) }) {
                guard token == self.listToken else { return }
                self.messages = self.clientSorted($0)
            }
        } else {
            load({ try $0.searchAcrossAccounts(query: plan.query, limit: 500) }) {
                guard token == self.listToken else { return }
                self.messages = self.clientSorted($0)
            }
        }
    }

    /// Reading-pane header click (MM-2): narrow the current search by one
    /// qualifier and run it. With an empty search field this simply starts a
    /// search for that sender / address / subject / day.
    func drillDown(qualifier: String, value: String) {
        searchText = SearchQueryBuilder.narrowing(searchText, qualifier: qualifier, value: value)
        runSearch()
    }

    /// Background (re-)sync of a configured account. S72: never stacks a
    /// second pass on an account whose sync is in flight -- the request is
    /// coalesced into one queued re-run (see `queuedResync`). `addAccount`
    /// deliberately bypasses this (it calls `performSync` directly): a
    /// Reconnect with a corrected config must supersede the running attempt.
    func sync(config: String) {
        if let target = AccountSyncTarget(configJSON: config), removedAccountIds.contains(target.accountId) {
            Log.sync.info("sync: skipped, account is being removed")
            return
        }
        if let target = AccountSyncTarget(configJSON: config), inFlightSyncs[target.accountId] != nil {
            queuedResync[target.accountId] = config
            Log.sync.info("sync: coalesced a request for in-flight account \(target.accountId, privacy: .private)")
            return
        }
        // Generation unused here: this closure only refreshes UI-side state
        // from `report` (this call's own, non-stale result) — it never
        // persists a captured value, so there is nothing for a stale
        // completion to regress. See `lastSavedGeneration`'s doc comment.
        performSync(config: config) { report, _ in
            self.reloadAccounts()
            self.refreshSmartMailboxCounts()
            self.reloadTags() // server keywords may have arrived with no local definition yet
            self.reloadCurrentMessages() // reflect moved/expunged mail in the view
            self.removalUndoBanner.syncCompleted(accountId: report.accountId) // #3022 "Returning…" done
            self.reloadVips() // S59: VIP unread count
            // A fresh sync is a good moment to flush the outbox.
            self.replayNow()
            // New mail can add people/edges; recompute shortly after the burst.
            self.refreshContactsDebounced()
            // Drain descriptor-only bodies in the background, newest-first.
            self.scheduleBackgroundHydration(accountId: report.accountId)
            // S58: incremental Spotlight indexing on ingest, off the main
            // actor — a no-op when the opt-in is off (the guard reads the
            // @MainActor-isolated flag here, BEFORE hopping off-actor, so
            // the detached task never touches `self`).
            if self.spotlightIndexingEnabled {
                let core = self.core
                let indexer = self.spotlightIndexer
                Task.detached { await indexer.indexRecentSync(core: core, report: report) }
            }
        }
    }

    /// Dedicated sync runner: background provider failures must never pass
    /// through generic `load`, whose contract is to populate modal `errorText`.
    /// Rust persists the structured kind; Swift owns the status/banner/raw-local
    /// presentation. `onSuccess` remains on MainActor for normal refresh work.
    /// `onFailure` is optional and additive — existing callers that only cared
    /// about success (the periodic loop, sidebar retry) are unaffected; the
    /// only current caller is `addAccount(completion:)`, driving the
    /// Reconnect sheet's inline result.
    ///
    /// `onSuccess` also receives this attempt's `generation`: it always runs
    /// (see the `isLatest`-gated presentation below), so a caller that
    /// persists something from its own captured state (only `addAccount`
    /// today) needs the generation to guard against a stale-but-completed
    /// attempt overwriting a newer one's save (PR #158).
    ///
    /// `onLoggedIn` (optional, only `addAccount` passes it) runs at most
    /// once, as soon as this attempt is provably past login — see
    /// `startSyncProgressPolling`.
    private func performSync(
        config: String,
        onLoggedIn: (@MainActor (UInt64) -> Void)? = nil,
        onSuccess: @escaping @MainActor (SyncReport, UInt64) -> Void,
        onFailure: (@MainActor (AccountSyncStatus) -> Void)? = nil
    ) {
        guard let target = AccountSyncTarget(configJSON: config) else {
            // Invalid locally-built configuration is not a provider sync error.
            errorText = "Could not read account configuration."
            return
        }
        guard !removedAccountIds.contains(target.accountId) else { return }
        beginSync()
        inFlightSyncs[target.accountId, default: 0] += 1
        syncPassesStarted[target.accountId, default: 0] += 1
        let generation = (syncGenerations[target.accountId] ?? 0) &+ 1
        syncGenerations[target.accountId] = generation
        accountSyncStatuses[target.accountId] = .syncing(target)
        Log.sync.info("sync: started for account \(target.accountId, privacy: .private)")
        startSyncProgressPolling(
            accountId: target.accountId, generation: generation, onLoggedIn: onLoggedIn)
        let core = self.core
        Task.detached {
            do {
                let report = try core.syncAccount(configJSON: config)
                let diagnostic = try? core.localDiagnostics().accounts.first {
                    $0.accountId == target.accountId
                }
                await MainActor.run {
                    self.endSync()
                    self.endInFlightSync(accountId: target.accountId)
                    // A genuinely completed sync's result (credential save,
                    // reloadAccounts, hydration, …) must never be silently
                    // thrown away just because a newer attempt for the same
                    // account has since superseded this generation — that
                    // was: cancel-the-sheet-and-retry could
                    // lose a 95%-done sync with no error shown. Only the
                    // UI-facing STATUS PRESENTATION (banner, dashboard/
                    // sidebar status, progress poller) is gated on being the
                    // latest generation, so a stale completion can't regress
                    // state a newer sync already established; `onSuccess`/
                    // `onFailure` themselves always run.
                    let isLatest = self.syncGenerations[target.accountId] == generation
                    // Only tear down THIS generation's poller: a stale
                    // completion (superseded by a newer sync for the same
                    // account) must not cancel the newer poller that already
                    // replaced it in `syncProgressPollTasks`.
                    if isLatest {
                        self.finishSyncProgressPolling(accountId: target.accountId)
                    }
                    // A panic-guarded provider can return a partial report while
                    // the durable row correctly says the account failed.
                    if let diagnostic, diagnostic.consecutiveFailures > 0 {
                        let status = AccountSyncStatus.failure(
                            target: target,
                            errorKind: diagnostic.lastErrorKind,
                            // A partial report has no thrown FFI detail; its
                            // structured kind still drives the status message.
                            rawError: nil
                        )
                        Log.sync.error("sync: partial failure for account \(target.accountId, privacy: .private) kind=\(diagnostic.lastErrorKind ?? "unknown", privacy: .public)")
                        if isLatest {
                            self.accountSyncStatuses[target.accountId] = status
                            self.syncBanner = status
                        }
                        onFailure?(status)
                    } else {
                        Log.sync.info("sync: succeeded for account \(target.accountId, privacy: .private)")
                        if isLatest {
                            self.accountSyncStatuses[target.accountId] = .ok(target)
                            self.accountLastChecked[target.accountId] = self.clock()
                            if self.syncBanner?.accountId == target.accountId {
                                self.syncBanner = self.fallbackSyncBanner()
                            }
                        }
                    }
                    // S59: announce what this pass just landed without waiting
                    // for the 3 s drain loop.
                    self.notificationCoordinator?.drainSoon()
                    onSuccess(report, generation)
                    self.runQueuedResyncIfIdle(accountId: target.accountId)
                    self.sweepRemovedAccountIfIdle(target.accountId)
                }
            } catch {
                let rawError = String(describing: error)
                let diagnostic = try? core.localDiagnostics().accounts.first {
                    $0.accountId == target.accountId
                }
                let status = AccountSyncStatus.failure(
                    target: target,
                    // Existing accounts always have a durable row. A first-ever
                    // connect can precede the account row/local join; in that
                    // narrow case the thrown text carries the core's leading
                    // "<kind>: " token (F3); only an unclassified failure falls
                    // back to the network status class.
                    errorKind: diagnostic?.lastErrorKind
                        ?? AccountSyncStatus.errorKind(fromThrown: rawError) ?? "network",
                    rawError: rawError
                )
                // `status.errorKind`/`state` are structured and safe to log
                // publicly; `rawError` (the thrown FFI text) is NOT — it can
                // carry provider-controlled text, so it stays out of the log
                // (it's already retained locally in `accountSyncStatuses` for
                // the Diagnostics window, same as today).
                Log.sync.error("sync: failed for account \(target.accountId, privacy: .private) kind=\(status.errorKind ?? "unknown", privacy: .public)")
                await MainActor.run {
                    self.endSync()
                    self.endInFlightSync(accountId: target.accountId)
                    // See the success path above: presentation is gated on
                    // being latest, but `onFailure` always runs.
                    let isLatest = self.syncGenerations[target.accountId] == generation
                    if isLatest {
                        self.finishSyncProgressPolling(accountId: target.accountId)
                        self.accountSyncStatuses[target.accountId] = status
                        self.syncBanner = status
                    }
                    onFailure?(status)
                    self.runQueuedResyncIfIdle(accountId: target.accountId)
                    self.sweepRemovedAccountIfIdle(target.accountId)
                }
            }
        }
    }

    /// An uncancellable sync pass that was in flight when the user removed the account has
    /// finished: it may have re-created the core row, its mail and (via a late save) the Keychain
    /// entry. Remove them again, then drop the tombstone (#3439). No-op unless the account is
    /// tombstoned and no pass for it is still running.
    func sweepRemovedAccountIfIdle(_ accountId: String) {
        guard removedAccountIds.contains(accountId), inFlightSyncs[accountId] == nil else { return }
        let core = self.core
        let store = accountStore
        let indexer = spotlightIndexer
        Task.detached {
            try? core.stopIdle(accountId: accountId)
            _ = try? core.removeAccount(accountId: accountId)   // "unknown account" when nothing came back
            await indexer.removeAll(forAccount: accountId)
            var keychainGone = true
            do { try store.delete(accountID: accountId) } catch { keychainGone = false }
            await MainActor.run {
                if keychainGone { self.removedAccountIds.remove(accountId) }
                self.dropRemovedAccountFromList(accountId)
                self.reloadAccounts()
            }
        }
    }

    /// Launch: finish any removal an earlier run could not complete (a Keychain delete that
    /// failed, or a quit in the middle of Remove).
    func retryPendingAccountRemovals() {
        for id in removedAccountIds { sweepRemovedAccountIfIdle(id) }
    }

    /// Starts (replacing any prior) background polling of `accountId`'s sync
    /// progress while its `performSync` pass is in flight, publishing into
    /// `accountSyncProgress` for the Status dashboard, sidebar tooltip and
    /// Add-account sheet to read. Stops itself once a newer
    /// generation supersedes this one or the snapshot reports `done`;
    /// `finishSyncProgressPolling` additionally cancels it and clears the
    /// published entry as soon as the sync call itself returns, so a stale
    /// value never lingers past that even if a poll is mid-flight.
    ///
    /// Also reloads `accounts`/mailboxes the first time the account appears
    /// in a snapshot: `sync_with_provider` upserts the account row (and its
    /// mailboxes, as it visits them) long before a large first sync's single
    /// `castellum_sync_account` call returns, but `model.accounts` otherwise
    /// only learns about a BRAND NEW account from that call's completion —
    /// leaving it invisible in the sidebar for the entire first sync, no
    /// matter how long that takes. Polling for progress doubles as the
    /// earliest safe point to notice "the account now exists locally".
    ///
    /// `onLoggedIn` fires once, on the first LIVE (`done == false`) snapshot
    /// seen while this is the account's ONLY in-flight sync — both before
    /// and after the poll. The core only begins a snapshot after LOGIN and
    /// LIST succeeded (and after creating the account row), and a finished
    /// pass leaves its snapshot `done`, so under that condition a live one
    /// can only be this attempt's: proof its config was accepted. While a
    /// superseded attempt is still running the proof is ambiguous and
    /// nothing fires; `onSuccess` still saves on completion, as before.
    private func startSyncProgressPolling(
        accountId: String, generation: UInt64,
        onLoggedIn: (@MainActor (UInt64) -> Void)? = nil
    ) {
        syncProgressPollTasks[accountId]?.cancel()
        let core = self.core
        syncProgressPollTasks[accountId] = Task { [weak self] in
            var announcedExisting = false
            var reportedLoggedIn = onLoggedIn == nil
            while !Task.isCancelled {
                guard let self, self.syncGenerations[accountId] == generation else { return }
                let soleBefore = self.inFlightSyncs[accountId] == 1
                let polled = try? await Task.detached { try core.syncProgress(accountId: accountId) }.value
                guard self.syncGenerations[accountId] == generation else { return }
                if let snapshot = polled ?? nil {
                    //: `@Published` fires every observer on
                    // every assignment regardless of whether the value
                    // actually changed, and this poll re-reads every 400ms
                    // for the whole sync -- a large first sync could
                    // otherwise republish an UNCHANGED snapshot dozens of
                    // times while nothing SwiftUI cares about moved.
                    // `SyncProgress` is `Hashable`/`Equatable`, so skip the
                    // publish outright when nothing changed.
                    if self.accountSyncProgress[accountId] != snapshot {
                        self.accountSyncProgress[accountId] = snapshot
                    }
                    if !reportedLoggedIn, !snapshot.done, soleBefore,
                       self.inFlightSyncs[accountId] == 1 {
                        reportedLoggedIn = true
                        onLoggedIn?(generation)
                    }
                    if !announcedExisting, !self.accounts.contains(where: { $0.id == accountId }) {
                        announcedExisting = true
                        self.reloadAccounts()
                    }
                    if snapshot.done { return }
                }
                try? await Task.sleep(nanoseconds: 400_000_000)
            }
        }
    }

    /// Stops `accountId`'s progress poller (if any) and clears its published
    /// snapshot — called right where `performSync` learns its FFI call
    /// returned, success or failure, so the UI never shows progress for a
    /// sync that has actually finished.
    private func endInFlightSync(accountId: String) {
        let remaining = (inFlightSyncs[accountId] ?? 1) - 1
        inFlightSyncs[accountId] = remaining > 0 ? remaining : nil
    }

    /// Start the one re-sync queued for `accountId` while its sync was in
    /// flight, once no pass for it is running any more (S72). Called at the
    /// very end of a pass's completion handling, so the finished pass has
    /// already published its own status before the re-run supersedes it.
    private func runQueuedResyncIfIdle(accountId: String) {
        guard inFlightSyncs[accountId] == nil,
              let config = queuedResync.removeValue(forKey: accountId) else { return }
        sync(config: config)
    }

    private func finishSyncProgressPolling(accountId: String) {
        syncProgressPollTasks[accountId]?.cancel()
        syncProgressPollTasks[accountId] = nil
        accountSyncProgress[accountId] = nil
    }

    // MARK: - background body hydration

    /// Keep running short bounded hydration passes for one account while they
    /// make progress, so descriptor-only rows gain bodies without user action.
    /// Body-on-open keeps priority structurally: each pass is small (≤100
    /// bodies), uses its own IMAP connection, and sleeps between passes so
    /// interactive FFI calls interleave. Stop conditions and per-sync
    /// pass/byte budgets live in `BackgroundHydrationLoop.drain`; the
    /// `SerializedTaskSlots` slot guarantees at most one blocking hydration
    /// call per account even across cancel-and-replace.
    private func scheduleBackgroundHydration(accountId: String) {
        reloadHydrationProgress(accountId) // runs after every sync, even with hydration off
        // Pilot default: OFF. Bodies are fetched only when a message is opened
        // (body-on-open), so adding an account never starts an unbounded
        // background mailbox download. The user opts in explicitly.
        guard Preferences.shared.backgroundBodyHydrationEnabled else { return }
        guard let configJSON = storedConfig(for: accountId) else { return }
        let core = self.core
        backgroundHydration.replace(key: accountId) { [weak self] in
            await BackgroundHydrationLoop.drain(
                runPass: { remainingBytes in
                    guard
                        let budgeted = HydrationConfig.withByteBudget(
                            configJSON, remainingBytes: remainingBytes)
                    else { return nil }
                    return try? core.hydratePendingBodies(configJSON: budgeted)
                },
                onProgress: {
                    // has_body flipped on visible rows; refresh the open list.
                    await MainActor.run {
                        self?.reloadCurrentMessages()
                        self?.reloadHydrationProgress(accountId)
                    }
                },
                betweenPasses: { try? await Task.sleep(nanoseconds: 2_000_000_000) }
            )
        }
    }

    /// Open (or re-front) the Diagnostics window.
    func openDiagnostics() {
        DiagnosticsWindowController.show(model: self)
    }

    /// Open (or re-front) the Outbox recovery window.
    func openOutbox() {
        OutboxWindowController.show(model: self)
    }

    /// Open (or re-front) the Status dashboard window.
    func openStatusDashboard() {
        StatusDashboardWindowController.show(model: self)
    }

    /// Open (or re-front) the AI Activity window.
    func openAiActivity() {
        AiActivityWindowController.show(model: self)
    }

    // MARK: - projects

    /// Open (or re-front) the Projects board with fresh data.
    func openProjectsBoard() {
        reloadProjects()
        ProjectsBoardWindowController.show(model: self)
    }

    func reloadProjects(includeArchived: Bool = false) {
        let ia = includeArchived
        load({ try $0.listProjects(includeArchived: ia) }) { self.projects = $0 }
    }

    func createProject(
        name: String,
        notes: String = "",
        status: String = "active",
        color: String? = nil,
        completion: @escaping @MainActor (String?) -> Void
    ) {
        let trimmed = name.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let json = Self.encodeUpsert(ProjectUpsertRequest(
                  name: trimmed, notes: notes, status: status, color: color
              )) else {
            completion(nil)
            return
        }

        let core = self.core
        Task.detached {
            do {
                let project = try core.upsertProject(json: json)
                await MainActor.run {
                    self.reloadProjects()
                    completion(project.id)
                }
            } catch {
                await MainActor.run {
                    self.errorText = "Could not create project: \(error)"
                    completion(nil)
                }
            }
        }
    }

    func loadProjectDetail(id: String, completion: @escaping @MainActor (ProjectDetail?) -> Void) {
        let core = self.core
        Task.detached {
            let detail = try? core.getProject(id: id)
            await MainActor.run { completion(detail) }
        }
    }

    /// The project's contact links from the shared link model, keyed by
    /// contact id — feeds the "why linked" evidence surface on person chips.
    func loadProjectContactLinks(
        projectId: String,
        completion: @escaping @MainActor ([Int64: LinkRecord]) -> Void
    ) {
        let core = self.core
        Task.detached {
            let records = (try? core.linksFor(kind: "project", id: projectId)) ?? []
            let byContact = Dictionary(
                records
                    .filter { $0.relation == "involves" && $0.dstKind == "contact" }
                    .compactMap { record in Int64(record.dstId).map { ($0, record) } },
                uniquingKeysWith: { first, _ in first }
            )
            await MainActor.run { completion(byContact) }
        }
    }

    /// Encodable upsert payload; `archived == nil` is omitted from the JSON
    /// (JSONEncoder skips nil optionals), which the core treats as "unchanged".
    /// Internal + static so tests can verify the encoding (quote-safety etc.)
    /// without a live core handle.
    struct ProjectUpsertRequest: Encodable {
        var id: String?
        let name: String
        var notes: String = ""
        var status: String = "active"
        var color: String?
        var archived: Bool?
    }

    nonisolated static func encodeUpsert(_ req: ProjectUpsertRequest) -> String? {
        guard let data = try? CastellumCore.encoder.encode(req) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    func saveProjectFields(id: String, name: String, notes: String,
                           status: String, color: String?, archived: Bool? = nil,
                           completion: (@MainActor () -> Void)? = nil) {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        guard let json = Self.encodeUpsert(ProjectUpsertRequest(
            id: id, name: trimmed, notes: notes, status: status,
            color: color, archived: archived)) else { return }
        load({ try $0.upsertProject(json: json) }) { _ in
            self.reloadProjects()
            completion?()
        }
    }

    func setProjectStatus(_ id: String, status: String,
                          completion: (@MainActor () -> Void)? = nil) {
        // Fetch current values, then save with the new status.
        loadProjectDetail(id: id) { detail in
            guard let d = detail else { return }
            self.saveProjectFields(id: id, name: d.project.name, notes: d.project.notes,
                                   status: status, color: d.project.color,
                                   completion: completion)
        }
    }

    func setProjectColor(_ id: String, color: String?,
                         completion: (@MainActor () -> Void)? = nil) {
        loadProjectDetail(id: id) { detail in
            guard let d = detail else { return }
            self.saveProjectFields(id: id, name: d.project.name, notes: d.project.notes,
                                   status: d.project.status, color: color,
                                   completion: completion)
        }
    }

    func setProjectArchived(_ id: String, archived: Bool,
                            completion: (@MainActor () -> Void)? = nil) {
        // Fetch current values, then save all fields with the new archived flag.
        loadProjectDetail(id: id) { detail in
            guard let d = detail else { return }
            self.saveProjectFields(id: id, name: d.project.name, notes: d.project.notes,
                                   status: d.project.status, color: d.project.color,
                                   archived: archived, completion: completion)
        }
    }

    func deleteProject(
        _ id: String,
        completion: @escaping @MainActor (Bool) -> Void = { _ in }
    ) {
        let core = self.core
        let deleteTask = Task.detached { () -> String? in
            do {
                try core.deleteProject(id: id)
                return nil
            } catch {
                return String(describing: error)
            }
        }
        Task { [weak self] in
            let error = await deleteTask.value
            guard let self else { return }
            if let error {
                self.errorText = "Could not delete project: \(error)"
                completion(false)
            } else {
                self.reloadProjects()
                completion(true)
            }
        }
    }

    func moveProject(_ id: String, to status: String) {
        let position = Int64(projects.filter { $0.status == status }.count)
        let core = self.core
        Task.detached { [weak self] in
            try? core.moveProject(projectId: id, status: status, position: position)
            await MainActor.run { self?.reloadProjects() }
        }
    }

    func archiveProject(_ id: String) {
        setProjectArchived(id, archived: true)
    }

    func unlinkEmail(projectId: String, messageId: String,
                     completion: (@MainActor () -> Void)? = nil) {
        let core = self.core
        Task.detached { [weak self] in
            try? core.projectSetEmailLink(projectId: projectId, messageId: messageId,
                                          linked: false, autoContacts: false)
            await MainActor.run {
                self?.reloadProjects()
                completion?()
            }
        }
    }

    /// `completion` fires on the main actor AFTER the write lands, so views can
    /// re-fetch the project detail without racing the FFI call.
    func linkContact(projectId: String, contactId: Int64, linked: Bool,
                     completion: (@MainActor () -> Void)? = nil) {
        let core = self.core
        Task.detached { [weak self] in
            try? core.projectSetContactLink(projectId: projectId,
                                             contactId: contactId, linked: linked)
            await MainActor.run {
                self?.reloadProjects()
                completion?()
            }
        }
    }

    func unlinkContact(projectId: String, contactId: Int64,
                       completion: (@MainActor () -> Void)? = nil) {
        linkContact(projectId: projectId, contactId: contactId, linked: false,
                    completion: completion)
    }

    func confirmAutoContact(projectId: String, contactId: Int64,
                            completion: (@MainActor () -> Void)? = nil) {
        // Re-linking with origin "manual" by calling set_contact_link(linked: true).
        linkContact(projectId: projectId, contactId: contactId, linked: true,
                    completion: completion)
    }

    func assignMessagesToProject(projectId: String, linked: Bool, messageIds: [String]) {
        let core = self.core
        let visibleMessageId = selectedMessage?.id
        let writeTask = Task.detached { () -> [ProjectTag]? in
            for mid in messageIds {
                try? core.projectSetEmailLink(projectId: projectId, messageId: mid,
                                              linked: linked, autoContacts: linked)
            }
            if let visibleMessageId, messageIds.contains(visibleMessageId) {
                return (try? core.projectsForMessage(messageId: visibleMessageId)) ?? []
            }
            return nil
        }
        Task { [weak self] in
            let visibleTags = await writeTask.value
            guard let self else { return }
            self.reloadProjects()
            // Keep the subtle project capsules in the reading pane in sync with
            // the assignment the user just made. Without this, the cue appeared
            // only after selecting another message and coming back, which made
            // a successful assignment look ineffective.
            if self.selectedMessage?.id == visibleMessageId,
               let visibleTags {
                self.currentProjectTags = visibleTags
            }
        }
    }

    func createProjectAndAssign(name: String, messageIds: [String],
                                completion: @escaping @MainActor (String) -> Void) {
        createProject(name: name) { projectId in
            guard let projectId else { return }
            self.assignMessagesToProject(projectId: projectId, linked: true,
                                         messageIds: messageIds)
            completion(projectId)
        }
    }

    func projectsForMessage(_ messageId: String,
                            completion: @escaping @MainActor ([ProjectTag]) -> Void) {
        let core = self.core
        Task.detached {
            let tags = (try? core.projectsForMessage(messageId: messageId)) ?? []
            await MainActor.run { completion(tags) }
        }
    }

    /// Project ids that contain EVERY message in `messageIds` (set intersection).
    /// Used by the assign popover so a checkmark means "the whole selection is
    /// in this project". Empty input yields an empty set.
    func projectMembershipIntersection(for messageIds: [String],
                                       completion: @escaping @MainActor (Set<String>) -> Void) {
        let core = self.core
        Task.detached {
            var common: Set<String>?
            for mid in messageIds {
                let ids = Set(((try? core.projectsForMessage(messageId: mid)) ?? [])
                    .map(\.projectId))
                common = common.map { $0.intersection(ids) } ?? ids
                if common?.isEmpty == true { break } // can only shrink
            }
            let result = common ?? []
            await MainActor.run { completion(result) }
        }
    }

    /// Open a message by id in its own standalone window.
    func openStandaloneMessage(id: String) {
        load({ try $0.getMessage(id: id) }) { [weak self] detail in
            guard let detail else {
                // A link's evidencing message (or a project email row) can be
                // purged by sync after the reference was stored — say so
                // instead of silently doing nothing.
                self?.errorText = "That message is no longer available locally."
                return
            }
            guard let self else { return }
            // No mark-read here: opening a linked/evidencing message was never a read.
            let state = self.beginStandaloneMessage(detail: detail, markReadAfterBody: nil)
            StandaloneMessageWindow.show(
                state: state,
                title: detail.header.subject ?? "(no subject)"
            )
        }
    }

    // MARK: - People graph (slice 8c)

    /// Load the contact co-occurrence graph off-actor and return it via callback.
    /// `projectId: nil` returns the global graph (all contacts).
    func loadContactGraph(
        projectId: String?,
        maxNodes: Int64,
        completion: @escaping @MainActor (ContactGraph) -> Void
    ) {
        let core = self.core
        let pid = projectId
        Task.detached {
            let g = (try? core.contactGraph(projectId: pid, maxNodes: maxNodes))
                ?? ContactGraph(nodes: [], edges: [])
            await MainActor.run { completion(g) }
        }
    }
}

/// Verb names for write-failure notices ("<verb> failed: ...").
enum JunkVerbName {
    static func name(_ isJunk: Bool) -> String {
        isJunk ? String(localized: "Mark as Junk") : String(localized: "Not Junk")
    }
}
