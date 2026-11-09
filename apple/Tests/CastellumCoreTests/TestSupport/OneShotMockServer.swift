// Castellum - a loopback-only HTTP mock server for S66's List-Unsubscribe
// hardening tests. Binds 127.0.0.1:0 (an ephemeral port), accepts
// connections one at a time on a background thread, records exactly what
// each one received (method, path, headers, body), replies with a fixed
// status line, and lets a test wait for "a request has arrived" via a real
// condition-variable signal rather than a blind, fixed-duration sleep.
//
// Swift port of unsubscribe.rs's own `one_shot_server` Rust test helper
// (native/crates/castellum-core/src/unsubscribe.rs) — same shape, same
// reason: this proves the mock records what the FFI's `post_one_click`
// actually sent, it does not reimplement or second-guess the transport
// Rust's own unit tests already cover.
//
// Never resolves to anything but 127.0.0.1 — the server itself is the
// loopback boundary every S66 test relies on to prove "no real network call
// leaves the test process".
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

final class OneShotMockServer {
    struct RecordedRequest {
        let method: String
        let path: String
        let headers: [String: String] // lower-cased keys
        let body: Data
    }

    /// The port this server is actually listening on (127.0.0.1 only).
    let port: UInt16
    /// `http://127.0.0.1:<port>` — hand this to a seeded message's
    /// `list_unsubscribe_url` so a real confirm click posts here.
    var urlString: String { "http://127.0.0.1:\(port)" }

    private let listenFD: Int32
    private let statusLine: String
    private let condition = NSCondition()
    private var requests: [RecordedRequest] = []
    private var closed = false

    init(statusLine: String = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n") throws {
        self.statusLine = statusLine

        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: "OneShotMockServer", code: 1, userInfo: [
                NSLocalizedDescriptionKey: "socket() failed",
            ])
        }
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
        guard bound == 0 else {
            close(fd)
            throw NSError(domain: "OneShotMockServer", code: 2, userInfo: [
                NSLocalizedDescriptionKey: "bind(127.0.0.1:0) failed",
            ])
        }
        guard listen(fd, 8) == 0 else {
            close(fd)
            throw NSError(domain: "OneShotMockServer", code: 3, userInfo: [
                NSLocalizedDescriptionKey: "listen() failed",
            ])
        }

        var actual = sockaddr_in()
        var actualLen = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &actual) { ptr -> Int32 in
            ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                getsockname(fd, sa, &actualLen)
            }
        }
        self.port = UInt16(bigEndian: actual.sin_port)
        self.listenFD = fd

        let condition = self.condition
        let recordAndReply: (Int32) -> Void = { [weak self] client in
            self?.handle(client: client, condition: condition)
        }
        Thread.detachNewThread {
            while true {
                var clientAddr = sockaddr()
                var clientLen = socklen_t(MemoryLayout<sockaddr>.size)
                let client = accept(fd, &clientAddr, &clientLen)
                if client < 0 { break } // listenFD closed by stop()/deinit
                recordAndReply(client)
            }
        }
    }

    /// Reads the WHOLE request (headers, then the body its Content-Length
    /// declares) before replying — a single `read()` can return just the
    /// headers, and replying/closing at that point can race a still-writing
    /// client (the same flakiness `unsubscribe.rs`'s own helper documents
    /// and works around).
    private func handle(client: Int32, condition: NSCondition) {
        defer { close(client) }
        var data = Data()
        var buf = [UInt8](repeating: 0, count: 4096)
        var headerEnd: Range<Data.Index>?
        var declaredLength = 0
        while true {
            let n = buf.withUnsafeMutableBytes { read(client, $0.baseAddress, $0.count) }
            if n <= 0 { break }
            data.append(contentsOf: buf[0..<n])
            if headerEnd == nil {
                headerEnd = data.range(of: Data("\r\n\r\n".utf8))
                if let r = headerEnd {
                    let head = String(data: data[..<r.lowerBound], encoding: .utf8) ?? ""
                    for line in head.components(separatedBy: "\r\n")
                    where line.lowercased().hasPrefix("content-length:") {
                        let value = line.split(separator: ":", maxSplits: 1)[1]
                            .trimmingCharacters(in: .whitespaces)
                        declaredLength = Int(value) ?? 0
                    }
                }
            }
            if let r = headerEnd, data.count >= r.upperBound + declaredLength { break }
        }
        guard let r = headerEnd else { return } // never got a full header block

        let head = String(data: data[..<r.lowerBound], encoding: .utf8) ?? ""
        let bodyData = Data(data[r.upperBound...])
        let lines = head.components(separatedBy: "\r\n").filter { !$0.isEmpty }
        let requestLine = lines.first ?? ""
        let parts = requestLine.split(separator: " ", maxSplits: 2)
        let method = parts.count > 0 ? String(parts[0]) : ""
        let path = parts.count > 1 ? String(parts[1]) : ""

        var headers: [String: String] = [:]
        for line in lines.dropFirst() {
            guard let colon = line.firstIndex(of: ":") else { continue }
            let key = line[..<colon].trimmingCharacters(in: .whitespaces).lowercased()
            let value = line[line.index(after: colon)...].trimmingCharacters(in: .whitespaces)
            headers[key] = value
        }

        condition.lock()
        requests.append(RecordedRequest(method: method, path: path, headers: headers, body: bodyData))
        condition.signal()
        condition.unlock()

        _ = statusLine.withCString { write(client, $0, strlen($0)) }
    }

    /// Blocks (this test's calling thread, not the app's) until at least one
    /// request has been recorded or `timeout` elapses. Returns whether one
    /// arrived. This is a real condition-variable wait signalled by the
    /// accept thread above — not a fixed sleep-then-check.
    @discardableResult
    func waitForRequest(timeout: TimeInterval = 5) -> Bool {
        condition.lock()
        defer { condition.unlock() }
        let deadline = Date().addingTimeInterval(timeout)
        while requests.isEmpty {
            if !condition.wait(until: deadline) { return !requests.isEmpty }
        }
        return true
    }

    func recordedRequests() -> [RecordedRequest] {
        condition.lock()
        defer { condition.unlock() }
        return requests
    }

    var requestCount: Int { recordedRequests().count }

    /// Stops accepting new connections. Safe to call more than once.
    func stop() {
        condition.lock()
        guard !closed else { condition.unlock(); return }
        closed = true
        condition.unlock()
        close(listenFD)
    }

    deinit { stop() }
}
