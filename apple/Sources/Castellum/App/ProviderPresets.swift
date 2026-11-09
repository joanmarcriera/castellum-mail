// Castellum - mail provider presets for account setup.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Typing an address at a well-known provider fills in the IMAP/SMTP host,
// ports and TLS, so the user doesn't need to know server names. The static
// table here needs no network. For any OTHER domain, MM-14 adds an optional
// lookup (`AutoconfigPolicy` + the Rust `castellum_autoconfig_lookup`: DNS
// SRV, MX hint, the domain's own autoconfig, Mozilla ISPDB) whose report maps
// back onto the same `ProviderDetection` cases, so the one "never overwrite
// what the user typed" rule (`PresetFields.mayAutoApply`) covers both. The
// sheet's "Use settings for" menu remains the manual override.
//
// Every supported preset here uses password (app-password) auth, which is
// all Castellum speaks today. Microsoft (Outlook.com / Hotmail / Live /
// Microsoft 365) is deliberately NOT a preset: Microsoft retired password
// sign-in for these accounts, so they need OAuth2/XOAUTH2 —
// (S37), not built yet. Those domains are recognised only to say so plainly
// instead of letting the user discover it as an opaque sign-in failure.
//
// Hosts and ports are the providers' published IMAP/SMTP settings. SMTP
// port 465 means implicit TLS and 587 means STARTTLS in the core's SMTP
// sender (`LettreSmtpSender::new`), so `smtpUseTLS` is true for both.

import Foundation

struct ProviderPreset: Equatable, Identifiable {
    let id: String
    let name: String
    let domains: [String]
    let imapHost: String
    let imapPort: Int
    let smtpHost: String
    let smtpPort: Int
    /// One line shown under the email field: what the user must do on the
    /// provider's side (app password / enable IMAP).
    let note: String

    static let gmail = ProviderPreset(
        id: "gmail", name: "Gmail", domains: ["gmail.com", "googlemail.com"],
        imapHost: "imap.gmail.com", imapPort: 993, smtpHost: "smtp.gmail.com", smtpPort: 587,
        note: "Use a Google app password (Google Account › Security › App passwords), not your normal password.")

    static let icloud = ProviderPreset(
        id: "icloud", name: "iCloud Mail", domains: ["icloud.com", "me.com", "mac.com"],
        imapHost: "imap.mail.me.com", imapPort: 993, smtpHost: "smtp.mail.me.com", smtpPort: 587,
        note: "Use an app-specific password from account.apple.com › Sign-In and Security.")

    static let fastmail = ProviderPreset(
        id: "fastmail", name: "Fastmail", domains: ["fastmail.com", "fastmail.fm"],
        imapHost: "imap.fastmail.com", imapPort: 993, smtpHost: "smtp.fastmail.com", smtpPort: 465,
        note: "Use a Fastmail app password (Settings › Privacy & Security › App passwords).")

    static let yahoo = ProviderPreset(
        id: "yahoo", name: "Yahoo Mail", domains: ["yahoo.com", "yahoo.co.uk", "ymail.com"],
        imapHost: "imap.mail.yahoo.com", imapPort: 993, smtpHost: "smtp.mail.yahoo.com", smtpPort: 465,
        note: "Use a Yahoo app password (Account Security › Generate app password).")

    /// Presets offered in the sheet, in menu order.
    static let all: [ProviderPreset] = [.gmail, .icloud, .fastmail, .yahoo]

    /// The preset with this id (`gmail`, `icloud`, `fastmail`, `yahoo`) -- the
    /// ids the Rust MX table can return. Nil for anything else.
    static func byId(_ id: String) -> ProviderPreset? { all.first { $0.id == id } }

    /// Consumer Microsoft domains — recognised, not supported (see header).
    static let microsoftDomains: Set<String> = [
        "outlook.com", "hotmail.com", "live.com", "msn.com",
        "hotmail.co.uk", "live.co.uk", "outlook.es", "hotmail.es",
    ]
}

/// Server settings found by the MM-14 lookup for a domain with no preset.
/// Nothing is connected: these only pre-fill the form for the user to check.
struct DiscoveredSettings: Equatable {
    /// The mail domain that was looked up.
    var domain: String
    /// Rust source id: srv | domain_autoconfig | well_known | ispdb.
    var source: String
    var displayName: String?
    var imapHost: String
    var imapPort: Int
    /// `.tls` or `.starttls` only; detection never yields cleartext.
    var imapSecurity: IMAPSecurity
    var smtpHost: String?
    var smtpPort: Int?
    var username: UsernameForm?
}

/// What the typed address tells us about its provider.
enum ProviderDetection: Equatable {
    /// Unknown domain: generic IMAP, the user enters the servers.
    case generic
    case preset(ProviderPreset)
    /// Found by lookup (not the static table).
    case discovered(DiscoveredSettings)
    /// A provider Castellum recognises but can't connect to yet.
    case unsupported(name: String, reason: String)

