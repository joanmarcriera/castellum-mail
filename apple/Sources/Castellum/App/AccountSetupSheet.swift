// Castellum - IMAP account setup sheet → read-only sync.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Once the server accepts the login and the first sync is under way (or, at
// the latest, when that sync succeeds) the connection config (including the
// password) is stored in the macOS Keychain (see AccountStore and
// `AppModel.addAccount`) so future syncs/replays run without a prompt — an
// account that is listed while its first sync still runs is never left
// unconfigured. The SMTP section prefills smart defaults derived from the IMAP values
// (host imap.x -> smtp.x, port 587, same credentials, TLS on); fields the user
// hasn't touched track the IMAP fields live. Config is JSON-encoded through
// AccountConfig so field values are escaped safely.
//
// Doubles as the Re-authenticate sheet:
// pass `existingAccountId` to prefill host/port/username/SMTP settings from
// the account's stored config and pin the sync to that same account id
// instead of deriving a new one from email/host. Passwords are NEVER
// prefilled from the Keychain — they must be re-entered, same as Add.
//
// Reconnect/Sync runs the connection inline: a spinner while
// connecting, then either a green checkmark that auto-closes the sheet after
// a beat, or a red xmark with the same human-first message AccountSyncStatus
// uses for the sidebar/dashboard — the sheet stays open on failure, fields
// stay editable, and "Save anyway" stores the config without a successful
// sync so the user can fix it later.
//
// First sync against a large mailbox ( — Marc's pilot against
// his real 78,851-message Gmail account) can take minutes: the "connecting"
// phase covers the WHOLE first sync, not just the login handshake, because
// `AppModel.addAccount`/`performSync` only report completion once the
// entire `castellum_sync_account` call returns. Sitting through that in a
// modal sheet, with only a spinner, is exactly the "give a sense of scale,
// then let them background it" gap Marc filed. This sheet now shows live
// progress (message/folder counts, `AppModel.accountSyncProgress`) during
// that wait, and a "Continue in Background" button that dismisses
// immediately — the sync keeps running in `AppModel`'s already-detached
// background task regardless of whether this view is still on screen, and
// `AppModel.startSyncProgressPolling` makes the account visible in the main
// window's sidebar as soon as it's durably created, not only once the whole
// first sync finishes.
//
// Duplicate an account's settings: pass `duplicateFromAccountId`
// (the "Duplicate…" toolbar action, Settings > Accounts) or let the user pick
// a source from the "Copy settings from" picker shown in Add mode. Either way
// this is still an Add — a NEW account id is derived from the email the user
// types, same as any other Add — with the non-identity fields (host, ports,
// TLS) prefilled from `AccountConfig.duplicateFields()`. Email, username and
// password are left for the user to type fresh; the source account is never
// read except to copy those fields, and is never written to.
//
// The id `buildConfig()` derives in Add mode (email, falling back to host
// when email is blank) can collide with the source account's own id, or any
// other existing account's id — typing the source's own email, or leaving
// email blank when the source's id equals its host, both reproduce it.
// `AccountStore.save` updates an existing entry in place, so saving under a
// colliding id would silently overwrite that other account. `idConflict`
// (backed by the pure, unit-tested `wouldOverwriteExistingAccount`) blocks
// both Sync and "Save anyway" while that would happen.
//
// Test Connection: verifies IMAP (login + folder list)
// and SMTP (login) for the current fields WITHOUT saving or syncing anything
// (`AppModel.testConnection`, off the main thread). Results show one
// actionable line per leg (`ConnectionTestPresentation`). Editing a field
// after a test marks the result stale rather than silently trusting it.
// Cancel during a test just dismisses; the late result is ignored.
//
// Automatic preflight before the first sync: Sync/Reconnect
// now runs that SAME probe itself before ever calling `addAccount` — fail
// fast on a bad password/host rather than discovering it only after a long
// first sync. A prior Test Connection with byte-identical settings that
// already passed is reused rather than re-run (`AppModel
// .addAccountWithPreflight`'s `priorTest`); anything else (no prior test, a
// prior failure, or edited fields) re-probes. A failing probe renders
// exactly like a manual Test Connection failure (`testResultBlock`) and the
// sheet stays open with the fields untouched — nothing is saved, nothing
// syncs.
//
// First-sync scope (S62): Add mode offers an initial-history window (always)
// and, once a test has listed the server's folders, a folder picker
// pre-ticked with the core's default set (`SyncScope`). Both land in the
// stored config (`mailboxes`, `initialSyncSince`) and so apply to every
// later sync of the account too. Re-authenticate never shows or changes
// them — it carries the stored values through untouched.
//
// Provider presets (S62): in Add mode, typing an address at Gmail, iCloud,
// Fastmail or Yahoo fills host/ports/TLS (`ProviderPresets.swift`) unless
// the user already typed a host of their own; "Use settings for" applies a
// preset explicitly (custom domains at those providers). Outlook/Microsoft
// addresses get a plain "not supported yet" note — they need OAuth (S37).

import SwiftUI

struct AccountSetupSheet: View {
    @ObservedObject var model: AppModel
    /// Non-nil = re-authenticate this existing account rather than add a new
    /// one: the account id is pinned and non-credential fields are prefilled.
    var existingAccountId: String? = nil
    /// Non-nil = prefill this Add sheet's non-identity fields from an
    /// existing account's settings (the "Duplicate…" toolbar action). Mutually
    /// exclusive with `existingAccountId`: this still creates a NEW account.
    var duplicateFromAccountId: String? = nil
    @Environment(\.dismiss) private var dismiss
    /// Backs the "Copy settings from" picker (Add mode only). Starts at
    /// `duplicateFromAccountId` when set, so the picker reflects how the
    /// sheet was opened; changing it re-applies `duplicateFields()`.
    @State private var copySourceAccountId: String? = nil

