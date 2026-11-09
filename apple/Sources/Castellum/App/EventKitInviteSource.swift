// Castellum - macOS EventKit bridge for calendar invite actions (S55).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// On-demand permission, mirroring SystemContactsSource's shape exactly:
// access is requested only when the user first taps an invite-card action
// (Accept/Decline/Tentative/Add to Calendar) — never at launch, never
// silently. `.denied`/`.restricted` degrades the card to read-only (see
// CalendarInviteCardView), never a crash and never a block on the rest of
// the message: Accept/Decline still send the reply through
// AppModel.respondToCalendarInvite regardless of EventKit access, since
// only "Add to Calendar" itself actually touches the local calendar.
//
// Nothing read from EventKit is ever logged or exported — this bridge only
// WRITES one event per "Add to Calendar" tap and reads nothing back beyond
// the default-calendar handle needed to do that.

import AppKit
import EventKit

@MainActor
final class EventKitInviteSource {
    static let shared = EventKitInviteSource()

    private let store = EKEventStore()

    /// Current authorization state, read fresh every call — the user can
    /// change it in System Settings > Privacy at any time.
    var authorizationStatus: EKAuthorizationStatus {
        EKEventStore.authorizationStatus(for: .event)
    }

    /// True once the user has granted at least write access to events.
    var isAuthorized: Bool {
        switch authorizationStatus {
        case .authorized:
            return true
        default:
            if #available(macOS 14.0, *) {
                return authorizationStatus == .fullAccess || authorizationStatus == .writeOnly
            }
            return false
        }
    }

    /// Explicitly refused (vs. simply never asked) — the card's read-only/
    /// explain state vs. "haven't asked yet".
    var isDenied: Bool {
        switch authorizationStatus {
        case .denied, .restricted:
            return true
        default:
            return false
        }
    }

    /// Requests access if (and only if) it has never been decided, then
    /// calls back on the main actor with whether we now have write access.
    /// Safe to call repeatedly — macOS never re-prompts once the user has
    /// decided, and this never asks again itself either.
    func requestAccessIfNeeded(completion: @escaping (Bool) -> Void) {
        if TestHost.isUIScript { completion(false); return }   // UIT-1: never raise a Calendar prompt
        guard authorizationStatus == .notDetermined else {
            completion(isAuthorized)
            return
        }
        if #available(macOS 14.0, *) {
            store.requestWriteOnlyAccessToEvents { [weak self] granted, _ in
                Task { @MainActor in completion(granted && (self?.isAuthorized ?? granted)) }
            }
        } else {
            store.requestAccess(to: .event) { granted, _ in
                Task { @MainActor in completion(granted) }
            }
        }
    }

    /// Create a calendar event for `invite` in the user's default calendar.
    /// Never throws — a failure here must never block the reply email
    /// (which, for Accept/Decline/Tentative, is queued independently
    /// through the normal outbox regardless of this call's outcome).
    ///
    /// Simplification: always creates a NEW event, rather than trying to
    /// find and update one from an earlier "Add to Calendar" on the same
    /// invite — EventKit has no UID-keyed lookup API, and de-duplicating
    /// across an unreliable match is worse than an occasional duplicate the
    /// user can merge/delete in Calendar.app. Documented here rather than
    /// silently accepted.
    @discardableResult
    func addEvent(invite: CalendarInvite) -> Bool {
        guard isAuthorized, let calendar = store.defaultCalendarForNewEvents else { return false }
        let event = EKEvent(eventStore: store)
        event.calendar = calendar
        event.title = invite.summary ?? String(localized: "(no title)")
        event.notes = organizerNote(invite: invite)
        let dates = Self.eventDates(for: invite)
        event.isAllDay = dates.isAllDay
        event.startDate = dates.start
        event.endDate = dates.end
        do {
            try store.save(event, span: .thisEvent)
            return true
        } catch {
            return false
        }
    }

    /// Start/end/all-day for the EventKit event. iCalendar's all-day DTEND is
    /// EXCLUSIVE (`20260925`..`20260926` is one day) while EventKit treats an
    /// all-day `endDate` as the last day covered, so the end is moved back one
    /// calendar day (DST-safe via `Calendar`), clamped to the start for
    /// producers that emit DTEND == DTSTART; no DTEND means a one-day event.
    /// Timed events pass through (1 h default). Pure, so XCTest can cover it
    /// (`addEvent` itself needs Calendar authorisation).
    nonisolated static func eventDates(
        for invite: CalendarInvite,
        calendar: Calendar = .current,
        now: Date = Date()
    ) -> (start: Date, end: Date, isAllDay: Bool) {
        let isAllDay = invite.dtstart?.isDateOnly ?? false
        let start = invite.dtstart?.resolvedDate(calendar: calendar) ?? now
        let rawEnd = invite.dtend?.resolvedDate(calendar: calendar)
        guard isAllDay else {
            return (start, rawEnd ?? start.addingTimeInterval(3600), false)
        }
        guard let rawEnd, let inclusive = calendar.date(byAdding: .day, value: -1, to: rawEnd) else {
            return (start, start, true)
        }
        return (start, max(start, inclusive), true)
    }

    private func organizerNote(invite: CalendarInvite) -> String? {
        guard let email = invite.organizerEmail else { return nil }
        let name = invite.organizerName ?? email
        return String(localized: "Organizer: \(name) <\(email)>")
    }

    /// Deep link to the Calendars privacy pane, for the read-only card's
    /// "Open System Settings" action after a denial.
    static func openPrivacySettings() {
        guard let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Calendars") else {
            return
        }
        NSWorkspace.shared.open(url)
    }
}
