// Castellum - the hover card on header addresses in the reader.
//
// Rest the pointer on a sender or recipient and a small popover shows who it is: name, address,
// how much mail has passed between you (from the locally stored contact row), first and last seen,
// and four actions (search mail from them, show the correspondence, VIP on/off, copy the address).
// Local only: a hover never fetches anything. The card opens after `AddressCardTiming.openDelay`
// and stays open while the pointer is over the address OR the card.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import SwiftUI

struct AddressHoverCard: View {
    @ObservedObject var model: AppModel
    let address: String
    let name: String?
    var onClose: () -> Void = {}

    @State private var content: AddressCardContent?

    private static let dateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .none
        return f
    }()

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            let shown = content ?? AddressCardContent.make(
                name: name, address: address, contact: nil, isVip: false, myAddresses: model.myAddresses)
            HStack(spacing: 6) {
                Text(shown.title).font(.headline).lineLimit(2)
                if shown.isVip { Image(systemName: "star.fill").foregroundStyle(.yellow).help(String(localized: "VIP")) }
            }
            Text(shown.address).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            if let summary = shown.exchangeSummary { Text(summary).font(.caption) }
            if let first = shown.firstSeen, let last = shown.lastSeen {
                Text("\(Self.dateFormatter.string(from: first)) – \(Self.dateFormatter.string(from: last))")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let notes = shown.notes { Text(notes).font(.caption).foregroundStyle(.secondary).lineLimit(3) }
            Divider()
            HStack(spacing: 10) {
                Button("Mail from") { model.drillDown(qualifier: "from", value: "=" + address); onClose() }
                    .help(String(localized: "Search for mail from this address"))
                if !shown.isSelf {
                    Button("Correspondence") { model.showCorrespondence(address: address, name: name); onClose() }
                        .help(String(localized: "All mail to and from this address, across accounts"))
                    Button(shown.isVip ? "Remove VIP" : "Make VIP") {
                        model.setVip(address: address, displayName: name, vip: !shown.isVip)
                        Task { content = await model.addressCardContent(address: address, name: name) }
                    }
                }
                Button("Copy") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(address, forType: .string)
                    onClose()
                }
                .help(String(localized: "Copy the address"))
            }
            .buttonStyle(.link)
            .font(.caption)
        }
        .padding(12)
        .frame(width: 300, alignment: .leading)
        .task { content = await model.addressCardContent(address: address, name: name) }
    }
}

/// Attaches the card to a header address: opens after a rest, stays while the pointer is on the
/// address or the card, closes shortly after it has left both.
private struct AddressHoverCardModifier: ViewModifier {
    @ObservedObject var model: AppModel
    let address: String
    let name: String?

    @State private var showing = false
    @State private var overTarget = false
    @State private var overCard = false
    @State private var pending: Task<Void, Never>?

    func body(content: Content) -> some View {
        content
            .onHover { inside in
                overTarget = inside
                reschedule()
            }
            .onDisappear { pending?.cancel(); showing = false }
            .popover(isPresented: $showing, arrowEdge: .bottom) {
                AddressHoverCard(model: model, address: address, name: name, onClose: { showing = false })
                    .onHover { inside in overCard = inside; reschedule() }
            }
    }

    /// One decision after each pointer change: open if on the address and not open yet, close if on neither.
    private func reschedule() {
        pending?.cancel()
        if overTarget, !showing {
            pending = Task {
                try? await Task.sleep(nanoseconds: UInt64(AddressCardTiming.openDelay * 1_000_000_000))
                if !Task.isCancelled, overTarget { showing = true }
            }
        } else if !overTarget, !overCard, showing {
            pending = Task {
                try? await Task.sleep(nanoseconds: UInt64(AddressCardTiming.closeDelay * 1_000_000_000))
                if !Task.isCancelled, !overTarget, !overCard { showing = false }
            }
        }
    }
}

extension View {
    /// The hover card for a header address (main reading pane only: it needs the model).
    func addressHoverCard(model: AppModel, address: String, name: String?) -> some View {
        modifier(AddressHoverCardModifier(model: model, address: address, name: name))
    }
}
