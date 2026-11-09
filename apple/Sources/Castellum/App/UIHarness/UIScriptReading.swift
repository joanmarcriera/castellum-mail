// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 extension ops for Reading, message list, smart mailboxes, rules and layouts
// (Debug builds only; stage U3).
//
// Usage: a scenario step `{"op":"reading","id":"<action>", ...}` is routed here by the runner, and an
// expectation `{"model":"reading:<key>","equals":"<value>"}` is evaluated by `UIReading.value`.
// Kept in its own file so sibling harness extensions merge cleanly; the shared runner has only
// additive one-line hooks. Everything calls real app entry points (the same AppModel methods the
// sidebar, toolbar, reading-pane header and Settings call); nothing here is reachable from a
// Release build (`#if DEBUG`; `scripts/ui-check.sh --verify-release` greps for these symbols).
//
// Actions (fields reuse UIStep: id=action, accountRef, text, name, window, tab, timeoutMs, value):
//   selectMailbox   accountRef=<ref> text=<role|name>   open one mailbox of an account
//   selectUnified   text=<role>                          the "All Inboxes" style cross-account list
//   selectSnoozed                                        the Snoozed sidebar group
//   selectSmart     text=<name>                          a smart mailbox
//   selectTag       text=<display name>                  a sidebar Tags row (MM-8): the model call a click makes
//   bareKey         text=<one character>                 MM-8: one bare key through the real KeySequenceRouter (registry lookup, focus stand-down,
//                                                        empty-slot pass-through); outcome in `reading:bareKeyOutcome`. Chords (g, l) keep state between steps.
//   reloadKeyBindings                                    MM-12: reload keybindings.json; read `reading:keyProblems` (kinds, comma-joined, or none) and `reading:keyProblemText`
//   goToTagReturn   text=<filter text>                   MM-8: what Return does in the open `g l` picker (model-level: the SwiftUI popover is not reachable)
//   selectMessage   text=<subject substring>            wait for the row, open it in the reading pane
//   clearMessage                                         deselect (reading pane back to empty)
//   searchScope     text=mailbox|account|all             the toolbar scope menu
//   drillDown       name=<qualifier> text=<value>        the reading-pane header click (sender/date/subject)
//   correspondence  text=<address>                       View > Show Correspondence
//   closeCorrespondence
//   settingsTab     tab=<general|ai|accounts|rules|notifications|tags|licence> [accountRef]   opens Settings there
//   setCounter      name=<dock|menuBar> text=<off|inbox|vip|smart:<smart mailbox name>>   Settings > Notifications > Counters
//                   (expectations: reading:dockBadgeLabel, reading:menuBarTitle, reading:counterMatches:<dock|menuBar>:<inbox|inbox:<ref>|smart:<name>>)
//   setCountsInCounters accountRef=<ref> value=<bool>    the per-account "Count in badges" toggle
//   upsertSmart     name=<name> text=<FilterGroupDTO JSON> [accountRef=<scope to that account>]
//   deleteSmart     name=<name>
//   saveRule        name=<name> text=<{"group":..,"actions":[..],"stop":bool,"account":"<ref>"} JSON>
//   runRule         name=<name>                          Run Now (result stored as the rule notice)
//   previewRule     name=<name>                          Preview Matches count (stored as the notice)
//   setWrite        accountRef=<ref> value=<bool>        Settings > Accounts "allow writes" for a test account
//   createTag       name=<display name> text=<colour>
//   toggleTag       text=<keyword>                       the `l` picker's toggle on the selection
//   toggleMute                                           `m` on the selection
//   snooze          text=<seconds from now>              Snooze > Custom on the selection
//   scheduleSend    accountRef text=<subject>            HP1: save a draft and schedule it a day ahead (local only; never due in a run)
//   unsnooze                                             Snoozed list "back to inbox"
//   findSimilar                                          the reading-pane Find Similar button
//   dismissFindSimilar
//   showPalette / showSource                             open the palette sheet / message-source window
//   fieldText       window=<label> text=<s>              type into the first editable text field
//   createProjectAssign name=<project name>              assign the selected message to a new project
//   deleteSelected / cancelDelete                        the Delete key (permanent-delete confirmation in Trash)
//   sortBy          name=<date|from|subject|unread|starred|attachments> text=<asc|desc>   the header-click sort (#3043), via the model: no header press
//   detailToggleFold text=<block index>   open/close one quote or signature block of the plain-text body (#2807)
//   detailSelect    text=<substring>      select that text of the shown message, as a mouse selection in the pane would
//   detailExpandAll                        View > Expand All Quotes
//   spotlightOpen   text=<subject substring | remembered | id:<raw id>>   what clicking a Spotlight result does (#3382); the id comes
//                   from the shown list, the `remember`ed message, or is given raw (a dead result)
// Expectation keys (`reading:`): see `value(_:runner:)` below.
#if DEBUG
import AppKit
import Foundation

@MainActor
enum UIReading {
    nonisolated static let actions: Set<String> = Set<String>([
        "selectMailbox", "selectUnified", "selectSnoozed", "selectSmart", "selectTag", "bareKey", "reloadKeyBindings", "goToTagReturn", "selectMessage", "clearMessage",
        "searchScope", "drillDown", "correspondence", "closeCorrespondence", "settingsTab", "setCounter", "setCountsInCounters", "upsertSmart",
        "deleteSmart", "saveRule", "runRule", "previewRule", "setWrite", "createTag", "toggleTag",
        "toggleMute", "snooze", "scheduleSend", "seedFailedSend", "seedServerDraft", "openDraftRow", "unsnooze", "findSimilar", "dismissFindSimilar", "showPalette",
        "showSource", "fieldText", "createProjectAssign", "deleteSelected", "cancelDelete", "drillSelected", "reSyncAll", "toggleRule", "moveRuleToTop", "waitFor", "projectLinks", "remember", "breakPassword", "sortBy", "detailToggleFold", "detailSelect", "detailExpandAll", "spotlightOpen",
    ]).union(UISplits.actions) // HP-2 Split Inbox ops live in UIScriptSplits.swift

