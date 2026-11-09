// Castellum - reading pane (header + HTML/text body).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Renders sanitized HTML in a policy-locked WKWebView when available,
// falling back to text_body. Remote images are blocked by default; a banner
// offers per-message opt-in (or the global preference overrides).

import AppKit
import SwiftUI

struct ReadingPaneView: View {
    let detail: MessageDetail?
    /// Nil in standalone (single-message) windows where project tags aren't shown.
    var model: AppModel?
    var aiCoordinator: AiCoordinator? = nil
    var isHydratingBody = false
    var bodyHydrationError: String?
    var onLoadBody: (() -> Void)?
    /// Resolves the message's inline (cid:) images; nil shows the grey placeholder (#3442).
    var inlineImages: ((MessageDetail) -> InlineImageSource?)?

    @ObservedObject private var prefs = Preferences.shared
    /// Per-message opt-in from the banner; reset when the message changes.
    @State private var loadRemoteImagesThisMessage = false
    /// Set when the webview must not be trusted (rule-list failure/crashes).
    @State private var webViewFailed = false
    /// Currently hovered link's raw payload from the hover script (S48).
    @State private var hoveredLink: LinkHoverInfo?
    /// "All Headers" mode: the raw header block of the shown message, once known.
    @State private var rawHeaders: (messageId: String, text: String)?
    @State private var loadingRawHeaders = false
    /// Fold state of the plain-text body in a standalone window (the main pane keeps it in the model).
    @State private var localFold = PlainFoldState()

    private var foldBinding: Binding<PlainFoldState> {
        if let model { return Binding(get: { model.plainFold }, set: { model.plainFold = $0 }) }
        return $localFold
    }

    private func recipients(_ d: MessageDetail, _ kind: String) -> String {
        d.addresses
            .filter { $0.kind == kind }
            .map { $0.name ?? $0.address }
            .joined(separator: ", ")
    }

    /// "To: a, b" / "Cc: ..." — each address is a drill-down link in the main
    /// pane (`model != nil`), plain selectable text in standalone windows.
    @ViewBuilder
    private func recipientLine(_ d: MessageDetail, kind: String, label: String) -> some View {
        let people = d.addresses.filter { $0.kind == kind }
        if !people.isEmpty {
            if let model {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 0) {
                        Text("\(label): ").font(.caption).foregroundStyle(.secondary)
                        ForEach(Array(people.enumerated()), id: \.offset) { index, person in
                            DrillLink(help: "Search for mail \(kind == "cc" ? "copied to" : "sent to") \(person.address)") {
                                model.drillDown(qualifier: kind, value: "=" + person.address)  // exact address, not contains
                            } label: {
                                Text(person.name ?? person.address).font(.caption).foregroundStyle(.secondary)
                            }
                            .addressHoverCard(model: model, address: person.address, name: person.name)
                            if index < people.count - 1 {
                                Text(", ").font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            } else {
                Text("\(label): \(recipients(d, kind))").font(.caption).foregroundStyle(.secondary)
                    .textSelection(.enabled).lineLimit(2)
            }
        }
    }

    private static func recipientLabel(_ kind: String) -> String {
        MessageHeaderMode.recipientLabel(kind)
    }

    /// "All Headers": Message-ID, plus the full raw header block when the
    /// source is cached locally (or after the user loads it).
    @ViewBuilder
    private func technicalHeaders(_ d: MessageDetail) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if let mid = d.rfc822MessageId {
                Text("Message-ID: \(mid)").font(.caption).foregroundStyle(.secondary)
                    .textSelection(.enabled).lineLimit(2)
            }
            if let raw = rawHeaders, raw.messageId == d.header.id {
                ScrollView {
                    Text(raw.text)
                        .font(.system(size: 10, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 160)
                .padding(6)
                .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 4))
            } else if let model {
                Button(loadingRawHeaders ? "…" : String(localized: "Load full headers")) {
                    loadingRawHeaders = true
                    Task {
                        let text = await model.loadRawHeaderBlock(
                            messageId: d.header.id, accountId: d.header.accountId)
                        loadingRawHeaders = false
                        if let text { rawHeaders = (d.header.id, text) }
                    }
                }
                .buttonStyle(.link)
                .font(.caption)
                .disabled(loadingRawHeaders)
            }
        }
        .task(id: d.header.id) {
            // Local-only: pick up an already-cached source without any network.
            if let model, rawHeaders?.messageId != d.header.id,
               let text = model.cachedRawHeaderBlock(messageId: d.header.id) {
                rawHeaders = (d.header.id, text)
            }
        }
    }

