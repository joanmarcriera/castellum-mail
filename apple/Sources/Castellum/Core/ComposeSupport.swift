// Castellum - pure compose logic: recipient parsing, SMTP config, reply prefill.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Everything here is headlessly testable: no AppKit, no FFI. The compose UI
// (ComposeView/ComposeWindowController) is a thin shell over these.

import Foundation

// MARK: - recipient parsing

/// Parses a To/Cc/Bcc text field into `DraftAddress`es. Accepts comma- or
/// semicolon-separated entries in the forms `a@b`, `Name <a@b>`, and
/// `"Quoted, Name" <a@b>` (commas inside quotes don't split).
enum AddressParser {
    struct ParseResult: Equatable {
        var addresses: [DraftAddress] = []
        /// Entries that could not be parsed into a valid address (surfaced to
        /// the user instead of being silently dropped on Send).
        var invalid: [String] = []
    }

    static func parse(_ input: String) -> ParseResult {
        var result = ParseResult()
        for token in split(input) {
            let entry = token.trimmingCharacters(in: .whitespacesAndNewlines)
            if entry.isEmpty { continue }
            if let address = parseEntry(entry) {
                result.addresses.append(address)
            } else {
                result.invalid.append(entry)
            }
        }
        return result
    }

    /// Split on `,` / `;` outside double quotes and outside `<...>`. A
    /// backslash inside quotes escapes the next character (so `\"` does not
    /// close the quote). If a quote is never closed (a display name with an
    /// odd number of `"`), quote tracking is dropped and the field is split
    /// on separators outside `<...>` only, so one stray quote can't swallow
    /// every recipient after it (audit #3443).
    private static func split(_ input: String) -> [String] {
        let tokens = split(input, trackQuotes: true)
        return tokens ?? split(input, trackQuotes: false)!
    }

    /// `nil` when `trackQuotes` is on and a quote is left open at the end.
    private static func split(_ input: String, trackQuotes: Bool) -> [String]? {
        var tokens: [String] = []
        var current = ""
        var inQuotes = false
        var inAngle = false
        var escaped = false
        for ch in input {
            if escaped { current.append(ch); escaped = false; continue }
            switch ch {
            case "\\" where inQuotes: escaped = true; current.append(ch)
            case "\"" where trackQuotes: inQuotes.toggle(); current.append(ch)
            case "<" where !inQuotes: inAngle = true; current.append(ch)
            case ">" where !inQuotes: inAngle = false; current.append(ch)
            case ",", ";":
                if inQuotes || inAngle {
                    current.append(ch)
                } else {
                    tokens.append(current)
                    current = ""
                }
            default: current.append(ch)
            }
        }
        if trackQuotes && inQuotes { return nil }
        tokens.append(current)
        return tokens
    }

    /// One trimmed entry -> address, or nil if it isn't a valid address.
    private static func parseEntry(_ entry: String) -> DraftAddress? {
        // `Name <a@b>` / `"Name" <a@b>` — the email is the last <...> group.
        if entry.hasSuffix(">"), let open = entry.lastIndex(of: "<") {
            let email = String(entry[entry.index(after: open)..<entry.index(before: entry.endIndex)])
                .trimmingCharacters(in: .whitespaces)
            guard isValidEmail(email) else { return nil }
            var name = String(entry[entry.startIndex..<open])
                .trimmingCharacters(in: .whitespaces)
            if name.count >= 2, name.hasPrefix("\""), name.hasSuffix("\"") {
                name = unescapeQuoted(String(name.dropFirst().dropLast()))
                    .trimmingCharacters(in: .whitespaces)
            } else if name.contains("\"") {
                // An unbalanced or stray quote in a display name: keep the text, drop the quote.
                name = name.replacingOccurrences(of: "\"", with: "").trimmingCharacters(in: .whitespaces)
            }
            return DraftAddress(name: name.isEmpty ? nil : name, email: email)
        }
        // Bare `a@b`.
        return isValidEmail(entry) ? DraftAddress(name: nil, email: entry) : nil
    }

    /// `\x` -> `x` inside a quoted display name (RFC 5322 quoted-pair).
    private static func unescapeQuoted(_ s: String) -> String {
        var out = ""
        var escaped = false
        for ch in s {
            if escaped { out.append(ch); escaped = false }
            else if ch == "\\" { escaped = true }
            else { out.append(ch) }
        }
        if escaped { out.append("\\") }
        return out
    }

    /// Minimal validity: one `@` with a non-empty local part and a domain
    /// containing at least one `.`, and no whitespace/quoting characters.
    static func isValidEmail(_ s: String) -> Bool {
        let parts = s.split(separator: "@", omittingEmptySubsequences: false)
        guard parts.count == 2, !parts[0].isEmpty, parts[1].contains(".") else { return false }
        guard !parts[1].hasPrefix("."), !parts[1].hasSuffix(".") else { return false }
        return s.rangeOfCharacter(from: .whitespacesAndNewlines) == nil
            && !s.contains("<") && !s.contains(">") && !s.contains("\"")
    }
}