    /// Free-text results of async actions (rule notice, preview count), read back via `reading:note`.
    static var note = ""
    /// Message id saved by the `remember` action (the duplicated GreenMail seeds share subjects, so tests track one id).
    static var rememberedId: String?
    /// MM-8: one router for the whole run, so a `g` then `l` chord spans two `bareKey` steps like real typing.
    private static var bareKeyRouter: KeySequenceRouter?
    /// "consumed" or "passthrough": what the last `bareKey` step did.
    static var lastBareKeyOutcome = "none"

    /// Validation problems for a `reading` step (checked before any step runs).
    nonisolated static func problems(_ s: UIStep) -> [String] {
        guard let a = s.id, actions.contains(a) else { return ["unknown reading action '\(s.id ?? "")'"] }
        if let split = UISplits.problems(s) { return split }
        func need(_ ok: Bool, _ what: String) -> [String] { ok ? [] : ["\(a) needs \(what)"] }
        switch a {
        case "selectMailbox": return need(s.accountRef != nil && s.text != nil, "accountRef and text")
        case "selectUnified", "selectSmart", "selectTag", "bareKey", "goToTagReturn", "selectMessage", "searchScope", "correspondence", "createTag",
             "toggleTag", "snooze":
            return need(s.text != nil, "text")
        case "drillDown": return need(s.name != nil && s.text != nil, "name and text")
        case "drillSelected": return need(["from", "to", "cc", "date", "subject"].contains(s.text ?? ""), "text=from|to|cc|date|subject")
        case "settingsTab": return need(s.tab != nil, "tab")
        case "upsertSmart", "saveRule": return need(s.name != nil && s.text != nil, "name and text")
        case "deleteSmart", "runRule", "previewRule", "createProjectAssign": return need(s.name != nil, "name")
        case "breakPassword": return need(s.accountRef != nil, "accountRef")
        case "waitFor": return need(s.name != nil && s.text != nil, "name (key) and text (value)")
        case "toggleRule": return need(s.name != nil && s.value != nil, "name and value")
        case "moveRuleToTop": return need(s.name != nil, "name")
        case "spotlightOpen": return need(s.text != nil, "text")
        case "detailToggleFold": return need(Int(s.text ?? "") != nil, "text=<block index>")
        case "detailSelect": return need(s.text != nil, "text")
        case "sortBy":
            return need(MessageSortKey(rawValue: s.name ?? "") != nil && ["asc", "desc"].contains(s.text ?? ""), "name=date|from|subject|unread|starred|attachments and text=asc|desc")
        case "setCounter":
            return need(["dock", "menuBar"].contains(s.name ?? "") && s.text != nil, "name=dock|menuBar and text=off|inbox|vip|smart:<name>")
        case "setCountsInCounters": return need(s.accountRef != nil && s.value != nil, "accountRef and value")
        case "setWrite": return need(s.accountRef != nil && s.value != nil, "accountRef and value")
        case "scheduleSend": return need(s.accountRef != nil && s.text != nil, "accountRef and text (subject)")
        case "seedFailedSend": return need(s.accountRef != nil && s.text != nil, "accountRef and text (subject)")
        case "seedServerDraft": return need(s.accountRef != nil && s.text != nil && s.name != nil, "accountRef, text (subject) and name (recipient email)")
        case "openDraftRow": return need(s.text != nil, "text (subject of the list row)")
        case "fieldText": return need(s.window != nil && s.text != nil, "window and text")
        default: return []
        }
    }

    // MARK: actions

