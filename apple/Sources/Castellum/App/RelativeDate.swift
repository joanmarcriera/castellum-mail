// Castellum - compact relative date labels for the message list.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// Epoch seconds → compact label: "10:32" (today), "Yesterday", "Mon" (this
/// week), "12 Jun" (this year), "12 Jun 2024" (older). Empty for nil.
func relativeDate(_ epoch: Int64?, now: Date = Date()) -> String {
    guard let epoch else { return "" }
    let date = Date(timeIntervalSince1970: TimeInterval(epoch))
    let cal = Calendar.current
    let f = DateFormatter()

    if cal.isDate(date, inSameDayAs: now) {
        f.dateFormat = "HH:mm"
        return f.string(from: date)
    }
    if let yesterday = cal.date(byAdding: .day, value: -1, to: now),
       cal.isDate(date, inSameDayAs: yesterday) {
        return String(localized: "Yesterday")
    }
    // Calendar days, not elapsed 24 h periods: last Monday 18:00 seen on Monday
    // 10:00 is 7 calendar days ago ("12 Oct"), not "Mon" (audit #3454).
    let days = cal.dateComponents([.day], from: cal.startOfDay(for: date), to: cal.startOfDay(for: now)).day ?? 0
    if days >= 0 && days < 7 {
        f.dateFormat = "EEE"
    } else if cal.isDate(date, equalTo: now, toGranularity: .year) {
        f.dateFormat = "d MMM"
    } else {
        f.dateFormat = "d MMM yyyy"
    }
    return f.string(from: date)
}
