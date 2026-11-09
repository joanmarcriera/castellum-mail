// Castellum - inline calendar invite card (S55): Accept/Decline/Tentative +
// Add to Calendar, one click, entirely on-device.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Parses synchronously — the invite is already parsed and persisted by the
// core at hydration time (`MessageDetail.calendarInvite`), so showing this
// card is a plain read with no spinner and no network round trip.
//
// EventKit access is requested only when "Add to Calendar" is first tapped
// (never at launch, never silently) — see EventKitInviteSource. A denial
// degrades ONLY that affordance to a read-only explanation + Settings deep
// link; Accept/Decline/Tentative are plain outgoing replies (queued and
// sent through the normal SMTP outbox, exactly like any other reply) and
// never depend on Calendar permission — gating an email reply on an
// unrelated OS permission would itself be a way of "blocking the rest of
// the message", which the design brief rules out. Calendar access is
// therefore requested independently, the first time IT specifically is
// needed, not piggybacked on Accept/Decline.
//
// Never logs or exports invite contents, attendee addresses, or `.ics` text
// — every value below is shown in the UI and nowhere else.

import AppKit
import SwiftUI

/// Classifies the core's "recipient mismatch: <address>" refusal (stable
/// prefix `RECIPIENT_MISMATCH_PREFIX` in castellum-core, pinned by its tests).
enum CalendarInviteRecipientPolicy {
    static let prefix = "recipient mismatch: "

    /// The organizer address the user must confirm, or nil for any other error.
    static func mismatchAddress(_ error: Error) -> String? {
        guard case .message(let reason)? = error as? CastellumError,
              reason.hasPrefix(prefix) else { return nil }
        return String(reason.dropFirst(prefix.count))
    }

    /// "Name <email>" so a display name can never hide the real address.
    static func organizerText(name: String?, email: String) -> String {
        guard let name, !name.isEmpty, name != email else { return email }
        return "\(name) <\(email)>"
    }
}

struct CalendarInviteCardView: View {
    let invite: CalendarInvite
    let messageId: String
    let accountId: String
    @ObservedObject var model: AppModel

    @State private var eventKitStatus = EventKitInviteSource.shared.authorizationStatus
    @State private var addedToCalendar = false
    @State private var calendarActionFailed = false

    private var isSending: Bool {
        if case .sending = model.calendarInviteResponses[messageId] { return true }
        return false
    }

    private var pendingConfirmation: (response: String, address: String)? {
        if case .needsRecipientConfirmation(let response, let address)
            = model.calendarInviteResponses[messageId] { return (response, address) }
        return nil
    }

    private var sendFailure: String? {
        if case .failed(let reason) = model.calendarInviteResponses[messageId] { return reason }
        return nil
    }

    private var timeRange: String? {
        guard let start = invite.dtstart?.resolvedDate() else { return nil }
        let formatter = DateFormatter()
        if invite.dtstart?.isDateOnly == true {
            formatter.dateStyle = .full
            formatter.timeStyle = .none
            return formatter.string(from: start)
        }
        formatter.dateStyle = .full
        formatter.timeStyle = .short
        var text = formatter.string(from: start)
        if let end = invite.dtend?.resolvedDate() {
            let timeOnly = DateFormatter()
            timeOnly.timeStyle = .short
            text += " – \(timeOnly.string(from: end))"
        }
        if invite.dtstart?.hasUnrecognisedTimeZone == true {
            // Don't present a guessed time as certain.
            text += " " + String(localized: "(time zone not recognised, shown in your time zone)")
        }
        return text
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Image(systemName: "calendar")
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(invite.summary ?? String(localized: "(no title)"))
                        .font(.subheadline).bold()
                    if let timeRange {
                        Text(timeRange).font(.caption).foregroundStyle(.secondary)
                    }
                    if let location = invite.location {
                        Label(location, systemImage: "mappin.and.ellipse")
                            .font(.caption).foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    if let organizer = organizerLine {
                        Text(organizer).font(.caption2).foregroundStyle(.secondary)
                    }
                    if !invite.attendees.isEmpty {
                        Text(attendeesLine).font(.caption2).foregroundStyle(.secondary)
                    }
                }
                Spacer()
            }

