// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

///: "Duplicate…" (Settings > Accounts) and "Copy settings from"
/// (Add sheet) both prefill `AccountSetupSheet` via `AccountConfig
/// .duplicateFields()` — a pure mapping from a stored config to the sheet's
/// non-identity form fields. These tests exercise that function directly (no
/// SwiftUI, no sheet, no Keychain).
final class AccountDuplicateFieldsTests: XCTestCase {
    private func makeConfig(
        accountId: String = "source-account",
        email: String = "source@example.com",
        host: String = "imap.example.com",
        port: Int = 993,
        username: String = "source@example.com",
        password: String = "sourcepass",
        useTls: Bool = true,
        smtpHost: String? = nil,
        smtpPort: Int? = nil,
        smtpUsername: String? = nil,
        smtpPassword: String? = nil,
        smtpUseTls: Bool? = nil,
        displayName: String? = "Source Person"
    ) -> AccountConfig {
        AccountConfig(
            accountId: accountId, email: email, host: host, port: port,
            username: username, password: password, useTls: useTls,
            allowInvalidCerts: false, smtpHost: smtpHost, smtpPort: smtpPort,
            smtpUsername: smtpUsername, smtpPassword: smtpPassword,
            smtpUseTls: smtpUseTls, displayName: displayName)
    }

    /// `AccountDuplicateFields` has no email/username/password properties at
    /// all — the type itself is the guarantee identity never carries over.
    /// This asserts the field that IS carried (display name) round-trips, as
    /// a sanity check that the mapping actually ran.
    func testIdentityFieldsAreNotPartOfTheMapping() {
        let fields = makeConfig(displayName: "Source Person").duplicateFields()
        XCTAssertEqual(fields.displayName, "Source Person")
    }

    func testImapHostPortAndTlsAreCopied() {
        let fields = makeConfig(host: "imap.fastmail.com", port: 143, useTls: false).duplicateFields()
        XCTAssertEqual(fields.host, "imap.fastmail.com")
        XCTAssertEqual(fields.port, "143")
        XCTAssertFalse(fields.useTLS)
    }

    func testExplicitSMTPSettingsAreCopiedAndNotMarkedAuto() {
        let fields = makeConfig(
            host: "imap.fastmail.com",
            smtpHost: "smtp.override.example.com", smtpPort: 465, smtpUseTls: true
        ).duplicateFields()
        XCTAssertEqual(fields.smtpHost, "smtp.override.example.com")
        XCTAssertEqual(fields.smtpPort, "465")
        XCTAssertTrue(fields.smtpUseTLS)
        XCTAssertFalse(fields.smtpHostAuto, "an explicit override must not read back as auto-derived")
    }

    /// No explicit SMTP settings stored: the mapping copies the same
    /// smart-defaulted values `effectiveSMTP` derives (imap. -> smtp., port
    /// 587) and reports them as auto, so a duplicated account keeps
    /// following IMAP-host edits the same way a freshly-added one does.
    func testDefaultedSMTPSettingsAreCopiedAndMarkedAuto() {
        let fields = makeConfig(host: "imap.example.com").duplicateFields()
        XCTAssertEqual(fields.smtpHost, "smtp.example.com")
        XCTAssertEqual(fields.smtpPort, "587")
        XCTAssertTrue(fields.smtpUseTLS)
        XCTAssertTrue(fields.smtpHostAuto)
    }

    func testDuplicateFieldsNeverMutatesTheSourceConfig() {
        let config = makeConfig(host: "imap.example.com", port: 993)
        let before = config
        _ = config.duplicateFields()
        XCTAssertEqual(config, before, "duplicateFields() must be side-effect free")
    }
}

/// `AccountSetupSheet.wouldOverwriteExistingAccount` is the exact check the
/// sheet runs (via `idConflict`) before Sync/"Save anyway" are allowed to
/// fire — these call the sheet's own static function directly, not a
/// re-implementation of its formula, so they actually catch a regression in
/// the id derivation itself.
final class AccountIdConflictTests: XCTestCase {
    private let existingAccounts = [
        Account(id: "source@example.com", email: "source@example.com",
                displayName: "Source Person", provider: "imap"),
        Account(id: "imap.hostid.example.com", email: "weird@example.com",
                displayName: nil, provider: "imap"),
    ]

    /// Typing the source's own email into "Copy settings from"/Duplicate…
    /// derives the source's own id — saving would overwrite it in place.
    func testTypingTheSourceAccountsOwnEmailConflicts() {
        XCTAssertTrue(AccountSetupSheet.wouldOverwriteExistingAccount(
            email: "source@example.com", host: "imap.example.com",
            existingAccountId: nil, accounts: existingAccounts))
    }

