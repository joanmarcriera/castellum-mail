// Castellum - Swift wrapper over the Rust core C ABI.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Each query calls the FFI (declared in Castellum-Bridging-Header.h), takes the
// returned JSON C-string, frees it, and decodes into the Codable DTOs. Calls may
// overlap on background tasks: each decode therefore uses its own JSONDecoder.
// `@unchecked Sendable` is limited to the opaque u64 handle; the Rust side
// serializes database access internally.

import Foundation

/// Result of `CastellumCore.pendingMigrationCount(profilePath:)` — how many
/// migrations this build knows about that the profile hasn't applied yet,
/// and whether any of them is heavy (see `castellum_db::is_heavy_migration`
/// on the Rust side). `appliedVersion` is the schema watermark the probe
/// observed (0 for a profile that doesn't exist yet or was never migrated)
/// — callers MUST check `appliedVersion > 0` before treating `count > 0` as
/// "show the migrating window": a brand-new profile also has every
/// migration "pending" (nothing applied yet), but there's nothing to
/// protect and `Database::open` creating+migrating it from empty is fast
/// enough to need no window.
struct PendingMigrations: Equatable {
    let count: Int
    let hasHeavy: Bool
    let appliedVersion: Int
}

final class CastellumCore: @unchecked Sendable {
    private let handle: UInt64

    init(profilePath: URL) throws {
        try FileManager.default.createDirectory(at: profilePath, withIntermediateDirectories: true)
        var h: UInt64 = 0
        let rc = profilePath.path.withCString { castellum_open_profile($0, &h) }
        // Same shape as `decodeJSON` below: a status code alone can't carry
        // the SchemaTooNew/MigrationFailed launch-refusal detail, so pull
        // `castellum_last_error()` (the core now sets it on this path) and
        // prefer it when present. `CastellumLaunchError.classify` is what
        // recognises those two cases from the `.message` text.
        if let err = CastellumError.from(status: rc) {
            throw CastellumError.lastReason().map(CastellumError.message) ?? err
        }
        guard h != 0 else { throw CastellumError.internalError }
        self.handle = h
    }

    deinit { _ = castellum_close_profile(handle) }

    /// Cheap, read-only probe: how many migrations are pending for the
    /// profile at `profilePath`, WITHOUT opening it for real (the tracker
    /// 2635's launch preflight). Static — no handle needed, mirrors
    /// `coreVersion`'s no-instance call shape. Throws the same classified
    /// `CastellumError.message` a `SchemaTooNew` refusal from `init` would
    /// (see `CastellumLaunchError.classify`), never a `MigrationFailed` one
    /// — this probe never runs a migration.
    static func pendingMigrationCount(profilePath: URL) throws -> PendingMigrations {
        var count: Int64 = 0
        var heavy: UInt8 = 0
        var appliedVersion: Int64 = 0
        let rc = profilePath.path.withCString {
            castellum_pending_migration_count($0, &count, &heavy, &appliedVersion)
        }
        if let err = CastellumError.from(status: rc) {
            throw CastellumError.lastReason().map(CastellumError.message) ?? err
        }
        return PendingMigrations(
            count: Int(count), hasHeavy: heavy != 0, appliedVersion: Int(appliedVersion))
    }

    var coreVersion: String {
        guard let c = castellum_version() else { return "unknown" }
        defer { castellum_string_free(c) }
        return String(cString: c)
    }

    /// Render Markdown to sanitized HTML for the compose live preview
    /// (S46). Mirrors `coreVersion`'s no-handle-needed call shape — this is
    /// the SAME render that ends up in a sent Draft's `bodyHtml` when
    /// Markdown is on, so the preview never lies about what will be sent.
    func renderMarkdownPreview(_ markdown: String) -> String {
        guard let c = markdown.withCString({ castellum_render_markdown_preview($0) }) else {
            return ""
        }
        defer { castellum_string_free(c) }
        return String(cString: c)
    }

    /// Themed Markdown render (MM-5): the compose preview AND the sent
    /// `text/html` part, so both always agree.
    func renderMarkdown(_ markdown: String, theme: MarkdownThemeChoice) -> String {
        guard let c = markdown.withCString({ md in
            theme.rawValue.withCString { castellum_render_markdown_themed(md, $0) }
        }) else { return "" }
        defer { castellum_string_free(c) }
        return String(cString: c)
    }

    /// The compose render for a DRAFT (MM-5b): as `renderMarkdown(_:theme:)`, but an image whose
    /// URL is `cid:` + the Content-ID of one of the draft's inline attachments renders as an
    /// `<img>`. With no draft (not saved yet), or if the call fails, it is exactly the stateless
    /// render, which drops every image. The compose preview and the sent HTML part both use this.
    func renderMarkdown(_ markdown: String, theme: MarkdownThemeChoice, draftId: Int64?) -> String {
        guard let draftId else { return renderMarkdown(markdown, theme: theme) }
        var out: UnsafeMutablePointer<CChar>?
        let rc = markdown.withCString { md in
            theme.rawValue.withCString { castellum_render_markdown_for_draft(handle, draftId, md, $0, &out) }
        }
        guard rc == 0, let out else { return renderMarkdown(markdown, theme: theme) }
        defer { castellum_string_free(out) }
        return String(cString: out)
    }

    func schemaVersion() throws -> Int64 {
        var v: Int64 = 0
        let rc = castellum_profile_schema_version(handle, &v)
        if let err = CastellumError.from(status: rc) { throw err }
        return v
    }

    // MARK: - JSON query helper

    /// Run an FFI call that writes a JSON string to an out-param, then decode it.
    private func decodeJSON<T: Decodable>(
        _ type: T.Type,
        _ call: (UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>) -> Int32
    ) throws -> T {
        var out: UnsafeMutablePointer<CChar>?
        let rc = call(&out)
        if let err = CastellumError.from(status: rc) {
            throw CastellumError.lastReason().map(CastellumError.message) ?? err
        }
        guard let out else { throw CastellumError.internalError }
        defer { castellum_string_free(out) }
        let json = Data(bytes: out, count: strlen(out))
        do {
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            return try decoder.decode(T.self, from: json)
        } catch {
            throw CastellumError.decode(String(describing: error))
        }
    }

    func listAccounts() throws -> [Account] {
        try decodeJSON([Account].self) { castellum_list_accounts(handle, $0) }
    }

    /// Permanently delete an account and every local mail record it owns
    /// (mailboxes, messages, sync cursors, queued operations, drafts). Never
    /// touches the Keychain — the caller must also call `AccountStore.delete`.
    func removeAccount(accountId: String) throws {
        try run { accountId.withCString { castellum_remove_account(handle, $0) } }
    }

    /// What an unforced reset would refuse on — read this first to word the
    /// confirmation dialog.
    func resetAccountBlockers(accountId: String) throws -> ResetAccountBlockers {
        try decodeJSON(ResetAccountBlockers.self) { out in
            accountId.withCString { castellum_reset_account_blockers(handle, $0, out) }
        }
    }

    /// Wipe an account's local mail state (mailboxes, messages, bodies, FTS,
    /// sync cursors, AI artefacts) — keeps its row, Keychain credentials
    /// (untouched here — the caller never needs to touch AccountStore),
    /// write policy, signature. Throws (matched via
    /// `AccountResetPolicy.isResetBlockedRefusal`) while an outbox draft or
    /// queued operation is still un-replayed, unless `force` is true.
    func resetAccount(accountId: String, force: Bool) throws {
        try run { accountId.withCString { castellum_reset_account(handle, $0, force ? 1 : 0) } }
    }

    /// What an unforced `applyMailboxSelection` would refuse on (the tracker
    /// #2617 follow-up review) — read this first to word the confirmation,
    /// exactly like `resetAccountBlockers`.
    func applyMailboxSelectionBlockers(accountId: String, keepPathsJSON: String) throws -> [MailboxSelectionBlocker] {
        try decodeJSON([MailboxSelectionBlocker].self) { out in
            accountId.withCString { a in
                keepPathsJSON.withCString { k in
                    castellum_apply_mailbox_selection_blockers(handle, a, k, out)
                }
            }
        }
    }

    /// Convenience over `applyMailboxSelectionBlockers(accountId:keepPathsJSON:)`
    /// for callers that already have the resolved path list as a Swift array.
    func applyMailboxSelectionBlockers(accountId: String, keepPaths: [String]) throws -> [MailboxSelectionBlocker] {
        let json = String(data: try JSONEncoder().encode(keepPaths), encoding: .utf8) ?? "[]"
        return try applyMailboxSelectionBlockers(accountId: accountId, keepPathsJSON: json)
    }

    /// Apply an edited folder selection to an ALREADY-ADDED account (the tracker
    /// #2617, Settings > Accounts > Folders…) — never the S62 first-sync
    /// picker. `keepPathsJSON` is a JSON array of the paths still selected
    /// (must not be empty); every other locally known mailbox for
    /// `accountId` is removed right away (messages cascade) UNLESS a
    /// non-terminal operation still touches it, in which case the call
    /// throws (matched via `MailboxSelectionPolicy.isRemovalBlockedRefusal`)
    /// and nothing changes — call `applyMailboxSelectionBlockers` first to
    /// word that refusal, then pass `force: true` only on explicit user
    /// confirmation. Never touches the server or the Keychain.
    func applyMailboxSelection(accountId: String, keepPathsJSON: String, force: Bool) throws -> MailboxSelectionResult {
        try decodeJSON(MailboxSelectionResult.self) { out in
            accountId.withCString { a in
                keepPathsJSON.withCString { k in
                    castellum_apply_mailbox_selection(handle, a, k, force ? 1 : 0, out)
                }
            }
        }
    }

    /// Convenience over `applyMailboxSelection(accountId:keepPathsJSON:force:)`
    /// for callers that already have the resolved path list as a Swift array
    /// (`SyncScope.resolvedKeepPaths`) rather than pre-encoded JSON.
    func applyMailboxSelection(accountId: String, keepPaths: [String], force: Bool) throws -> Int {
        let json = String(data: try JSONEncoder().encode(keepPaths), encoding: .utf8) ?? "[]"
        return try Int(applyMailboxSelection(accountId: accountId, keepPathsJSON: json, force: force).removed)
    }

    func listMailboxes(accountId: String) throws -> [Mailbox] {
        try decodeJSON([Mailbox].self) { out in
            accountId.withCString { castellum_list_mailboxes(handle, $0, out) }
        }
    }

    func mailboxCounts(accountId: String) throws -> [MailboxCounts] {
        try decodeJSON([MailboxCounts].self) { out in
            accountId.withCString { castellum_mailbox_counts(handle, $0, out) }
        }
    }

    /// Per-mailbox hydrated/total body counts (#3631 G5).
    func bodyHydrationProgress(accountId: String) throws -> [BodyHydrationProgress] {
        try decodeJSON([BodyHydrationProgress].self) { out in
            accountId.withCString { castellum_body_hydration_progress(handle, $0, out) }
        }
    }

