// Castellum - the locked-down web view still reaches the `cid:` scheme handler (MM-5b, #2712).
//
// The compose preview's embedded images (and the reader's inline images, #3442) are served
// through `CidSchemeHandler`. The block-all content rule used to swallow every `cid:` request
// before it got there; it now exempts `cid:` and nothing else.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import WebKit
import XCTest
@testable import Castellum

@MainActor
final class CidPreviewWebViewTests: XCTestCase {
    private func cidHandler() -> CidSchemeHandler {
        let handler = CidSchemeHandler()
        let png = TestPNG.data
        handler.source = InlineImageSource(
            attachments: [AttachmentDescriptor(
                ordinal: 1, filename: "p.png", contentType: "image/png", sizeBytes: Int64(png.count),
                section: "", contentId: "img-aaaaaaaaaaaa@castellum", isInline: true, transferEncoding: nil)],
            load: { _ in png })
        return handler
    }

    private func webView(rules encoded: String?, handler: CidSchemeHandler? = nil, scheme: String = "cid") async throws -> WKWebView {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        if let handler { config.setURLSchemeHandler(handler, forURLScheme: scheme) }
        if let encoded {
            let rules = try await WKContentRuleListStore.default().compileContentRuleList(
                forIdentifier: "castellum-cid-test-\(UUID().uuidString)", encodedContentRuleList: encoded)
            config.userContentController.add(try XCTUnwrap(rules))
        }
        return WKWebView(frame: NSRect(x: 0, y: 0, width: 300, height: 200), configuration: config)
    }

    /// Whether the image decoded (natural width above zero) within a few seconds.
    private func loads(_ src: String, in web: WKWebView) async throws -> Bool {
        web.loadHTMLString(emailHTMLShell(body: "<img id=\"i\" src=\"\(src)\" alt=\"x\">"), baseURL: nil)
        let deadline = Date().addingTimeInterval(4)
        while Date() < deadline {
            try await Task.sleep(nanoseconds: 100_000_000)
            let ok = (try? await web.callAsyncJavaScript(
                "var i = document.getElementById('i'); return i ? (i.complete && i.naturalWidth > 0) : false",
                contentWorld: .page)) as? Bool
            if ok == true { return true }
        }
        return false
    }

    func testControlWithNoRuleListTheHandlerServesTheImage() async throws {
        let web = try await webView(rules: nil, handler: cidHandler())
        let loaded = try await loads("cid:img-aaaaaaaaaaaa@castellum", in: web)
        XCTAssertTrue(loaded, "control: without a rule the handler serves it")
    }

    func testTheBlockAllRuleDoesNotStopCidRequestsReachingTheHandler() async throws {
        let web = try await webView(rules: EmailBodyWebView.blockAllRules, handler: cidHandler())
        let loaded = try await loads("cid:img-aaaaaaaaaaaa@castellum", in: web)
        XCTAssertTrue(loaded, "the 2x2 PNG was served through the cid: handler under the block-all rule")
    }

    func testTheImagesAllowedRuleAlsoReachesTheCidHandler() async throws {
        let web = try await webView(rules: EmailBodyWebView.imagesAllowedRules, handler: cidHandler())
        let loaded = try await loads("cid:img-aaaaaaaaaaaa@castellum", in: web)
        XCTAssertTrue(loaded)
    }

    /// The exemption is for `cid:` only: another custom scheme answered by a handler (it serves the
    /// placeholder image) is still blocked under block-all, and loads when no rule is installed.
    func testBlockAllStillBlocksOtherSchemes() async throws {
        let blocked = try await loads(
            "other:x", in: try await webView(rules: EmailBodyWebView.blockAllRules, handler: cidHandler(), scheme: "other"))
        XCTAssertFalse(blocked, "a non-cid scheme is still blocked")
        let control = try await loads(
            "other:x", in: try await webView(rules: nil, handler: cidHandler(), scheme: "other"))
        XCTAssertTrue(control, "control: the same request is answered when no rule is installed")
    }
}