    static func perform(_ s: UIStep, runner r: UIScriptRunner) async throws {
        let model = r.model
        let timeout = s.timeoutMs ?? 15_000
        func ref(_ x: String?) -> String? { x.flatMap { r.refs[$0] ?? $0 } }
        if UISplits.actions.contains(s.id ?? "") { try await UISplits.perform(s, runner: r); return }
        switch s.id ?? "" {
        case "selectMailbox":
            guard let acct = ref(s.accountRef) else { return }
            try await r.poll(timeoutMs: timeout, "mailboxes of \(acct)") {
                model.mailboxes[acct]?.contains { $0.role == s.text || $0.name == s.text } == true
            }
            guard let mb = model.mailboxes[acct]?.first(where: { $0.role == s.text || $0.name == s.text }) else { return }
            model.select(mailbox: mb)
        case "selectUnified": model.select(unifiedRole: s.text ?? "inbox")
        case "selectSnoozed": model.select(snoozed: ())
        case "selectSmart":
            try await r.poll(timeoutMs: timeout, "smart mailbox '\(s.text ?? "")'") {
                model.smartMailboxes.contains { $0.name == s.text }
            }
            if let sm = model.smartMailboxes.first(where: { $0.name == s.text }) { model.select(smartMailbox: sm) }
        case "selectTag":
            try await r.poll(timeoutMs: timeout, "tag '\(s.text ?? "")'") {
                model.tags.contains { $0.displayName == s.text }
            }
            if let tag = model.tags.first(where: { $0.displayName == s.text }) { model.select(tag: tag) }
        case "bareKey":
            // One stroke in file syntax ("e", "R", "⌃x", "Space") through the real KeySequenceRouter.
            guard let text = s.text, let stroke = KeyStroke(display: text) else {
                throw UIStepError(message: "bareKey needs exactly one key stroke (file syntax, e.g. e, R, ⌃x, Space)")
            }
            let router = bareKeyRouter ?? KeySequenceRouter(
                table: { [weak model] in model?.keyBindings.table ?? KeySequenceTable(specs: CommandRegistry.all) },
                specs: { [weak model] in model?.keyBindings.effectiveSpecs ?? CommandRegistry.all },
                isEnabled: { [weak model] spec in model?.isBareKeyCommandAvailable(spec.id) ?? true })
            bareKeyRouter = router
            lastBareKeyOutcome = router.handle(stroke: stroke, hasCommand: false) == .consumed ? "consumed" : "passthrough"
        case "reloadKeyBindings":   // MM-12: Settings > Reload Key Bindings without the alert; read the result with `reading:keyProblems`
            model.keyBindings.reload()
        case "goToTagReturn":
            guard model.goToTagPickerShown else { throw UIStepError(message: "the go-to-tag picker is not open") }
            guard let tag = TagGoToMatcher.tag(highlighted: nil, query: s.text ?? "", in: model.tags) else {
                throw UIStepError(message: "no tag matches '\(s.text ?? "")'")
            }
            model.select(tag: tag)
            model.goToTagPickerShown = false
        case "selectMessage":
            try await r.poll(timeoutMs: timeout, "message '\(s.text ?? "")' in the list") {
                model.messages.contains { ($0.subject ?? "").contains(s.text ?? "") }
            }
            let candidates = model.messages.filter { ($0.subject ?? "").contains(s.text ?? "") }
            guard var m = candidates.first else { return }
            if s.value == true {
                // The duplicated GreenMail seeds include copies without the ICS part: open copies until one carries an invite.
                var found = false
                for c in candidates.prefix(10) {
                    model.select(message: c)
                    try await r.poll(timeoutMs: timeout, "reading pane detail") { model.detail?.header.id == c.id }
                    for _ in 0..<30 where model.detail?.calendarInvite == nil { try? await Task.sleep(nanoseconds: 100_000_000) }
                    if model.detail?.calendarInvite != nil { m = c; found = true; break }
                }
                if !found { throw UIStepError(message: "none of \(candidates.count) copies of '\(s.text ?? "")' carries a calendar invite") }
            } else {
                model.select(message: m)
            }
            try await r.poll(timeoutMs: timeout, "reading pane detail") { model.detail?.header.id == m.id }
        case "clearMessage": model.clearSelection()
        case "spotlightOpen":
            let text = s.text ?? ""
            let id: String
            if text.hasPrefix("id:") { id = String(text.dropFirst(3)) }
            else if text == "remembered" {
                guard let r = rememberedId else { throw UIStepError(message: "spotlightOpen: nothing remembered") }
                id = r
            } else {
                guard let m = model.messages.first(where: { ($0.subject ?? "").contains(text) }) else {
                    throw UIStepError(message: "spotlightOpen: no message in the shown list matches '\(text)'")
                }
                id = m.id
            }
            model.openSpotlightResult(messageId: id)
        case "detailToggleFold": model.plainFold.toggle(index: Int(s.text ?? "") ?? -1)
        case "detailExpandAll": model.expandAllQuotes()
        case "detailSelect":
            guard let d = model.detail, let body = d.textBody, body.contains(s.text ?? "\u{0}") else {
                throw UIStepError(message: "detailSelect: the shown message does not contain '\(s.text ?? "")'")
            }
            model.readingSelection = ReadingSelection(messageId: d.header.id, text: s.text ?? "")
        case "sortBy":
            model.setMessageSort(MessageSort(key: MessageSortKey(rawValue: s.name ?? "") ?? .date, ascending: s.text == "asc"))
        case "searchScope":
            guard let sc = SearchScope(rawValue: s.text ?? "") else { throw UIStepError(message: "bad scope '\(s.text ?? "")'") }
            model.searchScope = sc
            if !model.searchText.isEmpty { model.runSearch() }
        case "drillDown": model.drillDown(qualifier: s.name ?? "", value: s.text ?? "")
        case "drillSelected":
            // Exactly what the reading-pane header links pass (ReadingPaneView): same qualifier, same value shape.
            guard let d = model.detail else { throw UIStepError(message: "no open message") }
            switch s.text ?? "" {
            case "from":
                guard let a = d.header.fromAddress else { throw UIStepError(message: "message has no sender") }
                model.drillDown(qualifier: "from", value: "=" + a)
            case "to", "cc":
                guard let p = d.addresses.first(where: { $0.kind == s.text }) else { throw UIStepError(message: "message has no \(s.text ?? "") recipient") }
                model.drillDown(qualifier: s.text ?? "", value: "=" + p.address)
            case "date":
                guard let e = d.header.sentAt else { throw UIStepError(message: "message has no date") }
                model.drillDown(qualifier: "date", value: SearchQueryBuilder.dayToken(epoch: e))
            default:
                guard let subj = d.header.subject else { throw UIStepError(message: "message has no subject") }
                model.drillDown(qualifier: "subject", value: subj)
            }
        case "reSyncAll": model.reSyncAll()
        case "correspondence": model.showCorrespondence(address: s.text ?? "", name: nil)
        case "closeCorrespondence": model.closeCorrespondence()
        case "settingsTab":
            r.labels.expect("settings")
            let tab: SettingsTab
            switch s.tab ?? "" {
            case "general": tab = .general
            case "ai": tab = .ai
            case "accounts": tab = .accounts
            case "rules": tab = .rules
            case "notifications": tab = .notifications
            case "tags": tab = .tags
            default: throw UIStepError(message: "unknown settings tab '\(s.tab ?? "")'")
            }
            guard model.settingsOpener != nil else { throw UIStepError(message: "wait settingsOpenerReady first") }
            if let a = ref(s.accountRef) { model.openAccountsSettings(accountId: a) } else { model.openSettingsTab(tab) }
        case "upsertSmart":
            // JSON object = a nested-conditions mailbox; anything else = an old text-query mailbox.
            let isGroup = (s.text ?? "").hasPrefix("{")
            let group: FilterGroupDTO? = isGroup ? try decode(FilterGroupDTO.self, s.text) : nil
            var scope: SmartMailboxScopeDTO?
            if let a = ref(s.accountRef) { scope = SmartMailboxScopeDTO(accountIds: [a], mailboxIds: []) }
            var saved: SmartMailbox?
            var done = false
            model.upsertSmartMailbox(id: nil, name: s.name ?? "", query: isGroup ? "" : (s.text ?? ""), group: group, scope: scope) { saved = $0; done = true }
            try await r.poll(timeoutMs: timeout, "smart mailbox saved") { done }
            if saved == nil { throw UIStepError(message: "smart mailbox not saved: \(model.smartMailboxSaveError ?? "invalid")") }
        case "deleteSmart":
            if let sm = model.smartMailboxes.first(where: { $0.name == s.name }) { model.deleteSmartMailbox(sm.id) }
        case "saveRule":
            struct Spec: Decodable {
                var group: FilterGroupDTO
                var actions: [RuleActionDTO]
                var stop: Bool?
                var account: String?
            }
            let spec = try decode(Spec.self, s.text)
            var d = RuleDraft()
            d.name = s.name ?? ""
            d.accountId = ref(spec.account)
            d.group = spec.group
            d.actions = spec.actions
            d.stopProcessing = spec.stop ?? false
            d.trigger = .manual
            var ok: Bool?
            model.saveRule(d) { ok = $0 }
            try await r.poll(timeoutMs: timeout, "rule saved") { ok != nil }
            // value == false: the scenario expects the save to be refused (it only asserts nothing was stored).
            if s.value == false {
                if ok == true { throw UIStepError(message: "rule was saved but a refusal was expected") }
            } else if ok != true {
                throw UIStepError(message: "rule not saved: \(model.ruleSaveError ?? d.validationProblem ?? "invalid")")
            }
        case "runRule":
            guard let rule = model.rules.first(where: { $0.name == s.name }) else { throw UIStepError(message: "no rule '\(s.name ?? "")'") }
            var out: String?
            model.runRuleNow(rule) { out = $0 }
            try await r.poll(timeoutMs: timeout, "rule run") { out != nil }
            note = out ?? ""
        case "previewRule":
            guard let rule = model.rules.first(where: { $0.name == s.name }) else { throw UIStepError(message: "no rule '\(s.name ?? "")'") }
            var out: String?
            model.previewRuleMatchCount(rule) { res in
                if case .success(let n) = res { out = String(n) } else { out = "error" }
            }
            try await r.poll(timeoutMs: timeout, "rule preview") { out != nil }
            note = out ?? ""
        case "remember":
            guard let id = model.selectedMessage?.id ?? model.detail?.header.id else { throw UIStepError(message: "no selected message") }
            rememberedId = id
        case "breakPassword":
            // "Break an account password": store a wrong password for the account and re-sync it (S73).
            guard let a = ref(s.accountRef), let cfg = model.storedConfig(for: a) else { throw UIStepError(message: "no stored config for \(s.accountRef ?? "")") }
            var c: AccountConfig
            let dec = JSONDecoder(); dec.keyDecodingStrategy = .convertFromSnakeCase
            do { c = try dec.decode(AccountConfig.self, from: Data(cfg.utf8)) } catch { throw UIStepError(message: "cannot decode the stored config: \(error)") }
            c.password = "wrong-password"
            guard let json = try? c.encodedJSON(), model.saveAccountWithoutSync(accountID: a, configJSON: json) else {
                throw UIStepError(message: "could not store the broken password")
            }
            model.retrySync(accountId: a)
        case "waitFor":
            let key = s.name ?? ""
            try await r.poll(timeoutMs: s.timeoutMs ?? 60_000, "reading:\(key) == \(s.text ?? "")") { value(key, runner: r) == s.text }
        case "projectLinks":
            try await r.poll(timeoutMs: timeout, "a project") { !model.projects.isEmpty }
            guard let p = model.projects.first else { return }
            // The link write is detached: retry until the auto-linked contact appears (or time runs out).
            let deadline = Date().addingTimeInterval(Double(timeout) / 1000)
            repeat {
                var out: String?
                model.loadProjectContactLinks(projectId: p.id) { recs in
                    out = recs.values.map { "\($0.origin)|\($0.provenance ?? "-")" }.sorted().joined(separator: ";")
                }
                try await r.poll(timeoutMs: timeout, "project links") { out != nil }
                note = out ?? ""
                if !note.isEmpty { break }
                try? await Task.sleep(nanoseconds: 500_000_000)
            } while Date() < deadline
        case "toggleRule":
            guard let rule = model.rules.first(where: { $0.name == s.name }) else { throw UIStepError(message: "no rule '\(s.name ?? "")'") }
            model.setRule(rule, enabled: s.value == true)
            try await r.poll(timeoutMs: timeout, "rule toggled") { model.rules.first { $0.name == s.name }?.enabled == (s.value == true) }
        case "moveRuleToTop":
            guard let i = model.rules.firstIndex(where: { $0.name == s.name }) else { throw UIStepError(message: "no rule '\(s.name ?? "")'") }
            model.moveRules(fromOffsets: IndexSet(integer: i), toOffset: 0)
            try await r.poll(timeoutMs: timeout, "rule moved") { model.rules.first?.name == s.name }
        case "setCounter":
            // The same two preference writes the Counters pickers make; smart mailboxes by name.
            let text = s.text ?? ""
            var mode = DockBadgeMode.off
            var smartId: String?
            switch text {
            case "off": mode = .off
            case "inbox": mode = .inboxUnread
            case "vip": mode = .vipUnread
            case _ where text.hasPrefix("smart:"):
                let name = String(text.dropFirst(6))
                guard let sm = model.smartMailboxes.first(where: { $0.name == name }) else {
                    throw UIStepError(message: "no smart mailbox named '\(name)'")
                }
                mode = .smartMailbox
                smartId = sm.id
            default: throw UIStepError(message: "unknown counter scope '\(text)'")
            }
            if s.name == "dock" {
                Preferences.shared.dockBadgeSmartMailboxId = smartId
                Preferences.shared.dockBadgeMode = mode
            } else {
                Preferences.shared.menuBarSmartMailboxId = smartId
                Preferences.shared.menuBarCounterMode = mode
            }
        case "setCountsInCounters":
            guard let a = ref(s.accountRef) else { return }
            Preferences.shared.setCountsInCounters(s.value == true, accountId: a)
        case "setWrite":
            guard let a = ref(s.accountRef) else { return }
            model.setWriteEnabled(accountId: a, enabled: s.value == true)
            try await r.poll(timeoutMs: timeout, "write policy") { model.accountWriteEnabled[a] == (s.value == true) }
        case "createTag":
            let created = model.createTag(displayName: s.name ?? "", colour: s.text ?? "grey")
            if s.value == false {
                // `"value": false` expects a refusal (FP43: reserved names).
                guard created == nil else { throw UIStepError(message: "tag unexpectedly created") }
                // The refusal reason is the expected outcome: check it, then consume it
                // so the implicit noErrorAlert on later asserts does not flag it.
                guard model.errorText?.contains("reserved") == true else {
                    throw UIStepError(message: "refusal did not say the name is reserved")
                }
                model.errorText = nil
            } else {
                guard created != nil else { throw UIStepError(message: "tag not created") }
            }
        case "toggleTag": model.toggleTag(s.text ?? "")
        case "toggleMute": model.toggleMuteSelectedThread()
        case "snooze": model.snoozeSelected(until: Int64(Date().timeIntervalSince1970) + (Int64(s.text ?? "") ?? 60))
        case "unsnooze": model.unsnoozeSelected()
        case "scheduleSend":
            guard let acct = ref(s.accountRef) else { return }
            let email = model.accounts.first { $0.id == acct }?.email ?? "alice@example.test"
            let id = try model.core.saveDraft(NewDraft(
                id: nil, accountId: acct, to: [DraftAddress(name: nil, email: email)],
                subject: s.text ?? "Scheduled", bodyText: "Scheduled by the UIT-1 harness.\n"))
            try model.core.scheduleSend(
                draftId: id, sendAt: Int64(Date().timeIntervalSince1970) + 86_400)
            model.refreshPendingCount()
        case "seedFailedSend":   // OB1: queue a send, then fail it for real against a closed local port
            guard let acct = ref(s.accountRef) else { return }
            let email = model.accounts.first { $0.id == acct }?.email ?? "alice@example.test"
            let id = try model.core.saveDraft(NewDraft(
                id: nil, accountId: acct, to: [DraftAddress(name: nil, email: email)],
                subject: s.text ?? "Failed", bodyText: "Queued by the UIT-1 harness.\n"))
            try model.core.sendDraft(id: id)
            // Port 1 on loopback refuses the connection, so the real SMTP path
            // records a classified network failure. No credentials are involved.
            let unreachable = """
            {"account_id":"\(acct)","from_email":"\(email)","smtp":{"host":"127.0.0.1","port":1,\
            "username":"uit","password":"","use_tls":false}}
            """
            _ = try model.core.sendDraftNow(configJSON: unreachable, draftId: id)
            model.refreshPendingCount()
            try await r.poll(timeoutMs: timeout, "outbox count") { model.outboxMessageCount > 0 }
        case "seedServerDraft":   // DR1: save a draft (name = recipient) and queue it for the server's Drafts folder
            guard let acct = ref(s.accountRef), let a = model.accounts.first(where: { $0.id == acct }) else { return }
            let id = try model.core.saveDraft(NewDraft(
                id: nil, accountId: acct, to: [DraftAddress(name: nil, email: s.name ?? "")],
                subject: s.text ?? "Draft", bodyText: "Seeded by the UIT-1 harness.\n"))
            _ = try model.core.saveDraftToServer(id: id, fromEmail: a.email, fromName: nil)
            model.replayNow()
        case "openDraftRow":   // DR1: what double-click / Return on the list row with this subject does
            guard let h = model.messages.first(where: { $0.subject == s.text }) else { throw UIStepError(message: "no list row '\(s.text ?? "")'") }
            model.select(message: h)
            if !model.openDraft(from: h) { throw UIStepError(message: "row '\(s.text ?? "")' is not a draft") }
        case "findSimilar":
            guard let h = model.detail?.header ?? model.selectedMessage else { throw UIStepError(message: "no selected message") }
            r.labels.expect("sheet:findSimilar")
            model.findSimilar(anchor: h)
        case "dismissFindSimilar": model.dismissFindSimilar()
        case "showPalette":
            r.labels.expect("sheet:palette")
            NotificationCenter.default.post(name: .castellumShowCommandPalette, object: nil)
        case "showSource":
            r.labels.expect("source")
            NotificationCenter.default.post(name: CommandRegistry.spec(for: .viewSource).notification!, object: nil)
        case "fieldText":
            let w = try r.window(s.window)
            guard let f = fields(in: w.contentView).first(where: { $0.isEditable }) else { throw UIStepError(message: "no editable text field in '\(s.window ?? "")'") }
            guard w.makeFirstResponder(f), let editor = f.currentEditor() as? NSTextView else { throw UIStepError(message: "could not focus the field") }
            editor.string = s.text ?? ""
            NotificationCenter.default.post(name: NSText.didChangeNotification, object: editor)
            f.stringValue = s.text ?? ""
        case "createProjectAssign":
            guard let m = model.selectedMessage else { throw UIStepError(message: "no selected message") }
            // Auto-links need contact rows: build them from the synced mail first (what opening Contacts does).
            model.refreshContactsNow()
            try await r.poll(timeoutMs: timeout, "contacts built") { !model.contacts.isEmpty }
            var pid: String?
            model.createProjectAndAssign(name: s.name ?? "Project", messageIds: [m.id]) { pid = $0 }
            try await r.poll(timeoutMs: timeout, "project created") { pid != nil }
        case "deleteSelected": model.deleteSelected()
        case "cancelDelete": model.cancelPermanentDelete()
        default: throw UIStepError(message: "unhandled reading action")
        }
        try? await Task.sleep(nanoseconds: 80_000_000)
    }

