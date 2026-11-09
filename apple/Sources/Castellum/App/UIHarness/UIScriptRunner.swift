// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 UI-script runner (Debug builds only).
//
// Usage: `scripts/ui-check.sh <scenario>` launches this app directly with
//   CASTELLUM_PROFILE_DIR=<temp> CASTELLUM_UI_SCRIPT=<scenario.json> CASTELLUM_UI_OUT=<dir>
// and `AppDelegate.modelBecameAvailable` calls `UIScriptRunner.startIfRequested`.
// The runner drives the REAL app in-process (real entry points, no mouse, no
// Accessibility or Screen Recording grant), captures PNG evidence plus a view
// tree, evaluates `UIRule` invariants, writes report.json + summary.txt and
// exits: 0 pass, 1 invariant violations, 2 step error / timeout / bad
// scenario, 3 skipped (target not on this branch), 64 unsafe environment.
//
// Safety refusal (exit 64) unless: CASTELLUM_PROFILE_DIR is set and is the
// model's profile, that profile is under the temp dir or a `build/` dir and is
// not an Application Support / .castellum-debug-profile path, and the Keychain
// is the in-memory backing under a namespaced service. A watchdog on a
// background queue (not the main actor, which a nested modal loop could block)
// writes a timeout report and `_exit(2)`s.
#if DEBUG
import AppKit
import Foundation

struct UIStepError: Error { var message: String }

struct UIStepResult: Codable {
    var index: Int
    var op: String
    var label: String
    var status: String     // ok | failed | violations
    var ms: Int
    var detail: String?
}

struct UIReport: Codable {
    var scenario: String
    var covers: [String]
    var status: String
    var exitCode: Int32
    var lastStep: String
    var steps: [UIStepResult]
    var violations: [UIViolation]
    var captures: [String]
    var profile: String
    var home: String
    var evidenceDir: String
}

/// Thread-safe mirror of the run's progress, read by the background watchdog.
final class UIRunState: @unchecked Sendable {
    private let lock = NSLock()
    private var report: UIReport
    init(_ r: UIReport) { report = r }
    func update(_ f: (inout UIReport) -> Void) { lock.lock(); f(&report); lock.unlock() }
    func snapshot() -> UIReport { lock.lock(); defer { lock.unlock() }; return report }
}

@MainActor
final class UIScriptRunner {
    static var current: UIScriptRunner?

    let model: AppModel
    let scenario: UIScenario
    let outDir: URL
    let state: UIRunState
    let labels = UIWindowLabels()
    var screen: UIVirtualScreen
    var refs: [String: String] = [:]
    var captureCount = 0

    init(model: AppModel, scenario: UIScenario, outDir: URL, profile: String) {
        self.model = model
        self.scenario = scenario
        self.outDir = outDir
        let s = scenario.screen.map { UIVirtualScreen(width: $0.width, height: $0.height, menuBar: $0.menuBar ?? 25) }
        self.screen = s ?? UIVirtualScreen.override ?? .standard
        UIVirtualScreen.override = self.screen
        state = UIRunState(UIReport(
            scenario: scenario.name, covers: scenario.covers ?? [], status: "running", exitCode: 0,
            lastStep: "start", steps: [], violations: [], captures: [], profile: profile,
            home: NSHomeDirectory(), evidenceDir: outDir.path))
    }

    // MARK: entry