    static func detect(email: String) -> ProviderDetection {
        let trimmed = email.trimmingCharacters(in: .whitespaces).lowercased()
        guard let at = trimmed.lastIndex(of: "@") else { return .generic }
        let domain = String(trimmed[trimmed.index(after: at)...])
        guard !domain.isEmpty else { return .generic }
        if ProviderPreset.microsoftDomains.contains(domain) {
            return .unsupported(
                name: "Outlook.com",
                reason: "Outlook.com and Microsoft 365 accounts need Microsoft sign-in, which Castellum doesn’t support yet — it’s planned.")
        }
        if let preset = ProviderPreset.all.first(where: { $0.domains.contains(domain) }) {
            return .preset(preset)
        }
        return .generic
    }
}

/// The sheet's connection fields a preset fills in. Pure mapping so the
/// "only overwrite what the user hasn't typed" rule is testable.
struct PresetFields: Equatable {
    var host: String
    var port: String
    var useTLS: Bool
    var smtpHost: String
    var smtpPort: String
    var smtpUseTLS: Bool
    /// IMAP security mode (TLS or STARTTLS); `useTLS` is true for both.
    var imapSecurity: IMAPSecurity = .tls

    init(_ preset: ProviderPreset) {
        host = preset.imapHost
        port = String(preset.imapPort)
        useTLS = true
        smtpHost = preset.smtpHost
        smtpPort = String(preset.smtpPort)
        smtpUseTLS = true
    }

    /// Fields for a looked-up result. A missing SMTP server leaves the SMTP
    /// host to follow the IMAP host as it does for a hand-entered account.
    init(_ d: DiscoveredSettings) {
        host = d.imapHost
        port = String(d.imapPort)
        useTLS = true
        imapSecurity = d.imapSecurity
        smtpHost = d.smtpHost ?? AccountConfig.defaultSMTPHost(imapHost: d.imapHost)
        smtpPort = String(d.smtpPort ?? 587)
        smtpUseTLS = true
    }

    /// Which of the secondary fields a (late) auto-detection may still write.
    struct ApplyMask: Equatable {
        var port = true, security = true, smtpHost = true, smtpPort = true, smtpTLS = true
        static let all = ApplyMask()
    }

    /// A secondary field is replaceable only while it still holds its pristine
    /// default or exactly what the previous auto-fill put there; anything else
    /// is something the user typed and is left alone (the host is gated
    /// separately by `mayAutoApply`). `smtpHostAuto` is the sheet's own
    /// "SMTP host still follows the IMAP host" flag.
    static func mask(current: PresetFields, lastApplied: PresetFields?, smtpHostAuto: Bool) -> ApplyMask {
        func free<T: Equatable>(_ cur: T, _ pristine: T, _ last: T?) -> Bool { cur == pristine || cur == last }
        return ApplyMask(
            port: free(current.port, "993", lastApplied?.port),
            security: free(current.imapSecurity, .tls, lastApplied?.imapSecurity),
            smtpHost: smtpHostAuto || current.smtpHost.isEmpty || current.smtpHost == lastApplied?.smtpHost,
            smtpPort: free(current.smtpPort, "587", lastApplied?.smtpPort),
            smtpTLS: free(current.smtpUseTLS, true, lastApplied?.smtpUseTLS))
    }

    /// True when auto-detection may (re)apply a preset: the host is still
    /// empty, or still exactly what a preset last filled in. Anything the
    /// user typed themselves is never overwritten by detection — only an
    /// explicit "Use settings for" choice does that.
    static func mayAutoApply(currentHost: String, lastAppliedHost: String?) -> Bool {
        currentHost.isEmpty || currentHost == lastAppliedHost
    }
}

/// Pure rules for the MM-14 lookup (testable without a sheet or the network).
enum AutoconfigPolicy {
    /// The ASCII (punycode) domain to look up for `email`, or nil when no
    /// lookup should happen: no `@`, empty local part, an incomplete domain
    /// (no dot, final label under 2 letters), a domain the static table
    /// already knows (gmail.com, outlook.com...), or one that will not convert
    /// to ASCII. Only the DOMAIN is returned, so the local part can never be
    /// passed on to the lookup by mistake.
    static func lookupDomain(email: String) -> String? {
        let trimmed = email.trimmingCharacters(in: .whitespaces).lowercased()
        guard let at = trimmed.lastIndex(of: "@") else { return nil }
        let local = trimmed[..<at]
        let domain = String(trimmed[trimmed.index(after: at)...])
        guard !local.isEmpty, !local.contains("@"), !domain.isEmpty, !domain.contains("@"),
              domain.rangeOfCharacter(from: .whitespacesAndNewlines) == nil
        else { return nil }
        guard case .generic = ProviderDetection.detect(email: trimmed) else { return nil }
        guard let ascii = asciiDomain(domain) else { return nil }
        let labels = ascii.split(separator: ".", omittingEmptySubsequences: false)
        guard labels.count >= 2, labels.allSatisfy({ !$0.isEmpty }) else { return nil }
        let last = labels[labels.count - 1]
        // Reserved names (RFC 2606/6761) never have public settings; refusing
        // them also keeps fixtures and tests off the network.
        guard !["test", "example", "invalid", "localhost", "local"].contains(String(last)) else { return nil }
        guard last.count >= 2, last.hasPrefix("xn--") || last.allSatisfy({ $0.isLetter && $0.isASCII })
        else { return nil }
        return ascii
    }

