// Castellum - incoming attachment strip (S32): open, Save As, drag-out,
// Quick Look, safe filenames, no auto-open of executables.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Trust boundary: the on-disk cache (Rust-owned, `attachments-cache/<sha256>`)
// holds extension-less, hash-named files that are never handed to the system
// directly. Every action that leaves the cache — Open, Quick Look, drag-out,
// Save As — goes through a COPY that carries the sanitised original filename
// (so LaunchServices/Quick Look can pick a handler by extension) and the
// `com.apple.quarantine` attribute (so Gatekeeper treats it like a browser
// download). Single-click Open refuses executables/installers outright.

import CoreServices
import SwiftUI
import UniformTypeIdentifiers

// MARK: - Pure helpers (unit-tested in AttachmentDTOTests)

/// Longest filename (UTF-8 bytes) we ever write; APFS/HFS+ allow 255, leave
/// headroom for the collision suffixes Finder adds.
private let maxAttachmentFilenameBytes = 200

/// Strip a sender-controlled `Content-Disposition: filename=` down to a safe
/// single path component: last component only (both `/` and `\` count as
/// separators), no control characters, no leading dots (no hidden files, no
/// `.`/`..`), capped in length with the extension preserved. Falls back to
/// "attachment" (extension-less; the caller's content type still governs how
/// NSWorkspace/Quick Look treat the bytes).
func sanitizedAttachmentFilename(_ raw: String?) -> String {
    guard let raw, !raw.isEmpty else { return "attachment" }
    let separatorsUnified = raw.replacingOccurrences(of: "\\", with: "/")
    let printable = String(String.UnicodeScalarView(
        separatorsUnified.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }
    ))
    // `controlCharacters` already covers Cc and Cf (bidi overrides, zero-width,
    // BOM). What survives are blank look-alikes and whitespace padding that
    // can push the real extension out of sight (`invoice.pdf<150 spaces>.scr`,
    // #3379): turn them all into plain spaces, collapse runs, and tidy the
    // spaces around the final dot.
    let blankLookalikes: Set<UInt32> = [0x2800, 0x115F, 0x1160, 0x3164, 0xFFA0]
    let spaced = String(String.UnicodeScalarView(printable.unicodeScalars.map {
        (CharacterSet.whitespacesAndNewlines.contains($0) || blankLookalikes.contains($0.value))
            ? " " : $0
    }))
    var collapsed = ""
    for ch in spaced where !(ch == " " && collapsed.last == " ") { collapsed.append(ch) }
    let lastComponent = tightenFinalDot(
        (collapsed as NSString).lastPathComponent.trimmingCharacters(in: .whitespacesAndNewlines))
    let noLeadingDots = String(lastComponent.drop(while: { $0 == "." }))
        .trimmingCharacters(in: .whitespacesAndNewlines)
    if noLeadingDots.isEmpty { return "attachment" }
    return capFilename(noLeadingDots, maxBytes: maxAttachmentFilenameBytes)
}

/// `invoice.pdf .scr` -> `invoice.pdf.scr`: drop spaces hugging the last dot.
private func tightenFinalDot(_ name: String) -> String {
    guard let dot = name.lastIndex(of: ".") else { return name }
    let before = name[..<dot].trimmingCharacters(in: .whitespaces)
    let after = name[name.index(after: dot)...].trimmingCharacters(in: .whitespaces)
    return before + "." + after
}

/// Truncate `name` to `maxBytes` UTF-8 bytes, keeping a short extension.
private func capFilename(_ name: String, maxBytes: Int) -> String {
    guard name.utf8.count > maxBytes else { return name }
    let ext = (name as NSString).pathExtension
    let suffix = (ext.isEmpty || ext.utf8.count > 20) ? "" : "." + ext
    var base = String(name.dropLast(suffix.count))
    while (base + suffix).utf8.count > maxBytes, !base.isEmpty {
        base.removeLast()
    }
    return base.isEmpty ? "attachment" + suffix : base + suffix
}