    /// A blank email falls back to host for the derived id (same formula as
    /// `buildConfig()`), so an account whose id equals its host collides
    /// too, even though its email was never typed.
    func testBlankEmailFallsBackToHostAndCanStillConflict() {
        XCTAssertTrue(AccountSetupSheet.wouldOverwriteExistingAccount(
            email: "", host: "imap.hostid.example.com",
            existingAccountId: nil, accounts: existingAccounts))
    }

    /// A genuinely new email/host pair never conflicts.
    func testUniqueEmailDoesNotConflict() {
        XCTAssertFalse(AccountSetupSheet.wouldOverwriteExistingAccount(
            email: "dup@example.com", host: "imap.example.com",
            existingAccountId: nil, accounts: existingAccounts))
    }

    /// FP01: capitals and stray spaces do not make a different mailbox.
    func testCapitalsAndTrailingSpaceStillConflict() {
        for typed in ["Source@Example.com", "source@example.com ", " SOURCE@example.com"] {
            XCTAssertTrue(AccountSetupSheet.wouldOverwriteExistingAccount(
                email: typed, host: "imap.example.com",
                existingAccountId: nil, accounts: existingAccounts), typed)
        }
    }

    /// FP01: a blank email cannot add an account; re-authenticate may rely on username.
    func testBlankEmailBlocksAddButNotReauth() {
        func complete(email: String, existing: String?) -> Bool {
            AccountSetupSheet.fieldsComplete(
                host: "h", password: "p", port: "993", smtpPort: "587",
                username: "u@x.test", email: email, existingAccountId: existing)
        }
        XCTAssertFalse(complete(email: "", existing: nil))
        XCTAssertFalse(complete(email: "   ", existing: nil))
        XCTAssertTrue(complete(email: "a@x.test", existing: nil))
        XCTAssertTrue(complete(email: "", existing: "a@x.test"))
    }

    /// FP01: ports outside 1...65535 are refused (IMAP and SMTP).
    func testPortRange() {
        for bad in ["0", "99999", "65536", "-1", "abc", ""] {
            XCTAssertFalse(AccountSetupSheet.validPort(bad), bad)
        }
        for good in ["1", "993", "65535"] { XCTAssertTrue(AccountSetupSheet.validPort(good), good) }
        XCTAssertFalse(AccountSetupSheet.fieldsComplete(
            host: "h", password: "p", port: "993", smtpPort: "99999",
            username: "", email: "a@x.test", existingAccountId: nil))
        XCTAssertTrue(AccountSetupSheet.fieldsComplete(
            host: "h", password: "p", port: "993", smtpPort: "",
            username: "", email: "a@x.test", existingAccountId: nil))
    }

    /// Re-authenticate always pins the id and never derives one, so it can
    /// never trip this check even when the email field mirrors an existing
    /// account (it's re-authenticating that very account).
    func testReauthenticateNeverConflicts() {
        XCTAssertFalse(AccountSetupSheet.wouldOverwriteExistingAccount(
            email: "source@example.com", host: "imap.example.com",
            existingAccountId: "source@example.com", accounts: existingAccounts))
    }
}

/// End-to-end guarantee that the pure mapping's caller (the sheet) actually
/// honours: duplicating an account creates a NEW account id and never
/// touches the source account's stored config. Uses a temp profile (its own
/// Keychain namespace, see `AccountStore.service(forProfileDirectory:)`) and
/// deletes everything it saves in tearDown — never the real profile/Keychain,
/// same pattern as `AccountReconnectTests`.
@MainActor
final class AccountDuplicateSaveTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-duplicate-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    func testDuplicatingAnAccountNeverOverwritesTheSource() throws {
        let model = makeModel()
        let sourceId = "source-\(UUID().uuidString)"
        let sourceConfig = AccountConfig(
            accountId: sourceId, email: "source@example.com", host: "imap.example.com",
            port: 993, username: "source@example.com", password: "sourcepass", useTls: true,
            allowInvalidCerts: false, smtpHost: nil, smtpPort: nil, smtpUsername: nil,
            smtpPassword: nil, smtpUseTls: nil, displayName: "Source Person")
        let sourceJSON = try sourceConfig.encodedJSON()
        model.saveAccountWithoutSync(accountID: sourceId, configJSON: sourceJSON)

        // Mirrors what the Duplicate… sheet does: prefill from the source's
        // duplicateFields(), give the new account a fresh identity, and save
        // it under a NEW id derived from the (new) email — never the source id.
        let fields = sourceConfig.duplicateFields()
        let duplicateId = "dup@example.com"
        let duplicateConfig = AccountConfig(
            accountId: duplicateId, email: "dup@example.com", host: fields.host,
            port: Int(fields.port) ?? 993, username: "dup@example.com",
            password: "duppass", useTls: fields.useTLS, allowInvalidCerts: false,
            smtpHost: fields.smtpHostAuto ? nil : fields.smtpHost,
            smtpPort: Int(fields.smtpPort), smtpUsername: nil, smtpPassword: nil,
            smtpUseTls: fields.smtpUseTLS,
            displayName: fields.displayName.isEmpty ? nil : fields.displayName)
        model.saveAccountWithoutSync(
            accountID: duplicateId, configJSON: try duplicateConfig.encodedJSON())

