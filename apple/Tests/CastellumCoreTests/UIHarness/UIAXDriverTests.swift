// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1 Layer B: the harness can find and drive SwiftUI
// Button / Toggle / Picker in-process, by label, with no Accessibility grant and no mouse.
// On macOS 27 these controls are not NSControls, so the path under test is
// `UIViewTree.resolveVisible` -> `UIAXDriver` (accessibility nodes). On an older macOS the
// same lookups resolve to NSButton / NSPopUpButton and the assertions hold unchanged.
//
// Usage: runs with the normal XCTest target (`scripts/build-macos.sh test`).
import AppKit
import SwiftUI
import XCTest
@testable import Castellum

@MainActor
final class UIAXDriverTests: XCTestCase {
    final class Model: ObservableObject {
        @Published var presses = 0
        @Published var flag = false
        @Published var kind = "one"
    }

    struct Fixture: View {
        @ObservedObject var model: Model
        var body: some View {
            VStack(alignment: .leading) {
                Button("Go…") { model.presses += 1 }
                Button("Twin") { model.presses += 10 }
                Button("Twin") { model.presses += 100 }
                Button("Off") { model.presses += 1000 }.disabled(true)
                Toggle("Flag", isOn: $model.flag)
                Picker("Kind", selection: $model.kind) {
                    Text("one").tag("one")
                    Text("two").tag("two")
                    Text("three").tag("three")
                }
            }
            .padding()
        }
    }

    private var window: NSWindow!
    private var model: Model!

    override func setUp() async throws {
        model = Model()
        window = UIHarnessWindow(contentRect: NSRect(x: 0, y: 0, width: 320, height: 260),
                                 styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        window.contentView = NSHostingView(rootView: Fixture(model: model))
        window.orderFront(nil)
        UISnapshotHarness.settle(window, 0.2)
    }

    override func tearDown() async throws {
        window.orderOut(nil)
        window = nil
        UIAXDriver.reset()   // later Layer A tests run as in a process that never asked for the tree
    }

    private func find(_ label: String, role: String? = nil, index: Int? = nil) async throws -> UIViewTree.Entry {
        let f = UIFind(identifier: nil, label: label, role: role, text: nil, index: index)
        let hit = await UIViewTree.resolveVisible(f, in: window)
        return try XCTUnwrap(hit, "no visible control labelled '\(label)'")
    }

    private func settle() async { try? await Task.sleep(nanoseconds: 300_000_000) }

    func testPressRunsAButtonActionFoundByLabel() async throws {
        try await find("Go…").press()
        await settle()
        XCTAssertEqual(model.presses, 1)
    }

    func testIndexPicksAmongControlsThatShareALabel() async throws {
        try await find("Twin", index: 1).press()
        await settle()
        XCTAssertEqual(model.presses, 100, "index 1 is the lower of the two Twin buttons")
        try await find("Twin", index: 0).press()
        await settle()
        XCTAssertEqual(model.presses, 110)
        let none = await UIViewTree.resolveVisible(UIFind(identifier: nil, label: "Twin", role: nil, text: nil, index: 2), in: window, timeoutMs: 200)
        XCTAssertNil(none, "there is no third Twin")
    }

    func testDisabledControlIsReportedDisabledAndRefusesThePress() async throws {
        let off = try await find("Off")
        XCTAssertFalse(off.node.enabled)
        do { try await off.press(); XCTFail("a disabled control must not be pressed") } catch {}
        await settle()
        XCTAssertEqual(model.presses, 0)
    }

    func testToggleFlipsThroughItsBinding() async throws {
        let flag = try await find("Flag", role: "checkbox")
        if let ax = flag.ax { XCTAssertEqual(UIAXDriver.isOn(ax), false) }
        if let b = flag.view as? NSButton { b.performClick(nil) } else { try await UIAXDriver.press(flag) }
        await settle()
        XCTAssertTrue(model.flag)
        if let ax = try await find("Flag", role: "checkbox").ax { XCTAssertEqual(UIAXDriver.isOn(ax), true) }
    }

    func testPickerListsItsItemsAndChoosesOneWithoutWaitingForAMouse() async throws {
        // A pop-up is known by the value it shows, like an NSPopUpButton's selected title.
        let pop = try await find("one", role: "popup")
        try XCTSkipIf(pop.ax == nil, "AppKit-backed Picker (macOS 26 and earlier): the NSPopUpButton path drives it")
        XCTAssertEqual(UIViewTree.popupTitles(in: window), ["one"])
        let listed = try await UIAXDriver.menu(of: pop, choose: nil)
        XCTAssertEqual(listed, ["one", "two", "three"])
        XCTAssertEqual(model.kind, "one", "listing the items must not choose one")
        try await UIAXDriver.menu(of: pop, choose: "three")
        await settle()
        XCTAssertEqual(model.kind, "three")
        XCTAssertEqual(UIViewTree.popupTitles(in: window), ["three"])
        let again = try await find("three", role: "popup")
        do { try await UIAXDriver.menu(of: again, choose: "nine"); XCTFail("unknown item must throw") } catch {}
        XCTAssertEqual(model.kind, "three")
    }

    func testLayoutNodesAreUnchangedByTheAccessibilityFallback() async throws {
        // `find` (what layout rules and `nodes` use) never returns accessibility entries.
        _ = try await find("Go…")
        XCTAssertTrue(UIViewTree.find(UIFind(identifier: nil, label: "Go…", role: nil, text: nil), in: window).allSatisfy { $0.ax == nil })
        XCTAssertTrue(UIViewTree.entries(in: window).allSatisfy { $0.ax == nil })
    }
}
