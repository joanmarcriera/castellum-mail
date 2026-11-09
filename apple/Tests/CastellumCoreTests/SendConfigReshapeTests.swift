// Castellum - regression test: every caller of the core's send_pending must
// hand it a SendConfig, not the flat AccountConfig the Keychain stores.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

/// `castellum_send_pending` deserialises `SendConfig` — `account_id`,
/// `from_email` and a NESTED `smtp` object (`native/crates/castellum-core/src/send.rs`).
/// The Keychain stores the flat `AccountConfig` shape instead: no `from_email`,
/// and the SMTP settings as optional `smtp_*` siblings. `drainSendPending`
/// passed that stored JSON straight through, so the delayed undo-send drain
/// and Outbox > Send Now both failed to deserialise — and `try?` swallowed it,
/// which looked exactly like "nothing to send". This pins the reshape.
@MainActor
final class SendConfigReshapeTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-sendconfig-\(UUID().uuidString)")
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

    private func storedConfigJSON(accountId: String) -> String {
        """
        {"account_id":"\(accountId)","email":"someone@local.test","host":"127.0.0.1",
         "port":3143,"username":"someone@local.test","password":"pw","use_tls":false,
         "allow_invalid_certs":false,"display_name":"Someone",
         "smtp_host":"127.0.0.1","smtp_port":3025,"smtp_username":"someone@local.test",
         "smtp_password":"pw","smtp_use_tls":false}
        """
    }

    func testSendConfigJSONReshapesTheStoredAccountConfig() throws {
        let model = makeModel()
        let accountId = "reshape-\(UUID().uuidString)"
        try model.accountStore.save(accountID: accountId,
                                    configJSON: storedConfigJSON(accountId: accountId))

        let json = try XCTUnwrap(model.sendConfigJSON(for: accountId),
                                 "a stored config must reshape into a SendConfig")
        let dict = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any])

        XCTAssertEqual(dict["account_id"] as? String, accountId)
        XCTAssertEqual(dict["from_email"] as? String, "someone@local.test")
        XCTAssertEqual(dict["from_name"] as? String, "Someone")
        // MM-17 S2: the Settings toggle rides along (default on).
        XCTAssertEqual(dict["plain_text_flowed"] as? Bool, Preferences.shared.sendFlowed)
        let smtp = try XCTUnwrap(dict["smtp"] as? [String: Any],
                                 "SendConfig.smtp must be a nested object")
        XCTAssertEqual(smtp["host"] as? String, "127.0.0.1")
        XCTAssertEqual(smtp["port"] as? Int, 3025)
        XCTAssertEqual(smtp["username"] as? String, "someone@local.test")
        XCTAssertEqual(smtp["use_tls"] as? Bool, false)
        // The flat stored keys must NOT survive the reshape: their presence is
        // what a SendConfig deserialise would choke on.
        XCTAssertNil(dict["smtp_host"])
        XCTAssertNil(dict["email"])
    }

    func testSendConfigJSONIsNilForAnUnknownAccount() {
        let model = makeModel()
        XCTAssertNil(model.sendConfigJSON(for: "no-such-account-\(UUID().uuidString)"))
    }
}
