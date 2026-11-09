// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
import XCTest
@testable import Castellum

/// GUI-4: several named signatures per account with one
/// default, plus migration from the pre-GUI-4 one-text-per-account storage.
final class SignatureLibraryTests: XCTestCase {
    func testFirstSignatureBecomesTheAccountDefault() {
        var lib = SignatureLibrary()
        let a = lib.add(name: "Work", accountId: "bob", text: "Bob W")
        let b = lib.add(name: "Casual", accountId: "bob", text: "Bob C")
        XCTAssertTrue(lib.isDefault(a))
        XCTAssertFalse(lib.isDefault(b))
        XCTAssertEqual(lib.defaultText(for: "bob"), "Bob W")
        XCTAssertEqual(lib.items(for: "bob").count, 2)
    }

    func testTwoSimilarAccountsKeepSeparateSignatures() {
        var lib = SignatureLibrary()
        lib.add(name: "S", accountId: "bob", text: "one")
        lib.add(name: "S", accountId: "bob2", text: "two")
        XCTAssertEqual(lib.defaultText(for: "bob"), "one")
        XCTAssertEqual(lib.defaultText(for: "bob2"), "two")
        XCTAssertEqual(lib.defaultText(for: "nobody"), "")
    }

    func testSetDefaultSwitchesWhichTextComposeGets() {
        var lib = SignatureLibrary()
        lib.add(name: "A", accountId: "x", text: "a")
        let b = lib.add(name: "B", accountId: "x", text: "b")
        lib.setDefault(id: b)
        XCTAssertEqual(lib.defaultText(for: "x"), "b")
    }

    func testDeletingTheDefaultPromotesTheNextAndDeletingTheLastClears() {
        var lib = SignatureLibrary()
        let a = lib.add(name: "A", accountId: "x", text: "a")
        let b = lib.add(name: "B", accountId: "x", text: "b")
        lib.delete(id: a)
        XCTAssertTrue(lib.isDefault(b))
        lib.delete(id: b)
        XCTAssertEqual(lib.defaultText(for: "x"), "")
        XCTAssertNil(lib.defaultItem(for: "x"))
    }

    func testMoveRepairsBothAccounts() {
        var lib = SignatureLibrary()
        let a = lib.add(name: "A", accountId: "x", text: "a")
        let b = lib.add(name: "B", accountId: "x", text: "b")
        lib.move(id: a, toAccount: "y")
        XCTAssertTrue(lib.isDefault(b), "x falls back to its remaining signature")
        XCTAssertTrue(lib.isDefault(a), "y had none, so the moved one becomes its default")
    }

    func testPruneDropsSignaturesOfRemovedAccounts() {
        var lib = SignatureLibrary()
        lib.add(name: "A", accountId: "gone", text: "a")
        lib.add(name: "B", accountId: "kept", text: "b")
        lib.prune(keepingAccounts: ["kept"])
        XCTAssertEqual(lib.items.map(\.accountId), ["kept"])
        XCTAssertEqual(lib.defaultText(for: "gone"), "")
    }

    func testLegacyMigrationKeepsTextAndSkipsBlank() {
        let lib = SignatureLibrary(migratingLegacy: ["a": "Ada", "b": "  \n", "c": "Cy"])
        XCTAssertEqual(lib.defaultText(for: "a"), "Ada")
        XCTAssertEqual(lib.defaultText(for: "c"), "Cy")
        XCTAssertNil(lib.defaultItem(for: "b"))
    }

    func testCodableRoundTripKeepsDefaults() throws {
        var lib = SignatureLibrary()
        lib.add(name: "A", accountId: "x", text: "a")
        let b = lib.add(name: "B", accountId: "x", text: "b")
        lib.setDefault(id: b)
        let back = try JSONDecoder().decode(SignatureLibrary.self, from: JSONEncoder().encode(lib))
        XCTAssertEqual(back, lib)
        XCTAssertEqual(back.defaultText(for: "x"), "b")
    }

    @MainActor
    func testPreferencesMigratesLegacyDictionaryOnce() {
        let suite = "SignatureLibraryTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set(["acctA": "Ada Lovelace"], forKey: "accountSignatures")

        let first = Preferences(defaults: defaults)
        XCTAssertEqual(first.signature(for: "acctA"), "Ada Lovelace")
        first.setSignature("Ada L.", for: "acctA")

        // A second load reads the library, not the stale legacy dictionary.
        let second = Preferences(defaults: defaults)
        XCTAssertEqual(second.signature(for: "acctA"), "Ada L.")
        XCTAssertEqual(second.signatureLibrary.items(for: "acctA").count, 1)
    }