    @State private var email = ""
    @State private var displayName = ""
    @State private var host = ""
    @State private var port = "993"
    @State private var username = ""
    @State private var password = ""
    /// Reveal/mask state for the Re-authenticate password (S75). Holds no
    /// text itself (`value` stays empty); `password` above is the field text.
    @State private var reauthPassword = ReauthPasswordField(storedConfigJSON: nil)
    /// Reveal state for the SMTP password (GUI-5). Only `isRevealed`/`remask`/
    /// `close` are used — the text itself lives in `smtpPassword`.
    @State private var reauthSMTPReveal = ReauthPasswordField(storedConfigJSON: nil)
    /// Save on a failing account asks first (GUI-5).
    @State private var confirmSaveFailed = false
    @State private var useTLS = true
    /// IMAP security mode; `useTLS` follows it.
    @State private var imapSecurity: IMAPSecurity = .tls

    // SMTP (sending). Auto-follows the IMAP values until the user edits a field.
    @State private var smtpHost = ""
    @State private var smtpHostAuto = true
    @State private var smtpPort = "587"
    @State private var smtpUsername = ""
    @State private var smtpUsernameAuto = true
    @State private var smtpPassword = ""
    @State private var smtpPasswordAuto = true
    @State private var smtpUseTLS = true

    /// Inline result of the current/last Sync-Reconnect attempt.
    private enum Phase: Equatable {
        case editing
        case connecting
        case success(String)
        case failure(AccountSyncStatus)
    }
    @State private var phase: Phase = .editing
    /// Error from the last Save / Save anyway attempt, shown IN the sheet
    /// (the app-level `errorText` alert is not visible behind a sheet, and
    /// the sheet must stay open so the typed passwords are not lost).
    @State private var saveError: String?
    /// The account id `startSync()` kicked off, once known — the key into
    /// `model.accountSyncProgress` for the inline progress line below.
    @State private var pendingAccountId: String?
    /// True for the duration of one `startSync()` attempt.
    /// Cancel and `onDisappear` flip this to false so a preflight probe that
    /// resolves as PASSING after the user has already backed out never goes
    /// on to persist credentials or start a sync nobody asked for any more —
    /// `AppModel.addAccountWithPreflight` re-checks it immediately before
    /// calling `addAccount`. Only meaningful while `phase == .editing`
    /// (probing): once `phase` becomes `.connecting`, the real sync is
    /// already running and "Continue in Background" — not cancellation —
    /// is the offered affordance, exactly as before this change.
    @State private var syncAttemptWanted = true
    /// This sheet's own lifetime: a deferred dismissal from a sync that
    /// outlives the sheet ("Continue in Background") must not close a newer
    /// sheet. See `SheetLifetime`.
    @State private var lifetime = SheetLifetime()

    /// State of the "Test Connection" preflight (S62). Independent of
    /// `phase`: testing never saves, so it never enters the sync phases.
    private enum TestPhase: Equatable {
        case idle
        case testing
        /// The report plus the exact config it tested, for staleness.
        case done(ConnectionTestReport, AccountConfig)
        /// The request itself failed (malformed config / core error).
        case error(String)
    }
    @State private var testPhase: TestPhase = .idle
    /// Bumped per test so a slower, superseded test can't overwrite a newer
    /// result.
    @State private var testGeneration = 0

    // First-sync scope (S62, Add mode only).
    @State private var initialHistory: InitialHistory = .all
    /// Ticked folder paths; nil until a successful test has listed folders
    /// (no list = no filter = the core's default set).
    @State private var selectedMailboxes: Set<String>?
    /// Stored scope carried through a Re-authenticate unchanged.
    @State private var storedMailboxes: [String]?
    @State private var storedInitialSyncSince: String?

    /// The IMAP host a preset last filled in, so auto-detection can tell
    /// its own value (safe to replace) from one the user typed (never
    /// touched). Nil = no preset applied yet.
    @State private var presetAppliedHost: String?

    /// MM-14 lookup state for the typed address's domain (Add mode only).
    @State private var lookupPhase: AutoconfigLookupPhase = .idle
    /// The debounced lookup in flight; cancelled whenever the domain changes.
    @State private var lookupTask: Task<Void, Never>?
    /// What the lookup last put in the Username field, so only that value is
    /// ever replaced or cleared (a name the user typed is never touched).
    @State private var lookupFilledUsername: String?
    /// The fields the last auto-fill wrote, to tell them from typed values.
    @State private var lastAppliedFields: PresetFields?

    /// Wait this long after the last keystroke before a lookup starts, so a
    /// half-typed domain is never sent anywhere (spec T4).
    private static let lookupDebounce: UInt64 = 600_000_000

    /// The static table's answer; a lookup result applies only when the table
    /// says `.generic` and the result is for the domain currently typed.
    private var detection: ProviderDetection {
        guard existingAccountId == nil else { return .generic }
        let known = ProviderDetection.detect(email: email)
        if case .generic = known,
           case .done(let domain, let found, _) = lookupPhase,
           domain == AutoconfigPolicy.lookupDomain(email: email) {
            return found
        }
        return known
    }

    /// The folders from the last successful test, while that test still
    /// matches the connection fields (a stale list hides the picker).
    private var testedMailboxes: [ConnectionTestMailbox]? {
        if case .done(let report, let tested) = testPhase, report.imap.ok,
           tested.connectionSettings == buildConnectionConfig().connectionSettings {
            return report.mailboxes
        }
        return nil
    }

    /// Ticking nothing would sync nothing — block that explicitly.
    private var noFolderSelected: Bool {
        selectedMailboxes?.isEmpty == true && testedMailboxes != nil
    }

    /// Fields complete enough to attempt a connection (shared by Sync and
    /// Test Connection; Sync additionally requires no id conflict).
    private var fieldsComplete: Bool {
        Self.fieldsComplete(
            host: host, password: password, port: port, smtpPort: smtpPort,
            username: username, email: email, existingAccountId: existingAccountId)
    }

