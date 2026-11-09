// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 extension ops for Accounts, Settings and Sync status (Debug builds only; stage U1).
//
// Usage: a scenario step `{"op":"accounts","id":"<action>", ...}` is routed here by the runner, and an
// expectation `{"model":"accounts:<key>","equals":"<value>"}` is evaluated by `UIAccounts.value`.
// Own file so sibling harness extensions merge cleanly; the shared runner has only additive one-line
// hooks. Everything calls the same entry points the real controls call. `#if DEBUG` only
// (`scripts/ui-check.sh --verify-release` greps for these symbols).
//
// Actions (fields reuse UIStep: id=action, window, find, name, accountRef, text):
//   pressExpect  window=<label> find=<UIFind> name=<label the new window/sheet gets, e.g. sheet:reauth | status>
//                presses a real control (NSButton / probe / SwiftUI accessibility node) after registering the label
//                the window it opens gets. Prefer this to `click`: a label survives a layout change, x,y does not.
//   click        window=<label> text="x,y" [name=<label the window it opens gets>]
//                a real mouse click (NSEvent via NSWindow.sendEvent, no CGEvent, no permissions) at content-area
//                point x,y in points from the content top-left. For SwiftUI controls that draw no NSView of their own
//                (Settings buttons, the sidebar status dot). Coordinates are read off the PNG of a fixed-size window.
//   popupSelect  window=<label> find=<UIFind> text=<item title>   choose an item of a pop-up button (sends its action)
//   settle       text=<milliseconds>                 sleep, for results that arrive over the network (inline tests)
//   setNth       window=<label> name=<N> text=<value>   type into the Nth (0-based, top to bottom) editable text field of a
//                window: for SwiftUI TextFields that have no probe (Display name, Port, SMTP port). Sheet order, Add /
//                Re-authenticate: 0 email, 1 display name, 2 IMAP host, 3 port, 4 username, 5 password, 6 SMTP host,
//                7 SMTP port, 8 SMTP username, 9 SMTP password (Add mode has a "Copy settings from" popup, no extra field).
//   waitNode     window=<label> find=<UIFind> value=<true|false> [timeoutMs]   poll until a visible node matching find
//                exists (true) or is gone (false); for state changes inside a sheet (Cancel becoming Continue in Background)
//   moveWindow   window=<label> text="x,y"          setFrameOrigin (AppKit screen coordinates, y up)
//   setEndpoint  accountRef=<ref> text=<host:port> [value=false]   store a new IMAP host:port for the account and re-sync it
//                (value=false: store only, no re-sync)
//                (how a scenario "stops" and "restarts" a server without touching the shared GreenMail)
//   sigAdd       accountRef=<ref> text=<name>        Signatures window "+ > account" (SignatureLibrary.add)
//   sigRename    text=<old name> name=<new name>     the Name field binding (SignatureLibrary.rename)
//   sigDefault   text=<name>                          the "use as default" toggle (setDefault)
//   sigDelete    text=<name>                          the "-" button (delete)
//   sigText      text=<name> name=<plain text>       the Plain Text editor binding (SignatureLibrary.setText)
//   sigHTML      text=<name> name=<html>             the HTML editor binding (SignatureLibrary.setHTML)
//   segment      window=<label> text=<segment label> select a segment of the first segmented control (sends its action)
// Expectation keys (`accounts:`): see `value(_:runner:)`.
#if DEBUG
import AppKit
import Foundation

@MainActor
enum UIAccounts {
    nonisolated static let actions: Set<String> = ["pressExpect", "click", "moveWindow", "waitNode", "setNth", "popupSelect", "settle", "setEndpoint", "sigAdd", "sigRename", "sigDefault", "sigDelete", "sigText", "sigHTML", "segment"]

