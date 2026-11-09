// Castellum - thread extraction panel (S54): action items, dates and
// amounts pulled from a thread by the local model, shown as quiet inline
// chips in the reading pane — never a blocking modal or a separate
// "processing" dialog (MailMate/Superhuman/Mimestream all keep this kind of
// AI surface out of the way of reading the mail itself).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

/// Fills in automatically once the `extract_thread_items` job completes —
/// nothing to click to reveal it. Degrades to a quiet, explained one-line
/// state on failure or when no model is configured; a legitimate "nothing to
/// extract" result renders nothing at all (not every thread has action
/// items, and an empty panel saying so would be noise on every plain email).
struct ThreadExtractionsPanel: View {
    let threadId: String
    @ObservedObject var coordinator: AiCoordinator
    @ObservedObject private var prefs = Preferences.shared

    private var items: [ThreadExtractionItem] {
        coordinator.threadExtractions[threadId] ?? []
    }

    var body: some View {
        Group {
            if let state = coordinator.jobState(forThreadId: threadId, feature: "extract_thread_items") {
                switch state.status {
                case "queued", "leased", "running", "streaming":
                    // Quiet — a small inline indicator, never a blocking
                    // overlay. Reading the thread is uninterrupted while
                    // this runs.
                    HStack(spacing: 6) {
                        ProgressView().controlSize(.mini)
                        Text("Finding action items…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                case "succeeded":
                    if !items.isEmpty {
                        chips
                    }
                    // Empty result: nothing to show, nothing to explain —
                    // not every thread has action items.
                case "cancelled":
                    EmptyView() // never started by the user directly; nothing to explain
                default: // "failed"
                    HStack(spacing: 6) {
                        Image(systemName: "sparkles")
                            .foregroundStyle(.tertiary)
                            .font(.caption)
                        Text(failureMessage(state.errorCode))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Button("Retry") {
                            coordinator.clear(jobId: state.id)
                            coordinator.extractThreadItems(threadId: threadId, forceRefresh: true)
                        }
                        .buttonStyle(.borderless)
                        .font(.caption)
                    }
                }
            } else {
                Color.clear.frame(width: 0, height: 0)
                    .onAppear { coordinator.extractThreadItems(threadId: threadId) }
            }
        }
        .onChange(of: threadId) { newThreadId in
            // Switching threads: nothing to click either — kick off
            // extraction for the newly-selected thread if not already known.
            if coordinator.jobState(forThreadId: newThreadId, feature: "extract_thread_items") == nil {
                coordinator.extractThreadItems(threadId: newThreadId)
            }
        }
    }

    private var chips: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 6) {
                ForEach(items) { item in
                    ExtractionChip(item: item)
                }
            }
        }
    }

    private func failureMessage(_ code: String?) -> String {
        switch code {
        case "model_missing":
            return "Model '\(prefs.aiModel)' not installed on the endpoint"
        case "endpoint_refused":
            return "No local model reachable"
        case "network", "timeout":
            return "Couldn't reach the local model"
        default:
            return "Couldn't find action items in this thread"
        }
    }
}

/// One extraction rendered as a quiet capsule, matching the project-tag
/// chips already used in `ReadingPaneView` — same visual language, so this
/// reads as part of the reading pane rather than a bolted-on AI feature.
private struct ExtractionChip: View {
    let item: ThreadExtractionItem

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: iconName)
                .font(.caption2)
            Text(label)
                .font(.caption2)
                .lineLimit(1)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
        .background(Capsule().fill(Color.accentColor.opacity(0.12)))
        .foregroundStyle(.secondary)
        .help(item.text) // the full extracted text, for a truncated label
        .textSelection(.enabled)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(kindLabel): \(item.text)")
    }

    private var iconName: String {
        switch item.extractionKind {
        case .actionItem: return "checkmark.circle"
        case .date: return "calendar"
        case .amount: return "banknote"
        case .unknown: return "sparkles"
        }
    }

    private var kindLabel: String {
        switch item.extractionKind {
        case .actionItem: return "Action item"
        case .date: return "Date"
        case .amount: return "Amount"
        case .unknown: return "Extracted"
        }
    }

    /// Prefer the structured value where it makes a tighter chip label than
    /// the free text would (a formatted amount, a bare date); action items
    /// and anything unrecognised just show the extracted text.
    private var label: String {
        switch item.extractionKind {
        case .date:
            return item.structuredValue?.date ?? item.text
        case .amount:
            if let amount = item.structuredValue?.amount,
               let currency = item.structuredValue?.currency {
                return String(format: "%@ %.2f", currency, amount)
            }
            return item.text
        case .actionItem, .unknown:
            return item.text
        }
    }
}
