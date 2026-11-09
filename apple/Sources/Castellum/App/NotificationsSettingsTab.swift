// Castellum - Settings > Notifications (S59, MM-9): new-mail banners, the
// Counters (Dock badge and menu bar counter, with per-account inclusion) and the VIP list.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Everything here is presentation-side preference (UserDefaults) except the VIP
// list, which the Rust core owns. Notification content never leaves this Mac.

import AppKit
import SwiftUI
import UserNotifications

/// Pure: whether a typed VIP address is acceptable to add (the core rejects
/// anything without an `@` too; this keeps the button honest).
enum VipAddressInput {
    static func normalised(_ text: String) -> String? {
        let t = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let at = t.firstIndex(of: "@"), at != t.startIndex,
              t.index(after: at) != t.endIndex, !t.contains(" ")
        else { return nil }
        return t
    }
}

struct NotificationsSettingsTab: View {
    @ObservedObject var prefs: Preferences
    @ObservedObject var model: AppModel
    @State private var authorisation: UNAuthorizationStatus = .notDetermined
    @State private var newVip = ""

    var body: some View {
        Form {
            Section {
                Picker("New mail", selection: $prefs.newMailNotifications) {
                    ForEach(NewMailNotifications.allCases) { Text($0.label).tag($0) }
                }
                Toggle("Show previews", isOn: $prefs.notificationsShowPreview)
                Toggle("Play sound", isOn: $prefs.notificationsPlaySound)
                if model.accounts.count > 1 {
                    ForEach(model.accounts) { account in
                        Toggle(account.email, isOn: includeBinding(account.id))
                    }
                }
                Text(authorisationLine).font(.caption).foregroundStyle(.secondary)
                if authorisation == .denied {
                    Button("Open System Settings…") { openSystemSettings() }
                }
            } header: {
                Text("Notifications")
            }

            Section {
                Picker("Dock badge", selection: $prefs.dockBadgeMode) {
                    ForEach(DockBadgeMode.allCases) { Text($0.label).tag($0) }
                }
                if prefs.dockBadgeMode == .smartMailbox {
                    smartMailboxPicker("Dock smart mailbox", selection: $prefs.dockBadgeSmartMailboxId)
                }
                Picker("Menu bar counter", selection: $prefs.menuBarCounterMode) {
                    ForEach(DockBadgeMode.allCases) { Text($0.label).tag($0) }
                }
                if prefs.menuBarCounterMode == .smartMailbox {
                    smartMailboxPicker("Menu bar smart mailbox", selection: $prefs.menuBarSmartMailboxId)
                }
                // Shown for one account too: an account left out must stay re-includable.
                if !model.accounts.isEmpty {
                    ForEach(model.accounts) { account in
                        Toggle(
                            String(localized: "Count \(account.email)"),
                            isOn: countsBinding(account.id))
                    }
                }
                Text(
                    "A smart mailbox shows the number in the sidebar and counts the accounts it covers. Account switches apply to Inbox and VIP counts."
                )
                .font(.caption).foregroundStyle(.secondary)
            } header: {
                Text("Counters")
            }

            Section {
                if model.vips.isEmpty {
                    Text("No VIPs yet. Select a message and press v to add its sender.")
                        .foregroundStyle(.secondary)
                }
                ForEach(model.vips) { vip in
                    HStack {
                        VStack(alignment: .leading) {
                            if let name = vip.displayName, !name.isEmpty { Text(name) }
                            Text(vip.address).font(.caption).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button("Remove") { model.setVip(address: vip.address, vip: false) }
                    }
                }
                HStack {
                    TextField("Add an address", text: $newVip)
                        .onSubmit(addVip)
                    Button("Add", action: addVip)
                        .disabled(VipAddressInput.normalised(newVip) == nil)
                }
            } header: {
                Text("VIPs")
            }

            Text(
                "Notifications stay on this Mac. Focus filters can limit them further: System Settings → Focus → Add Filter → Castellum."
            )
            .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .onAppear {
            model.reloadVips()
            Task { authorisation = await NotificationCentreFactory.make().authorisationStatus() }
        }
    }

    /// Lists the smart mailboxes by name. An unset or deleted choice shows as the nil
    /// row, which is what the counter really does then: count the inbox.
    private func smartMailboxPicker(_ title: LocalizedStringKey, selection: Binding<String?>) -> some View {
        let known = Binding<String?>(
            get: { selection.wrappedValue.flatMap { id in model.smartMailboxes.contains { $0.id == id } ? id : nil } },
            set: { selection.wrappedValue = $0 })
        return Picker(title, selection: known) {
            Text("Inbox (until chosen)").tag(String?.none)
            ForEach(model.smartMailboxes) { Text($0.name).tag(Optional($0.id)) }
        }
    }

    private func countsBinding(_ accountId: String) -> Binding<Bool> {
        Binding(
            get: { prefs.accountRule(accountId).countsInCounters },
            set: { prefs.setCountsInCounters($0, accountId: accountId) })
    }

    private func includeBinding(_ accountId: String) -> Binding<Bool> {
        Binding(
            get: { !prefs.notificationsExcludedAccountIds.contains(accountId) },
            set: { include in
                if include {
                    prefs.notificationsExcludedAccountIds.remove(accountId)
                } else {
                    prefs.notificationsExcludedAccountIds.insert(accountId)
                }
            })
    }

    private func addVip() {
        guard let address = VipAddressInput.normalised(newVip) else { return }
        model.setVip(address: address, vip: true)
        newVip = ""
    }

    private var authorisationLine: String {
        switch authorisation {
        case .denied: return String(localized: "Turned off in System Settings.")
        case .authorized, .provisional, .ephemeral: return String(localized: "Allowed in System Settings.")
        default: return String(localized: "Castellum asks for permission once you have added an account.")
        }
    }

    private func openSystemSettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
            NSWorkspace.shared.open(url)
        }
    }
}
