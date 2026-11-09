// Castellum - M3 (S64): signpost INTERVALS for the interactions a user feels
// after launch — mailbox switch, message open, compose open, send.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Usage:
//     var span: PerfSpan?                       // stored by whoever owns the interaction
//     PerfSignpost.begin(.mailboxSwitch, replacing: &span)
//     ...later, when the interaction is done...
//     PerfSignpost.end(&span, count: rows.count)
//
// View in Instruments (os_signpost / Points of Interest template) or
// `log stream --signpost --predicate 'subsystem == "uk.co.riera.castellum"'`.
//
// PRIVACY: names are fixed literals and the only metadata is a count.
// Never add a subject, address, body text or mailbox name here.
//
// COST: `begin` returns nil immediately unless something is recording
// (`OSSignposter.isEnabled`), so a normal run pays one branch.

import os

/// The interactions that are timed. One signpost name per case.
enum PerfInterval {
    case mailboxSwitch   // selection change -> list reloaded (`messages` published)
    case messageOpen     // selection -> body hydrated and handed to the reading pane
    case messageWebView  // HTML body handed to WKWebView -> didFinish
    case composeOpen     // command -> compose window visible
    case send            // Send pressed -> draft saved and handed to the send path (SMTP excluded)
}

/// An open interval. Hold it until the interaction finishes.
struct PerfSpan {
    let kind: PerfInterval
    let state: OSSignpostIntervalState
}

enum PerfSignpost {
    static let signposter = OSSignposter(subsystem: "uk.co.riera.castellum", category: "Perf")

    /// Starts an interval, or returns nil when nothing is recording.
    static func begin(_ kind: PerfInterval) -> PerfSpan? {
        guard signposter.isEnabled else { return nil }
        let id = signposter.makeSignpostID()
        let state: OSSignpostIntervalState
        switch kind {
        case .mailboxSwitch: state = signposter.beginInterval("MailboxSwitch", id: id)
        case .messageOpen: state = signposter.beginInterval("MessageOpen", id: id)
        case .messageWebView: state = signposter.beginInterval("MessageWebView", id: id)
        case .composeOpen: state = signposter.beginInterval("ComposeOpen", id: id)
        case .send: state = signposter.beginInterval("Send", id: id)
        }
        return PerfSpan(kind: kind, state: state)
    }

    /// Ends whatever `slot` holds (a superseded interaction, reported with
    /// count -1) and starts a fresh interval in it.
    static func begin(_ kind: PerfInterval, replacing slot: inout PerfSpan?) {
        end(&slot, count: -1)
        slot = begin(kind)
    }

    /// Ends and clears `slot`. `count` is a row count, or -1 when not
    /// applicable or the interval was abandoned. No-op on an empty slot.
    static func end(_ slot: inout PerfSpan?, count: Int = 0) {
        guard let span = slot else { return }
        slot = nil
        switch span.kind {
        case .mailboxSwitch: signposter.endInterval("MailboxSwitch", span.state, "count=\(count, privacy: .public)")
        case .messageOpen: signposter.endInterval("MessageOpen", span.state, "count=\(count, privacy: .public)")
        case .messageWebView: signposter.endInterval("MessageWebView", span.state, "count=\(count, privacy: .public)")
        case .composeOpen: signposter.endInterval("ComposeOpen", span.state, "count=\(count, privacy: .public)")
        case .send: signposter.endInterval("Send", span.state, "count=\(count, privacy: .public)")
        }
    }
}
