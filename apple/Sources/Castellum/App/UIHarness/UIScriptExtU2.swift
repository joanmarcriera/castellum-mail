// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 extension ops for Compose, Send and AI scenarios (Debug builds only).
//
// Usage: a scenario step `{"op":"ext","id":"<action>", ...}` is routed here by the runner, and an
// expectation `{"model":"ext:<key>","equals":"<value>"}` is evaluated by `UIExtU2.value`.
// Kept in its own file so sibling harness extensions merge cleanly; the shared runner has only
// additive one-line hooks. Everything calls real app entry points (model methods, notifications,
// AppKit controls); nothing here is reachable from a Release build (`#if DEBUG`).
//
// Actions (fields reuse UIStep: id=action, window, find, text, name, email, timeoutMs):
//   selectMailbox   email=<account email> text=<role, e.g. inbox>
//   selectMessage   text=<subject substring>   (waits for the list, then opens it in the reading pane)
//   setBody         window text=<body>         (the compose TextEditor, an NSTextView)
//   appendBody      window text=<text>
//   setPopup        window find text=<item title>  (a Picker / NSPopUpButton)
//   chooseSignature window text=<signature name|None>   (the compose footer picker's setter, #3142)
//   setPref         name=<pref> text=<value>   (aiEnabled aiEndpoint aiModel undoSendDelay markdownTheme signature:<email>)
//   aiUseFirstModel                            (lists models at a loopback endpoint, sets the smallest as aiModel)
//   waitAiJob       name=<feature> text=<status|"any"> (poll until a job of that feature has that status)
//   Also: describe (lists a window's controls as an error), pressHint, setToggle, setField (to|cc|bcc|subject),
//   keyEquivalent ("cmd+return" sends: SwiftUI buttons have no NSView), composeSet (markdown|theme),
//   rowMenu (text=row index: builds the row context menu the delegate would, without popping it),
//   rowMenuInvoke (text=<CommandID>|moveTo:<folder name>|enableWrites:<email>; server-writing ids need allowServerWrites), selectRows
//   (text="0,1,2"), closeCompose, waitExt, expectPopupItems, enableWrites (@local.test only), createIdentity, openFirstDraft,
//   loadSource (raw RFC 822 of the selected message), aiProbeFirstLaunch, aiDraftMessage, aiCancelRunning,
//   findSimilar, pause, setAutoconfigFixture (text=AutoconfigReport JSON; account-setup lookups return it, MM-14),
//   keys (window text=<key sequence, e.g. "g m">: synthetic key-downs through the window's BareKeyMonitorView, MM-12). `"window": "@compose"` addresses whichever compose window is open. `{run}` in step
//   text and keys is replaced by a per-run token so subjects are unique in the persistent GreenMail.
// Expectation keys (`ext:`): rowMenuTitles ("A|B|-|Sub[x|y]"), rowMenuHas:<exact title>, rowMenuDisabled:<CommandID>, rowMenuReason:<CommandID>, undoBanner, undoBannerRemaining, pendingCount, session.inlineCount, session.bodyHas:<text>, snippetPicker, snippetCount, snippetsWindow, undoDepth, removalBanner, removalBannerText, aiJobs, aiJobsRunning,
//   aiJob.<feature>, pref.<name>, composeWindows, body, bodyPrefix:<s>, bodyHas:<s>, subject, extractions,
//   signature, signaturePopup, signatureNotice, htmlHas:<s>
#if DEBUG
import AppKit
import Foundation
import SwiftUI

@MainActor
enum UIExtU2 {
    nonisolated static let actions: Set<String> = [
        "selectMailbox", "selectMessage", "setBody", "appendBody", "setPopup", "setPref",
        "aiUseFirstModel", "waitAiJob", "describe", "pressHint", "setToggle", "setField", "keyEquivalent", "enableWrites", "waitExt", "expectPopupItems", "createIdentity", "openFirstDraft", "loadSource", "composeSet", "closeCompose", "aiDraftMessage", "findSimilar", "aiProbeFirstLaunch", "aiCancelRunning", "pause", "chooseSignature", "rowMenu", "rowMenuInvoke", "selectRows", "moveSelectedTo", "snippetAdd", "insertSnippet", "openSnippetPicker", "showSnippets", "composeEmbedImage", "setAutoconfigFixture", "keys",
    ]

    /// Domains the account-setup lookup was asked about since `setAutoconfigFixture` (the `ext:autoconfigDomains` key, comma-joined).
    static var autoconfigDomains: [String] = []

    /// Validation problems for an `ext` step (checked before any step runs).
    nonisolated static func problems(_ s: UIStep) -> [String] {
        guard let a = s.id, actions.contains(a) else { return ["unknown ext action '\(s.id ?? "")'"] }
        switch a {
        case "setAutoconfigFixture": return s.text == nil ? ["setAutoconfigFixture needs text (an AutoconfigReport as JSON, snake_case keys)"] : []
        case "selectRows": return s.text == nil ? ["selectRows needs text (comma-separated row indexes)"] : []
        case "snippetAdd": return s.name == nil || s.text == nil ? ["snippetAdd needs name and text (the format); optional email scopes it to that account"] : []
        case "insertSnippet": return s.text == nil ? ["insertSnippet needs text (the snippet's name)"] : []
        case "openSnippetPicker", "showSnippets": return []
        case "composeEmbedImage": return s.window == nil ? ["composeEmbedImage needs window"] : []
        case "moveSelectedTo": return s.text == nil || s.allowServerWrites != true ? ["moveSelectedTo needs text (folder name) and allowServerWrites"] : []
        case "rowMenu": return s.text == nil ? ["rowMenu needs text (row index)"] : []
        case "rowMenuInvoke": return s.text == nil ? ["rowMenuInvoke needs text (command id or enableWrites:<email>)"] : []
        case "selectMailbox": return s.email == nil || s.text == nil ? ["selectMailbox needs email and text"] : []
        case "selectMessage": return s.text == nil ? ["selectMessage needs text"] : []
        case "setBody", "appendBody": return s.window == nil || s.text == nil ? ["\(a) needs window and text"] : []
        case "setPopup": return s.window == nil || s.find == nil || s.text == nil ? ["setPopup needs window, find, text"] : []
        case "setPref": return s.name == nil || s.text == nil ? ["setPref needs name and text"] : []
        case "chooseSignature": return s.window == nil || s.text == nil ? ["chooseSignature needs window and text (name or None)"] : []
        case "waitExt": return s.name == nil || s.text == nil ? ["waitExt needs name (ext key) and text (value)"] : []
        case "expectPopupItems": return s.window == nil || s.find == nil || s.text == nil ? ["expectPopupItems needs window, find, text"] : []
        case "createIdentity": return s.email == nil || s.text == nil ? ["createIdentity needs email and text"] : []
        case "openFirstDraft": return s.email == nil ? ["openFirstDraft needs email"] : []
        case "loadSource", "closeCompose", "findSimilar", "aiProbeFirstLaunch", "aiCancelRunning", "pause": return []
        case "aiDraftMessage": return s.window == nil || s.text == nil ? ["aiDraftMessage needs window and text"] : []
        case "composeSet": return s.window == nil || s.name == nil ? ["composeSet needs window and name"] : []
        case "enableWrites": return s.email == nil ? ["enableWrites needs email"] : []
        case "describe": return s.window == nil ? ["describe needs window"] : []
        case "keys": return s.window == nil || s.text == nil ? ["keys needs window and text (a key sequence in keybindings.json syntax, e.g. \"g m\" or \"⌃x s\")"] : []
        case "pressHint", "setToggle", "keyEquivalent": return s.window == nil || s.text == nil ? ["\(a) needs window and text"] : []
        case "setField": return s.window == nil || s.name == nil || s.text == nil ? ["setField needs window, name, text"] : []
        case "waitAiJob": return s.name == nil || s.text == nil ? ["waitAiJob needs name (feature) and text (status or any)"] : []
        default: return []
        }
    }