// MARK: - account config (Keychain JSON) + SMTP defaulting

/// Effective SMTP connection settings after defaulting from the IMAP values.
struct SMTPSettings: Equatable {
    var host: String
    var port: Int
    var username: String
    var password: String
    var useTls: Bool

    var endpoint: String { "\(host):\(port)" }
}

/// Non-identity form fields copied from one account to prefill another
/// (`AccountConfig.duplicateFields()`). Deliberately has no
/// email/username/password properties — a duplicate must always get a fresh
/// identity, never the source's.
struct AccountDuplicateFields: Equatable {
    var displayName: String
    var host: String
    var port: String
    var useTLS: Bool
    var smtpHost: String
    /// True when `smtpHost` is exactly what `defaultSMTPHost(imapHost:)` would
    /// derive from `host` — mirrors the sheet's own `smtpHostAuto` flag so a
    /// duplicated account keeps following IMAP-host edits the same way a
    /// freshly-added one does, rather than freezing on a copied literal.
    var smtpHostAuto: Bool
    var smtpPort: String
    var smtpUseTLS: Bool
    /// IMAP security mode; `useTLS` stays for callers that
    /// only know the old toggle.
    var imapSecurity: IMAPSecurity = .tls
}

/// How the IMAP connection is secured. Stored as the
/// `imap_security` key; Rust's `TransportSecurity::resolve` reads it with the
/// same table as `AccountConfig.effectiveIMAPSecurity`.
enum IMAPSecurity: String, CaseIterable, Identifiable {
    case tls, starttls, none
    var id: String { rawValue }

    /// The usual port for this mode, for the sheet's port auto-follow.
    var defaultPort: Int { self == .tls ? 993 : 143 }
}

/// The connection-config JSON stored per account in the Keychain (AccountStore).
/// The `smtp*` fields are optional: configs saved before the compose slice lack
/// them and must still decode — `effectiveSMTP` applies the same smart defaults
/// at read time. Extra keys in this JSON are ignored by the Rust sync/replay
/// deserializers (serde default), so adding fields is backward/forward safe.
struct AccountConfig: Codable, Equatable {
    var accountId: String
    var email: String
    var host: String
    var port: Int
    var username: String
    var password: String
    var useTls: Bool
    var allowInvalidCerts: Bool?
    /// "tls" | "starttls" | "none". Nil in configs saved
    /// before it existed: `useTls` decides, exactly as before.
    var imapSecurity: String? = nil
    var smtpHost: String?
    var smtpPort: Int?
    var smtpUsername: String?
    var smtpPassword: String?
    var smtpUseTls: Bool?
    /// From-header display name for outgoing mail ("Name <a@b>"). Optional:
    /// pre-compose configs lack it and a bare address stays valid.
    var displayName: String?
    /// Explicit folder filter chosen at setup (S62). Nil = the core's default
    /// set (Inbox, subscribed and special-use folders, new folders included).
    /// Maps to Rust `ImapSyncConfig::mailboxes`.
    var mailboxes: [String]? = nil
    /// Initial-history window chosen at setup (S62), `YYYY-MM-DD`. Nil = all
    /// mail. Maps to Rust `ImapSyncConfig::initial_sync_since`; only ever
    /// affects a folder's first import.
    var initialSyncSince: String? = nil

    /// The fields that decide whether a connection works — everything except
    /// the S62 sync-scope choices, so changing folders or history after a
    /// Test Connection doesn't mark the test stale.
    var connectionSettings: AccountConfig {
        var copy = self
        copy.mailboxes = nil
        copy.initialSyncSince = nil
        return copy
    }