/// Executable/installer denylist for the single-click "Open" action: a
/// filename-extension list plus UTType conformance (of the extension and,
/// when given, the declared MIME type). Save As and drag-out remain available
/// regardless — those are the user's own explicit action, and both write a
/// quarantined file.
func isBlockedFromAutoOpen(filename: String, contentType: String? = nil) -> Bool {
    let blockedExtensions: Set<String> = [
        "command", "tool", "sh", "scpt", "scptd", "workflow", "action",
        "pkg", "mpkg", "dmg", "app", "exe", "msi", "bat", "cmd", "jar",
        // Settings / location files that run commands or open network
        // locations when opened (#3378), plus plug-in bundles and other
        // image/installer kinds that resolve to dynamic types.
        "terminal", "inetloc", "fileloc", "webloc", "url", "scr", "osax", "saver",
        "prefpane", "kext", "qlgenerator", "plugin", "bundle", "appex", "xip", "iso",
        "sparseimage", "sparsebundle", "jnlp", "mobileconfig", "shortcut", "wflow", "fish",
    ]
    // Types that conform to none of the generic categories below.
    let blockedTypeIdentifiers: Set<String> = [
        "com.apple.terminal.settings", "com.apple.generic-internet-location",
        "com.apple.file-internet-location", "com.apple.web-internet-location",
        "com.microsoft.internet-shortcut", "com.sun.java-web-start", "com.apple.xip-archive",
        "com.apple.mobileconfig", "com.apple.configprofile", "com.apple.shortcut",
        "com.apple.shortcuts.workflow-file",
    ]
    // Declared MIME types LaunchServices has no UTType for (they resolve to
    // opaque `dyn.` types that conform to nothing), so match them by name.
    let blockedMimeTypes: Set<String> = [
        "application/x-mach-binary", "application/x-executable", "application/x-sh",
        "application/x-shellscript", "application/x-apple-diskimage",
        "application/x-msdownload", "application/x-msi", "application/java-archive",
    ]
    let ext = (filename as NSString).pathExtension.lowercased()
    if blockedExtensions.contains(ext) { return true }
    if let contentType, blockedMimeTypes.contains(contentType.lowercased()) { return true }
    var types: [UTType] = []
    if !ext.isEmpty, let byExtension = UTType(filenameExtension: ext) { types.append(byExtension) }
    if let contentType, let byMime = UTType(mimeType: contentType) { types.append(byMime) }
    return types.contains {
        blockedTypeIdentifiers.contains($0.identifier)
            || $0.conforms(to: .executable) || $0.conforms(to: .applicationBundle)
            || $0.conforms(to: .script) || $0.conforms(to: .diskImage)
            || $0.conforms(to: .bundle) || $0.conforms(to: .internetLocation)
    }
}

/// Copy `source` to `destination` and apply `com.apple.quarantine` so
/// Gatekeeper/LaunchServices treat it exactly like a browser download. If the
/// attribute cannot be applied the copy is removed again and the error
/// rethrown — an unquarantined attachment must never be left outside the
/// app-owned cache. This (via `stageAttachmentCopy`) is the ONLY place
/// attachment bytes leave the cache directory.
func saveQuarantined(from source: URL, to destination: URL) throws {
    let fm = FileManager.default
    if fm.fileExists(atPath: destination.path) {
        try fm.removeItem(at: destination)
    }
    try fm.copyItem(at: source, to: destination)
    do {
        var values = URLResourceValues()
        values.quarantineProperties = [
            kLSQuarantineTypeKey as String: kLSQuarantineTypeOtherDownload as String,
            kLSQuarantineAgentNameKey as String: "Castellum",
        ]
        var mutableDestination = destination
        try mutableDestination.setResourceValues(values)
    } catch {
        try? fm.removeItem(at: destination)
        throw error
    }
}

