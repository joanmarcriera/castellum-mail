// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// MM-14 slice 1: the pure rules around the Add-account sheet's server-settings
/// lookup (when to look, how a core report maps onto the sheet's detection
/// cases, what may fill what) plus the AppModel lookup seam. Nothing here
/// touches DNS or the network: reports are hand-built / decoded fixtures and
/// the lookup itself is `AppModel.autoconfigLookupOverride`.
final class AutoconfigPolicyTests: XCTestCase {
    // MARK: - lookupDomain

    func testLookupDomainIsNilUnlessTheAddressIsCompleteAndUnknown() {
        for email in ["", "me", "me@", "@isp.net", "me@foo", "me@foo.", "me@foo.c", "me@foo.123",
                      "me@a b.net", "me@@isp.net", "me@isp..net"] {
            XCTAssertNil(AutoconfigPolicy.lookupDomain(email: email), email)
        }
    }

    func testKnownProvidersAndMicrosoftNeverTriggerALookup() {
        for email in ["me@gmail.com", "me@googlemail.com", "me@icloud.com", "me@fastmail.com",
                      "me@yahoo.co.uk", "me@outlook.com", "me@hotmail.co.uk", "  Me@GMail.com "] {
            XCTAssertNil(AutoconfigPolicy.lookupDomain(email: email), email)
        }
    }

    func testLookupDomainIsOnlyTheLowerCasedDomain() {
        XCTAssertEqual(AutoconfigPolicy.lookupDomain(email: " Ada.Lovelace@Small-ISP.Net "), "small-isp.net")
        let domain = AutoconfigPolicy.lookupDomain(email: "secret.local.part@isp.net")
        XCTAssertEqual(domain, "isp.net")
        XCTAssertFalse(domain?.contains("secret") ?? true)
        XCTAssertFalse(domain?.contains("@") ?? true)
    }

    func testInternationalisedDomainsAreSentAsPunycodeOrNotAtAll() throws {
        let domain = try XCTUnwrap(AutoconfigPolicy.lookupDomain(email: "me@münchen.de"))
        XCTAssertTrue(domain.allSatisfy(\.isASCII), domain)
        XCTAssertTrue(domain.hasPrefix("xn--"), domain)
    }

    func testReservedNamesNeverTriggerALookup() {
        for email in ["me@x.test", "me@x.example", "me@x.invalid", "me@x.local", "me@x.localhost", "me@localhost"] {
            XCTAssertNil(AutoconfigPolicy.lookupDomain(email: email), email)
        }
    }

    func testALateResultOnlyFillsFieldsTheUserHasNotEdited() {
        let found = PresetFields(DiscoveredSettings(
            domain: "isp.net", source: "srv", displayName: nil, imapHost: "imap.isp.net", imapPort: 143,
            imapSecurity: .starttls, smtpHost: "smtp.isp.net", smtpPort: 465, username: nil))
        var now = PresetFields(.gmail)
        // Pristine form: everything may be filled.
        now.host = ""; now.port = "993"; now.imapSecurity = .tls; now.smtpHost = ""; now.smtpPort = "587"; now.smtpUseTLS = true
        XCTAssertEqual(PresetFields.mask(current: now, lastApplied: nil, smtpHostAuto: true), .all)
        // The user typed an SMTP host, port 2525 and unticked TLS: all kept.
        now.smtpHost = "mail.mine.net"; now.smtpPort = "2525"; now.smtpUseTLS = false
        let m = PresetFields.mask(current: now, lastApplied: nil, smtpHostAuto: false)
        XCTAssertFalse(m.smtpHost); XCTAssertFalse(m.smtpPort); XCTAssertFalse(m.smtpTLS)
        XCTAssertTrue(m.port && m.security)
        // A typed IMAP port / security mode are kept too.
        now.port = "1143"; now.imapSecurity = .none
        let m2 = PresetFields.mask(current: now, lastApplied: nil, smtpHostAuto: true)
        XCTAssertFalse(m2.port); XCTAssertFalse(m2.security)
        // Values the previous auto-fill wrote are replaceable again.
        let m3 = PresetFields.mask(current: found, lastApplied: found, smtpHostAuto: true)
        XCTAssertEqual(m3, .all)
    }

    func testStaleResultsAreDiscarded() {
        XCTAssertTrue(AutoconfigPolicy.accepts(resultFor: "isp.net", currentEmail: "me@isp.net"))
        XCTAssertTrue(AutoconfigPolicy.accepts(resultFor: "isp.net", currentEmail: "someone.else@isp.net"))
        XCTAssertFalse(AutoconfigPolicy.accepts(resultFor: "isp.net", currentEmail: "me@other.net"))
        XCTAssertFalse(AutoconfigPolicy.accepts(resultFor: "isp.net", currentEmail: "me@gmail.com"))
        XCTAssertFalse(AutoconfigPolicy.accepts(resultFor: "isp.net", currentEmail: ""))
    }

    // MARK: - report -> detection