    private static let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    private static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        return e
    }()

    static func decode(fromJSON json: String) throws -> AccountConfig {
        try decoder.decode(AccountConfig.self, from: Data(json.utf8))
    }

    func encodedJSON() throws -> String {
        let data = try Self.encoder.encode(self)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return json
    }

    /// Default SMTP host guessed from an IMAP host: a leading `imap.` becomes
    /// `smtp.` (imap.example.com -> smtp.example.com); otherwise the same host.
    static func defaultSMTPHost(imapHost: String) -> String {
        imapHost.lowercased().hasPrefix("imap.")
            ? "smtp." + imapHost.dropFirst("imap.".count)
            : imapHost
    }

    /// The IMAP security mode this config resolves to: the explicit key
    /// wins, an unknown value fails closed to implicit TLS, and a config
    /// without the key keeps its old `useTls` meaning.
    var effectiveIMAPSecurity: IMAPSecurity {
        guard let raw = imapSecurity?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() else {
            return useTls ? .tls : .none
        }
        return IMAPSecurity(rawValue: raw) ?? .tls
    }

    /// IMAP host:port, for display (Settings > Accounts, Status dashboard).
    var imapEndpoint: String { "\(host):\(port)" }

    /// SMTP settings with smart defaults for anything not explicitly stored:
    /// host derived from the IMAP host, port 587, IMAP username/password, TLS on.
    var effectiveSMTP: SMTPSettings {
        SMTPSettings(
            host: smtpHost.flatMap { $0.isEmpty ? nil : $0 }
                ?? Self.defaultSMTPHost(imapHost: host),
            port: smtpPort ?? 587,
            username: smtpUsername.flatMap { $0.isEmpty ? nil : $0 } ?? username,
            password: smtpPassword.flatMap { $0.isEmpty ? nil : $0 } ?? password,
            useTls: smtpUseTls ?? true
        )
    }

    /// Maps a stored config to `AccountSetupSheet`'s form fields with the
    /// identity fields (email, username, password) left out entirely — the
    /// type has no properties for them, so there's nothing for a caller to
    /// accidentally copy. Used by "Duplicate…" (Settings > Accounts) and
    /// "Copy settings from": both create a NEW
    /// account, so the source's address/credentials must never carry over,
    /// only the connection settings that were tedious to retype (host, ports,
    /// TLS). SMTP username/password are likewise left for the sheet's normal
    /// auto-follow-the-IMAP-fields behaviour rather than copied, for the same
    /// reason. Pure — never mutates `self`.
    func duplicateFields() -> AccountDuplicateFields {
        let smtp = effectiveSMTP
        return AccountDuplicateFields(
            displayName: displayName ?? "",
            host: host,
            port: String(port),
            useTLS: useTls,
            smtpHost: smtp.host,
            smtpHostAuto: smtp.host == Self.defaultSMTPHost(imapHost: host),
            smtpPort: String(smtp.port),
            smtpUseTLS: smtp.useTls,
            imapSecurity: effectiveIMAPSecurity
        )
    }

    /// Config JSON for `castellum_send_pending` (SendConfig shape). Built with
    /// JSONSerialization so all values are escaped safely.
    /// `plainTextFlowed` (MM-17 S2, the Settings > General toggle) is written as
    /// `plain_text_flowed`; the core defaults it to false when the key is absent.
    func sendConfigJSON(plainTextFlowed: Bool = false) throws -> String {
        let smtp = effectiveSMTP
        var dict: [String: Any] = [
            "account_id": accountId,
            "from_email": email.isEmpty ? username : email,
            "smtp": [
                "host": smtp.host,
                "port": smtp.port,
                "username": smtp.username,
                "password": smtp.password,
                "use_tls": smtp.useTls,
                "allow_invalid_certs": allowInvalidCerts ?? false,
            ] as [String: Any],
        ]
        if let name = displayName, !name.isEmpty {
            dict["from_name"] = name
        }
        dict["plain_text_flowed"] = plainTextFlowed
        let data = try JSONSerialization.data(withJSONObject: dict)
        guard let json = String(data: data, encoding: .utf8) else {
            throw CastellumError.internalError
        }
        return json
    }
}

// MARK: - reply prefill

enum ReplyMode {
    case reply
    case replyAll
    case forward
}

/// The compose-window prefill for replying to a message. Pure so it's testable.
/// `body` carries the quoted original (attribution + `> `-prefixed lines) below
/// two leading newlines, so the caret starts at the top (Superhuman style) —
/// text-only quoting in v1 (no HTML). Per-account plain-text signatures are
/// layered on afterwards by `SignatureSupport` (see below).
/// Per-account text formats a reply is built with (MM-7). The default value
/// keeps today's behaviour exactly: the built-in `On <date>, <who> wrote:`.
struct ReplyTemplates: Equatable {
    /// Custom attribution format (see `TemplateFormatter`); nil = built-in line.
    var attribution: String? = nil
    /// Custom forward header block (see `TemplateFormatter`, multi-line); nil = the built-in
    /// `From:` / `Date:` / `Subject:` lines under the fixed separator.
    var forwardHeader: String? = nil
    /// Re-wrap hard-wrapped prose in the quoted original (MM-17 decision M3); conservative, see
    /// `PlainTextStructure.rewrapped`. The hidden `quoteRewrapFixed` preference turns it off.
    var rewrapQuotes: Bool = true
    var myName: String? = nil
    var locale: Locale = .current
    var timeZone: TimeZone = .current
    var now: Date = Date()

    static let builtIn = ReplyTemplates()
}

struct ReplyPrefill: Equatable {
    var to: [DraftAddress]
    /// Reply-All only: the original Cc recipients (kept on Cc, not moved to To).
    var cc: [DraftAddress] = []
    var subject: String
    var inReplyTo: String?
    var referencesHeader: String?
    /// The database id of the original message (e.g. `"gmail:acct:42"`), used
    /// by the AI draft-reply feature. Distinct from `inReplyTo`, which is the
    /// RFC 2822 Message-ID header. Nil for non-reply prefills.
    var originalDbId: String? = nil
    /// True only for Forward: carries the original's attachments, no threading.
    var isForward: Bool = false
    /// How many attachments the forwarded original has (0 for Reply).
    var forwardAttachmentCount: Int = 0
    /// The original's To then Cc addresses (reply / reply-all only; empty for a
    /// forward): what "the address the mail was written to" is chosen from
    /// (`IdentitySelector`).
    var originalRecipients: [String] = []
    var body: String = ""
    /// The attribution line written into `body` (reply only; nil when there
    /// is none). `ForgottenAttachment` must not scan it: a custom line need not
    /// end in `wrote:` and may contain a trigger word.
    var attributionLine: String? = nil