            if let description = invite.description {
                Text(description)
                    .font(.caption).foregroundStyle(.secondary)
                    .lineLimit(6)
                    .textSelection(.enabled)
            }

            if let advisory = invite.possiblySuperseded {
                Label(advisory == "cancelled"
                      ? String(localized: "Possibly cancelled by a later message, but the sender could not be verified.")
                      : String(localized: "Possibly updated by a later message, but the sender could not be verified."),
                      systemImage: "exclamationmark.triangle")
                    .font(.caption).foregroundStyle(.orange)
            }

            if let responded = invite.ourResponse {
                Label(statusText(for: responded), systemImage: statusIcon(for: responded))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if let pending = pendingConfirmation {
                VStack(alignment: .leading, spacing: 4) {
                    Text("This reply will be sent to \(pending.address), which is not the sender of this message.")
                        .font(.caption2)
                        .foregroundStyle(.orange)
                    HStack {
                        Button(String(localized: "Send reply to \(pending.address)")) {
                            model.respondToCalendarInvite(
                                messageId: messageId, accountId: accountId,
                                response: pending.response,
                                // Bound to the address named on this button:
                                // the core refuses if it would send elsewhere.
                                confirmedRecipient: pending.address)
                        }
                        .controlSize(.small)
                        Button(String(localized: "Cancel")) {
                            model.calendarInviteResponses[messageId] = nil
                        }
                        .controlSize(.small)
                    }
                }
            }

            if let sendFailure {
                Text(sendFailure)
                    .font(.caption2)
                    .foregroundStyle(.red)
            }

            HStack(spacing: 8) {
                // RFC 5546: only METHOD:REQUEST is an invitation that expects
                // a reply. A CANCEL says the meeting is off, a REPLY is
                // someone else's answer, and PUBLISH is just a calendar
                // attachment — offering Accept/Decline on any of them invites
                // the user to "accept" a cancellation and sends a reply
                // nobody asked for. An unrecognised or absent METHOD is
                // treated as not-an-invitation, which is the safe direction.
                if isReplyableInvitation {
                    responseButton(String(localized: "Accept"), response: "accepted")
                    responseButton(String(localized: "Decline"), response: "declined")
                    responseButton(String(localized: "Maybe"), response: "tentative")
                } else if let note = nonInvitationNote {
                    Text(note).font(.caption).foregroundStyle(.secondary)
                }
                Spacer()
                if invite.offersAddToCalendar { addToCalendarControl }
            }
        }
        .padding(8)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .controlBackgroundColor)))
        .overlay(RoundedRectangle(cornerRadius: 8).stroke(Color(nsColor: .separatorColor)))
        // The authorisation status is read once when the view is built, so a
        // grant or revoke made in System Settings while this message is open
        // would otherwise leave the card showing "Calendar access denied"
        // (or an Add button that cannot work) until the selection changes.
        // Re-reading on appear and on re-activation costs nothing: it is a
        // synchronous EventKit class-property read, not a prompt.
        .onAppear { eventKitStatus = EventKitInviteSource.shared.authorizationStatus }
        .onReceive(NotificationCenter.default.publisher(
            for: NSApplication.didBecomeActiveNotification)) { _ in
            eventKitStatus = EventKitInviteSource.shared.authorizationStatus
        }
    }

    /// Whether this part is an invitation that expects a reply at all.
    /// Compared case-insensitively: METHOD is a token, and senders vary.
    /// The core defaults a missing METHOD to PUBLISH, so an ICS with no
    /// method lands here as not-an-invitation — the safe direction.
    private var isReplyableInvitation: Bool {
        invite.method.uppercased() == "REQUEST" && invite.superseded == nil
    }

    /// What to say instead of reply buttons, when there is something useful
    /// to say. `nil` keeps the card quiet rather than explaining a PUBLISH
    /// attachment nobody was asking about.
    private var nonInvitationNote: String? {
        // A newer invite for the same meeting is in the mailbox: this one must not be answered or
        // added (#3446).
        switch invite.superseded {
        case "cancelled": return String(localized: "This meeting was cancelled.")
        case "updated": return String(localized: "This invitation was updated: use the newer message.")
        default: break
        }
        switch invite.method.uppercased() {
        case "CANCEL": return String(localized: "This meeting was cancelled.")
        case "REPLY": return String(localized: "This is a reply to an invitation.")
        default: return nil
        }
    }

    private var organizerLine: String? {
        guard let email = invite.organizerEmail else { return nil }
        let who = CalendarInviteRecipientPolicy.organizerText(name: invite.organizerName, email: email)
        return String(localized: "Organizer: \(who)")
    }

    private var attendeesLine: String {
        let names = invite.attendees.map { $0.name ?? $0.email }
        let joined = names.joined(separator: ", ")
        return names.count == 1
            ? String(localized: "Attendee: \(joined)")
            : String(localized: "Attendees: \(joined)")
    }

    private func statusText(for response: String) -> String {
        switch response {
        case "declined": return String(localized: "You declined this invitation")
        case "tentative": return String(localized: "You tentatively accepted this invitation")
        default: return String(localized: "You accepted this invitation")
        }
    }

    private func statusIcon(for response: String) -> String {
        switch response {
        case "declined": return "xmark.circle"
        case "tentative": return "questionmark.circle"
        default: return "checkmark.circle"
        }
    }

    @ViewBuilder
    private func responseButton(_ title: String, response: String) -> some View {
        let isCurrent = invite.ourResponse == response
        let action = {
            model.respondToCalendarInvite(messageId: messageId, accountId: accountId, response: response)
        }
        // Two branches (not a ternary feeding one `.buttonStyle`) because
        // `.borderedProminent`/`.bordered` resolve to different concrete
        // `PrimitiveButtonStyle` types — a ternary between them doesn't
        // type-check against the modifier's generic parameter.
        if isCurrent {
            Button(title, action: action)
                .disabled(isSending)
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
        } else {
            Button(title, action: action)
                .disabled(isSending)
                .buttonStyle(.bordered)
                .controlSize(.small)
        }
    }

    /// "Add to Calendar" when access is granted or undetermined (tapping it
    /// asks); a plain read-only explanation + Settings link once denied —
    /// never a crash, never disabling the rest of the card.
    @ViewBuilder
    private var addToCalendarControl: some View {
        switch eventKitStatus {
        case .denied, .restricted:
            HStack(spacing: 4) {
                Text("Calendar access denied").font(.caption2).foregroundStyle(.secondary)
                Button(String(localized: "Open Settings")) {
                    EventKitInviteSource.openPrivacySettings()
                }
                .buttonStyle(.link)
                .font(.caption2)
            }
        default:
            if addedToCalendar {
                Label(String(localized: "Added"), systemImage: "checkmark")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            } else {
                VStack(alignment: .trailing, spacing: 2) {
                    Button(String(localized: "Add to Calendar")) { addToCalendar() }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                    if calendarActionFailed {
                        Text("Couldn't add to Calendar").font(.caption2).foregroundStyle(.red)
                    }
                }
            }
        }
    }

    private func addToCalendar() {
        calendarActionFailed = false
        EventKitInviteSource.shared.requestAccessIfNeeded { granted in
            eventKitStatus = EventKitInviteSource.shared.authorizationStatus
            guard granted else { return }
            let added = EventKitInviteSource.shared.addEvent(invite: invite)
            if added {
                addedToCalendar = true
            } else {
                calendarActionFailed = true
            }
        }
    }
}
