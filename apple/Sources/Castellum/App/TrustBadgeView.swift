// Castellum - SPF/DKIM/DMARC trust badge (S48).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Compact, non-alarmist: a neutral shield when nothing is verified (the
// common case — most personal/small-business mail servers don't add
// Authentication-Results at all), a plain checkmark shield when everything
// checked passed, and an amber (not red — this is informational, not an
// app error) caution shield only when something explicitly failed.

import SwiftUI

enum TrustBadgeKind: Equatable {
    case unverified
    case authenticated
    case failed

    /// Classify a message's trust signals. `nil` input (an older core
    /// payload with no `trust` key) reads the same as "unverified" — there
    /// is nothing to show either way.
    static func classify(_ trust: TrustSignals?) -> TrustBadgeKind {
        guard let trust else { return .unverified }
        let results = [trust.spf, trust.dkim, trust.dmarc]
        if results.contains("fail") { return .failed }
        if results.contains("pass") { return .authenticated }
        return .unverified
    }

    /// Whether a per-sender "always load remote images" decision may be honoured
    /// for a message with these trust signals (audit #3372). The allowlist is
    /// keyed on the unauthenticated From header, so a message that explicitly
    /// FAILED SPF, DKIM or DMARC (a likely forgery of an allowlisted sender)
    /// is treated as if the sender were not allowlisted: the per-message
    /// prompt shows and nothing is fetched until the user chooses to.
    static func senderAllowlistApplies(_ trust: TrustSignals?) -> Bool {
        classify(trust) != .failed
    }

    var systemImage: String {
        switch self {
        case .unverified: return "shield"
        case .authenticated: return "checkmark.shield"
        case .failed: return "exclamationmark.shield"
        }
    }

    var tint: Color {
        switch self {
        case .unverified: return .secondary
        case .authenticated: return .secondary
        case .failed: return .orange
        }
    }

    var summary: String {
        switch self {
        case .unverified: return String(localized: "Not verified")
        case .authenticated: return String(localized: "Authenticated")
        case .failed: return String(localized: "Failed a check")
        }
    }
}

struct TrustBadgeView: View {
    let trust: TrustSignals?
    @State private var showsDetails = false

    var body: some View {
        let kind = TrustBadgeKind.classify(trust)
        Button {
            showsDetails = true
        } label: {
            Image(systemName: kind.systemImage)
                .foregroundStyle(kind.tint)
        }
        .buttonStyle(.borderless)
        .help(kind.summary)
        .accessibilityLabel(kind.summary)
        .popover(isPresented: $showsDetails) {
            TrustDetailsView(trust: trust, kind: kind)
        }
    }
}

private struct TrustDetailsView: View {
    let trust: TrustSignals?
    let kind: TrustBadgeKind

    private func row(_ label: String, _ value: String?) -> some View {
        HStack {
            Text(label).font(.caption).foregroundStyle(.secondary)
            Spacer()
            Text(mechanismLabel(value)).font(.caption).monospaced()
        }
        .accessibilityElement(children: .combine)
    }

    private func mechanismLabel(_ value: String?) -> String {
        switch value {
        case "pass": return String(localized: "Pass")
        case "fail": return String(localized: "Fail")
        default: return String(localized: "Not checked")
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(kind.summary, systemImage: kind.systemImage)
                .font(.headline)
                .foregroundStyle(kind.tint)
            row(String(localized: "SPF"), trust?.spf)
            row(String(localized: "DKIM"), trust?.dkim)
            row(String(localized: "DMARC"), trust?.dmarc)
            Divider()
            if let authservId = trust?.authservId {
                Text("Checked by \(authservId).")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            } else {
                Text("Your mail server didn't check this message, or Castellum couldn't confirm which server did.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
            Text("Based on your mail server's own check when the message arrived. Not a guarantee — treat it as one signal among others.")
                .font(.caption2)
                .foregroundStyle(.secondary)
        }
        .padding(12)
        .frame(width: 260)
    }
}
