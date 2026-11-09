// Castellum - locked-down WKWebView for sanitized email HTML (Wall 2).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// The Rust core already sanitized the HTML (Wall 1). This view enforces the
// CAPABILITY policy: no JavaScript, no persisted state, no navigation, and a
// WKContentRuleList that blocks network subresources unless the user opted
// into remote images. The page's own script never runs; two passive handlers live in an
// isolated content world: link hover (S48) and the text selection (MM-17, read-only). Fail closed: if a rule list cannot be compiled, the
// caller is told to fall back to the text body — never render unrestricted.

import SwiftUI
import WebKit

/// One hovered link's raw payload from the hover script (S48). View-relative
/// point, for positioning the reveal popover near the cursor.
struct LinkHoverInfo: Equatable {
    let href: String
    let text: String
    let point: CGPoint
}

/// What the navigation delegate should do with a navigation action.
enum BodyNavigationDecision: Equatable {
    case allowInitialLoad
    case openExternally(URL)
    case cancel
}

/// Pure policy: only the initial loadHTMLString may proceed; link activations
/// with web/mail schemes open in the default browser; everything else dies.
func decideBodyNavigation(
    hasCompletedInitialLoad: Bool,
    navigationType: WKNavigationType,
    url: URL?
) -> BodyNavigationDecision {
    if !hasCompletedInitialLoad && navigationType == .other {
        return .allowInitialLoad
    }
    if navigationType == .linkActivated,
       let url,
       ["http", "https", "mailto"].contains(url.scheme?.lowercased() ?? "") {
        return .openExternally(url)
    }
    return .cancel
}

/// Wrap the sanitized body in a minimal document. The body slot is the ONLY
/// interpolation — never add another (that would bypass Wall 1).
func emailHTMLShell(body: String) -> String {
    """
    <!DOCTYPE html>
    <html>
    <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <style>
      :root { color-scheme: light dark; }
      body {
        font: -apple-system-body;
        font-family: -apple-system, sans-serif;
        margin: 0; padding: 16px;
        overflow-wrap: break-word;
      }
      img { max-width: 100%; height: auto; }
      table { max-width: 100%; }
      pre { white-space: pre-wrap; }
    </style>
    </head>
    <body>\(body)</body>
    </html>
    """
}

