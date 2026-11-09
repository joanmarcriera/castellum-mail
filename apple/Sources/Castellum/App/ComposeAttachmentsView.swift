// Castellum - compose attachment strip (S33): file picker, drag-drop, paste,
// total-size warning.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// With Markdown OFF, compose is plain text, so a pasted or dropped image is a REGULAR
// attachment: there is nowhere in a plain-text body to put a `cid:` reference. With Markdown
// ON (MM-5b), an image is embedded at the caret as `![name](cid:ID)` and sent inside the
// message (`ComposeSession.attach`/`embedImage`); anything the core cannot embed attaches
// normally. Forwarded originals keep their inline flag and Content-ID in the core (see
// `castellum_forward_attachments`), which is what makes forwarded HTML mail's inline images
// keep working.

import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Warn (never block: server limits vary, this is a heads-up) once the total
/// size of a message's attachments passes 25 MB (decimal, as mail providers
/// quote their limits).
let ATTACHMENT_SIZE_WARNING_BYTES: Int64 = 25 * 1_000_000

/// A single file above this size is refused outright rather than base64-encoded
/// into memory for the FFI's JSON-string contract.
let ATTACHMENT_MAX_FILE_BYTES: Int64 = 50 * 1_000_000

func exceedsAttachmentSizeWarning(totalBytes: Int64) -> Bool {
    totalBytes > ATTACHMENT_SIZE_WARNING_BYTES
}

/// User-facing size-warning text (British English), or `nil` while under the limit.
func attachmentSizeWarning(totalBytes: Int64) -> String? {
    guard exceedsAttachmentSizeWarning(totalBytes: totalBytes) else { return nil }
    let size = ByteCountFormatter.string(fromByteCount: totalBytes, countStyle: .file)
    return "Attachments total \(size). Some mail servers reject messages this large."
}

/// Why an attachment could not be prepared.
enum ComposeAttachmentError: Error, Equatable {
    case unreadable(String)
    case tooLarge(String)

    var message: String {
        switch self {
        case .unreadable(let name): return "Couldn't read \(name)."
        case .tooLarge(let name):
            return "\(name) is too large to attach (over \(ATTACHMENT_MAX_FILE_BYTES / 1_000_000) MB)."
        }
    }
}

/// Pure builders for `NewDraftAttachment`, kept out of the view so they are testable.
enum ComposeAttachmentBuilder {
    /// Read `url` and wrap it as an attachment. The MIME type comes from the
    /// filename extension, falling back to `application/octet-stream`.
    static func make(fileURL url: URL) throws -> NewDraftAttachment {
        let name = url.lastPathComponent
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        if let size = (try? url.resourceValues(forKeys: [.fileSizeKey]))?.fileSize,
           Int64(size) > ATTACHMENT_MAX_FILE_BYTES {
            throw ComposeAttachmentError.tooLarge(name)
        }
        guard let data = try? Data(contentsOf: url) else {
            throw ComposeAttachmentError.unreadable(name)
        }
        guard Int64(data.count) <= ATTACHMENT_MAX_FILE_BYTES else {
            throw ComposeAttachmentError.tooLarge(name)
        }
        let mime = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType
            ?? "application/octet-stream"
        return NewDraftAttachment(
            filename: name, contentType: mime, isInline: false, contentId: nil,
            dataBase64: data.base64EncodedString())
    }

    /// Wrap raw pasted image bytes (a screenshot, an image copied from a
    /// browser: no file behind them) as a regular attachment.
    static func make(pastedImage data: Data, contentType: String,
                     now: Date = Date()) throws -> NewDraftAttachment {
        let name = "Pasted Image"
        guard Int64(data.count) <= ATTACHMENT_MAX_FILE_BYTES else {
            throw ComposeAttachmentError.tooLarge(name)
        }
        let ext = UTType(mimeType: contentType)?.preferredFilenameExtension ?? "png"
        let stamp = Int(now.timeIntervalSince1970)
        return NewDraftAttachment(
            filename: "\(name) \(stamp).\(ext)", contentType: contentType, isInline: false,
            contentId: nil, dataBase64: data.base64EncodedString())
    }
}

/// What a paste (⌘V) should attach, read from a pasteboard.
enum PasteboardAttachments {
    enum Item: Equatable {
        case file(URL)
        case image(Data, contentType: String)
    }

    /// Files copied in Finder become file attachments. A copied image with no
    /// accompanying text (a screenshot, ⌘⇧⌃4) becomes an image attachment.
    /// Anything that carries text (including rich content that also has an
    /// image flavour) is left alone so ordinary text paste keeps working.
    static func items(from pasteboard: NSPasteboard) -> [Item] {
        if let urls = pasteboard.readObjects(
            forClasses: [NSURL.self],
            options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty {
            return urls.map { .file($0) }
        }
        if pasteboard.string(forType: .string) != nil { return [] }
        if let png = pasteboard.data(forType: .png) {
            return [.image(png, contentType: "image/png")]
        }
        if let tiff = pasteboard.data(forType: .tiff),
           let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) {
            return [.image(png, contentType: "image/png")]
        }
        return []
    }
}

