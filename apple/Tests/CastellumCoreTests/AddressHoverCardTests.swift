// Castellum - the address hover card's content and the View-menu remote-images switch
//. The popover itself needs a pointer: Marc's GUI check.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class AddressCardContentTests: XCTestCase {
    private func contact(
        email: String = "ada@x.test", derived: String? = "Ada L", display: String? = nil, notes: String = "",
        inbound: Int64 = 12, outbound: Int64 = 3, isSelf: Bool = false
    ) -> Contact {
        Contact(
            id: 1, email: email, derivedName: derived, displayName: display, notes: notes, isHidden: false,
            isSelf: isSelf, isUserCreated: false, firstSeenAt: 1_600_000_000, lastSeenAt: 1_700_000_000,
            inboundCount: inbound, outboundCount: outbound)
    }

    func testTitlePrefersTheUsersNameThenTheHeaderThenTheDerivedNameThenTheAddress() {
        XCTAssertEqual(AddressCardContent.make(name: "Header", address: "ada@x.test", contact: contact(display: "Mine"), isVip: false, myAddresses: []).title, "Mine")
        XCTAssertEqual(AddressCardContent.make(name: "Header", address: "ada@x.test", contact: contact(), isVip: false, myAddresses: []).title, "Header")
        XCTAssertEqual(AddressCardContent.make(name: nil, address: "ada@x.test", contact: contact(), isVip: false, myAddresses: []).title, "Ada L")
        XCTAssertEqual(AddressCardContent.make(name: "  ", address: "ada@x.test", contact: nil, isVip: false, myAddresses: []).title, "ada@x.test")
    }

    func testExchangeSummaryUsesTheStoredCounts() {
        let c = AddressCardContent.make(name: nil, address: "ada@x.test", contact: contact(inbound: 12, outbound: 3), isVip: false, myAddresses: [])
        XCTAssertEqual(c.received, 12)
        XCTAssertEqual(c.sent, 3)
        XCTAssertEqual(c.exchangeSummary, "12 received · 3 sent")
        XCTAssertEqual(c.firstSeen, Date(timeIntervalSince1970: 1_600_000_000))
        XCTAssertEqual(c.lastSeen, Date(timeIntervalSince1970: 1_700_000_000))
    }

    func testNoContactRowMeansNoCounts() {
        let c = AddressCardContent.make(name: "Zed", address: "zed@x.test", contact: nil, isVip: false, myAddresses: [])
        XCTAssertNil(c.exchangeSummary)
        XCTAssertNil(c.firstSeen)
        XCTAssertNil(c.notes)
        XCTAssertEqual(c.title, "Zed")
    }

    func testMyOwnAddressHidesTheCountsEvenWithoutAContactRow() {
        let mine = AddressCardContent.make(name: "Me", address: "Me@X.test", contact: nil, isVip: false, myAddresses: ["me@x.test"])
        XCTAssertTrue(mine.isSelf)
        XCTAssertNil(mine.exchangeSummary)
        let viaContact = AddressCardContent.make(name: nil, address: "a@x.test", contact: contact(isSelf: true), isVip: false, myAddresses: [])
        XCTAssertTrue(viaContact.isSelf)
        XCTAssertNil(viaContact.exchangeSummary)
    }

    func testNotesAndVipCarryThrough() {
        let c = AddressCardContent.make(name: nil, address: "ada@x.test", contact: contact(notes: "  Met at the conf.  "), isVip: true, myAddresses: [])
        XCTAssertEqual(c.notes, "Met at the conf.")
        XCTAssertTrue(c.isVip)
    }

    func testCardTimingOpensAfterARestAndClosesQuickly() {
        XCTAssertGreaterThan(AddressCardTiming.openDelay, AddressCardTiming.closeDelay)
        XCTAssertGreaterThanOrEqual(AddressCardTiming.openDelay, 0.4, "not while the pointer sweeps across the header")
    }
}

@MainActor
final class AddressCardModelTests: XCTestCase {
    func testTheCardReadsTheStoredContactAndVipState() async throws {
        let dir = TestScratch.root.appendingPathComponent("castellum-card-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        defer { try? model.accountStore.deleteAll(); try? FileManager.default.removeItem(at: dir) }
        _ = try model.core.createContact(email: "Ada@X.test", name: "Ada Lovelace")
        let known = await model.addressCardContent(address: "ada@x.test", name: nil)
        XCTAssertEqual(known.title, "Ada Lovelace")
        XCTAssertFalse(known.isVip)
        let unknown = await model.addressCardContent(address: "nobody@x.test", name: "Nobody")
        XCTAssertEqual(unknown.title, "Nobody")
        XCTAssertNil(unknown.exchangeSummary)
    }
}

final class RemoteImagesMenuTests: XCTestCase {
    func testTheCommandIsInTheViewMenuAndFlipsTheSamePreferenceAsSettings() {
        let spec = CommandRegistry.spec(for: .toggleRemoteImages)
        XCTAssertEqual(spec.menu?.name, .view)
        XCTAssertEqual(spec.notification, .castellumToggleRemoteImages)
        XCTAssertNil(spec.menuChord, "no chord: a privacy switch should not be one slip away")
        XCTAssertTrue(CommandRegistry.conflicts(in: CommandRegistry.all).isEmpty)
    }

    func testMenuTicksFollowThePreferences() {
        XCTAssertEqual(
            MenuChecks.checked(layout: .widescreen, headerMode: .extended, loadsRemoteImages: false),
            [.layoutWidescreen, .headerExtended])
        XCTAssertEqual(
            MenuChecks.checked(layout: .classic, headerMode: .all, loadsRemoteImages: true),
            [.layoutClassic, .headerAll, .toggleRemoteImages])
        XCTAssertEqual(
            MenuChecks.checked(layout: .widescreen, headerMode: .compact, loadsRemoteImages: false),
            [.layoutWidescreen, .headerCompact])
    }
}