    nonisolated static func problems(_ s: UIStep) -> [String] {
        guard let a = s.id, actions.contains(a) else { return ["unknown accounts action '\(s.id ?? "")'"] }
        func need(_ ok: Bool, _ what: String) -> [String] { ok ? [] : ["\(a) needs \(what)"] }
        switch a {
        case "pressExpect": return need(s.window != nil && s.find != nil && s.name != nil, "window, find and name")
        case "click": return need(s.window != nil && (s.text ?? "").split(separator: ",").count == 2, "window and text=\"x,y\"")
        case "setNth": return need(s.window != nil && Int(s.name ?? "") != nil && s.text != nil, "window, name=<N> and text")
        case "waitNode": return need(s.window != nil && s.find != nil && s.value != nil, "window, find and value")
        case "moveWindow": return need(s.window != nil && (s.text ?? "").split(separator: ",").count == 2, "window and text=\"x,y\"")
        case "popupSelect": return need(s.window != nil && s.find != nil && s.text != nil, "window, find and text")
        case "settle": return need(Int(s.text ?? "") != nil, "text=<milliseconds>")
        case "setEndpoint": return need(s.accountRef != nil && (s.text ?? "").contains(":"), "accountRef and text=host:port")
        case "sigAdd": return need(s.accountRef != nil && s.text != nil, "accountRef and text")
        case "sigRename": return need(s.text != nil && s.name != nil, "text (old name) and name (new name)")
        case "segment": return need(s.window != nil && s.text != nil, "window and text (segment label)")
        case "sigText", "sigHTML": return need(s.text != nil && s.name != nil, "text (signature name) and name (content)")
        default: return need(s.text != nil, "text (signature name)")
        }
    }

    /// Editable text fields of a window, top to bottom (window coordinates have y up, so larger minY first), then left to right.
    static func editableFields(in w: NSWindow) -> [NSTextField] {
        UIViewTree.entries(in: w)
            .filter { ($0.view as? NSTextField)?.isEditable == true && !$0.node.hidden }
            .sorted { ($0.node.frame.minY, -$0.node.frame.minX) > ($1.node.frame.minY, -$1.node.frame.minX) }
            .compactMap { $0.view as? NSTextField }
    }

    private static func sigID(named n: String?) throws -> String {
        guard let item = Preferences.shared.signatureLibrary.items.first(where: { $0.name == n }) else {
            throw UIStepError(message: "no signature named '\(n ?? "")'")
        }
        return item.id
    }