    @MainActor
    func testSetSignatureBlankRemovesTheDefault() {
        let suite = "SignatureLibraryTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let prefs = Preferences(defaults: defaults)
        prefs.setSignature("hello", for: "x")
        prefs.setSignature("  ", for: "x")
        XCTAssertEqual(prefs.signature(for: "x"), "")
        XCTAssertTrue(prefs.signatureLibrary.items.isEmpty)
    }

    /// Review fix: an undecodable library blob must NOT fall back to the
    /// stale legacy dictionary (it would resurrect old text and the next save
    /// would erase later edits); it is backed up and the library starts empty.
    @MainActor
    func testUndecodableLibraryBlobDoesNotResurrectLegacyData() {
        let suite = "SignatureLibraryTests-\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        defaults.set(["acctA": "stale legacy"], forKey: "accountSignatures")
        let garbage = Data("{not json".utf8)
        defaults.set(garbage, forKey: "signatureLibrary")

        let prefs = Preferences(defaults: defaults)
        XCTAssertEqual(prefs.signature(for: "acctA"), "")
        XCTAssertEqual(defaults.data(forKey: "signatureLibrary.undecodable-backup"), garbage)
    }

    // MARK: HTML version and naming (#3142)

    func testHTMLRoundTripsAndABlankOneIsStoredAsNone() throws {
        var lib = SignatureLibrary()
        let id = lib.add(name: "Rich", accountId: "bob", text: "Bob")
        lib.setHTML(id: id, "<p><b>Bob</b></p>\n\n")
        let decoded = try JSONDecoder().decode(SignatureLibrary.self, from: JSONEncoder().encode(lib))
        XCTAssertEqual(decoded.item(id: id)?.html, "<p><b>Bob</b></p>\n\n", "stored verbatim")
        lib.setHTML(id: id, "  \n")
        XCTAssertNil(lib.item(id: id)?.html)
    }

    func testABlobFromBeforeHTMLStillDecodes() throws {
        let old = #"{"items":[{"id":"s1","name":"Work","accountId":"bob","text":"Bob W"}],"defaults":{"bob":"s1"}}"#
        let lib = try JSONDecoder().decode(SignatureLibrary.self, from: Data(old.utf8))
        XCTAssertNil(lib.item(id: "s1")?.html)
        XCTAssertEqual(lib.defaultText(for: "bob"), "Bob W")
    }

    func testDefaultTextFallsBackToTheHTMLVersionsText() {
        var lib = SignatureLibrary()
        let id = lib.add(name: "Rich", accountId: "bob", text: " ")
        lib.setHTML(id: id, "<b>Bob</b><br>Builder")
        XCTAssertEqual(lib.defaultText(for: "bob"), "Bob\nBuilder")
        lib.setText(id: id, "Plain Bob")
        XCTAssertEqual(lib.defaultText(for: "bob"), "Plain Bob", "a written plain version wins")
    }

    func testSuggestedNamesSkipTakenOnes() {
        var lib = SignatureLibrary()
        XCTAssertEqual(lib.suggestedName(for: "bob"), "Signature")
        lib.add(name: "Signature", accountId: "bob")
        lib.add(name: "Signature 2", accountId: "bob")
        XCTAssertEqual(lib.suggestedName(for: "bob"), "Signature 3")
        XCTAssertEqual(lib.suggestedName(for: "alice", base: "Firma"), "Firma")
    }

    @MainActor
    func testWindowRequestFiresEvenForTheSameAccountTwice() {
        let request = SignatureWindowRequest()
        let first = request.token
        request.request(accountId: "a")
        let second = request.token
        request.request(accountId: "a")
        XCTAssertNotEqual(first, second)
        XCTAssertNotEqual(second, request.token)
        XCTAssertEqual(request.accountId, "a")
    }

    /// #3439: removing one account drops only its signatures and default.
    func testRemoveAllForAccountLeavesOthersAlone() {
        var lib = SignatureLibrary()
        let a = lib.add(name: "A1", accountId: "a", text: "x")
        _ = lib.add(name: "A2", accountId: "a", text: "y")
        let b = lib.add(name: "B1", accountId: "b", text: "z")
        lib.removeAll(forAccount: "a")
        XCTAssertTrue(lib.items(for: "a").isEmpty)
        XCTAssertNil(lib.item(id: a))
        XCTAssertNil(lib.defaultItem(for: "a"))
        XCTAssertEqual(lib.items(for: "b").map(\.id), [b])
        XCTAssertEqual(lib.defaultItem(for: "b")?.id, b)
    }
}
