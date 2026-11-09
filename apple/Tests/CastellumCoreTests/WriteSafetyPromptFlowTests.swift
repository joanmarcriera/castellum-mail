// Castellum - WS1: a refused write can be fixed on the spot.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
//
// Found in the S37 GUI check on 2026-10-05: Send from the compose window on a
// read-only account showed "Send was not applied: … is in evaluation mode" and
// offered no working way to enable writes. The prompt was raised in the model
// but never presented (see `WriteSafetyPresenter` for the cause).
//
// These tests cover the model half of the fix against the REAL core, in a temp
// profile, with accounts saved offline (`saveAccountWithoutSync` creates the
// durable row without ever opening a socket; the host is loopback port 1, so
// no real mail server could be reached even by mistake):
//   * the refusal dialog always offers Enable Writes and Keep Read-only;
//   * confirming flips `accounts.write_enabled`, and the value READ BACK from
//     the core is what drives the badge (`AppModel.writeState(for:)`);
//   * a refused action is retried once writes are on, or the user is told to
//     repeat it;
//   * "all accounts" is a separate, explicitly confirmed act over a snapshot;
//   * an account added afterwards still starts read-only.
// Nothing is presented: no `WriteSafetyPresenter` exists under XCTest.

import XCTest
@testable import Castellum

@MainActor
final class WriteSafetyPromptFlowTests: XCTestCase {
    private var models: [AppModel] = []

    private func makeModel() -> AppModel {
        let dir = TestScratch.root
            .appendingPathComponent("castellum-ws1-\(UUID().uuidString)")
        let model = AppModel(profileDirectory: dir)
        models.append(model)
        return model
    }

    override func tearDown() async throws {
        for model in models {
            XCTAssertNotEqual(model.accountStore.serviceName, AccountStore.defaultService)
            try model.accountStore.deleteAll()
        }
        models = []
        try await super.tearDown()
    }

    private func waitUntil(
        _ condition: @escaping () -> Bool, timeout: TimeInterval = 15,
        _ message: @autoclosure @escaping () -> String
    ) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() && Date() < deadline {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        if !condition() { XCTFail(message()) }
    }

    /// Persist an account row offline and wait for the model to list it.
    @discardableResult
    private func addOfflineAccount(_ model: AppModel, _ name: String) async throws -> Account {
        let id = "\(name)-\(UUID().uuidString)"
        let email = "\(name)@ws1.invalid"
        let cfg = """
        {"account_id":"\(id)","email":"\(email)","host":"127.0.0.1","port":1,\
        "username":"\(email)","password":"x","use_tls":false}
        """
        try model.core.saveAccountWithoutSync(configJSON: cfg)
        model.reloadAccounts()
        try await waitUntil({ model.accounts.contains { $0.id == id } }, "\(name) never listed")
        return try XCTUnwrap(model.accounts.first { $0.id == id })
    }

    private func settle(_ model: AppModel) async throws {
        try await waitUntil({ !model.isChangingWritePolicy }, "write-policy change never finished")
    }

    // MARK: - the refusal dialog

    func testRefusalDialogAlwaysOffersEnableWritesAndKeepReadOnly() {
        for willRetry in [false, true] {
            let spec = AccountWritePolicy.alertSpec(
                for: .blocked(accountId: "a1", email: "marc@example.com", verb: "Send"),
                willRetry: willRetry)
            XCTAssertEqual(spec.title, "This Account Is Read-only")
            XCTAssertEqual(spec.buttons.map(\.title), ["Enable Writes", "Keep Read-only"])
            XCTAssertEqual(spec.buttons.map(\.action), [.enable(["a1"]), .dismiss])
            XCTAssertTrue(spec.message.contains("Send was not applied"), spec.message)
            XCTAssertTrue(spec.message.contains("marc@example.com"), spec.message)
        }
    }

    func testRefusalDialogSaysWhatHappensToTheRefusedAction() {
        let prompt = WriteSafetyPrompt.blocked(accountId: "a1", email: "e@x.test", verb: "Send")
        let retried = AccountWritePolicy.alertSpec(for: prompt, willRetry: true).message
        let manual = AccountWritePolicy.alertSpec(for: prompt, willRetry: false).message
        XCTAssertTrue(retried.contains("retries"), retried)
        XCTAssertFalse(manual.contains("retries"), manual)
    }

