// Castellum - Send Later presets (HP1): pure date arithmetic, unit-testable
// without the FFI, the system clock or the process time zone. Every preset
// resolves in the user's LOCAL calendar/time zone and the result is stored as
// an absolute instant (UTC epoch seconds) in the durable outbox.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum SendLaterPreset: Hashable {
    /// Now + 1 hour of elapsed time (an absolute interval, so it is exact even
    /// across a daylight-saving change).
    case inOneHour
    /// 18:00 local today; once 18:00 has passed, 18:00 tomorrow.
    case thisEvening
    /// 08:00 local tomorrow.
    case tomorrowMorning
    /// 08:00 local on the next Monday strictly after today.
    case mondayMorning
    case custom(Date)

    static let standardCases: [SendLaterPreset] = [
        .inOneHour, .thisEvening, .tomorrowMorning, .mondayMorning,
    ]

    /// Menu/button label (localised; British English in the catalogue).
    var title: String {
        switch self {
        case .inOneHour: return String(localized: "In one hour")
        case .thisEvening: return String(localized: "This evening, 18:00")
        case .tomorrowMorning: return String(localized: "Tomorrow morning, 08:00")
        case .mondayMorning: return String(localized: "Monday morning, 08:00")
        case .custom: return String(localized: "Custom…")
        }
    }

    /// The concrete send time. `calendar` and `now` are injected so tests can
    /// pin the time zone (and so a daylight-saving boundary) and the clock.
    func sendDate(calendar: Calendar = .current, now: Date = Date()) -> Date {
        switch self {
        case .custom(let date):
            return date
        case .inOneHour:
            return now.addingTimeInterval(3600)
        case .thisEvening:
            let today = Self.at(hour: 18, onDayOf: now, calendar: calendar)
            if today > now { return today }
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: now) ?? now.addingTimeInterval(86_400)
            return Self.at(hour: 18, onDayOf: tomorrow, calendar: calendar)
        case .tomorrowMorning:
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: now) ?? now.addingTimeInterval(86_400)
            return Self.at(hour: 8, onDayOf: tomorrow, calendar: calendar)
        case .mondayMorning:
            var components = DateComponents()
            components.weekday = 2 // Monday
            components.hour = 8
            components.minute = 0
            components.second = 0
            // Start from the beginning of tomorrow so "today is Monday" gives
            // NEXT Monday, never later today.
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: calendar.startOfDay(for: now))
                ?? now.addingTimeInterval(86_400)
            return calendar.nextDate(
                after: tomorrow.addingTimeInterval(-1), matching: components,
                matchingPolicy: .nextTime, direction: .forward)
                ?? now.addingTimeInterval(7 * 86_400)
        }
    }

    /// `hour`:00 local on the calendar day containing `day`. `.nextTime`
    /// moves a wall-clock time that does not exist (spring-forward gap) to the
    /// next instant that does, instead of failing.
    private static func at(hour: Int, onDayOf day: Date, calendar: Calendar) -> Date {
        let start = calendar.startOfDay(for: day)
        return calendar.nextDate(
            after: start.addingTimeInterval(-1),
            matching: DateComponents(hour: hour, minute: 0, second: 0),
            matchingPolicy: .nextTime, direction: .forward) ?? start.addingTimeInterval(Double(hour) * 3600)
    }
}

/// The outbox/compose wording for a scheduled instant, in the user's locale
/// and time zone, e.g. "Mon 5 Oct, 08:00".
func sendLaterDisplay(_ epoch: Int64, locale: Locale = .current, timeZone: TimeZone = .current) -> String {
    let formatter = DateFormatter()
    formatter.locale = locale
    formatter.timeZone = timeZone
    formatter.setLocalizedDateFormatFromTemplate("EEE d MMM HH:mm")
    return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
}