    /// `ext:` keys read from SwiftUI Pickers: on macOS 27 they exist only in the accessibility
    /// tree, which the runner lets settle before evaluating them (see `UIAXDriver`).
    nonisolated static func needsAccessibility(_ model: String?) -> Bool {
        ["ext:fromPopup", "ext:sendAs", "ext:signaturePopup"].contains(model ?? "")
    }

    /// Per-process token substituted for `{run}` in step text and expectation keys, so a subject is
    /// unique per run (GreenMail mailboxes persist across runs; a fixed subject would count twice).
    /// Raw source loaded by `loadSource` (read by `src.*` expectations).
    static var lastSource = ""
    static let runToken = String(Int(Date().timeIntervalSince1970))
    /// A real 2x2 RGBA PNG for `composeEmbedImage`.
    static let generatedPNG: Data = {
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: 2, pixelsHigh: 2, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        return rep.representation(using: .png, properties: [:])!
    }()
    static func sub(_ t: String?) -> String? { t?.replacingOccurrences(of: "{run}", with: runToken) }

    static func perform(_ step: UIStep, model: AppModel, window givenWindow: NSWindow?) async throws {
        // `"window": "@compose"` = whichever compose window is open (its title varies with the subject).
        let window = givenWindow ?? (step.window == "@compose" ? composeWindow() : nil)
        if step.window == "@compose" && window == nil { throw UIStepError(message: "no compose window is open") }
        var s = step
        s.text = sub(s.text); s.name = sub(s.name)
        let timeout = s.timeoutMs ?? 10_000
        switch s.id ?? "" {
        case "setAutoconfigFixture":   // text = AutoconfigReport JSON: every later account-setup lookup returns it (MM-14); the network is never touched
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            guard let data = s.text?.data(using: .utf8), let fixture = try? decoder.decode(AutoconfigReport.self, from: data)
            else { throw UIStepError(message: "setAutoconfigFixture: text is not a valid AutoconfigReport") }
            Self.autoconfigDomains = []
            model.autoconfigLookupOverride = { domain in
                await MainActor.run { UIExtU2.autoconfigDomains.append(domain) }
                return fixture
            }
        case "snippetAdd":   // name=<snippet name> text=<format> [email=<account address; absent = every account>] [tab=abbreviation]
            let prefs = Preferences.shared
            let account = s.email.flatMap { e in model.accounts.first { $0.email == e }?.id }
            var item = prefs.templateLibrary.add(name: s.name ?? "Snippet", accountId: account, text: s.text ?? "")
            if let abbreviation = s.tab, !abbreviation.isEmpty { item.abbreviation = abbreviation }
            let problems = prefs.templateLibrary.update(item)
            if let first = problems.first { throw UIStepError(message: "snippetAdd: \(first.message)") }
        case "composeEmbedImage":   // window = compose; text = file name. Embeds a generated 2x2 PNG at the caret, as a paste with Markdown on does (MM-5b)
            guard let w = window, let session = composeSession(of: w) else { throw UIStepError(message: "window has no ComposeView session") }
            session.embedImage(data: Self.generatedPNG, filename: s.text ?? "Test Image.png", model: model)
        case "insertSnippet":   // text = snippet name: what choosing it in the picker does, minus the click (#3158)
            guard let session = ComposeWindowController.sessionForHarness else { throw UIStepError(message: "no compose window is open") }
            guard let item = Preferences.shared.templateLibrary.snippets(for: session.accountId).first(where: { $0.name == s.text })
            else { throw UIStepError(message: "no snippet '\(s.text ?? "")' for this account") }
            session.insertPickedSnippet(item, model: model)
        case "openSnippetPicker":   // the Insert Snippet… command (palette / menu / Option-Cmd-I)
            NotificationCenter.default.post(name: .castellumInsertSnippet, object: nil)
        case "showSnippets":
            NotificationCenter.default.post(name: .castellumShowSnippets, object: nil)
        case "moveSelectedTo":   // text = folder name of the selected message's account: Move to Folder, minus the popover (#3022 S4)
            guard case .ok(let accountId) = MoveToFolderPlan.eligibility(ids: model.currentSelectionIds(), rows: model.messages),
                  let box = (model.mailboxes[accountId] ?? []).first(where: { $0.name == s.text || $0.path == s.text })
            else { throw UIStepError(message: "moveSelectedTo: no such folder '\(s.text ?? "")' for the selection") }
            model.moveSelected(toMailboxId: box.id)
        case "selectRows":   // text = "0,1,2": selects those list rows (a multi-selection) like a click would
            guard let coord = rowMenuCoordinator(), let t = coord.table else { throw UIStepError(message: "no message list table") }
            t.selectRowIndexes(IndexSet((s.text ?? "").split(separator: ",").compactMap { Int($0) }), byExtendingSelection: false)
        case "rowMenu":   // text = row index; builds the menu the delegate would, without popping it
            guard let coord = rowMenuCoordinator() else { throw UIStepError(message: "no message list table") }
            _ = coord.prepareRowMenu(forRow: Int(s.text ?? "") ?? -1)
        case "rowMenuInvoke":   // text = CommandID raw value, or "enableWrites:<email>"
            guard let coord = rowMenuCoordinator() else { throw UIStepError(message: "no message list table") }
            let raw = s.text ?? ""
            if raw.hasPrefix("moveTo:") {   // moveTo:<folder name>: the "Move to ▸" destination with that title
                let name = String(raw.dropFirst(7))
                guard let entry = MessageContextMenu.allMoveTargets(in: coord.lastRowMenu).first(where: { $0.title == name })
                else { throw UIStepError(message: "row menu has no Move to destination '\(name)'") }
                if s.allowServerWrites != true { throw UIStepError(message: "moveTo may write to a server; set allowServerWrites") }
                coord.invokeRowMenuEntry(entry)
            } else if raw.hasPrefix("enableWrites:") {
                let email = String(raw.dropFirst(13))
                guard let entry = coord.lastRowMenu.first(where: {
                    if case .enableWrites(let e, _) = $0 { return e == email } else { return false }
                }) else { throw UIStepError(message: "row menu has no Enable Writes for \(email)") }
                coord.invokeRowMenuEntry(entry)
            } else {
                guard let id = CommandID(rawValue: raw),
                      let entry = MessageContextMenu.find(id, in: coord.lastRowMenu) else {
                    throw UIStepError(message: "row menu has no command '\(raw)'")
                }
                // Same gate as the `command` op: server-writing items need allowServerWrites.
                if MessageContextMenu.serverMutating.contains(id), s.allowServerWrites != true {
                    throw UIStepError(message: "command \(raw) may write to a server; set allowServerWrites")
                }
                coord.invokeRowMenuEntry(entry)
            }
        case "selectMailbox":
            guard let acct = model.accounts.first(where: { $0.email == s.email }) else {
                throw UIStepError(message: "no account \(s.email ?? "")")
            }
            try await poll(timeout, "mailboxes of \(acct.email)") { model.mailboxes[acct.id]?.contains { $0.role == s.text } == true }
            guard let mb = model.mailboxes[acct.id]?.first(where: { $0.role == s.text }) else { return }
            model.select(mailbox: mb)
        case "selectMessage":
            // text "=Subject" matches the whole subject; otherwise a substring.
            let raw = s.text ?? ""
            let exact = raw.hasPrefix("=")
            let want = exact ? String(raw.dropFirst()) : raw
            func hit(_ subject: String?) -> Bool { exact ? subject == want : (subject ?? "").contains(want) }
            try await poll(timeout, "message '\(want)' in the list") { model.messages.contains { hit($0.subject) } }
            guard let m = model.messages.first(where: { hit($0.subject) }) else { return }
            model.select(message: m)
            try await poll(timeout, "reading pane detail") { model.detail?.header.id == m.id }
        case "setBody", "appendBody":
            guard let w = window, let tv = textViews(in: w.contentView).first(where: { $0.isEditable }) else {
                throw UIStepError(message: "no editable text view (the compose body) in the window")
            }
            _ = w.makeFirstResponder(tv)
            let len = (tv.string as NSString).length
            let range = s.id == "setBody" ? NSRange(location: 0, length: len) : NSRange(location: len, length: 0)
            // insertText runs the same path as typing, so SwiftUI's binding updates.
            tv.insertText(s.text ?? "", replacementRange: range)
        case "setPopup":
            guard let w = window, let f = s.find else { throw UIStepError(message: "setPopup needs window and find") }
            guard let pop = UIViewTree.find(f, in: w).compactMap({ $0.view as? NSPopUpButton }).first else {
                // macOS 27: a SwiftUI Picker has no NSPopUpButton; drive its accessibility node.
                guard let e = await UIViewTree.resolveVisible(f, in: w), e.ax != nil else {
                    throw UIStepError(message: "no popup matching \(f)")
                }
                try await UIAXDriver.menu(of: e, choose: s.text ?? "")
                break
            }
            guard pop.itemTitles.contains(s.text ?? "") else {
                throw UIStepError(message: "popup has no item '\(s.text ?? "")'; items: \(pop.itemTitles)")
            }
            // Activating the menu item (not just selecting it) runs the same action a click on it would.
            if let i = pop.itemTitles.firstIndex(of: s.text ?? "") {
                pop.selectItem(at: i)
                pop.menu?.performActionForItem(at: i)
            }
        case "waitExt":   // name = ext key, text = wanted value; polls until equal
            try await poll(timeout, "ext:\(s.name ?? "") == '\(s.text ?? "")' (now '\(value(s.name ?? "", model: model) ?? "?")')") {
                value(s.name ?? "", model: model) == s.text
            }
        case "expectPopupItems":   // window, find (the popup), text = "a|b|c" exact item titles
            guard let w = window, let f = s.find else { throw UIStepError(message: "expectPopupItems needs window and find") }
            guard let pop = UIViewTree.find(f, in: w).compactMap({ $0.view as? NSPopUpButton }).first else {
                guard let e = await UIViewTree.resolveVisible(f, in: w), e.ax != nil else {
                    throw UIStepError(message: "no popup matching \(f)")
                }
                let titles = try await UIAXDriver.menu(of: e, choose: nil)   // opens, reads, cancels
                guard titles.joined(separator: "|") == s.text else {
                    throw UIStepError(message: "popup items \(titles.joined(separator: "|")) != expected \(s.text ?? "")")
                }
                break
            }
            guard pop.itemTitles.joined(separator: "|") == s.text else {
                throw UIStepError(message: "popup items \(pop.itemTitles.joined(separator: "|")) != expected \(s.text ?? "")")
            }
        case "createIdentity":
            // email = account, text = alias address, name = display name, label = "replyTo;bcc", value = default
            guard let acct = model.accounts.first(where: { $0.email == s.email }) else { throw UIStepError(message: "no account \(s.email ?? "")") }
            let extra = (s.label ?? ";").split(separator: ";", omittingEmptySubsequences: false).map(String.init)
            var ok: Bool?
            var failure: String?
            model.createIdentity(accountId: acct.id, email: s.text ?? "", displayName: s.name,
                                 replyTo: extra.first.flatMap { $0.isEmpty ? nil : $0 },
                                 bcc: extra.count > 1 && !extra[1].isEmpty ? extra[1] : nil,
                                 isDefault: s.value ?? false) { failure = $0; ok = ($0 == nil) }
            try await poll(timeout, "identity created") { ok != nil }
            if ok != true { throw UIStepError(message: "createIdentity failed: \(failure ?? "")") }
        case "openFirstDraft":   // email = account; reopens its first saved draft in a compose window
            guard let acct = model.accounts.first(where: { $0.email == s.email }) else { throw UIStepError(message: "no account \(s.email ?? "")") }
            try await poll(timeout, "a saved draft for \(acct.email)") {
                model.reloadDrafts(acct.id)
                return !(model.drafts[acct.id] ?? []).isEmpty
            }
            if let d = model.drafts[acct.id]?.first { model.openDraftForEditing(d) }
        case "chooseSignature":   // the footer picker's binding setter: on macOS 27 SwiftUI pop-ups
            // have no NSPopUpButton for setPopup to drive (see PR #224), so this calls the same entry point.
            guard let w = window, let session = composeSession(of: w) else { throw UIStepError(message: "needs a compose window") }
            if s.text == "None" { session.chooseSignature(nil); break }
            guard let item = Preferences.shared.signatureLibrary.items(for: session.accountId).first(where: { $0.name == s.text }) else {
                throw UIStepError(message: "no signature named '\(s.text ?? "")' on the From account")
            }
            session.chooseSignature(item.id)
        case "closeCompose":
            guard let w = window ?? composeWindow() else { throw UIStepError(message: "no compose window is open") }
            w.performClose(nil)
        case "aiDraftMessage":   // window=@compose, text = instruction: what the "Draft with AI" sheet's Draft button calls
            guard let w = window, let session = composeSession(of: w) else { throw UIStepError(message: "needs a compose window") }
            model.aiCoordinator.draftMessage(sessionId: session.composeSessionId, instruction: s.text ?? "",
                                             subject: session.subject, recipients: session.toText, existingBody: nil)
        case "findSimilar":   // the selected message as anchor (the reading-pane "Find Similar" button's call)
            guard let m = model.selectedMessage else { throw UIStepError(message: "no selected message") }
            model.findSimilar(anchor: m)
        case "aiProbeFirstLaunch":   // the real first-launch Ollama probe (loopback only); UI-script runs skip it at launch
            model.probeOllamaOnFirstLaunchIfNeeded()
            try await poll(timeout, "the first-launch probe to answer") { Preferences.shared.hasStoredAiEnabledPreference }
        case "aiCancelRunning":   // what the AI Activity row's Cancel button calls, for every running job
            for j in model.aiCoordinator.jobs.values where !j.isTerminal && j.id > 0 { model.aiCoordinator.cancel(jobId: j.id) }
        case "pause":   // text = milliseconds; lets a background job make progress before an assertion
            try await Task.sleep(nanoseconds: UInt64(Int(s.text ?? "1000") ?? 1000) * 1_000_000)
        case "loadSource":   // fetch the selected message's raw RFC 822 source into `src.*` expectations
            guard let m = model.selectedMessage else { throw UIStepError(message: "no selected message") }
            let path = try await model.fetchRawSourcePath(messageId: m.id, accountId: m.accountId)
            let raw = (try? String(contentsOfFile: path, encoding: .utf8)) ?? ""
            // Undo quoted-printable soft breaks and "=3D" so inline style="..." can be matched.
            lastSource = raw.replacingOccurrences(of: "=\r\n", with: "").replacingOccurrences(of: "=\n", with: "")
                .replacingOccurrences(of: "=3D", with: "=")
        case "enableWrites":
            // GreenMail personas only: the scenario's accounts are on 127.0.0.1, never a real server.
            guard let acct = model.accounts.first(where: { $0.email == s.email }), acct.email.hasSuffix("@local.test") else {
                throw UIStepError(message: "enableWrites only applies to @local.test personas")
            }
            model.setWriteEnabled(accountId: acct.id, enabled: true)
            try await poll(timeout, "writes enabled for \(acct.email)") { !model.writeState(for: acct.id).isReadOnly }
        case "setPref":
            try setPref(s.name ?? "", s.text ?? "", model: model)
        case "aiUseFirstModel":
            try await useFirstModel(prefer: s.text)
        case "waitAiJob":
            try await poll(timeout, "AI job \(s.name ?? "") \(s.text ?? "")") {
                model.aiCoordinator.jobs.values.contains { $0.feature == s.name && (s.text == "any" || $0.status == s.text) }
            }
        case "keys":
            guard let w = window else { throw UIStepError(message: "keys needs a window") }
            try sendKeys(s.text ?? "", to: w)
        case "describe", "pressHint", "setToggle", "setField", "keyEquivalent", "composeSet":
            guard let w = window else { throw UIStepError(message: "\(s.id ?? "") needs a window") }
            try await performControlAction(s, window: w)
        default: throw UIStepError(message: "unhandled ext action")
        }
        try? await Task.sleep(nanoseconds: 80_000_000)
    }