    func testEveryPromptHasADismissingButtonAndNoPromptEnablesByDefaultKey() {
        let prompts: [WriteSafetyPrompt] = [
            .blocked(accountId: "a1", email: "e", verb: "Archive"),
            .confirmEnable(accountId: "a1", email: "e"),
            .confirmDisable(accountId: "a1", email: "e"),
            .confirmEnableAll(accounts: [WriteSafetyAccountRef(id: "a1", email: "e")]),
            .retryNotice(accountId: "a1", email: "e", verb: "Archive"),
        ]
        for p in prompts {
            let spec = AccountWritePolicy.alertSpec(for: p, willRetry: false)
            XCTAssertTrue(spec.buttons.contains { $0.action == .dismiss }, "\(p)")
            // Return must never leave evaluation mode: an enabling button is
            // never the default one.
            for b in spec.buttons where b.isDefault {
                if case .enable = b.action { XCTFail("\(p): Enable is the default button") }
            }
        }
        XCTAssertEqual(Set(prompts.map(\.id)).count, prompts.count)
    }

    // MARK: - confirming flips write_enabled; the read-back drives the badge

    func testConfirmingTheRefusalFlipsWriteEnabledAndTheReadBackDrivesTheBadge() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "alice")
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: account.id))
        XCTAssertEqual(model.writeState(for: account.id), .readOnly)

        model.reportWriteFailure(
            verb: "Send", accountId: account.id, isEvaluationMode: true,
            reason: "evaluation mode: refused")
        XCTAssertEqual(
            model.writeSafetyPrompt,
            .blocked(accountId: account.id, email: account.email, verb: "Send"))
        // Raising the prompt changes nothing by itself.
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: account.id))

        var retries = 0
        model.registerWriteSafetyRetry(accountId: account.id) { retries += 1 }
        model.resolveWriteSafetyPrompt(.enable([account.id]))
        try await settle(model)

        XCTAssertTrue(
            try model.core.accountWriteEnabled(accountId: account.id),
            "accounts.write_enabled must be 1 in the core after the confirmation")
        XCTAssertEqual(
            model.writeState(for: account.id), .writesEnabled,
            "the sidebar/Settings badge reads AppModel.writeState, fed by the core read-back")
        XCTAssertEqual(retries, 1, "the refused Send is retried exactly once")
        XCTAssertNil(model.writeSafetyPrompt, "a retried action needs no further dialog")
    }

    func testTheBadgeFollowsTheCoreNotTheRequest() async throws {
        // An id the core does not know: the write fails, the read-back says
        // "not enabled", and the badge must keep saying read-only.
        let model = makeModel()
        model.resolveWriteSafetyPrompt(.enable(["no-such-account"]))
        try await settle(model)
        XCTAssertEqual(model.writeState(for: "no-such-account"), .readOnly)
    }

    func testKeepReadOnlyChangesNothingAndDropsTheRetry() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "alice")
        model.reportWriteFailure(
            verb: "Send", accountId: account.id, isEvaluationMode: true, reason: "evaluation mode: x")
        var retries = 0
        model.registerWriteSafetyRetry(accountId: account.id) { retries += 1 }

        model.resolveWriteSafetyPrompt(.dismiss)
        try await settle(model)

        XCTAssertNil(model.writeSafetyPrompt)
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: account.id))
        XCTAssertEqual(model.writeState(for: account.id), .readOnly)
        XCTAssertEqual(retries, 0)
    }

    func testWithoutARetryTheUserIsToldToRepeatTheAction() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "alice")
        model.reportWriteFailure(
            verb: "Archive", accountId: account.id, isEvaluationMode: true, reason: "evaluation mode: x")

        model.resolveWriteSafetyPrompt(.enable([account.id]))
        try await settle(model)

        XCTAssertEqual(model.writeState(for: account.id), .writesEnabled)
        XCTAssertEqual(
            model.writeSafetyPrompt,
            .retryNotice(accountId: account.id, email: account.email, verb: "Archive"))
        let spec = AccountWritePolicy.alertSpec(for: try XCTUnwrap(model.writeSafetyPrompt), willRetry: false)
        XCTAssertTrue(spec.message.contains("Archive"), spec.message)
        XCTAssertEqual(spec.buttons.map(\.action), [.dismiss])
    }

    func testARetryRegisteredForAnotherAccountIsIgnored() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "alice")
        model.reportWriteFailure(
            verb: "Send", accountId: account.id, isEvaluationMode: true, reason: "evaluation mode: x")
        var retries = 0
        model.registerWriteSafetyRetry(accountId: "someone-else") { retries += 1 }
        model.resolveWriteSafetyPrompt(.enable([account.id]))
        try await settle(model)
        XCTAssertEqual(retries, 0)
    }

    // MARK: - Settings: per-account control

    func testThePerAccountControlAsksFirstAndWorksBothWays() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "alice")

        model.requestWritePolicyChange(for: account)
        XCTAssertEqual(
            model.writeSafetyPrompt, .confirmEnable(accountId: account.id, email: account.email))
        XCTAssertEqual(model.writeState(for: account.id), .readOnly, "asking is not enabling")
        model.resolveWriteSafetyPrompt(.enable([account.id]))
        try await settle(model)
        XCTAssertTrue(try model.core.accountWriteEnabled(accountId: account.id))
        XCTAssertEqual(model.writeState(for: account.id), .writesEnabled)
        XCTAssertNil(model.writeSafetyPrompt)

        model.requestWritePolicyChange(for: account)
        XCTAssertEqual(
            model.writeSafetyPrompt, .confirmDisable(accountId: account.id, email: account.email))
        model.resolveWriteSafetyPrompt(.disable(account.id))
        try await settle(model)
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: account.id))
        XCTAssertEqual(model.writeState(for: account.id), .readOnly)
    }

    // MARK: - Settings: all accounts, explicitly confirmed, never remembered

    func testEnableForAllAccountsIsConfirmedCoversASnapshotAndIsNotRemembered() async throws {
        let model = makeModel()
        let alice = try await addOfflineAccount(model, "alice")
        let bob = try await addOfflineAccount(model, "bob")
        try await waitUntil({ model.accountsReadOnly.count == 2 }, "both accounts start read-only")

        model.requestEnableWritesForAllAccounts()
        guard case .confirmEnableAll(let refs)? = model.writeSafetyPrompt else {
            return XCTFail("expected the all-accounts confirmation, got \(String(describing: model.writeSafetyPrompt))")
        }
        XCTAssertEqual(Set(refs.map(\.id)), [alice.id, bob.id])
        // Asking is not enabling.
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: alice.id))
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: bob.id))

        let spec = AccountWritePolicy.alertSpec(for: try XCTUnwrap(model.writeSafetyPrompt), willRetry: false)
        XCTAssertTrue(spec.message.contains(alice.email) && spec.message.contains(bob.email), spec.message)
        XCTAssertEqual(spec.buttons.map(\.action), [.enable(refs.map(\.id)), .dismiss])

        model.resolveWriteSafetyPrompt(.enable(refs.map(\.id)))
        try await settle(model)
        XCTAssertTrue(try model.core.accountWriteEnabled(accountId: alice.id))
        XCTAssertTrue(try model.core.accountWriteEnabled(accountId: bob.id))
        XCTAssertEqual(model.writeState(for: alice.id), .writesEnabled)
        XCTAssertEqual(model.writeState(for: bob.id), .writesEnabled)
        XCTAssertTrue(model.accountsReadOnly.isEmpty)

        // The confirmation covered the accounts that existed; it is not a
        // standing preference. An account added afterwards starts read-only.
        let carol = try await addOfflineAccount(model, "carol")
        try await waitUntil({ model.accountsReadOnly.map(\.id) == [carol.id] }, "carol must be probed")
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: carol.id))
        XCTAssertEqual(model.writeState(for: carol.id), .readOnly)
        XCTAssertTrue(try model.core.accountWriteEnabled(accountId: alice.id), "existing accounts keep their choice")
    }

    func testCancellingTheAllAccountsConfirmationChangesNothing() async throws {
        let model = makeModel()
        let alice = try await addOfflineAccount(model, "alice")
        model.requestEnableWritesForAllAccounts()
        XCTAssertNotNil(model.writeSafetyPrompt)
        model.resolveWriteSafetyPrompt(.dismiss)
        try await settle(model)
        XCTAssertFalse(try model.core.accountWriteEnabled(accountId: alice.id))
        XCTAssertNil(model.writeSafetyPrompt)
    }

    func testEnableForAllAccountsWithNothingReadOnlyRaisesNoPrompt() {
        let model = makeModel()
        model.requestEnableWritesForAllAccounts()
        XCTAssertNil(model.writeSafetyPrompt)
    }

    // MARK: - a new account starts read-only

    func testANewAccountStartsReadOnly() async throws {
        let model = makeModel()
        let account = try await addOfflineAccount(model, "fresh")
        XCTAssertFalse(
            try model.core.accountWriteEnabled(accountId: account.id),
            "accounts.write_enabled defaults to 0: evaluation mode")
        XCTAssertEqual(model.writeState(for: account.id), .readOnly)
        XCTAssertNil(model.writeSafetyPrompt, "nothing offers to enable writes unasked")
    }

    // MARK: - strings exist in en, es and ca

    func testEveryWS1StringIsTranslatedIntoSpanishAndCatalan() throws {
        let catalogue = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/Resources/Localizable.xcstrings")
        let root = try XCTUnwrap(
            JSONSerialization.jsonObject(with: Data(contentsOf: catalogue)) as? [String: Any])
        let strings = try XCTUnwrap(root["strings"] as? [String: Any])
        let keys = [
            "Enable Writes", "Keep Read-only", "This Account Is Read-only",
            "If you enable writes, Castellum retries it straight away.",
            "Enable Writes for All Accounts…", "Enable Writes for All Accounts?",
            "Enable Writes for All", "Enable Writes…", "Return to Read-only…",
            "Writes Enabled", "Writes enabled", "Writes are enabled for every account.",
            // Multi-argument keys are stored as the runtime looks them up
            // (plain %@/%lld); a positional "%1$@" KEY is never found.
            "%lld of %lld accounts read-only (evaluation mode).",
            "%@ was not applied: %@ is in evaluation mode, so Castellum makes no changes to the mail server.\n\nEnable writes for this account to act on your mail.",
            "Leave evaluation mode for every account that is still read-only. Asks for confirmation first; accounts added later still start read-only.",
            "Castellum will start making real changes to the mail server for these accounts — archiving, deleting, flagging, marking read and sending will all be applied remotely:\n\n%@\n\nThis covers only the accounts listed. An account you add later still starts read-only.",
            "%@ now makes changes to the mail server.\n\n%@ was not applied earlier. Repeat it now.",
        ]
        for key in keys {
            let entry = try XCTUnwrap(strings[key] as? [String: Any], "missing catalogue key: \(key)")
            let localisations = try XCTUnwrap(entry["localizations"] as? [String: Any], key)
            for language in ["en", "es", "ca"] {
                let unit = (localisations[language] as? [String: Any])?["stringUnit"] as? [String: Any]
                let value = unit?["value"] as? String ?? ""
                XCTAssertFalse(value.isEmpty, "\(language) is missing for: \(key)")
            }
        }
    }

    /// The catalogue keys must be the ones the code actually looks up: resolve
    /// two interpolated WS1 strings through the BUILT es/ca tables.
    func testInterpolatedWS1StringsResolveInSpanishAndCatalan() throws {
        func table(_ language: String) throws -> Bundle {
            let path = try XCTUnwrap(
                Bundle.main.path(forResource: language, ofType: "lproj"), "\(language).lproj not built")
            return try XCTUnwrap(Bundle(path: path))
        }
        let (readOnly, total, email, verb) = (2, 3, "a@x.test", "Archive")
        let es = try table("es"), ca = try table("ca")
        XCTAssertEqual(
            String(localized: "\(readOnly) of \(total) accounts read-only (evaluation mode).", bundle: es),
            "2 de 3 cuentas en solo lectura (modo de evaluación).")
        XCTAssertEqual(
            String(localized: "\(readOnly) of \(total) accounts read-only (evaluation mode).", bundle: ca),
            "2 de 3 comptes en només lectura (mode d'avaluació).")
        let notice = String(
            localized: "\(email) now makes changes to the mail server.\n\n\(verb) was not applied earlier. Repeat it now.",
            bundle: es)
        XCTAssertTrue(notice.hasPrefix("a@x.test ahora hace cambios"), notice)
        // The refusal itself: its key was stored as "%1$@ … %2$@" until WS1,
        // so the dialog fell back to English in es and ca.
        for bundle in [es, ca] {
            let refusal = String(
                localized: "\(verb) was not applied: \(email) is in evaluation mode, so Castellum makes no changes to the mail server.\n\nEnable writes for this account to act on your mail.",
                bundle: bundle)
            XCTAssertFalse(refusal.contains("was not applied"), refusal)
            XCTAssertTrue(refusal.contains("a@x.test") && refusal.contains("Archive"), refusal)
        }
        XCTAssertEqual(String(localized: "Enable Writes for All", bundle: ca), "Activa l'escriptura per a tots")
    }

    // MARK: - one presenter for every window (the regression that caused WS1)

    func testNoSwiftUIViewOwnsTheWriteSafetyAlert() throws {
        // Two legacy `.alert(item:)` modifiers on one view: the later one
        // (compose-unavailable, #138) silently replaced the write-safety one,
        // so its dialog was never presented from 2026-09-23 until WS1. The
        // dialog now belongs to `WriteSafetyPresenter` (AppKit, any window);
        // this guards against it being moved back next to another alert.
        let app = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("Sources/Castellum/App")
        for file in ["RootView.swift", "ComposeView.swift", "SettingsView.swift", "SidebarView.swift"] {
            let text = try String(contentsOf: app.appendingPathComponent(file), encoding: .utf8)
            XCTAssertFalse(
                text.contains("model.writeSafetyPrompt"),
                "\(file) must not present or bind the write-safety prompt itself")
        }
        let root = try String(contentsOf: app.appendingPathComponent("RootView.swift"), encoding: .utf8)
        XCTAssertLessThanOrEqual(
            root.components(separatedBy: ".alert(item:").count - 1, 1,
            "at most one legacy .alert(item:) per view: a second one shadows the first")
    }
}
