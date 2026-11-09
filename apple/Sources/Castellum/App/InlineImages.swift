// Castellum - inline (cid:) images in HTML mail.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// `<img src="cid:logo123">` points at a MIME part of the same message. The part is an
// ordinary cached attachment (`isInline`, `contentId`), so this resolves the request to
// that attachment, reads (downloading on demand) its bytes, and serves them to the
// locked-down web view. Wall 2 still holds:
//   * only RASTER images are served, decided by sniffing the bytes, never by the part's
//     own Content-Type header (SVG and HTML keep declarative animation / navigation
//     vectors even with JavaScript off, so they get the placeholder);
//   * a part that is missing, too large or fails to download gets the grey placeholder.

import Foundation
import WebKit

/// Pure rules: cid normalisation, request -> part lookup, byte sniffing, strip filtering.
enum InlineImagePolicy {
    /// Largest inline image served (bytes). Anything bigger gets the placeholder.
    static let maxBytes = 12 * 1024 * 1024

    /// "<Logo123>", "cid:logo%31" -> "logo1": strips an optional `cid:` prefix and angle
    /// brackets, percent-decodes. Case is preserved (RFC 2392 compares case-sensitively).
    static func normalise(_ raw: String) -> String {
        var s = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        if s.lowercased().hasPrefix("cid:") { s = String(s.dropFirst(4)) }
        s = s.removingPercentEncoding ?? s
        if s.hasPrefix("<"), s.hasSuffix(">"), s.count >= 2 { s = String(s.dropFirst().dropLast()) }
        return s
    }

    /// The inline part a `cid:` request URL names, if any.
    static func part(forRequest url: URL, in attachments: [AttachmentDescriptor]) -> AttachmentDescriptor? {
        let wanted = normalise(url.absoluteString)
        guard !wanted.isEmpty else { return nil }
        return attachments.first { part in
            guard let cid = part.contentId else { return false }
            return normalise(cid) == wanted
        }
    }

    /// MIME type for raster image bytes by magic number, or nil for anything else
    /// (SVG, HTML, unknown). Never trusts the part header.
    static func rasterMime(for data: Data) -> String? {
        let b = [UInt8](data.prefix(16))
        func starts(_ p: [UInt8], at o: Int = 0) -> Bool {
            b.count >= o + p.count && Array(b[o..<(o + p.count)]) == p
        }
        if starts([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) { return "image/png" }
        if starts([0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if starts(Array("GIF87a".utf8)) || starts(Array("GIF89a".utf8)) { return "image/gif" }
        if starts(Array("RIFF".utf8)) && starts(Array("WEBP".utf8), at: 8) { return "image/webp" }
        if starts(Array("BM".utf8)) { return "image/bmp" }
        if starts([0x49, 0x49, 0x2A, 0x00]) || starts([0x4D, 0x4D, 0x00, 0x2A]) { return "image/tiff" }
        if starts(Array("ftyp".utf8), at: 4),
           ["heic", "heix", "mif1", "msf1", "hevc"].contains(String(decoding: b[8..<min(12, b.count)], as: UTF8.self)) {
            return "image/heic"
        }
        return nil
    }

    /// True when the part is an inline image the HTML body actually references, so the
    /// attachment strip can hide it. Unreferenced inline parts stay listed (nothing becomes
    /// unreachable) and so does everything when there is no HTML body.
    static func isReferencedInline(_ part: AttachmentDescriptor, html: String?) -> Bool {
        guard part.isInline, let cid = part.contentId, let html else { return false }
        let wanted = normalise(cid)
        guard !wanted.isEmpty else { return false }
        var searchRange = html.startIndex..<html.endIndex
        while let hit = html.range(of: "cid:", options: .caseInsensitive, range: searchRange) {
            let tail = html[hit.upperBound...]
            let end = tail.firstIndex { "\"' >)".contains($0) } ?? tail.endIndex
            if normalise(String(tail[..<end])) == wanted { return true }
            searchRange = hit.upperBound..<html.endIndex
        }
        return false
    }
}

/// Where the web view gets inline image bytes for ONE message.
struct InlineImageSource {
    let attachments: [AttachmentDescriptor]
    /// Blocking: returns the part's bytes (reading the cache, downloading when missing) or
    /// nil. Called on a background queue.
    let load: @Sendable (_ ordinal: Int64) -> Data?
}

/// `cid:` scheme handler. Serves the message's inline images when a source is set, the grey
/// placeholder otherwise (compose preview, signature editor, a part that cannot be loaded).
final class CidSchemeHandler: NSObject, WKURLSchemeHandler {
    /// Set on the main thread by the owning web view before each load.
    var source: InlineImageSource?
    /// Tasks WebKit has not stopped yet; answering a stopped task raises an exception.
    private var active: Set<ObjectIdentifier> = []
    private static let queue = DispatchQueue(label: "uk.co.riera.castellum.cid", qos: .userInitiated)

    private static let placeholderSVG = Data("""
    <svg xmlns="http://www.w3.org/2000/svg" width="120" height="80">\
    <rect width="100%" height="100%" fill="#d8dde0"/>\
    <text x="50%" y="50%" text-anchor="middle" dominant-baseline="middle" \
    font-family="sans-serif" font-size="11" fill="#5a6b73">image</text></svg>
    """.utf8)

    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
        let id = ObjectIdentifier(urlSchemeTask as AnyObject)
        active.insert(id)
        let url = urlSchemeTask.request.url ?? URL(string: "cid:placeholder")!
        guard let source, let part = InlineImagePolicy.part(forRequest: url, in: source.attachments) else {
            finish(urlSchemeTask, id: id, url: url, data: nil)
            return
        }
        let ordinal = part.ordinal
        Self.queue.async {
            var data = source.load(ordinal)
            if let d = data, d.count > InlineImagePolicy.maxBytes { data = nil }
            DispatchQueue.main.async { [weak self] in
                self?.finish(urlSchemeTask, id: id, url: url, data: data)
            }
        }
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: WKURLSchemeTask) {
        active.remove(ObjectIdentifier(urlSchemeTask as AnyObject))
    }

    /// Main thread only. Raster bytes are served with a sniffed type; anything else gets the
    /// placeholder.
    private func finish(_ task: WKURLSchemeTask, id: ObjectIdentifier, url: URL, data: Data?) {
        guard active.remove(id) != nil else { return }   // stopped meanwhile
        let body: Data
        let mime: String
        if let data, let sniffed = InlineImagePolicy.rasterMime(for: data) {
            body = data
            mime = sniffed
        } else {
            body = Self.placeholderSVG
            mime = "image/svg+xml"   // our own constant, never message-derived
        }
        task.didReceive(URLResponse(url: url, mimeType: mime, expectedContentLength: body.count, textEncodingName: nil))
        task.didReceive(body)
        task.didFinish()
    }
}