/// Sanitise every raw filename and de-duplicate case-insensitively, the way
/// Finder does: `report.pdf`, `report 2.pdf`, `report 3.pdf`. Order preserved.
func uniqueAttachmentFilenames(_ raw: [String?]) -> [String] {
    var taken = Set<String>()
    return raw.map { name in
        // NFC first: "e" + combining acute and precomposed "é" are the same
        // name on disk (APFS is normalisation-insensitive) but differ as
        // Swift strings, so duplicates would otherwise slip through.
        let safe = sanitizedAttachmentFilename(name).precomposedStringWithCanonicalMapping
        let ns = safe as NSString
        let ext = ns.pathExtension
        let stem = ext.isEmpty ? safe : ns.deletingPathExtension
        var candidate = safe
        var counter = 2
        while taken.contains(candidate.lowercased()) {
            candidate = ext.isEmpty ? "\(stem) \(counter)" : "\(stem) \(counter).\(ext)"
            counter += 1
        }
        taken.insert(candidate.lowercased())
        return candidate
    }
}

/// Save All: write every cached attachment into `folder` under its sanitised,
/// de-duplicated name, each through `saveQuarantined` (so every output carries
/// `com.apple.quarantine`). Returns the written URLs in input order.
@discardableResult
func saveAllQuarantined(_ items: [(source: URL, filename: String?)], into folder: URL) throws -> [URL] {
    let names = uniqueAttachmentFilenames(items.map { $0.filename })
    var written: [URL] = []
    for (item, name) in zip(items, names) {
        // Never overwrite a file the user already has: on a collision the
        // new file gets " 2", " 3", ... (saveQuarantined itself replaces).
        let destination = uniqueDestination(in: folder, filename: name)
        try saveQuarantined(from: item.source, to: destination)
        written.append(destination)
    }
    return written
}

/// Save as Zip: archive the attachments (sanitised, de-duplicated names, flat)
/// with `/usr/bin/ditto -c -k` and deliver the zip via `saveQuarantined`, so
/// the archive itself is quarantined. The staging copy is removed afterwards.
func saveZipQuarantined(_ items: [(source: URL, filename: String?)], to destination: URL) throws {
    let fm = FileManager.default
    let work = fm.temporaryDirectory
        .appendingPathComponent("CastellumZip-\(UUID().uuidString)", isDirectory: true)
    let folder = work.appendingPathComponent("files", isDirectory: true)
    try fm.createDirectory(at: folder, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: work) }
    try saveAllQuarantined(items, into: folder)
    let zip = work.appendingPathComponent("attachments.zip")
    let ditto = Process()
    ditto.executableURL = URL(fileURLWithPath: "/usr/bin/ditto")
    ditto.arguments = ["-c", "-k", "--norsrc", folder.path, zip.path]
    try ditto.run()
    ditto.waitUntilExit()
    guard ditto.terminationStatus == 0 else {
        throw NSError(domain: "Castellum.Attachments", code: Int(ditto.terminationStatus),
                      userInfo: [NSLocalizedDescriptionKey: "ditto exited with status \(ditto.terminationStatus)"])
    }
    try saveQuarantined(from: zip, to: destination)
}

/// Where staged (quarantined, correctly named) copies live: one directory per
/// content hash under the temp dir.
func attachmentStagingRoot() -> URL {
    FileManager.default.temporaryDirectory
        .appendingPathComponent("CastellumAttachments", isDirectory: true)
}

/// Stage a quarantined copy of a cached attachment under its real (sanitised)
/// filename and return its URL. Always a fresh copy, so an edit made to a
/// previously opened staged file never leaks into the next Open.
func stageAttachmentCopy(
    cachePath: String,
    filename: String?,
    stagingRoot: URL? = nil
) throws -> URL {
    if stagingRoot == nil { _ = pruneStagedAttachmentsOnce }
    let source = URL(fileURLWithPath: cachePath)
    let root = stagingRoot ?? attachmentStagingRoot()
    let dir = root.appendingPathComponent(source.lastPathComponent, isDirectory: true)
    try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let destination = dir.appendingPathComponent(sanitizedAttachmentFilename(filename))
    try saveQuarantined(from: source, to: destination)
    return destination
}

