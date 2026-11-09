// Castellum - .eml export/import filename and destination helpers (S49).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Deliberately independent from AttachmentStripView's
// sanitizedAttachmentFilename (a Content-Disposition filename sanitizer,
// pinned by AttachmentDTOTests) — this one turns a message SUBJECT into a
// safe, dated export filename.

import AppKit
import Foundation

/// Build a safe, chronologically-sortable filename for an exported message:
/// an ISO date prefix (when known) plus a sanitised subject, a single path
/// component, capped length, ".eml" extension. Never empty.
func sanitizedEmlFilename(subject: String?, sentAt: Int64?) -> String {
    let datePrefix: String
    if let sentAt {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.timeZone = .current
        datePrefix = f.string(from: Date(timeIntervalSince1970: TimeInterval(sentAt))) + " "
    } else {
        datePrefix = ""
    }
    let raw = subject?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    let base = raw.isEmpty ? "message" : raw
    let separatorsUnified = base
        .replacingOccurrences(of: "/", with: "-")
        .replacingOccurrences(of: ":", with: "-")
        .replacingOccurrences(of: "\\", with: "-")
    let printable = String(String.UnicodeScalarView(
        separatorsUnified.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }
    ))
    let noLeadingDots = String(printable.drop(while: { $0 == "." }))
        .trimmingCharacters(in: .whitespacesAndNewlines)
    let safe = noLeadingDots.isEmpty ? "message" : noLeadingDots

    let maxNameBytes = 150 // leaves headroom for the date prefix + ".eml"
    var name = datePrefix + safe
    while name.utf8.count > maxNameBytes, !name.isEmpty {
        name.removeLast()
    }
    return name.isEmpty ? "message.eml" : name + ".eml"
}

/// A destination URL in `folder` for `filename` that does not already
/// exist, appending " 2", " 3", ... before the extension on a collision.
func uniqueDestination(in folder: URL, filename: String) -> URL {
    let fm = FileManager.default
    let base = (filename as NSString).deletingPathExtension
    let ext = (filename as NSString).pathExtension
    var candidate = folder.appendingPathComponent(filename)
    var n = 2
    while fm.fileExists(atPath: candidate.path) {
        let numbered = ext.isEmpty ? "\(base) \(n)" : "\(base) \(n).\(ext)"
        candidate = folder.appendingPathComponent(numbered)
        n += 1
    }
    return candidate
}

/// One message to export: id/account for the fetch, a precomputed filename.
struct ExportableMessage {
    let id: String
    let accountId: String
    let filename: String
}

@MainActor
enum EmlExport {
    /// Export `items` as `.eml`: a single item gets a Save panel with the
    /// filename prefilled; several get a folder picker and one file each
    /// (collisions disambiguated by `uniqueDestination`). Reuses
    /// `saveQuarantined` (AttachmentStripView.swift) so exported files get
    /// the same quarantine treatment as a saved attachment.
    static func present(items: [ExportableMessage], model: AppModel) {
        guard !items.isEmpty else { return }
        if items.count == 1, let only = items.first {
            let panel = NSSavePanel()
            panel.nameFieldStringValue = only.filename
            panel.canCreateDirectories = true
            let response = panel.runModal()
            guard response == .OK, let destination = panel.url else { return }
            Task { await exportOne(only, to: destination, model: model) }
        } else {
            let panel = NSOpenPanel()
            panel.canChooseDirectories = true
            panel.canChooseFiles = false
            panel.canCreateDirectories = true
            panel.prompt = "Export"
            panel.message = String(localized: "Choose a folder for \(items.count) messages")
            let response = panel.runModal()
            guard response == .OK, let folder = panel.url else { return }
            Task { await exportMany(items, into: folder, model: model) }
        }
    }

    private static func exportOne(_ item: ExportableMessage, to destination: URL, model: AppModel) async {
        do {
            let path = try await model.fetchRawSourcePath(messageId: item.id, accountId: item.accountId)
            try saveQuarantined(from: URL(fileURLWithPath: path), to: destination)
        } catch {
            model.errorText = "Couldn't export the message: \(String(describing: error))"
        }
    }

    private static func exportMany(_ items: [ExportableMessage], into folder: URL, model: AppModel) async {
        var failures = 0
        for item in items {
            do {
                let path = try await model.fetchRawSourcePath(messageId: item.id, accountId: item.accountId)
                let destination = uniqueDestination(in: folder, filename: item.filename)
                try saveQuarantined(from: URL(fileURLWithPath: path), to: destination)
            } catch {
                failures += 1
            }
        }
        if failures > 0 {
            model.errorText = "\(failures) of \(items.count) messages couldn't be exported."
        }
    }
}
