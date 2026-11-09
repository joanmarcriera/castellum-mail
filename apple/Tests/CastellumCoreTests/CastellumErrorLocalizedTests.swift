// Castellum - CastellumError.localizedDescription is readable (RP05).
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import XCTest
@testable import Castellum

final class CastellumErrorLocalizedTests: XCTestCase {
    func testCoreMessageIsShownVerbatim() {
        let text = "Refused: endpoint is not local"
        XCTAssertEqual(CastellumError.message(text).localizedDescription, text)
    }

    func testBareStatusCasesAreNotFoundationBoilerplate() {
        let all: [CastellumError] = [.invalidArgument, .invalidHandle, .io, .alreadyOpen,
                                     .network, .internalError, .decode("x"), .unknown(7)]
        for error in all {
            XCTAssertFalse(error.localizedDescription.contains("operation couldn"),
                           "\(error) fell back to Foundation text")
            XCTAssertEqual(error.localizedDescription, error.description)
        }
    }
}
