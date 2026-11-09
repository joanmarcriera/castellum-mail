#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xh explorer helper: a TCP server on 127.0.0.1:<port> that accepts connections and never sends a byte
(a mail server behind a dead NAT / captive portal). Self-terminates after <seconds>.
Usage: silent-server.py <port> <seconds> [greet]   (greet: send an IMAP greeting, then go silent)"""
import socket, sys, time, threading
port, secs = int(sys.argv[1]), int(sys.argv[2]); greet = len(sys.argv) > 3
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(16); srv.settimeout(1.0)
held, end = [], time.time() + secs
while time.time() < end:
    try:
        c, _ = srv.accept()
        if greet: c.sendall(b"* OK xh silent server ready\r\n")
        held.append(c); print("accepted", len(held), time.strftime("%H:%M:%S"), flush=True)
    except socket.timeout:
        pass