    /// Reply: To = the original's Reply-To when it has one, else its From; but a
    /// reply to a message `selfEmail` sent goes to that message's To recipients
    /// (minus self, falling back to the sender, e.g. a note to self).
    /// Reply-All: those targets + From (when Reply-To replaced it) + To + Cc
    /// minus `selfEmail`, de-duplicated
    /// (falling back to the sender if that leaves nobody).
    /// Subject gets a single `Re: ` prefix (never doubled).
    ///
    /// Known v1 limitation: `referencesHeader` carries only the parent's
    /// Message-ID. RFC 2822 wants the parent's full References chain + its id,
    /// but `MessageDetail` doesn't expose `references` yet (needs a small
    /// Rust-core follow-up). Correct for replies to thread roots; deep-thread
    /// replies may not slot into the exact position in other clients.
    ///
    /// `aliases` are the user's other addresses on this account (identities);
    /// like `selfEmail` they are dropped from Reply and Reply-All recipients.
    static func make(detail: MessageDetail, mode: ReplyMode, selfEmail: String,
                     aliases: [String] = [],
                     templates: ReplyTemplates = .builtIn,
                     selection: String? = nil) -> ReplyPrefill {
        var senders = detail.addresses
            .filter { $0.kind == "from" }
            .map { DraftAddress(name: $0.name, email: $0.address) }
        if senders.isEmpty, let a = detail.header.fromAddress {
            senders = [DraftAddress(name: detail.header.fromName, email: a)]
        }

        if mode == .forward {
            return forwardPrefill(detail: detail, senders: senders, templates: templates)
        }

        // Where an answer goes: Reply-To wins over From (lists, ticket systems).
        let replyTos = detail.addresses
            .filter { $0.kind == "reply-to" }
            .map { DraftAddress(name: $0.name, email: $0.address) }
        let targets = replyTos.isEmpty ? senders : replyTos
        let selfLower = selfEmail.lowercased()
        let selfSet = Set(([selfEmail] + aliases).map { $0.lowercased() })
        var to = targets
        if mode == .reply, !senders.isEmpty, replyTos.isEmpty,
           senders.allSatisfy({ selfSet.contains($0.email.lowercased()) }) {
            // Replying to my own message: answer the people I wrote to.
            var seenTo = Set<String>()
            let others = detail.addresses
                .filter { $0.kind == "to" && !selfSet.contains($0.address.lowercased()) }
                .filter { seenTo.insert($0.address.lowercased()).inserted }
                .map { DraftAddress(name: $0.name, email: $0.address) }
            if !others.isEmpty { to = others }
        }
        var cc: [DraftAddress] = []
        if mode == .replyAll {
            // Reply-To redirects the answer but never removes the original
            // sender from a reply-all: they stay in the visible To field.
            // The original To recipients stay on To and the original Cc
            // recipients stay on Cc (audit #3443); the user's own address and
            // aliases are dropped from both, and nobody appears twice.
            if !replyTos.isEmpty { to += senders }
            to += detail.addresses
                .filter { $0.kind == "to" }
                .map { DraftAddress(name: $0.name, email: $0.address) }
            var seen = Set<String>()
            func keep(_ addr: DraftAddress) -> Bool {
                let e = addr.email.lowercased()
                guard !selfSet.contains(e), !seen.contains(e) else { return false }
                seen.insert(e)
                return true
            }
            to = to.filter(keep)
            cc = detail.addresses
                .filter { $0.kind == "cc" }
                .map { DraftAddress(name: $0.name, email: $0.address) }
                .filter(keep)
            if to.isEmpty && cc.isEmpty { to = senders } // replying to your own message
            else if to.isEmpty { to = cc; cc = [] }      // never leave To empty with Cc set
        }

        let base = (detail.header.subject ?? "").trimmingCharacters(in: .whitespaces)
        let subject = base.lowercased().hasPrefix("re:") ? base : "Re: " + base

        let senderDisplay = senders.first.map { a -> String in
            if let name = a.name, !name.isEmpty { return name }
            return a.email
        }

        let recipients = ["to", "cc"].flatMap { kind in
            detail.addresses.filter { $0.kind == kind }.map(\.address)
        }
        let attribution = attributionLine(
            detail: detail, sender: senders.first, senderDisplay: senderDisplay,
            writingTo: to.first, selfEmail: selfEmail, templates: templates)
        let quotedBody = quotedReplyBody(
            original: quotedOriginal(textBody: detail.textBody, selection: selection, rewrap: templates.rewrapQuotes),
            attributionLine: attribution)
        return ReplyPrefill(
            to: to,
            cc: cc,
            subject: subject,
            inReplyTo: detail.rfc822MessageId,
            referencesHeader: referencesChain(
                parentReferences: detail.references ?? [], parentMessageId: detail.rfc822MessageId),
            originalDbId: detail.header.id,
            originalRecipients: recipients,
            body: quotedBody,
            attributionLine: quotedBody.isEmpty || attribution.isEmpty ? nil : attribution
        )
    }

