// Castellum - a scripted loopback IMAP server that can hold a first sync
// IN FLIGHT, for AccountInFlightFirstSyncTests.
//
// Binds 127.0.0.1:0 (an ephemeral port), serves every connection on its own
// thread, and speaks just enough IMAP4rev1 for `castellum_sync_account`
// against a one-message INBOX: LOGIN (any password), CAPABILITY, LSUB, LIST,
// SELECT/EXAMINE, the three UID FETCH shapes the sync issues (descriptors
// with ENVELOPE/BODYSTRUCTURE, FLAGS-only reconcile, BODY.PEEK[] bodies) and
// LOGOUT; anything else gets a plain tagged OK.
//
// The point is `hold(user:)`: the NEXT descriptor fetch (`UID FETCH … ENVELOPE`)
// from a session logged in as `user` blocks — after LOGIN, LIST and SELECT
// have already succeeded, so the core has created the account row and begun
// its progress snapshot, exactly like Marc's 33k-message GreenMail adds
// sitting in "Continue in Background" — until the test calls
// `release(user:)`. That makes "account A's first sync is still running"
// a fact the test controls, not a timing bet against a real mailbox.
//
// Unlike GatedProxy (which holds a connection before any bytes flow, i.e.
// before login), this holds AFTER login — the state the in-flight-first-sync
// bug lives in. No GreenMail, no Docker: runs anywhere XCTest runs.
//
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

final class StallingImapServer {
    /// The port this server is listening on (127.0.0.1 only).
    let port: UInt16

    private let listenFD: Int32
    private let lock = NSCondition()
    /// Users whose NEXT descriptor fetch must block (one-shot: the hold is
    /// consumed by the session that hits it; later sessions pass freely).
    private var held: Set<String> = []
    /// Users with a descriptor fetch currently blocked here.
    private var stalled: Set<String> = []
    /// Stalled users `release(user:)` has let go.
    private var released: Set<String> = []
    /// Descriptor fetches served (after any hold), per user.
    private var descriptorFetches: [String: Int] = [:]
    /// Held users whose session disconnects (rather than answers) on release.
    private var dropOnRelease: Set<String> = []
    private var closed = false

