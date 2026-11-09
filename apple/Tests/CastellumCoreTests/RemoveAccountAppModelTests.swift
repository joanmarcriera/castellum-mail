// Castellum - Tests for FP04: removing an account takes its mail off cross-
// account lists, and the sync banner falls back to another failing account.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

@MainActor
final class RemoveAccountAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("castellum-remove-\(UUID().uuidString)")
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

    private func account(_ id: String) -> Account {
        Account(id: id, email: "\(id)@local.test", displayName: nil, provider: "imap")
    }

    private func header(_ id: String, accountId: String) -> MessageHeader {
        MessageHeader(id: id, accountId: accountId, mailboxId: "\(accountId):INBOX",
                      subject: nil, fromName: nil, fromAddress: nil, sentAt: nil, snippet: nil,
                      hasAttachments: false, hasBody: true, isRead: true, isStarred: false, threadCount: 1)
    }

    func testRemovingAccountClearsItsRowsFromUnifiedList() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedUnifiedRole = "inbox"
        model.messages = [header("a:1", accountId: "a"), header("b:1", accountId: "b")]
        model.selectedMessage = model.messages[1]

        model.dropRemovedAccountFromList("b")

        XCTAssertFalse(model.messages.contains { $0.accountId == "b" })
        XCTAssertNil(model.selectedMessage)
        XCTAssertNil(model.detail)
    }

    func testRemovingAccountClearsItsRowsFromSearchResults() {
        let model = makeModel()
        model.accounts = [account("a"), account("b")]
        model.selectedUnifiedRole = "inbox"
        model.searchText = "fox"
        model.messages = [header("a:1", accountId: "a"), header("b:1", accountId: "b")]

        model.dropRemovedAccountFromList("b")

        XCTAssertFalse(model.messages.contains { $0.accountId == "b" })
    }

    private func failure(_ id: String) -> AccountSyncStatus {
        let json = """
        {"account_id":"\(id)","email":"\(id)@local.test","host":"127.0.0.1","port":1,
         "username":"x","password":"x","use_tls":false,
         "allow_invalid_certs":false,"mailboxes":["INBOX"]}
        """
        let target = AccountSyncTarget(configJSON: json)!
        return AccountSyncStatus.failure(target: target, errorKind: "network", rawError: nil)
    }

    func testBannerFallsBackToAnotherFailingAccount() {
        let accounts = [account("a"), account("b")]
        var statuses = ["a": failure("a"), "b": failure("b")]
        XCTAssertEqual(AppModel.fallbackSyncBanner(accounts: accounts, statuses: statuses)?.accountId, "a")

        // B recovers: A's earlier failure must still be reported.
        statuses["b"] = nil
        XCTAssertEqual(AppModel.fallbackSyncBanner(accounts: accounts, statuses: statuses)?.accountId, "a")

        // Everything healthy: nothing to show.
        statuses["a"] = nil
        XCTAssertNil(AppModel.fallbackSyncBanner(accounts: accounts, statuses: statuses))
    }

    // MARK: per-account preferences (#3439)

    /// Removing an account forgets ITS signatures, legacy signature text and Markdown
    /// default, plus its identity aliases; another account is untouched.
    func testRemovingAnAccountForgetsItsPreferencesAndIdentities() throws {
        let suiteName = "castellum-remove-prefs-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }
        let prefs = Preferences(defaults: defaults)
        defaults.set(["a@x.test": "old text", "b@x.test": "keep"], forKey: "accountSignatures")
        _ = prefs.signatureLibrary.add(name: "Work", accountId: "a@x.test", text: "Ann")
        _ = prefs.signatureLibrary.add(name: "Work", accountId: "b@x.test", text: "Bob")
        prefs.setMarkdownDefault(true, for: "a@x.test")
        prefs.setMarkdownDefault(true, for: "b@x.test")

        let model = makeModel()
        let alias = Identity(
            id: 1, accountId: "a@x.test", email: "alias@x.test", displayName: nil,
            replyTo: nil)
        model.identities = ["a@x.test": [alias]]
        XCTAssertTrue(model.myAddresses.contains("alias@x.test"))

        model.forgetRemovedAccountPreferences("a@x.test", prefs: prefs)

        XCTAssertFalse(model.myAddresses.contains("alias@x.test"))
        XCTAssertTrue(prefs.signatureLibrary.items(for: "a@x.test").isEmpty)
        XCTAssertNil(prefs.signatureLibrary.defaultItem(for: "a@x.test"))
        XCTAssertFalse(prefs.markdownDefault(for: "a@x.test"))
        XCTAssertEqual(prefs.signatureLibrary.items(for: "b@x.test").count, 1)
        XCTAssertTrue(prefs.markdownDefault(for: "b@x.test"))
        let legacy = defaults.dictionary(forKey: "accountSignatures") as? [String: String]
        XCTAssertNil(legacy?["a@x.test"])
        XCTAssertEqual(legacy?["b@x.test"], "keep")
    }
}