    /// The single attribution line for a reply: the account's custom format
    /// rendered against the original, or the built-in `On <date>, <who> wrote:`.
    /// A custom format that renders empty gives "" (no attribution line).
    static func attributionLine(
        detail: MessageDetail, sender: DraftAddress?, senderDisplay: String?,
        writingTo: DraftAddress?, selfEmail: String, templates: ReplyTemplates
    ) -> String {
        guard let format = templates.attribution else {
            return builtInAttribution(senderDisplay: senderDisplay, sentAt: detail.header.sentAt)
        }
        let context = TemplateContext(
            senderName: sender?.name, senderEmail: sender?.email,
            sentAt: detail.header.sentAt, subject: detail.header.subject,
            recipientName: writingTo?.name, recipientEmail: writingTo?.email,
            myName: templates.myName, myEmail: selfEmail,
            now: templates.now, locale: templates.locale, timeZone: templates.timeZone)
        return TemplateFormatter.render(format, context: context, mode: .singleLine).text
    }

    /// RFC 5322 3.6.4: a reply's `References` is the parent's `References` followed by the
    /// parent's `Message-ID`, so deep threads keep threading in clients that thread on References
    /// (#3443). De-duplicated; a very long chain keeps the root plus the most recent ids (the
    /// usual trimming, header lines stay a sane length). Nil when there is nothing to reference.
    static func referencesChain(parentReferences: [String], parentMessageId: String?) -> String? {
        var ids: [String] = []
        for raw in parentReferences + [parentMessageId ?? ""] {
            let id = raw.trimmingCharacters(in: .whitespacesAndNewlines)
            if !id.isEmpty, !ids.contains(id) { ids.append(id) }
        }
        guard !ids.isEmpty else { return nil }
        let maxIds = 20
        if ids.count > maxIds { ids = [ids[0]] + ids.suffix(maxIds - 1) }
        return ids.joined(separator: " ")
    }

    /// Forward: empty To, a single `Fwd: ` subject prefix, no threading
    /// headers (a forward starts a new conversation), the original quoted
    /// below a "Forwarded message" header, and a note of how many attachments
    /// to carry over. Text-only in v1, like replies.
    private static func forwardPrefill(
        detail: MessageDetail, senders: [DraftAddress], templates: ReplyTemplates = .builtIn
    ) -> ReplyPrefill {
        let original = (detail.header.subject ?? "").trimmingCharacters(in: .whitespaces)
        let subject = original.lowercased().hasPrefix("fwd:") ? original : "Fwd: " + original
        let from = senders.first.map { a -> String in
            if let name = a.name, !name.isEmpty { return "\(name) <\(a.email)>" }
            return a.email
        } ?? ""
        // The separator line is fixed (the forgotten-attachment scan and tests key on it);
        // a custom header replaces only the From/Date/Subject lines under it (MM-7 slice 4).
        var lines = ["", "", ForgottenAttachment.forwardedSeparator]
        if let format = templates.forwardHeader {
            let context = TemplateContext(
                senderName: senders.first?.name, senderEmail: senders.first?.email,
                sentAt: detail.header.sentAt, subject: original.isEmpty ? nil : original,
                recipientName: nil, recipientEmail: nil,
                myName: templates.myName, myEmail: nil,
                now: templates.now, locale: templates.locale, timeZone: templates.timeZone)
            let rendered = TemplateFormatter.render(format, context: context, mode: .multiLine).text
                .trimmingCharacters(in: .whitespacesAndNewlines)
            if !rendered.isEmpty { lines.append(contentsOf: rendered.components(separatedBy: .newlines)) }
        } else {
            if !from.isEmpty { lines.append("From: \(from)") }
            if let sent = detail.header.sentAt {
                lines.append("Date: " + attributionDateFormatter.string(
                    from: Date(timeIntervalSince1970: TimeInterval(sent))))
            }
            lines.append("Subject: \(original)")
        }
        lines.append("")
        lines.append(detail.textBody ?? "")
        return ReplyPrefill(
            to: [],
            subject: subject,
            inReplyTo: nil,
            referencesHeader: nil,
            originalDbId: detail.header.id,
            isForward: true,
            forwardAttachmentCount: detail.attachments?.count ?? 0,
            body: lines.joined(separator: "\n")
        )
    }

