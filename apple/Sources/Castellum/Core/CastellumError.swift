// Castellum - Swift errors mapping the C ABI's CastellumStatus codes.
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

enum CastellumError: Error, Equatable {
    case invalidArgument
    case invalidHandle
    case io
    case alreadyOpen
    case network
    case internalError
    case decode(String)
    case unknown(Int32)
    /// A specific reason string from the core (via castellum_last_error).
    case message(String)

    /// Map a CastellumStatus code (see castellum.h) to an error. `nil` for OK (0).
    static func from(status: Int32) -> CastellumError? {
        switch status {
        case 0: return nil
        case 1: return .invalidArgument
        case 2: return .invalidHandle
        case 3: return .io
        case 4: return .alreadyOpen
        case 5: return .network
        case 99: return .internalError
        default: return .unknown(status)
        }
    }

    /// The core's last-error reason on this thread, if any. Call immediately after
    /// a failed FFI call, on the same thread.
    static func lastReason() -> String? {
        guard let c = castellum_last_error() else { return nil }
        defer { castellum_string_free(c) }
        let s = String(cString: c)
        return s.isEmpty ? nil : s
    }
}

/// Without this, `error.localizedDescription` is Foundation's generic
/// "The operation couldn't be completed. (Castellum.CastellumError error 2.)"
/// wherever the UI shows an error (RP05). `description` already carries the
/// readable text, and the core's verbatim reason for `.message`.
extension CastellumError: LocalizedError {
    var errorDescription: String? { description }
}

extension CastellumError: CustomStringConvertible {
    var description: String {
        switch self {
        case .invalidArgument: return String(localized: "invalid argument")
        case .invalidHandle: return String(localized: "invalid handle")
        case .io: return String(localized: "I/O error")
        case .alreadyOpen: return String(localized: "already open")
        case .network: return String(localized: "network error")
        case .internalError: return String(localized: "internal error")
        // The detail (`s`) is free-form text from the core/SQLite — not
        // translatable in general, so only the fixed template localises.
        case .decode(let s): return String(localized: "decode error: \(s)")
        case .unknown(let code): return String(localized: "error \(code)")
        // Verbatim text from the Rust core (`castellum_last_error`): a mix of
        // fixed prefixes (some load-bearing, matched by AccountWritePolicy /
        // AccountResetPolicy — never reworded) and free-form driver detail
        // that cannot be translated generically.
        case .message(let s): return s
        }
    }
}