    private func fullDate(_ epoch: Int64?) -> String {
        guard let epoch else { return "" }
        let f = DateFormatter()
        f.dateStyle = .full
        f.timeStyle = .short
        return f.string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
    }

    var body: some View {
        if let d = detail {
            let bodyState = ReadingPaneBodyState.evaluate(
                detail: d,
                isHydrating: isHydratingBody,
                error: bodyHydrationError
            )
            VStack(alignment: .leading, spacing: 8) {
                if let model, let subject = d.header.subject, !subject.trimmingCharacters(in: .whitespaces).isEmpty {
                    DrillLink(help: "Search for this subject") {
                        model.drillDown(qualifier: "subject", value: subject)
                    } label: {
                        Text(subject).font(.title3).bold().multilineTextAlignment(.leading)
                    }
                } else {
                    Text(d.header.subject ?? "(no subject)")
                        .font(.title3).bold()
                        .textSelection(.enabled)
                }
                HStack(spacing: 6) {
                    if let model, let from = d.header.fromAddress {
                        DrillLink(help: "Search for mail from \(from)") {
                            model.drillDown(qualifier: "from", value: "=" + from)  // exact address, not contains
                        } label: {
                            HStack(spacing: 6) {
                                Text(d.header.fromName ?? from).font(.subheadline).bold()
                                Text("<\(from)>").font(.subheadline).foregroundStyle(.secondary)
                            }
                        }
                        .addressHoverCard(model: model, address: from, name: d.header.fromName)
                    } else {
                        Text(d.header.fromName ?? d.header.fromAddress ?? "")
                            .font(.subheadline).bold()
                        if let a = d.header.fromAddress {
                            Text("<\(a)>").font(.subheadline).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    TrustBadgeView(trust: d.trust)
                    if let model, let sent = d.header.sentAt {
                        DrillLink(help: "Search for mail from this day (\(fullDate(sent)))") {
                            model.drillDown(qualifier: "date", value: SearchQueryBuilder.dayToken(epoch: sent))
                        } label: {
                            Text(relativeDate(d.header.sentAt)).font(.subheadline).foregroundStyle(.secondary)
                        }
                    } else {
                        Text(relativeDate(d.header.sentAt))
                            .font(.subheadline).foregroundStyle(.secondary)
                            .help(fullDate(d.header.sentAt))
                    }
                }
                ForEach(prefs.headerMode.recipientKinds, id: \.self) { kind in
                    recipientLine(d, kind: kind, label: Self.recipientLabel(kind))
                }
                if prefs.headerMode.showsTechnicalHeaders {
                    technicalHeaders(d)
                }
                // Project tags + assign link (only in the main reading pane, not standalone windows)
                if let model {
                    HStack(spacing: 6) {
                        if !model.currentProjectTags.isEmpty {
                            ScrollView(.horizontal, showsIndicators: false) {
                                HStack(spacing: 4) {
                                    ForEach(model.currentProjectTags, id: \.projectId) { tag in
                                        Text(tag.name)
                                            .font(.caption2)
                                            .padding(.horizontal, 6)
                                            .padding(.vertical, 2)
                                            .background(
                                                Capsule().fill(
                                                    (tag.color.flatMap { Color(hex: $0) } ?? Color.accentColor)
                                                        .opacity(0.15)
                                                )
                                            )
                                            .foregroundStyle(.secondary)
                                    }
                                }
                            }
                        }
                        if let party = Correspondence.otherParty(
                            of: d, myAddresses: model.myAddresses) {
                            Button("Correspondence") {
                                model.showCorrespondence(address: party.address, name: party.name)
                            }
                            .buttonStyle(.link)
                            .font(.caption)
                            .help("All mail to and from \(party.address), across accounts")
                        }
                        Button("Assign to project") {
                            NotificationCenter.default.post(name: .castellumAssignProject, object: nil)
                        }
                        .buttonStyle(.link)
                        .font(.caption)
                        if d.hasUnsubscribeAffordance {
                            Button("Unsubscribe") { model.unsubscribeCandidate = d }
                                .buttonStyle(.link)
                                .font(.caption)
                        }
                    }
                }
                Divider()
                // S32: incoming attachments (open / Save As / drag / Quick
                // Look). Needs the hydrated `MessageDetail.attachments` list,
                // so it is gated on body availability like the AI section;
                // standalone windows have no model and show no strip.
                if messageBodyActionsAvailable(d), let model {
                    AttachmentStripView(detail: d, model: model)
                    if !d.stripAttachments.isEmpty {
                        Divider()
                    }
                }
                // S55: parsed synchronously at hydration time by the core —
                // this is a plain read, no spinner, no network round trip.
                if let invite = d.calendarInvite, let model {
                    CalendarInviteCardView(
                        invite: invite, messageId: d.header.id, accountId: d.header.accountId,
                        model: model
                    )
                    Divider()
                }
                if messageBodyActionsAvailable(d),
                   prefs.aiEnabled,
                   let coordinator = aiCoordinator {
                    AiSummarySection(messageId: d.header.id, coordinator: coordinator)
                    if let threadId = d.threadId {
                        ThreadExtractionsPanel(threadId: threadId, coordinator: coordinator)
                    }
                    // S53-UI: "Find similar" — a distinct icon/label from
                    // Summarize (which shares a Task.detached-off-actor
                    // model with this), never inline results here — it only
                    // ever opens FindSimilarResultsSheet, a separate sheet,
                    // per DECISION-S53-ui.
                    if let model {
                        Button {
                            model.findSimilar(anchor: d.header)
                        } label: {
                            Label("Find Similar", systemImage: "text.magnifyingglass")
                        }
                        .buttonStyle(.borderless)
                        .font(.callout)
                    }
                    Divider()
                }
                switch bodyState {
                case .ready:
                    if let html = d.sanitizedHtml, !html.isEmpty, !webViewFailed {
                        // The sender allowlist keys on the unauthenticated From header, so
                        // it is ignored when SPF/DKIM/DMARC explicitly failed (audit #3372).
                        let senderAllowed = TrustBadgeKind.senderAllowlistApplies(d.trust) && (model?.remoteContentAllowed(
                            accountId: d.header.accountId, fromAddress: d.header.fromAddress
                        ) ?? false)
                        let allowRemote = prefs.alwaysLoadRemoteImages || loadRemoteImagesThisMessage || senderAllowed
                        ZStack(alignment: .topLeading) {
                            VStack(spacing: 0) {
                                if (d.hasRemoteContent ?? false) && !allowRemote {
                                    HStack(spacing: 8) {
                                        Image(systemName: "photo.badge.exclamationmark")
                                            .foregroundStyle(.secondary)
                                        let report = model?.remoteContentReport(for: d)
                                        Text(TrackerReportPresentation.label(for: report)).font(.caption)
                                            .foregroundStyle(.secondary)
                                            .help(TrackerReportPresentation.detail(for: report) ?? "")
                                        Button("Load") { loadRemoteImagesThisMessage = true }
                                            .font(.caption)
                                        if let model, let from = d.header.fromAddress {
                                            Button("Always allow \(from)") {
                                                model.allowRemoteContentAlways(accountId: d.header.accountId, fromAddress: from)
                                            }
                                            .font(.caption)
                                        }
                                        Spacer()
                                    }
                                    .padding(.vertical, 4)
                                }
                                EmailBodyWebView(
                                    sanitizedHtml: html,
                                    allowRemoteImages: allowRemote,
                                    onUnrecoverableFailure: { webViewFailed = true },
                                    onLinkHover: { hoveredLink = $0 },
                                    onSelectionChange: { text in
                                        guard let model else { return }
                                        model.readingSelection = text.isEmpty
                                            ? nil : ReadingSelection(messageId: d.header.id, text: text)
                                    },
                                    inlineImages: inlineImages?(d)
                                )
                            }
                            if let (hover, inspection) = LinkHoverPolicy.reveal(for: hoveredLink) {
                                LinkRevealView(inspection: inspection)
                                    .position(x: hover.point.x + 140, y: hover.point.y + 20)
                            }
                        }
                    } else {
                        PlainTextBodyView(
                            body: d.textBody ?? String(localized: "(no text body)"),
                            fold: foldBinding,
                            foldingEnabled: prefs.foldQuotes,
                            onSelection: { text in
                                guard let model else { return }
                                model.readingSelection = text.isEmpty
                                    ? nil : ReadingSelection(messageId: d.header.id, text: text)
                            })
                            .frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                case .loading:
                    DescriptorBodyPlaceholder(
                        hasAttachments: d.header.hasAttachments,
                        isLoading: true,
                        error: nil,
                        onLoad: nil
                    )
                case .notDownloaded(let hasAttachments):
                    DescriptorBodyPlaceholder(
                        hasAttachments: hasAttachments,
                        isLoading: false,
                        error: bodyHydrationError,
                        onLoad: onLoadBody
                    )
                }
            }
            .onChange(of: d.header.id) { _ in
                localFold.reset()
                loadRemoteImagesThisMessage = false
                webViewFailed = false
                hoveredLink = nil
            }
            .padding(16)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        } else {
            VStack(spacing: 6) {
                Image(systemName: "envelope.open")
                    .font(.largeTitle)
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Text("No message selected").foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .accessibilityElement(children: .combine)
        }
    }
}

enum ReadingPaneBodyState: Equatable {
    case ready
    case loading
    case notDownloaded(hasAttachments: Bool)

    static func evaluate(
        detail: MessageDetail,
        isHydrating: Bool,
        error: String?
    ) -> ReadingPaneBodyState {
        guard detail.header.hasBody == false else { return .ready }
        if isHydrating && error == nil { return .loading }
        return .notDownloaded(hasAttachments: detail.header.hasAttachments)
    }
}

private struct DescriptorBodyPlaceholder: View {
    let hasAttachments: Bool
    let isLoading: Bool
    let error: String?
    let onLoad: (() -> Void)?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                if isLoading {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: hasAttachments ? "paperclip" : "arrow.down.circle")
                        .foregroundStyle(.secondary)
                        .accessibilityHidden(true)
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(isLoading
                         ? String(localized: "Loading message body...")
                         : String(localized: "Message body not downloaded"))
                        .font(.callout)
                    if hasAttachments {
                        Text("Attachments are indicated by server metadata.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    if let error, !error.isEmpty {
                        Text(error)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                    }
                }
                Spacer()
                if !isLoading, let onLoad {
                    Button("Load") { onLoad() }
                        .buttonStyle(.bordered)
                }
            }
            .padding(12)
            .background(
                RoundedRectangle(cornerRadius: 8)
                    .fill(Color.secondary.opacity(0.08))
            )
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
    }
}

// MARK: - Link hover reveal (S48)

private struct LinkRevealView: View {
    let inspection: LinkInspection

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let host = inspection.displayHost {
                Text(host).font(.caption).bold().monospaced()
            } else {
                Text(inspection.scheme.map { "\($0):" } ?? "link").font(.caption).monospaced()
            }
            Text(inspection.href)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.middle)
            if inspection.textMismatch {
                Label("Link text doesn't match its destination", systemImage: "exclamationmark.triangle")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
            if inspection.idnCaution, let unicode = inspection.unicodeHost {
                Label("Non-ASCII domain: \(unicode)", systemImage: "globe")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
        }
        .padding(8)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 6))
        .frame(maxWidth: 280, alignment: .leading)
    }
}

