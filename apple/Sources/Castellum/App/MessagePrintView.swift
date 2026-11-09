// Castellum - print a message (S49): headers block + body + attachment
// list, rendered through the SAME locked-down WKWebView policy as the
// reading pane (Wall 2) — no JavaScript, no persisted state, and remote
// content is ALWAYS blocked here regardless of the reading pane's
// per-message opt-in (a print job must never trigger a silent network
// fetch — roadmap Principle 4).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import WebKit

@MainActor
enum MessagePrint {
    /// Build the printable HTML: a headers table, a rule, the body
    /// (sanitized HTML if present, else the plain text wrapped in <pre>),
    /// and an attachments list. Every interpolated value is escaped.
    ///
    /// `mode` is the reader's View > Header choice (#2703), so the printout
    /// shows the same rows as the screen: compact = From/Date/Subject,
    /// extended = + To/Cc, all = + Bcc/Reply-To/Message-ID. The raw header
    /// block of "All Headers" is not printed: the reader loads it lazily and a
    /// print job must not fetch anything.
    static func printableHTML(detail: MessageDetail, mode: MessageHeaderMode = .defaultMode) -> String {
        func esc(_ s: String) -> String {
            s.replacingOccurrences(of: "&", with: "&amp;")
                .replacingOccurrences(of: "<", with: "&lt;")
                .replacingOccurrences(of: ">", with: "&gt;")
        }
        func row(_ label: String, _ value: String) -> String {
            value.isEmpty ? "" : "<tr><th>\(label)</th><td>\(esc(value))</td></tr>"
        }
        func people(_ kind: String) -> String {
            detail.addresses.filter { $0.kind == kind }
                .map { $0.name ?? $0.address }.joined(separator: ", ")
        }
        let from = [detail.header.fromName, detail.header.fromAddress]
            .compactMap { $0 }.joined(separator: " ")
        let date: String
        if let epoch = detail.header.sentAt {
            let f = DateFormatter()
            f.dateStyle = .full
            f.timeStyle = .short
            date = f.string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
        } else {
            date = ""
        }
        let recipients = mode.recipientKinds.map {
            row(MessageHeaderMode.recipientLabel($0), people($0))
        }
        let technical = mode.showsTechnicalHeaders
            ? [row("Message-ID", detail.rfc822MessageId ?? "")] : []
        let headers = ([row("From", from)] + recipients
            + [row("Date", date), row("Subject", detail.header.subject ?? "")]
            + technical).joined()

        let attachmentsItems = (detail.attachments ?? [])
            .map { "<li>\(esc($0.filename ?? "attachment"))</li>" }
            .joined()
        let attachmentsBlock = attachmentsItems.isEmpty ? "" : "<h3>Attachments</h3><ul>\(attachmentsItems)</ul>"

        let body: String
        if let html = detail.sanitizedHtml, !html.isEmpty {
            body = html
        } else {
            body = "<pre>\(esc(detail.textBody ?? ""))</pre>"
        }

        return """
        <!DOCTYPE html><html><head><meta charset="utf-8"><style>
        body { font: -apple-system-body; font-family: -apple-system, sans-serif; margin: 24px; }
        table { border-collapse: collapse; margin-bottom: 16px; }
        th { text-align: right; padding: 2px 8px; color: #555; font-weight: 600; vertical-align: top; white-space: nowrap; }
        td { padding: 2px 0; }
        hr { border: none; border-top: 1px solid #ccc; margin: 16px 0; }
        pre { white-space: pre-wrap; font-family: -apple-system, sans-serif; }
        </style></head><body>
        <table>\(headers)</table><hr>\(body)\(attachmentsBlock)
        </body></html>
        """
    }
}

/// One-shot, off-screen `WKWebView` that loads the printable HTML with the
/// same block-everything content rule list as the reading pane's default
/// (`EmailBodyWebView.blockAllRules`), then hands AppKit a real
/// `NSPrintOperation`. Retains itself (via reference-identity Hashable,
/// same as `StandaloneMessageWindow`) until printing completes.
@MainActor
final class MessagePrintController: NSObject, WKNavigationDelegate {
    private static var active: Set<MessagePrintController> = []
    private let webView: WKWebView

    static func printMessage(_ detail: MessageDetail, mode: MessageHeaderMode = .defaultMode) {
        let controller = MessagePrintController(detail: detail, mode: mode)
        active.insert(controller)
    }

    private init(detail: MessageDetail, mode: MessageHeaderMode) {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        webView = WKWebView(frame: NSRect(x: 0, y: 0, width: 700, height: 900), configuration: config)
        super.init()
        webView.navigationDelegate = self
        WKContentRuleListStore.default().compileContentRuleList(
            forIdentifier: "castellum-print-blockall",
            encodedContentRuleList: EmailBodyWebView.blockAllRules
        ) { [weak self] list, error in
            guard let self else { return }
            // Fail CLOSED (audit #3373): the block-all rule list is the only wall
            // against remote images / CSS url() fetches (Wall 1 lets http(s) img
            // src and style url() through), so without it nothing is loaded and
            // nothing is printed.
            guard let list, error == nil else {
                Self.active.remove(self)
                let alert = NSAlert()
                alert.messageText = String(localized: "Can't print this message safely")
                alert.informativeText = String(localized: "Castellum could not set up its remote-content protection, so it did not print. Try again after restarting the app.")
                alert.runModal()
                return
            }
            self.webView.configuration.userContentController.add(list)
            self.webView.loadHTMLString(MessagePrint.printableHTML(detail: detail, mode: mode), baseURL: nil)
        }
    }

    /// Print HTML never needs to navigate: the only allowed load is the
    /// initial `loadHTMLString` (about:blank); link clicks and redirects are
    /// refused so a printed message can't open or fetch anything.
    func webView(
        _ webView: WKWebView,
        decidePolicyFor navigationAction: WKNavigationAction,
        decisionHandler: @escaping @MainActor @Sendable (WKNavigationActionPolicy) -> Void
    ) {
        let allowed = navigationAction.navigationType == .other
            && (navigationAction.request.url == nil
                || navigationAction.request.url?.absoluteString == "about:blank")
        decisionHandler(allowed ? .allow : .cancel)
    }

    func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
        let op = webView.printOperation(with: NSPrintInfo.shared)
        op.canSpawnSeparateThread = true
        _ = op.run()
        Self.active.remove(self)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        Self.active.remove(self)
    }
}