    /// MM-12: type a key sequence into a window the way the keyboard would. One synthetic key-down
    /// per stroke goes to the window's `BareKeyMonitorView.deliver` (the body of the real NSEvent
    /// monitor); an event it hands back is passed on with `sendEvent`, as the monitor would let it
    /// through. Nothing touches the real keyboard or mouse.
    private static func sendKeys(_ text: String, to w: NSWindow) throws {
        guard let sequence = KeySequence(display: text) else {
            throw UIStepError(message: "keys: '\(text)' is not a key sequence")
        }
        func monitorView(in v: NSView?) -> BareKeyMonitorView? {
            guard let v else { return nil }
            if let m = v as? BareKeyMonitorView { return m }
            for sub in v.subviews { if let m = monitorView(in: sub) { return m } }
            return nil
        }
        guard let monitor = monitorView(in: w.contentView?.superview ?? w.contentView) else {
            throw UIStepError(message: "keys: window has no key monitor (only the main window does)")
        }
        for stroke in sequence.strokes {
            var flags: NSEvent.ModifierFlags = []
            if stroke.control { flags.insert(.control) }
            if stroke.option { flags.insert(.option) }
            var chars = "", code: UInt16 = 0
            switch stroke.key {
            case .character(let c):
                chars = String(c)
                if c.isUppercase { flags.insert(.shift) }
            case .named(let n):
                if stroke.shift { flags.insert(.shift) }
                switch n {
                case .returnKey: (chars, code) = ("\r", 36)
                case .space: (chars, code) = (" ", 49)
                case .delete: (chars, code) = ("\u{7f}", 51)
                case .left: (chars, code) = ("\u{F702}", 123)
                case .right: (chars, code) = ("\u{F703}", 124)
                case .down: (chars, code) = ("\u{F701}", 125)
                case .up: (chars, code) = ("\u{F700}", 126)
                case .escape: (chars, code) = ("\u{1b}", 53)
                case .tab: (chars, code) = ("\t", 48)
                }
            }
            guard let ev = NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
                windowNumber: w.windowNumber, context: nil, characters: chars,
                charactersIgnoringModifiers: chars,
                isARepeat: false, keyCode: code) else {
                throw UIStepError(message: "keys: could not build a key event for '\(stroke.fileSyntax)'")
            }
            if let passed = monitor.deliver(ev) { w.sendEvent(passed) }
        }
    }

    private static func poll(_ ms: Int, _ what: String, _ p: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(Double(ms) / 1000)
        while !p() {
            if Date() > deadline { throw UIStepError(message: "timed out after \(ms) ms waiting for \(what)") }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
    }

    static func textViews(in v: NSView?) -> [NSTextView] {
        guard let v else { return [] }
        var out: [NSTextView] = []
        if let t = v as? NSTextView, !t.isFieldEditor { out.append(t) }
        for s in v.subviews { out += textViews(in: s) }
        return out
    }

    private static func setPref(_ name: String, _ value: String, model: AppModel) throws {
        let p = Preferences.shared
        switch name {
        case "aiEnabled": p.aiEnabled = (value == "true")
        case "aiEndpoint": p.aiEndpoint = value
        case "aiModel": p.aiModel = value
        case "aiEmbeddingModel": p.aiEmbeddingModel = value
        case "sendFlowed": p.sendFlowed = (value == "true")
        case "undoSendDelay":
            guard let d = UndoSendDelay(rawValue: Int(value) ?? -1) else { throw UIStepError(message: "bad undoSendDelay \(value)") }
            p.undoSendDelay = d
        case "markdownTheme":
            guard let t = MarkdownThemeChoice(rawValue: value) else { throw UIStepError(message: "bad markdownTheme \(value)") }
            p.markdownTheme = t
        default:
            if name.hasPrefix("signature:") {
                let email = String(name.dropFirst(10))
                guard let a = model.accounts.first(where: { $0.email == email }) else { throw UIStepError(message: "no account \(email)") }
                p.setSignature(value, for: a.id)
            } else { throw UIStepError(message: "unknown pref \(name)") }
        }
    }

    /// Lists the models at the configured endpoint (loopback only; anything else is refused so a
    /// scenario can never send text off-machine) and selects the smallest one.
    private static func useFirstModel(prefer: String?) async throws {
        let ep = Preferences.shared.aiEndpoint
        guard let url = URL(string: ep + "/api/tags"), ["127.0.0.1", "localhost"].contains(url.host ?? "") else {
            throw UIStepError(message: "aiUseFirstModel refuses non-loopback endpoint \(ep)")
        }
        var req = URLRequest(url: url)
        req.timeoutInterval = 3
        let (data, _) = try await URLSession.shared.data(for: req)
        struct Tags: Decodable {
            struct M: Decodable { var name: String; var size: Int64? }
            var models: [M]
        }
        let tags = try JSONDecoder().decode(Tags.self, from: data)
        // `prefer` = "a|b|c": the first name present wins; "largest"/"smallest" pick by size among chat models.
        let chat = tags.models.filter { !$0.name.contains("embed") }.sorted { ($0.size ?? 0) < ($1.size ?? 0) }
        let names = (prefer ?? "smallest").split(separator: "|").map(String.init)
        var pick: String?
        for n in names { if let m = tags.models.first(where: { $0.name == n }) { pick = m.name; break } }
        if pick == nil { pick = (names.contains("largest") ? chat.last : chat.first)?.name }
        guard let pick else { throw UIStepError(message: "no models at \(ep)") }
        Preferences.shared.aiModel = pick
    }

    /// The compose window: a titled window (not Settings/main) holding an editable text view.
    static func composeWindow() -> NSWindow? {
        UIWindows.candidates().first { w in
            w.sheetParent == nil && w.title != "Castellum" && w.title != "Castellum Dev" && w.title != "Settings"
                && textViews(in: w.contentView).contains { $0.isEditable }
        }
    }

    /// Value of an `ext:` model expectation key (nil = unknown key).
    /// The message list's coordinator (the row-menu delegate), found through its table.
    static func rowMenuCoordinator() -> MessageListView.Coordinator? {
        for w in NSApp.windows {
            if let t = UIReading.table(in: w.contentView),
               let c = t.delegate as? MessageListView.Coordinator { return c }
        }
        return nil
    }

    /// `rowMenuTitles` etc.: "-" is a separator, "Title>" a submenu (its items follow in brackets).
    static func describeRowMenu(_ entries: [MessageMenuEntry]) -> String {
        entries.map { e -> String in
            switch e {
            case .separator: return "-"
            case .submenu(let t, let inner): return "\(t)[\(describeRowMenu(inner))]"
            default: return e.title ?? ""
            }
        }.joined(separator: "|")
    }

    static func value(_ rawKey: String, model: AppModel) -> String? {
        let key = sub(rawKey) ?? rawKey
        if key.hasPrefix("rowMenuHas:") {   // "true" when an item with exactly that title is anywhere in the last row menu
            let want = String(key.dropFirst(11))
            func has(_ entries: [MessageMenuEntry]) -> Bool {
                entries.contains { e in
                    if e.title == want { return true }
                    if case .submenu(_, let inner) = e { return has(inner) }
                    return false
                }
            }
            return String(has(rowMenuCoordinator()?.lastRowMenu ?? []))
        }
        if key == "rowMenuTitles" { return describeRowMenu(rowMenuCoordinator()?.lastRowMenu ?? []) }
        if key.hasPrefix("rowMenuDisabled:") || key.hasPrefix("rowMenuReason:") {
            let reason = key.hasPrefix("rowMenuReason:")
            let raw = String(key.dropFirst(reason ? 14 : 16))
            guard let id = CommandID(rawValue: raw),
                  let entry = MessageContextMenu.find(id, in: rowMenuCoordinator()?.lastRowMenu ?? []),
                  case .command(_, _, let enabled, let why) = entry else { return "absent" }
            return reason ? (why ?? "none") : String(!enabled)
        }
        let ai = model.aiCoordinator
        func body() -> String {
            textViews(in: composeWindow()?.contentView).first { $0.isEditable }?.string ?? "<no compose window>"
        }
        switch key {
        // WS1: which write-safety prompt is pending, and which window (by title) carries a sheet.
        case "autoconfigDomains": return Self.autoconfigDomains.joined(separator: ",")
        case "writePrompt": return model.writeSafetyPrompt?.id ?? "none"
        case "sheetHosts":
            let hosts = NSApp.windows.filter { $0.isVisible && $0.attachedSheet != nil }.map(\.title).sorted()
            return hosts.isEmpty ? "none" : hosts.joined(separator: "|")
        case "sheetButtons":   // button titles of the (single) attached sheet, in the order they were added
            guard let sheet = NSApp.windows.first(where: { $0.isVisible && $0.attachedSheet != nil })?.attachedSheet else { return "none" }
            return controls(in: sheet).compactMap { $0 as? NSButton }.map(\.title).filter { !$0.isEmpty }
                .sorted().joined(separator: "|")
        case "readOnlyAccounts":
            let emails = model.accountsReadOnly.map(\.email).sorted()
            return emails.isEmpty ? "none" : emails.joined(separator: "|")
        case "session.inlineCount": return composeWindow().flatMap { composeSession(of: $0) }.map { String($0.attachments.filter(\.isInline).count) } ?? "<none>"
        case "session.markdown": return composeWindow().flatMap { composeSession(of: $0) }.map { String($0.markdownEnabled) } ?? "<none>"
        case "session.theme": return composeWindow().flatMap { composeSession(of: $0) }.map { $0.markdownTheme.rawValue } ?? "<none>"
        case "detail.thread": return model.detail?.threadId ?? "nil"
        case "detail.subject": return model.detail?.header.subject ?? "nil"
        case "aiJobStates": return ai.jobs.values.map { "\($0.feature):\($0.status):\($0.errorCode ?? "-")" }.sorted().joined(separator: ",")
        // F1 background embeddings: the counts the AI Activity window shows, and the state.
        case "embed.queued": return String(model.aiCoordinator.embeddings.progress.queued)
        case "embed.done": return String(model.aiCoordinator.embeddings.progress.done)
        case "embed.drained":   // true once at least one vector is stored and nothing is left queued
            let p = model.aiCoordinator.embeddings.progress
            return String(p.done > 0 && p.queued == 0)
        case "embed.state":   // off | waitingForList | running | idle | backingOff | failed:<code>
            switch model.aiCoordinator.embeddings.progress.state {
            case .off: return "off"
            case .waitingForList: return "waitingForList"
            case .running: return "running"
            case .idle: return "idle"
            case .backingOff: return "backingOff"
            case .failed(let code): return "failed:\(code)"
            }
        case "embed.error": return EmbeddingDrainer.errorText(for: model.aiCoordinator.embeddings.progress) ?? "none"
        case "similar.inFlight": return String(model.findSimilarInFlight)
        case "similar.hasResults": return String(!model.findSimilarResults.isEmpty)
        case "similar.results": return String(model.findSimilarResults.count)
        case "similar.error": return model.findSimilarError ?? "none"
        case "similar.anchor": return model.findSimilarAnchor?.subject ?? "none"
        case "extractionsAny": return ai.threadExtractions.values.contains { !$0.isEmpty } ? "true" : "false"
        case "undoBanner": return model.undoSendBanner.pending == nil ? "hidden" : "shown"
        case "undoBannerRemaining": return model.undoSendBanner.pending.map { String($0.remainingSeconds) } ?? "none"
        case "pendingCount": return String(model.pendingCount)
        case "snippetPicker": return ComposeWindowController.sessionForHarness?.showSnippetPicker == true ? "shown" : "hidden"
        case "snippetCount": return String(Preferences.shared.templateLibrary.snippets.count)
        case "snippetsWindow": return SnippetEditorWindowController.isOpen ? "open" : "closed"
        case "undoDepth": return String(model.removalUndoStack.depth)   // #3022: archive/delete/move undo entries
        case "removalBanner":   // #3022 S2: hidden | offering | returning
            switch model.removalUndoBanner.shown?.phase {
            case nil: return "hidden"
            case .offering?: return "offering"
            case .returning?: return "returning"
            }
        case "editUndo":   // #3022: the Edit > Undo item AppKit would run for Cmd-Z: "<title>|enabled|<first responder>|<responder undo depth>"
            let items = NSApp.mainMenu?.items.compactMap { $0.submenu }.flatMap { $0.items } ?? []
            guard let item = items.first(where: { $0.action == Selector(("undo:")) }) else { return "no Edit > Undo item" }
            NSApp.mainMenu?.update()
            let win = NSApp.mainWindow ?? model.mainWindowForUndo
            return "\(item.title)|\(item.isEnabled)|\(win.map { _ in "window" } ?? "nil")|winCanUndo=\(win?.undoManager?.canUndo ?? false)|winUndo=\(win?.undoManager?.undoMenuItemTitle ?? "-")"
        case "removalBannerText":
            guard let s = model.removalUndoBanner.shown else { return "none" }
            switch s.phase {
            case .offering: return RemovalUndoBannerState.offerText(verb: s.verb, count: s.count)
            case .returning: return RemovalUndoBannerState.returningText(destination: s.destinationName)
            }
        case "aiJobs": return String(ai.jobs.count)
        case "aiJobsRunning": return String(ai.jobs.values.filter { !$0.isTerminal }.count)
        case "composeWindows": return composeWindow() == nil ? "0" : "1"
        case "body": return body()
        case "subject": return composeWindow()?.title ?? "<no compose window>"
        case "extractions": return String(ai.threadExtractions.values.map(\.count).reduce(0, +))
        default:
            if key.hasPrefix("aiRunning:") {   // aiRunning:<feature> -> non-terminal jobs of that feature (zombie check)
                let f = String(key.dropFirst(10))
                return String(ai.jobs.values.filter { $0.feature == f && !$0.isTerminal }.count)
            }
            if key.hasPrefix("aiJob.") {
                let f = String(key.dropFirst(6))
                return ai.jobs.values.filter { $0.feature == f }.max { $0.createdAt < $1.createdAt }?.status ?? "none"
            }
            if key.hasPrefix("session.bodyHas:") {   // the compose body contains the text (#2712)
                let t = String(key.dropFirst(16))
                return composeWindow().flatMap { composeSession(of: $0) }.map { String($0.body.contains(t)) } ?? "<none>"
            }
            if key.hasPrefix("messageCount:") {
                let s = String(key.dropFirst(13))
                return String(model.messages.filter { ($0.subject ?? "").contains(s) }.count)
            }
            if key == "sendAs" {   // the "Send As" identity picker (the lower of the two popups)
                guard let w = composeWindow() else { return "<none>" }
                let pops = UIViewTree.popupTitles(in: w)   // top first
                return pops.count > 1 ? pops[1] : "<no Send As picker>"
            }
            //: the compose signature picker. `signature` = the session's chosen
            // signature name ("None"); `signaturePopup` = the bottom-most popup's title;
            // `signatureNotice` = shown|hidden; `htmlHas:<s>` = the HTML part (as Send renders it) contains s.
            if key == "signature" || key == "signatureNotice" || key.hasPrefix("htmlHas:") {
                guard let s = composeWindow().flatMap(composeSession(of:)) else { return "<no compose window>" }
                if key == "signatureNotice" { return s.signatureNotice == nil ? "hidden" : "shown" }
                if key == "signature" {
                    return s.signatureId.flatMap { Preferences.shared.signatureLibrary.item(id: $0)?.name } ?? "None"
                }
                let html = s.markdownRenderer(core: model.core)(s.body) ?? ""
                return String(html.contains(String(key.dropFirst(8))))
            }
            if key == "signaturePopup" {
                guard let w = composeWindow() else { return "<none>" }
                return UIViewTree.popupTitles(in: w).last ?? "<none>"
            }
            if key == "fromPopup" {
                return composeWindow().flatMap { UIViewTree.popupTitles(in: $0).first } ?? "<none>"
            }
            if key.hasPrefix("identities:") {   // identities:<account email> -> "email[*default]|..."
                let email = String(key.dropFirst(11))
                guard let a = model.accounts.first(where: { $0.email == email }) else { return "no account" }
                return (model.identities[a.id] ?? []).map { "\($0.email)\($0.isDefault ? "*" : "")" }.joined(separator: "|")
            }
            if key.hasPrefix("src.has:") { return String(lastSource.contains(String(key.dropFirst(8)))) }
            if key.hasPrefix("src.count:") { return String(lastSource.components(separatedBy: String(key.dropFirst(10))).count - 1) }
            if key == "src.length" { return lastSource.isEmpty ? "0" : "nonempty" }
            if key.hasPrefix("drafts:") {   // drafts:<account email> -> "subject|subject" of its saved drafts
                guard let a = model.accounts.first(where: { $0.email == String(key.dropFirst(7)) }) else { return "no account" }
                return (model.drafts[a.id] ?? []).map { $0.subject }.joined(separator: "|")
            }
            if key.hasPrefix("bodyBefore:") {   // bodyBefore:<a>|<b> -> "true" when a occurs before b
                let p = String(key.dropFirst(11)).components(separatedBy: "|")
                guard p.count == 2, let ra = body().range(of: p[0]), let rb = body().range(of: p[1]) else { return "false" }
                return String(ra.lowerBound < rb.lowerBound)
            }
            if key.hasPrefix("bodyCount:") {   // bodyCount:<s> -> occurrences of s in the body
                let s = String(key.dropFirst(10))
                return String(body().components(separatedBy: s).count - 1)
            }
            if key.hasPrefix("bodyPrefix:") { return String(body().hasPrefix(String(key.dropFirst(11)))) }
            if key.hasPrefix("bodyHas:") { return String(body().contains(String(key.dropFirst(8)))) }
            if key.hasPrefix("pref.") {
                let p = Preferences.shared
                switch key.dropFirst(5) {
                case "aiEnabled": return String(p.aiEnabled)
                case "aiEndpoint": return p.aiEndpoint
                case "aiModel": return p.aiModel
                case "sendFlowed": return String(p.sendFlowed)
                case "undoSendDelay": return String(p.undoSendDelay.rawValue)
                case "markdownTheme": return p.markdownTheme.rawValue
                default: return nil
                }
            }
            return nil
        }
    }

    // MARK: control discovery for SwiftUI controls with no accessibility label in the NSView tree

    /// Every candidate string AppKit exposes for a control (label, title, tooltip...).
    static func hints(_ c: NSControl) -> [String] {
        var out: [String] = [c.accessibilityLabel() ?? "", c.accessibilityTitle() ?? "", c.toolTip ?? ""]
        if let b = c as? NSButton { out.append(b.title); out.append(b.attributedTitle.string) }
        if let t = c as? NSTextField { out.append(t.placeholderString ?? "") }
        return out.filter { !$0.isEmpty }
    }

    static func controls(in w: NSWindow) -> [NSControl] {
        func walk(_ v: NSView) -> [NSControl] {
            var o: [NSControl] = []
            if let c = v as? NSControl, !(c is NSScroller) { o.append(c) }
            for s in v.subviews { o += walk(s) }
            return o
        }
        return w.contentView.map(walk) ?? []
    }

    /// Visible control whose hints equal (preferred) or contain `text`.
    static func control(matching text: String, in w: NSWindow) -> NSControl? {
        let all = controls(in: w).filter { !$0.isHiddenOrHasHiddenAncestor }
        if text == "#checkbox" { return all.first { $0 is NSButton && $0.bounds.width <= 24 && $0.bounds.height <= 24 } }
        return all.first { hints($0).contains(text) } ?? all.first { hints($0).contains { $0.contains(text) } }
    }

    /// Editable single-line fields, top of the window first (To, Cc, Bcc, Subject in compose).
    static func fieldsTopDown(in w: NSWindow) -> [NSTextField] {
        controls(in: w).compactMap { $0 as? NSTextField }.filter { $0.isEditable && !$0.isHiddenOrHasHiddenAncestor }
            .sorted { $0.convert($0.bounds, to: nil).maxY > $1.convert($1.bounds, to: nil).maxY }
    }

    /// The `ComposeSession` a compose window's ComposeView observes (nil for any other window).
    static func composeSession(of w: NSWindow) -> ComposeSession? {
        (w.contentViewController as? NSHostingController<ComposeView>)?.rootView.session
    }

    static func describe(_ w: NSWindow) -> String {
        (controls(in: w).map { "\(type(of: $0)) hints=\(hints($0)) frame=\($0.convert($0.bounds, to: nil).integral)" }
            + UIAXDriver.entries(in: w).map { "ax \($0.node.role) label='\($0.node.label)' id='\($0.node.identifier)' enabled=\($0.node.enabled) frame=\($0.node.frame.integral)" })
            .joined(separator: "\n")
    }

    /// `ext` actions that need discovery by hint/position rather than UIFind.
    static func performControlAction(_ s: UIStep, window w: NSWindow) async throws {
        switch s.id ?? "" {
        case "describe":
            throw UIStepError(message: "describe:\n" + describe(w))
        case "keyEquivalent":   // `text` = "cmd+return" | "cmd+w" | "cmd+z": SwiftUI-drawn buttons (Send) have no NSView
            let parts = (s.text ?? "").split(separator: "+").map(String.init)
            var mods: NSEvent.ModifierFlags = []
            if parts.contains("cmd") { mods.insert(.command) }
            if parts.contains("shift") { mods.insert(.shift) }
            let key = parts.last ?? ""
            let (chars, code): (String, UInt16) = key == "return" ? ("\r", 36) : (key, 0)
            guard let ev = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: mods, timestamp: 0,
                                            windowNumber: w.windowNumber, context: nil, characters: chars,
                                            charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code) else {
                throw UIStepError(message: "could not build key event \(s.text ?? "")")
            }
            w.makeKeyAndOrderFront(nil)
            if !w.performKeyEquivalent(with: ev) && NSApp.mainMenu?.performKeyEquivalent(with: ev) != true {
                throw UIStepError(message: "nothing handled key equivalent \(s.text ?? "")")
            }
        case "pressHint":   // press the control whose label/title/tooltip matches `text`
            guard let c = control(matching: s.text ?? "", in: w) else {
                // macOS 27: SwiftUI buttons have no NSControl; press the accessibility node by label.
                let f = UIFind(identifier: nil, label: s.text, role: nil, text: nil)
                guard let e = await UIViewTree.resolveVisible(f, in: w), e.ax != nil else {
                    throw UIStepError(message: "no control with hint '\(s.text ?? "")'. Controls:\n" + describe(w))
                }
                try await UIAXDriver.press(e)
                break
            }
            guard c.isEnabled else { throw UIStepError(message: "control '\(s.text ?? "")' is disabled") }
            c.performClick(nil)
        case "setToggle":   // `text` hint, `value` on/off
            let want = s.value ?? true
            guard let b = control(matching: s.text ?? "", in: w) as? NSButton else {
                // macOS 27: a SwiftUI Toggle has no NSButton; flip its accessibility node.
                let f = UIFind(identifier: nil, label: s.text, role: "checkbox", text: nil)
                guard let e = await UIViewTree.resolveVisible(f, in: w), let ax = e.ax else {
                    throw UIStepError(message: "no checkbox with hint '\(s.text ?? "")'. Controls:\n" + describe(w))
                }
                if UIAXDriver.isOn(ax) != want {
                    try await UIAXDriver.press(e)
                    try await poll(2000, "checkbox '\(e.node.label)' to become \(want ? "on" : "off")") {
                        UIViewTree.resolve(f, in: w).first { $0.ax != nil }.flatMap { $0.ax }.flatMap(UIAXDriver.isOn) == want
                    }
                }
                break
            }
            // A SwiftUI checkbox here cycles off -> mixed -> on; click until it reaches the wanted state.
            var n = 0
            while (b.state == .on) != want && n < 4 {
                // The accessibility press is what drives SwiftUI's own binding.
                if !b.accessibilityPerformPress() { b.performClick(nil) }
                try? await Task.sleep(nanoseconds: 100_000_000)
                n += 1
            }
            if (b.state == .on) != want { throw UIStepError(message: "checkbox did not reach \(want) (state \(b.state.rawValue))") }
        case "composeSet":   // name = markdown (value) | theme (text = plain|clean|serif|compact): the session behind the window
            guard let session = composeSession(of: w) else { throw UIStepError(message: "window has no ComposeView session") }
            switch s.name ?? "" {
            case "markdown": session.markdownEnabled = s.value ?? true
            case "theme":
                guard let t = MarkdownThemeChoice(rawValue: s.text ?? "") else { throw UIStepError(message: "bad theme \(s.text ?? "")") }
                session.markdownTheme = t
            default: throw UIStepError(message: "composeSet: unknown name \(s.name ?? "")")
            }
        case "setField":    // name = to|cc|bcc|subject (position in the compose form), text = value
            let idx = ["to": 0, "cc": 1, "bcc": 2, "subject": 3][s.name ?? ""] ?? -1
            let fields = fieldsTopDown(in: w)
            guard idx >= 0, idx < fields.count else { throw UIStepError(message: "no field '\(s.name ?? "")' (\(fields.count) fields)") }
            let f = fields[idx]
            guard w.makeFirstResponder(f), let ed = f.currentEditor() as? NSTextView else {
                throw UIStepError(message: "could not focus field '\(s.name ?? "")'")
            }
            ed.insertText(s.text ?? "", replacementRange: NSRange(location: 0, length: (ed.string as NSString).length))
        default: throw UIStepError(message: "unhandled control action")
        }
        try? await Task.sleep(nanoseconds: 80_000_000)
    }
}
#endif
