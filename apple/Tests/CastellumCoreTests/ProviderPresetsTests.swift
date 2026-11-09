// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// S62 provider presets: domain detection, the Microsoft "not yet" state,
/// field mapping, and the never-overwrite-a-typed-host rule.
final class ProviderPresetsTests: XCTestCase {
    func testDetectsEachSupportedProviderByDomain() {
        XCTAssertEqual(ProviderDetection.detect(email: "a@gmail.com"), .preset(.gmail))
        XCTAssertEqual(ProviderDetection.detect(email: "a@googlemail.com"), .preset(.gmail))
        XCTAssertEqual(ProviderDetection.detect(email: "a@icloud.com"), .preset(.icloud))
        XCTAssertEqual(ProviderDetection.detect(email: "a@me.com"), .preset(.icloud))
        XCTAssertEqual(ProviderDetection.detect(email: "a@fastmail.com"), .preset(.fastmail))
        XCTAssertEqual(ProviderDetection.detect(email: "a@yahoo.co.uk"), .preset(.yahoo))
    }

    func testDetectionIsCaseAndWhitespaceInsensitive() {
        XCTAssertEqual(ProviderDetection.detect(email: "  Marc@GMail.COM "), .preset(.gmail))
    }

    func testUnknownOrIncompleteAddressesAreGenericIMAP() {
        for email in ["", "marc", "marc@", "user@example.net", "a@gmail.com.evil.test", "a@notgmail.com"] {
            XCTAssertEqual(ProviderDetection.detect(email: email), .generic, email)
        }
    }

    func testMicrosoftIsRecognisedButNotFaked() {
        for email in ["a@outlook.com", "a@hotmail.co.uk", "a@live.com"] {
            guard case .unsupported(_, let reason) = ProviderDetection.detect(email: email) else {
                return XCTFail("\(email) must be flagged unsupported, not given IMAP settings")
            }
            XCTAssertTrue(reason.contains("doesn’t support yet"), reason)
        }
        XCTAssertFalse(ProviderPreset.all.contains { $0.imapHost.contains("office365") || $0.imapHost.contains("outlook") })
    }

    func testPresetFieldsMatchPublishedSettings() {
        let gmail = PresetFields(.gmail)
        XCTAssertEqual(gmail.host, "imap.gmail.com")
        XCTAssertEqual(gmail.port, "993")
        XCTAssertEqual(gmail.smtpHost, "smtp.gmail.com")
        XCTAssertEqual(gmail.smtpPort, "587")
        XCTAssertTrue(gmail.useTLS && gmail.smtpUseTLS)
        XCTAssertEqual(PresetFields(.fastmail).smtpPort, "465")
        XCTAssertEqual(PresetFields(.icloud).host, "imap.mail.me.com")
    }

    func testEveryPresetSMTPHostFollowsTheIMAPDefaultDerivation() {
        // The sheet keeps SMTP host "auto" (tracking IMAP edits) when the
        // preset's SMTP host is what defaultSMTPHost would derive anyway.
        for preset in ProviderPreset.all {
            XCTAssertEqual(AccountConfig.defaultSMTPHost(imapHost: preset.imapHost), preset.smtpHost, preset.name)
        }
    }

    func testAutoApplyNeverOverwritesATypedHost() {
        XCTAssertTrue(PresetFields.mayAutoApply(currentHost: "", lastAppliedHost: nil))
        XCTAssertTrue(PresetFields.mayAutoApply(currentHost: "imap.gmail.com", lastAppliedHost: "imap.gmail.com"))
        XCTAssertFalse(PresetFields.mayAutoApply(currentHost: "mail.example.net", lastAppliedHost: nil))
        XCTAssertFalse(PresetFields.mayAutoApply(currentHost: "mail.example.net", lastAppliedHost: "imap.gmail.com"))
    }

    func testPresetIdsAreUnique() {
        XCTAssertEqual(Set(ProviderPreset.all.map(\.id)).count, ProviderPreset.all.count)
    }
}