        XCTAssertEqual(
            model.storedConfig(for: sourceId), sourceJSON,
            "duplicating must never touch the source account's stored config")

        let storedDuplicate = try XCTUnwrap(
            model.storedConfig(for: duplicateId).flatMap { try? AccountConfig.decode(fromJSON: $0) })
        XCTAssertEqual(storedDuplicate.host, "imap.example.com")
        XCTAssertEqual(storedDuplicate.email, "dup@example.com")
        XCTAssertEqual(storedDuplicate.username, "dup@example.com")
        XCTAssertNotEqual(storedDuplicate.accountId, sourceId)
    }

    /// Against `model.accounts` as `reloadAccounts()` actually populates it —
    /// the core's durable DB row a real sync creates, not a hand-built array
    /// — typing the source account's own id into the Duplicate… sheet would
    /// derive that same id, and `wouldOverwriteExistingAccount` (the exact
    /// check `idConflict` runs) must catch it before any save happens.
    ///
    /// `model.accounts` only reflects an account once it has a durable core
    /// row, which only a sync (even a failed one, see `performSync`)
    /// creates — `saveAccountWithoutSync` alone does not (it only writes the
    /// Keychain blob; `AccountDuplicateSaveTests.
    /// testDuplicatingAnAccountNeverOverwritesTheSource` above covers that
    /// path directly instead). So this syncs for real against local
    /// GreenMail (`scripts/local-imap`, same fixed-password preset account
    /// as `AccountReconnectTests`) and skips, rather than fails, if it isn't
    /// running — same pattern, same reason (CI without Docker stays green).
    private static let greenMailHost = "127.0.0.1"
    private static let greenMailPort: UInt16 = 3143

    private func greenMailReachable() -> Bool {
        let sock = socket(AF_INET, SOCK_STREAM, 0)
        defer { close(sock) }
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = Self.greenMailPort.bigEndian
        addr.sin_addr.s_addr = inet_addr(Self.greenMailHost)
        return withUnsafePointer(to: &addr) { ptr in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                connect(sock, sa, socklen_t(MemoryLayout<sockaddr_in>.size)) == 0
            }
        }
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 15,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 50_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    func testTypingSourceEmailIntoDuplicateSheetIsCaughtBeforeSaving() async throws {
        try XCTSkipUnless(
            greenMailReachable(), "GreenMail not running on \(Self.greenMailHost):\(Self.greenMailPort)")
        let model = makeModel()
        // The account id an Add would derive from typing this email — the
        // realistic collision, not a synthetic UUID id.
        let sourceId = "source-\(UUID().uuidString)@example.com"
        let config = """
            {"account_id":"\(sourceId)","email":"\(sourceId)","host":"\(Self.greenMailHost)",
             "port":\(Self.greenMailPort),"username":"test@local.test","password":"secret",
             "use_tls":false,"allow_invalid_certs":false,"mailboxes":["INBOX"]}
            """
        var addResult: AccountSyncStatus?
        model.addAccount(accountID: sourceId, configJSON: config, completion: { addResult = $0 })
        try await waitUntil({ addResult != nil }, "source add never completed")
        XCTAssertEqual(addResult?.state, .ok)
        try await waitUntil(
            { model.accounts.contains { $0.id == sourceId } },
            "reloadAccounts() never picked up the just-added source account")

        // The Duplicate… sheet opens with email blank; the user then types
        // the source's own email (== its id here) by mistake or habit.
        XCTAssertTrue(AccountSetupSheet.wouldOverwriteExistingAccount(
            email: sourceId, host: Self.greenMailHost,
            existingAccountId: nil, accounts: model.accounts))
    }

    /// Audit #3439: stray whitespace in host/username must not count as filled in
    /// and must not reach the connection.
    func testBlankOrSpacedHostAndUsernameAreNotComplete() {
        XCTAssertFalse(AccountSetupSheet.fieldsComplete(
            host: "  \n", password: "p", port: "993", smtpPort: "", username: "", email: "a@x.test",
            existingAccountId: nil))
        XCTAssertFalse(AccountSetupSheet.fieldsComplete(
            host: "imap.x.test", password: "p", port: "993", smtpPort: "", username: " \t", email: "",
            existingAccountId: "x"))
        XCTAssertEqual(AccountSetupSheet.clean(" imap.x.test\n"), "imap.x.test")
        XCTAssertEqual(
            AccountSetupSheet.derivedAccountId(email: "", host: " imap.x.test ", existingAccountId: nil),
            "imap.x.test")
    }
}