    init() throws {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw Self.error(1, "socket() failed") }
        var reuse: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &reuse, socklen_t(MemoryLayout<Int32>.size))

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        addr.sin_port = 0 // ask the kernel for an ephemeral port
        let bound = withUnsafePointer(to: &addr) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                bind(fd, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard bound == 0 else { close(fd); throw Self.error(2, "bind(127.0.0.1:0) failed") }
        guard listen(fd, 16) == 0 else { close(fd); throw Self.error(3, "listen() failed") }

        var actual = sockaddr_in()
        var actualLen = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &actual) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                getsockname(fd, sa, &actualLen)
            }
        }
        port = UInt16(bigEndian: actual.sin_port)
        listenFD = fd

        Thread.detachNewThread { [weak self] in
            while true {
                var clientAddr = sockaddr()
                var clientLen = socklen_t(MemoryLayout<sockaddr>.size)
                let client = accept(fd, &clientAddr, &clientLen)
                if client < 0 { return } // listenFD closed by stop()/deinit
                Thread.detachNewThread { self?.serve(client) }
            }
        }
    }

    // MARK: - test controls

    /// The NEXT descriptor fetch from a session logged in as `user` blocks
    /// until `release(user:)`. One-shot: a later session for the same user
    /// (e.g. a relaunched app resuming the sync) is answered normally.
    func hold(user: String) {
        lock.lock(); held.insert(user); lock.unlock()
    }

    /// Lets the held descriptor fetch for `user` proceed (or cancels a hold
    /// that no session has hit yet). `disconnect: true` closes that session
    /// instead of answering, so the held sync ends promptly (test cleanup)
    /// rather than completing.
    func release(user: String, disconnect: Bool = false) {
        lock.lock()
        held.remove(user)
        if stalled.contains(user) {
            released.insert(user)
            if disconnect { dropOnRelease.insert(user) }
        }
        lock.broadcast()
        lock.unlock()
    }

    /// Whether a descriptor fetch for `user` is currently blocked here —
    /// i.e. that session is past LOGIN/LIST/SELECT and genuinely in flight.
    func isStalled(user: String) -> Bool {
        lock.lock(); defer { lock.unlock() }
        return stalled.contains(user)
    }

    /// How many descriptor fetches for `user` have been answered.
    func descriptorFetchCount(user: String) -> Int {
        lock.lock(); defer { lock.unlock() }
        return descriptorFetches[user] ?? 0
    }

    /// Stops accepting new connections and wakes any held session (which
    /// then disconnects). Safe to call more than once.
    func stop() {
        lock.lock()
        guard !closed else { lock.unlock(); return }
        closed = true
        lock.broadcast()
        lock.unlock()
        close(listenFD)
    }

    deinit { stop() }

    // MARK: - protocol

    /// One RFC 5322 message; its size is what RFC822.SIZE/literals report.
    private static let rawMessage =
        "From: Ann <ann@example.com>\r\nTo: Bob <bob@example.com>\r\n"
        + "Subject: hello\r\nMessage-ID: <m1@example.com>\r\n"
        + "Date: Thu, 01 Jan 2026 00:00:00 +0000\r\n\r\nhello\r\n"

    private static let envelope =
        #"ENVELOPE ("Thu, 01 Jan 2026 00:00:00 +0000" "hello" "#
        + #"(("Ann" NIL "ann" "example.com")) (("Ann" NIL "ann" "example.com")) "#
        + #"(("Ann" NIL "ann" "example.com")) (("Bob" NIL "bob" "example.com")) "#
        + #"NIL NIL NIL "<m1@example.com>")"#

    private func serve(_ client: Int32) {
        defer { close(client) }
        var user = ""
        var buffer = Data()
        guard send(client, "* OK [CAPABILITY IMAP4rev1] mock ready\r\n") else { return }
        while let line = readLine(client, &buffer) {
            let parts = line.split(separator: " ", maxSplits: 2).map(String.init)
            guard parts.count >= 2 else { continue }
            let tag = parts[0]
            let command = parts[1].uppercased()
            let rest = parts.count > 2 ? parts[2] : ""
            var reply: String
            switch command {
            case "LOGIN":
                user = Self.firstArgument(rest)
                reply = "\(tag) OK LOGIN completed\r\n"
            case "CAPABILITY":
                reply = "* CAPABILITY IMAP4rev1\r\n\(tag) OK CAPABILITY completed\r\n"
            case "LSUB":
                reply = "* LSUB () \"/\" INBOX\r\n\(tag) OK LSUB completed\r\n"
            case "LIST":
                reply = "* LIST () \"/\" INBOX\r\n\(tag) OK LIST completed\r\n"
            case "SELECT", "EXAMINE":
                reply = "* FLAGS (\\Seen \\Flagged)\r\n* 1 EXISTS\r\n* 0 RECENT\r\n"
                    + "* OK [UIDVALIDITY 42] UIDs valid\r\n* OK [UIDNEXT 2] next\r\n"
                    + "\(tag) OK [READ-WRITE] \(command) completed\r\n"
            case "UID" where rest.uppercased().hasPrefix("FETCH"):
                let upper = rest.uppercased()
                if upper.contains("ENVELOPE") {
                    guard waitIfHeld(user: user) else { return } // disconnect
                    let size = Self.rawMessage.utf8.count
                    reply = "* 1 FETCH (UID 1 FLAGS (\\Seen) RFC822.SIZE \(size) "
                        + "INTERNALDATE \"01-Jan-2026 00:00:00 +0000\" \(Self.envelope) "
                        + "BODYSTRUCTURE (\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"7BIT\" 5 1))\r\n"
                        + "\(tag) OK UID FETCH completed\r\n"
                } else if upper.contains("BODY") {
                    let size = Self.rawMessage.utf8.count
                    reply = "* 1 FETCH (UID 1 FLAGS (\\Seen) BODY[] {\(size)}\r\n"
                        + Self.rawMessage + ")\r\n\(tag) OK UID FETCH completed\r\n"
                } else {
                    reply = "* 1 FETCH (UID 1 FLAGS (\\Seen))\r\n\(tag) OK UID FETCH completed\r\n"
                }
            case "LOGOUT":
                _ = send(client, "* BYE bye\r\n\(tag) OK LOGOUT completed\r\n")
                return
            default:
                reply = "\(tag) OK \(command) completed\r\n"
            }
            guard send(client, reply) else { return }
        }
    }

    /// Blocks while `user` is held. Returns false when the session should
    /// disconnect instead of answering (released with `disconnect: true`,
    /// or the server stopped).
    private func waitIfHeld(user: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if held.remove(user) != nil {
            stalled.insert(user)
            while !released.contains(user) && !closed { lock.wait() }
            stalled.remove(user)
            released.remove(user)
            if dropOnRelease.remove(user) != nil || closed { return false }
        }
        descriptorFetches[user, default: 0] += 1
        return true
    }

    /// `LOGIN "user" "pass"` or `LOGIN user pass` → `user`.
    private static func firstArgument(_ rest: String) -> String {
        if rest.hasPrefix("\"") {
            let body = rest.dropFirst()
            return String(body.prefix { $0 != "\"" })
        }
        return String(rest.prefix { $0 != " " })
    }

    private func readLine(_ fd: Int32, _ buffer: inout Data) -> String? {
        let crlf = Data("\r\n".utf8)
        while true {
            if let r = buffer.range(of: crlf) {
                let line = String(decoding: buffer[..<r.lowerBound], as: UTF8.self)
                buffer.removeSubrange(..<r.upperBound)
                return line
            }
            var chunk = [UInt8](repeating: 0, count: 4096)
            let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress, $0.count) }
            if n <= 0 { return nil }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }

    private func send(_ fd: Int32, _ text: String) -> Bool {
        let bytes = Array(text.utf8)
        var off = 0
        while off < bytes.count {
            let w = bytes[off...].withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            if w <= 0 { return false }
            off += w
        }
        return true
    }

    private static func error(_ code: Int, _ message: String) -> NSError {
        NSError(domain: "StallingImapServer", code: code, userInfo: [
            NSLocalizedDescriptionKey: message,
        ])
    }
}