struct ComposeAttachmentsView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var session: ComposeSession

    private var totalBytes: Int64 {
        session.attachments.reduce(0) { $0 + $1.sizeBytes }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(session.attachments) { attachment in
                HStack(spacing: 8) {
                    Image(systemName: attachment.isInline ? "photo" : "paperclip")
                        .foregroundStyle(.secondary)
                    Text(attachment.filename).font(.callout).lineLimit(1)
                    Text(ByteCountFormatter.string(fromByteCount: attachment.sizeBytes, countStyle: .file))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button {
                        session.removeAttachment(id: attachment.id, model: model)
                    } label: {
                        Image(systemName: "xmark.circle.fill")
                    }
                    .buttonStyle(.borderless)
                    .help("Remove attachment")
                    .disabled(session.queuedInOutbox)
                }
                .contextMenu {
                    // MM-5b slice 3: only with Markdown on, where there is an HTML part to reference it from.
                    if session.markdownEnabled, !session.queuedInOutbox, !session.isSending {
                        if attachment.isInline {
                            Button(String(localized: "Keep as Attachment Only")) {
                                session.keepAttachmentOnly(id: attachment.id, model: model)
                            }
                        } else if ComposeInlineImages.looksLikeImage(URL(fileURLWithPath: attachment.filename)) {
                            Button(String(localized: "Insert in Body")) {
                                session.insertAttachmentInBody(id: attachment.id, model: model)
                            }
                        }
                    }
                }
            }
            if !session.markdownEnabled, session.attachments.contains(where: { $0.isInline && ($0.contentId ?? "").hasSuffix("@castellum") }) {
                Text("Markdown is off, so pictures embedded in this message are sent as attachments, not in place.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let warning = attachmentSizeWarning(totalBytes: totalBytes) {
                Label(warning, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            HStack {
                Button {
                    presentFilePicker()
                } label: {
                    Label("Attach File…", systemImage: "paperclip")
                }
                .buttonStyle(.borderless)
                .disabled(session.queuedInOutbox || session.isSending)
                if session.markdownEnabled {
                    // MM-5b: with Markdown on, an image is embedded at the caret in the body.
                    Button {
                        presentFilePicker(imagesOnly: true)
                    } label: {
                        Label("Insert Image…", systemImage: "photo")
                    }
                    .buttonStyle(.borderless)
                    .disabled(session.queuedInOutbox || session.isSending)
                    .accessibilityIdentifier("compose.insertImage")
                    .help(String(localized: "Put a picture in the message body (PNG, JPEG, GIF or WebP). It is sent inside the message, not fetched from the web."))
                }
                if let error = session.attachmentError {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
            }
        }
        .onAppear { session.loadAttachments(model: model) }
    }

    private func presentFilePicker(imagesOnly: Bool = false) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        if imagesOnly { panel.allowedContentTypes = [.image] }
        if panel.runModal() == .OK {
            session.attach(fileURLs: panel.urls, model: model)
        }
    }
}

extension View {
    /// Make the whole compose window a drop target for files, and route ⌘V of
    /// Finder files or a bare image to attachments (see `PasteboardAttachments`).
    func composeAttachmentInput(model: AppModel, session: ComposeSession) -> some View {
        self
            .onDrop(of: [.fileURL], isTargeted: nil) { providers in
                guard !session.queuedInOutbox, !session.isSending else { return false }
                for provider in providers {
                    _ = provider.loadObject(ofClass: URL.self) { url, _ in
                        guard let url else { return }
                        Task { @MainActor in session.attach(fileURLs: [url], model: model) }
                    }
                }
                return true
            }
            .background(PasteInterceptor(session: session, model: model))
    }
}

/// Intercepts ⌘V while THIS compose window is key, when (and only when) the
/// pasteboard holds Finder files or a bare image. A text editor would otherwise
/// swallow those flavours (or paste a file path), and SwiftUI's
/// `onPasteCommand` does not fire while a `TextEditor` has focus.
private struct PasteInterceptor: NSViewRepresentable {
    let session: ComposeSession
    let model: AppModel

    func makeNSView(context: Context) -> NSView { NSView(frame: .zero) }

    func updateNSView(_ view: NSView, context: Context) {
        context.coordinator.session = session
        context.coordinator.model = model
        context.coordinator.hostView = view
        context.coordinator.installIfNeeded()
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        coordinator.remove()
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    @MainActor
    final class Coordinator {
        var session: ComposeSession?
        var model: AppModel?
        weak var hostView: NSView?
        private var monitor: Any?

        func installIfNeeded() {
            guard monitor == nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
                guard let self, self.handle(event) else { return event }
                return nil
            }
        }

        func remove() {
            if let monitor { NSEvent.removeMonitor(monitor) }
            monitor = nil
        }

        /// True when the event was a paste we turned into attachments.
        private func handle(_ event: NSEvent) -> Bool {
            guard event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
                  event.charactersIgnoringModifiers == "v",
                  let window = hostView?.window, window.isKeyWindow,
                  let session, let model,
                  !session.queuedInOutbox, !session.isSending else { return false }
            let items = PasteboardAttachments.items(from: .general)
            guard !items.isEmpty else { return false }
            for item in items {
                switch item {
                case .file(let url):
                    session.attach(fileURLs: [url], model: model)
                case .image(let data, let contentType):
                    session.attachPastedImage(data: data, contentType: contentType, model: model)
                }
            }
            return true
        }
    }
}