/// Remove staging directories untouched for `olderThan` seconds so decrypted
/// attachment copies don't linger in the temp dir indefinitely.
func pruneStagedAttachments(root: URL, olderThan age: TimeInterval = 24 * 3600, now: Date = Date()) {
    let fm = FileManager.default
    guard let entries = try? fm.contentsOfDirectory(
        at: root, includingPropertiesForKeys: [.contentModificationDateKey]) else { return }
    for entry in entries {
        let modified = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]))?
            .contentModificationDate ?? .distantPast
        if now.timeIntervalSince(modified) > age {
            try? fm.removeItem(at: entry)
        }
    }
}

/// Runs the stale-copy prune exactly once per process, lazily, the first time
/// anything is staged.
private let pruneStagedAttachmentsOnce: Void = pruneStagedAttachments(root: attachmentStagingRoot())

private func presentAttachmentError(_ title: String, _ error: Error) {
    let alert = NSAlert()
    alert.messageText = title
    alert.informativeText = String(describing: error)
    alert.runModal()
}

// MARK: - View

struct AttachmentStripView: View {
    let detail: MessageDetail
    @ObservedObject var model: AppModel
    @State private var quickLookItem: AttachmentQuickLookItem?
    @State private var hostWindow: NSWindow?
    @ObservedObject private var prefs = Preferences.shared
    /// True while Save All / Save as Zip is fetching missing attachments.
    @State private var bulkBusy = false

    private var attachments: [AttachmentDescriptor] { detail.stripAttachments }

    private static let maxVisibleRowsBeforeScroll = 4
    private static let scrollCapHeight: CGFloat = 160

    @ViewBuilder private var attachmentRows: some View {
        ForEach(attachments) { attachment in
            AttachmentRow(
                attachment: attachment,
                state: model.attachmentDownloads[
                    model.attachmentKey(messageId: detail.header.id, ordinal: attachment.ordinal)
                ] ?? .idle,
                onDownload: {
                    model.downloadAttachment(
                        messageId: detail.header.id,
                        ordinal: attachment.ordinal,
                        accountId: detail.header.accountId)
                },
                onPrimary: { path in primaryClick(path: path, attachment: attachment) },
                onOpen: { path in openAttachment(path: path, attachment: attachment) },
                onSaveAs: { path in presentSaveAsPanel(path: path, filename: attachment.filename) },
                onQuickLook: { path in quickLook(path: path, filename: attachment.filename) }
            )
        }
    }

