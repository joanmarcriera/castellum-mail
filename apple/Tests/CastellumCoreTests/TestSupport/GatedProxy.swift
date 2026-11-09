// Castellum - a loopback TCP gate for deterministic sync-completion
// ordering in AccountReconnectTests.
//
// Binds 127.0.0.1:0 (an ephemeral port) and accepts exactly one client
// connection immediately — so the caller's `connect()` succeeds right away,
// exactly like a real, reachable IMAP server — but withholds every byte
// until the test calls `release()`. Only then does it open its own
// connection to `upstreamHost:upstreamPort` and pipe bytes bidirectionally,
// so the held IMAP session resumes and completes normally against the real
// server (GreenMail).
//
// This exists because AccountReconnectTests' out-of-order-completion test
// needs two REAL, SUCCESSFUL GreenMail syncs to finish in a specific,
// proven order — unlike `testOriginalSyncSurvivesConcurrentRetryWithDifferentGeneration`
// (which only ever fires one `onSuccess`, so it doesn't matter which
// attempt's background work finishes first), a race between two successful
// attempts has no safe, structural speed differential to lean on. The gate
// makes the ordering a fact, not a bet: `release()` is a real semaphore
// signal, called only after the test has already confirmed the OTHER
// attempt landed — never a sleep.
//
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Joan Marc Riera Duocastella

import Foundation

final class GatedProxy {
    /// The port this gate listens on (127.0.0.1 only) — point a config's
    /// `host`/`port` here instead of at the real server to hold that sync.
    let port: UInt16

    private let listenFD: Int32
    private let gate = DispatchSemaphore(value: 0)

    init(upstreamHost: String, upstreamPort: UInt16) throws {
        let fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw NSError(domain: "GatedProxy", code: 1, userInfo: [
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
            throw NSError(domain: "GatedProxy", code: 2, userInfo: [
                NSLocalizedDescriptionKey: "bind(127.0.0.1:0) failed",
            ])
        }
        guard listen(fd, 8) == 0 else {
            close(fd)
            throw NSError(domain: "GatedProxy", code: 3, userInfo: [
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

        let gate = self.gate
        Thread.detachNewThread {
            var clientAddr = sockaddr()
            var clientLen = socklen_t(MemoryLayout<sockaddr>.size)
            let client = accept(fd, &clientAddr, &clientLen)
            if client < 0 { return } // listenFD closed by stop()/deinit before a connect ever arrived

            // Hold here — no bytes flow to/from the client — until the test
            // explicitly releases us.
            gate.wait()

            let up = socket(AF_INET, SOCK_STREAM, 0)
            guard up >= 0 else { close(client); return }
            var upAddr = sockaddr_in()
            upAddr.sin_family = sa_family_t(AF_INET)
            upAddr.sin_port = upstreamPort.bigEndian
            upAddr.sin_addr.s_addr = inet_addr(upstreamHost)
            let connected = withUnsafePointer(to: &upAddr) { ptr -> Int32 in
                ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sa in
                    connect(up, sa, socklen_t(MemoryLayout<sockaddr_in>.size))
                }
            }
            guard connected == 0 else { close(client); close(up); return }

            let pump: (Int32, Int32) -> Void = { src, dst in
                var buf = [UInt8](repeating: 0, count: 4096)
                while true {
                    let n = buf.withUnsafeMutableBytes { read(src, $0.baseAddress, $0.count) }
                    if n <= 0 { break }
                    var off = 0
                    while off < n {
                        let w = buf[off..<n].withUnsafeBytes { write(dst, $0.baseAddress, n - off) }
                        if w <= 0 { break }
                        off += w
                    }
                }
            }
            let done = DispatchGroup()
            done.enter()
            Thread.detachNewThread { pump(client, up); done.leave() }
            done.enter()
            Thread.detachNewThread { pump(up, client); done.leave() }
            done.wait()
            close(client)
            close(up)
        }
    }

    /// Lets the held connection through to the real upstream. Exactly one
    /// held connection per gate, so exactly one `release()` per gate.
    func release() {
        gate.signal()
    }

    /// Stops accepting; safe to call more than once.
    func stop() {
        close(listenFD)
    }

    deinit { stop() }
}
