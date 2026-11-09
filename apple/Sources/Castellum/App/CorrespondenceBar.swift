// Castellum - the Correspondence bar above the message list.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Shown while the list is a person's correspondence (AppModel.correspondence):
// who, how many, sent/received, first/last, and a 12-month activity strip.
// The numbers come from the pure `Correspondence.stats`.

import SwiftUI

struct CorrespondenceBar: View {
    let context: CorrespondenceContext
    let stats: CorrespondenceStats
    let onClose: () -> Void

    private static let dayFormat: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .none
        return f
    }()

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Correspondence with \(context.title)")
                    .font(.subheadline).bold()
                    .lineLimit(1)
                Text(summary)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            ActivityStrip(months: stats.perMonth)
                .frame(width: 120, height: 24)
                .help(activityHelp)
                .accessibilityElement(children: .ignore)
                .accessibilityLabel("Messages per month, last 12 months")
                .accessibilityValue(activityValue)
            Button(action: onClose) { Image(systemName: "xmark.circle.fill") }
                .buttonStyle(.borderless)
                .help("Close Correspondence")
                .accessibilityLabel("Close Correspondence")
                .keyboardShortcut(.cancelAction) // Esc
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
        .background(.bar)
    }

    private var summary: String {
        guard stats.total > 0 else { return String(localized: "No messages found") }
        let plus = stats.isCapped ? "+" : ""
        let noun = stats.total == 1 && !stats.isCapped ? "message" : "messages"
        var parts = ["\(stats.total)\(plus) \(noun)", "\(stats.received)\(plus) received", "\(stats.sent)\(plus) sent"]
        if let first = stats.first, let last = stats.last {
            let from = stats.isCapped ? "since at least " : ""
            parts.append("\(from)\(Self.dayFormat.string(from: first)) – \(Self.dayFormat.string(from: last))")
        }
        if stats.isCapped { parts.append(String(localized: "newest shown")) }
        return parts.joined(separator: " · ")
    }

    /// Spoken summary of the strip: the busiest month and the total it spans.
    private var activityValue: String {
        let total = stats.perMonth.reduce(0) { $0 + $1.count }
        guard total > 0, let busiest = stats.busiestMonth else { return "No messages" }
        return String(localized: "\(total) messages") + "; "
            + String(localized: "busiest month \(busiest.month) with \(busiest.count)")
    }

    private var activityHelp: String {
        guard let busiest = stats.busiestMonth, busiest.count > 0 else { return "" }
        return "Busiest of the last 12 months: \(busiest.month) (\(busiest.count))"
    }
}

/// Twelve tiny bars, oldest to newest; height is relative to the busiest month.
private struct ActivityStrip: View {
    let months: [(month: String, count: Int)]

    var body: some View {
        GeometryReader { geo in
            let peak = max(1, months.map(\.count).max() ?? 1)
            HStack(alignment: .bottom, spacing: 2) {
                ForEach(Array(months.enumerated()), id: \.offset) { _, entry in
                    RoundedRectangle(cornerRadius: 1.5)
                        .fill(entry.count == 0 ? Color.secondary.opacity(0.15) : Color.accentColor.opacity(0.8))
                        .frame(height: max(2, geo.size.height * CGFloat(entry.count) / CGFloat(peak)))
                }
            }
            .frame(maxHeight: .infinity, alignment: .bottom)
        }
    }
}