// MARK: - AI Summary Section

private struct AiSummarySection: View {
    let messageId: String
    @ObservedObject var coordinator: AiCoordinator
    @ObservedObject private var prefs = Preferences.shared

    var body: some View {
        let state = coordinator.jobState(forMessageId: messageId, feature: "summarize_message")

        VStack(alignment: .leading, spacing: 6) {
            if let state {
                switch state.status {
                case "queued", "leased", "running", "streaming":
                    HStack(alignment: .top, spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text(state.text.isEmpty ? "Summarizing…" : state.text)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                        Spacer()
                        Button("Cancel") { coordinator.cancel(jobId: state.id) }
                            .buttonStyle(.borderless)
                            .font(.caption)
                    }
                case "succeeded":
                    VStack(alignment: .leading, spacing: 4) {
                        Text(state.text)
                            .font(.callout)
                            .textSelection(.enabled)
                        HStack {
                            Label(state.isCached ? "AI summary (cached)" : "AI summary",
                                  systemImage: "sparkles")
                                .font(.caption2)
                                .foregroundStyle(.tertiary)
                            Spacer()
                            Button("Regenerate") {
                                coordinator.clear(jobId: state.id)
                                coordinator.summarize(messageId: messageId,
                                                      forceRefresh: true)
                            }
                            .buttonStyle(.borderless)
                            .font(.caption)
                        }
                    }
                case "cancelled":
                    HStack {
                        Label("Stopped early", systemImage: "stop.circle")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button("Summarize") { coordinator.summarize(messageId: messageId) }
                            .buttonStyle(.borderless)
                            .font(.caption)
                    }
                default: // "failed"
                    VStack(alignment: .leading, spacing: 4) {
                        HStack {
                            Label(errorMessage(state.errorCode), systemImage: "exclamationmark.triangle")
                                .font(.caption)
                                .foregroundStyle(.red)
                            Spacer()
                            Button("Retry") { coordinator.summarize(messageId: messageId) }
                                .buttonStyle(.borderless)
                                .font(.caption)
                        }
                        if state.errorCode == "model_missing" {
                            Text("Run `ollama pull \(prefs.aiModel)` on the endpoint to install the model.")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }
                    }
                }
            } else {
                HStack(spacing: 6) {
                    Button {
                        coordinator.summarize(messageId: messageId)
                    } label: {
                        Label("Summarize", systemImage: "sparkles")
                    }
                    .buttonStyle(.borderless)
                    .font(.callout)

                    Button {
                        NotificationCenter.default.post(name: .castellumReplyWithAi, object: nil)
                    } label: {
                        Label("Reply with AI", systemImage: "arrowshape.turn.up.left.circle")
                    }
                    .buttonStyle(.borderless)
                    .font(.callout)
                }
            }
        }
        .padding(.vertical, 6)
        // NOTE: ⌘⇧S (.castellumSummarize) is handled ONCE, in RootView's
        // NotificationHandlers → model.summarizeSelectedMessage(). Subscribing
        // here as well double-fired one job per keypress.
    }

    private func errorMessage(_ code: String?) -> String {
        AiEventReducer.errorMessage(code: code, modelName: prefs.aiModel)
    }
}

/// A header element that narrows the current search when clicked (MM-2).
/// Plain-looking text that dims on hover and shows the pointing-hand cursor.
private struct DrillLink<Label: View>: View {
    let help: String
    let action: () -> Void
    @ViewBuilder let label: () -> Label
    @State private var hovering = false
    /// Whether THIS view currently holds a pushed cursor, so every push is
    /// popped exactly once — including when the view disappears under the
    /// pointer (a click can replace the list and remove the link without a
    /// hover-exit event).
    @State private var cursorPushed = false

    var body: some View {
        Button(action: action) { label().opacity(hovering ? 0.65 : 1) }
            .buttonStyle(.plain)
            .onHover { inside in
                hovering = inside
                if inside { pushCursor() } else { popCursor() }
            }
            .onDisappear { popCursor() }
            .help(help)
    }

    private func pushCursor() {
        guard !cursorPushed else { return }
        NSCursor.pointingHand.push()
        cursorPushed = true
    }

    private func popCursor() {
        guard cursorPushed else { return }
        NSCursor.pop()
        cursorPushed = false
    }
}
