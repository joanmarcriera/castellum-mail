// Castellum - Window > AI Activity: every AI job this session, with its
// message subject, state, elapsed time, per-row Cancel, and Clear Finished
//. Reuses AiCoordinator.jobs directly — no separate store.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import SwiftUI

struct AiActivityView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var coordinator: AiCoordinator

    /// Newest first. Cached (negative-id) entries sort last within a status
    /// tier since they never "ran" in this session in any meaningful order.
    private var rows: [AiJobViewState] {
        coordinator.jobs.values.sorted { $0.createdAt > $1.createdAt }
    }

    var body: some View {
        VStack(spacing: 0) {
            EmbeddingProgressBanner(drainer: coordinator.embeddings)
            if rows.isEmpty {
                emptyState
            } else {
                TimelineView(.periodic(from: .now, by: 1)) { context in
                    List(rows, id: \.id) { row in
                        AiActivityRowView(
                            row: row,
                            subject: subject(forMessageId: row.ownerId),
                            now: context.date,
                            onCancel: { coordinator.cancel(jobId: row.id) }
                        )
                    }
                    .listStyle(.inset(alternatesRowBackgrounds: true))
                }
            }
            Divider()
            bottomBar
                .padding(.horizontal, 16)
                .padding(.vertical, 10)
        }
        .frame(minWidth: 460, minHeight: 300)
    }

    private var emptyState: some View {
        VStack(spacing: 8) {
            Image(systemName: "sparkles")
                .font(.system(size: 28))
                .foregroundStyle(.tertiary)
            Text("No AI activity yet.")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var bottomBar: some View {
        HStack {
            Spacer()
            Button("Clear Finished") { clearFinished() }
                .disabled(!rows.contains { $0.isTerminal })
        }
    }

    private func clearFinished() {
        for row in rows where row.isTerminal {
            coordinator.clear(jobId: row.id)
        }
    }

    /// Best-effort subject lookup against the currently-loaded messages; a
    /// job for a message from another mailbox this session hasn't loaded
    /// just falls back to the message id, same as Diagnostics does elsewhere.
    private func subject(forMessageId messageId: String) -> String {
        AiActivityLabels.subject(forOwnerId: messageId, in: model.messages)
    }
}

/// Pure label helpers for AI Activity rows (RP03), kept apart from the view so
/// they can be unit-tested.
enum AiActivityLabels {
    /// A human name for a job's `feature`; unknown features fall back to the raw
    /// identifier so a new job kind is never hidden.
    static func kind(forFeature feature: String) -> String {
        switch feature {
        case "summarize_message": return "Summarise"
        case "draft_reply": return "Draft reply"
        case "extract_thread_items": return String(localized: "Extract thread items")
        default: return feature
        }
    }

    /// The subject for a job's owner. Message jobs are owned by a message id;
    /// thread jobs by `t:<root message id>`, so the prefix is stripped before
    /// the lookup. Falls back to the raw owner id when nothing is loaded.
    static func subject(forOwnerId ownerId: String, in messages: [MessageHeader]) -> String {
        let messageId = ownerId.hasPrefix("t:") ? String(ownerId.dropFirst(2)) : ownerId
        guard let message = messages.first(where: { $0.id == messageId || $0.id == ownerId }),
              let subject = message.subject, !subject.isEmpty
        else { return ownerId }
        return subject
    }
}

/// Queued / done counts for the background embedding index (F1), and the readable error when
/// the configured model cannot embed or the endpoint is down. Hidden while embedding is off.
struct EmbeddingProgressBanner: View {
    @ObservedObject var drainer: EmbeddingDrainer

    var body: some View {
        let p = drainer.progress
        if p.state != .off {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Image(systemName: "point.3.connected.trianglepath.dotted")
                        .foregroundStyle(.secondary)
                    Text("Embeddings: \(p.done) done, \(p.queued) queued")
                        .font(.callout)
                        .accessibilityIdentifier("ai-activity-embeddings")
                }
                if let error = EmbeddingDrainer.errorText(for: p) {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("ai-activity-embeddings-error")
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            Divider()
        }
    }
}

private struct AiActivityRowView: View {
    let row: AiJobViewState
    let subject: String
    let now: Date
    let onCancel: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            icon
            VStack(alignment: .leading, spacing: 2) {
                Text(subject).font(.callout).lineLimit(1)
                HStack(spacing: 4) {
                    Text(kindLabel).font(.caption2).foregroundStyle(.secondary)
                    Text("·").font(.caption2).foregroundStyle(.tertiary)
                    Text(stateLabel).font(.caption2).foregroundStyle(isFailure ? .red : .secondary)
                    if row.isCached {
                        Text("(cached)").font(.caption2).foregroundStyle(.tertiary)
                    }
                    Text("·").font(.caption2).foregroundStyle(.tertiary)
                    Text(elapsedLabel).font(.caption2).foregroundStyle(.tertiary)
                    if let timing = AiEventReducer.timingLabel(
                        durationMs: row.durationMs, tokensPerSecond: row.tokensPerSecond
                    ) {
                        Text("·").font(.caption2).foregroundStyle(.tertiary)
                        Text(timing).font(.caption2).foregroundStyle(.tertiary)
                    }
                }
            }
            Spacer()
            if !row.isTerminal {
                Button("Cancel", action: onCancel)
                    .buttonStyle(.borderless)
                    .accessibilityLabel(String(localized: "Cancel \(kindLabel) for \(subject)"))
            }
        }
        .padding(.vertical, 2)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(String(localized: "\(subject): \(kindLabel), \(stateLabel)"))
    }

    private var isFailure: Bool { row.status == "failed" }

    private var kindLabel: String {
        AiActivityLabels.kind(forFeature: row.feature)
    }

    private var stateLabel: String { row.status.capitalized }

    private var elapsedLabel: String {
        let seconds = max(0, now.timeIntervalSince(row.createdAt))
        if seconds < 60 { return "\(Int(seconds))s" }
        let minutes = Int(seconds) / 60
        let remainder = Int(seconds) % 60
        return "\(minutes)m \(remainder)s"
    }

    private var icon: some View {
        let name: String
        let colour: Color
        switch row.status {
        case "succeeded":
            (name, colour) = ("checkmark.circle.fill", .green)
        case "failed":
            (name, colour) = ("exclamationmark.circle.fill", .red)
        case "cancelled":
            (name, colour) = ("slash.circle", .secondary)
        case "running", "streaming":
            (name, colour) = ("arrow.triangle.2.circlepath", .secondary)
        default:
            (name, colour) = ("clock", .secondary)
        }
        return Image(systemName: name).foregroundStyle(colour)
    }
}

/// Settings > AI: the embedding index's error line (e.g. a chat model chosen as the embedding
/// model), next to the picker that fixes it. Empty while all is well.
struct EmbeddingSettingsStatus: View {
    @ObservedObject var drainer: EmbeddingDrainer

    var body: some View {
        if let error = EmbeddingDrainer.errorText(for: drainer.progress) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Image(systemName: "xmark.circle.fill").foregroundStyle(.red)
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .accessibilityIdentifier("settings-ai-embeddings-error")
            }
            .padding(.top, 4)
        }
    }
}
