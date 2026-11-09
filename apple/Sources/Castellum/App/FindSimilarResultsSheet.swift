// Castellum - "Find similar" results sheet (S53-UI).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Per DECISION-S53-ui (2026-09-23, option 3): semantic results appear ONLY
// behind this explicit gesture, never on the typing path. This sheet is
// deliberately styled distinctly from the main message list and the live
// search results — its own title bar, its own row layout (with a similarity
// indicator no other list shows) — so it can never be mistaken for either.

import SwiftUI

struct FindSimilarResultsSheet: View {
    @ObservedObject var model: AppModel
    let anchor: MessageHeader
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            content
        }
        .frame(width: 460, height: 420)
        // Only when no NEW anchor was set meanwhile: a reopen on the same
        // message has the same id, so a stale disappear must not clear it.
        .onDisappear { if model.findSimilarAnchor == nil { model.dismissFindSimilar() } }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Label("Find Similar", systemImage: "text.magnifyingglass")
                    .font(.headline)
                Text("Similar to: \(anchor.subject ?? "(no subject)")")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
            Button("Done") { dismiss() }
        }
        .padding(16)
    }

    @ViewBuilder
    private var content: some View {
        if model.findSimilarInFlight {
            VStack(spacing: 10) {
                ProgressView()
                Text("Finding similar messages…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let error = model.findSimilarError {
            VStack(spacing: 10) {
                Label("Couldn't find similar messages", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
                Button("Retry") { model.findSimilar(anchor: anchor) }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if model.findSimilarResults.isEmpty {
            VStack(spacing: 6) {
                Text("No similar messages found")
                    .foregroundStyle(.secondary)
                Text("Nothing in this mailbox looked close enough to compare.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List(model.findSimilarResults) { result in
                ResultRow(result: result) {
                    // Fetched fresh + opened in its own window (like a
                    // double-clicked list row) rather than routed through
                    // the current mailbox selection — a result can be from
                    // ANY account/mailbox, not necessarily the one on
                    // screen.
                    model.openStandaloneWindow(for: result.header)
                    dismiss()
                }
            }
            .listStyle(.plain)
        }
    }
}

private struct ResultRow: View {
    let result: SimilarMessageResult
    let onSelect: () -> Void

    var body: some View {
        Button(action: onSelect) {
            HStack(alignment: .top, spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(result.header.subject ?? "(no subject)")
                        .font(.callout).bold()
                        .lineLimit(1)
                    Text(result.header.fromName ?? result.header.fromAddress ?? "")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Text(relativeDate(result.header.sentAt))
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
                Spacer()
                SimilarityBadge(similarity: result.similarity)
            }
            .padding(.vertical, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

/// A short "94%" badge — deliberately not a chart (this isn't a data-viz
/// stage); just enough to rank results at a glance.
private struct SimilarityBadge: View {
    let similarity: Double

    private var percent: Int {
        Int((similarity.clamped(to: 0...1) * 100).rounded())
    }

    var body: some View {
        Text("\(percent)%")
            .font(.caption2).monospacedDigit()
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(Capsule().fill(Color.accentColor.opacity(0.15)))
            .foregroundStyle(Color.accentColor)
    }
}

private extension Double {
    func clamped(to range: ClosedRange<Double>) -> Double {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