    /// A pasted host or username can carry spaces, a tab or a newline, which then
    /// fails later with an opaque network error: connection fields are always
    /// used trimmed (the password never is, #3439).
    static func clean(_ s: String) -> String {
        s.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// A TCP port is 1...65535; anything else fails later with an opaque error.
    static func validPort(_ text: String) -> Bool {
        guard let value = Int(text.trimmingCharacters(in: .whitespaces)) else { return false }
        return (1...65535).contains(value)
    }

    /// Pure form of `fieldsComplete`. Adding needs a non-blank email (it names
    /// the account); re-authenticating pins the id, so a username is enough.
    static func fieldsComplete(
        host: String, password: String, port: String, smtpPort: String,
        username: String, email: String, existingAccountId: String?
    ) -> Bool {
        let hasEmail = !email.trimmingCharacters(in: .whitespaces).isEmpty
        let identityOK = hasEmail || (existingAccountId != nil && !Self.clean(username).isEmpty)
        return !Self.clean(host).isEmpty && !password.isEmpty && validPort(port)
            && identityOK
            && (smtpPort.isEmpty || validPort(smtpPort))
    }

    /// Account ids compare case- and whitespace-insensitively for the
    /// duplicate warning (the stored id keeps the user's capitals).
    static func normalisedAccountId(_ id: String) -> String {
        id.trimmingCharacters(in: .whitespaces).lowercased()
    }

    private var canTest: Bool {
        fieldsComplete && phase != .connecting && testPhase != .testing
    }

    private var canSync: Bool {
        phase != .connecting && testPhase != .testing
            && fieldsComplete
            && !idConflict
            && !noFolderSelected
    }

    private var titleText: String {
        existingAccountId == nil ? "Add IMAP account" : "Re-authenticate account"
    }

    /// A footer button whose title, enabled state and action are declared ONCE; the
    /// Debug UI probe (UIT-1) reuses the same values, so `press` / `expect enabled`
    /// exercise the real behaviour instead of a duplicated copy of it.
    private func footerButton(_ title: String, id: String, role: ButtonRole? = nil, enabled: Bool = true,
                              key: String = "", action: @escaping () -> Void) -> some View {
        let button = Button(title, role: role, action: action).disabled(!enabled)
        #if DEBUG
        return button.uiProbe(id, label: title, enabled: enabled, keyEquivalent: key, action: action)
        #else
        return button
        #endif
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ScrollView {
            VStack(alignment: .leading, spacing: 12) {
            Text(titleText).font(.headline)
            if existingAccountId != nil {
                Text(String(localized: "Host, port, username and password are prefilled from the stored account. Check them and Reconnect to restore SMTP sending."))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Form {
                if existingAccountId == nil && !model.accounts.isEmpty {
                    Picker("Copy settings from", selection: $copySourceAccountId) {
                        Text("None").tag(nil as String?)
                        ForEach(model.accounts) { account in
                            Text(account.email).tag(account.id as String?)
                        }
                    }
                    .onChange(of: copySourceAccountId) { newValue in
                        if let newValue { applyDuplicateFields(from: newValue) }
                    }
                    Text(String(localized: "Host, ports and TLS are copied from the chosen account. Email, username and password are always left blank."))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                TextField("Email", text: $email)
                    #if DEBUG
                    .uiProbe("email", role: "field", label: "email")
                    #endif
                    .onChange(of: email) { _ in emailChanged() }
                providerNote
                if idConflict {
                    Text("An account with this id already exists — enter a different email.")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .fixedSize(horizontal: false, vertical: true)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                TextField("Display name (optional)", text: $displayName)
                TextField("IMAP host", text: $host)
                    #if DEBUG
                    .uiProbe("imapHost", role: "field", label: "imapHost")
                    // UIT-1 cannot read a text field's value, so the current value rides on a probe's label.
                    .uiProbe("imapHostValue", role: "value", label: host)
                    .uiProbe("imapPortValue", role: "value", label: port)
                    .uiProbe("imapSecurityValue", role: "value", label: imapSecurity.rawValue)
                    .uiProbe("smtpHostValue", role: "value", label: smtpHost)
                    .uiProbe("smtpPortValue", role: "value", label: smtpPort)
                    .uiProbe("usernameValue", role: "value", label: username)
                    #endif
                    .onChange(of: host) { newHost in
                        if smtpHostAuto {
                            smtpHost = AccountConfig.defaultSMTPHost(imapHost: newHost)
                        }
                    }
                // AA1: port and TLS share one row so the whole form fits the
                // sheet's default height without scrolling.
                LabeledContent("Port") {
                    HStack(spacing: 8) {
                        TextField("Port", text: $port).labelsHidden().frame(width: 64)
                        // TLS / STARTTLS / None. STARTTLS is
                        // mandatory when chosen: no silent cleartext fallback.
                        Picker("Security", selection: Binding(
                            get: { imapSecurity },
                            set: { newMode in
                                imapSecurity = newMode
                                useTLS = newMode != .none
                                // Only a user pick moves the port (not a
                                // prefill), and only while the field still
                                // holds another mode's default.
                                let defaults = Set(IMAPSecurity.allCases.map { String($0.defaultPort) })
                                if defaults.contains(port) { port = String(newMode.defaultPort) }
                            })) {
                            Text(verbatim: "TLS").tag(IMAPSecurity.tls)
                            Text(verbatim: "STARTTLS").tag(IMAPSecurity.starttls)
                            Text("None").tag(IMAPSecurity.none)
                        }
                        .labelsHidden()
                        .fixedSize()
                        Spacer(minLength: 0)
                    }
                }
                TextField("Username", text: $username)
                    #if DEBUG
                    .uiProbe("username", role: "field", label: "username")
                    #endif
                    .onChange(of: username) { newUser in
                        if smtpUsernameAuto { smtpUsername = newUser }
                    }
                HStack {
                    // Re-authenticate (S75): masked by default, eye button
                    // reveals; the same field either way so focus and text
                    // survive the toggle.
                    Group {
                        if reauthPassword.isRevealed {
                            TextField("Password", text: $password)
                                #if DEBUG
                                .uiProbe("password", role: "field", label: "password")
                                #endif
                        } else {
                            SecureField("Password", text: $password)
                                #if DEBUG
                                .uiProbe("password", role: "field", label: "password")
                                #endif
                        }
                    }
                    .onChange(of: password) { newPassword in
                        if smtpPasswordAuto { smtpPassword = newPassword }
                    }
                    if existingAccountId != nil {
                        Button {
                            reauthPassword.toggleReveal()
                        } label: {
                            Image(systemName: reauthPassword.isRevealed ? "eye.slash" : "eye")
                        }
                        .buttonStyle(.borderless)
                        .help(reauthPassword.isRevealed ? "Hide password" : "Show password")
                        .accessibilityLabel(reauthPassword.isRevealed ? "Hide password" : "Show password")
                        Button("Test") { startTest() }
                            .disabled(!fieldsComplete || testPhase == .testing || phase == .connecting)
                        reauthInlineStatus(leg: .imap)
                    }
                }
                if existingAccountId != nil {
                    Text("The stored password is shown masked; use the eye to reveal it, or type a new one, then Test.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("Sending (SMTP)") {
                    TextField("SMTP host", text: $smtpHost)
                        #if DEBUG
                        .uiProbe("smtpHost", role: "field", label: "smtpHost")
                        #endif
                        .onChange(of: smtpHost) { edited in
                            smtpHostAuto = edited == AccountConfig.defaultSMTPHost(imapHost: host)
                        }
                    LabeledContent("SMTP port") {
                        HStack(spacing: 8) {
                            TextField("SMTP port", text: $smtpPort).labelsHidden().frame(width: 64)
                            Toggle("SMTP uses TLS", isOn: $smtpUseTLS)
                            Spacer(minLength: 0)
                        }
                    }
                    TextField("SMTP username", text: $smtpUsername)
                        #if DEBUG
                        .uiProbe("smtpUsername", role: "field", label: "smtpUsername")
                        #endif
                        .onChange(of: smtpUsername) { edited in
                            smtpUsernameAuto = edited == effectiveUsername
                        }
                    HStack {
                        Group {
                            if reauthSMTPReveal.isRevealed {
                                TextField("SMTP password", text: $smtpPassword)
                                    #if DEBUG
                                    .uiProbe("smtpPassword", role: "field", label: "smtpPassword")
                                    #endif
                            } else {
                                SecureField("SMTP password", text: $smtpPassword)
                                    #if DEBUG
                                    .uiProbe("smtpPassword", role: "field", label: "smtpPassword")
                                    #endif
                            }
                        }
                        .onChange(of: smtpPassword) { edited in
                            smtpPasswordAuto = edited == password
                        }
                        if existingAccountId != nil {
                            // GUI-5: SMTP gets the same reveal + Test as IMAP.
                            // The Test is the shared probe (SMTP leg = login
                            // only, nothing is sent); this row shows the SMTP
                            // verdict.
                            Button {
                                reauthSMTPReveal.toggleReveal()
                            } label: {
                                Image(systemName: reauthSMTPReveal.isRevealed ? "eye.slash" : "eye")
                            }
                            .buttonStyle(.borderless)
                            .help(reauthSMTPReveal.isRevealed ? "Hide SMTP password" : "Show SMTP password")
                            .accessibilityLabel(reauthSMTPReveal.isRevealed ? "Hide SMTP password" : "Show SMTP password")
                            Button("Test") { startTest() }
                                .help("Sign in to the SMTP server without sending anything.")
                                .disabled(!fieldsComplete || testPhase == .testing || phase == .connecting)
                            reauthInlineStatus(leg: .smtp)
                        }
                    }
                }

                if existingAccountId == nil {
                    firstSyncSection
                }
            }
            .textFieldStyle(.roundedBorder)
            .disabled(phase == .connecting)

            testResultBlock

            resultBanner
            if let saveError {
                Label(saveError, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
                    .font(.caption)
                    .textSelection(.enabled)
            }
            }
            // 16 pt (was 20): Catalan/Spanish labels make the form ~5 pt wider
            // than the 420 pt sheet at 20 pt, which clipped the field edges.
            .padding(16)
            .frame(width: SheetLayout.width)
            }

            Divider()
            // Footer pinned outside the scroll area: always on screen (S71).
            HStack {
                if phase == .connecting {
                    // The sync itself runs in `AppModel`'s already-detached
                    // background task, independent of this view — dismissing
                    // here does not stop it. "Cancel" would
                    // be misleading at this point (there's nothing left to
                    // actually cancel), so it becomes an explicit "keep
                    // going, but let me back into the app" affordance.
                    Button("Continue in Background") { dismiss() }
                } else {
                    footerButton("Cancel", id: "cancel", role: .cancel, key: "\u{1b}") { syncAttemptWanted = false; dismiss() }
                }
                if case .failure = phase,
                   AccountSheetButtons.showsSaveAnyway(isFailure: true, isReauth: existingAccountId != nil) {
                    footerButton("Save anyway", id: "saveAnyway") { saveAnyway() }
                }
                Spacer()
                if phase == .connecting || testPhase == .testing {
                    ProgressView().controlSize(.small)
                }
                if existingAccountId != nil, AccountSheetButtons.footerShowsEditActions(isConnecting: phase == .connecting) {
                    // GUI-5: persist the credentials without needing a passing
                    // test (a wrong-password account must still be fixable
                    // later); a known-failed test asks first.
                    footerButton("Save", id: "save", enabled: AccountSheetButtons.canSave(
                        fieldsComplete: fieldsComplete, isConnecting: phase == .connecting,
                        isSuccess: { if case .success = phase { return true } else { return false } }(),
                        isTesting: testPhase == .testing)) { saveReauth() }
                        .help("Store these credentials without reconnecting.")
                }
                if AccountSheetButtons.footerShowsEditActions(isConnecting: phase == .connecting) {
                    footerButton("Test Connection", id: "testConnection", enabled: canTest) { startTest() }
                        .help("Check the IMAP and SMTP sign-in without saving the account.")
                }
                footerButton(existingAccountId == nil ? "Sync" : "Reconnect", id: "sync", enabled: canSync, key: "\r") { startSync() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 12)
        }
        .frame(minWidth: SheetLayout.width, idealWidth: SheetLayout.width, maxWidth: SheetLayout.width,
               minHeight: SheetLayout.minHeight, idealHeight: SheetLayout.idealHeight, maxHeight: .infinity)
        .movableResizableSheet()
        .onAppear {
            if let existingAccountId { prefill(from: existingAccountId) }
            if let duplicateFromAccountId {
                copySourceAccountId = duplicateFromAccountId
                applyDuplicateFields(from: duplicateFromAccountId)
            }
            if username.isEmpty { username = email }
        }
        .onDisappear {
            // Belt-and-suspenders alongside the Cancel button above: the
            // sheet can also go away via Escape or clicking outside, and a
            // preflight probe still in flight at that point must not go on
            // to persist anything either.
            syncAttemptWanted = false
            lookupTask?.cancel()
            lifetime.close()
            // S75: drop the prefilled credentials from view state on close.
            reauthPassword.close()
            reauthSMTPReveal.close()
            if existingAccountId != nil { password = ""; smtpPassword = "" }
        }
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didResignKeyNotification)) { _ in
            reauthPassword.remask()
            reauthSMTPReveal.remask()
        }
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didResignActiveNotification)) { _ in
            reauthPassword.remask()
            reauthSMTPReveal.remask()
        }
        .confirmationDialog(
            "The last connection test failed.", isPresented: $confirmSaveFailed, titleVisibility: .visible
        ) {
            Button("Save anyway") { saveCredentials() }
            Button("Keep Editing", role: .cancel) {}
        } message: {
            Text("These credentials did not sign in. Saving stores them, but the account will keep showing a sync error until they work.")
        }
    }

    /// Compact pass/fail next to the password field after an inline Test
    /// (full per-leg wording stays in `testResultBlock` below the form).
    @ViewBuilder
    private func reauthInlineStatus(leg: ReauthLeg) -> some View {
        switch testPhase {
        case .idle:
            EmptyView()
        case .testing:
            ProgressView().controlSize(.small)
        case .error:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
        case .done(let report, let tested):
            // Nothing shown once a field changed since the test (stale).
            if let outcome = ReauthTestOutcome.matching(
                report: report, tested: tested, current: buildConnectionConfig(), leg: leg) {
                Label(outcome.succeeded ? "Works" : "Failed",
                      systemImage: outcome.succeeded ? "checkmark.circle.fill" : "xmark.circle.fill")
                    .foregroundStyle(outcome.succeeded ? .green : .red)
                    .font(.caption)
            }
        }
    }

    @ViewBuilder
    private var resultBanner: some View {
        switch phase {
        case .editing:
            EmptyView()
        case .connecting:
            connectingProgress
        case .success(let message):
            Label(message, systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
                .font(.caption)
        case .failure(let status):
            Label(status.message, systemImage: "xmark.circle.fill")
                .foregroundStyle(.red)
                .font(.caption)
                .textSelection(.enabled)
        }
    }

    /// Detected-provider hint under the email field, plus the explicit
    /// "Use settings for" menu (Add mode only).
    @ViewBuilder
    private var providerNote: some View {
        if existingAccountId == nil {
            switch detection {
            case .preset(let preset):
                if case .done(let domain, _, let source) = lookupPhase, source == "mx" {
                    Text("\(domain) is hosted at \(preset.name) — server settings filled in. \(preset.note)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        #if DEBUG
                        .uiProbe("providerNote", role: "value", label: "preset:\(preset.id) via mx")
                        #endif
                } else {
                    Text("\(preset.name) detected — server settings filled in. \(preset.note)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            case .discovered(let found):
                // Found by DNS or a web page the domain's owner (or Mozilla)
                // controls: the user is the only check on the server names.
                Text("Settings found for \(found.domain) (\(AutoconfigPolicy.sourceLabel(found.source))) — check the server names, then Test.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                    #if DEBUG
                    .uiProbe("providerNote", role: "value", label: "discovered:\(found.source)")
                    #endif
            case .unsupported(_, let reason):
                Label(reason, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            case .generic:
                if case .looking(let domain) = lookupPhase {
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.small)
                        Text("Looking up settings for \(domain)…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    #if DEBUG
                    .uiProbe("providerNote", role: "value", label: "looking:\(domain)")
                    #endif
                }
            }
            Menu("Use settings for…") {
                ForEach(ProviderPreset.all) { preset in
                    Button(preset.name) { applyPreset(preset) }
                }
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Fill in the server settings for a provider — e.g. a custom domain hosted at Gmail or Fastmail.")
        }
    }

    /// The address changed (MM-14): keep or drop the lookup result, apply
    /// whatever the detection now says, and (re)schedule a debounced lookup.
    /// A change that keeps the same domain (typing the local part) neither
    /// cancels nor restarts anything.
    private func emailChanged() {
        guard existingAccountId == nil else { return }
        let domain = AutoconfigPolicy.lookupDomain(email: email)
        if domain != lookupPhase.domain {
            lookupTask?.cancel()
            lookupTask = nil
            lookupPhase = .idle
        }
        autoApplyPreset()
        applyLookupUsername()
        if let domain, lookupPhase == .idle { scheduleLookup(domain: domain) }
    }

    /// Debounce, then ask the core about `domain` off the main actor. The
    /// result is dropped unless the typed address still has that domain, and
    /// it only fills fields the user has not edited (`autoApplyPreset`).
    private func scheduleLookup(domain: String) {
        guard Preferences.shared.autoconfigLookupEnabled else { return }
        lookupTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: Self.lookupDebounce)
            guard !Task.isCancelled,
                  AutoconfigPolicy.accepts(resultFor: domain, currentEmail: email) else { return }
            lookupPhase = .looking(domain)
            let report = await model.autoconfigLookup(domain: domain)
            guard !Task.isCancelled,
                  AutoconfigPolicy.accepts(resultFor: domain, currentEmail: email) else { return }
            guard Preferences.shared.autoconfigLookupEnabled else {
                lookupPhase = .idle   // switched off while the lookup ran: apply nothing
                return
            }
            guard let report else {
                lookupPhase = .offline(domain)   // lookups off or the call failed: act as today
                return
            }
            if report.status == "offline" || report.status == "not_found" {
                // Nothing to show either way; remember the domain so typing
                // the local part does not re-ask.
                lookupPhase = report.status == "offline" ? .offline(domain) : .done(domain: domain, detection: .generic, source: nil)
                return
            }
            lookupPhase = .done(
                domain: domain,
                detection: AutoconfigPolicy.detection(from: report, domain: domain),
                source: report.source)
            autoApplyPreset()
            applyLookupUsername()
        }
    }

    /// Fill Username with the local part only when the found server wants it
    /// and the field is empty or still holds what this lookup put there; clear
    /// such a value when it no longer applies. A typed username is untouched.
    private func applyLookupUsername() {
        guard existingAccountId == nil else { return }
        var wanted: String?
        if case .discovered(let found) = detection {
            wanted = AutoconfigPolicy.username(for: found.username, email: email)
        }
        if let wanted {
            if username.isEmpty || username == lookupFilledUsername {
                username = wanted
                lookupFilledUsername = wanted
            }
        } else if let filled = lookupFilledUsername {
            if username == filled { username = "" }
            lookupFilledUsername = nil
        }
    }

    /// Email changed: apply the detected preset, or undo a preset's own
    /// values when the address no longer matches one — but never overwrite
    /// a host the user typed themselves.
    private func autoApplyPreset() {
        guard existingAccountId == nil,
              PresetFields.mayAutoApply(currentHost: host, lastAppliedHost: presetAppliedHost)
        else { return }
        switch detection {
        case .preset(let preset):
            if host != preset.imapHost { applyFields(PresetFields(preset), mask: currentMask()) }
        case .discovered(let found):
            if host != found.imapHost { applyFields(PresetFields(found), mask: currentMask()) }
        case .generic, .unsupported:
            if presetAppliedHost != nil, host == presetAppliedHost {
                host = ""
                port = "993"
                imapSecurity = .tls
                useTLS = true
                smtpPort = "587"
                smtpHostAuto = true
                smtpHost = ""
                presetAppliedHost = nil
            }
        }
    }

    /// The explicit "Use settings for…" choice overrides everything.
    private func applyPreset(_ preset: ProviderPreset) {
        applyFields(PresetFields(preset), mask: .all)
    }

    /// What the form holds now, for the never-overwrite-typed-fields rule.
    private func currentMask() -> PresetFields.ApplyMask {
        var now = PresetFields(.gmail)
        now.host = host; now.port = port; now.imapSecurity = imapSecurity
        now.smtpHost = smtpHost; now.smtpPort = smtpPort; now.smtpUseTLS = smtpUseTLS
        return PresetFields.mask(current: now, lastApplied: lastAppliedFields, smtpHostAuto: smtpHostAuto)
    }

    /// Write `fields`; with a partial `mask`, only the secondary fields the
    /// user has not edited (the host was already gated by `mayAutoApply`).
    private func applyFields(_ fields: PresetFields, mask: PresetFields.ApplyMask) {
        host = fields.host
        if mask.port { port = fields.port }
        if mask.security { useTLS = fields.useTLS; imapSecurity = fields.imapSecurity }
        if mask.smtpHost {
            smtpHost = fields.smtpHost
            smtpHostAuto = fields.smtpHost == AccountConfig.defaultSMTPHost(imapHost: fields.host)
        }
        if mask.smtpPort { smtpPort = fields.smtpPort }
        if mask.smtpTLS { smtpUseTLS = fields.smtpUseTLS }
        presetAppliedHost = fields.host
        lastAppliedFields = fields
    }

    /// First-sync scope (S62): history window, plus the folder picker once
    /// a test has listed the server's folders.
    @ViewBuilder
    private var firstSyncSection: some View {
        Section("First sync") {
            Picker("Import", selection: $initialHistory) {
                ForEach(InitialHistory.allCases) { Text($0.label).tag($0) }
            }
            #if DEBUG
            .uiProbe("initialHistory", role: "field", label: "initialHistory")
            #endif
            if let mailboxes = testedMailboxes, !mailboxes.isEmpty {
                FolderChecklist(
                    mailboxes: mailboxes,
                    selection: Binding(
                        get: { selectedMailboxes ?? [] },
                        set: { selectedMailboxes = $0 }
                    )
                )
                if noFolderSelected {
                    Text("Choose at least one folder to sync.")
                        .font(.caption)
                        .foregroundStyle(.red)
                }
            } else {
                Text("Test Connection to choose which folders to sync. Without it, Castellum syncs Inbox, Sent, Drafts, Trash, Junk and subscribed folders.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// Result of the last Test Connection (S62): one ticked/crossed line per
    /// leg, plus a stale warning once the fields no longer match what was
    /// tested.
    @ViewBuilder
    private var testResultBlock: some View {
        switch testPhase {
        case .idle:
            EmptyView()
        case .testing:
            Text("Testing the connection…")
                .font(.caption)
                .foregroundStyle(.secondary)
        case .error(let message):
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.caption)
                .textSelection(.enabled)
        case .done(let report, let tested):
            VStack(alignment: .leading, spacing: 3) {
                ForEach(Array(ConnectionTestPresentation.lines(for: report, config: tested).enumerated()),
                        id: \.offset) { _, line in
                    Label(line.text, systemImage: line.ok ? "checkmark.circle.fill" : "xmark.circle.fill")
                        .foregroundStyle(line.ok ? .green : .red)
                        .font(.caption)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if report.smtp?.errorKind == SmtpAuthDisabledGuidance.kind {
                    Link(SmtpAuthDisabledGuidance.linkTitle, destination: SmtpAuthDisabledGuidance.helpURL)
                        .font(.caption)
                }
                if tested.connectionSettings != buildConnectionConfig().connectionSettings {
                    Text("Settings changed since this test — test again to check them.")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    /// Live progress while `.connecting` — nothing shown
    /// until the first snapshot arrives (the initial LOGIN/CAPABILITY
    /// handshake has no mailbox/message counts to report yet), then the same
    /// formatted line the Status dashboard and sidebar tooltip use, plus a
    /// reminder that this can run in the background.
    @ViewBuilder
    private var connectingProgress: some View {
        if let pendingAccountId, let progress = model.accountSyncProgress[pendingAccountId] {
            VStack(alignment: .leading, spacing: 2) {
                SyncProgressLabel(progress: progress)
                Text("This can take a while for a large mailbox — Continue in Background and keep using Castellum.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    /// Fills every non-credential field from the account's stored config.
    /// Re-authenticate (S75): the stored password prefills the (masked)
    /// password field so it can be revealed, checked and re-tested. It lives
    /// only in `password`/`smtpPassword` and is cleared in `onDisappear`.
    private func prefill(from accountId: String) {
        guard let stored = model.storedConfig(for: accountId),
              let config = try? AccountConfig.decode(fromJSON: stored) else { return }
        password = ReauthPasswordField(storedConfigJSON: stored).value
        let storedSMTPPassword = config.effectiveSMTP.password
        smtpPasswordAuto = storedSMTPPassword == config.password
        smtpPassword = storedSMTPPassword
        email = config.email
        displayName = config.displayName ?? ""
        host = config.host
        port = String(config.port)
        username = config.username
        useTLS = config.useTls
        imapSecurity = config.effectiveIMAPSecurity
        let smtp = config.effectiveSMTP
        smtpHost = smtp.host
        smtpHostAuto = smtp.host == AccountConfig.defaultSMTPHost(imapHost: config.host)
        smtpPort = String(smtp.port)
        smtpUsername = smtp.username
        smtpUsernameAuto = smtp.username == config.username
        smtpUseTLS = smtp.useTls
        storedMailboxes = config.mailboxes
        storedInitialSyncSince = config.initialSyncSince
    }

    /// Applies `AccountConfig.duplicateFields()` (the pure mapping) to this
    /// sheet's form state. Email, username and password are untouched here —
    /// the caller (this is still an Add) leaves them for the user to type.
    private func applyDuplicateFields(from accountId: String) {
        guard let stored = model.storedConfig(for: accountId),
              let config = try? AccountConfig.decode(fromJSON: stored) else { return }
        let fields = config.duplicateFields()
        displayName = fields.displayName
        host = fields.host
        port = fields.port
        useTLS = fields.useTLS
        imapSecurity = fields.imapSecurity
        smtpHost = fields.smtpHost
        smtpHostAuto = fields.smtpHostAuto
        smtpPort = fields.smtpPort
        smtpUseTLS = fields.smtpUseTLS
    }

    private var effectiveUsername: String {
        let user = Self.clean(username)
        return user.isEmpty ? email.trimmingCharacters(in: .whitespaces) : user
    }

    /// The account id `buildConfig()` will use for the current fields: the
    /// pinned `existingAccountId` when re-authenticating, otherwise email
    /// (falling back to host when email is blank), exactly as Add always
    /// has. Pulled out so the conflict check below asks about the exact same
    /// id `buildConfig()` derives — never a re-implementation that could
    /// drift from it. Static and pure so it's unit-testable without a view.
    static func derivedAccountId(email: String, host: String, existingAccountId: String?) -> String {
        existingAccountId ?? (clean(email).isEmpty ? clean(host) : clean(email))
    }

    /// True when saving now would derive an id that collides with an
    /// existing account — including the "Copy settings from"/Duplicate…
    /// source itself — and so would silently overwrite that account's
    /// stored config instead of creating a new one (`AccountStore.save`
    /// updates an existing entry in place). Always false when
    /// re-authenticating: `existingAccountId` pins the id deliberately,
    /// there's nothing derived to collide.
    static func wouldOverwriteExistingAccount(
        email: String, host: String, existingAccountId: String?, accounts: [Account]
    ) -> Bool {
        guard existingAccountId == nil else { return false }
        let derivedId = normalisedAccountId(
            derivedAccountId(email: email, host: host, existingAccountId: nil))
        guard !derivedId.isEmpty else { return false }
        return accounts.contains { normalisedAccountId($0.id) == derivedId }
    }

    private var idConflict: Bool {
        Self.wouldOverwriteExistingAccount(
            email: email, host: host, existingAccountId: existingAccountId, accounts: model.accounts)
    }

    /// Builds the config for the current field values, pinning the existing
    /// account id (re-authenticating) rather than deriving a new one, with
    /// the first-sync scope applied.
    private func buildConfig() -> AccountConfig {
        var config = buildConnectionConfig()
        config.mailboxes = scopeMailboxes(for: config)
        config.initialSyncSince = existingAccountId != nil
            ? storedInitialSyncSince : initialHistory.sinceDate()
        return config
    }

    /// The connection fields only (no S62 scope) — what a test checks.
    private func buildConnectionConfig() -> AccountConfig {
        let trimmedEmail = email.trimmingCharacters(in: .whitespaces)
        let account = Self.derivedAccountId(email: trimmedEmail, host: host, existingAccountId: existingAccountId)
        var config = AccountConfig(
            accountId: account,
            email: trimmedEmail,
            host: Self.clean(host),
            port: Int(port) ?? 993,
            username: effectiveUsername,
            password: password,
            useTls: imapSecurity != .none,
            allowInvalidCerts: false,
            // Store nil for anything left at (or edited back to) empty so the
            // read-time defaulting in AccountConfig.effectiveSMTP applies.
            smtpHost: Self.clean(smtpHost).isEmpty ? nil : Self.clean(smtpHost),
            smtpPort: Int(smtpPort),
            smtpUsername: Self.clean(smtpUsername).isEmpty ? nil : Self.clean(smtpUsername),
            smtpPassword: smtpPassword.isEmpty ? nil : smtpPassword,
            smtpUseTls: smtpUseTLS,
            displayName: displayName.isEmpty ? nil : displayName
        )
        config.imapSecurity = imapSecurity.rawValue
        return config
    }

    /// Folder filter for `connection`: the stored one when
    /// re-authenticating; otherwise the picker's choice (nil = defaults) —
    /// but only while the folder list still belongs to these connection
    /// settings. A list from a stale test (another host or user) is ignored
    /// rather than applied to a different server.
    private func scopeMailboxes(for connection: AccountConfig) -> [String]? {
        if existingAccountId != nil { return storedMailboxes }
        guard case .done(let report, let tested) = testPhase, report.imap.ok,
              tested.connectionSettings == connection.connectionSettings,
              let selectedMailboxes else { return nil }
        return SyncScope.mailboxFilter(selected: selectedMailboxes, mailboxes: report.mailboxes)
    }

    /// Runs the connection inline: a preflight IMAP + SMTP probe first
    /// ( — fail fast before a possibly long first sync), then,
    /// once it passes, the sync itself: spinner (with live progress once the
    /// first mailbox starts fetching), then a green checkmark that closes
    /// the sheet after a beat, or a red xmark that leaves the sheet open
    /// with the fields still editable so the user can fix and retry. The
    /// user isn't stuck here for the whole first sync either way — see
    /// "Continue in Background" below.
    ///
    /// The probe is skipped when a Test Connection with byte-identical
    /// settings already passed in this sheet session (`testPhase`'s `.done`
    /// case) — see `AppModel.addAccountWithPreflight`. While it runs, `phase`
    /// deliberately stays `.editing` (only `testPhase` becomes `.testing`),
    /// so Cancel is still offered exactly as it is during a manual Test
    /// Connection — a probe is quick, and closing the sheet here just
    /// abandons the sheet's interest in the result, same as Cancel during a
    /// manual test. `phase` only becomes `.connecting` (Cancel becomes
    /// "Continue in Background") once the probe has actually passed and the
    /// real sync starts.
    ///
    /// Cancel-while-probing (found in review of PR #163): `syncAttemptWanted`
    /// is reset true here and flipped false by Cancel/`onDisappear` — passed
    /// through as `isStillWanted` so a probe that resolves as PASSING after
    /// the user has already backed out never reaches `addAccount`.
    private func startSync() {
        // Capture the lifetime now, while the view is certainly installed, so
        // the deferred dismissal below never depends on reading @State after
        // the sheet has gone (GUI-2 review hardening).
        let lifetime = self.lifetime
        let connectionConfig = buildConnectionConfig()
        let config = buildConfig()
        guard let json = try? config.encodedJSON() else {
            model.errorText = "Could not encode account config."
            return
        }
        var priorTest: (report: ConnectionTestReport, tested: AccountConfig)?
        if case .done(let report, let tested) = testPhase {
            priorTest = (report, tested)
        }
        // Mirrors `AppModel.addAccountWithPreflight`'s own skip condition —
        // purely so "Testing the connection…" only replaces a stale result
        // when a probe is actually about to run (cosmetic; the model
        // decides for real).
        let willSkipProbe = priorTest.map {
            $0.tested.connectionSettings == connectionConfig.connectionSettings
                && ConnectionTestPresentation.allPassed($0.report)
        } ?? false
        if !willSkipProbe {
            testPhase = .testing
        }
        syncAttemptWanted = true
        pendingAccountId = config.accountId
        model.addAccountWithPreflight(
            accountID: config.accountId, connectionConfig: connectionConfig, fullConfigJSON: json,
            priorTest: priorTest,
            isStillWanted: { syncAttemptWanted },
            onProbeComplete: { report in
                testPhase = .done(report, connectionConfig)
                // Fresh folder list → fresh default ticks (the old list may
                // be from a different server).
                selectedMailboxes = report.imap.ok ? SyncScope.defaultSelection(report.mailboxes) : nil
            },
            onProbeRequestFailed: { error in
                testPhase = .error("The connection test couldn’t run: \(error.localizedDescription)")
            },
            onProbePassed: { phase = .connecting },
            completion: { status in
                switch status.state {
                case .ok:
                    phase = .success("Connected to \(config.imapEndpoint).")
                    Task {
                        try? await Task.sleep(nanoseconds: 900_000_000)
                        lifetime.dismissIfOpen { dismiss() }
                    }
                default:
                    phase = .failure(status)
                }
            }
        )
    }

    /// Test Connection (S62): preflight IMAP + SMTP for the current fields
    /// without saving anything. Runs off the main thread
    /// (`AppModel.testConnection` detaches); the UI stays responsive and the
    /// sheet can still be cancelled while it runs.
    private func startTest() {
        let config = buildConnectionConfig()
        guard let json = try? config.connectionTestJSON() else {
            testPhase = .error("Could not encode the account settings.")
            return
        }
        testGeneration += 1
        let generation = testGeneration
        testPhase = .testing
        Task { @MainActor in
            let result = await model.testConnection(configJSON: json)
            guard generation == testGeneration else { return }
            switch result {
            case .success(let report):
                testPhase = .done(report, config)
                // Fresh folder list → fresh default ticks (the old list may
                // be from a different server).
                selectedMailboxes = report.imap.ok ? SyncScope.defaultSelection(report.mailboxes) : nil
            case .failure(let error):
                testPhase = .error("The connection test couldn’t run: \(error.localizedDescription)")
            }
        }
    }

    /// Outcome of the latest probe if it tested exactly the current
    /// connection fields; nil when untested, running, or edited since.
    private var currentTestOutcome: ReauthTestOutcome? {
        // `.error` (the probe couldn't run) and `.idle`/`.testing` tell us
        // nothing about the credentials, so they never trigger the warning.
        guard case .done(let report, let tested) = testPhase else { return nil }
        return ReauthTestOutcome.matching(report: report, tested: tested, current: buildConnectionConfig())
    }

    /// Re-authenticate "Save" (GUI-5): store immediately, or ask first when
    /// the latest test of these exact fields failed.
    private func saveReauth() {
        switch ReauthSaveDecision.decide(lastTest: currentTestOutcome) {
        case .saveNow: saveCredentials()
        case .confirmFailedTest: confirmSaveFailed = true
        }
    }

    /// Persist the re-authenticated config without syncing, then close.
    private func saveCredentials() {
        saveAnyway()
    }

    /// "Save anyway": persist the config without a successful sync, leaving
    /// the account showing the failed status from the attempt above — the
    /// user can retry later via Re-authenticate or the Status dashboard.
    /// Re-checks `idConflict` even though the Sync button already blocked
    /// it: the sheet stays open and editable after a failed sync, so the
    /// email could have been changed to a colliding one since.
    private func saveAnyway() {
        saveError = nil
        guard !idConflict else {
            saveError = "An account with this id already exists; use a different email."
            return
        }
        let config = buildConfig()
        guard let json = try? config.encodedJSON() else {
            saveError = "Could not encode account config."
            return
        }
        // Dismiss only on success: closing on failure would run onDisappear
        // and wipe the passwords the user just re-entered.
        if model.saveAccountWithoutSync(accountID: config.accountId, configJSON: json) {
            dismiss()
        } else {
            saveError = model.errorText ?? "Could not save the account."
            model.errorText = nil
        }
    }
}