    static func startIfRequested(model: AppModel) {
        guard let path = TestHost.uiScriptPath, current == nil else { return }
        let env = ProcessInfo.processInfo.environment
        let out = URL(fileURLWithPath: env["CASTELLUM_UI_OUT"] ?? FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-ui-out-\(UUID().uuidString)").path)
        try? FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        if let why = unsafeReason(model: model) {
            finishEarly(out: out, name: "unknown", code: 64, status: "unsafe", message: why)
        }
        let scenario: UIScenario
        do {
            let data = try Data(contentsOf: URL(fileURLWithPath: path))
            scenario = try JSONDecoder().decode(UIScenario.self, from: data)
        } catch {
            finishEarly(out: out, name: path, code: 2, status: "invalid", message: "cannot decode scenario: \(error)")
        }
        let problems = UIScenarioValidator.problems(in: scenario)
        if !problems.isEmpty {
            finishEarly(out: out, name: scenario.name, code: 2, status: "invalid", message: problems.joined(separator: "\n"))
        }
        // Only a target the scenario itself lists as `optionalTargets` and that is missing from
        // this branch's registry may skip; the wrapper turns it into a non-zero aggregate.
        let missing = UIScenarioValidator.missingOptionalTargets(in: scenario)
        if !missing.isEmpty {
            finishEarly(out: out, name: scenario.name, code: 3, status: "skipped",
                        message: "SKIPPED: target(s) not on this branch: \(missing.joined(separator: ", "))")
        }
        let runner = UIScriptRunner(model: model, scenario: scenario, outDir: out,
                                    profile: model.profileDirectory.path)
        current = runner
        runner.startWatchdog()
        Task { @MainActor in await runner.run() }
    }

    /// nil = safe. Otherwise why the run must be refused (exit 64). Model-time
    /// form of the guard: adds the checks that need the live model (profile the
    /// model actually opened, Keychain namespace) to the early ones.
    static func unsafeReason(model: AppModel) -> String? {
        UIScriptSafety.reason(
            bundleID: Bundle.main.bundleIdentifier,
            envProfile: ProcessInfo.processInfo.environment["CASTELLUM_PROFILE_DIR"],
            profile: model.profileDirectory.path,
            tmp: FileManager.default.temporaryDirectory.path,
            inMemoryKeychain: TestHost.usesInMemoryKeychain,
            keychainService: model.accountStore.serviceName,
            defaultService: AccountStore.defaultService)
    }

    /// Runs from `CastellumApp.init` BEFORE the profile preflight / migration /
    /// `AppModel` open, so a Dev binary launched directly with
    /// `CASTELLUM_UI_SCRIPT` can never touch Dev defaults or the real profile.
    /// No-op unless a UI script was requested. Exits 64 when unsafe.
    static func refuseUnsafeLaunchEarly() {
        guard TestHost.isUIScript else { return }
        let env = ProcessInfo.processInfo.environment
        let profile = AppModel.resolveProfileDirectory(override: TestHost.profileDirectory).path
        if let why = UIScriptSafety.reason(
            bundleID: Bundle.main.bundleIdentifier, envProfile: env["CASTELLUM_PROFILE_DIR"], profile: profile,
            tmp: FileManager.default.temporaryDirectory.path, inMemoryKeychain: TestHost.usesInMemoryKeychain,
            keychainService: nil, defaultService: "") {
            let out = URL(fileURLWithPath: env["CASTELLUM_UI_OUT"] ?? FileManager.default.temporaryDirectory
                .appendingPathComponent("castellum-ui-out-\(UUID().uuidString)").path)
            FileHandle.standardError.write(Data("UNSAFE: \(why)\n".utf8))
            finishEarly(out: out, name: "unknown", code: 64, status: "unsafe", message: why)
        }
    }

    /// Writes a minimal report and exits without ever starting a run.
    static func finishEarly(out: URL, name: String, code: Int32, status: String, message: String) -> Never {
        let r = UIReport(scenario: name, covers: [], status: status, exitCode: code, lastStep: message, steps: [],
                         violations: [], captures: [], profile: "", home: NSHomeDirectory(), evidenceDir: out.path)
        writeReport(r, to: out, summary: "\(status.uppercased()): \(message)\n")
        exit(code)
    }

    static func writeReport(_ r: UIReport, to dir: URL, summary: String) {
        let enc = JSONEncoder()
        enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        if let d = try? enc.encode(r) { try? d.write(to: dir.appendingPathComponent("report.json")) }
        try? summary.write(to: dir.appendingPathComponent("summary.txt"), atomically: true, encoding: .utf8)
        fflush(stdout)   // synchronizeFile() throws on a pipe
    }

    private func startWatchdog() {
        let ms = scenario.timeoutMs ?? 60_000
        let state = state, out = outDir
        let t = DispatchSource.makeTimerSource(queue: DispatchQueue.global(qos: .userInitiated))
        t.schedule(deadline: .now() + .milliseconds(ms))
        t.setEventHandler {
            var r = state.snapshot()
            r.status = "timeout"; r.exitCode = 2
            UIScriptRunner.writeReport(r, to: out,
                summary: "TIMEOUT after \(ms) ms; last step: \(r.lastStep)\n")
            _exit(2)
        }
        t.resume()
        watchdog = t
    }
    private var watchdog: DispatchSourceTimer?

    // MARK: run loop

    func run() async {
        labels.baseline()
        var stepError: String?
        for (i, step) in scenario.steps.enumerated() {
            let name = "\(i + 1) \(step.op)" + (step.label.map { " (\($0))" } ?? "")
            state.update { $0.lastStep = name }
            let t0 = Date()
            let before = state.snapshot().violations.count
            var status = "ok", detail: String?
            do {
                if step.op == "quit" { break }
                try await perform(step, index: i + 1)
                if state.snapshot().violations.count > before { status = "violations" }
            } catch let e as UIStepError {
                status = "failed"; detail = e.message; stepError = "step \(name): \(e.message)"
            } catch {
                status = "failed"; detail = "\(error)"; stepError = "step \(name): \(error)"
            }
            let r = UIStepResult(index: i + 1, op: step.op, label: step.label ?? "", status: status,
                                 ms: Int(Date().timeIntervalSince(t0) * 1000), detail: detail)
            state.update { $0.steps.append(r) }
            if stepError != nil {
                _ = try? await perform(UIStep(op: "capture", name: "failure"), index: i + 1)
                break
            }
        }
        finish(stepError: stepError)
    }

    private func finish(stepError: String?) {
        var r = state.snapshot()
        r.exitCode = stepError != nil ? 2 : (r.violations.isEmpty ? 0 : 1)
        r.status = ["pass", "violations", "error"][stepError != nil ? 2 : (r.violations.isEmpty ? 0 : 1)]
        var lines = ["scenario \(r.scenario): \(r.status.uppercased()) (exit \(r.exitCode))"]
        if let e = stepError { lines.append("ERROR \(e)") }
        for v in r.violations { lines.append("VIOLATION [\(v.rule)] \(v.window) \(v.element): \(v.detail)") }
        lines.append("steps: \(r.steps.count), captures: \(r.captures.count)")
        lines.append("evidence: \(outDir.path)")
        let summary = lines.joined(separator: "\n") + "\n"
        UIScriptRunner.writeReport(r, to: outDir, summary: summary)
        print(summary, terminator: "")
        fflush(stdout)
        exit(r.exitCode)
    }

    // MARK: polling

    func poll(timeoutMs: Int, _ what: String, _ predicate: @MainActor () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(Double(timeoutMs) / 1000)
        while !predicate() {
            if Date() > deadline { throw UIStepError(message: "timed out after \(timeoutMs) ms waiting for \(what)") }
            try? await Task.sleep(nanoseconds: 50_000_000)
        }
    }

    func window(_ label: String?) throws -> NSWindow {
        guard let label, let w = labels.resolve(label) else {
            throw UIStepError(message: "no window labelled '\(label ?? "nil")'; known: \(labels.labelled().map(\.0).joined(separator: ", "))")
        }
        return w
    }

    private func syncState(_ ref: String) -> AccountSyncState? {
        model.accountSyncStatuses[refs[ref] ?? ref]?.state
    }

    // MARK: steps

    private func perform(_ step: UIStep, index: Int) async throws {
        let timeout = step.timeoutMs ?? 10_000
        switch step.op {
        case "wait": try await wait(step, timeout: timeout)
        case "addAccount": try await addAccount(step, timeout: step.timeoutMs ?? 60_000)
        case "openWindow": try openWindow(step)
        case "openSheet": try openSheet(step)
        case "command": try command(step)
        case "search": model.searchText = step.text ?? ""; model.runSearch()
        case "resizeWindow": try resize(step)
        case "press": try await press(step)
        case "ext": try await UIExtU2.perform(step, model: model, window: try step.window.flatMap { $0 == "@compose" ? nil : try window($0) })
        case "setText": try setText(step)
        case "toggle": try await toggle(step)
        case "closeWindow": try window(step.window).performClose(nil)
        case "dismissSheet":
            let sheet = try window(step.window)
            guard let parent = sheet.sheetParent else { throw UIStepError(message: "'\(step.window ?? "")' is not a sheet") }
            parent.endSheet(sheet)
        case "reading": try await UIReading.perform(step, runner: self)
        case "accounts": try await UIAccounts.perform(step, runner: self)
        case "capture": try await capture(step, index: index)
        case "assert": try await assert(step, index: index)
        default: throw UIStepError(message: "unhandled op \(step.op)")
        }
    }

    private func wait(_ step: UIStep, timeout: Int) async throws {
        let u = step.until ?? ""
        switch u {
        case "idle":
            if let why = await UIIdle.wait(model: model, timeoutMs: timeout) { throw UIStepError(message: why) }
        case "accountsLoaded":
            try await poll(timeoutMs: timeout, u) { self.model.accountsLoaded }
        case "settingsOpenerReady":
            try await poll(timeoutMs: timeout, u) { self.model.settingsOpener != nil }
        default:
            if u.hasPrefix("window:") {
                let l = String(u.dropFirst(7))
                try await poll(timeoutMs: timeout, u) { self.labels.resolve(l) != nil }
            } else if u.hasPrefix("sheet:") {
                try await poll(timeoutMs: timeout, u) { self.labels.resolve(u) != nil }
            } else if u.hasPrefix("noSheet:") {
                let l = "sheet:" + u.dropFirst(8)
                try await poll(timeoutMs: timeout, u) { self.labels.resolve(l) == nil }
            } else if u.hasPrefix("sync:") {
                let parts = u.dropFirst(5).split(separator: "=", maxSplits: 1).map(String.init)
                guard parts.count == 2 else { throw UIStepError(message: "sync wait must be sync:<ref>=ok|failed") }
                try await poll(timeoutMs: timeout, u) {
                    guard let s = self.syncState(parts[0]) else { return false }
                    return parts[1] == "ok" ? s == .ok : s.isFailure
                }
            }
        }
    }

    private func addAccount(_ step: UIStep, timeout: Int) async throws {
        let email = step.email ?? "\(step.persona ?? "")@local.test"
        let password = step.password ?? "\(step.persona ?? "")-pass"
        let imap = (step.imap ?? "127.0.0.1:3143").split(separator: ":").map(String.init)
        let smtp = (step.smtp ?? "127.0.0.1:3025").split(separator: ":").map(String.init)
        let tls = step.tls ?? false
        let id = AccountSetupSheet.derivedAccountId(email: email, host: imap[0], existingAccountId: nil)
        let config = AccountConfig(
            accountId: id, email: email, host: imap[0], port: Int(imap.last ?? "3143") ?? 3143,
            username: email, password: password, useTls: tls, allowInvalidCerts: false,
            smtpHost: smtp[0], smtpPort: Int(smtp.last ?? "3025") ?? 3025, smtpUsername: email,
            smtpPassword: password, smtpUseTls: tls, displayName: nil)
        let json: String
        do { json = try config.encodedJSON() } catch { throw UIStepError(message: "cannot encode account: \(error)") }
        refs[step.as ?? step.persona ?? id] = id
        var outcome: AccountSyncStatus?
        model.addAccount(accountID: id, configJSON: json) { outcome = $0 }
        switch step.waitFor ?? "synced" {
        case "none": return
        case "saved":
            try await poll(timeoutMs: timeout, "credentials saved for \(id)") {
                (try? self.model.accountStore.allAccountIDs())?.contains(id) ?? false
            }
        default:
            try await poll(timeoutMs: timeout, "first sync of \(id)") { outcome != nil }
            if let o = outcome, o.state != .ok { throw UIStepError(message: "add \(id) failed: \(o.state.rawValue) \(o.message)") }
        }
    }

    private func openWindow(_ step: UIStep) throws {
        let target = step.target ?? ""
        labels.expect(target)
        switch target {
        case "settings":
            guard model.settingsOpener != nil else {
                throw UIStepError(message: "Settings opener not installed yet; wait settingsOpenerReady first")
            }
            let tab: SettingsTab
            switch step.tab ?? "general" {
            case "general": tab = .general
            case "ai": tab = .ai
            case "accounts": tab = .accounts
            case "rules": tab = .rules
            case "notifications": tab = .notifications
            case "tags": tab = .tags
            default: throw UIStepError(message: "unknown settings tab '\(step.tab ?? "")'")
            }
            if let ref = step.accountRef { model.openAccountsSettings(accountId: refs[ref] ?? ref) } else { model.openSettingsTab(tab) }
        case "contacts": model.openContacts()
        case "diagnostics": model.openDiagnostics()
        case "outbox": model.openOutbox()
        case "status": model.openStatusDashboard()
        case "aiActivity": model.openAiActivity()
        case "projects": model.openProjectsBoard()
        case "compose": NotificationCenter.default.post(name: .castellumCompose, object: nil)
        case "signatures":   // the same entry point as Settings > Accounts "Signatures…"
            SignatureEditorWindowController.show(prefs: .shared, model: model,
                                                 accountId: step.accountRef.map { refs[$0] ?? $0 })
        default: throw UIStepError(message: "unknown window target \(target)")
        }
    }

    private func openSheet(_ step: UIStep) throws {
        let target = step.target ?? ""
        labels.expect("sheet:" + target)
        var params: [String: String] = [:]
        if let ref = step.accountRef { params["accountId"] = refs[ref] ?? ref }
        NotificationCenter.default.post(name: .uiScriptPresent, object: nil,
                                        userInfo: ["target": target, "params": params])
    }

    /// Commands that write to a server or delete; refused without `allowServerWrites`.
    private static let writeish = ["archive", "delete", "trash", "toggle", "snooze", "markAll", "markRead", "markUnread", "send", "junk", "import", "checkForUpdates", "checkforupdates"]

    private func command(_ step: UIStep) throws {
        guard let raw = step.id, let id = CommandID(rawValue: raw) else {
            throw UIStepError(message: "unknown CommandID '\(step.id ?? "")'")
        }
        if Self.writeish.contains(where: { raw.lowercased().contains($0.lowercased()) }) && step.allowServerWrites != true {
            throw UIStepError(message: "command \(raw) may write to a server; set allowServerWrites")
        }
        guard let n = CommandRegistry.spec(for: id).notification else {
            throw UIStepError(message: "command \(raw) has no notification")
        }
        NotificationCenter.default.post(name: n, object: nil)
    }

    private func resize(_ step: UIStep) throws {
        let w = try window(step.window)
        switch step.contentSize {
        case .size(let a, let b)?: w.setContentSize(NSSize(width: a, height: b))
        case .min?:
            // A window with no declared minimum would shrink to 0x0 and vanish. That is
            // itself the finding (the layout policy never took effect), so record it.
            if w.contentMinSize.width < 1 || w.contentMinSize.height < 1 {
                state.update { $0.violations.append(UIViolation(
                    rule: "declaresMinSize", window: step.window ?? "-", element: "-",
                    detail: "window has no contentMinSize (\(w.contentMinSize)); not shrinking it to nothing")) }
                return
            }
            w.setContentSize(w.contentMinSize)
        case .ideal?: if let v = w.contentView { w.setContentSize(v.fittingSize) }
        case nil: throw UIStepError(message: "resizeWindow needs contentSize")
        }
        // SwiftUI's per-control `KeyViewProxy` views keep their pre-resize frames until the
        // key-view loop is rebuilt (a user's resize does that through the event loop; a
        // scripted one has no events). Without this the layout rules judged the Cancel
        // button's stale proxy 328 pt below a sheet shrunk to its minimum while the button
        // itself sat in the footer (u1-gui1-add-sheet-long-error).
        w.contentView?.layoutSubtreeIfNeeded()
        w.recalculateKeyViewLoop()
    }

    // MARK: interaction (phase 2)

    /// First visible control matching the step's `find`: an NSView / probe, else (macOS 27
    /// SwiftUI controls) an accessibility node, see `UIAXDriver`.
    func target(_ step: UIStep) async throws -> (NSWindow, UIViewTree.Entry) {
        let w = try window(step.window)
        guard let f = step.find else { throw UIStepError(message: "missing find") }
        guard let e = await UIViewTree.resolveVisible(f, in: w) else {
            throw UIStepError(message: "no visible control matching \(f) in '\(step.window ?? "")'")
        }
        return (w, e)
    }

    private func press(_ step: UIStep) async throws {
        try await target(step).1.press()
        try? await Task.sleep(nanoseconds: 50_000_000)
    }

    private func toggle(_ step: UIStep) async throws {
        let (w, e) = try await target(step)
        if let b = e.view as? NSButton {
            let want = step.value ?? (b.state != .on)
            if (b.state == .on) != want { b.performClick(nil) }
        } else if let ax = e.ax, e.node.role == "checkbox", let f = step.find {
            let on = UIAXDriver.isOn(ax) ?? false
            let want = step.value ?? !on
            if on != want {
                try await UIAXDriver.press(e)
                try await poll(timeoutMs: 2000, "checkbox '\(e.node.label)' to become \(want ? "on" : "off")") {
                    UIViewTree.resolve(f, in: w).first { $0.ax != nil }.flatMap { $0.ax }.flatMap(UIAXDriver.isOn) == want
                }
            }
        } else { throw UIStepError(message: "control is not a checkbox/button") }
        try? await Task.sleep(nanoseconds: 50_000_000)
    }

    private func setText(_ step: UIStep) throws {
        let w = try window(step.window)
        guard let f = step.find, var c = UIViewTree.find(f, in: w).first(where: { !$0.node.hidden })?.view else {
            throw UIStepError(message: "no visible control matching \(String(describing: step.find)) in '\(step.window ?? "")'")
        }
        // A `field` probe marks a SwiftUI TextField: act on the AppKit text field under it.
        if let p = c as? UIProbeView {
            let mid = NSPoint(x: p.convert(p.bounds, to: nil).midX, y: p.convert(p.bounds, to: nil).midY)
            let hit = UIViewTree.entries(in: w).first { e in
                (e.view as? NSTextField)?.isEditable == true && e.node.frame.contains(mid) }
            guard let f = hit?.view else { throw UIStepError(message: "no text field under probe '\(p.probeId)'") }
            c = f
        }
        guard let f = c as? NSTextField, f.isEditable else { throw UIStepError(message: "control is not an editable text field") }
        guard w.makeFirstResponder(f), let editor = f.currentEditor() as? NSTextView else {
            throw UIStepError(message: "could not focus the text field")
        }
        editor.string = step.text ?? ""
        // SwiftUI's TextField binding listens for the editing notifications.
        NotificationCenter.default.post(name: NSText.didChangeNotification, object: editor)
        f.stringValue = step.text ?? ""
    }

    // MARK: evidence

    @discardableResult
    private func capture(_ step: UIStep, index: Int, into dirName: String? = nil) async throws -> [String: UIWindowInfo] {
        captureCount += 1
        let dir = outDir.appendingPathComponent(dirName ?? String(format: "%02d-%@", captureCount, UICapture.sanitise(step.name ?? "capture")))
        var wanted = labels.labelled()
        if let ws = step.windows, ws != ["all"] { wanted = wanted.filter { ws.contains($0.0) } }
        var infos: [String: UIWindowInfo] = [:]
        /// `axNodes`: SwiftUI accessibility nodes, present once a step needed them (UIAXDriver).
        struct Dump: Encodable { var window: UIWindowInfo; var nodes: [UINode]; var axNodes: [UINode]? }
        var dumps: [Dump] = []
        for (label, w) in wanted {
            let web = await UICapture.webSnapshots(in: w)
            let res = UICapture.capture(window: w, label: label, dir: dir, web: web)
            var info = UIViewTree.info(for: w, label: label)
            info.nonBlankRatio = res.nonBlankRatio
            infos[label] = info
            dumps.append(Dump(window: info, nodes: UIViewTree.nodes(in: w),
                              axNodes: UIAXDriver.isEnabled ? UIAXDriver.entries(in: w).map(\.node) : nil))
            for p in [res.contentPath, res.framePath].compactMap({ $0 }) {
                state.update { $0.captures.append(p) }
            }
        }
        let enc = JSONEncoder(); enc.outputFormatting = [.prettyPrinted, .sortedKeys]
        if let d = try? enc.encode(dumps) { try? d.write(to: dir.appendingPathComponent("tree.json")) }
        return infos
    }

    private func assert(_ step: UIStep, index: Int) async throws {
        var found: [UIViolation] = []
        if let label = step.window {
            let w = try window(label)
            let dirName = String(format: "%02d-assert-%@", index, UICapture.sanitise(label))
            let infos = try await capture(UIStep(op: "capture", name: "x", windows: [label]), index: index, into: dirName)
            let info = infos[label] ?? UIViewTree.info(for: w, label: label)
            let nodes = UIViewTree.nodes(in: w)
            for rule in step.rules ?? [] {
                found += UIRule.evaluate(rule, window: info, nodes: nodes, screen: screen)
            }
            for e in step.expect ?? [] where e.find != nil {
                // No visible NSView match: the control may be SwiftUI-only (macOS 27), so let the
                // accessibility tree settle before `expectFind` consults it.
                if let f = e.find, !UIViewTree.find(f, in: w).contains(where: { !$0.node.hidden }) { await UIAXDriver.ready(w) }
                found += expectFind(e, window: w, label: label)
            }
        }
        if step.expect?.contains(where: { UIExtU2.needsAccessibility($0.model) }) == true, let w = UIExtU2.composeWindow() {
            await UIAXDriver.ready(w)
        }
        for e in step.expect ?? [] where e.find == nil { found += try expectGlobal(e) }
        // Implicit noErrorAlert on every assert.
        if let err = model.errorText {
            found.append(UIViolation(rule: "noErrorAlert", window: step.window ?? "-", element: "model.errorText", detail: err))
        }
        // WS1: the write-safety dialog is an NSAlert sheet the MODEL asked for (`writeSafetyPrompt` is pending and
        // the panel is a sheet), not an error; any other alert panel still violates.
        // Likewise the "Permanently delete?" confirmation: the model asked for it (`pendingPermanentDelete`).
        let writeSafetyDialogUp = model.writeSafetyPrompt != nil || model.pendingPermanentDelete != nil
        if let alert = UIWindows.candidates().first(where: { $0 is NSPanel && $0.title.isEmpty && $0.attachedSheet == nil
            && String(describing: type(of: $0)).contains("Alert") && !(writeSafetyDialogUp && $0.sheetParent != nil) }) {
            found.append(UIViolation(rule: "noErrorAlert", window: step.window ?? "-", element: String(describing: type(of: alert)),
                                     detail: "an alert window is on screen"))
        }
        if !found.isEmpty { state.update { $0.violations += found } }
    }

    private func expectFind(_ e: UIExpect, window w: NSWindow, label: String) -> [UIViolation] {
        let hits = UIViewTree.resolve(e.find!, in: w)
        func v(_ d: String) -> [UIViolation] {
            [UIViolation(rule: "expect", window: label, element: "\(e.find!)", detail: d)]
        }
        if let ex = e.exists { if (!hits.isEmpty) != ex { return v(ex ? "control not found" : "control unexpectedly present") } }
        guard let first = hits.first else { return e.exists == false ? [] : (e.visible != nil || e.enabled != nil ? v("control not found") : []) }
        if let vis = e.visible, (!first.node.hidden) != vis { return v("visible=\(!first.node.hidden), expected \(vis)") }
        if let inRect = e.inVisibleRect {
            guard let view = first.view else { return v("inVisibleRect: control has no backing view") }
            let clipped = UIViewTree.clippedOut(view)
            if clipped == inRect { return v("inVisibleRect=\(!clipped), expected \(inRect): control is scrolled/clipped out of view") }
        }
        if let en = e.enabled, first.node.enabled != en { return v("enabled=\(first.node.enabled), expected \(en)") }
        return []
    }

    private func expectGlobal(_ e: UIExpect) throws -> [UIViolation] {
        func v(_ d: String) -> [UIViolation] { [UIViolation(rule: "expect", window: "-", element: e.model ?? "-", detail: d)] }
        if let n = e.windowCount, UIWindows.candidates().count != n {
            return v("windowCount=\(UIWindows.candidates().count), expected \(n)")
        }
        if let s = e.sheetAttached {
            guard let main = try? window("main") else { return v("sheetAttached: no main window to inspect") }
            if (main.attachedSheet != nil) != s { return v("sheetAttached=\(main.attachedSheet != nil), expected \(s)") }
        }
        if let m = e.model {
            let actual: String
            if m == "errorText" { actual = model.errorText ?? "" }
            else if m == "accountCount" { actual = String(model.accounts.count) }
            else if m.hasPrefix("ext:") {
                guard let v = UIExtU2.value(String(m.dropFirst(4)), model: model) else { throw UIStepError(message: "unknown ext expectation '\(m)'") }
                actual = v
            }
            else if m.hasPrefix("accountSync:") {
                guard let s = syncState(String(m.dropFirst(12))) else { return v("no sync status for \(m)") }
                actual = s == .ok ? "ok" : (s.isFailure ? "failed" : s.rawValue)
            } else if m.hasPrefix("reading:") {
                guard let v = UIReading.value(String(m.dropFirst(8)), runner: self) else { throw UIStepError(message: "unknown reading key '\(m)'") }
                actual = v
            } else if m.hasPrefix("accounts:") {
                guard let v = UIAccounts.value(String(m.dropFirst(9)), runner: self) else { throw UIStepError(message: "unknown or unresolved accounts key '\(m)'") }
                actual = v
            } else { throw UIStepError(message: "unknown model expectation '\(m)'") }
            if let want = e.equals, actual != want { return v("\(m) = '\(actual)', expected '\(want)'") }
        }
        return []
    }
}

/// Pure, nonisolated safety predicate so every branch is unit-testable and the
/// same rules run both early (CastellumApp.init) and after the model exists.
enum UIScriptSafety {
    /// Bundle id of the re-signed throwaway copy; a per-run suffix (`.<runid>`) is accepted.
    static let bundleIDBase = "uk.co.riera.castellum.uitest"