    static func perform(_ s: UIStep, runner r: UIScriptRunner) async throws {
        func ref(_ x: String?) -> String { x.flatMap { r.refs[$0] ?? $0 } ?? "" }
        switch s.id ?? "" {
        case "pressExpect":
            let (_, e) = try await r.target(s)
            r.labels.expect(s.name ?? "")
            try await e.press()
            try? await Task.sleep(nanoseconds: 100_000_000)
        case "click":
            let w = try r.window(s.window)
            let xy = (s.text ?? "").split(separator: ",").compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }
            guard xy.count == 2 else { throw UIStepError(message: "click text must be \"x,y\"") }
            let contentH = w.contentRect(forFrameRect: w.frame).height
            let at = NSPoint(x: xy[0], y: contentH - xy[1])
            if let n = s.name { r.labels.expect(n) }
            if !w.isKeyWindow { w.makeKeyAndOrderFront(nil) }
            // An NSButton's mouseDown runs a tracking loop that waits for the mouse-up, which would deadlock a
            // synchronous synthetic click: a real AppKit control under the point is pressed with performClick instead.
            if let cv = w.contentView, let host = cv.superview,
               var hit = cv.hitTest(host.convert(at, from: nil)) {
                while !(hit is NSControl), let up = hit.superview { hit = up }
                if let c = hit as? NSControl, c.isEnabled {
                    c.performClick(nil)
                    try? await Task.sleep(nanoseconds: 150_000_000)
                    return
                }
            }
            func event(_ t: NSEvent.EventType) -> NSEvent? {
                NSEvent.mouseEvent(with: t, location: at, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                   windowNumber: w.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1)
            }
            guard let down = event(.leftMouseDown), let up = event(.leftMouseUp) else { throw UIStepError(message: "cannot build a mouse event") }
            w.sendEvent(down)
            try? await Task.sleep(nanoseconds: 80_000_000)
            w.sendEvent(up)
            try? await Task.sleep(nanoseconds: 150_000_000)
        case "setNth":
            let w = try r.window(s.window)
            let n = Int(s.name ?? "") ?? -1
            let fields = editableFields(in: w)
            guard n >= 0, n < fields.count else { throw UIStepError(message: "window has \(fields.count) editable text fields; no #\(n)") }
            let f = fields[n]
            guard w.makeFirstResponder(f), let editor = f.currentEditor() as? NSTextView else { throw UIStepError(message: "could not focus text field #\(n)") }
            editor.string = s.text ?? ""
            NotificationCenter.default.post(name: NSText.didChangeNotification, object: editor)
            f.stringValue = s.text ?? ""
        case "waitNode":
            let want = s.value ?? true
            // Let SwiftUI's accessibility tree be walked first: with value=false an
            // empty, not-yet-built tree would otherwise read as "gone" (#3158).
            if let w = try? r.window(s.window) { await UIAXDriver.ready(w) }
            try await r.poll(timeoutMs: s.timeoutMs ?? 30_000, "node \(String(describing: s.find)) \(want ? "present" : "gone")") {
                guard let w = try? r.window(s.window), let f = s.find else { return false }
                let present = UIViewTree.resolve(f, in: w).contains { !$0.node.hidden }
                if !want && !present {
                    // "Gone" only counts once the window has some visible content at all.
                    let treeBuilt = UIViewTree.entries(in: w).contains { !$0.node.hidden }
                        || UIAXDriver.entries(in: w).contains { !$0.node.hidden }
                    return treeBuilt
                }
                return present == want
            }
        case "moveWindow":
            let w = try r.window(s.window)
            let xy = (s.text ?? "").split(separator: ",").compactMap { Double($0.trimmingCharacters(in: .whitespaces)) }
            guard xy.count == 2 else { throw UIStepError(message: "moveWindow text must be \"x,y\"") }
            w.setFrameOrigin(NSPoint(x: xy[0], y: xy[1]))
            try? await Task.sleep(nanoseconds: 150_000_000)
        case "popupSelect":
            let (_, e) = try await r.target(s)
            if e.ax != nil {   // macOS 27: a SwiftUI Picker has no NSPopUpButton
                try await UIAXDriver.menu(of: e, choose: s.text ?? "")
                break
            }
            guard let p = e.view as? NSPopUpButton else {
                throw UIStepError(message: "no visible pop-up button matching \(String(describing: s.find))")
            }
            guard p.itemTitles.contains(s.text ?? "") else { throw UIStepError(message: "pop-up has no item '\(s.text ?? "")'; has \(p.itemTitles)") }
            // SwiftUI binds each menu item, not the popup: fire the item's own action, as a click on it would.
            if let i = p.itemTitles.firstIndex(of: s.text ?? "") {
                p.select(p.item(at: i))
                p.menu?.performActionForItem(at: i)
            }
            try? await Task.sleep(nanoseconds: 100_000_000)
        case "settle":
            try? await Task.sleep(nanoseconds: UInt64(Int(s.text ?? "0") ?? 0) * 1_000_000)
        case "setEndpoint":
            let id = ref(s.accountRef)
            guard let cfg = r.model.storedConfig(for: id) else { throw UIStepError(message: "no stored config for \(id)") }
            let dec = JSONDecoder(); dec.keyDecodingStrategy = .convertFromSnakeCase
            var c: AccountConfig
            do { c = try dec.decode(AccountConfig.self, from: Data(cfg.utf8)) } catch { throw UIStepError(message: "cannot decode the stored config: \(error)") }
            let parts = (s.text ?? "").split(separator: ":").map(String.init)
            guard parts.count == 2, let port = Int(parts[1]) else { throw UIStepError(message: "text must be host:port") }
            c.host = parts[0]; c.port = port
            guard let json = try? c.encodedJSON(), r.model.saveAccountWithoutSync(accountID: id, configJSON: json) else {
                throw UIStepError(message: "could not store the new endpoint")
            }
            if s.value != false { r.model.retrySync(accountId: id) }   // value=false: store only, as a server coming back does
        case "sigAdd":
            Preferences.shared.signatureLibrary.add(name: s.text ?? "Signature", accountId: ref(s.accountRef))
        case "sigRename":
            Preferences.shared.signatureLibrary.rename(id: try sigID(named: s.text), to: s.name ?? "")
        case "sigDefault":
            Preferences.shared.signatureLibrary.setDefault(id: try sigID(named: s.text))
        case "sigDelete":
            Preferences.shared.signatureLibrary.delete(id: try sigID(named: s.text))
        case "segment":
            let w = try r.window(s.window)
            func seg(_ v: NSView) -> NSSegmentedControl? { (v as? NSSegmentedControl) ?? v.subviews.lazy.compactMap(seg).first }
            guard let c = w.contentView.flatMap(seg) else { throw UIStepError(message: "no segmented control in '\(s.window ?? "")'") }
            guard let i = (0..<c.segmentCount).first(where: { c.label(forSegment: $0) == s.text }) else {
                throw UIStepError(message: "no segment '\(s.text ?? "")'; segments: \((0..<c.segmentCount).map { c.label(forSegment: $0) ?? "" })")
            }
            c.selectedSegment = i
            c.sendAction(c.action, to: c.target)
        case "sigText":
            Preferences.shared.signatureLibrary.setText(id: try sigID(named: s.text), s.name ?? "")
        case "sigHTML":
            Preferences.shared.signatureLibrary.setHTML(id: try sigID(named: s.text), s.name ?? "")
        default: throw UIStepError(message: "unknown accounts action '\(s.id ?? "")'")
        }
    }

    /// Keys: `dotState:<ref>` (sidebar dot state label) · `dotMessage:<ref>` (its tooltip text, without the click hint)
    /// · `sidebarLabel:<ref>` · `displayName:<ref>` · `fromHeader:<ref>` · `endpoint:<ref>` (stored IMAP host:port)
    /// · `signatures:<ref>` (names in order, default marked `*`) · `field:<window>|<probe id or label>` (text of a field)
    /// · `frame:<window>` (x,y,w,h) · `autosaveName:<window>` · `defaults:<key>` (set|unset in the run's throwaway defaults)