    /// What a reply quotes: the text the user selected in the reading pane
    /// when there is any (a blank selection counts as none); otherwise the whole message WITHOUT the
    /// sender's signature (stripped only on the strict `-- ` delimiter). Either way hard-wrapped prose
    /// is re-wrapped into paragraphs when `rewrap` is on. Forward never calls this: it keeps the whole message.
    static func quotedOriginal(textBody: String?, selection: String?, rewrap: Bool) -> String? {
        let source: String
        if let selection, !selection.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            source = selection
        } else if let textBody {
            source = PlainTextStructure.strippingSignature(textBody)
        } else {
            return nil
        }
        return rewrap ? PlainTextStructure.rewrapped(source) : source
    }

    /// Formats the original message's date inside the reply attribution.
    /// Static so tests build the exact expected string regardless of the
    /// machine's locale/timezone.
    static let attributionDateFormatter: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .medium
        f.timeStyle = .short
        return f
    }()

    /// Attribution + line-quoted original for a reply body. Pure and total:
    /// - nil/blank original -> "" (no attribution block over nothing)
    /// - each original line is prefixed `> `; lines already quoted (`>`) get a
    ///   bare `>` so nesting reads `>>` rather than `> >`
    /// - no date -> attribution without the "On <date>, " clause
    /// - no sender -> attribution without a name ("wrote:" keeps its shape via
    ///   the date alone); long bodies are never truncated (v1)
    /// - two leading newlines so the user types at the top (caret at start).
    static func quotedReplyBody(original: String?,
                                senderDisplay: String?,
                                sentAt: Int64?) -> String {
        quotedReplyBody(
            original: original,
            attributionLine: builtInAttribution(senderDisplay: senderDisplay, sentAt: sentAt))
    }

    /// The built-in attribution line (`On <date>, <who> wrote:` and its
    /// degenerate forms); "" when there is neither a date nor a sender.
    static func builtInAttribution(senderDisplay: String?, sentAt: Int64?) -> String {
        let date = sentAt.map {
            attributionDateFormatter.string(from: Date(timeIntervalSince1970: TimeInterval($0)))
        }
        switch (date, senderDisplay) {
        case let (.some(d), .some(who)): return "On \(d), \(who) wrote:"
        case let (nil, .some(who)): return "\(who) wrote:"
        case let (.some(d), nil): return "On \(d):"
        case (nil, nil): return ""
        }
    }

    /// Line-quoted original under `attributionLine` ("" = no attribution).
    /// Shared by the built-in and the custom-format paths.
    static func quotedReplyBody(original: String?, attributionLine attribution: String) -> String {
        guard let original,
              !original.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
            return ""
        }

        let trimmed = original.trimmingCharacters(in: .whitespacesAndNewlines)
        let quoted = trimmed
            .components(separatedBy: "\n")
            .map { $0.hasPrefix(">") ? ">\($0)" : "> \($0)" }
            .joined(separator: "\n")

        let block = attribution.isEmpty ? quoted : attribution + "\n" + quoted
        return "\n\n" + block
    }

    /// The To field's text-form for the compose window.
    var toText: String { AddressFormatter.text(to) }
    /// The Cc field's text-form for the compose window.
    var ccText: String { AddressFormatter.text(cc) }

    /// Insert an AI suggestion above the quoted attribution block.
    ///
    /// A reply body from `quotedReplyBody` starts with `"\n\n"` (two leading
    /// newlines) before the attribution line. This function places `suggestion`
    /// before that separator, preserving the full attribution and quote block.
    ///
    /// Rules:
    ///   - Body contains `"\n\n"` → `suggestion + body[from "\n\n" onwards]`.
    ///     Any text the user typed above the `"\n\n"` is replaced by the
    ///     suggestion ("insert at top, above the quoted block").
    ///   - Body is blank/whitespace-only → `suggestion` (new-message case).
    ///   - Body has content but no `"\n\n"` → `suggestion + "\n\n" + trimmedBody`.
    static func insertAboveQuote(suggestion: String, in body: String) -> String {
        if let sepRange = body.range(of: "\n\n") {
            // Keep "\n\n" and everything after it (attribution + quote).
            return suggestion + String(body[sepRange.lowerBound...])
        }
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? suggestion : suggestion + "\n\n" + trimmed
    }
}

// MARK: - per-account signatures (S26)

/// Pure logic for inserting, swapping, and detecting a per-account plain-text
/// signature in a compose body. No UI, no storage: the signature TEXT itself
/// comes from `Preferences` (Settings > Accounts); this only shapes the body
/// string. Kept separate from `ReplyPrefill.insertAboveQuote` (the AI-suggestion
/// equivalent) because the two run at different points in the compose
/// lifecycle and must not be confused with each other.
enum SignatureSupport {
    /// The standard plain-text signature delimiter (a line reading exactly
    /// `-- `, i.e. dash-dash-space, so mail clients that recognise it can
    /// strip signatures from quoted replies), followed by the signature text.
    static let delimiter = "-- \n"