    static func bundleIDOK(_ id: String?) -> Bool {
        guard let id else { return false }
        return id == bundleIDBase || id.hasPrefix(bundleIDBase + ".")
    }

    /// nil = safe. `keychainService == nil` skips the namespace check (early phase).
    static func reason(bundleID: String?, envProfile: String?, profile: String, tmp: String,
                       inMemoryKeychain: Bool, keychainService: String?, defaultService: String) -> String? {
        if !bundleIDOK(bundleID) {
            return "bundle id '\(bundleID ?? "nil")' is not \(bundleIDBase)[.<run>]; run via scripts/ui-check.sh, never the Dev binary"
        }
        guard let envProfile, !envProfile.isEmpty else { return "CASTELLUM_PROFILE_DIR is not set" }
        let p = URL(fileURLWithPath: profile).resolvingSymlinksInPath().path
        if p != URL(fileURLWithPath: envProfile).resolvingSymlinksInPath().path {
            return "model profile \(p) is not CASTELLUM_PROFILE_DIR"
        }
        if p.contains("Application Support") || p.contains(".castellum-debug-profile") {
            return "profile \(p) looks like a real profile"
        }
        let t = URL(fileURLWithPath: tmp).resolvingSymlinksInPath().path
        if !(p.hasPrefix(t) || p.contains("/build/")) {
            return "profile \(p) is neither under the temp dir nor a build/ dir"
        }
        if !inMemoryKeychain { return "Keychain backing is not in-memory" }
        if let svc = keychainService, svc == defaultService { return "Keychain service is the real namespace" }
        return nil
    }
}
#endif