    var body: some View {
        if !attachments.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                // Always-visible strip header (GUI-6): Save All / Save as Zip
                // for 2+ attachments (they download whatever is still remote),
                // and the gear menu for the click-default.
                HStack(spacing: 8) {
                    Spacer()
                    if bulkBusy { ProgressView().controlSize(.small) }
                    if attachments.count > 1 {
                        Button("Save All…") { presentSaveAllPanel() }
                            .buttonStyle(.borderless)
                            .disabled(bulkBusy)
                        Button("Save as Zip…") { presentSaveAsZipPanel() }
                            .buttonStyle(.borderless)
                            .disabled(bulkBusy)
                    }
                    Menu {
                        Section("Clicking an attachment") {
                            Picker("Clicking an attachment", selection: $prefs.attachmentDefaultAction) {
                                Text("Ask each time").tag(nil as AttachmentDefaultAction?)
                                ForEach(AttachmentDefaultAction.allCases) { action in
                                    Text(action.label).tag(action as AttachmentDefaultAction?)
                                }
                            }
                            .pickerStyle(.inline)
                        }
                    } label: {
                        Image(systemName: "gearshape")
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .help("Default attachment action")
                }
                .padding(.horizontal, 8)
                // Past a few rows the list scrolls inside a capped height, so a
                // message with many attachments cannot inflate the reading pane
                // (and with it the window's minimum height). Few rows size to content.
                if attachments.count > Self.maxVisibleRowsBeforeScroll {
                    ScrollView(.vertical) {
                        VStack(alignment: .leading, spacing: 4) { attachmentRows }
                    }
                    .frame(maxHeight: Self.scrollCapHeight)
                } else {
                    attachmentRows
                }
            }
            .padding(.vertical, 4)
            .onAppear(perform: refreshStatuses)
            .onChange(of: detail.header.id) { _ in refreshStatuses() }
            .background(WindowAccessor(window: $hostWindow))
            .background(
                AttachmentQuickLookHost(
                    item: quickLookItem,
                    onDismiss: { quickLookItem = nil }
                )
            )
        }
    }

    private func refreshStatuses() {
        for attachment in attachments {
            model.refreshAttachmentCacheStatus(messageId: detail.header.id, ordinal: attachment.ordinal)
        }
    }

    private func openAttachment(path: String, attachment: AttachmentDescriptor) {
        let safeName = sanitizedAttachmentFilename(attachment.filename)
        guard !isBlockedFromAutoOpen(filename: safeName, contentType: attachment.contentType) else {
            let alert = NSAlert()
            alert.messageText = String(localized: "Can't Open This File")
            alert.informativeText = String(
                localized: "\(safeName) looks like an executable or installer. Use Save As, then open it from Finder if you trust it."
            )
            alert.runModal()
            return
        }
        do {
            let staged = try stageAttachmentCopy(cachePath: path, filename: attachment.filename)
            NSWorkspace.shared.open(staged)
        } catch {
            presentAttachmentError(String(localized: "Couldn't Open Attachment"), error)
        }
    }

    private func quickLook(path: String, filename: String?) {
        do {
            let staged = try stageAttachmentCopy(cachePath: path, filename: filename)
            quickLookItem = AttachmentQuickLookItem(
                url: staged, title: sanitizedAttachmentFilename(filename))
        } catch {
            presentAttachmentError(String(localized: "Couldn't Preview Attachment"), error)
        }
    }

    private func status(of attachment: AttachmentDescriptor) -> AttachmentBulkPlan.Status {
        let key = model.attachmentKey(messageId: detail.header.id, ordinal: attachment.ordinal)
        switch model.attachmentDownloads[key] ?? .idle {
        case .ready: return .ready
        case .downloading: return .downloading
        case .checking: return .checking
        case .idle: return .idle
        case .failed: return .failed
        }
    }

    private func readyItems() -> [(source: URL, filename: String?)] {
        attachments.compactMap { attachment in
            let key = model.attachmentKey(messageId: detail.header.id, ordinal: attachment.ordinal)
            if case .ready(let path)? = model.attachmentDownloads[key] {
                return (URL(fileURLWithPath: path), attachment.filename)
            }
            return nil
        }
    }

    /// Starts a download for every attachment not yet local, then waits until
    /// all are ready. Returns nil (after telling the user) if any fails or
    /// the wait times out.
    @MainActor
    private func prepareAllAttachments() async -> [(source: URL, filename: String?)]? {
        bulkBusy = true
        defer { bulkBusy = false }
        let toFetch = AttachmentBulkPlan.ordinalsToDownload(
            attachments.map { (ordinal: $0.ordinal, status: status(of: $0)) })
        var started = Set<Int64>(toFetch)
        for ordinal in toFetch {
            model.downloadAttachment(
                messageId: detail.header.id, ordinal: ordinal, accountId: detail.header.accountId)
        }
        // The 300 s budget restarts whenever another attachment finishes, so
        // a big batch (or one slow large file) is not cut off while it is
        // still making progress.
        let idleBudget: TimeInterval = 300
        var deadline = Date().addingTimeInterval(idleBudget)
        var readyCount = 0
        while Date() < deadline {
            let nowReady = attachments.filter { status(of: $0) == .ready }.count
            if nowReady > readyCount {
                readyCount = nowReady
                deadline = Date().addingTimeInterval(idleBudget)
            }
            switch AttachmentBulkPlan.progress(attachments.map(status(of:))) {
            case .allReady:
                return readyItems()
            case .failed(let count):
                presentAttachmentError(
                    String(localized: "Couldn't Save Attachments"),
                    NSError(domain: "Castellum.Attachments", code: 1, userInfo: [
                        NSLocalizedDescriptionKey: "\(count) attachment(s) could not be downloaded. Use Retry on them, then try again."]))
                return nil
            case .waiting:
                // A cache probe can settle to .idle after planning (not
                // cached): start those too. downloadAttachment is a no-op
                // while one is already in flight.
                let idle = attachments.filter { status(of: $0) == .idle }.map(\.ordinal)
                let decision = AttachmentBulkPlan.idleDecision(idle: idle, started: started)
                // The user cancelled one of ours: honour it, stop quietly.
                if decision.userCancelled { return nil }
                for ordinal in decision.start {
                    started.insert(ordinal)
                    model.downloadAttachment(
                        messageId: detail.header.id, ordinal: ordinal,
                        accountId: detail.header.accountId)
                }
                try? await Task.sleep(nanoseconds: 200_000_000)
            }
        }
        presentAttachmentError(
            String(localized: "Couldn't Save Attachments"),
            NSError(domain: "Castellum.Attachments", code: 2, userInfo: [
                NSLocalizedDescriptionKey: "Downloading the attachments took too long."]))
        return nil
    }

    /// A click on a downloaded attachment: the remembered default, or ask
    /// once and remember when "Always do this" is ticked.
    private func primaryClick(path: String, attachment: AttachmentDescriptor) {
        switch AttachmentPrimaryClick.resolve(preference: prefs.attachmentDefaultAction) {
        case .perform(let action):
            perform(action, path: path, attachment: attachment)
        case .ask:
            let alert = NSAlert()
            alert.messageText = String(localized: "What should clicking an attachment do?")
            alert.informativeText = sanitizedAttachmentFilename(attachment.filename)
            for action in AttachmentDefaultAction.allCases { alert.addButton(withTitle: action.label) }
            alert.addButton(withTitle: String(localized: "Cancel"))
            alert.showsSuppressionButton = true
            alert.suppressionButton?.title = String(localized: "Always do this")
            let response = alert.runModal()
            let index = response.rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
            guard AttachmentDefaultAction.allCases.indices.contains(index) else { return }
            let chosen = AttachmentDefaultAction.allCases[index]
            if alert.suppressionButton?.state == .on { prefs.attachmentDefaultAction = chosen }
            perform(chosen, path: path, attachment: attachment)
        }
    }

    private func perform(_ action: AttachmentDefaultAction, path: String, attachment: AttachmentDescriptor) {
        switch action {
        case .open: openAttachment(path: path, attachment: attachment)
        case .saveAs: presentSaveAsPanel(path: path, filename: attachment.filename)
        case .quickLook: quickLook(path: path, filename: attachment.filename)
        }
    }

    private func presentSaveAllPanel() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = String(localized: "Save")
        let complete: (NSApplication.ModalResponse) -> Void = { response in
            guard response == .OK, let folder = panel.url else { return }
            Task { @MainActor in
                guard let items = await prepareAllAttachments() else { return }
                do {
                    // File copies off the main actor: big attachments must
                    // not beachball the UI.
                    try await Task.detached { try saveAllQuarantined(items, into: folder) }.value
                } catch {
                    presentAttachmentError(String(localized: "Couldn't Save Attachments"), error)
                }
            }
        }
        if let hostWindow {
            panel.beginSheetModal(for: hostWindow, completionHandler: complete)
        } else {
            complete(panel.runModal())
        }
    }

    private func presentSaveAsZipPanel() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = String(localized: "Attachments.zip")
        panel.allowedContentTypes = [.zip]
        panel.canCreateDirectories = true
        let complete: (NSApplication.ModalResponse) -> Void = { response in
            guard response == .OK, let destination = panel.url else { return }
            Task { @MainActor in
                guard let items = await prepareAllAttachments() else { return }
                do {
                    // ditto + copies run off the main actor.
                    try await Task.detached { try saveZipQuarantined(items, to: destination) }.value
                } catch {
                    presentAttachmentError(String(localized: "Couldn't Save Attachments"), error)
                }
            }
        }
        if let hostWindow {
            panel.beginSheetModal(for: hostWindow, completionHandler: complete)
        } else {
            complete(panel.runModal())
        }
    }

    private func presentSaveAsPanel(path: String, filename: String?) {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = sanitizedAttachmentFilename(filename)
        panel.canCreateDirectories = true
        let complete: (NSApplication.ModalResponse) -> Void = { response in
            guard response == .OK, let destination = panel.url else { return }
            do {
                try saveQuarantined(from: URL(fileURLWithPath: path), to: destination)
            } catch {
                presentAttachmentError(String(localized: "Couldn't Save Attachment"), error)
            }
        }
        if let hostWindow {
            panel.beginSheetModal(for: hostWindow, completionHandler: complete)
        } else {
            complete(panel.runModal())
        }
    }
}

