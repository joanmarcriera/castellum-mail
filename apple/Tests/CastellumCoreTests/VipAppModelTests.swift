// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// VIP senders through `AppModel` (S59): the `v` command's model call, the
/// list flag, the sidebar's `is:vip` view and the command registry entry.
@MainActor
final class VipAppModelTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-vip-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(_ condition: () -> Bool, _ message: String) async throws {
        let deadline = Date().addingTimeInterval(5)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        if !condition() { XCTFail(message) }
    }

    private func seed(_ model: AppModel) throws -> Mailbox {
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: "acc", email: "me@local.test",
            mailboxId: "acc:INBOX", role: "inbox")
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: "acc:INBOX:1", accountId: "acc",
            mailboxId: "acc:INBOX", subject: "Hello", fromAddress: "Boss@Local.Test",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        return Mailbox(
            id: "acc:INBOX", accountId: "acc", path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
    }

    func testTogglingVipAddsThenRemovesTheSenderAndUpdatesTheHeader() async throws {
        let model = makeModel()
        let inbox = try seed(model)
        model.select(mailbox: inbox)
        try await waitUntil({ !model.messages.isEmpty }, "list never loaded")
        XCTAssertFalse(model.messages[0].fromVip)
        model.selectedMessage = model.messages[0]

        model.toggleVipForSelectedSender()
        try await waitUntil({ model.vips.count == 1 }, "VIP never added")
        XCTAssertEqual(model.vips.first?.address, "boss@local.test")
        try await waitUntil({ model.messages.first?.fromVip == true }, "reloaded header not VIP")
        try await waitUntil({ model.vipUnreadCount == 1 }, "VIP unread count never refreshed")

        // The selection still holds the stale (non-VIP) header copy; the toggle
        // must decide from the VIP list and remove.
        model.toggleVipForSelectedSender()
        try await waitUntil({ model.vips.isEmpty }, "VIP never removed")
        try await waitUntil({ model.messages.first?.fromVip == false }, "header still VIP")
    }

    func testVipsViewIsTheUnifiedInboxFilteredByIsVip() async throws {
        let model = makeModel()
        let inbox = try seed(model)
        model.select(mailbox: inbox)
        try await waitUntil({ !model.messages.isEmpty }, "list never loaded")
        XCTAssertFalse(model.isVipViewSelected)
        model.setVip(address: "boss@local.test", vip: true)
        try await waitUntil({ model.vips.count == 1 }, "VIP never added")

        model.selectVips()
        XCTAssertTrue(model.isVipViewSelected)
        try await waitUntil({ model.messages.count == 1 && model.messages[0].fromVip }, "VIP search empty")
    }

    func testTheVipCommandExistsWithoutKeyCollisions() {
        let spec = CommandRegistry.spec(for: .toggleVipSender)
        XCTAssertEqual(spec.bareKey, "v")
        let bare = CommandRegistry.all.compactMap(\.bareKey)
        XCTAssertEqual(bare.count, Set(bare).count, "duplicate bare key")
        let names = CommandRegistry.all.compactMap(\.notification)
        XCTAssertEqual(names.count, Set(names).count, "duplicate notification name")
    }

    func testVipAddressInputValidation() {
        XCTAssertEqual(VipAddressInput.normalised(" a@b.co "), "a@b.co")
        XCTAssertNil(VipAddressInput.normalised("nobody"))
        XCTAssertNil(VipAddressInput.normalised("@b.co"))
        XCTAssertNil(VipAddressInput.normalised("a@"))
        XCTAssertNil(VipAddressInput.normalised("a b@c.d"))
    }
}
