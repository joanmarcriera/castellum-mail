// Castellum - pure helpers for inline images in Markdown mail.
//
// The core owns the durable side (`castellum_embed_draft_image`: type from the bytes, generated
// Content-ID, `![alt](cid:ID)` returned); this file only holds the Swift-side rules that need no
// UI: which picked files are tried as inline images, how a reference is found and removed from
// the body, and the one-line text shown when the core falls back to a regular attachment.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import AppKit
import Foundation
import ImageIO
import UniformTypeIdentifiers

enum ComposeInlineImages {
    /// Largest inline image (decimal MB), the core's cap; a bigger file is attached normally.
    static let maxBytes = 10 * 1_000_000

    /// Whether a picked or dropped file is worth offering to the core as an inline image: an
    /// image type by extension. The core's own sniffing (PNG, JPEG, GIF, WebP) has the last word.
    static func looksLikeImage(_ url: URL) -> Bool {
        UTType(filenameExtension: url.pathExtension)?.conforms(to: .image) ?? false
    }

    /// Every `![alt](cid:<contentId>)` in `body` removed (the Content-ID is matched exactly, so
    /// another image's reference is never touched). A reference alone on its line leaves no blank line behind.
    static func removingReferences(to contentId: String, from body: String) -> String {
        let escaped = NSRegularExpression.escapedPattern(for: contentId)
        guard let re = try? NSRegularExpression(
            pattern: "(?:^|(?<=\\n))[ \\t]*!\\[[^\\]\\n]*\\]\\(cid:\(escaped)\\)[ \\t]*(?:\\n|$)|!\\[[^\\]\\n]*\\]\\(cid:\(escaped)\\)")
        else { return body }
        let range = NSRange(body.startIndex..., in: body)
        return re.stringByReplacingMatches(in: body, options: [], range: range, withTemplate: "")
    }

    /// True when `body` still references `contentId`.
    static func references(_ contentId: String, in body: String) -> Bool {
        body.contains("](cid:\(contentId))")
    }

    /// HEIC, TIFF and BMP pictures are re-encoded as PNG before embedding (the core only embeds
    /// PNG, JPEG, GIF and WebP, which every mail client shows). Anything else, or a picture that
    /// cannot be decoded, comes back unchanged (the core then refuses it and it is attached as a file).
    static func preparedForEmbedding(_ data: Data, filename: String) -> (data: Data, filename: String) {
        guard let mime = InlineImagePolicy.rasterMime(for: data),
              ["image/heic", "image/tiff", "image/bmp"].contains(mime),
              let source = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
              let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])
        else { return (data, filename) }
        let stem = (filename as NSString).deletingPathExtension
        return (png, (stem.isEmpty ? "Image" : stem) + ".png")
    }

    /// The note shown when "Insert in Body" is refused (the row stays a regular attachment).
    static func cannotInsertNote(filename: String, reason: String?) -> String {
        if let reason, reason.hasPrefix("too_large") {
            return String(localized: "\(filename) is over 10 MB, so it can’t be put in the message body.")
        }
        return String(localized: "\(filename) isn’t a PNG, JPEG, GIF or WebP, so it can’t be put in the message body.")
    }

    /// The one-line note shown when an image could not be embedded and was attached as a file,
    /// from the core's refusal text (`not_an_image:` / `too_large:`).
    static func fallbackNote(filename: String, reason: String?) -> String {
        if let reason, reason.hasPrefix("too_large") {
            return String(localized: "\(filename) is over 10 MB, so it was attached as a file instead of shown in the message.")
        }
        return String(localized: "\(filename) isn’t a PNG, JPEG, GIF or WebP, so it was attached as a file instead of shown in the message.")
    }
}