    private func decode(_ json: String) throws -> AutoconfigReport {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        return try decoder.decode(AutoconfigReport.self, from: Data(json.utf8))
    }

    func testFoundMapsToDiscoveredWithStartTLSKept() throws {
        let report = try decode("""
        {"status":"found","source":"ispdb","display_name":"Small ISP",
         "imap":{"host":"imap.small-isp.net","port":143,"tls":"starttls"},
         "smtp":{"host":"smtp.small-isp.net","port":465,"tls":"implicit"},
         "username":"local_part","tried":[{"source":"srv","outcome":"miss"}]}
        """)
        guard case .discovered(let d) = AutoconfigPolicy.detection(from: report, domain: "small-isp.net") else {
            return XCTFail("expected discovered")
        }
        XCTAssertEqual(d.imapHost, "imap.small-isp.net")
        XCTAssertEqual(d.imapPort, 143)
        XCTAssertEqual(d.imapSecurity, .starttls)
        XCTAssertEqual(d.smtpHost, "smtp.small-isp.net")
        XCTAssertEqual(d.smtpPort, 465)
        XCTAssertEqual(d.username, .localPart)
        XCTAssertEqual(d.source, "ispdb")
        let fields = PresetFields(d)
        XCTAssertEqual(fields.host, "imap.small-isp.net")
        XCTAssertEqual(fields.port, "143")
        XCTAssertEqual(fields.imapSecurity, .starttls)
        XCTAssertTrue(fields.useTLS, "STARTTLS is still encrypted; detection never yields cleartext")
        XCTAssertEqual(fields.smtpPort, "465")
        XCTAssertTrue(fields.smtpUseTLS)
    }

    func testFoundWithoutSMTPLetsSMTPFollowIMAP() throws {
        let report = AutoconfigReport(
            status: "found", source: "srv",
            imap: AutoconfigEndpoint(host: "imap.isp.net", port: 993, tls: "implicit"))
        guard case .discovered(let d) = AutoconfigPolicy.detection(from: report, domain: "isp.net") else {
            return XCTFail("expected discovered")
        }
        let fields = PresetFields(d)
        XCTAssertEqual(fields.imapSecurity, .tls)
        XCTAssertEqual(fields.smtpHost, AccountConfig.defaultSMTPHost(imapHost: "imap.isp.net"))
        XCTAssertEqual(fields.smtpPort, "587")
    }

    func testAnUnknownOrPlaintextEndpointIsNeverFilledIn() {
        for tls in ["none", "plain", "", "ssl3"] {
            let report = AutoconfigReport(
                status: "found", source: "srv",
                imap: AutoconfigEndpoint(host: "imap.isp.net", port: 143, tls: tls))
            XCTAssertEqual(AutoconfigPolicy.detection(from: report, domain: "isp.net"), .generic, tls)
        }
        let badPort = AutoconfigReport(
            status: "found", imap: AutoconfigEndpoint(host: "imap.isp.net", port: 0, tls: "implicit"))
        XCTAssertEqual(AutoconfigPolicy.detection(from: badPort, domain: "isp.net"), .generic)
        XCTAssertEqual(AutoconfigPolicy.detection(from: AutoconfigReport(status: "found"), domain: "isp.net"), .generic)
    }

    func testAnOffDomainDNSHostIsNeverPrefilled() {
        let report = AutoconfigReport(
            status: "found", source: "srv",
            imap: AutoconfigEndpoint(host: "imap.hoster.net", port: 993, tls: "implicit"),
            hostOutsideDomain: true)
        guard case .unsupported(_, let reason) = AutoconfigPolicy.detection(from: report, domain: "isp.net") else {
            return XCTFail("a flagged host must not become .discovered")
        }
        XCTAssertTrue(reason.contains("imap.hoster.net"))
    }

    func testPresetReportsResolveThroughByIdAndUnknownIdsAreGeneric() {
        for id in ["gmail", "fastmail", "icloud", "yahoo"] {
            let detection = AutoconfigPolicy.detection(
                from: AutoconfigReport(status: "preset", source: "mx", presetId: id), domain: "corp.net")
            XCTAssertEqual(detection, .preset(ProviderPreset.byId(id)!), id)
        }
        XCTAssertEqual(
            AutoconfigPolicy.detection(from: AutoconfigReport(status: "preset", presetId: "nope"), domain: "corp.net"),
            .generic)
    }

    /// Every preset id the Rust MX table can emit (`autoconfig::PRESET_IDS`)
    /// must resolve; keep this list in step with that constant.
    func testEveryRustPresetIdExists() {
        for id in ["gmail", "fastmail", "icloud", "yahoo"] {
            XCTAssertNotNil(ProviderPreset.byId(id), id)
        }
        XCTAssertNil(ProviderPreset.byId("outlook"))
        XCTAssertNil(ProviderPreset.byId(""))
    }

