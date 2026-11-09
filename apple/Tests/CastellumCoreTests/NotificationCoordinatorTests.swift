// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// The outbox drain, banner content and banner actions (S59). Every centre is a
/// `FakeNotificationCentre`, so no system permission prompt or banner can
/// appear; profiles are temp directories with the in-memory Keychain.
@MainActor
final class NotificationCoordinatorTests: XCTestCase {
    private var models: [AppModel] = []
    private var suites: [String] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-notify-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    /// Isolated preferences + defaults so tests never touch the real ones.
    private func makeDefaults() -> UserDefaults {
        let name = "castellum-notify-tests-\(UUID().uuidString)"
        suites.append(name)
        return UserDefaults(suiteName: name)!
    }

    override func tearDown() async throws {
        for model in models { try model.accountStore.deleteAll() }
        models = []
        for s in suites { UserDefaults(suiteName: s)?.removePersistentDomain(forName: s) }
        suites = []
        try await super.tearDown()
    }

    /// One inbox message from a named sender, queued in the outbox.
    private func seed(_ model: AppModel, accountId: String = "acc", id: String = "acc:INBOX:1") throws {
        try DirectFixture.seedAccountAndMailbox(
            profileDirectory: model.profileDirectory, accountId: accountId,
            email: "me@local.test", mailboxId: "\(accountId):INBOX", role: "inbox")
        try DirectFixture.seedMessage(
            profileDirectory: model.profileDirectory, id: id, accountId: accountId,
            mailboxId: "\(accountId):INBOX", subject: "Lunch?", fromAddress: "ana@example.com",
            listUnsubscribeMailto: nil, listUnsubscribeUrl: nil, listUnsubscribeOneClick: false)
        try DirectFixture.seedOutbox(
            profileDirectory: model.profileDirectory, messageId: id, accountId: accountId)
    }

    private func makeCoordinator(
        _ model: AppModel, centre: FakeNotificationCentre, prefs: Preferences
    ) -> NotificationCoordinator {
        NotificationCoordinator(
            model: model, centre: centre, focusState: { .inactive }, preferences: prefs,
            defaults: makeDefaults())
    }

    func testDrainPostsOneBannerWithTheExpectedContent() async throws {
        let model = makeModel()
        try seed(model)
        let centre = FakeNotificationCentre()
        let prefs = Preferences(defaults: makeDefaults())
        let coordinator = makeCoordinator(model, centre: centre, prefs: prefs)

        await coordinator.drain()

        XCTAssertEqual(centre.posted.count, 1)
        let p = try XCTUnwrap(centre.posted.first)
        XCTAssertEqual(p.id, "msg:acc:INBOX:1")
        XCTAssertEqual(p.title, "ana@example.com")  // no display name in the fixture
        XCTAssertEqual(p.subtitle, "Lunch?")
        XCTAssertEqual(p.body, "Lunch?")  // the fixture's snippet is the subject
        XCTAssertEqual(p.userInfo["messageId"], "acc:INBOX:1")
        XCTAssertEqual(p.userInfo["accountId"], "acc")
        XCTAssertEqual(p.userInfo["mailboxId"], "acc:INBOX")
        XCTAssertTrue(p.sound)
        // At-most-once: a second drain finds nothing.
        await coordinator.drain()
        XCTAssertEqual(centre.posted.count, 1)
    }

    func testPreviewOffLeavesTheBodyEmpty() async throws {
        let model = makeModel()
        try seed(model)
        let centre = FakeNotificationCentre()
        let prefs = Preferences(defaults: makeDefaults())
        prefs.notificationsShowPreview = false
        let coordinator = makeCoordinator(model, centre: centre, prefs: prefs)
        await coordinator.drain()
        XCTAssertNil(centre.posted.first?.body)
    }

    func testOffModePostsNothingButStillConsumesTheOutbox() async throws {
        let model = makeModel()
        try seed(model)
        let centre = FakeNotificationCentre()
        let prefs = Preferences(defaults: makeDefaults())
        prefs.newMailNotifications = .off
        let coordinator = makeCoordinator(model, centre: centre, prefs: prefs)
        await coordinator.drain()
        XCTAssertTrue(centre.posted.isEmpty)
        // Switching back on must not replay the old mail.
        prefs.newMailNotifications = .all
        await coordinator.drain()
        XCTAssertTrue(centre.posted.isEmpty)
        XCTAssertEqual(try model.core.takePendingNotifications(limit: 50), [])
    }

    func testDefaultActionOpensTheMessage() async throws {
        let model = makeModel()
        try seed(model)
        let coordinator = makeCoordinator(
            model, centre: FakeNotificationCentre(), prefs: Preferences(defaults: makeDefaults()))
        coordinator.handleOpen(userInfo: ["messageId": "acc:INBOX:1"])
        let deadline = Date().addingTimeInterval(5)
        while model.selectedMessage?.id != "acc:INBOX:1" && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertEqual(model.selectedMessage?.id, "acc:INBOX:1")
    }