private struct AttachmentRow: View {
    let attachment: AttachmentDescriptor
    let state: AppModel.AttachmentDownloadState
    let onDownload: () -> Void
    /// Click on the file name once downloaded: the default-action policy.
    let onPrimary: (String) -> Void
    let onOpen: (String) -> Void
    let onSaveAs: (String) -> Void
    let onQuickLook: (String) -> Void

    private var label: some View {
        HStack(spacing: 8) {
            Image(systemName: "paperclip")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 0) {
                Text(sanitizedAttachmentFilename(attachment.filename))
                    .font(.callout)
                    .lineLimit(1)
                    // Middle truncation keeps the real extension visible (#3379).
                    .truncationMode(.middle)
                if let size = attachment.sizeBytes {
                    Text(ByteCountFormatter.string(fromByteCount: size, countStyle: .file))
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            // Drag-out is only offered once the bytes are local.
            if case .ready(let path) = state {
                label
                    .contentShape(Rectangle())
                    .onTapGesture(count: 2) { onPrimary(path) }
                    .onDrag { dragProvider(path: path, filename: attachment.filename) }
            } else {
                label
            }
            Spacer()
            switch state {
            case .idle, .checking:
                Button("Download") { onDownload() }
                    .buttonStyle(.bordered)
                    .disabled(state == .checking)
            case .downloading:
                ProgressView().controlSize(.small)
            case .ready(let path):
                Button("Open") { onOpen(path) }.buttonStyle(.borderless)
                Button("Save As…") { onSaveAs(path) }.buttonStyle(.borderless)
                Button("Quick Look") { onQuickLook(path) }.buttonStyle(.borderless)
            case .failed(let reason):
                Label(reason, systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .lineLimit(2)
                Button("Retry") { onDownload() }.buttonStyle(.borderless)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 4)
    }

    /// Drag-out: hand the drag session a QUARANTINED, correctly named staged
    /// copy (never the cache file itself — the cache is app-owned and must not
    /// be mutated or deleted by whatever the drop target does with it).
    private func dragProvider(path: String, filename: String?) -> NSItemProvider {
        guard let staged = try? stageAttachmentCopy(cachePath: path, filename: filename) else {
            return NSItemProvider()
        }
        return NSItemProvider(contentsOf: staged) ?? NSItemProvider()
    }
}


extension MessageDetail {
    /// The attachments the strip lists. When the message carries a parsed
    /// calendar invite (the card represents it), the nameless inline
    /// `text/calendar` alternative is hidden so the invite is not listed twice
    /// next to its `invite.ics` (RP07). A named .ics is still shown.
    var stripAttachments: [AttachmentDescriptor] {
        // Inline images the HTML body shows (signature logos, embedded screenshots) are part
        // of the message, not downloads; unreferenced inline parts stay listed (#3442).
        let all = (attachments ?? []).filter {
            !InlineImagePolicy.isReferencedInline($0, html: sanitizedHtml)
        }
        guard calendarInvite != nil else { return all }
        return all.filter { part in
            let nameless = (part.filename ?? "").isEmpty
            let isCalendar = (part.contentType ?? "").lowercased().hasPrefix("text/calendar")
            return !(nameless && isCalendar)
        }
    }
}
