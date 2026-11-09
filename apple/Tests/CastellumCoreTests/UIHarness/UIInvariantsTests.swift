// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella
// Castellum - UIT-1: each `UIRule` on synthetic nodes, as a pass/fail pair.
// The rules are pure (value types only), so no AppKit is involved here.
import XCTest
@testable import Castellum

final class UIInvariantsTests: XCTestCase {
    private let content = CGRect(x: 0, y: 0, width: 400, height: 300)

    private func window(resizable: Bool = true, movable: Bool = true, drag: Bool = true,
                        frame: CGRect = CGRect(x: 100, y: 100, width: 420, height: 560),
                        minSize: CGSize = CGSize(width: 420, height: 300), ratio: Double? = 0.3,
                        screen: CGRect? = CGRect(x: 0, y: 0, width: 1280, height: 743)) -> UIWindowInfo {
        UIWindowInfo(label: "w", title: "t", cls: "NSWindow", frame: frame, contentRect: content,
                     contentLayoutRect: content, contentMinSize: minSize, chrome: CGSize(width: 0, height: 28),
                     resizable: resizable, movable: movable, isSheet: true, hasDragHandle: drag,
                     realScreenVisible: screen, nonBlankRatio: ratio)
    }

    private func node(role: String = "button", label: String = "OK", frame: CGRect = CGRect(x: 10, y: 10, width: 80, height: 24),
                      hidden: Bool = false, enabled: Bool = true, key: String = "", scroll: Bool = false,
                      overflow: Bool = false) -> UINode {
        UINode(path: "/n", cls: "X", role: role, label: label, identifier: "", frame: frame, hidden: hidden,
               enabled: enabled, keyEquivalent: key, inScrollView: scroll, textOverflow: overflow)
    }

    private func violations(_ rule: String, _ w: UIWindowInfo, _ nodes: [UINode] = [],
                            screen: UIVirtualScreen = .standard) -> Int {
        UIRule.evaluate(rule, window: w, nodes: nodes, screen: screen).count
    }

    func testNonBlank() {
        XCTAssertEqual(violations("nonBlank", window(ratio: 0.3)), 0)
        XCTAssertEqual(violations("nonBlank", window(ratio: 0.0)), 1)
        XCTAssertEqual(violations("nonBlank", window(ratio: nil)), 1)
    }

    func testResizableAndMovable() {
        XCTAssertEqual(violations("resizable", window()), 0)
        XCTAssertEqual(violations("resizable", window(resizable: false)), 1)
        XCTAssertEqual(violations("movable", window(movable: false)), 1)
        XCTAssertEqual(violations("movableSheet", window()), 0)
        XCTAssertEqual(violations("movableSheet", window(drag: false)), 1)
    }

    func testFitsScreenUsesVirtualScreen() {
        XCTAssertEqual(violations("fitsScreen", window()), 0)
        let tiny = UIVirtualScreen(width: 400, height: 400, menuBar: 25)
        XCTAssertEqual(violations("fitsScreen", window(), screen: tiny), 2, "frame and minimum both too big")
        // A minimum that does not fit even when the current frame does.
        XCTAssertEqual(violations("fitsScreen", window(frame: CGRect(x: 0, y: 0, width: 300, height: 300),
                                                        minSize: CGSize(width: 300, height: 900))), 1)
    }

    func testOnScreen() {
        XCTAssertEqual(violations("onScreen", window()), 0)
        XCTAssertEqual(violations("onScreen", window(frame: CGRect(x: 1000, y: 100, width: 420, height: 560))), 1)
        XCTAssertEqual(violations("onScreen", window(screen: nil)), 1)
    }

    func testPrimaryButtonsVisible() {
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(key: "\r")]), 0)
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(hidden: true)]), 1)
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(frame: CGRect(x: 10, y: 10, width: 0, height: 24))]), 1)
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(frame: CGRect(x: 10, y: 400, width: 80, height: 24))]), 1,
                       "footer pushed below the window")
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(frame: CGRect(x: 10, y: 400, width: 80, height: 24), scroll: true)]), 0,
                       "a button inside a scroll view is reachable by scrolling")
    }

    func testControlsReachable() {
        XCTAssertEqual(violations("controlsReachable", window(), [node()]), 0)
        XCTAssertEqual(violations("controlsReachable", window(), [node(frame: CGRect(x: 10, y: 500, width: 80, height: 24))]), 1)
        XCTAssertEqual(violations("controlsReachable", window(), [node(frame: CGRect(x: 10, y: 500, width: 80, height: 24), scroll: true)]), 0)
        XCTAssertEqual(violations("controlsReachable", window(), [node(frame: CGRect(x: 10, y: 500, width: 80, height: 24), enabled: false)]), 0)
    }

    func testNodeRulesFailWhenNothingWasInspected() {
        for rule in ["primaryButtonsVisible", "controlsReachable", "noZeroSizeTextFields", "noClippedText"] {
            let out = UIRule.evaluate(rule, window: window(), nodes: [], screen: .standard)
            XCTAssertEqual(out.count, 1, rule)
            XCTAssertTrue(out.first?.detail.contains("0 ") == true && out.first?.detail.contains("inspected") == true, rule)
        }
        // Nodes present but no button: primaryButtonsVisible still proved nothing.
        XCTAssertEqual(violations("primaryButtonsVisible", window(), [node(role: "textField")]), 1)
    }

    func testNoZeroSizeTextFields() {
        XCTAssertEqual(violations("noZeroSizeTextFields", window(), [node(role: "textField", frame: CGRect(x: 0, y: 0, width: 200, height: 22))]), 0)
        XCTAssertEqual(violations("noZeroSizeTextFields", window(), [node(role: "textField", frame: CGRect(x: 0, y: 0, width: 5, height: 22))]), 1)
        XCTAssertEqual(violations("noZeroSizeTextFields", window(), [node(role: "secureTextField", frame: CGRect(x: 0, y: 0, width: 200, height: 4))]), 1)
    }

    func testNoClippedText() {
        XCTAssertEqual(violations("noClippedText", window(), [node(role: "staticText")]), 0)
        XCTAssertEqual(violations("noClippedText", window(), [node(role: "staticText", overflow: true)]), 1)
        // The #2646 shape: a control (or scroll document) crossing the window edge.
        XCTAssertEqual(violations("noClippedText", window(), [node(frame: CGRect(x: -17, y: 0, width: 514, height: 300))]), 1)
        XCTAssertEqual(violations("noClippedText", window(), [node(frame: CGRect(x: -17, y: 0, width: 514, height: 300), scroll: true)]), 0)
    }

    func testEveryDeclaredRuleIsHandled() {
        for name in UIRule.allNames {
            let out = UIRule.evaluate(name, window: window(), nodes: [], screen: .standard)
            XCTAssertFalse(out.contains { $0.detail == "unknown rule" }, "\(name) has no implementation")
        }
        XCTAssertEqual(UIRule.evaluate("bogus", window: window(), nodes: [], screen: .standard).count, 1)
    }
}