    /// Body-hydration coverage of a search scope (#3633 G13): nil `accountId`
    /// = every account, nil `mailboxId` = the whole account.
    /// `query` is the search text: the core uses only its `in:`/`folder:`/`path:`
    /// filters, so a search that excludes Trash is not told about Trash bodies.
    func bodyHydrationScope(accountId: String?, mailboxId: String?, query: String? = nil) throws -> BodyHydrationScope {
        try decodeJSON(BodyHydrationScope.self) { out in
            withOptionalCString(accountId) { a in
                withOptionalCString(mailboxId) { m in
                    withOptionalCString(query) { q in
                        castellum_body_hydration_scope(handle, a, m, q, out)
                    }
                }
            }
        }
    }

    func listMessages(accountId: String, mailboxId: String, limit: Int64, offset: Int64) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            accountId.withCString { a in
                mailboxId.withCString { m in
                    castellum_list_messages(handle, a, m, limit, offset, out)
                }
            }
        }
    }

    /// `s?.withCString`, but calling `body(nil)` for `nil` rather than
    /// skipping the call — the FFI side reads a null `cursor_json` as
    /// "first page", not an error.
    private func withOptionalCString<R>(_ s: String?, _ body: (UnsafePointer<CChar>?) -> R) -> R {
        if let s { return s.withCString(body) }
        return body(nil)
    }

    /// JSON-encode `cursor` with the shared snake_case coder, or `nil` for
    /// the first page — the exact string `castellum_list_messages_page`/
    /// `_by_role_page` expect in `cursor_json`.
    private func cursorJSON(_ cursor: ListCursor?) throws -> String? {
        guard let cursor else { return nil }
        let data = try Self.encoder.encode(cursor)
        guard let json = String(data: data, encoding: .utf8) else { throw CastellumError.internalError }
        return json
    }

    /// Keyset (cursor) page of one mailbox's message headers, newest first
    /// (S64). `after` is `nil` for the first page, or the `next` cursor from
    /// the previous `MessagePage` — same ordering/snooze-exclusion as
    /// `listMessages(accountId:mailboxId:limit:offset:)`, but its cost does
    /// not grow with how deep the page is.
    func listMessagesPage(
        accountId: String, mailboxId: String, sort: MessageSort = .default,
        after cursor: ListCursor?, limit: Int64
    ) throws -> MessagePage {
        let cursor = try cursorJSON(cursor)
        return try decodeJSON(MessagePage.self) { out in
            accountId.withCString { a in
                mailboxId.withCString { m in
                    sort.key.rawValue.withCString { k in
                        withOptionalCString(cursor) { c in
                            castellum_list_messages_page_sorted(
                                handle, a, m, k, sort.ascending ? 1 : 0, c, limit, out)
                        }
                    }
                }
            }
        }
    }

    /// A full message, or `nil` if not found (the core returns JSON `null`).
    func getMessage(id: String) throws -> MessageDetail? {
        try decodeJSON(MessageDetail?.self) { out in
            id.withCString { castellum_get_message(handle, $0, out) }
        }
    }

    // MARK: - Account auto-detect (MM-14)

    /// Look up IMAP/SMTP settings for a mail DOMAIN (never an address): DNS SRV,
    /// MX hint, the domain's own autoconfig, Mozilla ISPDB. Stateless -- no
    /// profile handle, nothing persisted. BLOCKING network I/O bounded by
    /// `timeoutMs` (the core clamps it to 1...15 s): call from a detached task,
    /// never the main actor. A lookup that finds nothing or is offline is a
    /// normal report (`not_found` / `offline`), not a thrown error.
    static func autoconfigLookup(domain: String, sources: [String]? = nil, timeoutMs: Int? = nil) throws -> AutoconfigReport {
        var body: [String: Any] = ["domain": domain]
        if let sources { body["sources"] = sources }
        if let timeoutMs { body["timeout_ms"] = timeoutMs }
        let data = try JSONSerialization.data(withJSONObject: body)
        let request = String(decoding: data, as: UTF8.self)
        var out: UnsafeMutablePointer<CChar>?
        let rc = request.withCString { castellum_autoconfig_lookup($0, &out) }
        if let err = CastellumError.from(status: rc) { throw err }
        guard let out else { throw CastellumError.internalError }
        defer { castellum_string_free(out) }
        let json = Data(bytes: out, count: strlen(out))
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(AutoconfigReport.self, from: json)
        } catch {
            throw CastellumError.decode(String(describing: error))
        }
    }

    // MARK: - Trust surface (S48)

    /// Analyse one link's (href, visible text) pair. Stateless — no handle
    /// required, callable even with no profile open.
    static func inspectLink(href: String, visibleText: String) throws -> LinkInspection {
        var out: UnsafeMutablePointer<CChar>?
        let rc = href.withCString { h in
            visibleText.withCString { t in
                castellum_inspect_link(h, t, &out)
            }
        }
        if let err = CastellumError.from(status: rc) { throw err }
        guard let out else { throw CastellumError.internalError }
        defer { castellum_string_free(out) }
        let json = Data(bytes: out, count: strlen(out))
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(LinkInspection.self, from: json)
        } catch {
            throw CastellumError.decode(String(describing: error))
        }
    }

    /// What remote content a message's sanitized HTML tries to load, and how
    /// much of it looks like tracking (#2812). Pure and stateless: no profile,
    /// no network.
    static func remoteContentReport(sanitizedHtml: String) throws -> RemoteContentReport {
        var out: UnsafeMutablePointer<CChar>?
        let rc = sanitizedHtml.withCString { castellum_remote_content_report($0, &out) }
        if let err = CastellumError.from(status: rc) { throw err }
        guard let out else { throw CastellumError.internalError }
        defer { castellum_string_free(out) }
        let json = Data(bytes: out, count: strlen(out))
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        do {
            return try decoder.decode(RemoteContentReport.self, from: json)
        } catch {
            throw CastellumError.decode(String(describing: error))
        }
    }

    /// Remember (or update) whether `fromAddress` may load remote content
    /// automatically for `accountId`, without a per-message prompt.
    func setSenderRemoteContentAllowed(accountId: String, fromAddress: String, allowed: Bool) throws {
        try run {
            accountId.withCString { a in
                fromAddress.withCString { f in
                    castellum_set_sender_remote_content_allowed(handle, a, f, allowed ? 1 : 0)
                }
            }
        }
    }

    /// `nil` when no remembered policy exists for this sender.
    func senderRemoteContentAllowed(accountId: String, fromAddress: String) throws -> Bool? {
        try decodeJSON(Bool?.self) { out in
            accountId.withCString { a in
                fromAddress.withCString { f in
                    castellum_sender_remote_content_allowed(handle, a, f, out)
                }
            }
        }
    }

    /// Every remembered sender policy across every account, for the Settings
    /// review list.
    func listSenderRemoteContentPolicies() throws -> [SenderRemoteContentPolicy] {
        try decodeJSON([SenderRemoteContentPolicy].self) { out in
            castellum_list_sender_remote_content_policies(handle, out)
        }
    }

    /// Forget a remembered sender policy (Settings' Revoke action).
    func revokeSenderRemoteContent(accountId: String, fromAddress: String) throws {
        try run {
            accountId.withCString { a in
                fromAddress.withCString { f in
                    castellum_revoke_sender_remote_content(handle, a, f)
                }
            }
        }
    }

    // MARK: - List-Unsubscribe (S44)

    struct UnsubscribeResult: Decodable {
        /// "sent" | "unavailable".
        let status: String
        let httpStatus: Int?
    }

    /// Perform the RFC 8058 one-click unsubscribe POST for `messageId`. Call
    /// ONLY after the user has confirmed against the exact destination shown
    /// in a confirmation sheet — this makes no confirmation of its own.
    func unsubscribeOneClick(messageId: String) throws -> UnsubscribeResult {
        try decodeJSON(UnsubscribeResult.self) { out in
            messageId.withCString { castellum_unsubscribe_one_click(handle, $0, out) }
        }
    }

    /// Search within one account. `query` accepts the S39 search grammar
    /// (`from:`/`to:`/`cc:`/`subject:`/`has:attachment`/`before:`/`after:`/
    /// `in:`/`is:read|unread|flagged|starred`, `-` negation, quoted phrases)
    /// as well as bare keywords — unchanged signature/behaviour otherwise.
    func search(accountId: String, query: String, limit: Int64) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            accountId.withCString { a in
                query.withCString { q in
                    castellum_search_messages(handle, a, q, limit, out)
                }
            }
        }
    }

    func listThreads(accountId: String, limit: Int64, offset: Int64) throws -> [ThreadSummary] {
        try decodeJSON([ThreadSummary].self) { out in
            accountId.withCString { castellum_list_threads(handle, $0, limit, offset, out) }
        }
    }

    func getThread(id: String) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            id.withCString { castellum_get_thread(handle, $0, out) }
        }
    }

    // MARK: - unified (cross-account) reads (S34)

    /// Message headers across every account whose mailbox has `role` (e.g.
    /// "inbox"), newest first, paginated — the "All Inboxes" sidebar group.
    func listMessages(byRole role: String, limit: Int64, offset: Int64) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            role.withCString { castellum_list_messages_by_role(handle, $0, limit, offset, out) }
        }
    }

    /// Same shape as `listMessagesPage(accountId:mailboxId:after:limit:)`,
    /// scoped by mailbox role across every account (the unified list).
    func listMessagesPage(
        byRole role: String, sort: MessageSort = .default, after cursor: ListCursor?, limit: Int64
    ) throws -> MessagePage {
        let cursor = try cursorJSON(cursor)
        return try decodeJSON(MessagePage.self) { out in
            role.withCString { r in
                sort.key.rawValue.withCString { k in
                    withOptionalCString(cursor) { c in
                        castellum_list_messages_by_role_page_sorted(
                            handle, r, k, sort.ascending ? 1 : 0, c, limit, out)
                    }
                }
            }
        }
    }

    /// Thread summaries across every account (newest activity first,
    /// paginated). Not yet wired to a view.
    func listThreadsAcrossAccounts(limit: Int64, offset: Int64) throws -> [ThreadSummary] {
        try decodeJSON([ThreadSummary].self) { out in
            castellum_list_threads_across_accounts(handle, limit, offset, out)
        }
    }

    /// Search across every account. Same S39 grammar as `search(accountId:query:limit:)`.
    func searchAcrossAccounts(query: String, limit: Int64) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            query.withCString { castellum_search_messages_across_accounts(handle, $0, limit, out) }
        }
    }

    func syncAccount(configJSON: String) throws -> SyncReport {
        try decodeJSON(SyncReport.self) { out in
            configJSON.withCString { castellum_sync_account(handle, $0, out) }
        }
    }

    /// Start (or restart) an account's supervised IMAP IDLE connection.
    /// `configJSON` is the same shape `syncAccount` takes. Returns once the
    /// background thread has been spawned, not once it has connected.
    func startIdle(configJSON: String) throws {
        try run { configJSON.withCString { castellum_idle_start(handle, $0) } }
    }

    /// Stop an account's IDLE thread if one is running. Blocks until it has
    /// fully exited (bounded by the core's own backoff — never indefinite).
    func stopIdle(accountId: String) throws {
        try run { accountId.withCString { castellum_idle_stop(handle, $0) } }
    }

    /// Wake an account's IDLE thread now — call after the Mac wakes from
    /// sleep or the network path changes. A no-op if IDLE isn't running for
    /// that account.
    func nudgeIdle(accountId: String) throws {
        try run { accountId.withCString { castellum_idle_nudge(handle, $0) } }
    }

    /// Whether an account currently has a running IDLE thread.
    func isIdleRunning(accountId: String) throws -> Bool {
        var out: Int32 = 0
        let rc = accountId.withCString { castellum_idle_running(handle, $0, &out) }
        if let err = CastellumError.from(status: rc) { throw err }
        return out != 0
    }

    /// Count of finished IDLE-thread resync passes this process. A pass can
    /// rewrite rows with no new arrival, so callers poll this to know when the
    /// open list is stale (#3040).
    func idlePassEpoch() throws -> UInt64 {
        var out: UInt64 = 0
        let rc = castellum_idle_pass_epoch(handle, &out)
        if let err = CastellumError.from(status: rc) { throw err }
        return out
    }

    /// A best-effort snapshot of `accountId`'s current (or most recently
    /// finished) sync pass, or `nil` if no sync has run for it yet this
    /// process. Cheap and local-only — safe to poll from a timer alongside a
    /// long-running `syncAccount` call on another thread.
    func syncProgress(accountId: String) throws -> SyncProgress? {
        try decodeJSON(SyncProgress?.self) { out in
            accountId.withCString { castellum_sync_progress(handle, $0, out) }
        }
    }

    /// Account-setup preflight (S62): verify IMAP (connect, LOGIN, LIST) and
    /// SMTP (connect, AUTH) for `configJSON` (`AccountConfig.connectionTestJSON()`)
    /// WITHOUT saving anything. A connection failure comes back inside the
    /// report; only a malformed request throws. Blocking network I/O — call
    /// off the main actor.
    func testConnection(configJSON: String) throws -> ConnectionTestReport {
        try decodeJSON(ConnectionTestReport.self) { out in
            configJSON.withCString { castellum_test_connection(handle, $0, out) }
        }
    }

    /// Persist an account's identity (id/email, parsed from `configJSON` —
    /// same shape `syncAccount` takes) WITHOUT syncing: the "Save anyway"
    /// path for a connection check that failed. Makes the account visible to
    /// `listAccounts()`; never touches the network.
    func saveAccountWithoutSync(configJSON: String) throws {
        try run { configJSON.withCString { castellum_save_account_without_sync(handle, $0) } }
    }

    /// Hydrate a descriptor-only message body on demand. Unlike `getMessage`,
    /// this may perform blocking IMAP network I/O and should be called off-actor.
    func hydrateMessageBody(configJSON: String, messageId: String) throws -> MessageDetail? {
        try decodeJSON(MessageDetail?.self) { out in
            configJSON.withCString { cfg in
                messageId.withCString { mid in
                    castellum_hydrate_message_body(handle, cfg, mid, out)
                }
            }
        }
    }

    /// Run one bounded background body-hydration pass over the account's
    /// durable missing-body queue. Blocking network I/O (call off-actor); an
    /// empty queue returns immediately without a connection.
    func hydratePendingBodies(configJSON: String) throws -> HydrationReport {
        try decodeJSON(HydrationReport.self) { out in
            configJSON.withCString { castellum_hydrate_pending_bodies(handle, $0, out) }
        }
    }

    /// Record an Accept/Decline/Tentative response (S55) to `messageId`'s
    /// calendar invite and queue an iTIP REPLY to its organizer through the
    /// normal outbox — call `sendPending`/rely on the usual drain afterwards
    /// to actually deliver it, same as any other queued send. `response`
    /// must be `"accepted"`, `"declined"` or `"tentative"`. Returns the
    /// queued draft's id. Fails with a "recipient mismatch: <address>" message
    /// (see `CalendarInviteRecipientPolicy`) when the ORGANIZER matches
    /// neither From nor Reply-To and `confirmedRecipient` is not that exact
    /// address. `confirmedRecipient` is the address the confirmation prompt
    /// showed the user (nil when nothing was confirmed): the core refuses,
    /// with the same mismatch message naming the current address, whenever it
    /// differs from where the reply would actually go (#3472). Local-only.
    func respondToCalendarInvite(
        messageId: String, response: String, confirmedRecipient: String? = nil
    ) throws -> Int64 {
        struct Result: Decodable { let draftId: Int64 }
        let result = try decodeJSON(Result.self) { out in
            messageId.withCString { mid in
                response.withCString { resp in
                    withOptionalCString(confirmedRecipient) { confirmed in
                        castellum_respond_to_calendar_invite(handle, mid, resp, confirmed, out)
                    }
                }
            }
        }
        return result.draftId
    }

    /// Local-only: is this attachment already cached, and where. Never opens
    /// a network connection — cheap enough to call on every selection.
    func attachmentCacheStatus(messageId: String, ordinal: Int64) throws -> AttachmentDownloadResult {
        try decodeJSON(AttachmentDownloadResult.self) { out in
            messageId.withCString { castellum_attachment_cache_status(handle, $0, ordinal, out) }
        }
    }

    /// Download one incoming attachment part over IMAP and cache it locally.
    /// Blocking network I/O unless already cached (call off-actor).
    func downloadAttachment(configJSON: String, messageId: String, ordinal: Int64) throws -> AttachmentDownloadResult {
        try decodeJSON(AttachmentDownloadResult.self) { out in
            configJSON.withCString { cfg in
                messageId.withCString { mid in
                    castellum_download_attachment(handle, cfg, mid, ordinal, out)
                }
            }
        }
    }

    func listDraftAttachments(draftId: Int64) throws -> [DraftAttachmentMeta] {
        try decodeJSON([DraftAttachmentMeta].self) { out in
            castellum_list_draft_attachments(handle, draftId, out)
        }
    }

    func addDraftAttachment(draftId: Int64, attachment: NewDraftAttachment) throws -> [DraftAttachmentMeta] {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = String(data: try encoder.encode(attachment), encoding: .utf8) ?? "{}"
        return try decodeJSON([DraftAttachmentMeta].self) { out in
            json.withCString { castellum_add_draft_attachment(handle, draftId, $0, out) }
        }
    }

    /// Embed one image in a draft as an inline part (MM-5b). Throws `CastellumError.message` whose
    /// text starts `not_an_image:` or `too_large:` when the core refuses (nothing is written).
    func embedDraftImage(draftId: Int64, image: NewInlineImage) throws -> EmbeddedImage {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        let json = String(data: try encoder.encode(image), encoding: .utf8) ?? "{}"
        return try decodeJSON(EmbeddedImage.self) { out in
            json.withCString { castellum_embed_draft_image(handle, draftId, $0, out) }
        }
    }

    /// Make a draft attachment inline (`inline == true`) or a regular attachment (MM-5b slice 3).
    /// Returns the refreshed list and, when it became inline, the Markdown reference to insert.
    /// Throws (`not_an_image:` / `too_large:` in the message) when the core refuses; nothing changes then.
    func setDraftAttachmentInline(
        draftId: Int64, attachmentId: Int64, inline: Bool
    ) throws -> (attachments: [DraftAttachmentMeta], markdown: String?) {
        struct Payload: Decodable { let attachments: [DraftAttachmentMeta]; let markdown: String? }
        let payload = try decodeJSON(Payload.self) { out in
            castellum_set_draft_attachment_inline(handle, draftId, attachmentId, inline ? 1 : 0, out)
        }
        return (payload.attachments, payload.markdown)
    }

    /// Bytes and raster MIME type of a draft's inline image by Content-ID (the compose preview);
    /// nil when it is missing or not a PNG, JPEG, GIF or WebP.
    func draftInlineImage(draftId: Int64, contentId: String) -> (data: Data, mimeType: String)? {
        struct Payload: Decodable { let contentType: String; let dataBase64: String }
        guard let payload = try? decodeJSON(Payload.self, { out in
            contentId.withCString { castellum_draft_inline_image(handle, draftId, $0, out) }
        }), let data = Data(base64Encoded: payload.dataBase64) else { return nil }
        return (data, payload.contentType)
    }

    func removeDraftAttachment(draftId: Int64, attachmentId: Int64) throws -> [DraftAttachmentMeta] {
        try decodeJSON([DraftAttachmentMeta].self) { out in
            castellum_remove_draft_attachment(handle, draftId, attachmentId, out)
        }
    }

    /// Copies every attachment of `messageId` into `draftId` (Forward),
    /// downloading any not already cached. Blocking network I/O: call
    /// off-actor.
    func forwardAttachments(configJSON: String, messageId: String, draftId: Int64) throws -> [DraftAttachmentMeta] {
        try decodeJSON([DraftAttachmentMeta].self) { out in
            configJSON.withCString { cfg in
                messageId.withCString { mid in
                    castellum_forward_attachments(handle, cfg, mid, draftId, out)
                }
            }
        }
    }

    /// Local-only: is this message's raw RFC822 source already cached, and
    /// where. Never opens a network connection.
    func rawSourceCacheStatus(messageId: String) throws -> AttachmentDownloadResult {
        try decodeJSON(AttachmentDownloadResult.self) { out in
            messageId.withCString { mid in
                castellum_raw_source_cache_status(handle, mid, out)
            }
        }
    }

    /// Fetch one message's full raw RFC822 source over IMAP and cache it.
    /// Blocking network I/O unless already cached. Reuses
    /// `AttachmentDownloadResult` — the JSON shape is identical
    /// (status/local_path/error).
    func fetchRawSource(configJSON: String, messageId: String) throws -> AttachmentDownloadResult {
        try decodeJSON(AttachmentDownloadResult.self) { out in
            configJSON.withCString { cfg in
                messageId.withCString { mid in
                    castellum_fetch_raw_source(handle, cfg, mid, out)
                }
            }
        }
    }

    /// Read, parse and queue a local `.eml` file for import into
    /// `mailboxId` on `accountId`. No network I/O — the import replays via
    /// the durable operation queue.
    func importEml(accountId: String, mailboxId: String, filePath: String) throws -> ImportEmlResult {
        try decodeJSON(ImportEmlResult.self) { out in
            accountId.withCString { acct in
                mailboxId.withCString { mbox in
                    filePath.withCString { path in
                        castellum_import_eml(handle, acct, mbox, path, out)
                    }
                }
            }
        }
    }

    /// Generate the allowlisted, redacted support bundle. Strictly read-only:
    /// no database writes, no network, and never credentials, message
    /// content, folder names, or raw error text.
    func supportBundle() throws -> SupportBundle {
        try decodeJSON(SupportBundle.self) { out in
            castellum_support_bundle(handle, out)
        }
    }

    /// Fetch the LOCAL-ONLY diagnostics view: real account emails and mailbox
    /// paths, with error text still held to fixed redacted phrases.
    /// MUST NEVER be written to an exported support bundle or sent off-device.
    func localDiagnostics() throws -> LocalDiagnostics {
        try decodeJSON(LocalDiagnostics.self) { out in
            castellum_local_diagnostics(handle, out)
        }
    }

    /// Run the read-only SQLite integrity check (`PRAGMA quick_check`).
    /// Never modifies the profile.
    func integrityCheck() throws -> IntegrityReport {
        try decodeJSON(IntegrityReport.self) { out in
            castellum_integrity_check(handle, out)
        }
    }

    /// Best-effort redaction of free text (crash reports, unified-log
    /// excerpts) for diagnostics export. NOT the allowlist `supportBundle()`
    /// uses. Needs no open profile — provided as an instance method only so
    /// call sites stay consistent (`core.redactDiagnosticText(...)`).
    func redactDiagnosticText(_ text: String) throws -> String {
        var out: UnsafeMutablePointer<CChar>?
        let rc = text.withCString { castellum_redact_diagnostic_text($0, &out) }
        if let err = CastellumError.from(status: rc) { throw err }
        guard let out else { throw CastellumError.internalError }
        defer { castellum_string_free(out) }
        return String(cString: out)
    }

    /// Last-run sync KPI snapshot for this profile (throughput, per-phase
    /// IMAP/DB/core timings) — `nil` when no sync has completed yet, which is
    /// a normal state (a fresh profile, or every account has only ever
    /// errored before syncing), never a failure.
    ///
    /// `castellum_last_sync_kpi` signals that "no snapshot yet" state by
    /// still returning `CASTELLUM_OK` while writing the literal JSON string
    /// `"null"` to the out-param (see its doc comment in castellum.h) — there
    /// is no precedent elsewhere in this file for a "success but null" FFI
    /// shape. Rather than invent a new call convention for it, this reuses
    /// `decodeJSON` unchanged and simply asks it to decode into
    /// `LastSyncKpi?` instead of `LastSyncKpi`: `Optional<Wrapped>` is itself
    /// `Decodable` whenever `Wrapped` is, so `JSONDecoder` turns a top-level
    /// JSON `null` into Swift `nil` with no special-casing needed here.
    func lastSyncKpi() throws -> LastSyncKpi? {
        try decodeJSON(LastSyncKpi?.self) { out in
            castellum_last_sync_kpi(handle, out)
        }
    }

    /// Directory this profile's Rust-side structured log files live in
    /// (daily-rotating, INFO+, capped at 14 files — see `castellum-ffi`'s
    /// `castellum_log_directory` and `native/crates/castellum-core/src/logging.rs`),
    /// or `nil` for an invalid handle.
    ///
    /// Unlike every other call in this file, `castellum_log_directory` is a
    /// DIRECT string return, not the usual out-param + `CastellumStatus`
    /// pattern — there is no failure mode here worth a status code beyond
    /// "invalid handle", which the C side already collapses to a `NULL`
    /// return, so no `decodeJSON`/`run`-style helper applies. Ownership is
    /// the same as every other returned string: newly allocated by Rust,
    /// freed here with `castellum_string_free`. The directory itself may not
    /// exist yet, or may exist but be empty, if nothing has logged at INFO+
    /// in this process — diagnostics callers must treat both as normal, not
    /// as an error.
    func logDirectory() -> URL? {
        guard let c = castellum_log_directory(handle) else { return nil }
        defer { castellum_string_free(c) }
        return URL(fileURLWithPath: String(cString: c))
    }

    // MARK: - work items (Active Work backend, migration 0018)

    /// Create or update a work item; returns the stored row. `requestJSON`:
    /// {id?, project_id?, title, notes?, state?, position?, due_at?,
    /// delegated_to_contact_id?}.
    func upsertWorkItem(requestJSON: String) throws -> WorkItem {
        try decodeJSON(WorkItem.self) { out in
            requestJSON.withCString { castellum_upsert_work_item(handle, $0, out) }
        }
    }

    /// The project's work items in deterministic order.
    func listWorkItems(projectId: String) throws -> [WorkItem] {
        try decodeJSON([WorkItem].self) { out in
            projectId.withCString { castellum_list_work_items(handle, $0, out) }
        }
    }

    /// Work items matching a filter, ordered by due date (undated last), then
    /// position, then id. `filterJSON` keys, all optional: project_id, states
    /// (array), due_before, due_after (epoch seconds), overdue, contact_id,
    /// linked_message_id, linked_thread_id, unassigned. Unknown keys throw.
    func listWorkItemsFiltered(filterJSON: String) throws -> [WorkItem] {
        try decodeJSON([WorkItem].self) { out in
            filterJSON.withCString { castellum_list_work_items_filtered(handle, $0, out) }
        }
    }

    /// Reverse lookup: the work items linked to a message, thread or contact
    /// (`kind` is "message", "thread" or "contact"; a contact also matches
    /// items delegated to it). `id` is the message/thread id or the contact's
    /// numeric id as text.
    func workItems(forKind kind: String, id: String) throws -> [WorkItem] {
        let request = (try? JSONSerialization.data(withJSONObject: ["kind": kind, "id": id]))
            .flatMap { String(data: $0, encoding: .utf8) } ?? "{}"
        return try decodeJSON([WorkItem].self) { out in
            request.withCString { castellum_work_items_for(handle, $0, out) }
        }
    }

    /// Permanently delete a work item (its shared-link rows cascade).
    func deleteWorkItem(id: String) throws {
        try run { id.withCString { castellum_delete_work_item(handle, $0) } }
    }

    // MARK: - suggestions you accept (migration 0046, AW2)

    /// Suggestions, newest first. `filterJSON` keys, all optional: status
    /// (proposed|accepted|dismissed), kind (work_item|waiting_for_reply|
    /// state_change), thread_id, project_id, limit. An empty string lists
    /// everything; unknown keys throw.
    func listSuggestions(filterJSON: String = "") throws -> [Suggestion] {
        try decodeJSON([Suggestion].self) { out in
            filterJSON.withCString { castellum_list_suggestions(handle, $0, out) }
        }
    }

    /// Accept a suggestion: creates the work item (or applies the state
    /// change) and links it to the evidence, atomically. Idempotent;
    /// accepting a dismissed suggestion throws.
    func acceptSuggestion(id: String) throws -> SuggestionAccepted {
        try decodeJSON(SuggestionAccepted.self) { out in
            id.withCString { castellum_accept_suggestion(handle, $0, out) }
        }
    }

    /// Dismiss a suggestion (idempotent; it is never proposed again).
    /// Dismissing an accepted one throws.
    func dismissSuggestion(id: String) throws -> Suggestion {
        try decodeJSON(Suggestion.self) { out in
            id.withCString { castellum_dismiss_suggestion(handle, $0, out) }
        }
    }

    /// How many suggestions still await a decision (for a badge).
    func countSuggestions() throws -> Int64 {
        try decodeJSON(SuggestionCount.self) { out in
            castellum_count_suggestions(handle, out)
        }.proposed
    }

    // MARK: - saved Active Work views (migration 0047)

    /// Saved views in display order; the four built-ins are seeded on first
    /// use. Hidden views only when `includeHidden`.
    func listSavedViews(includeHidden: Bool = false) throws -> [SavedView] {
        try decodeJSON([SavedView].self) { out in
            castellum_list_saved_views(handle, includeHidden ? 1 : 0, out)
        }
    }

    /// Create a custom view. `filterJSON` is a work-item filter object (see
    /// `listWorkItemsFiltered`, plus `due` windows and the `waiting_on_*`
    /// shorthands); an invalid filter or a duplicate name throws.
    func createSavedView(name: String, filterJSON: String = "{}") throws -> SavedView {
        let request = try Self.jsonObject(["name": name, "filter": Self.jsonValue(filterJSON)])
        return try decodeJSON(SavedView.self) { out in
            request.withCString { castellum_create_saved_view(handle, $0, out) }
        }
    }

    /// Rename, re-filter and/or hide a view. A built-in accepts only `hidden`.
    func updateSavedView(id: String, name: String? = nil, filterJSON: String? = nil,
                         hidden: Bool? = nil) throws -> SavedView {
        var body: [String: Any] = ["id": id]
        if let name { body["name"] = name }
        if let filterJSON { body["filter"] = try Self.jsonValue(filterJSON) }
        if let hidden { body["hidden"] = hidden }
        let request = try Self.jsonObject(body)
        return try decodeJSON(SavedView.self) { out in
            request.withCString { castellum_update_saved_view(handle, $0, out) }
        }
    }

    /// Delete a custom view; built-ins throw (hide them instead).
    func deleteSavedView(id: String) throws {
        try run { id.withCString { castellum_delete_saved_view(handle, $0) } }
    }

    /// Reorder: listed ids first in that order, the rest keep their order.
    /// Returns every view (hidden included) in the new order.
    @discardableResult
    func reorderSavedViews(ids: [String]) throws -> [SavedView] {
        let request = try String(decoding: JSONSerialization.data(withJSONObject: ids),
                                 as: UTF8.self)
        return try decodeJSON([SavedView].self) { out in
            request.withCString { castellum_reorder_saved_views(handle, $0, out) }
        }
    }

    /// The item count of every visible view, in display order, in one call.
    func savedViewCounts() throws -> [SavedViewCount] {
        try decodeJSON([SavedViewCount].self) { out in
            castellum_saved_view_counts(handle, out)
        }
    }

    /// The work items of one view (same evaluator and order as
    /// `listWorkItemsFiltered`).
    func savedViewItems(id: String) throws -> [WorkItem] {
        try decodeJSON([WorkItem].self) { out in
            id.withCString { castellum_saved_view_items(handle, $0, out) }
        }
    }

    /// One page of inbox threads with no project or work-item link, newest
    /// first (ids and counts only). `limit` defaults to 50, capped at 500.
    func listUnassignedMail(limit: Int = 50, offset: Int = 0) throws -> UnassignedMailPage {
        let request = try Self.jsonObject(["limit": limit, "offset": offset])
        return try decodeJSON(UnassignedMailPage.self) { out in
            request.withCString { castellum_list_unassigned_mail(handle, $0, out) }
        }
    }

    /// Parse JSON text into a value to embed in a request object; text that
    /// is not JSON throws instead of being silently dropped.
    private static func jsonValue(_ text: String) throws -> Any {
        try JSONSerialization.jsonObject(with: Data(text.utf8), options: [.fragmentsAllowed])
    }

    private static func jsonObject(_ body: [String: Any]) throws -> String {
        String(decoding: try JSONSerialization.data(withJSONObject: body), as: UTF8.self)
    }

    // MARK: - shared link model (migration 0017)

    /// Idempotently create/refresh a link; returns the stored row. Manual
    /// rows are never downgraded; derived refreshes never touch the note.
    /// `requestJSON`: {src_kind, src_id, dst_kind, dst_id, relation,
    /// origin?, provenance?, confidence?}.
    func upsertLink(requestJSON: String) throws -> LinkRecord {
        try decodeJSON(LinkRecord.self) { out in
            requestJSON.withCString { castellum_upsert_link(handle, $0, out) }
        }
    }

    /// Remove one link (idempotent). `requestJSON`: endpoints + relation.
    func unlink(requestJSON: String) throws {
        try run { requestJSON.withCString { castellum_unlink(handle, $0) } }
    }

    /// Set or clear the user-owned note on an existing link.
    func setLinkNote(requestJSON: String) throws {
        try run { requestJSON.withCString { castellum_set_link_note(handle, $0) } }
    }

    /// Every link touching (kind, id) in either direction, newest first —
    /// the "why is this connected?" read path.
    func linksFor(kind: String, id: String) throws -> [LinkRecord] {
        try decodeJSON([LinkRecord].self) { out in
            kind.withCString { k in
                id.withCString { i in
                    castellum_links_for(handle, k, i, out)
                }
            }
        }
    }

    // MARK: - write path

    /// Run an FFI call that returns only a status code; throw on non-OK, attaching
    /// the core's specific reason when available.
    private func run(_ call: () -> Int32) throws {
        if let err = CastellumError.from(status: call()) {
            throw CastellumError.lastReason().map(CastellumError.message) ?? err
        }
    }

    /// Explicit Toggle Read/Unread: applies locally + enqueues a replay op.
    /// Refused in evaluation mode like every other write — callers must report
    /// the refusal (see `AccountWritePolicy.isEvaluationModeRefusal`).
    func markRead(messageId: String, isRead: Bool) throws {
        try run { messageId.withCString { castellum_mark_read(handle, $0, isRead ? 1 : 0) } }
    }

    /// Auto-mark-read on open: never refused in evaluation mode, and queues no
    /// remote intent while the account is read-only. Reading mail must not
    /// require leaving read-only.
    func markReadAuto(messageId: String, isRead: Bool) throws {
        try run { messageId.withCString { castellum_mark_read_auto(handle, $0, isRead ? 1 : 0) } }
    }

    /// Mark every unread message in a mailbox read: applies locally + enqueues
    /// one bulk replay op.
    func markAllRead(accountId: String, mailboxId: String) throws {
        try run {
            accountId.withCString { a in
                mailboxId.withCString { m in castellum_mark_all_read(handle, a, m) }
            }
        }
    }

    /// Set/clear a message's `\Flagged` (starred) state: applies locally + enqueues.
    func setFlagged(messageId: String, flagged: Bool) throws {
        try run { messageId.withCString { castellum_set_flagged(handle, $0, flagged ? 1 : 0) } }
    }

    /// Whether the account may make changes to the mail server. Fail-closed:
    /// a new/unknown account is in evaluation (read-only) mode.
    func accountWriteEnabled(accountId: String) throws -> Bool {
        var enabled: Int32 = 0
        let rc = accountId.withCString { castellum_account_write_enabled(handle, $0, &enabled) }
        if let err = CastellumError.from(status: rc) { throw err }
        return enabled != 0
    }

    /// Enable or disable writes for an account. Enabling leaves evaluation mode
    /// and permits Castellum to change the mail server — the caller MUST confirm
    /// the consequences with the user first.
    func setAccountWriteEnabled(accountId: String, enabled: Bool) throws {
        try run {
            accountId.withCString { castellum_set_account_write_enabled(handle, $0, enabled ? 1 : 0) }
        }
    }

    /// Whether CoreSpotlight indexing of message metadata is opted in (S58,
    ///). Profile-wide (`app_meta`), fail-closed: a profile
    /// that has never set this reads `false`.
    func spotlightIndexingEnabled() throws -> Bool {
        var enabled: Int32 = 0
        let rc = castellum_spotlight_indexing_enabled(handle, &enabled)
        if let err = CastellumError.from(status: rc) { throw err }
        return enabled != 0
    }

    /// Persist the CoreSpotlight indexing opt-in. Only persists the flag —
    /// the caller (`AppModel`) is responsible for the resulting one-off
    /// backfill or domain-scoped `CSSearchableIndex` removal.
    func setSpotlightIndexingEnabled(_ enabled: Bool) throws {
        try run { castellum_set_spotlight_indexing_enabled(handle, enabled ? 1 : 0) }
    }

    /// Archive a message (move to the account's Archive mailbox).
    func archiveMessage(messageId: String) throws {
        try run { messageId.withCString { castellum_archive_message(handle, $0) } }
    }

    /// Delete a message (move to the account's Trash mailbox; undoable).
    func deleteMessage(messageId: String) throws {
        try run { messageId.withCString { castellum_delete_message(handle, $0) } }
    }

    /// Archive, trash or move a batch of messages in ONE call and get back the
    /// operation ids that `undoMoves` takes. Each message is its own
    /// transaction: a refusal does not stop the rest (it is listed in the
    /// result). Replaces the per-message `archiveMessage` / `deleteMessage`
    /// loop on the interactive path.
    func moveMessages(ids: [String], target: MoveTarget) throws -> MoveBatchResult {
        let data = try Self.encoder.encode(MoveRequest(ids: ids, target: target))
        guard let json = String(data: data, encoding: .utf8) else { throw CastellumError.internalError }
        return try decodeJSON(MoveBatchResult.self) { out in
            json.withCString { castellum_move_messages(handle, $0, out) }
        }
    }

    /// Queue the inverse of earlier moves (atomic for the group). The
    /// messages come back on the next replay + sync, as new rows.
    func undoMoves(operationIds: [Int64]) throws -> UndoMovesResult {
        struct Request: Encodable { let operationIds: [Int64] }
        let data = try Self.encoder.encode(Request(operationIds: operationIds))
        guard let json = String(data: data, encoding: .utf8) else { throw CastellumError.internalError }
        return try decodeJSON(UndoMovesResult.self) { out in
            json.withCString { castellum_undo_moves(handle, $0, out) }
        }
    }

    /// Permanently delete ONE message that is already in Trash (destructive;
    /// queued through the operation queue, refused for any non-Trash message).
    /// Callers must have shown an explicit confirmation first.
    func deleteMessagePermanently(messageId: String) throws {
        try run { messageId.withCString { castellum_delete_message_permanently(handle, $0) } }
    }

    /// Permanently empty the account's Trash (destructive).
    func emptyTrash(accountId: String) throws {
        try run { accountId.withCString { castellum_empty_trash(handle, $0) } }
    }

    // MARK: - snooze (S43)

    /// Snooze `messageId` until `untilEpoch` (Unix epoch seconds).
    func snoozeMessage(messageId: String, untilEpoch: Int64) throws {
        try run { messageId.withCString { castellum_snooze_message(handle, $0, untilEpoch) } }
    }

    /// Un-snooze `messageId` now. Returns whether it had been snoozed.
    func unsnoozeMessage(id: String) throws -> Bool {
        struct Result: Decodable { let unsnoozed: Bool }
        let result = try decodeJSON(Result.self) { out in
            id.withCString { castellum_unsnooze_message(handle, $0, out) }
        }
        return result.unsnoozed
    }

    /// Every currently-snoozed message, soonest due first.
    func listSnoozedMessages(limit: Int64, offset: Int64) throws -> [SnoozedMessageHeader] {
        try decodeJSON([SnoozedMessageHeader].self) { out in
            castellum_list_snoozed_messages(handle, limit, offset, out)
        }
    }

    /// Pop every snooze due at or before `nowEpoch`.
    func unsnoozeDue(nowEpoch: Int64) throws -> [UnsnoozedMessage] {
        try decodeJSON([UnsnoozedMessage].self) { out in
            castellum_unsnooze_due(handle, nowEpoch, out)
        }
    }

    // MARK: - tags and mute (S42)

    /// Every tag definition, in display order.
    func listTags() throws -> [TagDefinition] {
        try decodeJSON([TagDefinition].self) { castellum_list_tags(handle, $0) }
    }

    /// Create (`tag.keyword.isEmpty`) or update a tag definition. Returns the
    /// stored row (with `keyword`/`keywordKey` filled in on create).
    func upsertTag(_ tag: TagDefinition) throws -> TagDefinition {
        let data = try Self.encoder.encode(tag)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try decodeJSON(TagDefinition.self) { out in
            json.withCString { castellum_upsert_tag(handle, $0, out) }
        }
    }

    /// Delete a tag definition (the definition only — the keyword stays on
    /// the server and on any message that already carries it).
    func deleteTag(keyword: String) throws {
        try run { keyword.withCString { castellum_delete_tag(handle, $0) } }
    }

    /// Add (`add == true`) or remove one IMAP keyword on a message.
    func setMessageTag(messageId: String, keyword: String, add: Bool) throws -> TagApplyOutcome {
        try decodeJSON(TagApplyOutcome.self) { out in
            messageId.withCString { m in
                keyword.withCString { k in
                    castellum_set_message_tag(handle, m, k, add ? 1 : 0, out)
                }
            }
        }
    }

    /// Mute (`muted == true`) or unmute `messageId`'s whole conversation.
    /// Local only.
    func setThreadMuted(messageId: String, muted: Bool) throws {
        try run { messageId.withCString { castellum_set_thread_muted(handle, $0, muted ? 1 : 0) } }
    }

    /// Every distinct visible keyword in use with a message count — Settings'
    /// "In use on your servers" adopt list.
    func listKeywordsInUse() throws -> [KeywordInUse] {
        try decodeJSON([KeywordInUse].self) { castellum_list_keywords_in_use(handle, $0) }
    }

    /// Message counts for every defined tag — the sidebar/menu badges.
    func tagCounts() throws -> [TagCount] {
        try decodeJSON([TagCount].self) { castellum_tag_counts(handle, $0) }
    }

    /// Headers carrying the defined tag `keyword`, newest first, across every
    /// account except Trash, Junk and snoozed mail, one row per email (MM-8).
    /// Keyset-paged: pass the last row of the previous page as `after`.
    func messagesForTag(keyword: String, limit: Int64, after: MessageHeader? = nil) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            keyword.withCString { k in
                if let after {
                    return after.id.withCString { id in
                        castellum_messages_for_tag(handle, k, limit, after.sentAt ?? 0, id, out)
                    }
                }
                return castellum_messages_for_tag(handle, k, limit, 0, nil, out)
            }
        }
    }

    // MARK: - VIP senders and notifications (S59)

    private struct VipChange: Decodable { let changed: Bool }

    /// Add (`vip == true`) or remove a VIP. Returns whether the stored state
    /// changed. The core rejects an address without an `@`.
    @discardableResult
    func setVip(address: String, displayName: String? = nil, vip: Bool) throws -> Bool {
        let result = try decodeJSON(VipChange.self) { out in
            address.withCString { a in
                if let displayName {
                    return displayName.withCString { n in
                        castellum_set_vip(handle, a, n, vip ? 1 : 0, out)
                    }
                }
                return castellum_set_vip(handle, a, nil, vip ? 1 : 0, out)
            }
        }
        return result.changed
    }

    func listVips() throws -> [VipSender] {
        try decodeJSON([VipSender].self) { castellum_list_vips(handle, $0) }
    }

    /// Pop up to `limit` undelivered new-mail notices. At-most-once: the core
    /// marks them delivered in the same transaction.
    func takePendingNotifications(limit: Int64 = 50) throws -> [PendingNotification] {
        try decodeJSON([PendingNotification].self) {
            castellum_take_pending_notifications(handle, limit, $0)
        }
    }

    /// Unread inbox messages from VIP senders; `nil` = every account.
    func vipUnreadCount(accountIds: [String]?) throws -> Int64 {
        var n: Int64 = 0
        let rc: Int32
        if let accountIds {
            let data = try JSONEncoder().encode(accountIds)
            let json = String(data: data, encoding: .utf8) ?? "[]"
            rc = json.withCString { castellum_vip_unread_count(handle, $0, &n) }
        } else {
            rc = castellum_vip_unread_count(handle, nil, &n)
        }
        if let err = CastellumError.from(status: rc) { throw err }
        return n
    }

    // MARK: - compose / send

    /// Snake_case encoder for JSON sent down to the core (mirror of `decoder`).
    /// Exposed as `internal` so AppModel can build JSON payloads with the same strategy.
    static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    /// Insert (`draft.id == nil`) or update a compose draft; returns its id.
    /// The core refuses updates once the draft has left `draft` status.
    func saveDraft(_ draft: NewDraft) throws -> Int64 {
        let data = try Self.encoder.encode(draft)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try saveDraft(draftJSON: json)
    }

    /// Insert or update a compose draft from raw JSON; returns its id.
    func saveDraft(draftJSON: String) throws -> Int64 {
        var id: Int64 = 0
        try run { draftJSON.withCString { castellum_save_draft(handle, $0, &id) } }
        return id
    }

    /// All drafts for an account (any status, newest first).
    func listDrafts(accountId: String) throws -> [Draft] {
        try decodeJSON([Draft].self) { out in
            accountId.withCString { castellum_list_drafts(handle, $0, out) }
        }
    }

    /// Delete a draft (refused once it has left `draft` status).
    func deleteDraft(id: Int64) throws {
        try run { castellum_delete_draft(handle, id) }
    }

    /// Queue a draft for delivery: status -> outbox + one durable `send` op.
    func sendDraft(id: Int64) throws {
        try run { castellum_send_draft(handle, id) }
    }

    /// Queue a draft for delivery after `delaySeconds` (S25 undo-send window;
    /// `0` behaves exactly like `sendDraft(id:)`): the sender skips the op
    /// until that durable earliest-send time passes.
    func sendDraft(id: Int64, delaySeconds: Int64) throws {
        try run { castellum_send_draft_delayed(handle, id, delaySeconds) }
    }

    /// Undo a not-yet-submitted queued send: cancels its `send` op and
    /// returns the draft to `draft` status IF the sender has not already
    /// claimed it. Returns whether it was undone in time.
    func undoSend(draftId: Int64) throws -> Bool {
        struct Result: Decodable { let inTime: Bool }
        let result = try decodeJSON(Result.self) { out in
            castellum_undo_send(handle, draftId, out)
        }
        return result.inTime
    }

    /// Build RFC822 bytes for `id` and enqueue a server-draft save (S36).
    /// Returns whether it was actually queued (`false` = the draft already
    /// left `draft` status - a benign no-op).
    func saveDraftToServer(id: Int64, fromEmail: String, fromName: String?) throws -> Bool {
        struct From: Encodable { let fromEmail: String; let fromName: String? }
        let data = try Self.encoder.encode(From(fromEmail: fromEmail, fromName: fromName))
        guard let json = String(data: data, encoding: .utf8) else { throw CastellumError.internalError }
        struct Result: Decodable { let queued: Bool }
        let result = try decodeJSON(Result.self) { out in
            json.withCString { j in castellum_save_draft_to_server(handle, id, j, out) }
        }
        return result.queued
    }

    /// Hydrate (or reuse) a local editable draft from an already-synced
    /// server message (S36). Returns the draft id.
    func openServerDraft(messageId: String) throws -> Int64 {
        var id: Int64 = 0
        try run { messageId.withCString { castellum_open_server_draft(handle, $0, &id) } }
        return id
    }

    /// Send Later (HP1): queue a draft for delivery at `sendAt` (UTC epoch
    /// seconds, must be in the future). The core keeps it in the durable
    /// outbox across quit and relaunch.
    func scheduleSend(draftId: Int64, sendAt: Int64) throws {
        try run { castellum_schedule_send(handle, draftId, sendAt) }
    }

    /// Move a waiting or needs-confirmation scheduled send to `sendAt`.
    /// Returns whether it was moved.
    func rescheduleSend(draftId: Int64, sendAt: Int64) throws -> Bool {
        struct Result: Decodable { let rescheduled: Bool }
        let result = try decodeJSON(Result.self) { out in
            castellum_reschedule_send(handle, draftId, sendAt, out)
        }
        return result.rescheduled
    }

    /// Cancel a scheduled send: the draft returns, intact, to Drafts. Returns
    /// whether it was cancelled (`false` = the sender already claimed it).
    func cancelScheduledSend(draftId: Int64) throws -> Bool {
        struct Result: Decodable { let cancelled: Bool }
        let result = try decodeJSON(Result.self) { out in
            castellum_cancel_scheduled_send(handle, draftId, out)
        }
        return result.cancelled
    }

    /// Release a `held` send (crash-safety hold, see `reconcileUndoSends`) for
    /// immediate delivery on the next drain. Returns whether one was released.
    func sendNow(draftId: Int64) throws -> Bool {
        struct Result: Decodable { let released: Bool }
        let result = try decodeJSON(Result.self) { out in
            castellum_send_now(handle, draftId, out)
        }
        return result.released
    }

    /// Crash-safety startup pass (S25 — see DECISIONS.md): holds any queued
    /// `send` op for `accountId` whose undo window cannot be proven to have
    /// stayed open under a continuously-live app, so a delayed send is never
    /// auto-delivered after an ambiguous crash. Call ONCE per account at
    /// launch, before scheduling any drain. Returns the number held.
    func reconcileUndoSends(accountId: String) throws -> Int {
        struct Result: Decodable { let held: Int }
        let result = try decodeJSON(Result.self) { out in
            accountId.withCString { castellum_reconcile_undo_sends(handle, $0, out) }
        }
        return result.held
    }

    /// Deliver queued (outbox) drafts over SMTP. `configJSON` =
    /// {account_id, from_email, from_name?, smtp: {host, port, username,
    /// password, use_tls, allow_invalid_certs}}.
    func sendPending(configJSON: String) throws -> SendReport {
        try decodeJSON(SendReport.self) { out in
            configJSON.withCString { castellum_send_pending(handle, $0, out) }
        }
    }

    /// OB1: every message waiting in the Outbox for an account (queued, in
    /// flight, failed or stranded), oldest first.
    func outboxSends(accountId: String) throws -> [OutboxSend] {
        try decodeJSON([OutboxSend].self) { out in
            accountId.withCString { castellum_outbox_sends(handle, $0, out) }
        }
    }

    /// OB1 Outbox "Send Now": attempt exactly this message now, ignoring the
    /// backoff and a permanent failure. `configJSON` is a SendConfig, as for
    /// `sendPending`. `blocked == 1` means the account is read-only.
    func sendDraftNow(configJSON: String, draftId: Int64) throws -> SendReport {
        try decodeJSON(SendReport.self) { out in
            configJSON.withCString { castellum_send_draft_now(handle, $0, draftId, out) }
        }
    }

    /// OB1 Outbox "Send All": give every failed send of the account a fresh
    /// attempt (permanent failures included). Returns how many were requeued.
    @discardableResult
    func requeueFailedSends(accountId: String) throws -> Int {
        struct Result: Decodable { let requeued: Int }
        return try decodeJSON(Result.self) { out in
            accountId.withCString { castellum_requeue_failed_sends(handle, $0, out) }
        }.requeued
    }

    /// Pending (queued/running) write-path operations for an account.
    func pendingOperations(accountId: String) throws -> [OperationRecord] {
        try decodeJSON([OperationRecord].self) { out in
            accountId.withCString { castellum_pending_operations(handle, $0, out) }
        }
    }

    /// Manually retry a failed/failed_permanent operation (outbox action).
    /// Non-failed statuses are a benign no-op in the core.
    func retryOperation(id: Int64) throws {
        try run { castellum_retry_operation(handle, id) }
    }

    /// Manually cancel an undelivered operation (outbox action). Running ops
    /// are refused by the core; the op list reload reflects the outcome.
    func cancelOperation(id: Int64) throws {
        try run { castellum_cancel_operation(handle, id) }
    }

    /// Replay queued operations to the IMAP server (config carries credentials).
    func replayOperations(configJSON: String) throws -> ReplayReport {
        try decodeJSON(ReplayReport.self) { out in
            configJSON.withCString { castellum_replay_operations(handle, $0, out) }
        }
    }

    // MARK: - contacts

    /// Recompute the contacts aggregate + edge graph from synced mail.
    func refreshContacts() throws -> AggregateReport {
        try decodeJSON(AggregateReport.self) { castellum_refresh_contacts(handle, $0) }
    }

    /// X1: decode subjects and display names an earlier build stored as RFC 2047
    /// encoded words, re-index and re-thread them. One-off per profile, resumable,
    /// blocking: call it off the main actor and off the launch path.
    func repairEncodedEnvelopeText() throws -> EnvelopeTextRepairReport {
        try decodeJSON(EnvelopeTextRepairReport.self) {
            castellum_repair_encoded_envelope_text(handle, $0)
        }
    }

    /// Contacts, most recent first. `query` substring-matches email/derived
    /// name/display name (nil = all). Hidden contacts are excluded by default.
    func listContacts(query: String? = nil, includeHidden: Bool = false,
                      limit: Int64 = 500) throws -> [Contact] {
        try decodeJSON([Contact].self) { out in
            if let query {
                return query.withCString {
                    castellum_list_contacts(handle, $0, includeHidden ? 1 : 0, limit, out)
                }
            }
            return castellum_list_contacts(handle, nil, includeHidden ? 1 : 0, limit, out)
        }
    }

    /// Recipients of sent/queued mail, newest first (address autocomplete).
    func listSentRecipients(limit: Int64 = 500) throws -> [SentRecipient] {
        try decodeJSON([SentRecipient].self) { out in
            castellum_list_sent_recipients(handle, limit, out)
        }
    }

    /// One contact by id (with enrichment rows), or nil (core returns JSON `null`).
    func getContact(id: Int64) throws -> ContactDetail? {
        try decodeJSON(ContactDetail?.self) { castellum_get_contact(handle, id, $0) }
    }

    /// Update a contact's user-owned fields (display name / notes / hidden).
    /// Derived stats are refresh-owned and can't be changed from here.
    func updateContact(id: Int64, displayName: String?, notes: String, isHidden: Bool) throws {
        struct UpdateRequest: Encodable {
            let id: Int64
            let displayName: String?
            let notes: String
            let isHidden: Bool
        }
        let payload = UpdateRequest(id: id, displayName: displayName,
                                    notes: notes, isHidden: isHidden)
        let data = try Self.encoder.encode(payload)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        try run { json.withCString { castellum_update_contact(handle, $0) } }
    }

    /// Create a contact by hand (is_user_created = true); returns the stored row.
    func createContact(email: String, name: String?) throws -> Contact {
        struct CreateRequest: Encodable {
            let email: String
            let name: String?
        }
        let data = try Self.encoder.encode(CreateRequest(email: email, name: name))
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try decodeJSON(Contact.self) { out in
            json.withCString { castellum_create_contact(handle, $0, out) }
        }
    }

    // MARK: - identities (S47)

    /// Extra From addresses for `accountId` (the account's own address is
    /// not included — see migration 0026's doc comment).
    func listIdentities(accountId: String) throws -> [Identity] {
        try decodeJSON([Identity].self) { out in
            accountId.withCString { castellum_list_identities(handle, $0, out) }
        }
    }

    /// Create an identity; returns the stored row.
    func createIdentity(
        accountId: String, email: String, displayName: String?,
        replyTo: String? = nil, bcc: String? = nil, isDefault: Bool = false
    ) throws -> Identity {
        struct Req: Encodable {
            let accountId: String
            let email: String
            let displayName: String?
            let replyTo: String?
            let bcc: String?
            let isDefault: Bool
        }
        let data = try Self.encoder.encode(
            Req(accountId: accountId, email: email, displayName: displayName,
                replyTo: replyTo, bcc: bcc, isDefault: isDefault))
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try decodeJSON(Identity.self) { out in
            json.withCString { castellum_create_identity(handle, $0, out) }
        }
    }

    /// Update an identity. A nil argument leaves that field alone; an empty
    /// string clears display name / reply-to / bcc (the email is immutable).
    func updateIdentity(
        id: Int64, displayName: String? = nil, replyTo: String? = nil, bcc: String? = nil,
        isDefault: Bool? = nil
    ) throws {
        struct Req: Encodable {
            let id: Int64
            let displayName: String?
            let replyTo: String?
            let bcc: String?
            let isDefault: Bool?
        }
        let data = try Self.encoder.encode(
            Req(id: id, displayName: displayName, replyTo: replyTo, bcc: bcc, isDefault: isDefault))
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        try run { json.withCString { castellum_update_identity(handle, $0) } }
    }

    /// Delete an identity.
    func deleteIdentity(id: Int64) throws {
        try run { castellum_delete_identity(handle, id) }
    }

    // MARK: - enrichment (slice 8d)

    /// JSON-dispatch helper for the enrichment endpoint.
    private func enrichmentOp<T: Decodable>(_ type: T.Type, opJSON: String) throws -> T {
        try decodeJSON(T.self) { out in
            opJSON.withCString { castellum_enrichment(handle, $0, out) }
        }
    }

    /// All registered enrichment adapters (id, displayName, kind, inputLabel).
    func enrichmentSources() throws -> [EnrichmentSourceInfo] {
        try enrichmentOp([EnrichmentSourceInfo].self,
                         opJSON: #"{"op":"list_sources"}"#)
    }

    /// Preview: parse `input` through `sourceId` WITHOUT writing to the DB.
    /// The core returns preview rows shaped {field, value, provenance} —
    /// no id/source/fetchedAt (those only exist once saved), hence the
    /// distinct `PreviewEnrichmentField` DTO. Guarded by the Rust FFI test
    /// `enrichment_preview_and_save_shapes_over_the_c_abi`.
    func previewEnrichment(contactId: Int64, sourceId: String,
                           input: String) throws -> [PreviewEnrichmentField] {
        let payload = try Self.encoder.encode(EnrichRequest(
            op: "enrich", contactId: contactId,
            sourceId: sourceId, input: input, save: false))
        guard let json = String(data: payload, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try enrichmentOp([PreviewEnrichmentField].self, opJSON: json)
    }

    /// Save: parse `input` through `sourceId` and upsert the fields.
    /// Returns the saved rows for that source, with real DB ids.
    @discardableResult
    func saveEnrichment(contactId: Int64, sourceId: String,
                        input: String) throws -> [EnrichmentField] {
        let payload = try Self.encoder.encode(EnrichRequest(
            op: "enrich", contactId: contactId,
            sourceId: sourceId, input: input, save: true))
        guard let json = String(data: payload, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return try enrichmentOp([EnrichmentField].self, opJSON: json)
    }

    /// Delete a single enrichment row by id.
    func deleteEnrichment(id: Int64) throws {
        struct Req: Encodable { let op: String; let id: Int64 }
        /// Empty acknowledgement; decodes the `{}` the dispatch returns.
        struct Ack: Decodable {}
        let data = try Self.encoder.encode(Req(op: "delete", id: id))
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        _ = try decodeJSON(Ack.self) { out in
            json.withCString { castellum_enrichment(handle, $0, out) }
        }
    }

    /// Directly upsert one enrichment field (bypasses the heuristic parser).
    /// Used by the quick-add row in the detail pane.
    func rawAddEnrichment(contactId: Int64, source: String,
                          field: String, value: String,
                          provenance: String? = nil) throws {
        struct Req: Encodable {
            let op: String; let contactId: Int64
            let source: String; let field: String; let value: String
            let provenance: String?
        }
        struct Ack: Decodable {}
        let data = try Self.encoder.encode(
            Req(op: "raw_add", contactId: contactId, source: source,
                field: field, value: value, provenance: provenance))
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        _ = try decodeJSON(Ack.self) { out in
            json.withCString { castellum_enrichment(handle, $0, out) }
        }
    }

    private struct EnrichRequest: Encodable {
        let op: String
        let contactId: Int64
        let sourceId: String
        let input: String
        let save: Bool
    }

    // MARK: - projects

    /// All projects; set `includeArchived` to also return archived ones.
    func listProjects(includeArchived: Bool = false) throws -> [ProjectCard] {
        try decodeJSON([ProjectCard].self) {
            castellum_list_projects(handle, includeArchived ? 1 : 0, $0)
        }
    }

    /// Create (id absent/empty) or update a project from a JSON payload; returns
    /// the stored project row.
    func upsertProject(json: String) throws -> Project {
        try decodeJSON(Project.self) { out in
            json.withCString { castellum_upsert_project(handle, $0, out) }
        }
    }

    /// Permanently delete a project and its link rows.
    func deleteProject(id: String) throws {
        try run { id.withCString { castellum_delete_project(handle, $0) } }
    }

    /// Full project detail (project + linked emails + linked contacts), or nil if
    /// not found (the core returns JSON `null`).
    func getProject(id: String) throws -> ProjectDetail? {
        try decodeJSON(ProjectDetail?.self) { out in
            id.withCString { castellum_get_project(handle, $0, out) }
        }
    }

    /// Link or unlink a message to a project. `autoContacts` true also links
    /// the message's participants automatically.
    func projectSetEmailLink(projectId: String, messageId: String,
                             linked: Bool, autoContacts: Bool) throws {
        try run {
            projectId.withCString { pid in
                messageId.withCString { mid in
                    castellum_project_set_email_link(handle, pid, mid,
                                                     linked ? 1 : 0,
                                                     autoContacts ? 1 : 0)
                }
            }
        }
    }

    /// Link or unlink a contact to a project (sets origin to "manual").
    func projectSetContactLink(projectId: String, contactId: Int64, linked: Bool) throws {
        try run {
            projectId.withCString {
                castellum_project_set_contact_link(handle, $0, contactId, linked ? 1 : 0)
            }
        }
    }

    /// Reverse lookup: projects (as lightweight tags) that contain this message.
    func projectsForMessage(messageId: String) throws -> [ProjectTag] {
        try decodeJSON([ProjectTag].self) { out in
            messageId.withCString { castellum_projects_for_message(handle, $0, out) }
        }
    }

    /// Move a project to a new status column at the given position.
    func moveProject(projectId: String, status: String, position: Int64) throws {
        try run {
            projectId.withCString { pid in
                status.withCString { st in
                    castellum_move_project(handle, pid, st, position)
                }
            }
        }
    }

    // MARK: - smart mailboxes (S41)

    /// All smart mailboxes, sidebar order.
    func listSmartMailboxes() throws -> [SmartMailbox] {
        try decodeJSON([SmartMailbox].self) { castellum_list_smart_mailboxes(handle, $0) }
    }

    /// Create (`id` absent/empty) or rename/re-query (`id` present) a smart
    /// mailbox from a JSON payload; returns the stored row with freshly
    /// recomputed counts.
    func upsertSmartMailbox(json: String) throws -> SmartMailbox {
        try decodeJSON(SmartMailbox.self) { out in
            json.withCString { castellum_upsert_smart_mailbox(handle, $0, out) }
        }
    }

    /// Permanently delete a smart mailbox (never touches any mail).
    func deleteSmartMailbox(id: String) throws {
        try run { id.withCString { castellum_delete_smart_mailbox(handle, $0) } }
    }

    /// Reorder from a JSON array of ids, e.g. `["id1","id2","id3"]`.
    func reorderSmartMailboxes(orderedIdsJSON: String) throws {
        try run { orderedIdsJSON.withCString { castellum_reorder_smart_mailboxes(handle, $0) } }
    }

    /// Recompute every smart mailbox's cached counts; returns the refreshed
    /// list. Call after a sync pass, never per keystroke.
    func refreshSmartMailboxCounts() throws -> [SmartMailbox] {
        try decodeJSON([SmartMailbox].self) { castellum_refresh_smart_mailbox_counts(handle, $0) }
    }

    /// MM-1c: which header names a Header condition may use (single source in
    /// Rust) plus up to 200 header names seen in this profile for completion.
    /// Local-only, no network I/O.
    func filterHeaderPolicy() throws -> FilterHeaderPolicy {
        try decodeJSON(FilterHeaderPolicy.self) { castellum_filter_header_policy(handle, $0) }
    }

    /// MM-1c: how many messages have their headers stored vs all messages
    /// ("Headers known: N of M" caption). Local-only.
    func messageHeaderCoverage() throws -> HeaderCoverageDTO {
        try decodeJSON(HeaderCoverageDTO.self) { castellum_message_header_coverage(handle, $0) }
    }

    /// Runs one smart mailbox's saved S39-grammar query cross-account — same
    /// path as `searchAcrossAccounts`. `nil` when `id` is unknown (deleted
    /// elsewhere) — never thrown as an error.
    func messagesForSmartMailbox(id: String, limit: Int64) throws -> [MessageHeader]? {
        try decodeJSON([MessageHeader]?.self) { out in
            id.withCString { castellum_messages_for_smart_mailbox(handle, $0, limit, out) }
        }
    }

    // MARK: - Split Inbox (HP-2)

    /// The request body both split calls take: which Inbox(es) — empty means
    /// every account's Inbox — and, for a list, which tab.
    private struct InboxSplitRequest: Encodable {
        let mailboxIds: [String]
        var splitId: String?
        var limit: Int64?
    }

    private func inboxSplitJSON(_ req: InboxSplitRequest) throws -> String {
        let data = try Self.encoder.encode(req)
        return String(decoding: data, as: UTF8.self)
    }

    /// Exact total/unread per split (tab order), then Other (`id == "other"`,
    /// always last). Rust owns the definitions and the counts.
    func inboxSplitCounts(mailboxIds: [String]) throws -> [InboxSplitCount] {
        let json = try inboxSplitJSON(InboxSplitRequest(mailboxIds: mailboxIds))
        return try decodeJSON([InboxSplitCount].self) { out in
            json.withCString { castellum_inbox_split_counts(handle, $0, out) }
        }
    }

    /// One tab's messages, newest first. `nil` when the split no longer
    /// exists (deleted or unflagged elsewhere) — never thrown as an error.
    func inboxSplitMessages(mailboxIds: [String], splitId: String, limit: Int64) throws -> [MessageHeader]? {
        let json = try inboxSplitJSON(InboxSplitRequest(mailboxIds: mailboxIds, splitId: splitId, limit: limit))
        return try decodeJSON([MessageHeader]?.self) { out in
            json.withCString { castellum_inbox_split_messages(handle, $0, out) }
        }
    }

    // MARK: - rules (S40 engine, MM-3 UI)

    /// Every stored filter for `accountId` (its own plus account-agnostic), or
    /// all of them when nil, in rule run order. Smart mailboxes are included;
    /// callers filter on `kind`.
    func listFilters(accountId: String?) throws -> [SavedRule] {
        try decodeJSON([SavedRule].self) { out in
            if let accountId {
                return accountId.withCString { castellum_list_filters(handle, $0, out) }
            }
            return castellum_list_filters(handle, nil, out)
        }
    }

    func upsertFilter(json: String) throws -> SavedRule {
        try decodeJSON(SavedRule.self) { out in
            json.withCString { castellum_upsert_filter(handle, $0, out) }
        }
    }

    func deleteFilter(id: String) throws {
        try run { id.withCString { castellum_delete_filter(handle, $0) } }
    }

    /// Run order from a JSON array of ids.
    func reorderFilters(orderedIdsJSON: String) throws {
        try run { orderedIdsJSON.withCString { castellum_reorder_filters(handle, $0) } }
    }

    /// Dry run of an UNSAVED condition group: the matching messages, nothing
    /// enqueued, nothing changed.
    func evaluateFilter(groupJSON: String, accountId: String?, limit: Int64) throws -> [MessageHeader] {
        try decodeJSON([MessageHeader].self) { out in
            groupJSON.withCString { g in
                if let accountId {
                    return accountId.withCString { castellum_evaluate_filter(handle, g, $0, limit, 0, out) }
                }
                return castellum_evaluate_filter(handle, g, nil, limit, 0, out)
            }
        }
    }

    /// Dry run of UNSAVED rule conditions with the engine's candidate rules
    /// (Trash, Junk, Drafts and Sent left out before the limit): the first
    /// `limit` matches and the exact total. Enqueues nothing.
    func previewRuleConditions(groupJSON: String, accountId: String?, limit: Int64) throws -> RuleConditionsPreviewDTO {
        try decodeJSON(RuleConditionsPreviewDTO.self) { out in
            groupJSON.withCString { g in
                if let accountId {
                    return accountId.withCString { castellum_preview_rule_conditions(handle, g, $0, limit, out) }
                }
                return castellum_preview_rule_conditions(handle, g, nil, limit, out)
            }
        }
    }

    /// Dry run of a SAVED rule: the first `limit` matches paired with its
    /// actions, plus the exact `total` Run Now would act on; enqueues nothing.
    func previewRule(id: String, accountId: String?, limit: Int64) throws -> RulePreviewDTO {
        try decodeJSON(RulePreviewDTO.self) { out in
            id.withCString { f in
                if let accountId {
                    return accountId.withCString { castellum_preview_rule(handle, f, $0, limit, out) }
                }
                return castellum_preview_rule(handle, f, nil, limit, out)
            }
        }
    }

    /// One bounded "Run now" batch: queues the rule's actions (through the
    /// operation queue, where write-safety is enforced at replay) for its next
    /// `limit` candidates after message id `after` (nil = from the start).
    /// The report's `nextAfter` is where the next batch starts; nil = done.
    /// Blocking: call off the main actor.
    func runRuleNowBatch(id: String, accountId: String?, after: String?, limit: Int64) throws -> RuleRunReportDTO {
        try decodeJSON(RuleRunReportDTO.self) { out in
            id.withCString { f in
                withOptionalCString(accountId) { a in
                    withOptionalCString(after) { castellum_run_rule_now_batch(handle, f, a, $0, limit, out) }
                }
            }
        }
    }

    // MARK: - People graph (slice 8c)

    /// Build the top-N contact co-occurrence graph.
    /// `projectId: nil` = all contacts (no project filter).
    /// `maxNodes` is clamped to [1..200] by the core.
    func contactGraph(projectId: String? = nil, maxNodes: Int64 = 100) throws -> ContactGraph {
        try decodeJSON(ContactGraph.self) { out in
            if let projectId {
                return projectId.withCString {
                    castellum_contact_graph(handle, $0, maxNodes, out)
                }
            }
            return castellum_contact_graph(handle, nil, maxNodes, out)
        }
    }

    // MARK: - AI job queue

    /// Enqueue an AI job and return `{"job_id": n}`.
    func enqueueAiJob(requestJSON: String) throws -> AiEnqueueResult {
        try decodeJSON(AiEnqueueResult.self) { out in
            requestJSON.withCString { castellum_enqueue_ai_job(handle, $0, out) }
        }
    }

    /// Run pending AI jobs until the queue is drained (blocking). Returns a
    /// report of how many jobs ran/succeeded/failed/were cancelled.
    func runAiJobs(configJSON: String) throws -> AiRunReport {
        try decodeJSON(AiRunReport.self) { out in
            configJSON.withCString { castellum_run_ai_jobs(handle, $0, out) }
        }
    }

    /// Long-poll a job's current state and any new events after `afterEventId`.
    /// Returns `nil` if the job id is unknown (the core serialises `null`).
    func pollAiJob(id: Int64, afterEventId: Int64) throws -> AiPollResult? {
        try decodeJSON(AiPollResult?.self) { out in
            castellum_poll_ai_job(handle, id, afterEventId, out)
        }
    }

    /// Request cooperative cancellation of a job. Queued jobs are cancelled
    /// immediately; in-flight jobs honour the cancel flag between flushes.
    func cancelAiJob(id: Int64) throws {
        try run { castellum_cancel_ai_job(handle, id) }
    }

    /// List models available at the Ollama endpoint. `configJSON` must include
    /// `"endpoint"`. The egress guard in the provider fires here.
    func aiListModels(configJSON: String) throws -> [AiModelInfo] {
        try decodeJSON([AiModelInfo].self) { out in
            configJSON.withCString { castellum_ai_list_models(handle, $0, out) }
        }
    }

    /// Retrieve a cached AI output by its natural key. Returns `nil` when no
    /// output has been stored (the core serialises `null`).
    func getAiOutput(scope: String, ownerId: String, feature: String) throws -> AiOutput? {
        try decodeJSON(AiOutput?.self) { out in
            scope.withCString { s in
                ownerId.withCString { o in
                    feature.withCString { f in
                        castellum_get_ai_output(handle, s, o, f, out)
                    }
                }
            }
        }
    }

    /// "Find similar" (S53-UI): embeds `messageId` (ad-hoc, reusing its
    /// stored embedding when one exists), then finds its nearest neighbours
    /// by cosine similarity, most similar first. `configJSON` is the same
    /// shape `runAiJobs(configJSON:)` takes. `accountId: nil` searches every
    /// account.
    ///
    /// Deliberately SYNCHRONOUS/blocking, like every other method in this
    /// file (`enqueueAiJob`, `runAiJobs`, …) — this class has no `async`
    /// methods anywhere; the off-main-actor hop is the CALLER's job via
    /// `Task.detached` (see `AppModel.findSimilar`), matching how
    /// `AiCoordinator`/`AppModel` already bridge every other blocking FFI
    /// call. May take longer than an ordinary search call: the ad-hoc-embed
    /// path is a network round trip to Ollama.
    func findSimilarMessages(
        configJSON: String, messageId: String, accountId: String?, limit: Int64
    ) throws -> [SimilarMessageResult] {
        struct FindSimilarResponse: Decodable {
            let results: [SimilarMessageResult]
        }
        return try decodeJSON(FindSimilarResponse.self) { out in
            configJSON.withCString { cfg in
                messageId.withCString { mid in
                    if let accountId {
                        return accountId.withCString { acc in
                            castellum_find_similar_messages(handle, cfg, mid, acc, limit, out)
                        }
                    }
                    return castellum_find_similar_messages(handle, cfg, mid, nil, limit, out)
                }
            }
        }.results
    }

    /// Retrieve a thread's typed extraction rows (`extract_thread_items`,
    /// S54). Empty array when nothing has been extracted yet — never throws
    /// for that case.
    func getThreadExtractions(threadId: String) throws -> [ThreadExtractionItem] {
        try decodeJSON([ThreadExtractionItem].self) { out in
            threadId.withCString { t in castellum_get_thread_extractions(handle, t, out) }
        }
    }
}