/// · `secureFields:<window>` (number of masked password fields) · `windowTitles` (visible window titles, sorted) · `updateSchedule` (Settings > General > Updates value).
    static func value(_ key: String, runner r: UIScriptRunner) -> String? {
        func id(_ x: Substring) -> String { r.refs[String(x)] ?? String(x) }
        if key.hasPrefix("frame:") {   // "x,y,w,h" of a window's frame, rounded
            guard let w = try? r.window(String(key.dropFirst(6))) else { return nil }
            return [w.frame.minX, w.frame.minY, w.frame.width, w.frame.height].map { String(Int($0.rounded())) }.joined(separator: ",")
        }
        if key.hasPrefix("autosaveName:") { return (try? r.window(String(key.dropFirst(13))))?.frameAutosaveName }
        if key.hasPrefix("defaults:") { return UserDefaults.standard.string(forKey: String(key.dropFirst(9))) == nil ? "unset" : "set" }
        if key == "updateSchedule" { return Preferences.shared.updateCheckSchedule.rawValue }
        if key == "windowTitles" { return UIWindows.candidates().map(\.title).sorted().joined(separator: "|") }
        if key.hasPrefix("field:") {
            let parts = key.dropFirst(6).split(separator: "|", maxSplits: 1).map(String.init)
            guard parts.count == 2, let w = try? r.window(parts[0]) else { return nil }
            if parts[1].hasPrefix("#"), let n = Int(parts[1].dropFirst()) {
                let fields = editableFields(in: w)
                return n < fields.count ? fields[n].stringValue : nil
            }
            let f = UIFind(identifier: nil, label: parts[1], role: nil, text: nil)
            let byId = UIFind(identifier: parts[1], label: nil, role: nil, text: nil)
            for find in [byId, f] {
                guard let v = UIViewTree.find(find, in: w).first?.view else { continue }
                if let t = v as? NSTextField, t.isEditable { return t.stringValue }
                if let p = v as? UIProbeView {   // a `field` probe sits over the real text field
                    let mid = NSPoint(x: p.convert(p.bounds, to: nil).midX, y: p.convert(p.bounds, to: nil).midY)
                    if let t = UIViewTree.entries(in: w).first(where: { ($0.view as? NSTextField)?.isEditable == true && $0.node.frame.contains(mid) })?.view as? NSTextField {
                        return t.stringValue
                    }
                }
            }
            return nil
        }
        if key.hasPrefix("secureFields:") {   // count of visible secure (masked) password fields in a window
            guard let w = try? r.window(String(key.dropFirst(13))) else { return nil }
            return String(UIViewTree.entries(in: w).filter { $0.node.role == "secureTextField" && !$0.node.hidden }.count)
        }
        guard let colon = key.firstIndex(of: ":") else { return nil }
        let kind = String(key[..<colon]), acct = id(key[key.index(after: colon)...])
        let m = r.model
        let account = m.accounts.first { $0.id == acct }
        switch kind {
        case "dotState": return m.accountSyncStatuses[acct].map { $0.state == .ok ? "ok" : $0.state.rawValue } ?? "none"
        case "dotMessage": return m.accountSyncStatuses[acct]?.message ?? "none"
        case "sidebarLabel": return account?.sidebarLabel
        case "displayName": return account.map { $0.displayName ?? "" }
        case "fromHeader": return account?.fromHeader
        case "endpoint":
            guard let cfg = m.storedConfig(for: acct) else { return nil }
            let dec = JSONDecoder(); dec.keyDecodingStrategy = .convertFromSnakeCase
            return (try? dec.decode(AccountConfig.self, from: Data(cfg.utf8))).map { "\($0.host):\($0.port)" }
        case "signatures":
            let lib = Preferences.shared.signatureLibrary
            return lib.items(for: acct).map { $0.name + (lib.isDefault($0.id) ? "*" : "") }.joined(separator: ",")
        default: return nil
        }
    }
}
#endif