    private static func decode<T: Decodable>(_ type: T.Type, _ text: String?) throws -> T {
        guard let data = text?.data(using: .utf8) else { throw UIStepError(message: "missing JSON text") }
        do { return try JSONDecoder().decode(type, from: data) } catch { throw UIStepError(message: "bad JSON for \(T.self): \(error)") }
    }

    // MARK: view lookups

    static func fields(in v: NSView?) -> [NSTextField] {
        guard let v else { return [] }
        var out: [NSTextField] = []
        if let f = v as? NSTextField { out.append(f) }
        for s in v.subviews { out += fields(in: s) }
        return out
    }

    static func table(in v: NSView?) -> NSTableView? {
        guard let v else { return nil }
        if let t = v as? NSTableView, t.tableColumns.contains(where: { $0.identifier.rawValue == "subject" }) { return t }
        for s in v.subviews { if let t = table(in: s) { return t } }
        return nil
    }

    /// First `UIProbeView` with this id under `v` (nil when absent).
    static func probe(_ id: String, in v: NSView) -> NSView? {
        if let p = v as? UIProbeView, p.probeId == id { return p }
        for s in v.subviews { if let f = probe(id, in: s) { return f } }
        return nil
    }

    /// The text shown in one cell of the message list (`column` is the column identifier).
    private static func cellText(_ t: NSTableView, row: Int, column: String) -> String {
        guard row >= 0, row < t.numberOfRows,
              let ci = t.tableColumns.firstIndex(where: { $0.identifier.rawValue == column }),
              let cell = t.view(atColumn: ci, row: row, makeIfNecessary: true) else { return "<no cell>" }
        func texts(_ v: NSView) -> [String] {
            var o: [String] = []
            if let f = v as? NSTextField, !f.stringValue.isEmpty { o.append(f.stringValue) }
            for s in v.subviews { o += texts(s) }
            return o
        }
        return texts(cell).joined(separator: " ")
    }