    func testUnsupportedReportsFillNothing() {
        for why in ["microsoft", "oauth_only", "no_tls_imap"] {
            let detection = AutoconfigPolicy.detection(
                from: AutoconfigReport(status: "unsupported", source: "mx", unsupported: why), domain: "corp.net")
            guard case .unsupported(_, let reason) = detection else { return XCTFail(why) }
            XCTAssertFalse(reason.isEmpty)
        }
        XCTAssertEqual(
            AutoconfigPolicy.detection(from: AutoconfigReport(status: "unsupported", unsupported: "?"), domain: "x.net"),
            .generic)
    }

    func testNotFoundOfflineAndGarbageStatusesShowNothing() {
        for status in ["not_found", "offline", "", "bogus"] {
            XCTAssertEqual(AutoconfigPolicy.detection(from: AutoconfigReport(status: status), domain: "x.net"), .generic, status)
        }
    }

    func testUsernameOnlyFollowsAnExplicitLocalPartRequest() {
        XCTAssertEqual(AutoconfigPolicy.username(for: .localPart, email: "me@isp.net"), "me")
        XCTAssertNil(AutoconfigPolicy.username(for: .email, email: "me@isp.net"))
        XCTAssertNil(AutoconfigPolicy.username(for: nil, email: "me@isp.net"))
        XCTAssertNil(AutoconfigPolicy.username(for: .localPart, email: "@isp.net"))
    }

    // MARK: - never overwrite a typed host

    func testAFilledHostIsReplaceableOnlyByTheLookupItselfNeverATypedOne() {
        // The sheet applies a discovered result only while this holds.
        XCTAssertTrue(PresetFields.mayAutoApply(currentHost: "", lastAppliedHost: nil))
        XCTAssertTrue(PresetFields.mayAutoApply(currentHost: "imap.small-isp.net", lastAppliedHost: "imap.small-isp.net"))
        XCTAssertFalse(PresetFields.mayAutoApply(currentHost: "mail.mine.net", lastAppliedHost: "imap.small-isp.net"))
        XCTAssertFalse(PresetFields.mayAutoApply(currentHost: "mail.mine.net", lastAppliedHost: nil))
    }

    func testSourceLabelsAreHumanNotRaw() {
        for source in ["srv", "domain_autoconfig", "well_known", "ispdb", nil] {
            let label = AutoconfigPolicy.sourceLabel(source)
            XCTAssertFalse(label.contains("_"), label)
            XCTAssertFalse(label.isEmpty)
        }
    }

    // MARK: - AppModel seam

    @MainActor
    func testLookupSeamPassesOnlyTheDomainAndHonoursTheSettingsToggle() async throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-autoconfig-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }
        let prefs = Preferences.shared
        let original = prefs.autoconfigLookupEnabled
        defer { prefs.autoconfigLookupEnabled = original }

        var asked: [String] = []
        model.autoconfigLookupOverride = { domain in
            asked.append(domain)
            return AutoconfigReport(status: "not_found")
        }

        prefs.autoconfigLookupEnabled = true
        let report = await model.autoconfigLookup(domain: "isp.net")
        XCTAssertEqual(report?.status, "not_found")
        XCTAssertEqual(asked, ["isp.net"])

        prefs.autoconfigLookupEnabled = false
        let none = await model.autoconfigLookup(domain: "isp.net")
        XCTAssertNil(none, "toggle off: no lookup result")
        XCTAssertEqual(asked, ["isp.net"], "toggle off: the lookup must not even be attempted")
    }

    /// Under XCTest an un-faked lookup must never reach DNS or the network.
    @MainActor
    func testUnfakedLookupUnderTestAnswersNotFoundWithoutNetwork() async {
        let dir = TestScratch.root.appendingPathComponent("castellum-autoconfig-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll() }
        let prefs = Preferences.shared
        let original = prefs.autoconfigLookupEnabled
        defer { prefs.autoconfigLookupEnabled = original }
        prefs.autoconfigLookupEnabled = true
        XCTAssertNil(model.autoconfigLookupOverride)
        let report = await model.autoconfigLookup(domain: "isp.net")
        XCTAssertEqual(report?.status, "not_found")
    }

    @MainActor
    func testLookupDefaultsToOn() {
        let defaults = UserDefaults(suiteName: "autoconfig-default-\(UUID().uuidString)")!
        XCTAssertTrue(Preferences(defaults: defaults).autoconfigLookupEnabled)
    }

    // MARK: - the real FFI round trip, with no network

    func testCoreRefusesALocalPartAndAnswersNotFoundWithEverySourceDisabled() throws {
        // Every source disabled: the core performs no I/O at all.
        let report = try CastellumCore.autoconfigLookup(domain: "isp.net", sources: [])
        XCTAssertEqual(report.status, "not_found")
        XCTAssertThrowsError(try CastellumCore.autoconfigLookup(domain: "me@isp.net", sources: []))
        XCTAssertThrowsError(try CastellumCore.autoconfigLookup(domain: "", sources: []))
    }
}
