// Castellum - the slice of UNUserNotificationCenter the app uses (S59).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Production wraps the real centre. XCTest (`TestHost.isActive`) always gets
// `FakeNotificationCentre`, so a test run can never raise the system
// permission prompt or post a banner (same guard as `InMemoryKeychainBacking`).

import Foundation
import UserNotifications

protocol UserNotificationCentre: AnyObject {
    func requestAuthorisation() async -> Bool
    func authorisationStatus() async -> UNAuthorizationStatus
    func post(
        id: String, title: String, subtitle: String?, body: String?,
        threadId: String?, userInfo: [String: String], sound: Bool
    ) async throws
    func registerCategories()
}

enum NotificationCentreFactory {
    static func make() -> UserNotificationCentre {
        TestHost.suppressesLaunchSideEffects ? FakeNotificationCentre() : SystemNotificationCentre()
    }
}

/// Identifiers shared by the system centre and the coordinator's delegate.
enum NotificationIds {
    static let newMailCategory = "castellum.newMail"
    static let markReadAction = "castellum.markRead"
}

/// The real thing. Only ever constructed outside XCTest.
final class SystemNotificationCentre: UserNotificationCentre {
    private var centre: UNUserNotificationCenter { UNUserNotificationCenter.current() }

    func requestAuthorisation() async -> Bool {
        (try? await centre.requestAuthorization(options: [.alert, .sound, .badge])) ?? false
    }

    func authorisationStatus() async -> UNAuthorizationStatus {
        await centre.notificationSettings().authorizationStatus
    }

    func post(
        id: String, title: String, subtitle: String?, body: String?,
        threadId: String?, userInfo: [String: String], sound: Bool
    ) async throws {
        let content = UNMutableNotificationContent()
        content.title = title
        if let subtitle { content.subtitle = subtitle }
        if let body { content.body = body }
        if let threadId { content.threadIdentifier = threadId }
        content.userInfo = userInfo
        content.categoryIdentifier = NotificationIds.newMailCategory
        if sound { content.sound = .default }
        try await centre.add(UNNotificationRequest(identifier: id, content: content, trigger: nil))
    }

    func registerCategories() {
        let markRead = UNNotificationAction(
            identifier: NotificationIds.markReadAction, title: "Mark as Read", options: [])
        let category = UNNotificationCategory(
            identifier: NotificationIds.newMailCategory, actions: [markRead],
            intentIdentifiers: [], options: [])
        centre.setNotificationCategories([category])
    }
}

/// In-memory stand-in: records what would have been posted.
final class FakeNotificationCentre: UserNotificationCentre {
    struct Posted: Equatable {
        let id: String
        let title: String
        let subtitle: String?
        let body: String?
        let threadId: String?
        let userInfo: [String: String]
        let sound: Bool
    }

    private(set) var posted: [Posted] = []
    private(set) var authorisationRequests = 0
    private(set) var categoriesRegistered = false
    /// Scriptable: what `requestAuthorisation()` answers.
    var grantAuthorisation = true
    var status: UNAuthorizationStatus = .notDetermined

    func requestAuthorisation() async -> Bool {
        authorisationRequests += 1
        status = grantAuthorisation ? .authorized : .denied
        return grantAuthorisation
    }

    func authorisationStatus() async -> UNAuthorizationStatus { status }

    func post(
        id: String, title: String, subtitle: String?, body: String?,
        threadId: String?, userInfo: [String: String], sound: Bool
    ) async throws {
        posted.append(
            Posted(
                id: id, title: title, subtitle: subtitle, body: body, threadId: threadId,
                userInfo: userInfo, sound: sound))
    }

    func registerCategories() { categoriesRegistered = true }
}