struct EmailBodyWebView: NSViewRepresentable {
    /// Block every network subresource (default: remote images off).
    ///
    /// `cid:` is exempt (the last rule): it is not a network scheme, it is answered ONLY by the
    /// app's own `CidSchemeHandler`, which serves raster bytes from the message's own parts or a
    /// constant placeholder. Without the exemption the block-all rule swallowed every `cid:` request
    /// before it reached the handler, so neither the reader's inline images (#3442) nor the compose
    /// preview's embedded images (#2712) ever displayed (`CidPreviewWebViewTests`).
    static let blockAllRules = #"""
    [{"trigger":{"url-filter":".*"},"action":{"type":"block"}},
     {"trigger":{"url-filter":"^cid:"},"action":{"type":"ignore-previous-rules"}}]
    """#
    /// User opted in: allow image loads only — still no fonts/media/fetches.
    static let imagesAllowedRules = #"""
    [{"trigger":{"url-filter":".*"},"action":{"type":"block"}},
     {"trigger":{"url-filter":".*","resource-type":["image"]},"action":{"type":"ignore-previous-rules"}},
     {"trigger":{"url-filter":"^cid:"},"action":{"type":"ignore-previous-rules"}}]
    """#

    /// Same isolated world, second passive handler (MM-17, #2807): posts the page's CURRENT TEXT
    /// SELECTION (debounced, capped at 200 kB) so Reply can quote just that part of an HTML message.
    /// It only reads `window.getSelection()`; it never touches the DOM or calls out.
    static let selectionScript = #"""
    (function () {
      var timer = null;
      document.addEventListener('selectionchange', function () {
        if (timer) { clearTimeout(timer); }
        timer = setTimeout(function () {
          var t = String(window.getSelection() || '');
          if (t.length > 200000) { t = t.substring(0, 200000); }
          window.webkit.messageHandlers.castellumSelection.postMessage(t);
        }, 150);
      });
    })();
    """#

    /// Runs in an isolated WKContentWorld (its own JS globals, never shared
    /// with page-authored script — moot here anyway since Wall 1 already
    /// strips every <script> tag before this HTML exists, but isolation
    /// keeps this script's own state private regardless). Passive listeners
    /// only: reads `href`/`textContent`/pointer position on hover, posts
    /// them to Swift, clears on mouseout. No eval, no DOM mutation, no
    /// access to anything the page itself defines. Empirically confirmed
    /// (S48 Task 9 Step 1) to run even with
    /// `allowsContentJavaScript = false` — see the plan's Deviations list.
    static let linkHoverScript = #"""
    (function () {
      function onOver(e) {
        var a = e.target.closest('a[href]');
        if (!a) { return; }
        window.webkit.messageHandlers.castellumLinkHover.postMessage({
          href: a.getAttribute('href') || '',
          text: a.textContent || '',
          x: e.clientX,
          y: e.clientY
        });
      }
      function onOut(e) {
        var a = e.target.closest('a[href]');
        if (!a) { return; }
        window.webkit.messageHandlers.castellumLinkHover.postMessage(null);
      }
      document.addEventListener('mouseover', onOver, true);
      document.addEventListener('mouseout', onOut, true);
    })();
    """#

    static let hoverContentWorld = WKContentWorld.world(name: "castellum-link-hover")

    let sanitizedHtml: String
    let allowRemoteImages: Bool
    /// Called on rule-list compile failure or repeated web-process crash;
    /// the parent must fall back to the text body (fail closed).
    let onUnrecoverableFailure: () -> Void
    /// Called with the hovered link's info, or `nil` on mouseout / no link.
    var onLinkHover: ((LinkHoverInfo?) -> Void)?
    /// Called with the selected text whenever the selection changes ("" = cleared). MM-17.
    var onSelectionChange: ((String) -> Void)?
    /// This message's inline (cid:) images; nil serves the grey placeholder (#3442).
    var inlineImages: InlineImageSource?

    func makeCoordinator() -> Coordinator { Coordinator(parent: self) }

    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        config.defaultWebpagePreferences.allowsContentJavaScript = false
        let cidHandler = CidSchemeHandler()
        cidHandler.source = inlineImages
        context.coordinator.cidHandler = cidHandler
        config.setURLSchemeHandler(cidHandler, forURLScheme: "cid")
        config.userContentController.addUserScript(
            WKUserScript(
                source: Self.linkHoverScript,
                injectionTime: .atDocumentEnd,
                forMainFrameOnly: true,
                in: Self.hoverContentWorld
            )
        )
        config.userContentController.add(
            context.coordinator,
            contentWorld: Self.hoverContentWorld,
            name: "castellumLinkHover"
        )
        config.userContentController.addUserScript(
            WKUserScript(
                source: Self.selectionScript,
                injectionTime: .atDocumentEnd,
                forMainFrameOnly: true,
                in: Self.hoverContentWorld
            )
        )
        config.userContentController.add(
            context.coordinator,
            contentWorld: Self.hoverContentWorld,
            name: "castellumSelection"
        )

        let webView = WKWebView(frame: .zero, configuration: config)
        webView.navigationDelegate = context.coordinator
        webView.setValue(false, forKey: "drawsBackground") // blend with the pane
        context.coordinator.compileRulesAndLoad(into: webView, parent: self)
        return webView
    }

    func updateNSView(_ webView: WKWebView, context: Context) {
        context.coordinator.cidHandler?.source = inlineImages
        let changed = context.coordinator.loadedHtml != sanitizedHtml
            || context.coordinator.loadedAllowRemote != allowRemoteImages
        if changed {
            context.coordinator.hasCompletedInitialLoad = false
            context.coordinator.compileRulesAndLoad(into: webView, parent: self)
        }
    }

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate, WKScriptMessageHandler {
        var parent: EmailBodyWebView
        var hasCompletedInitialLoad = false
        /// Serves `cid:` requests; its source is refreshed on every `updateNSView`.
        var cidHandler: CidSchemeHandler?
        /// M3: HTML handed to WKWebView -> didFinish (PerfSignpost.swift).
        private var loadSpan: PerfSpan?
        var loadedHtml: String?
        var loadedAllowRemote: Bool?
        private var crashReloads = 0
        /// Two in-flight compiles (message switch = blockall ⇄ images) can
        /// complete out of order; only the newest may install rules + load.
        private var ruleLoadGeneration = 0

        init(parent: EmailBodyWebView) { self.parent = parent }

        func compileRulesAndLoad(into webView: WKWebView, parent: EmailBodyWebView) {
            self.parent = parent
            loadedHtml = parent.sanitizedHtml
            loadedAllowRemote = parent.allowRemoteImages
            let rules = parent.allowRemoteImages
                ? EmailBodyWebView.imagesAllowedRules
                : EmailBodyWebView.blockAllRules
            let identifier = parent.allowRemoteImages ? "castellum-images-v2" : "castellum-blockall-v2"
            ruleLoadGeneration &+= 1
            let generation = ruleLoadGeneration
            WKContentRuleListStore.default().compileContentRuleList(
                forIdentifier: identifier,
                encodedContentRuleList: rules
            ) { [weak self, weak webView] list, error in
                Task { @MainActor in
                    guard let self, let webView, self.ruleLoadGeneration == generation else {
                        return // superseded by a newer message/policy load
                    }
                    guard let list, error == nil else {
                        // Fail CLOSED: no rule list means no webview.
                        PerfSignpost.end(&self.loadSpan, count: -1)
                        self.parent.onUnrecoverableFailure()
                        return
                    }
                    webView.configuration.userContentController.removeAllContentRuleLists()
                    webView.configuration.userContentController.add(list)
                    PerfSignpost.begin(.messageWebView, replacing: &self.loadSpan)
                    webView.loadHTMLString(emailHTMLShell(body: self.parent.sanitizedHtml), baseURL: nil)
                }
            }
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor navigationAction: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            switch decideBodyNavigation(
                hasCompletedInitialLoad: hasCompletedInitialLoad,
                navigationType: navigationAction.navigationType,
                url: navigationAction.request.url
            ) {
            case .allowInitialLoad:
                decisionHandler(.allow)
            case .openExternally(let url):
                NSWorkspace.shared.open(url)
                decisionHandler(.cancel)
            case .cancel:
                decisionHandler(.cancel)
            }
        }

        func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            hasCompletedInitialLoad = true
            PerfSignpost.end(&loadSpan)
        }

        /// A failed load never reaches `didFinish`: close the interval (-1).
        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            PerfSignpost.end(&loadSpan, count: -1)
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!,
                     withError error: Error) {
            PerfSignpost.end(&loadSpan, count: -1)
        }

        func webViewWebContentProcessDidTerminate(_ webView: WKWebView) {
            crashReloads += 1
            PerfSignpost.end(&loadSpan, count: -1)
            if crashReloads <= 1 {
                hasCompletedInitialLoad = false
                compileRulesAndLoad(into: webView, parent: parent)
            } else {
                parent.onUnrecoverableFailure()
            }
        }

        func userContentController(_ controller: WKUserContentController, didReceive message: WKScriptMessage) {
            if message.name == "castellumSelection" {
                parent.onSelectionChange?((message.body as? String) ?? "")
                return
            }
            guard message.name == "castellumLinkHover" else { return }
            guard let body = message.body as? [String: Any],
                  let href = body["href"] as? String,
                  let x = body["x"] as? Double,
                  let y = body["y"] as? Double
            else {
                parent.onLinkHover?(nil) // mouseout posts `null`, decoded here as "clear"
                return
            }
            let text = body["text"] as? String ?? ""
            parent.onLinkHover?(LinkHoverInfo(href: href, text: text, point: CGPoint(x: x, y: y)))
        }
    }
}