    /// Formats `signature` into the delimited block inserted into a compose
    /// body. An empty (or whitespace-only) signature formats to `""` — no
    /// block, nothing inserted, nothing to detect.
    static func block(for signature: String) -> String {
        let trimmed = signature.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "" }
        return delimiter + trimmed
    }

    /// Start index of the first line beginning with `>` — the marker
    /// `ReplyPrefill.quotedReplyBody` uses for every quoted original-message
    /// line (nested quotes stay `>`-prefixed too). Nil when `body` has no
    /// quoted line at all, i.e. it's a genuine new message.
    private static func firstQuoteLineStart(in body: String) -> String.Index? {
        var found: String.Index?
        body.enumerateSubstrings(in: body.startIndex..<body.endIndex, options: .byLines) { substring, range, _, stop in
            if let substring, substring.hasPrefix(">") {
                found = range.lowerBound
                stop = true
            }
        }
        return found
    }

    /// The blank-line gap (`"\n\n"`) immediately above the quoted-reply block
    /// (attribution line, if any, + the `>`-prefixed quote), if `body` has
    /// one. A plain first-`"\n\n"` search would instead find a blank line
    /// between two of the user's OWN paragraphs when there's more than one —
    /// this walks back from the quote itself so a signature (or the
    /// account-swap logic below) only ever anchors to the real quote
    /// boundary. Nil when `body` has no quoted line (new message).
    private static func quoteBlockBoundary(in body: String) -> String.Index? {
        guard let quoteLineStart = firstQuoteLineStart(in: body) else { return nil }
        let prefix = body[body.startIndex..<quoteLineStart]
        guard let sepRange = prefix.range(of: "\n\n", options: .backwards) else { return nil }
        return sepRange.lowerBound
    }

    /// Inserts `signature`'s block into a fresh compose body (new message or
    /// reply prefill), above any quoted text.
    ///
    /// Reply prefills (`ReplyPrefill.quotedReplyBody`) start with `"\n\n"`
    /// before the attribution/quote block, exactly like the AI-suggestion
    /// insertion point (`insertAboveQuote`); the signature block is placed at
    /// that same separator — found via `quoteBlockBoundary`, not a naive
    /// first-`"\n\n"` search, so several paragraphs of the user's own text
    /// above the quote never get the signature wedged in the middle — so the
    /// shape is: `<caret>\n\n-- \n<signature>\n\n<attribution>\n> quoted…`.
    ///
    /// A blank new-message body (no quoted text, no content) becomes
    /// `"\n\n" + block`: two empty leading lines for the caret, signature
    /// below — the same caret-at-top shape a reply gets. Non-empty content
    /// with no quoted text (a plain new message, however many paragraphs)
    /// gets the block appended at the very end, after a blank line.
    ///
    /// An empty signature is a total no-op: `body` is returned unchanged.
    static func insert(signature: String, into body: String) -> String {
        let block = block(for: signature)
        guard !block.isEmpty else { return body }
        if let boundary = quoteBlockBoundary(in: body) {
            return String(body[body.startIndex..<boundary])
                + "\n\n" + block + String(body[boundary...])
        }
        let trimmed = body.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? "\n\n" + block : trimmed + "\n\n" + block
    }

    /// Where the text the user writes ends: the start of the signature block or of the
    /// quoted reply, whichever comes first (together with the blank-line separator before
    /// it), else the end of `body`. A snippet is inserted at or before this point, never
    /// inside a signature or a quote.
    static func authoredRegionEnd(in body: String) -> String.Index {
        var end = body.endIndex
        if let sig = structuralSignatureRange(in: body) { end = min(end, sig.lowerBound) }
        if let quote = quoteBlockBoundary(in: body) { end = min(end, quote) }
        // A leading "\n\n" separator belongs to the block below, not to the user's text.
        if end > body.startIndex, body[body.startIndex..<end].hasSuffix("\n\n") {
            end = body.index(end, offsetBy: -2)
        }
        return end
    }

    /// True when `body` still contains `signature`'s exact, unmodified block —
    /// i.e. the user has not edited (or deleted) it since it was inserted.
    /// An empty `signature` has no block to find, so this is always `false`
    /// for it (there is nothing to detect as "unmodified").
    static func hasUnmodifiedBlock(_ signature: String, in body: String) -> Bool {
        let block = block(for: signature)
        guard !block.isEmpty else { return false }
        return body.contains(block)
    }

    /// The exact structural region a signature block occupies in `body`: from
    /// the standard delimiter through the end of the signature text — i.e. up
    /// to the blank-line gap above any quoted text (`quoteBlockBoundary`), or
    /// the end of `body` when there is none. Nil when `body` has no delimiter
    /// at all (the signature line was deleted outright).
    private static func structuralSignatureRange(in body: String) -> Range<String.Index>? {
        guard let delimRange = delimiterLineRange(in: body) else { return nil }
        if let boundary = quoteBlockBoundary(in: body), boundary >= delimRange.upperBound {
            return delimRange.lowerBound..<boundary
        }
        return delimRange.lowerBound..<body.endIndex
    }

    /// Lines that read as a signature delimiter: the standard `-- ` plus the
    /// en/em dash a text system's smart-dash substitution turns it into
    /// (: a body showing `—` for the delimiter made the swap
    /// logic think there was no signature and add another).
    private static let delimiterLookalikes: Set<String> = ["--", "\u{2013}", "\u{2014}"]

    /// Range of the first line (including its newline) that is a delimiter or
    /// one of its smart-dash look-alikes. Quoted lines start with `>` so never
    /// match. Nil when the body has no such line.
    private static func delimiterLineRange(in body: String) -> Range<String.Index>? {
        var found: Range<String.Index>?
        body.enumerateSubstrings(in: body.startIndex..<body.endIndex, options: .byLines) {
            substring, _, enclosing, stop in
            guard let substring, delimiterLookalikes.contains(trimmedTrailing(substring)) else { return }
            found = enclosing
            stop = true
        }
        return found
    }

    /// `region` (delimiter line + signature text) with its first line dropped,
    /// so a smart-dash delimiter still compares equal to the standard block.
    private static func droppingDelimiterLine(_ region: String) -> String {
        guard let nl = region.firstIndex(where: \.isNewline) else { return "" }
        return String(region[region.index(after: nl)...])
    }

    /// Equal except for trailing whitespace — trailing spaces/newlines an
    /// editor can leave behind don't count as a user edit.
    private static func trimmedTrailing(_ s: String) -> String {
        var result = Substring(s)
        while let last = result.last, last.isWhitespace {
            result.removeLast()
        }
        return String(result)
    }

    /// Called when the compose window's From account changes. Swaps
    /// `oldSignature`'s block for `newSignature`'s — but ONLY when the
    /// signature block, taken structurally (delimiter through the end of the
    /// signature text — see `structuralSignatureRange`), is still EXACTLY
    /// `oldSignature`'s untouched block (trailing whitespace aside). A plain
    /// substring search would still match when the user has only appended
    /// text after the signature (the block is a prefix of what's there), and
    /// swapping would then clobber that appended text; comparing the whole
    /// structural region instead means any edit at all — in the middle, or
    /// merely appended after — leaves `body` untouched.
    ///
    /// When there was no old block to begin with (`oldSignature` empty, or
    /// never inserted) and `newSignature` is non-empty, a fresh block is
    /// inserted via `insert(signature:into:)` — switching to an account that
    /// signs its mail should add a signature exactly where a fresh insert
    /// would put it (end of the user's text, or above the quote), never
    /// wedged between paragraphs.
    static func swapForAccountChange(from oldSignature: String,
                                     to newSignature: String,
                                     in body: String) -> String {
        let oldBlock = block(for: oldSignature)
        guard !oldBlock.isEmpty else {
            // A body that already carries a delimiter line (even one a smart
            // dash mangled to an em dash) already has a signature: never
            // stack a second one under it.
            if delimiterLineRange(in: body) != nil { return body }
            return insert(signature: newSignature, into: body)
        }
        guard let range = structuralSignatureRange(in: body) else {
            // No delimiter at all — removed outright by the user.
            return body
        }
        guard trimmedTrailing(droppingDelimiterLine(String(body[range])))
                == trimmedTrailing(droppingDelimiterLine(oldBlock)) else {
            // Edited in any way (including appended-to) — leave it alone.
            return body
        }
        return body.replacingCharacters(in: range, with: block(for: newSignature))
    }
}

