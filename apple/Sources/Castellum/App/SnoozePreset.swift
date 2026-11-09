// Castellum - Snooze preset intervals (S43): pure date math, unit-testable
// without touching the FFI. Labels are British English throughout, per
// CommandSpec's own convention (CommandRegistry.swift:82).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum SnoozePreset: Hashable {
    case laterToday
    case tomorrow
    case thisWeekend
    case nextWeek
    case custom(Date)

    static let standardCases: [SnoozePreset] = [.laterToday, .tomorrow, .thisWeekend, .nextWeek]

    /// Menu/button label.
    var title: String {
        switch self {
        case .laterToday: return "Later today"
        case .tomorrow: return "Tomorrow"
        case .thisWeekend: return "This weekend"
        case .nextWeek: return "Next week"
        case .custom: return "Custom…"
        }
    }

    /// The concrete due date for this preset. `calendar`/`now` default to the
    /// system values so call sites don't pass them, but a test can pin both
    /// without touching the system clock or the process's time zone.
    func dueDate(calendar: Calendar = .current, now: Date = Date()) -> Date {
        switch self {
        case .custom(let date):
            return date
        case .laterToday:
            let today6pm = calendar.date(bySettingHour: 18, minute: 0, second: 0, of: now) ?? now
            let candidate = today6pm > now ? today6pm : now.addingTimeInterval(3 * 3600)
            // Past ~21:00 "now + 3 h" lands after midnight and would wake the
            // message in the small hours; that is not "later today" (audit
            // #3453), so roll over to tomorrow morning instead.
            if !calendar.isDate(candidate, inSameDayAs: now) {
                return SnoozePreset.tomorrow.dueDate(calendar: calendar, now: now)
            }
            return candidate
        case .tomorrow:
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: now) ?? now
            return calendar.date(bySettingHour: 8, minute: 0, second: 0, of: tomorrow) ?? tomorrow
        case .thisWeekend:
            // On Saturday after 09:00 the rest of the weekend is Sunday 09:00;
            // on Sunday before 09:00 it is today 09:00. Only once Sunday 09:00
            // has passed does the preset mean the NEXT Saturday (audit #3453).
            let weekday = calendar.component(.weekday, from: now)
            let today9 = calendar.date(bySettingHour: 9, minute: 0, second: 0, of: now) ?? now
            if weekday == 7 || weekday == 1 {
                if today9 > now { return today9 }
                if weekday == 7 {
                    return calendar.date(byAdding: .day, value: 1, to: today9) ?? today9.addingTimeInterval(86_400)
                }
            }
            return Self.nextWeekday(7 /* Saturday */, hour: 9, calendar: calendar, now: now)
        case .nextWeek:
            // Strictly after today: on a Monday morning "Next week" is next
            // Monday, not 08:00 an hour from now (mirrors
            // SendLaterPreset.mondayMorning).
            let tomorrow = calendar.date(byAdding: .day, value: 1, to: calendar.startOfDay(for: now))
                ?? now.addingTimeInterval(86_400)
            return Self.nextWeekday(2 /* Monday */, hour: 8, calendar: calendar,
                                    now: tomorrow.addingTimeInterval(-1))
        }
    }

    /// The next date matching `weekday` (Calendar's 1=Sunday...7=Saturday) at
    /// `hour`:00:00, searching forward from `now` (today counts if that time
    /// hasn't passed yet).
    private static func nextWeekday(_ weekday: Int, hour: Int, calendar: Calendar, now: Date) -> Date {
        var components = DateComponents()
        components.weekday = weekday
        components.hour = hour
        components.minute = 0
        components.second = 0
        return calendar.nextDate(after: now, matching: components, matchingPolicy: .nextTime, direction: .forward)
            ?? now.addingTimeInterval(86_400)
    }
}