    /// What the sidebar shows for a counter source, "none" when it shows no badge (nil = unknown source).
    private static func counterExpectation(_ spec: String, runner r: UIScriptRunner) -> String? {
        let m = r.model
        func label(_ n: Int64) -> String { n > 0 ? String(n) : "none" }
        if spec == "inbox" {
            return label(Int64(UnifiedInboxBadge.unreadCount(role: "inbox", mailboxes: m.mailboxes, counts: m.counts)))
        }
        if spec.hasPrefix("inbox:") {
            let ref = String(spec.dropFirst(6))
            let id = r.refs[ref] ?? ref
            return label(UnifiedInboxBadge.unreadByAccount(role: "inbox", mailboxes: m.mailboxes, counts: m.counts)[id] ?? 0)
        }
        if spec.hasPrefix("smart:") {
            let name = String(spec.dropFirst(6))
            return m.smartMailboxes.first { $0.name == name }
                .map { SmartMailboxBadge.label(unread: $0.cachedUnread, capped: $0.countsCapped) ?? "none" }
        }
        return nil
    }

    // MARK: expectations

    /// Value of a `reading:` model expectation key (nil = unknown key).
    static func value(_ key: String, runner r: UIScriptRunner) -> String? {
        let m = r.model
        let tbl = table(in: (try? r.window("main"))?.contentView)
        func b(_ x: Bool) -> String { String(x) }
        switch key {
        case "messageCount": return String(m.messages.count)
        case "dockBadgeLabel": return m.dockBadgeController?.currentLabel ?? "none"   // MM-9: "none" = no badge
        case _ where key.hasPrefix("counterMatches:"):
            // counterMatches:<dock|menuBar>:<inbox|inbox:<ref>|smart:<name>> -> does that counter show what the
            // sidebar shows for it (the same UnifiedInboxBadge / SmartMailboxBadge numbers)? "none" = nothing.
            let parts = key.split(separator: ":", maxSplits: 2, omittingEmptySubsequences: false).map(String.init)
            guard parts.count == 3 else { return nil }
            let shown = parts[1] == "dock" ? (m.dockBadgeController?.currentLabel ?? "none")
                : (m.menuBarCounterController?.currentTitle ?? "off")
            guard let want = counterExpectation(parts[2], runner: r) else { return nil }
            return b(shown == want || (shown == "" && want == "none"))
        case "dockBadgeShown": return b(m.dockBadgeController?.currentLabel != nil)   // guards counterMatches passing on "none" == "none"
        case "menuBarTitle": return m.menuBarCounterController?.currentTitle ?? "off"   // "off" = no item, "" = icon only
        case _ where key.hasPrefix("smartUnread:"):   // the sidebar row's number
            let name = String(key.dropFirst(12))
            return m.smartMailboxes.first { $0.name == name }
                .map { SmartMailboxBadge.label(unread: $0.cachedUnread, capped: $0.countsCapped) ?? "none" } ?? "missing"
        case "undoDepth": return String(m.removalUndoStack.depth)   // #3022
        case _ where key.hasPrefix("rowFor:"):   // rowFor:<subject> -> the From/To column text of the first row with it
            return m.messages.first { $0.subject == String(key.dropFirst(7)) }.map { messageListSenderText($0) } ?? "none"
        case "rowSenders": return m.messages.map { messageListSenderText($0) }.joined(separator: " | ")
        case "subjects": return m.messages.map { $0.subject ?? "" }.joined(separator: " | ")
        case "isUnified": return b(m.isUnifiedList)
        case "isSnoozedList": return b(m.selectedIsSnoozed)
        case "mailboxRole": return m.selectedMailbox?.role ?? "none"
        case "smartSelected": return m.smartMailboxes.first { $0.id == m.selectedSmartMailboxId }?.name ?? "none"
        case "keyProblems": return m.keyBindings.problems.isEmpty ? "none" : m.keyBindings.problems.map(\.kind.rawValue).joined(separator: ",")
        case "keyProblemText": return m.keyBindings.problems.first?.message ?? "none"
        case "selectedIndex":   // MM-12: row index of the selected message in the loaded list, or "none"
            return m.selectedMessage.flatMap { sel in m.messages.firstIndex { $0.id == sel.id } }.map(String.init) ?? "none"
        case "selectedSubject": return m.detail?.header.subject ?? m.selectedMessage?.subject ?? "none"
        case "selectedId": return m.selectedMessage?.id ?? m.detail?.header.id ?? "none"
        case "rememberedInList": return b(rememberedId.map { id in m.messages.contains { $0.id == id } } ?? false)
        case "rememberedSnoozed": return b(rememberedId.map { id in m.snoozedMessages.contains { $0.id == id } } ?? false)
        case "hasInvite": return b(m.detail?.calendarInvite != nil)
        case "invite": return m.detail?.calendarInvite.map { "\($0.method)|\($0.summary ?? "-")|\($0.organizerEmail ?? "-")|\($0.attendees.count)" } ?? "none"
        case "selectedUnread": return b(m.selectedMessage.map { !$0.isRead } ?? false)
        case "selectedMuted": return b(m.selectedMessage?.threadMuted ?? m.detail?.header.threadMuted ?? false)
        case "selectedKeywords": return (m.selectedMessage?.keywords ?? m.detail?.header.keywords ?? []).joined(separator: ",")
        case "attachmentCount": return String(m.detail?.attachments?.count ?? 0)
        case "trustVerdict": return m.detail?.trust.map { "\($0)" } ?? "none"
        case "searchText": return m.searchText
        case "searchScope": return m.searchScope.rawValue
        case "layout": return Preferences.shared.viewerLayout.rawValue
        case "correspondence": return m.correspondence?.address ?? "none"
        case "corrTotal": return String(m.correspondenceStats.total)
        case "corrSent": return String(m.correspondenceStats.sent)
        case "corrReceived": return String(m.correspondenceStats.received)
        case "corrTotalPositive": return b(m.correspondenceStats.total > 0)
        case "corrMonthsPositive": return b(m.correspondenceStats.perMonth.count > 0)
        case "hasMessages": return b(!m.messages.isEmpty)
        case "corrMonths": return String(m.correspondenceStats.perMonth.count)
        case "smartNames": return m.smartMailboxes.map(\.name).sorted().joined(separator: ",")
        case "smartCount": return String(m.smartMailboxes.count)
        case "ruleNames": return m.rules.map(\.name).joined(separator: ",")
        case "ruleCount": return String(m.rules.count)
        case "note": return note
        case "bareKeyOutcome": return lastBareKeyOutcome
        case "goToTagPicker": return b(m.goToTagPickerShown)
        case _ where key.hasPrefix("keywordsOf:"):   // keywordsOf:<subject> -> that listed row's tags, comma-joined
            return m.messages.first { $0.subject == String(key.dropFirst(11)) }?.keywords.joined(separator: ",") ?? "none"
        case "selectedTag":   // MM-8: display name of the open sidebar tag, or "none"
            return m.tags.first { $0.keywordKey == m.selectedTagKey }?.displayName ?? "none"
        case _ where key.hasPrefix("tagCount:"):   // MM-8: tagCount:<display name> -> "total/unread" as the sidebar badge sees it
            let name = String(key.dropFirst(9))
            guard let tag = m.tags.first(where: { $0.displayName == name }), let c = m.tagCounts[tag.keywordKey]
            else { return "none" }
            return "\(c.total)/\(c.unread)"
        case _ where key.hasPrefix("tagBadgeAgrees:"):   // MM-8: the open tag list's rows match that tag's badge numbers
            let name = String(key.dropFirst(15))
            guard let tag = m.tags.first(where: { $0.displayName == name }), let c = m.tagCounts[tag.keywordKey],
                  m.selectedTagKey == tag.keywordKey else { return "false" }
            return b(Int64(m.messages.count) == c.total && Int64(m.messages.filter { !$0.isRead }.count) == c.unread)
        case "tagNames": return m.tags.map(\.displayName).sorted().joined(separator: ",")
        case "tagNotice": return m.lastTagLocalOnlyNotice ?? "none"
        case "snoozedCount": return String(m.snoozedMessages.count)
        case "pendingDelete": return b(m.pendingPermanentDelete != nil)
        case "findSimilarShown": return b(m.findSimilarAnchor != nil)
        case "findSimilarError": return m.findSimilarError ?? "none"
        case "spotlight": return b(m.spotlightIndexingEnabled)
        case "accountCount": return String(m.accounts.count)
        case "syncBanner": return m.syncBanner.map { "\($0.state.rawValue): \($0.message)" } ?? "none"
        case "bannerOverlapsHeader":   // F3: sync banner vs message-list column header
            guard let cv = (try? r.window("main"))?.contentView else { return "no window" }
            guard let banner = probe("syncBanner", in: cv) else { return "no banner" }
            guard let hdr = tbl?.headerView else { return "no header" }
            let bf = cv.convert(banner.bounds, from: banner)
            let hf = cv.convert(hdr.bounds, from: hdr)
            return b(bf.intersects(hf))
        case "bannerWithinWindow":   // F3: the banner must be on-screen, not laid out above the window's top edge
            guard let cv = (try? r.window("main"))?.contentView else { return "no window" }
            guard let banner = probe("syncBanner", in: cv) else { return "no banner" }
            let bf = cv.convert(banner.bounds, from: banner)
            return b(cv.bounds.insetBy(dx: -0.5, dy: -0.5).contains(bf))
        case "windowMinHeightCompact":   // F3: the banner must not inflate the window's minimum height
            // (the squeezed-text bug measured 1126 pt; Classic's own floor is ~524 pt).
            guard let w = try? r.window("main") else { return "no window" }
            return b(w.contentMinSize.height < 600)
        case "trust": return m.detail?.trust.map { "spf=\($0.spf ?? "-") dkim=\($0.dkim ?? "-") dmarc=\($0.dmarc ?? "-")" } ?? "none"
        case "hasCalendarPart": return b(m.detail?.attachments?.contains { ($0.contentType ?? "").lowercased().contains("calendar") } == true)
        case "hasRemoteContent": return b(m.detail?.hasRemoteContent ?? false)
        case "projectCount": return String(m.projects.count)
        case "errorText": return m.errorText ?? "none"
        case "tableFound": return b(tbl != nil)
        case "columns": return tbl?.tableColumns.map { $0.identifier.rawValue + ($0.isHidden ? "(hidden)" : "") }.joined(separator: ",") ?? "no table"
        case "autosave": return tbl?.autosaveName ?? "none"
        case "autosaveColumns": return b(tbl?.autosaveTableColumns ?? false)
        default:
            if let split = UISplits.value(key, model: m) { return split }
            if key == "dateColumnVisible" {   // #3042: not hidden AND its right edge inside the clip view
                guard let t = tbl, let i = t.tableColumns.firstIndex(where: { $0.identifier.rawValue == "date" }),
                      !t.tableColumns[i].isHidden, let clip = t.enclosingScrollView?.contentView else { return "false" }
                return b(t.rect(ofColumn: i).maxX <= clip.bounds.width + 0.5)
            }
            if key == "smartNotice" { return m.smartMailboxNotice ?? "none" }   // #3382: the banner text
            if key == "detailBlocks" { return String(PlainTextStructure.parse(m.detail?.textBody ?? "").count) }   // #2807
            if key == "detailFolded" { return String(m.plainFold.foldedCount(PlainTextStructure.parse(m.detail?.textBody ?? ""))) }
            if key == "readingSelection" { return m.readingSelection?.text ?? "" }
            if key == "sort" { return m.effectiveSort.label }   // #3043, e.g. "from:asc"
            if key == "firstRowSubjects" { return m.messages.prefix(5).map { $0.subject ?? "" }.joined(separator: "|") }
            if key == "sortIndicator" {   // the table's first sort descriptor: proves the VIEW followed the model
                guard let d = tbl?.sortDescriptors.first, let k = d.key else { return "none" }
                return "\(k):\(d.ascending ? "asc" : "desc")"
            }
            if key == "paneWidth" { return tbl?.enclosingScrollView.map { String(Int($0.contentView.bounds.width)) } ?? "none" }
            if key == "columnsRight" {   // right edge of the last visible column, for the capture log
                guard let t = tbl, let last = t.tableColumns.lastIndex(where: { !$0.isHidden }) else { return "none" }
                return String(Int(t.rect(ofColumn: last).maxX))
            }
            if key.hasPrefix("tagDots:") {   // #2245: the tag dots (and mute glyph) a row SHOWS:
                // the Tags cell while that column is visible, else the run leading the Subject
                // cell. tagDots:<row>, <row> = "selected" or an index; "" = nothing shown.
                let arg = String(key.dropFirst(8))
                let selectedRow = m.messages.firstIndex { $0.id == m.selectedMessage?.id }
                guard let t = tbl, let row = arg == "selected" ? selectedRow : Int(arg),
                      row >= 0, row < m.messages.count else { return "no table" }
                let tagsShown = t.tableColumns.first { $0.identifier.rawValue == "tags" }.map { !$0.isHidden } ?? false
                if tagsShown { return cellText(t, row: row, column: "tags") }
                let n = TagsCell.attributedString(for: m.messages[row], tags: m.tags).length
                guard n > 0 else { return "" }
                return String(cellText(t, row: row, column: "subject").prefix(n))
            }
            if key.hasPrefix("columnVisible:") {
                let id = String(key.dropFirst(14))
                return b(tbl?.tableColumns.first { $0.identifier.rawValue == id }.map { !$0.isHidden } ?? false)
            }
            if key.hasPrefix("columnTitle:") {
                let id = String(key.dropFirst(12))
                return tbl?.tableColumns.first { $0.identifier.rawValue == id }?.title ?? "none"
            }
            if key.hasPrefix("cell:") {   // cell:<column>:<row>; <row> = "selected" for the selected message's row
                let p = key.dropFirst(5).split(separator: ":").map(String.init)
                // "selected" keeps a scenario independent of row order, which shifts when the
                // shared GreenMail account holds more mail than the scenario seeded. It is the
                // model's selected message (the table rows mirror `m.messages`).
                let selectedRow = m.messages.firstIndex { $0.id == m.selectedMessage?.id }
                guard p.count == 2, let t = tbl,
                      let row = p[1] == "selected" ? selectedRow : Int(p[1]) else { return "no table" }
                return cellText(t, row: row, column: p[0])
            }
            if key.hasPrefix("smartTotal:") {
                return m.smartMailboxes.first { $0.name == String(key.dropFirst(11)) }.map { String($0.cachedTotal) } ?? "none"
            }
            if key.hasPrefix("sourceHas:") {   // text of the message-source window (label "source")
                guard let w = try? r.window("source") else { return "no source window" }
                func tv(_ v: NSView) -> [NSTextView] { (v as? NSTextView).map { [$0] } ?? v.subviews.flatMap(tv) }
                let text = w.contentView.map { tv($0).map(\.string).joined() } ?? ""
                return b(text.contains(String(key.dropFirst(10))))
            }
            if key.hasPrefix("segment:") {     // selected label of the first segmented control in window <label>
                guard let w = try? r.window(String(key.dropFirst(8))) else { return "no window" }
                func seg(_ v: NSView) -> NSSegmentedControl? { (v as? NSSegmentedControl) ?? v.subviews.lazy.compactMap(seg).first }
                guard let c = w.contentView.flatMap(seg), c.selectedSegment >= 0 else { return "none" }
                return c.label(forSegment: c.selectedSegment) ?? "none"
            }
            if key.hasPrefix("noteHas:") { return b(note.contains(String(key.dropFirst(8)))) }
            // Plain-text bodies are SwiftUI Text (already in the PNG); this checks the model's copy.
            if key.hasPrefix("detailTextHas:") { return b((m.detail?.textBody ?? "").contains(String(key.dropFirst(14)))) }
            if key.hasPrefix("webBodyHas:") { return b(UICapture.lastWebText.contains { $0.contains(String(key.dropFirst(11))) }) }
            if key.hasPrefix("hasSubject:") { return b(m.messages.contains { ($0.subject ?? "").contains(String(key.dropFirst(11))) }) }
            if key.hasPrefix("ruleEnabled:") {
                return m.rules.first { $0.name == String(key.dropFirst(12)) }.map { b($0.enabled) } ?? "none"
            }
            return nil
        }
    }
}
#endif