    /// True when a result obtained for `domain` still belongs to the address
    /// now typed. A late answer for a domain the user has since edited away
    /// from is dropped, never applied.
    static func accepts(resultFor domain: String, currentEmail email: String) -> Bool {
        lookupDomain(email: email) == domain
    }

    /// IDNA/punycode form of `domain` via Foundation's URL machinery; nil when
    /// it cannot be expressed as ASCII (the core accepts nothing else).
    static func asciiDomain(_ domain: String) -> String? {
        if domain.allSatisfy({ $0.isASCII }) { return domain }
        var comps = URLComponents()
        comps.scheme = "https"
        comps.host = domain
        guard let host = comps.url?.host?.lowercased(), host.allSatisfy({ $0.isASCII }) else { return nil }
        return host
    }

    /// Human label for the source line ("Settings found for x (DNS)").
    static func sourceLabel(_ source: String?) -> String {
        switch source {
        case "srv": return String(localized: "DNS")
        case "domain_autoconfig", "well_known": return String(localized: "the domain’s own settings")
        case "ispdb": return String(localized: "Mozilla’s provider database")
        default: return String(localized: "lookup")
        }
    }

    /// Map a core report onto the detection cases the sheet already renders.
    /// Anything that is not a usable answer is `.generic` (the form behaves as
    /// it does with no lookup). `unsupported` reuses the Microsoft wording.
    static func detection(from report: AutoconfigReport, domain: String) -> ProviderDetection {
        switch report.status {
        case "preset":
            if let id = report.presetId, let preset = ProviderPreset.byId(id) { return .preset(preset) }
            return .generic
        case "unsupported":
            switch report.unsupported {
            case "microsoft":
                return .unsupported(
                    name: "Microsoft 365",
                    reason: String(localized: "Outlook.com and Microsoft 365 accounts need Microsoft sign-in, which Castellum doesn’t support yet — it’s planned."))
            case "oauth_only":
                return .unsupported(
                    name: report.displayName ?? domain,
                    reason: String(localized: "This provider only allows sign-in through its own web login (OAuth), which Castellum doesn’t support yet."))
            case "no_tls_imap":
                return .unsupported(
                    name: report.displayName ?? domain,
                    reason: String(localized: "This provider only offers unencrypted IMAP, so Castellum won’t fill it in. You can still enter the servers yourself."))
            default:
                return .generic
            }
        case "found":
            guard let imap = report.imap, imap.port > 0, imap.port <= 65535 else { return .generic }
            // An unauthenticated DNS answer naming a host outside the domain
            // could redirect the user's credentials: say so, fill nothing.
            if report.hostOutsideDomain == true {
                return .unsupported(
                    name: domain,
                    reason: String(localized: "DNS for \(domain) suggests the mail server \(imap.host), which is not under \(domain), so Castellum did not fill it in. If you trust it, enter the servers yourself."))
            }
            let security: IMAPSecurity
            switch imap.tls {
            case "implicit": security = .tls
            case "starttls": security = .starttls
            default: return .generic   // never anything else: no plaintext from a lookup
            }
            return .discovered(DiscoveredSettings(
                domain: domain, source: report.source ?? "", displayName: report.displayName,
                imapHost: imap.host, imapPort: imap.port, imapSecurity: security,
                smtpHost: report.smtp?.host, smtpPort: report.smtp?.port, username: report.username))
        default:
            return .generic
        }
    }

    /// The username a result asks for given the typed address, or nil to leave
    /// the field alone (blank already means "the full address").
    static func username(for form: UsernameForm?, email: String) -> String? {
        guard form == .localPart else { return nil }
        let trimmed = email.trimmingCharacters(in: .whitespaces)
        guard let at = trimmed.lastIndex(of: "@") else { return nil }
        let local = String(trimmed[..<at])
        return local.isEmpty ? nil : local
    }
}

/// The Add-account sheet's lookup state machine (MM-14).
enum AutoconfigLookupPhase: Equatable {
    case idle
    case looking(String)
    case done(domain: String, detection: ProviderDetection, source: String?)
    case offline(String)

    /// The domain this phase is about, if any.
    var domain: String? {
        switch self {
        case .idle: return nil
        case .looking(let d), .offline(let d): return d
        case .done(let d, _, _): return d
        }
    }
}