/// Renders `DraftAddress`es back into field text that round-trips through
/// `AddressParser` (names containing a separator are quoted).
enum AddressFormatter {
    /// RFC 5322 specials that force a display name into a quoted-string.
    private static let specials = CharacterSet(charactersIn: "()<>[]:;@\\,.\"")

    /// `Name <addr>` for one mailbox (From picker, account labels), or the bare
    /// address when the name is nil/blank. A name with an RFC 5322 special
    /// (comma, quote, dot, …) becomes a quoted-string with `"` and `\` escaped;
    /// non-ASCII letters (Catalan accents) stay as typed (RFC 6532). The SMTP
    /// provider does the wire encoding (RFC 2047) independently.
    static func header(name: String?, email: String) -> String {
        guard let name = name?.trimmingCharacters(in: .whitespacesAndNewlines),
              !name.isEmpty else { return email }
        guard name.rangeOfCharacter(from: specials) != nil else { return "\(name) <\(email)>" }
        let escaped = name
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        return "\"\(escaped)\" <\(email)>"
    }

    static func text(_ addresses: [DraftAddress]) -> String {
        addresses.map { addr in
            guard let name = addr.name, !name.isEmpty else { return addr.email }
            let needsQuoting = name.contains(",") || name.contains(";") || name.contains("\"")
                || name.contains("\\")
            let display = needsQuoting
                ? "\"\(name.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))\""
                : name
            return "\(display) <\(addr.email)>"
        }
        .joined(separator: ", ")
    }
}

/// One identity/alias chosen in the compose From picker (S47) — just the
/// two fields `NewDraft.fromEmail`/`fromName` need, so `ComposeSession`
/// never has to look an `Identity.id` back up against `AppModel.identities`.
struct ComposeIdentity: Equatable {
    var email: String
    var name: String?
}