    /// Audit #3450: clicking a "N new messages" summary banner used to do nothing.
    func testSummaryClickOpensTheAccountsInboxOrAllInboxes() async throws {
        let model = makeModel()
        try seed(model)
        let coordinator = makeCoordinator(
            model, centre: FakeNotificationCentre(), prefs: Preferences(defaults: makeDefaults()))
        coordinator.handleOpen(userInfo: ["accountId": "acc"])
        XCTAssertTrue(model.selectedMailbox?.id == "acc:INBOX" || model.selectedUnifiedRole == "inbox",
                      "a summary click must select an inbox")
        // An unknown (removed) account falls back to All Inboxes without crashing.
        model.select(mailbox: Mailbox(id: "x:Sent", accountId: "x", path: "Sent", name: "Sent", role: "sent",
                                      delimiter: "/", subscribed: true, selectable: true, syncByDefault: true))
        coordinator.handleOpen(userInfo: ["accountId": "gone"])
        XCTAssertEqual(model.selectedUnifiedRole, "inbox")
    }

    func testMarkAsReadActionFlipsTheReadStateWithoutAnError() async throws {
        let model = makeModel()
        try seed(model)
        XCTAssertEqual(try model.core.getMessage(id: "acc:INBOX:1")?.header.isRead, false)
        let coordinator = makeCoordinator(
            model, centre: FakeNotificationCentre(), prefs: Preferences(defaults: makeDefaults()))
        coordinator.handleMarkRead(userInfo: ["messageId": "acc:INBOX:1", "accountId": "acc"])
        let deadline = Date().addingTimeInterval(5)
        while (try model.core.getMessage(id: "acc:INBOX:1")?.header.isRead) != true && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertEqual(try model.core.getMessage(id: "acc:INBOX:1")?.header.isRead, true)
        XCTAssertNil(model.errorText, "the automatic path never raises the evaluation-mode refusal")
    }

    func testAuthorisationIsRequestedOnceAndOnlyWithAnAccount() async throws {
        let model = makeModel()
        let centre = FakeNotificationCentre()
        let defaults = makeDefaults()
        let prefs = Preferences(defaults: makeDefaults())
        let coordinator = NotificationCoordinator(
            model: model, centre: centre, focusState: { .inactive }, preferences: prefs,
            defaults: defaults)
        // No account yet: never asks at a blank first launch.
        await coordinator.ensureAuthorisation()
        XCTAssertEqual(centre.authorisationRequests, 0)
        model.accounts = [Account(id: "acc", email: "me@local.test", displayName: nil, provider: "imap")]
        await coordinator.ensureAuthorisation()
        await coordinator.ensureAuthorisation()
        XCTAssertEqual(centre.authorisationRequests, 1)
    }

    func testNoBannerWhenThatMailboxIsAlreadyOpenInTheFrontApp() throws {
        let model = makeModel()
        let coordinator = makeCoordinator(
            model, centre: FakeNotificationCentre(), prefs: Preferences(defaults: makeDefaults()))
        model.selectedMailbox = Mailbox(
            id: "acc:INBOX", accountId: "acc", path: "INBOX", name: "INBOX", role: "inbox",
            delimiter: "/", subscribed: true, selectable: true, syncByDefault: true)
        XCTAssertFalse(coordinator.shouldPresent(userInfo: ["mailboxId": "acc:INBOX"], appIsActive: true))
        XCTAssertTrue(coordinator.shouldPresent(userInfo: ["mailboxId": "acc:INBOX"], appIsActive: false))
        XCTAssertTrue(coordinator.shouldPresent(userInfo: ["mailboxId": "other:INBOX"], appIsActive: true))
    }

    func testTheFactoryHandsOutTheFakeUnderXCTest() {
        XCTAssertTrue(TestHost.isActive)
        XCTAssertTrue(NotificationCentreFactory.make() is FakeNotificationCentre)
    }

    /// #3040: an IDLE resync that rewrote rows with no arrival must reload the list.
    func testIdlePassEpochMovementTriggersAListReload() throws {
        XCTAssertFalse(NotificationCoordinator.shouldReloadForIdlePass(last: 0, current: 0))
        XCTAssertTrue(NotificationCoordinator.shouldReloadForIdlePass(last: 0, current: 1))
        XCTAssertTrue(NotificationCoordinator.shouldReloadForIdlePass(last: 3, current: 4))
        // The FFI counter is readable and starts at zero on a fresh profile.
        let model = makeModel()
        XCTAssertEqual(try model.core.idlePassEpoch(), 0)
    }
}
