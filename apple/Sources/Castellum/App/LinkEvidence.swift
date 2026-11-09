// Castellum - "why is this connected?" presentation helpers for the shared
// evidence-backed link model. Pure functions, pinned by LinkEvidenceTests.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

/// One-sentence explanation of why a link exists, from its origin and
/// provenance. British English; no entity content, only references.
func linkEvidenceExplanation(origin: String, provenance: String?) -> String {
    switch origin {
    case "manual":
        return "Linked manually."
    case "derived":
        if evidenceMessageId(provenance: provenance) != nil {
            return "Auto-linked because this person participated in a message assigned to the project."
        }
        if provenance == "participant" {
            return "Auto-linked as a message participant."
        }
        return "Auto-linked."
    default:
        return "Imported from \(origin)."
    }
}

/// The evidencing message id when the provenance is a message reference
/// ("message:acc:INBOX:9" → "acc:INBOX:9"); nil otherwise.
func evidenceMessageId(provenance: String?) -> String? {
    guard let provenance, provenance.hasPrefix("message:") else { return nil }
    let id = String(provenance.dropFirst("message:".count))
    return id.isEmpty ? nil : id
}

/// Why a derived link has no "Open the evidencing message" button, or nil when
/// the button is shown (or when the question does not apply).
///
/// Migration 0017 backfilled every pre-existing auto-link with
/// `provenance = "participant"`, because the evidencing message id was simply
/// never recorded before that migration — the data does not exist and cannot
/// be recovered. Links made since carry `message:<id>` and get the button.
///
/// Without this the popover just silently omitted the button, which reads as
/// "this link has no reason" rather than "the reason predates the feature".
/// Saying so is the whole point of a "why linked" popover.
func linkEvidenceMissingMessageNote(origin: String, provenance: String?) -> String? {
    // Accepts BOTH spellings on purpose. There are two origin representations
    // in play (see PersonChipView): the LinkRecord carries "derived", while the
    // ProjectContactLink DTO renders the same stored value as the legacy
    // "auto". The popover passes the LinkRecord — but if that is ever rewired,
    // matching only "derived" would return nil for every row and the note would
    // vanish SILENTLY, which is the exact failure mode this function exists to
    // remove. Its neighbour `linkEvidenceExplanation` fails loudly on "auto"
    // ("Imported from auto."); this one could not, so it must not care.
    guard origin == "derived" || origin == "auto",
          evidenceMessageId(provenance: provenance) == nil
    else {
        return nil
    }
    if provenance == "participant" {
        return String(localized: "Linked before Castellum recorded evidence, so the specific message was never saved.")
    }
    return String(localized: "No evidencing message was recorded for this link.")
}

/// Absolute date text for link timestamps (medium date, no time — evidence
/// popovers care about the day, not the second).
func linkEvidenceDateText(epoch: Int64, locale: Locale = .current) -> String {
    let formatter = DateFormatter()
    formatter.locale = locale
    formatter.dateStyle = .medium
    formatter.timeStyle = .none
    return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(epoch)))
}
